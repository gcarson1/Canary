//! Canary's subtractive synth: two band-limited oscillators per voice, a
//! resonant low-pass with its own envelope, an amp ADSR and glide. 16 voices.

use std::f32::consts::{PI, TAU};
use std::sync::Arc;

use daw_core::{MidiEvent, ProcessContext};
use daw_dsp::effects::ParamSpec;

use crate::{AudioNode, ParamBank};

const VOICES: usize = 16;
/// Keeps a chord of loud notes clear of clipping.
const VOICE_GAIN: f32 = 0.18;
/// Filter coefficients follow the envelope every this many samples.
const CONTROL: usize = 16;

const fn spec(name: &'static str, min: f32, max: f32, default: f32) -> ParamSpec {
    ParamSpec {
        name,
        min,
        max,
        default,
        log: false,
    }
}

pub const WAVE: usize = 0;
pub const OSC2: usize = 1;
pub const DETUNE: usize = 2;
pub const MIX2: usize = 3;
pub const CUTOFF: usize = 4;
pub const RESO: usize = 5;
pub const ENV_AMT: usize = 6;
pub const ATTACK: usize = 7;
pub const DECAY: usize = 8;
pub const SUSTAIN: usize = 9;
pub const RELEASE: usize = 10;
pub const GLIDE: usize = 11;
pub const GAIN: usize = 12;

/// The synth's parameters, in id order.
pub const SYNTH_PARAMS: [ParamSpec; 13] = [
    // 0 saw, 1 square, 2 triangle, 3 sine.
    spec("WAVE", 0.0, 3.0, 0.0),
    spec("OSC2 ST", -24.0, 24.0, 0.0),
    spec("DETUNE", 0.0, 50.0, 8.0),
    spec("OSC2 MIX", 0.0, 1.0, 0.5),
    ParamSpec {
        name: "CUTOFF",
        min: 30.0,
        max: 18_000.0,
        default: 2_500.0,
        log: true,
    },
    spec("RESO", 0.5, 10.0, 1.0),
    spec("ENV AMT", 0.0, 6.0, 2.0),
    spec("ATTACK", 0.5, 4_000.0, 5.0),
    spec("DECAY", 5.0, 4_000.0, 300.0),
    spec("SUSTAIN", 0.0, 1.0, 0.6),
    spec("RELEASE", 5.0, 8_000.0, 250.0),
    spec("GLIDE", 0.0, 1_000.0, 0.0),
    spec("GAIN", -24.0, 12.0, 0.0),
];

#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Stage {
    #[default]
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// Linear-attack, exponential decay/release envelope.
#[derive(Debug, Clone, Copy, Default)]
struct Adsr {
    stage: Stage,
    level: f32,
}

impl Adsr {
    fn tick(&mut self, attack: f32, decay: f32, sustain: f32, release: f32) -> f32 {
        match self.stage {
            Stage::Idle => {}
            Stage::Attack => {
                self.level += attack;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level = sustain + (self.level - sustain) * decay;
                if (self.level - sustain).abs() < 1e-4 {
                    self.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => self.level = sustain,
            Stage::Release => {
                self.level *= release;
                if self.level < 1e-4 {
                    self.level = 0.0;
                    self.stage = Stage::Idle;
                }
            }
        }
        self.level
    }
}

/// PolyBLEP: smooths a saw or square step `t` (phase 0..1) wide `dt`.
fn blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let t = t / dt;
        2.0 * t - t * t - 1.0
    } else if t > 1.0 - dt {
        let t = (t - 1.0) / dt;
        t * t + 2.0 * t + 1.0
    } else {
        0.0
    }
}

