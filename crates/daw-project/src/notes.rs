//! Notes, FL-style patterns and pattern clips, and turning them into the
//! timeline-sample MIDI sequences the engine plays.
//!
//! Musical positions are ticks at [`PPQ`] per quarter note, so they survive tempo
//! changes; `Project::samples_at` converts them.

use daw_core::SeqEvent;
use serde::{Deserialize, Serialize};

use crate::Project;

/// Ticks per quarter note.
pub const PPQ: u64 = 960;
/// One step of the step sequencer: a sixteenth note.
pub const STEP: u64 = PPQ / 4;

/// One note. Valid notes have `length >= 1`, `key <= 127` and `velocity` in 1..=127.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Note {
    /// Ticks from the start of the pattern.
    pub start: u64,
    /// Ticks.
    pub length: u64,
    pub key: u8,
    pub velocity: u8,
}

impl Note {
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.length)
    }

    pub fn is_valid(&self) -> bool {
        self.length >= 1 && self.key <= 127 && (1..=127).contains(&self.velocity)
    }
}

/// A pattern: a named stretch of time that every track can hold notes in (FL
/// Studio's model, where one pattern plays many channels).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pattern {
    /// Unique in the project; notes and clips refer to the pattern by it.
    pub id: u64,
    pub name: String,
    /// Ticks, at least 1. Pattern mode loops this long.
    pub length: u64,
}

/// A track's notes in one pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternNotes {
    pub pattern: u64,
    pub notes: Vec<Note>,
}

/// A pattern placed on a playlist row. It plays the pattern's notes on every track,
/// repeating the pattern for as long as the clip lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternClip {
    pub pattern: u64,
    /// Timeline position in ticks.
    pub start: u64,
    /// Ticks, at least 1.
    pub length: u64,
    /// Ticks into the pattern where the clip begins (a trimmed left edge).
    #[serde(default)]
    pub offset: u64,
}

impl PatternClip {
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.length)
    }
}

/// What plays: one pattern looped (FL's PAT mode) or the playlist (SONG).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    Pattern(u64),
    Song,
}

impl Project {
    pub fn pattern(&self, id: u64) -> Option<&Pattern> {
        self.patterns.iter().find(|p| p.id == id)
    }

    /// Timeline samples at tick `ticks`, at the project tempo.
    pub fn samples_at(&self, ticks: u64) -> u64 {
        let per_tick = 60.0 * f64::from(self.sample_rate) / (self.bpm * PPQ as f64);
        (ticks as f64 * per_tick).round() as u64
    }

    /// The tick at timeline sample `sample` (rounded down).
    pub fn ticks_at(&self, sample: u64) -> u64 {
        let per_sample = self.bpm * PPQ as f64 / (60.0 * f64::from(self.sample_rate));
        (sample as f64 * per_sample + 1e-6).floor() as u64
    }

    /// The MIDI that `track`'s instrument plays, in timeline samples, sorted with
    /// note-offs before note-ons at the same sample. In pattern mode the pattern
    /// starts at 0 (the engine loops it); notes past the pattern's end are cut.
    pub fn sequence(&self, track: usize, playback: Playback) -> Vec<SeqEvent> {
        let Some(t) = self.tracks.get(track) else {
            return Vec::new();
        };
        // (start tick, end tick, key, velocity)
        let mut notes: Vec<(u64, u64, u8, u8)> = Vec::new();
        match playback {
            Playback::Pattern(id) => {
                if let Some(p) = self.pattern(id) {
                    for n in t.notes_in(id).iter().filter(|n| n.start < p.length) {
                        notes.push((n.start, n.end().min(p.length), n.key, n.velocity));
                    }
                }
            }
            Playback::Song => {
                let clips = self.tracks.iter().flat_map(|row| &row.pattern_clips);
                for clip in clips {
                    let (Some(p), list) = (self.pattern(clip.pattern), t.notes_in(clip.pattern))
                    else {
                        continue;
                    };
                    if list.is_empty() {
                        continue;
                    }
                    // Pattern repeats overlapping the clip: repeat k starts at
                    // clip time k * length - offset % length.
                    let first = clip.offset % p.length;
                    let mut rep_start = clip.start as i128 - first as i128;
                    while rep_start < clip.end() as i128 {
                        for n in list.iter().filter(|n| n.start < p.length) {
                            let on = rep_start + n.start as i128;
                            let off = rep_start + n.end().min(p.length) as i128;
                            // A note cut off by the clip's left edge does not play.
                            let off = off.min(clip.end() as i128);
                            if on >= clip.start as i128 && on < off {
                                notes.push((on as u64, off as u64, n.key, n.velocity));
                            }
                        }
                        rep_start += p.length as i128;
                    }
                }
            }
        }
        let mut events = Vec::with_capacity(notes.len() * 2);
        for (on, off, key, velocity) in notes {
            let (on, off) = (self.samples_at(on), self.samples_at(off));
            if off <= on {
                continue;
            }
            events.push(SeqEvent {
                at: on,
                data: [0x90, key & 0x7f, velocity.clamp(1, 127)],
            });
            events.push(SeqEvent {
                at: off,
                data: [0x80, key & 0x7f, 0],
            });
        }
        // Offs (0x80) sort before ons (0x90) at the same sample.
        events.sort_by_key(|e| (e.at, e.data[0], e.data[1]));
        events
    }
}

