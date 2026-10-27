//! Loudness (ITU-R BS.1770-4 / EBU R128: momentary, short-term, integrated) and
//! 4x oversampled true peak. Allocation-free after construction.

use std::f32::consts::PI;

use crate::filter::Biquad;

/// Integrated loudness keeps a histogram of 400 ms block loudness, 0.1 LU per bin
/// from -70 to +10 LUFS, so it never grows.
const BIN_LU: f32 = 0.1;
const BIN_FLOOR: f32 = -70.0;
const BINS: usize = 800;
/// 100 ms sub-blocks: momentary is the last 4, short-term the last 30.
const SUBS: usize = 30;

/// The two-stage K-weighting filter at `sample_rate` (BS.1770-4 Annex 1, with the
/// bilinear prewarp so it holds at any rate).
fn k_weighting(sample_rate: f32) -> (Biquad, Biquad) {
    let fs = f64::from(sample_rate);
    let (f0, gain_db, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (std::f64::consts::PI * f0 / fs).tan();
    let vh = 10f64.powf(gain_db / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad::from_coefficients(
        ((vh + vb * k / q + k * k) / a0) as f32,
        (2.0 * (k * k - vh) / a0) as f32,
        ((vh - vb * k / q + k * k) / a0) as f32,
        (2.0 * (k * k - 1.0) / a0) as f32,
        ((1.0 - k / q + k * k) / a0) as f32,
    );
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (std::f64::consts::PI * f0 / fs).tan();
    let a0 = 1.0 + k / q + k * k;
    let high_pass = Biquad::from_coefficients(
        1.0,
        -2.0,
        1.0,
        (2.0 * (k * k - 1.0) / a0) as f32,
        ((1.0 - k / q + k * k) / a0) as f32,
    );
    (shelf, high_pass)
}

fn lufs(mean_square: f64) -> f32 {
    (-0.691 + 10.0 * mean_square.max(1e-20).log10()) as f32
}

fn energy(lufs: f32) -> f64 {
    10f64.powf((f64::from(lufs) + 0.691) / 10.0)
}

/// Stereo loudness meter. Silence reads `f32::NEG_INFINITY`.
pub struct Loudness {
    sample_rate: f32,
    k: [(Biquad, Biquad); 2],
    sub_frames: usize,
    count: usize,
    sum: f64,
    /// Channel-summed mean square of the last `SUBS` sub-blocks, a ring.
    subs: [f64; SUBS],
    head: usize,
    filled: usize,
    hist: Box<[u32; BINS]>,
    /// Recomputed when a block lands in `hist`, not on every read.
    integrated: f32,
}

impl Loudness {
    pub fn new(sample_rate: f32) -> Self {
        let k = k_weighting(sample_rate);
        Self {
            sample_rate,
            k: [k, k],
            sub_frames: ((sample_rate / 10.0) as usize).max(1),
            count: 0,
            sum: 0.0,
            subs: [0.0; SUBS],
            head: 0,
            filled: 0,
            hist: Box::new([0; BINS]),
            integrated: f32::NEG_INFINITY,
        }
    }

    pub fn reset(&mut self) {
        let k = k_weighting(self.sample_rate);
        self.k = [k, k];
        (self.count, self.sum) = (0, 0.0);
        self.subs = [0.0; SUBS];
        (self.head, self.filled) = (0, 0);
        self.hist.fill(0);
        self.integrated = f32::NEG_INFINITY;
    }

    pub fn process(&mut self, left: &[f32], right: &[f32]) {
        for (&l, &r) in left.iter().zip(right) {
            let [(s0, h0), (s1, h1)] = &mut self.k;
            let (l, r) = (h0.process(s0.process(l)), h1.process(s1.process(r)));
            self.sum += f64::from(l * l + r * r);
            self.count += 1;
            if self.count == self.sub_frames {
                self.subs[self.head] = self.sum / self.sub_frames as f64;
                self.head = (self.head + 1) % SUBS;
                self.filled = (self.filled + 1).min(SUBS);
                (self.count, self.sum) = (0, 0.0);
                // Each new sub-block completes a 400 ms block (75% overlap).
                if self.filled >= 4 {
                    let block = self.window(4);
                    if block > BIN_FLOOR {
                        let bin = ((block - BIN_FLOOR) / BIN_LU) as usize;
                        self.hist[bin.min(BINS - 1)] += 1;
                        self.integrated = self.gated();
                    }
                }
            }
        }
    }

    /// Loudness over the last `subs` sub-blocks (or all there are).
    fn window(&self, subs: usize) -> f32 {
        let n = subs.min(self.filled);
        if n == 0 {
            return f32::NEG_INFINITY;
        }
        let sum: f64 = (1..=n)
            .map(|i| self.subs[(self.head + SUBS - i) % SUBS])
            .sum();
        lufs(sum / n as f64)
    }

    /// 400 ms window.
    pub fn momentary(&self) -> f32 {
        self.window(4)
    }

    /// 3 s window.
    pub fn short_term(&self) -> f32 {
        self.window(SUBS)
    }

    /// Gated loudness of everything since the last reset: blocks above -70 LUFS,
    /// then those within 10 LU of their mean.
    pub fn integrated(&self) -> f32 {
        self.integrated
    }

    fn gated(&self) -> f32 {
        let center = |bin: usize| BIN_FLOOR + (bin as f32 + 0.5) * BIN_LU;
        let mean = |from: usize| {
            let (mut sum, mut n) = (0.0, 0u64);
            for (bin, &count) in self.hist.iter().enumerate().skip(from) {
                sum += energy(center(bin)) * f64::from(count);
                n += u64::from(count);
            }
            (n > 0).then(|| lufs(sum / n as f64))
        };
        let Some(ungated) = mean(0) else {
            return f32::NEG_INFINITY;
        };
        let gate = (((ungated - 10.0 - BIN_FLOOR) / BIN_LU).max(0.0)) as usize;
        mean(gate).unwrap_or(f32::NEG_INFINITY)
    }
}

/// Taps per phase of the 4x interpolator.
const TAPS: usize = 12;

/// Peak of a channel oversampled 4x (a Hann-windowed sinc, 48 taps), catching
/// inter-sample peaks a sample-peak meter misses.
pub struct TruePeak {
    phases: [[f32; TAPS]; 4],
    history: [f32; TAPS],
    pos: usize,
}

impl Default for TruePeak {
    fn default() -> Self {
        let n = 4 * TAPS;
        let center = (n as f32 - 1.0) / 2.0;
        let mut phases = [[0.0; TAPS]; 4];
        for (p, phase) in phases.iter_mut().enumerate() {
            for (j, tap) in phase.iter_mut().enumerate() {
                let m = p + 4 * j;
                let t = (m as f32 - center) / 4.0;
                let sinc = if t == 0.0 {
                    1.0
                } else {
                    (PI * t).sin() / (PI * t)
                };
                let window = 0.5 - 0.5 * (2.0 * PI * (m as f32 + 0.5) / n as f32).cos();
                *tap = sinc * window;
            }
            // Unity gain at DC for every phase.
            let sum: f32 = phase.iter().sum();
            phase.iter_mut().for_each(|t| *t /= sum);
        }
        Self {
            phases,
            history: [0.0; TAPS],
            pos: 0,
        }
    }
}

impl TruePeak {
    /// The largest absolute value of `x` and its interpolated points.
    pub fn process(&mut self, x: &[f32]) -> f32 {
        let mut peak = 0.0f32;
        for &s in x {
            self.history[self.pos] = s;
            peak = peak.max(s.abs());
            for phase in &self.phases {
                // Newest sample first: history runs backwards from `pos`.
                let mut y = 0.0;
                for (j, tap) in phase.iter().enumerate() {
                    y += tap * self.history[(self.pos + TAPS - j) % TAPS];
                }
                peak = peak.max(y.abs());
            }
            self.pos = (self.pos + 1) % TAPS;
        }
        peak
    }

    pub fn reset(&mut self) {
        self.history = [0.0; TAPS];
    }
}

/// Loudness of a whole stereo (or mono) file, as a mix would be metered.
pub fn integrated_loudness(left: &[f32], right: &[f32], sample_rate: f32) -> f32 {
    let mut m = Loudness::new(sample_rate);
    m.process(left, right);
    m.integrated()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn sine(freq: f32, dbfs: f32, seconds: f32, phase: f32) -> Vec<f32> {
        let a = 10f32.powf(dbfs / 20.0);
        (0..(seconds * SR) as usize)
            .map(|i| a * (2.0 * PI * freq * i as f32 / SR + phase).sin())
            .collect()
    }

    #[test]
    fn ebu_1k_sine_at_minus_23_reads_minus_23_lufs() {
        // EBU Tech 3341 case 1: stereo 1 kHz at -23 dBFS reads -23.0 LUFS.
        let x = sine(1_000.0, -23.0, 20.0, 0.0);
        let mut m = Loudness::new(SR);
        m.process(&x, &x);
        for (name, v) in [
            ("momentary", m.momentary()),
            ("short-term", m.short_term()),
            ("integrated", m.integrated()),
        ] {
            assert!((v + 23.0).abs() < 0.1, "{name} {v}");
        }
    }

    #[test]
    fn integrated_gates_out_silence_and_quiet_passages() {
        // EBU Tech 3341 case 3: -36 dBFS 10 s, -23 dBFS 60 s, -36 dBFS 10 s reads
        // -23.0: the quiet ends fall under the relative gate.
        let x: Vec<f32> = [(-36.0, 10.0), (-23.0, 60.0), (-36.0, 10.0)]
            .iter()
            .flat_map(|&(db, seconds)| sine(1_000.0, db, seconds, 0.0))
            .chain(vec![0.0; 48_000 * 5])
            .collect();
        let i = integrated_loudness(&x, &x, SR);
        assert!((i + 23.0).abs() < 0.2, "integrated {i}");
        assert_eq!(
            integrated_loudness(&[0.0; 48_000], &[0.0; 48_000], SR),
            f32::NEG_INFINITY
        );
    }

    #[test]
    fn k_weighting_holds_at_44_1_khz() {
        let sr = 44_100.0;
        let x: Vec<f32> = (0..(10.0 * sr) as usize)
            .map(|i| 10f32.powf(-23.0 / 20.0) * (2.0 * PI * 1_000.0 * i as f32 / sr).sin())
            .collect();
        let i = integrated_loudness(&x, &x, sr);
        assert!((i + 23.0).abs() < 0.1, "integrated {i}");
    }

    #[test]
    fn true_peak_finds_the_peak_between_samples() {
        // fs/4 at 45 degrees: every sample is ±0.707 but the wave peaks at 1.0.
        let x = sine(12_000.0, 0.0, 0.1, PI / 4.0);
        let sample_peak = x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((sample_peak - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
        let mut tp = TruePeak::default();
        let peak = tp.process(&x);
        let db = 20.0 * peak.log10();
        assert!(db > -0.5 && db < 0.3, "true peak {db} dBTP");
        // A low tone's true peak is its sample peak.
        let mut tp = TruePeak::default();
        let low = tp.process(&sine(100.0, -6.0, 0.1, 0.0));
        assert!((20.0 * low.log10() + 6.0).abs() < 0.05);
    }
}