/// One oscillator sample at `phase` (0..1) advancing by `dt` a sample.
fn osc(wave: u32, phase: f32, dt: f32) -> f32 {
    match wave {
        0 => 2.0 * phase - 1.0 - blep(phase, dt),
        1 => {
            let sq = if phase < 0.5 { 1.0 } else { -1.0 };
            sq + blep(phase, dt) - blep((phase + 0.5) % 1.0, dt)
        }
        2 => 1.0 - 4.0 * (phase - 0.5).abs(),
        _ => (TAU * phase).sin(),
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Voice {
    note: u8,
    held: bool,
    velocity: f32,
    /// Current and target pitch in Hz (glide moves one toward the other).
    freq: f32,
    target: f32,
    phase: [f32; 2],
    amp: Adsr,
    filt: Adsr,
    /// TPT state-variable low-pass state and coefficients.
    ic: [f32; 2],
    g: f32,
    k: f32,
    age: u64,
}

impl Voice {
    fn is_free(&self) -> bool {
        self.amp.stage == Stage::Idle
    }
}

/// A 16-voice subtractive synth. Plays note on/off on any channel; CC 120 and
/// 123 release every note. Parameters come from a shared `ParamBank`, so the
/// rack and automation move them while it plays.
pub struct PolySynthNode {
    params: Arc<ParamBank>,
    voices: [Voice; VOICES],
    /// Last note's pitch, where the next glides from.
    last_freq: f32,
    clock: u64,
}

impl PolySynthNode {
    pub fn new() -> (Self, Arc<ParamBank>) {
        let params = Arc::new(ParamBank::new(SYNTH_PARAMS.to_vec()));
        (
            Self {
                params: params.clone(),
                voices: [Voice::default(); VOICES],
                last_freq: 0.0,
                clock: 0,
            },
            params,
        )
    }

    fn p(&self, id: usize) -> f32 {
        self.params.get(id).unwrap_or(SYNTH_PARAMS[id].default)
    }

    fn handle(&mut self, data: [u8; 3]) {
        let [status, note, value] = data;
        match (status & 0xF0, value) {
            (0x90, v) if v > 0 => {
                // A free voice, else the oldest released one, else the oldest.
                let i = (0..VOICES)
                    .min_by_key(|&i| {
                        let v = &self.voices[i];
                        (!v.is_free(), v.held, v.age)
                    })
                    .expect("VOICES > 0");
                let target = 440.0 * 2f32.powf((f32::from(note & 0x7F) - 69.0) / 12.0);
                let glide = self.p(GLIDE) > 0.0 && self.last_freq > 0.0;
                self.clock += 1;
                let v = &mut self.voices[i];
                *v = Voice {
                    note: note & 0x7F,
                    held: true,
                    velocity: f32::from(v_clamp(value)) / 127.0,
                    freq: if glide { self.last_freq } else { target },
                    target,
                    amp: Adsr {
                        stage: Stage::Attack,
                        level: 0.0,
                    },
                    filt: Adsr {
                        stage: Stage::Attack,
                        level: 0.0,
                    },
                    age: self.clock,
                    ..Voice::default()
                };
                self.last_freq = target;
            }
            (0x80 | 0x90, _) => {
                for v in self
                    .voices
                    .iter_mut()
                    .filter(|v| v.held && v.note == note & 0x7F)
                {
                    v.held = false;
                    v.amp.stage = Stage::Release;
                    v.filt.stage = Stage::Release;
                }
            }
            (0xB0, _) if note == 120 || note == 123 => {
                for v in &mut self.voices {
                    v.held = false;
                    if v.amp.stage != Stage::Idle {
                        v.amp.stage = Stage::Release;
                        v.filt.stage = Stage::Release;
                    }
                }
            }
            _ => {}
        }
    }
}

fn v_clamp(v: u8) -> u8 {
    v.clamp(1, 127)
}

impl AudioNode for PolySynthNode {
    fn process(
        &mut self,
        _inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi_in: &[MidiEvent],
        _midi_out: &mut Vec<MidiEvent>,
        context: &ProcessContext,
    ) {
        let sr = context.sample_rate.max(1.0);
        let n = context.num_samples;
        let wave = self.p(WAVE).round() as u32;
        let osc2 = 2f32.powf((self.p(OSC2) + self.p(DETUNE) / 100.0) / 12.0);
        let mix2 = self.p(MIX2);
        let cutoff = self.p(CUTOFF);
        let k = 1.0 / self.p(RESO).max(0.5);
        let env_amt = self.p(ENV_AMT);
        // Decay, release and glide times are to -60 dB.
        let per = |ms: f32| (-6.9 / (ms / 1000.0 * sr).max(1.0)).exp();
        let attack = 1.0 / (self.p(ATTACK) / 1000.0 * sr).max(1.0);
        let (decay, sustain, release) = (per(self.p(DECAY)), self.p(SUSTAIN), per(self.p(RELEASE)));
        let glide = per(self.p(GLIDE).max(0.01));
        let gain = 10f32.powf(self.p(GAIN) / 20.0) * VOICE_GAIN;

        let Some((first, rest)) = outputs.split_first_mut() else {
            return;
        };
        first.fill(0.0);
        let mut events = midi_in.iter().peekable();
        for i in 0..n {
            while let Some(e) = events.next_if(|e| e.sample_offset as usize <= i) {
                self.handle(e.data);
            }
            let mut sum = 0.0;
            for v in self.voices.iter_mut().filter(|v| !v.is_free()) {
                v.freq = v.target + (v.freq - v.target) * glide;
                let a = v.amp.tick(attack, decay, sustain, release);
                let f = v.filt.tick(attack, decay, sustain, release);
                // A voice started between control ticks has no filter yet.
                if i % CONTROL == 0 || v.g == 0.0 {
                    let fc = (cutoff * 2f32.powf(env_amt * f)).clamp(20.0, 0.45 * sr);
                    v.g = (PI * fc / sr).tan();
                    v.k = k;
                }
                let mut s = 0.0;
                for (o, ratio) in [(0, 1.0), (1, osc2)] {
                    let dt = (v.freq * ratio / sr).min(0.5);
                    let level = if o == 0 { 1.0 - mix2 * 0.5 } else { mix2 };
                    s += osc(wave, v.phase[o], dt) * level;
                    v.phase[o] = (v.phase[o] + dt) % 1.0;
                }
                // TPT state-variable filter (Zavalishin), low-pass output.
                let (g, kk) = (v.g, v.k);
                let a1 = 1.0 / (1.0 + g * (g + kk));
                let v3 = s - v.ic[1];
                let v1 = a1 * v.ic[0] + g * a1 * v3;
                let v2 = v.ic[1] + g * v1;
                v.ic = [2.0 * v1 - v.ic[0], 2.0 * v2 - v.ic[1]];
                sum += v2 * a * v.velocity;
            }
            first[i] = sum * gain;
        }
        for e in events {
            self.handle(e.data);
        }
        for out in rest {
            out.copy_from_slice(first);
        }
    }

    fn automate(&mut self, id: u32, value: f64) {
        self.params.set(id as usize, value as f32);
    }

    fn reset(&mut self) {
        self.voices = [Voice::default(); VOICES];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_no_alloc::{assert_no_alloc, AllocDisabler};

    #[global_allocator]
    static ALLOC: AllocDisabler = AllocDisabler;

    fn ctx(n: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: 48_000.0,
            num_samples: n,
            playhead: 0,
            playing: true,
            ..Default::default()
        }
    }

    fn block(node: &mut PolySynthNode, midi: &[MidiEvent]) -> Vec<f32> {
        let (mut l, mut r) = (vec![0.0; 4_800], vec![0.0; 4_800]);
        assert_no_alloc(|| {
            node.process(
                &[],
                &mut [&mut l, &mut r],
                midi,
                &mut Vec::new(),
                &ctx(4_800),
            )
        });
        assert_eq!(l, r);
        l
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0, |m, s| m.max(s.abs()))
    }

    #[test]
    fn notes_sound_release_and_fall_silent() {
        let (mut node, params) = PolySynthNode::new();
        params.set(RELEASE, 20.0);
        let on = MidiEvent {
            sample_offset: 100,
            data: [0x90, 60, 100],
        };
        let out = block(&mut node, &[on]);
        assert!(
            out[..100].iter().all(|&s| s == 0.0),
            "nothing before the note"
        );
        assert!(peak(&out[2_000..]) > 0.05);
        assert!(out.iter().all(|s| s.is_finite() && s.abs() < 1.0));
        let off = MidiEvent {
            sample_offset: 0,
            data: [0x80, 60, 0],
        };
        block(&mut node, &[off]);
        assert_eq!(peak(&block(&mut node, &[])), 0.0, "released within 100 ms");
    }

    #[test]
    fn a_lower_cutoff_darkens_and_more_voices_than_slots_steal() {
        // A C6 saw through a 200 Hz low-pass is far quieter than through 10 kHz.
        let level = |cutoff: f32| {
            let (mut node, params) = PolySynthNode::new();
            params.set(CUTOFF, cutoff);
            params.set(ENV_AMT, 0.0);
            let on = MidiEvent {
                sample_offset: 0,
                data: [0x90, 84, 127],
            };
            peak(&block(&mut node, &[on])[2_400..])
        };
        assert!(level(200.0) < level(10_000.0) * 0.3);
        let (mut node, _) = PolySynthNode::new();
        let chord: Vec<MidiEvent> = (0..20)
            .map(|k| MidiEvent {
                sample_offset: k,
                data: [0x90, 40 + k as u8, 100],
            })
            .collect();
        let out = block(&mut node, &chord);
        assert!(out.iter().all(|s| s.is_finite()));
        assert_eq!(node.voices.iter().filter(|v| v.held).count(), VOICES);
    }
}