/// Notes as they look after an edit, sorted by start then key, so two lists with
/// the same notes compare equal.
pub fn sorted(mut notes: Vec<Note>) -> Vec<Note> {
    notes.sort_by_key(|n| (n.start, n.key, n.length, n.velocity));
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Track;

    fn note(start: u64, length: u64, key: u8) -> Note {
        Note {
            start,
            length,
            key,
            velocity: 100,
        }
    }

    /// 120 BPM at 48 kHz: a quarter note (960 ticks) is 24 000 samples, 25 per tick.
    fn project() -> Project {
        let mut p = Project {
            patterns: vec![Pattern {
                id: 7,
                name: "P1".into(),
                length: 4 * PPQ,
            }],
            ..Project::default()
        };
        let mut t = Track::new("Drums");
        t.notes.push(PatternNotes {
            pattern: 7,
            notes: vec![note(0, PPQ, 36), note(3 * PPQ, 2 * PPQ, 38)],
        });
        p.tracks.push(t);
        p
    }

    #[test]
    fn ticks_and_samples_convert_at_the_tempo() {
        let p = project();
        assert_eq!(p.samples_at(PPQ), 24_000);
        assert_eq!(p.ticks_at(24_000), PPQ);
        assert_eq!(p.ticks_at(24_024), PPQ);
        assert_eq!(p.ticks_at(24_025), PPQ + 1);
    }

    #[test]
    fn pattern_mode_starts_at_zero_and_cuts_notes_at_the_pattern_end() {
        let ev = project().sequence(0, Playback::Pattern(7));
        let want = [
            (0, [0x90, 36, 100]),
            (24_000, [0x80, 36, 0]),
            (72_000, [0x90, 38, 100]),
            (96_000, [0x80, 38, 0]), // cut from 5 beats to 4
        ];
        let got: Vec<_> = ev.iter().map(|e| (e.at, e.data)).collect();
        assert_eq!(got, want);
        assert!(project().sequence(0, Playback::Pattern(99)).is_empty());
        assert!(project().sequence(5, Playback::Song).is_empty());
    }

    #[test]
    fn song_mode_repeats_patterns_inside_clips_from_any_row() {
        let mut p = project();
        let mut row = Track::new("Row 2");
        // Two bars long, starting a beat into the pattern, at beat 8.
        row.pattern_clips.push(PatternClip {
            pattern: 7,
            start: 8 * PPQ,
            length: 8 * PPQ,
            offset: PPQ,
        });
        p.tracks.push(row);
        let ons: Vec<_> = p
            .sequence(0, Playback::Song)
            .iter()
            .filter(|e| e.data[0] == 0x90)
            .map(|e| (p.ticks_at(e.at) / PPQ, e.data[1]))
            .collect();
        // Pattern time 1 at beat 8: snare (pattern beat 3) at 10, kick at 11, snare at 14,
        // kick at 15; the kick of the first repeat (pattern beat 0) falls before the clip.
        assert_eq!(ons, [(10, 38), (11, 36), (14, 38), (15, 36)]);
        // The track without notes in the pattern plays nothing.
        assert!(p.sequence(1, Playback::Song).is_empty());
    }

    #[test]
    fn offs_sort_before_ons_so_repeated_notes_retrigger() {
        let mut p = project();
        p.tracks[0].notes[0].notes = vec![note(0, PPQ, 60), note(PPQ, PPQ, 60)];
        let ev = p.sequence(0, Playback::Pattern(7));
        assert_eq!(ev[1].at, ev[2].at);
        assert_eq!((ev[1].data[0], ev[2].data[0]), (0x80, 0x90));
    }
}
