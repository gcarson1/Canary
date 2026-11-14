//! A port of Cloud Seed's reverb core (Ghost Note Audio's CloudSeedCore, MIT;
//! see THIRD_PARTY_NOTICES.md), set up as its "Dark Plate" program with five
//! knobs: PREDELAY, SIZE, DECAY, MOD and TONE. Twelve modulated delay lines per
//! side, each with an all-pass diffuser, a high shelf and a low-pass in its
//! feedback loop. Changes from the original: it runs sample by sample (the
//! original's feedback waits a whole block, so its sound depended on the block
//! size), buffers are sized to what the knobs reach instead of 192 kHz worst
//! cases, and the seeded random tables are made once, not on every knob turn.

use std::f32::consts::TAU;

use crate::effects::{Effect, ParamSpec};
use crate::filter::Biquad;
use crate::fx::{eq_band, EqKind};

const LINES: usize = 12;
const STAGES: usize = 12;
/// Diffuser stages in each late line (the program's 0.488 of 1..8).
const LATE_STAGES: usize = 4;
/// Modulation is recomputed every this many samples, as in the original.
const MOD_EVERY: u32 = 8;

const PARAMS: [ParamSpec; 5] = [
    ParamSpec {
        name: "PREDELAY",
        min: 0.0,
        max: 500.0,
        default: 20.0,
        log: false,
    },
    ParamSpec {
        name: "SIZE",
        min: 20.0,
        max: 1_000.0,
        default: 96.0,
        log: true,
    },
    ParamSpec {
        name: "DECAY",
        min: 0.05,
        max: 60.0,
        default: 4.8,
        log: true,
    },
    ParamSpec {
        name: "MOD",
        min: 0.0,
        max: 100.0,
        default: 27.0,
        log: false,
    },
    ParamSpec {
        name: "TONE",
        min: 400.0,
        max: 20_000.0,
        default: 20_000.0,
        log: true,
    },
];

/// Cloud Seed's seeded random series: an LCG, values in 0..=1, blended with the
/// series of the inverted seed by `cross`.
fn random_series<const N: usize>(seed: u64, cross: f32) -> [f32; N] {
    let series = |mut x: u64| -> [f32; N] {
        std::array::from_fn(|_| {
            x = 22_695_477u64.wrapping_mul(x).wrapping_add(1) & 0xFFFF_FFFF;
            x as u32 as f32 / u32::MAX as f32
        })
    };
    let (a, b) = (series(seed), series(!seed));
    std::array::from_fn(|i| a[i] * (1.0 - cross) + b[i] * cross)
}

/// A one-pole low-pass (Cloud Seed's Lp1).
#[derive(Default, Clone, Copy)]
struct Lp1 {
    b0: f32,
    a1: f32,
    out: f32,
}

impl Lp1 {
    fn set(&mut self, hz: f32, sample_rate: f32) {
        let hz = hz.min(0.499 * sample_rate);
        let x = TAU * hz / sample_rate;
        let nn = 2.0 - x.cos();
        let alpha = nn - (nn * nn - 1.0).sqrt();
        (self.a1, self.b0) = (alpha, 1.0 - alpha);
    }

    fn tick(&mut self, x: f32) -> f32 {
        self.out = if x == 0.0 && self.out < 1e-7 {
            0.0
        } else {
            self.b0 * x + self.a1 * self.out
        };
        self.out
    }
}

/// A delay read at a sine-modulated, linearly interpolated position.
#[derive(Default)]
struct ModDelay {
    buf: Box<[f32]>,
    write: usize,
    phase: f32,
    count: u32,
    /// Base delay and modulation depth in samples; rate in cycles per sample.
    delay: f32,
    amount: f32,
    rate: f32,
    read: (usize, usize),
    gains: (f32, f32),
}

impl ModDelay {
    fn new(len: usize, phase: f32) -> Self {
        Self {
            buf: vec![0.0; len.max(4)].into_boxed_slice(),
            phase,
            delay: 1.0,
            ..Self::default()
        }
    }

    fn update(&mut self) {
        let n = self.buf.len();
        self.phase = (self.phase + self.rate * MOD_EVERY as f32).fract();
        let total =
            (self.delay + self.amount * (self.phase * TAU).sin()).clamp(1.0, (n - 2) as f32);
        let whole = total as usize;
        self.gains = (1.0 - (total - whole as f32), total - whole as f32);
        self.read = (
            (self.write + n - whole) % n,
            (self.write + n - whole - 1) % n,
        );
    }

