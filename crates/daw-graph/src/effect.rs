//! Built-in effects as graph nodes: any `daw_dsp` `Effect` plus dry/wet, automatic
//! level matching and delta monitoring, with parameters shared lock-free with the
//! main thread.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use daw_core::{MidiEvent, ProcessContext};
use daw_dsp::effects::{Effect, ParamSpec};

use crate::AudioNode;

/// Parameters every built-in effect has, after its own.
const COMMON: [ParamSpec; 3] = [
    // Percent wet.
    ParamSpec {
        name: "MIX",
        min: 0.0,
        max: 100.0,
        default: 100.0,
        log: false,
    },
    // On (1): the wet signal is held at the dry signal's loudness.
    ParamSpec {
        name: "MATCH",
        min: 0.0,
        max: 1.0,
        default: 1.0,
        log: false,
    },
    // On (1): output only what the effect changes (wet minus dry).
    ParamSpec {
        name: "DELTA",
        min: 0.0,
        max: 1.0,
        default: 0.0,
        log: false,
    },
];

/// Seconds of loudness the level match averages; long, so it never pumps.
const MATCH_WINDOW: f32 = 1.0;
/// Below this mean square (-70 dBFS) the match gain holds instead of chasing noise.
const MATCH_FLOOR: f32 = 1e-7;
/// The match gain stays within ±24 dB.
const MATCH_LIMIT: f32 = 16.0;

/// An effect's parameter values, written by the main thread and read by the
/// audio thread each block. Ids are indexes into `specs`.
pub struct EffectParams {
    specs: Vec<ParamSpec>,
    values: Box<[AtomicU32]>,
}

impl EffectParams {
    fn new(own: &[ParamSpec]) -> Self {
        let specs: Vec<ParamSpec> = own.iter().chain(&COMMON).copied().collect();
        let values = specs
            .iter()
            .map(|p| AtomicU32::new(p.default.to_bits()))
            .collect();
        Self { specs, values }
    }

    pub fn specs(&self) -> &[ParamSpec] {
        &self.specs
    }

    pub fn get(&self, id: usize) -> Option<f32> {
        Some(f32::from_bits(self.values.get(id)?.load(Ordering::Relaxed)))
    }

