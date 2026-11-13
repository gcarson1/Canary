//! Sequenced MIDI for a plan step: plays a timeline-sample note sequence in step
//! with the transport, merged with the live MIDI input, and never leaves a note
//! hanging when playback stops, jumps or the sequence changes.

use daw_core::{MidiEvent, ProcessContext, SeqEvent};

/// Which notes a step's sequence has started and not yet ended, and where the
/// transport should be next block if playback runs on without a jump.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct NoteState {
    /// Bit `key` of `held[channel]`.
    held: [u128; 16],
    next: Option<u64>,
    /// Release every held note at the next block (the sequence was replaced).
    flush: bool,
}

impl NoteState {
    fn note(&mut self, data: [u8; 3]) {
        let (channel, bit) = ((data[0] & 0x0f) as usize, 1u128 << (data[1] & 0x7f));
        match data[0] & 0xf0 {
            0x90 if data[2] > 0 => self.held[channel] |= bit,
            0x80 | 0x90 => self.held[channel] &= !bit,
            _ => {}
        }
    }

    /// Carries state over from the same step in the plan being replaced; held
    /// notes are released next block unless the sequence is unchanged.
    pub(crate) fn adopt(&mut self, old: &NoteState, same_sequence: bool) {
        *self = *old;
        self.flush |= !same_sequence;
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.held.iter().all(|&h| h == 0)
    }

    /// Writes this block's MIDI into `out` (cleared first): note-offs for held notes
    /// at offset 0 if playback stopped or jumped, then the sequence's events in the
    /// block merged in time order with `live`. Never grows `out` past its capacity;
    /// events that do not fit are dropped.
    pub(crate) fn render(
        &mut self,
        sequence: &[SeqEvent],
        context: &ProcessContext,
        live: &[MidiEvent],
        out: &mut Vec<MidiEvent>,
    ) {
        out.clear();
        let push = |out: &mut Vec<MidiEvent>, e: MidiEvent| {
            if out.len() < out.capacity() {
                out.push(e);
            }
        };
        let jumped = !context.playing || self.next != Some(context.playhead) || self.flush;
        if jumped {
            for (channel, held) in self.held.iter_mut().enumerate() {
                // A key stays held until its note-off fits, so none is lost.
                while *held != 0 && out.len() < out.capacity() {
                    let key = held.trailing_zeros() as u8;
                    *held &= *held - 1;
                    let data = [0x80 | channel as u8, key, 0];
                    push(
                        out,
                        MidiEvent {
                            sample_offset: 0,
                            data,
                        },
                    );
                }
            }
            // Anything that did not fit is released next block.
            self.flush = !self.is_idle();
        }
        let mut live = live.iter().copied().peekable();
        if context.playing {
            let start = context.playhead;
            let end = start + context.num_samples as u64;
            let from = sequence.partition_point(|e| e.at < start);
            let to = sequence.partition_point(|e| e.at < end);
            for e in &sequence[from..to] {
                let offset = (e.at - start) as u32;
                while let Some(l) = live.next_if(|l| l.sample_offset <= offset) {
                    push(out, l);
                }
                if out.len() < out.capacity() {
                    self.note(e.data);
                }
                push(
                    out,
                    MidiEvent {
                        sample_offset: offset,
                        data: e.data,
                    },
                );
            }
            self.next = Some(end);
        } else {
            self.next = None;
        }
        for l in live {
            push(out, l);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(playhead: u64, playing: bool) -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            num_samples: 100,
            playhead,
            playing,
        }
    }

    fn on(at: u64, key: u8) -> SeqEvent {
        SeqEvent {
            at,
            data: [0x90, key, 100],
        }
    }

    fn off(at: u64, key: u8) -> SeqEvent {
        SeqEvent {
            at,
            data: [0x80, key, 0],
        }
    }

    fn run(
        s: &mut NoteState,
        seq: &[SeqEvent],
        c: ProcessContext,
        live: &[MidiEvent],
    ) -> Vec<(u32, [u8; 3])> {
        let mut out = Vec::with_capacity(16);
        s.render(seq, &c, live, &mut out);
        out.iter().map(|e| (e.sample_offset, e.data)).collect()
    }

    #[test]
    fn plays_events_at_their_offsets_merged_with_live_midi() {
        let seq = [on(10, 60), off(150, 60), on(150, 62)];
        let mut s = NoteState::default();
        let live = [MidiEvent {
            sample_offset: 50,
            data: [0x90, 70, 1],
        }];
        let got = run(&mut s, &seq, ctx(0, true), &live);
        assert_eq!(got, [(10, [0x90, 60, 100]), (50, [0x90, 70, 1])]);
        let got = run(&mut s, &seq, ctx(100, true), &[]);
        assert_eq!(got, [(50, [0x80, 60, 0]), (50, [0x90, 62, 100])]);
    }

    #[test]
    fn stop_jump_and_new_sequences_release_held_notes() {
        let seq = [on(0, 60), on(5, 64), off(1_000, 60), off(1_000, 64)];
        let mut s = NoteState::default();
        run(&mut s, &seq, ctx(0, true), &[]);
        // A seek (or loop wrap) back to 0: release both, then replay the start.
        let got = run(&mut s, &seq, ctx(0, true), &[]);
        assert_eq!(
            got,
            [
                (0, [0x80, 60, 0]),
                (0, [0x80, 64, 0]),
                (0, [0x90, 60, 100]),
                (5, [0x90, 64, 100])
            ]
        );
        // Stopping releases them once.
        assert_eq!(run(&mut s, &seq, ctx(100, false), &[]).len(), 2);
        assert!(run(&mut s, &seq, ctx(100, false), &[]).is_empty());
        assert!(s.is_idle());

        // A plan swap with a different sequence flushes; the same one plays on.
        run(&mut s, &seq, ctx(0, true), &[]);
        let mut same = NoteState::default();
        same.adopt(&s, true);
        assert!(run(&mut same, &seq, ctx(100, true), &[]).is_empty());
        let mut changed = NoteState::default();
        changed.adopt(&s, false);
        assert_eq!(run(&mut changed, &seq, ctx(100, true), &[]).len(), 2);
    }

    #[test]
    fn a_full_buffer_drops_events_without_tracking_them() {
        let seq: Vec<_> = (0..40).map(|k| on(k, k as u8)).collect();
        let mut s = NoteState::default();
        let got = run(&mut s, &seq, ctx(0, true), &[]);
        assert_eq!(got.len(), 16);
        // Only the 16 that were sent are held, so only they are released.
        assert_eq!(run(&mut s, &seq, ctx(500, true), &[]).len(), 16);
    }
}