    fn tick(&mut self, x: f32) -> f32 {
        if self.count == 0 {
            self.update();
        }
        self.count = (self.count + 1) % MOD_EVERY;
        let n = self.buf.len();
        self.buf[self.write] = x;
        let y = self.buf[self.read.0] * self.gains.0 + self.buf[self.read.1] * self.gains.1;
        self.write = (self.write + 1) % n;
        self.read = ((self.read.0 + 1) % n, (self.read.1 + 1) % n);
        y
    }

    fn clear(&mut self) {
        self.buf.fill(0.0);
    }
}

/// A Schroeder all-pass whose delay is modulated (Cloud Seed's ModulatedAllpass).
#[derive(Default)]
struct Allpass {
    buf: Box<[f32]>,
    index: usize,
    phase: f32,
    count: u32,
    delay: usize,
    feedback: f32,
    amount: f32,
    rate: f32,
    taps: (usize, usize),
    gains: (f32, f32),
}

impl Allpass {
    fn update(&mut self) {
        self.phase = (self.phase + self.rate * MOD_EVERY as f32).fract();
        let amount = self.amount.min(self.delay as f32 - 1.0).max(0.0);
        let total = (self.delay as f32 + amount * (self.phase * TAU).sin())
            .clamp(1.0, (self.buf.len() - 2) as f32);
        let whole = total as usize;
        self.taps = (whole, whole + 1);
        self.gains = (1.0 - (total - whole as f32), total - whole as f32);
    }

    fn tick(&mut self, x: f32) -> f32 {
        if self.count == 0 {
            self.update();
        }
        self.count = (self.count + 1) % MOD_EVERY;
        let n = self.buf.len();
        let at = |d: usize| (self.index + n - d) % n;
        let out =
            self.buf[at(self.taps.0)] * self.gains.0 + self.buf[at(self.taps.1)] * self.gains.1;
        let v = x + out * self.feedback;
        self.buf[self.index] = v;
        self.index = (self.index + 1) % n;
        out - v * self.feedback
    }
}

/// One late line: delay, diffuser, high shelf, low-pass, fed back on itself.
#[derive(Default)]
struct Line {
    delay: ModDelay,
    diffuser: [Allpass; LATE_STAGES],
    shelf: Biquad,
    lowpass: Lp1,
    lowpass_on: bool,
    feedback: f32,
    last: f32,
}

impl Line {
    fn tick(&mut self, x: f32) -> f32 {
        let mut t = self.delay.tick(x + self.last * self.feedback);
        for a in &mut self.diffuser {
            t = a.tick(t);
        }
        t = self.shelf.process(t);
        if self.lowpass_on {
            t = self.lowpass.tick(t);
        }
        if t.abs() < 1e-20 {
            t = 0.0;
        }
        self.last = t;
        t
    }
}

/// One side of the reverb.
struct Channel {
    highpass: (f32, f32, f32),
    pre: ModDelay,
    lines: [Line; LINES],
    /// Per line: modulation depth, modulation rate and delay-length seeds.
    seeds: [f32; LINES * 3],
}

/// The "Dark Plate" program's fixed settings, in real units.
const LOW_CUT_HZ: f32 = 200.0;
const INPUT_MIX: f32 = 0.2347;
const LATE_OUT_DB: f32 = -10.16;
const DIFFUSE_DELAY_MS: f32 = 31.6;
const DIFFUSE_FEEDBACK: f32 = 0.8507;
const SHELF_HZ: f32 = 4_518.0;
const SHELF_DB: f32 = -4.64;
const LINE_MOD_HZ: f32 = 0.0946;
const DIFFUSE_MOD_HZ: f32 = 0.0583;
/// Late-diffuser modulation depth relative to the line's (0.1468 / 0.272).
const DIFFUSE_MOD_RATIO: f32 = 0.54;
const SEED_DELAY: u64 = 218;
const SEED_POST_DIFFUSION: u64 = 365;
/// Longest line delay the knobs reach: SIZE 1 s times the 1.5 seed spread.
const MAX_LINE_SECONDS: f32 = 1.52;
const MAX_PREDELAY_SECONDS: f32 = 0.51;

