use daw_core::{Command, TimeSignature};

/// Playback position and musical time. Owned by the audio thread and changed only
/// through `Command`s, so it needs no synchronization.
#[derive(Debug, Clone, PartialEq)]
pub struct Transport {
    playhead: u64,
    playing: bool,
    bpm: f64,
    time_signature: TimeSignature,
    /// `(start, end)` with `start < end`.
    looping: Option<(u64, u64)>,
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            playhead: 0,
            playing: false,
            bpm: 120.0,
            time_signature: TimeSignature::default(),
            looping: None,
        }
    }
}

impl Transport {
    /// Playhead position in samples.
    pub fn playhead(&self) -> u64 {
        self.playhead
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn bpm(&self) -> f64 {
        self.bpm
    }

    pub fn time_signature(&self) -> TimeSignature {
        self.time_signature
    }

    /// Invalid tempos (non-finite or not positive) and time signatures with a zero
    /// field are ignored, so a bad command can never corrupt the audio thread's state.
    pub fn apply(&mut self, command: Command) {
        match command {
            Command::Play => self.playing = true,
            Command::Stop => self.playing = false,
            Command::Seek(sample) => self.playhead = sample,
            Command::SetTempo(bpm) if bpm.is_finite() && bpm > 0.0 => self.bpm = bpm,
            Command::SetTimeSignature(ts) if ts.numerator > 0 && ts.denominator > 0 => {
                self.time_signature = ts
            }
            Command::SetLoop(range) => {
                self.looping = range.filter(|(start, end)| start < end);
            }
            Command::SetTempo(_) | Command::SetTimeSignature(_) => {}
        }
    }

    pub fn loop_range(&self) -> Option<(u64, u64)> {
        self.looping
    }

    /// Frames until the playhead wraps to the loop start, if it will: render no
    /// more than this in one go so the wrap falls between blocks. Always at least 1.
    pub fn frames_until_wrap(&self) -> Option<u64> {
        let (_, end) = self.looping?;
        (self.playing && self.playhead < end).then(|| end - self.playhead)
    }

    /// Moves the playhead past a rendered block, wrapping at the loop end. Does
    /// nothing while stopped.
    pub fn advance(&mut self, frames: u64) {
        if !self.playing {
            return;
        }
        let before = self.playhead;
        self.playhead = before.saturating_add(frames);
        if let Some((start, end)) = self.looping {
            if before < end && self.playhead >= end {
                self.playhead = start + (self.playhead - end) % (end - start);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loops_wrap_at_the_end_and_bad_loops_are_ignored() {
        let mut t = Transport::default();
        t.apply(Command::SetLoop(Some((100, 100))));
        assert_eq!(t.loop_range(), None);
        t.apply(Command::SetLoop(Some((100, 300))));
        t.apply(Command::Play);
        t.apply(Command::Seek(250));
        assert_eq!(t.frames_until_wrap(), Some(50));
        t.advance(50);
        assert_eq!(t.playhead(), 100);
        t.advance(450); // one whole loop and 50 more, from a block too long to split
        assert_eq!(t.playhead(), 150);
        // Past the end the transport plays on.
        t.apply(Command::Seek(400));
        assert_eq!(t.frames_until_wrap(), None);
        t.advance(10);
        assert_eq!(t.playhead(), 410);
        t.apply(Command::SetLoop(None));
        t.apply(Command::Seek(250));
        t.advance(100);
        assert_eq!(t.playhead(), 350);
    }

    #[test]
    fn advances_only_while_playing() {
        let mut t = Transport::default();
        t.advance(128);
        assert_eq!(t.playhead(), 0);

        t.apply(Command::Play);
        t.advance(128);
        t.advance(64);
        assert_eq!(t.playhead(), 192);

        t.apply(Command::Stop);
        t.advance(128);
        assert_eq!(t.playhead(), 192);
    }

    #[test]
    fn seek_sets_absolute_position() {
        let mut t = Transport::default();
        t.apply(Command::Seek(48_000));
        assert_eq!(t.playhead(), 48_000);
    }

    #[test]
    fn rejects_invalid_tempo_and_time_signature() {
        let mut t = Transport::default();
        t.apply(Command::SetTempo(140.0));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            t.apply(Command::SetTempo(bad));
        }
        assert_eq!(t.bpm(), 140.0);

        let waltz = TimeSignature {
            numerator: 3,
            denominator: 4,
        };
        t.apply(Command::SetTimeSignature(waltz));
        t.apply(Command::SetTimeSignature(TimeSignature {
            numerator: 0,
            denominator: 4,
        }));
        assert_eq!(t.time_signature(), waltz);
    }
}
