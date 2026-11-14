//! Real-time pitch: a shifter (two crossfaded read heads sliding through a
//! delay line) and pitch correction (YIN pitch detection snapping the voice to
//! a key and scale through the same shifter). Time-domain and cheap; best on
//! single voices. The heads center on a fixed delay (half the longest grain),
//! reported as latency so PDC lines the shifted signal up.

use crate::effects::{Effect, ParamSpec};

/// Two read heads, half a grain apart, sliding through a delay line at the
/// speed that changes pitch by `ratio`; each fades in and out with a triangle
/// so the jump back is never heard.
#[derive(Default)]
struct Shifter {
    line: Box<[f32]>,
    write: usize,
    /// Where the first head is in its grain, 0..1.
    phase: f32,
    grain: f32,
    /// The heads' average delay, whatever the grain: the latency.
    center: f32,
    /// How far the output has crossfaded to one plain read at `center`, used
    /// while there is no shift so two heads never comb.
    rest: f32,
}

/// Samples the crossfade to and from the plain read takes (about 20 ms).
const REST_FADE: f32 = 1024.0;

/// The shortest delay a head reads at, so it never reads ahead of the writer.
const MIN_DELAY: f32 = 4.0;

impl Shifter {
    fn new(max_grain: usize) -> Self {
        Self {
            line: vec![0.0; max_grain + 16].into_boxed_slice(),
            grain: max_grain as f32,
            center: MIN_DELAY + (max_grain / 2) as f32,
            ..Self::default()
        }
    }

    fn set_grain(&mut self, samples: f32) {
        self.grain = samples.clamp(64.0, (self.line.len() - 16) as f32);
    }

    fn read(&self, delay: f32) -> f32 {
        let n = self.line.len();
        let back = self.write as f32 + n as f32 - delay;
        let (i, frac) = (back as usize % n, back.fract());
        self.line[i] * (1.0 - frac) + self.line[(i + 1) % n] * frac
    }

    fn tick(&mut self, x: f32, ratio: f32) -> f32 {
        let n = self.line.len();
        self.line[self.write] = x;
        let still = (1.0 - ratio).abs() < 1e-4;
        if still {
            self.rest = (self.rest + 1.0 / REST_FADE).min(1.0);
            if self.rest == 1.0 {
                // Head one at full gain on the center: the plain read, so
                // shifting again later starts without a jump.
                self.phase = 0.5;
            }
        } else {
            self.rest = (self.rest - 1.0 / REST_FADE).max(0.0);
            // Raising pitch shortens the delay each sample; lowering lengthens it.
            self.phase = (self.phase + (1.0 - ratio) / self.grain).rem_euclid(1.0);
        }
        let other = (self.phase + 0.5) % 1.0;
        let tri = |p: f32| 1.0 - (2.0 * p - 1.0).abs();
        let delay = |p: f32| self.center + (p - 0.5) * self.grain;
        let heads =
            self.read(delay(self.phase)) * tri(self.phase) + self.read(delay(other)) * tri(other);
        let y = if self.rest > 0.0 {
            self.rest * self.read(self.center) + (1.0 - self.rest) * heads
        } else {
            heads
        };
        self.write = (self.write + 1) % n;
        y
    }

    fn clear(&mut self) {
        self.line.fill(0.0);
    }
}

const SHIFT_PARAMS: [ParamSpec; 3] = [
    ParamSpec {
        name: "SEMI",
        min: -24.0,
        max: 24.0,
        default: 0.0,
        log: false,
    },
    ParamSpec {
        name: "CENTS",
        min: -100.0,
        max: 100.0,
        default: 0.0,
        log: false,
    },
    ParamSpec {
        name: "GRAIN MS",
        min: 10.0,
        max: 100.0,
        default: 40.0,
        log: false,
    },
];

/// Pitch shifter: SEMI and CENTS up or down, GRAIN sets the crossfade length
/// (short for drums, long for pads).
#[derive(Default)]
pub struct PitchShift {
    sr: f32,
    ratio: f32,
    sides: [Shifter; 2],
    values: [f32; 3],
}

impl PitchShift {
    fn update(&mut self) {
        let [semi, cents, grain] = self.values;
        self.ratio = 2f32.powf((semi + cents / 100.0) / 12.0);
        for s in &mut self.sides {
            s.set_grain(grain / 1000.0 * self.sr);
        }
    }
}