impl Channel {
    fn new(sample_rate: f32, right: bool) -> Self {
        // Cloud Seed's cross seed at 0: the left side runs on the inverted seed.
        let cross = if right { 0.0 } else { 1.0 };
        let phases = random_series::<{ LINES * (STAGES + 1) + 1 }>(7 + right as u64, 0.0);
        let mut phase = phases.iter().map(|p| 0.01 + 0.98 * p);
        let line_len = (MAX_LINE_SECONDS * sample_rate) as usize + 8;
        let ap_len = (0.04 * sample_rate) as usize + 8;
        let lines = std::array::from_fn(|i| {
            let post = random_series::<{ STAGES * 3 }>(SEED_POST_DIFFUSION * (i as u64 + 1), cross);
            let delay = DIFFUSE_DELAY_MS / 1000.0 * sample_rate;
            Line {
                delay: ModDelay::new(line_len, phase.next().unwrap_or(0.5)),
                diffuser: std::array::from_fn(|s| Allpass {
                    buf: vec![0.0; ap_len].into_boxed_slice(),
                    phase: phase.next().unwrap_or(0.5),
                    delay: ((delay * 10f32.powf(post[s]) * 0.1) as usize).max(2),
                    feedback: DIFFUSE_FEEDBACK,
                    // Depth and rate are set per knob turn from these seeds.
                    amount: 0.85 + 0.3 * post[STAGES + s],
                    rate: DIFFUSE_MOD_HZ * (0.85 + 0.3 * post[STAGES * 2 + s]) / sample_rate,
                    ..Allpass::default()
                }),
                shelf: eq_band(EqKind::HighShelf, SHELF_HZ, 0.707, SHELF_DB, sample_rate),
                ..Line::default()
            }
        });
        Self {
            highpass: (0.0, 0.0, 0.0),
            pre: ModDelay::new((MAX_PREDELAY_SECONDS * sample_rate) as usize + 4, 0.5),
            lines,
            seeds: random_series(SEED_DELAY, cross),
        }
    }

    fn clear(&mut self) {
        self.highpass.2 = 0.0;
        self.pre.clear();
        for l in &mut self.lines {
            l.delay.clear();
            l.last = 0.0;
            l.lowpass.out = 0.0;
            l.shelf.reset();
            for a in &mut l.diffuser {
                a.buf.fill(0.0);
            }
        }
    }
}

pub struct CloudSeed {
    sr: f32,
    values: [f32; 5],
    sides: Option<Box<[Channel; 2]>>,
    /// The base modulation depth of each late diffuser stage (seeded), so MOD
    /// can scale it without the seeds.
    stage_depth: [[[f32; LATE_STAGES]; LINES]; 2],
    out_gain: f32,
}

impl Default for CloudSeed {
    fn default() -> Self {
        Self {
            sr: 0.0,
            values: PARAMS.map(|p| p.default),
            sides: None,
            stage_depth: [[[1.0; LATE_STAGES]; LINES]; 2],
            out_gain: 10f32.powf(LATE_OUT_DB / 20.0) / (LINES as f32).sqrt(),
        }
    }
}

impl CloudSeed {
    /// Line lengths, feedback and modulation from SIZE, DECAY and MOD (the
    /// original's UpdateLines), and the loop low-pass from TONE.
    fn update_lines(&mut self) {
        let sr = self.sr;
        let [pre, size, decay, modulation, tone] = self.values;
        let Some(sides) = &mut self.sides else {
            return;
        };
        let line_mod = modulation / 100.0 * 2.5 / 1000.0 * sr;
        for (side, ch) in sides.iter_mut().enumerate() {
            ch.pre.delay = (pre / 1000.0 * sr).max(1.0);
            let seeds = ch.seeds;
            for (i, line) in ch.lines.iter_mut().enumerate() {
                let amount = line_mod * (0.7 + 0.3 * seeds[i]);
                let rate = LINE_MOD_HZ * (0.7 + 0.3 * seeds[LINES + i]) / sr;
                let samples = ((0.5 + seeds[LINES * 2 + i]) * size / 1000.0 * sr).max(amount + 2.0);
                // The loop is the line plus its diffuser's delays; counting
                // them keeps DECAY honest in the mids (the original left them
                // out, so its tails ran long).
                let loop_len = samples + line.diffuser.iter().map(|a| a.delay as f32).sum::<f32>();
                let db = loop_len / (decay * sr) * -60.0;
                line.delay.delay = samples.floor();
                line.delay.amount = amount;
                line.delay.rate = rate;
                line.feedback = 10f32.powf(db / 20.0);
                line.lowpass_on = tone < 19_000.0;
                line.lowpass.set(tone, sr);
                for (a, depth) in line.diffuser.iter_mut().zip(self.stage_depth[side][i]) {
                    a.amount = line_mod * DIFFUSE_MOD_RATIO * depth;
                }
            }
        }
    }
}