    /// Clamps into the parameter's range. False for an unknown id or a value that
    /// is not finite.
    pub fn set(&self, id: usize, value: f32) -> bool {
        match (self.specs.get(id), self.values.get(id)) {
            (Some(spec), Some(slot)) if value.is_finite() => {
                let v = value.clamp(spec.min, spec.max);
                slot.store(v.to_bits(), Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }
}

pub struct EffectNode {
    effect: Box<dyn Effect>,
    params: Arc<EffectParams>,
    /// The values last handed to `effect`; NaN forces the first update.
    seen: Box<[f32]>,
    dry: [Box<[f32]>; 2],
    mix: f32,
    dry_ms: f32,
    wet_ms: f32,
    average: f32,
    gain: f32,
    glide: f32,
}

impl EffectNode {
    /// `effect` must already know the sample rate. Returns the node and the handle
    /// the main thread turns its knobs with.
    pub fn new(
        effect: Box<dyn Effect>,
        sample_rate: f32,
        max_block: usize,
    ) -> (Self, Arc<EffectParams>) {
        let params = Arc::new(EffectParams::new(effect.params()));
        let coef = |seconds: f32| 1.0 - (-1.0 / (seconds * sample_rate)).exp();
        let node = Self {
            seen: vec![f32::NAN; effect.params().len()].into_boxed_slice(),
            effect,
            params: params.clone(),
            dry: std::array::from_fn(|_| vec![0.0; max_block].into_boxed_slice()),
            mix: 1.0,
            dry_ms: 0.0,
            wet_ms: 0.0,
            average: coef(MATCH_WINDOW),
            gain: 1.0,
            glide: coef(0.05),
        };
        (node, params)
    }
}

impl AudioNode for EffectNode {
    /// Stereo: a mono input feeds both channels. Outputs past the second are zeroed.
    fn process(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        _midi_in: &[MidiEvent],
        _midi_out: &mut Vec<MidiEvent>,
        context: &ProcessContext,
    ) {
        let n = context.num_samples;
        let own = self.seen.len();
        for (i, seen) in self.seen.iter_mut().enumerate() {
            let v = self.params.get(i).unwrap_or(*seen);
            if v != *seen {
                self.effect.set_param(i, v);
                *seen = v;
            }
        }
        let common = |i: usize| self.params.get(own + i).unwrap_or(COMMON[i].default);
        let mix = common(0) / 100.0;
        let matching = common(1) >= 0.5;
        let delta = common(2) >= 0.5;

        let (stereo, rest) = outputs.split_at_mut(outputs.len().min(2));
        rest.iter_mut().for_each(|o| o.fill(0.0));
        let [left, right] = stereo else {
            stereo.iter_mut().for_each(|o| o.fill(0.0));
            return;
        };
        for (ch, (out, dry)) in [&mut **left, &mut **right]
            .into_iter()
            .zip(&mut self.dry)
            .enumerate()
        {
            match inputs.get(ch).or(inputs.first()) {
                Some(input) => {
                    out.copy_from_slice(input);
                    dry[..n].copy_from_slice(input);
                }
                None => {
                    out.fill(0.0);
                    dry[..n].fill(0.0);
                }
            }
        }
        self.effect.process(left, right);

        let start = self.mix;
        let [dry_l, dry_r] = &self.dry;
        for i in 0..n {
            let (wl, wr) = (left[i], right[i]);
            let (dl, dr) = (dry_l[i], dry_r[i]);
            self.dry_ms += (dl * dl + dr * dr - self.dry_ms) * self.average;
            self.wet_ms += (wl * wl + wr * wr - self.wet_ms) * self.average;
            let target = if !matching {
                1.0
            } else if self.dry_ms > MATCH_FLOOR && self.wet_ms > MATCH_FLOOR {
                (self.dry_ms / self.wet_ms)
                    .sqrt()
                    .clamp(1.0 / MATCH_LIMIT, MATCH_LIMIT)
            } else {
                self.gain
            };
            self.gain += (target - self.gain) * self.glide;
            // Mix ramps across the block so a turned knob never clicks.
            let m = start + (mix - start) * (i + 1) as f32 / n as f32;
            let keep = if delta { 0.0 } else { 1.0 };
            left[i] = keep * dl + m * (wl * self.gain - dl);
            right[i] = keep * dr + m * (wr * self.gain - dr);
        }
        self.mix = mix;
    }

    fn reset(&mut self) {
        self.effect.reset();
        self.dry_ms = 0.0;
        self.wet_ms = 0.0;
        self.gain = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daw_dsp::effects::effect;
    use daw_dsp::gain_to_db;

    const SR: f32 = 48_000.0;

    fn ctx(n: usize) -> ProcessContext {
        ProcessContext {
            sample_rate: SR,
            num_samples: n,
            playhead: 0,
            playing: true,
        }
    }

    /// Runs `seconds` of a 220 Hz sine (amplitude 0.5) and returns the RMS of the
    /// last block's left channel and of the input over it.
    fn run(node: &mut EffectNode, seconds: f32) -> (f32, f32) {
        let block = 256;
        let mut phase = 0usize;
        let (mut out_rms, mut in_rms) = (0.0, 0.0);
        for _ in 0..(seconds * SR) as usize / block {
            let input: Vec<f32> = (phase..phase + block)
                .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 220.0 * i as f32 / SR).sin())
                .collect();
            phase += block;
            let (mut l, mut r) = (vec![9.0; block], vec![9.0; block]);
            node.process(
                &[&input],
                &mut [&mut l, &mut r],
                &[],
                &mut Vec::new(),
                &ctx(block),
            );
            let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
            assert_eq!(l, r, "mono input feeds both channels");
            (out_rms, in_rms) = (rms(&l), rms(&input));
        }
        (out_rms, in_rms)
    }

    fn saturator() -> (EffectNode, Arc<EffectParams>) {
        let fx = effect("canary.saturator", SR).unwrap();
        EffectNode::new(fx, SR, 256)
    }

    #[test]
    fn level_match_holds_the_dry_loudness() {
        let (mut node, params) = saturator();
        params.set(0, 30.0); // heavy drive: far louder than dry
        let (out, input) = run(&mut node, 4.0);
        let diff = gain_to_db(out) - gain_to_db(input);
        assert!(diff.abs() < 0.5, "matched within 0.5 dB, off by {diff}");

        let match_id = params
            .specs()
            .iter()
            .position(|p| p.name == "MATCH")
            .unwrap();
        params.set(match_id, 0.0);
        let (out, input) = run(&mut node, 1.0);
        assert!(
            gain_to_db(out) - gain_to_db(input) > 3.0,
            "unmatched is louder"
        );
    }

    #[test]
    fn dry_mix_passes_input_and_delta_of_dry_is_silent() {
        let (mut node, params) = saturator();
        let id = |name| params.specs().iter().position(|p| p.name == name).unwrap();
        params.set(id("MIX"), 0.0);
        let (out, input) = run(&mut node, 0.5);
        assert!((out - input).abs() < 1e-6, "0% wet is the dry signal");
        params.set(id("DELTA"), 1.0);
        let (out, _) = run(&mut node, 0.5);
        assert!(out < 1e-6, "delta of a dry mix is silence, got {out}");
        params.set(id("MIX"), 100.0);
        let (out, _) = run(&mut node, 0.5);
        assert!(out > 0.01, "delta of a wet mix is what the effect changed");
    }

    #[test]
    fn params_clamp_and_reject_bad_values() {
        let (_, params) = saturator();
        assert!(params.set(0, 1e9));
        assert_eq!(params.get(0), Some(params.specs()[0].max));
        assert!(!params.set(0, f32::NAN));
        assert!(!params.set(99, 1.0));
        assert_eq!(params.get(99), None);
    }

    #[test]
    fn extra_outputs_are_zeroed_and_one_output_is_silent() {
        let (mut node, _) = saturator();
        let input = [0.5; 4];
        let (mut a, mut b, mut c) = ([9.0; 4], [9.0; 4], [9.0; 4]);
        node.process(
            &[&input, &input],
            &mut [&mut a, &mut b, &mut c],
            &[],
            &mut Vec::new(),
            &ctx(4),
        );
        assert_eq!(c, [0.0; 4]);
        node.process(&[&input], &mut [&mut a], &[], &mut Vec::new(), &ctx(4));
        assert_eq!(a, [0.0; 4]);
    }
}
