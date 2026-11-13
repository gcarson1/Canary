//! Standard MIDI Files: read format 0 and 1 into notes, write notes as format 0.

use std::fmt;

use crate::{Note, MAX_TICKS, PPQ};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmfError {
    /// Not a MIDI file, or cut short.
    Malformed,
    /// SMPTE time division (frames, not beats), which has no tempo grid.
    Smpte,
}

impl fmt::Display for SmfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => write!(f, "not a valid MIDI file"),
            Self::Smpte => write!(f, "SMPTE-timed MIDI files are not supported"),
        }
    }
}

impl std::error::Error for SmfError {}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], SmfError> {
        let end = self.pos.checked_add(n).ok_or(SmfError::Malformed)?;
        let s = self.bytes.get(self.pos..end).ok_or(SmfError::Malformed)?;
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, SmfError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, SmfError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, SmfError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A variable-length quantity: at most four bytes.
    fn vlq(&mut self) -> Result<u32, SmfError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let b = self.u8()?;
            v = v << 7 | u32::from(b & 0x7f);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(SmfError::Malformed)
    }
}

/// Most notes one file may hold down at once; more is a broken or hostile file
/// (each note-off searches them).
const MAX_HELD: usize = 4096;

/// Reads a format 0 or 1 file: every track and channel merged, at `PPQ`. Notes
/// still held at the end of a track end there; notes past `MAX_TICKS` are dropped.
pub fn read(bytes: &[u8]) -> Result<Vec<Note>, SmfError> {
    let mut r = Reader { bytes, pos: 0 };
    if r.take(4)? != b"MThd" {
        return Err(SmfError::Malformed);
    }
    let header_len = r.u32()? as usize;
    let header_end = r.pos.checked_add(header_len).ok_or(SmfError::Malformed)?;
    let (_format, tracks, division) = (r.u16()?, r.u16()?, r.u16()?);
    if division & 0x8000 != 0 {
        return Err(SmfError::Smpte);
    }
    if division == 0 || header_len < 6 {
        return Err(SmfError::Malformed);
    }
    r.pos = header_end;
    let scale = |t: u64| t.saturating_mul(PPQ) / u64::from(division);
    let mut notes = Vec::new();
    for _ in 0..tracks {
        // A header may count more tracks than the file holds.
        if r.pos >= r.bytes.len() {
            break;
        }
        // Skip chunks that are not tracks, as the spec asks.
        let (id, len) = loop {
            let id: [u8; 4] = r.take(4)?.try_into().expect("four bytes");
            let len = r.u32()? as usize;
            if &id == b"MTrk" {
                break (id, len);
            }
            r.take(len)?;
        };
        debug_assert_eq!(&id, b"MTrk");
        let mut t = Reader {
            bytes: r.take(len)?,
            pos: 0,
        };
        let mut tick = 0u64;
        let mut status = 0u8;
        // (channel, key) -> starts of notes sounding, oldest first.
        let mut open: Vec<((u8, u8), u64, u8)> = Vec::new();
        while t.pos < t.bytes.len() {
            tick += u64::from(t.vlq()?);
            let mut b = t.u8()?;
            if b < 0x80 {
                // Running status: reuse the last channel status.
                if status == 0 {
                    return Err(SmfError::Malformed);
                }
                t.pos -= 1;
                b = status;
            }
            match b {
                0xff => {
                    let kind = t.u8()?;
                    let len = t.vlq()? as usize;
                    t.take(len)?;
                    if kind == 0x2f {
                        break;
                    }
                }
                0xf0 | 0xf7 => {
                    let len = t.vlq()? as usize;
                    t.take(len)?;
                }
                0x80..=0xef => {
                    status = b;
                    let kind = b & 0xf0;
                    let a = t.u8()? & 0x7f;
                    let v = if matches!(kind, 0xc0 | 0xd0) {
                        0
                    } else {
                        t.u8()? & 0x7f
                    };
                    let key = (b & 0x0f, a);
                    if kind == 0x90 && v > 0 {
                        if open.len() >= MAX_HELD {
                            return Err(SmfError::Malformed);
                        }
                        open.push((key, tick, v));
                    } else if kind == 0x80 || kind == 0x90 {
                        if let Some(i) = open.iter().position(|o| o.0 == key) {
                            let (_, start, velocity) = open.remove(i);
                            notes.push((start, tick, a, velocity));
                        }
                    }
                }
                _ => return Err(SmfError::Malformed),
            }
        }
        for ((_, a), start, velocity) in open {
            notes.push((start, tick, a, velocity));
        }
    }
    let notes = notes
        .into_iter()
        .map(|(start, end, key, velocity)| {
            let (start, end) = (scale(start), scale(end));
            Note {
                start,
                length: end.saturating_sub(start).max(1),
                key,
                velocity,
            }
        })
        .filter(Note::is_valid)
        .collect();
    Ok(crate::sorted(notes))
}

