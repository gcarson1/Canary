//! The master node: meters the mix (loudness, true peak, correlation) and sets
//! what you hear: the mix or a level-matched reference, in stereo, mid or side.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use daw_core::{MidiEvent, ProcessContext};
use daw_dsp::{Loudness, TruePeak};

use crate::AudioNode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorMode {
    #[default]
    Stereo,
    /// (L + R) / 2 on both sides: mono compatibility.
    Mid,
    /// (L - R) / 2 on both sides: what only the width carries.
    Side,
}

/// What the meters read, as of the last block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Readings {
    /// LUFS: 400 ms, 3 s, and gated since the last reset. -inf when silent.
    pub momentary: f32,
    pub short_term: f32,
    pub integrated: f32,
    /// Highest true peak since the last reset, dBTP.
    pub true_peak: f32,
    /// -1 (out of phase) to +1 (mono); 0 for silence.
    pub correlation: f32,
}

/// Shared between the master node and the main thread: controls in, readings out.
#[derive(Default)]
pub struct Monitor {
    mode: AtomicU32,
    reference: AtomicBool,
    /// Linear gain on the reference, so it plays as loud as the mix.
    reference_gain: AtomicU32,
    reset: AtomicBool,
    readings: [AtomicU32; 5],
}

fn load(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

fn store(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Ordering::Relaxed);
}

impl Monitor {
    pub fn mode(&self) -> MonitorMode {
        match self.mode.load(Ordering::Relaxed) {
            1 => MonitorMode::Mid,
            2 => MonitorMode::Side,
            _ => MonitorMode::Stereo,
        }
    }

    pub fn set_mode(&self, mode: MonitorMode) {
        self.mode.store(mode as u32, Ordering::Relaxed);
    }

    /// True while the reference plays instead of the mix.
    pub fn reference(&self) -> bool {
        self.reference.load(Ordering::Relaxed)
    }

    pub fn set_reference(&self, on: bool) {
        self.reference.store(on, Ordering::Relaxed);
    }

    /// Ignored unless finite and positive.
    pub fn set_reference_gain(&self, gain: f32) {
        if gain.is_finite() && gain > 0.0 {
            store(&self.reference_gain, gain);
        }
    }

    /// Clears integrated loudness and the true-peak hold at the next block.
    pub fn reset_meters(&self) {
        self.reset.store(true, Ordering::Relaxed);
    }

    pub fn readings(&self) -> Readings {
        let r = &self.readings;
        Readings {
            momentary: load(&r[0]),
            short_term: load(&r[1]),
            integrated: load(&r[2]),
            true_peak: load(&r[3]),
            correlation: load(&r[4]),
        }
    }
}

pub struct MasterNode {
    monitor: Arc<Monitor>,
    loudness: Loudness,
    peaks: [TruePeak; 2],
    peak: f32,
    /// Running averages of L*L, R*R and L*R for correlation (300 ms).
    sums: [f32; 3],
    average: f32,
    /// 0 = mix, 1 = reference; glides so A/B never clicks.
    fade: f32,
    fade_step: f32,
}

impl MasterNode {
    pub fn new(sample_rate: f32) -> (Self, Arc<Monitor>) {
        let monitor = Arc::new(Monitor::default());
        monitor.set_reference_gain(1.0);
        let node = Self {
            monitor: monitor.clone(),
            loudness: Loudness::new(sample_rate),
            peaks: Default::default(),
            peak: 0.0,
            sums: [0.0; 3],
            average: 1.0 - (-1.0 / (0.3 * sample_rate)).exp(),
            fade: 0.0,
            fade_step: 1.0 / (0.01 * sample_rate),
        };
        (node, monitor)
    }
}