impl Effect for CloudSeed {
    fn params(&self) -> &'static [ParamSpec] {
        &PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        if let Some(v) = self.values.get_mut(index) {
            *v = value;
            self.update_lines();
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
        let sides = Box::new([
            Channel::new(sample_rate, false),
            Channel::new(sample_rate, true),
        ]);
        for (depths, ch) in self.stage_depth.iter_mut().zip(sides.iter()) {
            for (d, line) in depths.iter_mut().zip(&ch.lines) {
                *d = std::array::from_fn(|s| line.diffuser[s].amount);
            }
        }
        self.sides = Some(sides);
        self.update_lines();
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let Some(sides) = &mut self.sides else {
            return;
        };
        // Input low cut (Cloud Seed's Hp1: input minus a one-pole low-pass).
        let x = TAU * LOW_CUT_HZ / self.sr;
        let nn = 2.0 - x.cos();
        let alpha = nn - (nn * nn - 1.0).sqrt();
        let (cm, cmi) = (INPUT_MIX * 0.5, 1.0 - INPUT_MIX * 0.5);
        let gain = self.out_gain;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let inputs = [*l * cmi + *r * cm, *r * cmi + *l * cm];
            let mut out = [0.0f32; 2];
            for ((ch, x), o) in sides.iter_mut().zip(inputs).zip(&mut out) {
                let lp = &mut ch.highpass.2;
                *lp = (1.0 - alpha) * x + alpha * *lp;
                let mut t = x - *lp;
                if t * t < 1e-9 {
                    t = 0.0;
                }
                t = ch.pre.tick(t);
                *o = ch.lines.iter_mut().map(|line| line.tick(t)).sum::<f32>() * gain;
            }
            (*l, *r) = (out[0], out[1]);
        }
    }
    fn reset(&mut self) {
        if let Some(sides) = &mut self.sides {
            sides.iter_mut().for_each(Channel::clear);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::effects::effect;

    const SR: f32 = 48_000.0;

    /// Energy of the left output in each of `n` windows after a click.
    fn tail(params: &[(usize, f32)], windows: usize, len: usize) -> Vec<f32> {
        let mut fx = effect("canary.cloudseed", SR).unwrap();
        for &(i, v) in params {
            fx.set_param(i, v);
        }
        fx.reset();
        let mut l = vec![0.0; windows * len];
        let mut r = l.clone();
        l[0] = 1.0;
        r[0] = 1.0;
        for (a, b) in l.chunks_mut(512).zip(r.chunks_mut(512)) {
            fx.process(a, b);
        }
        l.chunks(len)
            .map(|w| w.iter().map(|s| s * s).sum())
            .collect()
    }

    #[test]
    fn decay_sets_how_long_it_rings_and_it_stays_stable() {
        // DECAY is the loop's time to -60 dB in the mids; the diffusers' extra
        // low-frequency delay lets lows ring somewhat longer.
        let db = |t: &[f32], a: usize, b: usize| 10.0 * (t[b] / t[a].max(1e-30)).log10();
        let short = tail(&[(2, 1.0)], 8, 24_000);
        let long = tail(&[(2, 20.0)], 8, 24_000);
        assert!(long.iter().all(|e| e.is_finite()));
        let (short_drop, long_drop) = (db(&short, 1, 3), db(&long, 1, 3));
        assert!(short_drop < -15.0, "{short_drop} dB over a second");
        assert!(long_drop > -8.0 && long_drop < 1.0, "{long_drop} dB");
        assert!(
            db(&short, 1, 7) < -40.0,
            "keeps falling: {}",
            db(&short, 1, 7)
        );
    }

    #[test]
    fn the_tail_is_wide_and_starts_after_the_predelay() {
        let mut fx = effect("canary.cloudseed", SR).unwrap();
        fx.set_param(0, 100.0);
        fx.reset();
        let mut l = vec![0.0; 48_000];
        let mut r = l.clone();
        l[0] = 1.0;
        r[0] = 1.0;
        fx.process(&mut l, &mut r);
        assert!(
            l[..4_700].iter().all(|s| s.abs() < 1e-6),
            "silent before 100 ms"
        );
        let corr: f32 = l.iter().zip(&r).map(|(a, b)| a * b).sum::<f32>()
            / (l.iter().map(|a| a * a).sum::<f32>() * r.iter().map(|b| b * b).sum::<f32>()).sqrt();
        assert!(corr.abs() < 0.5, "decorrelated sides: {corr}");
    }
}