fn vlq(out: &mut Vec<u8>, mut v: u32) {
    let mut bytes = [0u8; 4];
    let mut n = 0;
    loop {
        bytes[n] = (v & 0x7f) as u8;
        n += 1;
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        out.push(bytes[i] | if i > 0 { 0x80 } else { 0 });
    }
}

/// Writes `notes` as a format 0 file at `PPQ` with one tempo, channel 1.
pub fn write(notes: &[Note], bpm: f64) -> Vec<u8> {
    let mut events: Vec<(u64, [u8; 3])> = Vec::with_capacity(notes.len() * 2);
    for n in notes.iter().filter(|n| n.is_valid()) {
        events.push((n.start, [0x90, n.key, n.velocity]));
        events.push((n.end(), [0x80, n.key, 0]));
    }
    events.sort_by_key(|e| (e.0, e.1[0]));
    let mut track = Vec::new();
    // 24 bits of microseconds per beat: 3.58 BPM is the slowest a file can say.
    let us = (60_000_000.0 / bpm.clamp(1.0, 1000.0))
        .round()
        .min(f64::from(0xff_ffffu32)) as u32;
    track.extend([0x00, 0xff, 0x51, 0x03]);
    track.extend(&us.to_be_bytes()[1..]);
    let mut last = 0u64;
    for (tick, data) in events {
        vlq(
            &mut track,
            (tick - last).min(u64::from(u32::MAX >> 4)) as u32,
        );
        track.extend(data);
        last = tick;
    }
    track.extend([0x00, 0xff, 0x2f, 0x00]);
    let mut out = Vec::with_capacity(track.len() + 22);
    out.extend(b"MThd");
    out.extend(6u32.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(1u16.to_be_bytes());
    out.extend((PPQ as u16).to_be_bytes());
    out.extend(b"MTrk");
    out.extend((track.len() as u32).to_be_bytes());
    out.extend(track);
    debug_assert!(MAX_TICKS < u64::from(u32::MAX >> 4));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::STEP;

    fn n(start: u64, length: u64, key: u8, velocity: u8) -> Note {
        Note {
            start,
            length,
            key,
            velocity,
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let notes = vec![n(0, STEP, 60, 100), n(0, PPQ, 64, 80), n(PPQ, STEP, 60, 1)];
        assert_eq!(read(&write(&notes, 128.0)).unwrap(), crate::sorted(notes));
    }

    #[test]
    fn reads_running_status_other_divisions_and_format_1() {
        // Format 1, 96 ticks per quarter, two tracks: a tempo track and a note
        // track using running status and velocity-0 note-offs.
        let mut f = Vec::new();
        f.extend(b"MThd\0\0\0\x06\0\x01\0\x02\0\x60");
        let tempo = [
            0x00, 0xff, 0x51, 0x03, 0x07, 0xa1, 0x20, 0x00, 0xff, 0x2f, 0x00,
        ];
        f.extend(b"MTrk");
        f.extend((tempo.len() as u32).to_be_bytes());
        f.extend(tempo);
        let notes = [
            0x00, 0x91, 60, 90, // on, channel 2
            0x30, 62, 70, // running status: another on, 48 ticks later
            0x30, 60, 0, // velocity 0: off for 60
            0x81, 0x00, 62, 0, // 128 ticks later, off for 62 (vlq 2 bytes)
            0x00, 0xff, 0x2f, 0x00,
        ];
        f.extend(b"MTrk");
        f.extend((notes.len() as u32).to_be_bytes());
        f.extend(notes);
        let read = read(&f).unwrap();
        assert_eq!(
            read,
            [n(0, 2 * 480, 60, 90), n(480, (48 + 128) * 10, 62, 70)]
        );
    }

    #[test]
    fn rejects_garbage_and_smpte() {
        assert_eq!(read(b"RIFF"), Err(SmfError::Malformed));
        assert_eq!(read(b"MThd\0\0\0\x06\0\0\0\x01"), Err(SmfError::Malformed));
        assert_eq!(
            read(b"MThd\0\0\0\x06\0\0\0\x01\xe7\x28"),
            Err(SmfError::Smpte)
        );
        // A truncated track.
        assert_eq!(
            read(b"MThd\0\0\0\x06\0\0\0\x01\0\x60MTrk\0\0\0\x10\0\x90"),
            Err(SmfError::Malformed)
        );
    }
}