impl AudioNode for MasterNode {
    /// Inputs: mix left, right, then optionally reference left, right. Outputs:
    /// left, right; more are zeroed.
    fn process(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        _midi_in: &[MidiEvent],
        _midi_out: &mut Vec<MidiEvent>,
        context: &ProcessContext,
    ) {
        let n = context.num_samples;
        let silence = &[][..];
        let input = |i: usize| inputs.get(i).copied().unwrap_or(silence);
        let (mix_l, mix_r) = (input(0), inputs.get(1).copied().unwrap_or(input(0)));
        let (ref_l, ref_r) = (input(2), inputs.get(3).copied().unwrap_or(input(2)));
        let m = &*self.monitor;

        if m.reset.swap(false, Ordering::Relaxed) {
            self.loudness.reset();
            self.peaks.iter_mut().for_each(TruePeak::reset);
            self.peak = 0.0;
        }
        if mix_l.len() == n && mix_r.len() == n {
            self.loudness.process(mix_l, mix_r);
            for (tp, x) in self.peaks.iter_mut().zip([mix_l, mix_r]) {
                self.peak = self.peak.max(tp.process(x));
            }
            for (&l, &r) in mix_l.iter().zip(mix_r) {
                for (sum, v) in self.sums.iter_mut().zip([l * l, r * r, l * r]) {
                    *sum += (v - *sum) * self.average;
                }
            }
        }
        let [ll, rr, lr] = self.sums;
        let power = (ll * rr).sqrt();
        let correlation = if power > 1e-9 {
            (lr / power).clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let r = &m.readings;
        store(&r[0], self.loudness.momentary());
        store(&r[1], self.loudness.short_term());
        store(&r[2], self.loudness.integrated());
        store(&r[3], 20.0 * self.peak.max(1e-6).log10());
        store(&r[4], correlation);

        outputs.iter_mut().skip(2).for_each(|o| o.fill(0.0));
        let (left, right) = match outputs {
            [l, r, ..] => (l, r),
            [l] => {
                l.fill(0.0);
                return;
            }
            [] => return,
        };
        let has_reference = ref_l.len() == n && ref_r.len() == n;
        let target = if has_reference && m.reference() {
            1.0
        } else {
            0.0
        };
        let gain = load(&m.reference_gain);
        let mode = m.mode();
        let at = |x: &[f32], i: usize| x.get(i).copied().unwrap_or(0.0);
        for i in 0..n {
            self.fade = if self.fade < target {
                (self.fade + self.fade_step).min(target)
            } else {
                (self.fade - self.fade_step).max(target)
            };
            let mix = |a: f32, b: f32| a * (1.0 - self.fade) + b * gain * self.fade;
            let l = mix(at(mix_l, i), at(ref_l, i));
            let r = mix(at(mix_r, i), at(ref_r, i));
            (left[i], right[i]) = match mode {
                MonitorMode::Stereo => (l, r),
                MonitorMode::Mid => ((l + r) / 2.0, (l + r) / 2.0),
                MonitorMode::Side => ((l - r) / 2.0, (l - r) / 2.0),
            };
        }
    }

    fn reset(&mut self) {
        self.monitor.reset_meters();
        self.sums = [0.0; 3];
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
            playing: true,
        }
    }

    fn run(node: &mut MasterNode, inputs: &[&[f32]], n: usize) -> (Vec<f32>, Vec<f32>) {
        let (mut l, mut r) = (vec![9.0; n], vec![9.0; n]);
        node.process(inputs, &mut [&mut l, &mut r], &[], &mut Vec::new(), &ctx(n));
        (l, r)
    }

    #[test]
    fn modes_route_mid_and_side() {
        let (mut node, monitor) = MasterNode::new(48_000.0);
        let (a, b) = ([1.0f32; 8], [0.5f32; 8]);
        assert_eq!(run(&mut node, &[&a, &b], 8), (a.to_vec(), b.to_vec()));
        monitor.set_mode(MonitorMode::Mid);
        assert_eq!(run(&mut node, &[&a, &b], 8).0, vec![0.75; 8]);
        monitor.set_mode(MonitorMode::Side);
        let (l, r) = run(&mut node, &[&a, &b], 8);
        assert_eq!((l[0], r[0]), (0.25, 0.25));
        assert!(
            (monitor.readings().correlation - 1.0).abs() < 1e-3,
            "in phase"
        );
    }

    #[test]
    fn reference_fades_in_at_its_gain_and_only_when_present() {
        let (mut node, monitor) = MasterNode::new(48_000.0);
        let (mix, reference) = ([0.2f32; 960], [0.8f32; 960]);
        monitor.set_reference(true);
        monitor.set_reference_gain(0.25);
        // No reference inputs: the mix plays.
        assert_eq!(run(&mut node, &[&mix, &mix], 960).0[959], 0.2);
        let (l, _) = run(&mut node, &[&mix, &mix, &reference, &reference], 960);
        assert!(l[0] > 0.19 && l[0] < 0.21, "fades, no jump: {}", l[0]);
        assert!((l[959] - 0.2).abs() < 1e-6, "0.8 x 0.25 matches the mix");
        monitor.set_reference_gain(f32::NAN);
        monitor.set_reference(false);
        let (l, _) = run(&mut node, &[&mix, &mix, &reference, &reference], 960);
        assert!((l[959] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn meters_read_the_mix_and_reset_on_request() {
        let (mut node, monitor) = MasterNode::new(48_000.0);
        let x: Vec<f32> = (0..4_800).map(|i| 0.5 * (i as f32 * 0.2).sin()).collect();
        let (silent, loud) = (vec![0.0; 4_800], vec![0.9; 4_800]);
        for _ in 0..10 {
            // The reference is louder but not playing: meters read only the mix.
            run(&mut node, &[&x, &x, &loud, &loud], 4_800);
        }
        let r = monitor.readings();
        assert!(r.integrated.is_finite() && r.short_term.is_finite());
        assert!(r.true_peak > -7.0 && r.true_peak < -5.0, "{}", r.true_peak);
        monitor.reset_meters();
        run(&mut node, &[&silent, &silent], 4_800);
        let r = monitor.readings();
        assert_eq!(r.integrated, f32::NEG_INFINITY);
        assert!(r.true_peak < -100.0);
    }
}