impl Effect for PitchShift {
    fn params(&self) -> &'static [ParamSpec] {
        &SHIFT_PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        if let Some(v) = self.values.get_mut(index) {
            *v = value;
            self.update();
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
        let max = (0.1 * sample_rate) as usize;
        self.sides = [Shifter::new(max), Shifter::new(max)];
        self.update();
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.sides[0].line.is_empty() {
            return;
        }
        for (side, buf) in [left, right].into_iter().enumerate() {
            for s in buf.iter_mut() {
                *s = self.sides[side].tick(*s, self.ratio);
            }
        }
    }
    fn reset(&mut self) {
        self.sides.iter_mut().for_each(Shifter::clear);
    }
    fn latency(&self) -> u32 {
        self.sides[0].center as u32
    }
}

// ---------------------------------------------------------------- Detection

/// Samples YIN looks at (after halving the rate) and the longest period it
/// looks for: 60 Hz at 48 kHz.
const YIN_WINDOW: usize = 512;
const YIN_MAX_LAG: usize = 400;
/// Below this aperiodicity a frame counts as voiced.
const YIN_THRESHOLD: f32 = 0.15;
/// Input samples between detections.
const DETECT_EVERY: usize = 512;

/// YIN (de Cheveigné and Kawahara) over a ring of the input at half rate.
#[derive(Default)]
struct Detector {
    ring: Box<[f32]>,
    pos: usize,
    half: f32,
    odd: bool,
    since: usize,
    frame: Box<[f32]>,
    diff: Box<[f32]>,
}

impl Detector {
    fn new() -> Self {
        let len = YIN_WINDOW + YIN_MAX_LAG;
        Self {
            ring: vec![0.0; len].into_boxed_slice(),
            frame: vec![0.0; len].into_boxed_slice(),
            diff: vec![0.0; YIN_MAX_LAG].into_boxed_slice(),
            ..Self::default()
        }
    }

    /// Takes one input sample; every `DETECT_EVERY` returns Some(period in
    /// input samples) when voiced, Some(None) when not.
    fn push(&mut self, x: f32) -> Option<Option<f32>> {
        // Average pairs: half rate, and a gentle low-pass against aliasing.
        if self.odd {
            self.ring[self.pos] = 0.5 * (self.half + x);
            self.pos = (self.pos + 1) % self.ring.len();
        } else {
            self.half = x;
        }
        self.odd = !self.odd;
        self.since += 1;
        if self.since < DETECT_EVERY {
            return None;
        }
        self.since = 0;
        Some(self.detect().map(|lag| lag * 2.0))
    }

    fn detect(&mut self) -> Option<f32> {
        let n = self.ring.len();
        for (i, f) in self.frame.iter_mut().enumerate() {
            *f = self.ring[(self.pos + i) % n];
        }
        let energy: f32 = self.frame.iter().map(|s| s * s).sum();
        if energy < 1e-6 * n as f32 {
            return None;
        }
        // Difference function, then its cumulative mean normalized form.
        let mut running = 0.0;
        self.diff[0] = 1.0;
        for lag in 1..YIN_MAX_LAG {
            let d: f32 = (0..YIN_WINDOW)
                .map(|i| self.frame[i] - self.frame[i + lag])
                .map(|d| d * d)
                .sum();
            running += d;
            self.diff[lag] = if running > 0.0 {
                d * lag as f32 / running
            } else {
                1.0
            };
        }
        // The first dip under the threshold, followed to its bottom.
        let mut lag = (2..YIN_MAX_LAG).find(|&l| self.diff[l] < YIN_THRESHOLD)?;
        while lag + 1 < YIN_MAX_LAG && self.diff[lag + 1] < self.diff[lag] {
            lag += 1;
        }
        // Parabolic interpolation around the minimum.
        let (a, b) = (self.diff[lag - 1], self.diff[lag]);
        let c = self.diff.get(lag + 1).copied().unwrap_or(b);
        let den = a - 2.0 * b + c;
        let shift = if den.abs() > 1e-9 {
            0.5 * (a - c) / den
        } else {
            0.0
        };
        Some(lag as f32 + shift.clamp(-0.5, 0.5))
    }
}

