use std::f32::consts::TAU;

use daw_core::{MidiEvent, ProcessContext};

use crate::AudioNode;

const VOICES: usize = 16;
const ATTACK_SECONDS: f32 = 0.005;
const RELEASE_SECONDS: f32 = 0.08;
/// Keeps a full chord of loud notes clear of clipping.
const VOICE_GAIN: f32 = 0.2;

#[derive(Debug, Clone, Copy, Default)]
struct Voice {
    note: u8,
    held: bool,
    /// Envelope, 0.0..=1.0. A voice with level 0 that is not held is free.
    level: f32,
    velocity: f32,
    phase: f32,
    step: f32,
}

impl Voice {
    fn is_free(&self) -> bool {
        !self.held && self.level == 0.0
    }
}

/// A small polyphonic sine instrument: the built-in sound source for instrument
/// tracks until plugins are hosted. Plays MIDI note on/off on any channel at the
/// sample each event is stamped with; CC 120 and 123 release every note.
///
/// Writes the same signal to every output channel. Feed it into a `TrackNode`'s
/// inputs for gain, pan, mute and solo.
#[derive(Debug, Default)]
pub struct SynthNode {
    voices: [Voice; VOICES],
}

impl SynthNode {
    pub fn new() -> Self {
        Self::default()
    }

    fn handle(&mut self, data: [u8; 3], sample_rate: f32) {
        let [status, note, value] = data;
        match (status & 0xF0, value) {
            (0x90, v) if v > 0 => {
                // Reuse a free voice, else steal the quietest.
                let voice = self
                    .voices
                    .iter_mut()
                    .min_by(|a, b| {
                        (!a.is_free())
                            .cmp(&!b.is_free())
                            .then(a.level.total_cmp(&b.level))
                    })
                    .expect("VOICES > 0");
                let freq = 440.0 * 2f32.powf((f32::from(note & 0x7F) - 69.0) / 12.0);
                *voice = Voice {
                    note,
                    held: true,
                    level: voice.level,
                    velocity: f32::from(v) / 127.0,
                    phase: voice.phase,
                    step: freq / sample_rate,
                };
            }
            (0x80, _) | (0x90, _) => {
                for v in self.voices.iter_mut().filter(|v| v.note == note) {
                    v.held = false;
                }
            }
            (0xB0, _) if note == 120 || note == 123 => {
                for v in self.voices.iter_mut() {
                    v.held = false;
                }
            }
            _ => {}
        }
    }

    fn render(&mut self, out: &mut [f32], sample_rate: f32) {
        let attack = 1.0 / (ATTACK_SECONDS * sample_rate);
        let release = 1.0 / (RELEASE_SECONDS * sample_rate);
        for v in self.voices.iter_mut().filter(|v| !v.is_free()) {
            for o in out.iter_mut() {
                v.level = if v.held {
                    (v.level + attack).min(1.0)
                } else {
                    (v.level - release).max(0.0)
                };
                *o += (v.phase * TAU).sin() * v.level * v.velocity * VOICE_GAIN;
                v.phase = (v.phase + v.step).fract();
            }
        }
    }
}

impl AudioNode for SynthNode {
    fn process(
        &mut self,
        _inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi_in: &[MidiEvent],
        _midi_out: &mut Vec<MidiEvent>,
        context: &ProcessContext,
    ) {
        let Some((first, rest)) = outputs.split_first_mut() else {
            return;
        };
        first.fill(0.0);
        let sr = context.sample_rate;
        let mut at = 0;
        for event in midi_in {
            let offset = (event.sample_offset as usize).clamp(at, first.len());
            self.render(&mut first[at..offset], sr);
            self.handle(event.data, sr);
            at = offset;
        }
        self.render(&mut first[at..], sr);
        for out in rest {
            out.copy_from_slice(first);
        }
    }

    fn reset(&mut self) {
        self.voices = Default::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(n: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            num_samples: n,
            playhead: 0,
            playing: false,
        }
    }

    fn run(synth: &mut SynthNode, midi: &[MidiEvent], n: usize) -> Vec<f32> {
        let (mut l, mut r) = (vec![9.0; n], vec![9.0; n]);
        synth.process(&[], &mut [&mut l, &mut r], midi, &mut Vec::new(), &ctx(n));
        assert_eq!(l, r);
        l
    }

    fn ev(sample_offset: u32, data: [u8; 3]) -> MidiEvent {
        MidiEvent {
            sample_offset,
            data,
        }
    }

    #[test]
    fn note_starts_at_its_sample_offset_and_releases_to_silence() {
        let mut s = SynthNode::new();
        let out = run(&mut s, &[ev(100, [0x90, 69, 127])], 256);
        assert!(
            out[..=100].iter().all(|&v| v == 0.0),
            "sound before the note"
        );
        assert!(out[101..].iter().any(|&v| v.abs() > 0.01));

        // Note off on another channel still releases it (omni).
        run(&mut s, &[ev(0, [0x83, 69, 0])], 256);
        let tail = run(&mut s, &[], 48_000 / 10); // longer than the release
        assert_eq!(*tail.last().unwrap(), 0.0);
        assert!(s.voices.iter().all(Voice::is_free));
    }

    #[test]
    fn velocity_zero_is_note_off_and_all_notes_off_releases_everything() {
        let mut s = SynthNode::new();
        run(
            &mut s,
            &[ev(0, [0x90, 60, 100]), ev(0, [0x90, 64, 100])],
            64,
        );
        run(&mut s, &[ev(0, [0x90, 60, 0])], 64);
        assert_eq!(s.voices.iter().filter(|v| v.held).count(), 1);
        run(&mut s, &[ev(0, [0xB0, 123, 0])], 64);
        assert!(s.voices.iter().all(|v| !v.held));
    }

    #[test]
    fn more_notes_than_voices_steal_and_stay_finite() {
        let mut s = SynthNode::new();
        let notes: Vec<_> = (0..40).map(|i| ev(i, [0x90, 30 + i as u8, 127])).collect();
        let out = run(&mut s, &notes, 512);
        assert!(out
            .iter()
            .all(|v| v.is_finite() && v.abs() <= VOICE_GAIN * VOICES as f32));
        assert_eq!(s.voices.iter().filter(|v| v.held).count(), VOICES);
        // Out-of-order or out-of-range offsets are clamped, not a panic.
        run(&mut s, &[ev(500, [0x80, 60, 0]), ev(3, [0x80, 61, 0])], 8);
    }
}
