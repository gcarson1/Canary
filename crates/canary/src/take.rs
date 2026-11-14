//! MIDI recording: turns the engine's log of live MIDI into notes in a pattern,
//! overdubbed on what is there, as one undo step when the take ends.

use daw_core::SeqEvent;
use daw_project::{Note, Project};

pub struct MidiTake {
    pub track: usize,
    pub pattern: u64,
    /// Tracks in the project when the take began: if that changes, `track` may
    /// point at another track, so the take is not saved.
    pub tracks: usize,
    /// Pattern length in ticks; positions wrap at it, as the loop does.
    length: u64,
    /// Sounding notes by channel and key: start tick and velocity.
    open: Vec<((u8, u8), u64, u8)>,
    notes: Vec<Note>,
}

impl MidiTake {
    pub fn new(track: usize, tracks: usize, pattern: u64, length: u64) -> Self {
        Self {
            track,
            tracks,
            pattern,
            length: length.max(1),
            open: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Adds one logged event (timeline samples) to the take.
    pub fn add(&mut self, e: SeqEvent, project: &Project) {
        let tick = project.ticks_at(e.at) % self.length;
        let key = (e.data[0] & 0x0f, e.data[1] & 0x7f);
        match e.data[0] & 0xf0 {
            0x90 if e.data[2] > 0 => self.open.push((key, tick, e.data[2])),
            0x80 | 0x90 => {
                if let Some(i) = self.open.iter().position(|o| o.0 == key) {
                    let (_, start, velocity) = self.open.remove(i);
                    self.close(key.1, start, tick, velocity);
                }
            }
            _ => {}
        }
    }

    /// A note from `start` to `end`; one that wrapped past the loop end is cut
    /// at the pattern's end.
    fn close(&mut self, key: u8, start: u64, end: u64, velocity: u8) {
        let end = if end <= start { self.length } else { end };
        self.notes.push(Note {
            start,
            length: (end - start).max(1),
            key,
            velocity: velocity.clamp(1, 127),
        });
    }

    /// The pattern's notes with the take added. Keys still held end at the
    /// pattern's end.
    pub fn finish(mut self, existing: &[Note]) -> Vec<Note> {
        for ((_, key), start, velocity) in std::mem::take(&mut self.open) {
            self.close(key, start, self.length, velocity);
        }
        let mut notes = existing.to_vec();
        notes.extend(self.notes.into_iter().filter(Note::is_valid));
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daw_project::PPQ;

    #[test]
    fn notes_pair_up_wrap_at_the_loop_and_overdub() {
        // 120 BPM at 48 kHz: 25 samples per tick. A one-bar pattern.
        let project = Project::default();
        let bar = 4 * PPQ;
        let mut take = MidiTake::new(0, 1, 1, bar);
        let at = |tick: u64| tick * 25;
        let ev = |tick, status, key, vel| SeqEvent {
            at: at(tick),
            data: [status, key, vel],
        };
        take.add(ev(PPQ, 0x90, 60, 100), &project);
        take.add(ev(2 * PPQ, 0x90, 60, 0), &project); // velocity-0 off
        take.add(ev(bar + 3 * PPQ, 0x90, 64, 80), &project); // second pass
        take.add(ev(2 * bar + 10, 0x80, 64, 0), &project); // off after the wrap
        take.add(ev(bar + 100, 0x91, 67, 90), &project); // still held at the end
        let existing = [Note {
            start: 0,
            length: 10,
            key: 36,
            velocity: 100,
        }];
        let notes = take.finish(&existing);
        let n = |start, length, key, velocity| Note {
            start,
            length,
            key,
            velocity,
        };
        assert_eq!(
            notes,
            [
                existing[0],
                n(PPQ, PPQ, 60, 100),
                n(3 * PPQ, PPQ, 64, 80),
                n(100, bar - 100, 67, 90)
            ]
        );
    }
}

/// A loop recording that started at timeline sample `at` and ran `frames`
/// while the transport looped `[a, b)`: the clip that plays its latest full
/// pass over the loop, (start, offset, length, takes). None unless it holds at
/// least two full passes.
pub fn loop_passes(
    at: u64,
    frames: u64,
    (a, b): (u64, u64),
) -> Option<(u64, u64, u64, daw_project::Takes)> {
    if a >= b || at >= b {
        return None;
    }
    let len = b - a;
    // Where the first full pass begins in the file: the transport reaches a
    // from before the loop, or wraps to it at b.
    let first = if at <= a { a - at } else { b - at };
    let count = frames.checked_sub(first)? / len;
    if count < 2 {
        return None;
    }
    let count = u32::try_from(count).ok()?;
    let takes = daw_project::Takes { first, len, count };
    Some((a, first + u64::from(count - 1) * len, len, takes))
}

#[cfg(test)]
mod loop_tests {
    use super::*;

    #[test]
    fn a_loop_recording_becomes_its_last_full_pass_with_all_passes_kept() {
        // Loop 1000..2000, recorded from the loop start for 3.5 passes.
        let (start, offset, len, t) = loop_passes(1_000, 3_500, (1_000, 2_000)).unwrap();
        assert_eq!((start, offset, len), (1_000, 2_000, 1_000));
        assert_eq!((t.first, t.count), (0, 3));
        // Started mid-loop at 1500: the first full pass begins 500 frames in.
        let (_, offset, _, t) = loop_passes(1_500, 2_600, (1_000, 2_000)).unwrap();
        assert_eq!((t.first, t.count, offset), (500, 2, 1_500));
        // One pass is a plain take.
        assert!(loop_passes(1_000, 1_900, (1_000, 2_000)).is_none());
        assert!(
            loop_passes(2_500, 9_000, (1_000, 2_000)).is_none(),
            "started past it"
        );
    }

    #[test]
    fn a_comp_piece_switches_pass_and_keeps_its_place_in_the_loop() {
        let t = daw_project::Takes {
            first: 500,
            len: 1_000,
            count: 3,
        };
        // A piece 200 into the loop, showing pass 2.
        let clip = daw_project::ClipRef {
            source: "take.wav".into(),
            start: 1_200,
            offset: 500 + 2 * 1_000 + 200,
            length: 300,
            fade_in: 0,
            fade_out: 0,
            gain: 1.0,
            reverse: false,
            stretch: None,
            takes: Some(t),
        };
        assert_eq!(t.pass_of(clip.offset), 2);
        let first = clip.with_take(0).unwrap();
        assert_eq!((first.offset, first.start), (700, 1_200));
        assert!(clip.with_take(3).is_none());
    }
}