// ---------------------------------------------------------------- Correction

/// Scales by the semitones they keep, from the key's root.
const SCALES: [&[u8]; 3] = [
    &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
    &[0, 2, 4, 5, 7, 9, 11],
    &[0, 2, 3, 5, 7, 8, 10],
];

const TUNE_PARAMS: [ParamSpec; 4] = [
    ParamSpec {
        name: "KEY",
        min: 0.0,
        max: 11.0,
        default: 0.0,
        log: false,
    },
    ParamSpec {
        name: "SCALE",
        min: 0.0,
        max: 2.0,
        default: 0.0,
        log: false,
    },
    ParamSpec {
        name: "SPEED MS",
        min: 0.0,
        max: 400.0,
        default: 50.0,
        log: false,
    },
    ParamSpec {
        name: "AMOUNT",
        min: 0.0,
        max: 100.0,
        default: 100.0,
        log: false,
    },
];

/// The nearest note of the scale to `note` (MIDI, fractional).
pub fn snap(note: f32, key: u8, scale: &[u8]) -> f32 {
    let base = (note - key as f32).floor();
    let octave = (base / 12.0).floor();
    // Candidates in this octave and the ones either side.
    (-1..=1)
        .flat_map(|o| {
            scale
                .iter()
                .map(move |&s| (octave + o as f32) * 12.0 + s as f32 + key as f32)
        })
        .min_by(|a, b| (a - note).abs().total_cmp(&(b - note).abs()))
        .unwrap_or(note)
}

/// Pitch correction: KEY (0 = C) and SCALE (chromatic, major, minor), SPEED
/// (0 is the hard, robotic snap), AMOUNT of the correction applied. Detects on
/// the mid signal, shifts both sides alike.
#[derive(Default)]
pub struct PitchCorrect {
    sr: f32,
    values: [f32; 4],
    detector: Detector,
    sides: [Shifter; 2],
    /// Semitones to shift now, and where it glides to.
    shift: f32,
    target: f32,
    glide: f32,
}

impl Effect for PitchCorrect {
    fn params(&self) -> &'static [ParamSpec] {
        &TUNE_PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        if let Some(v) = self.values.get_mut(index) {
            *v = value;
        }
        if index == 2 {
            self.glide = if value <= 0.0 {
                1.0
            } else {
                1.0 - (-1.0 / (value / 1000.0 * self.sr.max(1.0))).exp()
            };
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
        self.detector = Detector::new();
        // Grains of about two periods of a low voice.
        let grain = (0.03 * sample_rate) as usize;
        self.sides = [Shifter::new(grain), Shifter::new(grain)];
        let speed = self.values[2];
        self.set_param(2, speed);
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        if self.sides[0].line.is_empty() {
            return;
        }
        let key = self.values[0].round().clamp(0.0, 11.0) as u8;
        let scale = SCALES[(self.values[1].round() as usize).min(SCALES.len() - 1)];
        let amount = self.values[3] / 100.0;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            match self.detector.push(0.5 * (*l + *r)) {
                Some(Some(period)) => {
                    let note = 69.0 + 12.0 * (self.sr / period / 440.0).log2();
                    self.target = (snap(note, key, scale) - note) * amount;
                }
                // Unvoiced: let go of the correction.
                Some(None) => self.target = 0.0,
                None => {}
            }
            self.shift += (self.target - self.shift) * self.glide;
            let ratio = 2f32.powf(self.shift / 12.0);
            *l = self.sides[0].tick(*l, ratio);
            *r = self.sides[1].tick(*r, ratio);
        }
    }
    fn reset(&mut self) {
        self.sides.iter_mut().for_each(Shifter::clear);
        self.shift = 0.0;
        self.target = 0.0;
    }
    fn latency(&self) -> u32 {
        self.sides[0].center as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::effect;
    use std::f32::consts::PI;

    const SR: f32 = 48_000.0;

    fn sine(f: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| 0.5 * (2.0 * PI * f * i as f32 / SR).sin())
            .collect()
    }

    /// The strongest frequency in `x` within 15% of `near`, to 1 Hz (a DFT scan).
    fn frequency(x: &[f32], near: f32) -> f32 {
        let power = |f: f32| {
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for (i, s) in x.iter().enumerate() {
                let w = 2.0 * PI * f * i as f32 / SR;
                re += s * w.cos();
                im += s * w.sin();
            }
            re * re + im * im
        };
        let (lo, hi) = ((near * 0.85) as i32, (near * 1.15) as i32);
        (lo..=hi)
            .map(|f| f as f32)
            .max_by(|a, b| power(*a).total_cmp(&power(*b)))
            .unwrap_or(0.0)
    }

    fn run(id: &str, params: &[(usize, f32)], input: &[f32]) -> Vec<f32> {
        let mut fx = effect(id, SR).unwrap();
        for &(i, v) in params {
            fx.set_param(i, v);
        }
        fx.reset();
        let (mut l, mut r) = (input.to_vec(), input.to_vec());
        for (a, b) in l.chunks_mut(256).zip(r.chunks_mut(256)) {
            fx.process(a, b);
        }
        l
    }

    #[test]
    fn shifts_by_the_interval_set() {
        for (semi, want) in [(12.0, 880.0), (-12.0, 220.0), (7.0, 440.0 * 1.4983)] {
            // Long grains: the crossfade's sidebands sit close to the tone.
            let out = run(
                "canary.pitch",
                &[(0, semi), (2, 100.0)],
                &sine(440.0, 144_000),
            );
            let got = frequency(&out[48_000..144_000], want);
            assert!(
                // The grain crossfade spreads a tone a few hertz either way.
                (got / want - 1.0).abs() < 0.015,
                "{semi}: {got} Hz, want {want}"
            );
        }
    }

    #[test]
    fn back_at_zero_it_is_a_clean_delay_of_its_latency() {
        let mut fx = effect("canary.pitch", SR).unwrap();
        let latency = fx.latency() as usize;
        assert!(latency > 0);
        // Shifted for a while, then back to 0: the heads must settle on one
        // delay, not stay split (a comb) wherever they stopped.
        let noise: Vec<f32> = (0..240_000u32)
            .map(|i| ((i.wrapping_mul(2_654_435_761) >> 8) as f32 / 8_388_608.0) - 1.0)
            .collect();
        let (mut l, mut r) = (noise.clone(), noise.clone());
        fx.set_param(0, 3.0);
        fx.process(&mut l[..20_000], &mut r[..20_000]);
        fx.set_param(0, 0.0);
        for (a, b) in l[20_000..].chunks_mut(256).zip(r[20_000..].chunks_mut(256)) {
            fx.process(a, b);
        }
        let err = (200_000..240_000)
            .map(|i| (l[i] - noise[i - latency]).abs())
            .fold(0.0f32, f32::max);
        assert!(err < 1e-3, "{err}");
    }

    #[test]
    fn returning_to_zero_does_not_bend_the_pitch() {
        let mut fx = effect("canary.pitch", SR).unwrap();
        let input = sine(1_000.0, 48_000);
        let (mut l, mut r) = (input.clone(), input.clone());
        fx.set_param(0, 3.0);
        fx.process(&mut l[..20_000], &mut r[..20_000]);
        fx.set_param(0, 0.0);
        for (a, b) in l[20_000..].chunks_mut(256).zip(r[20_000..].chunks_mut(256)) {
            fx.process(a, b);
        }
        // Straight after the switch (past the latency and the 20 ms fade).
        let got = frequency(&l[24_000..36_000], 1_000.0);
        assert!((got - 1_000.0).abs() < 2.0, "{got} Hz");
    }

    #[test]
    fn snaps_to_the_scale() {
        assert_eq!(snap(61.3, 0, SCALES[1]), 62.0, "C# in C major goes to D");
        assert_eq!(snap(60.4, 0, SCALES[0]), 60.0);
        assert_eq!(snap(63.2, 0, SCALES[2]), 63.0, "Eb is in C minor");
    }

    #[test]
    fn corrects_a_flat_note_to_pitch() {
        // 430 Hz is 40 cents flat of A: corrected, it reads 440.
        let out = run("canary.tune", &[(2, 0.0)], &sine(430.0, 96_000));
        let got = frequency(&out[48_000..72_000], 435.0);
        assert!((got - 440.0).abs() < 3.0, "{got} Hz");
    }
}
