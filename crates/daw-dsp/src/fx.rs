//! The stock effects: EQ, reverb, delay, limiter, gate, filter, chorus,
//! bitcrusher and utility. Every buffer is sized in `set_sample_rate` (on the
//! main thread, when the effect is made), so `process` never allocates.

use std::f32::consts::{FRAC_PI_4, TAU};

use crate::effects::{Effect, ParamSpec};
use crate::filter::{db_to_gain, gain_to_db, Biquad, FilterKind};
use crate::Smoother;

const GLIDE: f32 = 0.02;

const fn spec(name: &'static str, min: f32, max: f32, default: f32) -> ParamSpec {
    ParamSpec {
        name,
        min,
        max,
        default,
        log: false,
    }
}

const fn freq(name: &'static str, min: f32, max: f32, default: f32) -> ParamSpec {
    ParamSpec {
        name,
        min,
        max,
        default,
        log: true,
    }
}

/// Peaking and shelving biquads (RBJ cookbook), for the EQ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqKind {
    Peak,
    LowShelf,
    HighShelf,
}

/// Coefficients for an EQ band: `gain_db` at `freq` with width `q`.
pub fn eq_band(kind: EqKind, freq: f32, q: f32, gain_db: f32, sample_rate: f32) -> Biquad {
    let freq = freq.clamp(10.0, 0.49 * sample_rate);
    let a = 10f32.powf(gain_db / 40.0);
    let w0 = TAU * freq / sample_rate;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * q.clamp(0.1, 20.0));
    let (b0, b1, b2, a0, a1, a2) = match kind {
        EqKind::Peak => (
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        ),
        EqKind::LowShelf => {
            let s = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) - (a - 1.0) * cos + s),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - s),
                (a + 1.0) + (a - 1.0) * cos + s,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - s,
            )
        }
        EqKind::HighShelf => {
            let s = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) + (a - 1.0) * cos + s),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - s),
                (a + 1.0) - (a - 1.0) * cos + s,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - s,
            )
        }
    };
    Biquad::from_coefficients(b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0)
}

/// Copies new coefficients into a running filter without clearing its state.
fn retune(f: &mut Biquad, new: Biquad) {
    let state = f.state();
    *f = new;
    f.set_state(state);
}

// ---------------------------------------------------------------- EQ

/// Low cut, low shelf (100 Hz), a peaking mid band and a high shelf (8 kHz).
#[derive(Default)]
pub struct Eq {
    sr: f32,
    values: [f32; 5],
    bands: [[Biquad; 2]; 4],
}

impl Eq {
    const PARAMS: [ParamSpec; 5] = [
        freq("LOWCUT", 10.0, 1_000.0, 10.0),
        spec("LOW", -18.0, 18.0, 0.0),
        freq("MID HZ", 100.0, 10_000.0, 1_000.0),
        spec("MID", -18.0, 18.0, 0.0),
        spec("HIGH", -18.0, 18.0, 0.0),
    ];

    fn update(&mut self) {
        if self.sr <= 0.0 {
            return;
        }
        let [cut, low, mid_hz, mid, high] = self.values;
        let mut hp = Biquad::default();
        hp.set(
            FilterKind::HighPass,
            cut,
            std::f32::consts::FRAC_1_SQRT_2,
            self.sr,
        );
        let made = [
            hp,
            eq_band(EqKind::LowShelf, 100.0, 0.7, low, self.sr),
            eq_band(EqKind::Peak, mid_hz, 1.0, mid, self.sr),
            eq_band(EqKind::HighShelf, 8_000.0, 0.7, high, self.sr),
        ];
        for (pair, new) in self.bands.iter_mut().zip(made) {
            for f in pair {
                retune(f, new);
            }
        }
    }
}

impl Effect for Eq {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        if let Some(v) = self.values.get_mut(index) {
            *v = value;
            self.update();
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
        self.update();
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        // The low cut only runs once it is above its floor.
        let skip = usize::from(self.values[0] <= 10.0);
        for (ch, buf) in [left, right].into_iter().enumerate() {
            for s in buf.iter_mut() {
                let mut x = *s;
                for band in &mut self.bands[skip..] {
                    x = band[ch].process(x);
                }
                *s = x;
            }
        }
    }
    fn reset(&mut self) {
        self.bands.iter_mut().flatten().for_each(Biquad::reset);
    }
}

// ---------------------------------------------------------------- Reverb

/// A feedback comb with a one-pole low-pass in its loop (Freeverb's).
#[derive(Default)]
struct Comb {
    buf: Box<[f32]>,
    pos: usize,
    store: f32,
}

impl Comb {
    fn process(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let y = self.buf[self.pos];
        self.store = y * (1.0 - damp) + self.store * damp;
        self.buf[self.pos] = x + self.store * feedback;
        self.pos = (self.pos + 1) % self.buf.len();
        y
    }
}

#[derive(Default)]
struct Allpass {
    buf: Box<[f32]>,
    pos: usize,
}

impl Allpass {
    fn process(&mut self, x: f32) -> f32 {
        let b = self.buf[self.pos];
        self.buf[self.pos] = x + b * 0.5;
        self.pos = (self.pos + 1) % self.buf.len();
        b - x
    }
}

/// Comb and all-pass lengths at 44.1 kHz (Freeverb's tuning); the right side
/// runs 23 samples longer for width.
const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
const SPREAD: usize = 23;

/// A Schroeder-Moorer room (Freeverb topology) with predelay. Wet only; the
/// node's MIX sets the blend.
#[derive(Default)]
pub struct Reverb {
    sr: f32,
    size: f32,
    damp: f32,
    width: f32,
    combs: [[Comb; 8]; 2],
    allpasses: [[Allpass; 4]; 2],
    pre: [Box<[f32]>; 2],
    pre_pos: usize,
    pre_len: usize,
}

impl Reverb {
    const PARAMS: [ParamSpec; 4] = [
        spec("SIZE", 0.0, 1.0, 0.6),
        spec("DAMP", 0.0, 1.0, 0.4),
        spec("WIDTH", 0.0, 1.0, 1.0),
        spec("PREDELAY", 0.0, 200.0, 10.0),
    ];
}

impl Effect for Reverb {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.size = value,
            1 => self.damp = value,
            2 => self.width = value,
            3 => {
                let max = self.pre[0].len().max(1) - 1;
                self.pre_len = ((value / 1000.0 * self.sr) as usize).min(max);
            }
            _ => {}
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
        let scale = sample_rate / 44_100.0;
        let len = |n: usize| ((n as f32 * scale) as usize).max(1);
        for (side, (combs, allpasses)) in self.combs.iter_mut().zip(&mut self.allpasses).enumerate()
        {
            for (c, n) in combs.iter_mut().zip(COMBS) {
                *c = Comb {
                    buf: vec![0.0; len(n + side * SPREAD)].into_boxed_slice(),
                    ..Comb::default()
                };
            }
            for (a, n) in allpasses.iter_mut().zip(ALLPASSES) {
                *a = Allpass {
                    buf: vec![0.0; len(n + side * SPREAD)].into_boxed_slice(),
                    pos: 0,
                };
            }
        }
        let pre = (0.2 * sample_rate) as usize + 1;
        self.pre = [
            vec![0.0; pre].into_boxed_slice(),
            vec![0.0; pre].into_boxed_slice(),
        ];
        self.pre_pos = 0;
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let feedback = 0.7 + 0.28 * self.size;
        let damp = self.damp * 0.4;
        let n = self.pre[0].len();
        let (wet1, wet2) = ((1.0 + self.width) / 2.0, (1.0 - self.width) / 2.0);
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            // Predelay: read what was written `pre_len` samples ago.
            let read = (self.pre_pos + n - self.pre_len) % n;
            self.pre[0][self.pre_pos] = *l;
            self.pre[1][self.pre_pos] = *r;
            let input = (self.pre[0][read] + self.pre[1][read]) * 0.015;
            self.pre_pos = (self.pre_pos + 1) % n;
            let mut out = [0.0f32; 2];
            for (side, o) in out.iter_mut().enumerate() {
                let mut y: f32 = self.combs[side]
                    .iter_mut()
                    .map(|c| c.process(input, feedback, damp))
                    .sum();
                for a in &mut self.allpasses[side] {
                    y = a.process(y);
                }
                *o = y;
            }
            *l = out[0] * wet1 + out[1] * wet2;
            *r = out[1] * wet1 + out[0] * wet2;
        }
    }
    fn reset(&mut self) {
        for c in self.combs.iter_mut().flatten() {
            c.buf.fill(0.0);
            c.store = 0.0;
        }
        for a in self.allpasses.iter_mut().flatten() {
            a.buf.fill(0.0);
        }
        self.pre.iter_mut().for_each(|p| p.fill(0.0));
    }
}

// ---------------------------------------------------------------- Delay

/// Stereo delay with feedback through a tone low-pass; PING-PONG crosses the
/// feedback between sides. Wet only.
#[derive(Default)]
pub struct Delay {
    sr: f32,
    lines: [Box<[f32]>; 2],
    pos: usize,
    time: Smoother,
    feedback: f32,
    ping_pong: bool,
    tone: [Biquad; 2],
    tone_hz: f32,
}

impl Delay {
    const PARAMS: [ParamSpec; 4] = [
        spec("TIME MS", 1.0, 2_000.0, 375.0),
        spec("FEEDBACK", 0.0, 95.0, 40.0),
        spec("PINGPONG", 0.0, 1.0, 0.0),
        freq("TONE", 500.0, 20_000.0, 8_000.0),
    ];
}

impl Effect for Delay {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.time.set_target(value / 1000.0 * self.sr),
            1 => self.feedback = value / 100.0,
            2 => self.ping_pong = value >= 0.5,
            3 => {
                self.tone_hz = value;
                for t in &mut self.tone {
                    t.set(FilterKind::LowPass, value, 0.7, self.sr.max(1.0));
                }
            }
            _ => {}
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
        let len = (2.0 * sample_rate) as usize + 4;
        self.lines = [
            vec![0.0; len].into_boxed_slice(),
            vec![0.0; len].into_boxed_slice(),
        ];
        // Glides slowly, so turning TIME bends the pitch like tape, never clicks.
        self.time.set_time(0.2, sample_rate);
        let tone = if self.tone_hz > 0.0 {
            self.tone_hz
        } else {
            8_000.0
        };
        for t in &mut self.tone {
            t.set(FilterKind::LowPass, tone, 0.7, sample_rate);
        }
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = self.lines[0].len();
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let d = self.time.tick().clamp(1.0, (n - 2) as f32);
            // Linear interpolation between the two samples around the delay.
            let back = self.pos as f32 + n as f32 - d;
            let (i, frac) = (back as usize % n, back.fract());
            let read = |line: &[f32]| line[i] * (1.0 - frac) + line[(i + 1) % n] * frac;
            let out = [read(&self.lines[0]), read(&self.lines[1])];
            let fb = [self.tone[0].process(out[0]), self.tone[1].process(out[1])];
            let (fl, fr) = if self.ping_pong {
                (fb[1], fb[0])
            } else {
                (fb[0], fb[1])
            };
            // Ping-pong feeds the input to the left only, so it bounces across.
            let (in_l, in_r) = if self.ping_pong {
                ((*l + *r) * 0.5, 0.0)
            } else {
                (*l, *r)
            };
            self.lines[0][self.pos] = in_l + fl * self.feedback;
            self.lines[1][self.pos] = in_r + fr * self.feedback;
            self.pos = (self.pos + 1) % n;
            *l = out[0];
            *r = out[1];
        }
    }
    fn reset(&mut self) {
        self.lines.iter_mut().for_each(|l| l.fill(0.0));
        self.tone.iter_mut().for_each(Biquad::reset);
        self.time.settle();
    }
}

// ---------------------------------------------------------------- Limiter

/// A brickwall peak limiter: instant attack, smooth release, never above the
/// ceiling (a final clamp catches what the envelope cannot). No lookahead, so
/// no latency.
#[derive(Default)]
pub struct Limiter {
    sr: f32,
    input: Smoother,
    ceiling: f32,
    release: f32,
    gain: f32,
}

impl Limiter {
    const PARAMS: [ParamSpec; 3] = [
        spec("INPUT", 0.0, 24.0, 0.0),
        spec("CEILING", -12.0, 0.0, -0.3),
        spec("RELEASE", 10.0, 1_000.0, 100.0),
    ];
}

impl Effect for Limiter {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.input.set_target(db_to_gain(value)),
            1 => self.ceiling = db_to_gain(value),
            2 => self.release = (-1.0 / (value / 1000.0 * self.sr.max(1.0))).exp(),
            _ => {}
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
        self.input.set_time(GLIDE, sample_rate);
        self.gain = 1.0;
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let g_in = self.input.tick();
            let (xl, xr) = (*l * g_in, *r * g_in);
            let peak = xl.abs().max(xr.abs());
            let want = if peak > self.ceiling {
                self.ceiling / peak
            } else {
                1.0
            };
            // Down at once, back up at the release rate.
            self.gain = if want < self.gain {
                want
            } else {
                want + (self.gain - want) * self.release
            };
            let c = self.ceiling;
            *l = (xl * self.gain).clamp(-c, c);
            *r = (xr * self.gain).clamp(-c, c);
        }
    }
    fn reset(&mut self) {
        self.gain = 1.0;
        self.input.settle();
    }
}

// ---------------------------------------------------------------- Gate

/// A noise gate: below THRESH the signal falls by RANGE dB over RELEASE, and
/// opens again over ATTACK.
#[derive(Default)]
pub struct Gate {
    sr: f32,
    threshold: f32,
    range: f32,
    attack: f32,
    release: f32,
    level: f32,
    gain: f32,
}

impl Gate {
    const PARAMS: [ParamSpec; 4] = [
        spec("THRESH", -80.0, 0.0, -40.0),
        spec("RANGE", 0.0, 80.0, 60.0),
        spec("ATTACK", 0.1, 50.0, 1.0),
        spec("RELEASE", 5.0, 1_000.0, 100.0),
    ];

    fn coef(&self, ms: f32) -> f32 {
        (-1.0 / (ms / 1000.0 * self.sr.max(1.0))).exp()
    }
}

impl Effect for Gate {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.threshold = value,
            1 => self.range = db_to_gain(-value),
            2 => self.attack = self.coef(value),
            3 => self.release = self.coef(value),
            _ => {}
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
        self.gain = 1.0;
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        // A 10 ms peak detector decides open or closed.
        let detect = (-1.0 / (0.01 * self.sr.max(1.0))).exp();
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let x = l.abs().max(r.abs());
            self.level = x.max(self.level * detect);
            let open = gain_to_db(self.level) > self.threshold;
            let (target, coef) = if open {
                (1.0, self.attack)
            } else {
                (self.range, self.release)
            };
            self.gain = target + (self.gain - target) * coef;
            *l *= self.gain;
            *r *= self.gain;
        }
    }
    fn reset(&mut self) {
        self.level = 0.0;
        self.gain = 1.0;
    }
}

// ---------------------------------------------------------------- Filter

/// A resonant multimode filter (low, high or band pass) with input drive.
#[derive(Default)]
pub struct Filter {
    sr: f32,
    mode: FilterKind,
    cutoff: Smoother,
    q: f32,
    drive: Smoother,
    f: [Biquad; 2],
}

impl Filter {
    const PARAMS: [ParamSpec; 4] = [
        spec("MODE", 0.0, 2.0, 0.0),
        freq("CUTOFF", 20.0, 20_000.0, 2_000.0),
        spec("RESO", 0.5, 12.0, 0.7),
        spec("DRIVE", 0.0, 24.0, 0.0),
    ];
}

impl Effect for Filter {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => {
                self.mode = match value.round() as u32 {
                    0 => FilterKind::LowPass,
                    1 => FilterKind::HighPass,
                    _ => FilterKind::BandPass,
                }
            }
            1 => self.cutoff.set_target(value),
            2 => self.q = value,
            3 => self.drive.set_target(db_to_gain(value)),
            _ => {}
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
        self.cutoff.set_time(GLIDE, sample_rate);
        self.drive.set_time(GLIDE, sample_rate);
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        // Coefficients follow the gliding cutoff every 32 samples.
        for (i, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
            let cutoff = self.cutoff.tick();
            if i % 32 == 0 {
                for f in &mut self.f {
                    f.set(self.mode, cutoff, self.q, self.sr);
                }
            }
            let d = self.drive.tick();
            // Soft drive into the filter: tanh, level-compensated.
            let shape = |x: f32| {
                if d > 1.0001 {
                    (x * d).tanh() / d.tanh()
                } else {
                    x
                }
            };
            *l = self.f[0].process(shape(*l));
            *r = self.f[1].process(shape(*r));
        }
    }
    fn reset(&mut self) {
        self.f.iter_mut().for_each(Biquad::reset);
        self.cutoff.settle();
        self.drive.settle();
    }
}

// ---------------------------------------------------------------- Chorus

/// Chorus and flanger: a modulated delay per side, the right side's LFO a
/// quarter turn on. Short DELAY with FEEDBACK flanges. Wet only.
#[derive(Default)]
pub struct Chorus {
    sr: f32,
    lines: [Box<[f32]>; 2],
    pos: usize,
    phase: f32,
    rate: f32,
    depth: f32,
    delay: f32,
    feedback: f32,
}

impl Chorus {
    const PARAMS: [ParamSpec; 4] = [
        spec("RATE", 0.05, 8.0, 0.8),
        spec("DEPTH MS", 0.0, 10.0, 3.0),
        spec("DELAY MS", 0.5, 30.0, 12.0),
        spec("FEEDBACK", 0.0, 90.0, 0.0),
    ];
}

impl Effect for Chorus {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.rate = value,
            1 => self.depth = value / 1000.0 * self.sr,
            2 => self.delay = value / 1000.0 * self.sr,
            3 => self.feedback = value / 100.0,
            _ => {}
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
        // DELAY + DEPTH at most 40 ms.
        let len = (0.045 * sample_rate) as usize + 4;
        self.lines = [
            vec![0.0; len].into_boxed_slice(),
            vec![0.0; len].into_boxed_slice(),
        ];
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = self.lines[0].len();
        let step = TAU * self.rate / self.sr.max(1.0);
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let mut out = [0.0f32; 2];
            for (side, o) in out.iter_mut().enumerate() {
                let lfo = (self.phase + side as f32 * FRAC_PI_4 * 2.0).sin();
                let d = (self.delay + self.depth * (0.5 + 0.5 * lfo)).clamp(1.0, (n - 2) as f32);
                let back = self.pos as f32 + n as f32 - d;
                let (i, frac) = (back as usize % n, back.fract());
                let line = &self.lines[side];
                *o = line[i] * (1.0 - frac) + line[(i + 1) % n] * frac;
            }
            self.lines[0][self.pos] = *l + out[0] * self.feedback;
            self.lines[1][self.pos] = *r + out[1] * self.feedback;
            self.pos = (self.pos + 1) % n;
            self.phase = (self.phase + step) % TAU;
            *l = out[0];
            *r = out[1];
        }
    }
    fn reset(&mut self) {
        self.lines.iter_mut().for_each(|l| l.fill(0.0));
    }
}

// ---------------------------------------------------------------- Crusher

/// Bit depth and sample-rate reduction (sample and hold).
#[derive(Default)]
pub struct Crusher {
    levels: f32,
    hold: u32,
    count: u32,
    held: [f32; 2],
}

impl Crusher {
    const PARAMS: [ParamSpec; 2] = [
        spec("BITS", 1.0, 16.0, 8.0),
        spec("DOWNSMPL", 1.0, 32.0, 4.0),
    ];
}

impl Effect for Crusher {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.levels = 2f32.powf(value.round() - 1.0),
            1 => self.hold = value.round().max(1.0) as u32,
            _ => {}
        }
    }
    fn set_sample_rate(&mut self, _sample_rate: f32) {}
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let q = self.levels.max(1.0);
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            if self.count == 0 {
                self.held = [(*l * q).round() / q, (*r * q).round() / q];
            }
            self.count = (self.count + 1) % self.hold.max(1);
            *l = self.held[0];
            *r = self.held[1];
        }
    }
    fn reset(&mut self) {
        self.count = 0;
        self.held = [0.0; 2];
    }
}

// ---------------------------------------------------------------- Utility

/// Gain, stereo width (0 mono, 1 as is, 2 extra wide), equal-power pan and a
/// polarity flip.
#[derive(Default)]
pub struct Utility {
    gain: Smoother,
    width: Smoother,
    pan: Smoother,
    invert: bool,
}

impl Utility {
    const PARAMS: [ParamSpec; 4] = [
        spec("GAIN", -48.0, 24.0, 0.0),
        spec("WIDTH", 0.0, 2.0, 1.0),
        spec("PAN", -1.0, 1.0, 0.0),
        spec("INVERT", 0.0, 1.0, 0.0),
    ];
}

impl Effect for Utility {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.gain.set_target(db_to_gain(value)),
            1 => self.width.set_target(value),
            2 => self.pan.set_target(value),
            3 => self.invert = value >= 0.5,
            _ => {}
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        for s in [&mut self.gain, &mut self.width, &mut self.pan] {
            s.set_time(GLIDE, sample_rate);
        }
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let sign = if self.invert { -1.0 } else { 1.0 };
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let (g, w, p) = (self.gain.tick(), self.width.tick(), self.pan.tick());
            let mid = (*l + *r) * 0.5;
            let side = (*l - *r) * 0.5 * w;
            // Equal power, normalized so the center is unity.
            let angle = (p + 1.0) * FRAC_PI_4;
            let (pl, pr) = (angle.cos() * 2f32.sqrt(), angle.sin() * 2f32.sqrt());
            *l = (mid + side) * g * pl * sign;
            *r = (mid - side) * g * pr * sign;
        }
    }
    fn reset(&mut self) {
        for s in [&mut self.gain, &mut self.width, &mut self.pan] {
            s.settle();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::effect;

    const SR: f32 = 48_000.0;

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (TAU * freq * i as f32 / SR).sin())
            .collect()
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0, |m, s| m.max(s.abs()))
    }

    /// Runs `id` with params set, on a stereo copy of `input`.
    fn run(id: &str, params: &[(usize, f32)], input: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut fx = effect(id, SR).unwrap();
        for &(i, v) in params {
            fx.set_param(i, v);
        }
        // As the node does before its first block: start at the set values.
        fx.reset();
        let (mut l, mut r) = (input.to_vec(), input.to_vec());
        fx.process(&mut l, &mut r);
        (l, r)
    }

    #[test]
    fn every_stock_effect_is_registered_and_stays_finite_and_bounded() {
        for (id, _) in crate::effects::EFFECTS {
            let noise: Vec<f32> = (0..9_600)
                .map(|i| ((i * 7919) % 2000) as f32 / 1000.0 - 1.0)
                .collect();
            let (l, r) = run(id, &[], &noise);
            assert!(
                l.iter().chain(&r).all(|s| s.is_finite() && s.abs() < 8.0),
                "{id}"
            );
        }
    }

    #[test]
    fn eq_boosts_its_mid_band_and_leaves_others() {
        let at = |f: f32| {
            peak(
                &run(
                    "canary.eq",
                    &[(2, 1_000.0), (3, 12.0)],
                    &sine(f, 0.1, 24_000),
                )
                .0[12_000..],
            )
        };
        assert!((gain_to_db(at(1_000.0) / 0.1) - 12.0).abs() < 0.5);
        assert!(gain_to_db(at(10_000.0) / 0.1).abs() < 1.0);
    }

    #[test]
    fn reverb_rings_on_after_the_input_stops() {
        let mut click = vec![0.0; 48_000];
        click[0] = 1.0;
        let (l, _) = run("canary.reverb", &[(3, 0.0)], &click);
        assert!(peak(&l[24_000..]) > 1e-4, "a half-second tail");
        assert!(peak(&l[..200]) < 1e-6, "comb delay before the first echo");
    }

    #[test]
    fn delay_echoes_after_its_time_and_decays_by_the_feedback() {
        let mut click = vec![0.0; 48_000];
        click[0] = 1.0;
        let (l, _) = run(
            "canary.delay",
            &[(0, 100.0), (1, 50.0), (3, 20_000.0)],
            &click,
        );
        // Sums, not peaks: the tone filter spreads each echo but keeps its area.
        let first: f32 = l[4_780..4_840].iter().sum();
        let second: f32 = l[9_580..9_640].iter().sum();
        assert!(first > 0.9, "{first}");
        assert!((second / first - 0.5).abs() < 0.1, "{second}");
    }

    #[test]
    fn limiter_never_passes_the_ceiling() {
        let (l, _) = run(
            "canary.limiter",
            &[(0, 18.0), (1, -1.0)],
            &sine(100.0, 0.9, 9_600),
        );
        assert!(peak(&l) <= db_to_gain(-1.0) + 1e-6);
        assert!(peak(&l) > db_to_gain(-1.5), "and gets close to it");
    }

    #[test]
    fn gate_closes_on_quiet_input_and_passes_loud() {
        let quiet = run("canary.gate", &[], &sine(200.0, 0.001, 48_000)).0;
        assert!(peak(&quiet[24_000..]) < 1e-5);
        let loud = run("canary.gate", &[], &sine(200.0, 0.5, 48_000)).0;
        assert!(peak(&loud[24_000..]) > 0.49);
    }

    #[test]
    fn filter_modes_cut_the_other_end() {
        let lp_high =
            peak(&run("canary.filter", &[(1, 500.0)], &sine(8_000.0, 1.0, 9_600)).0[4_800..]);
        let hp_low = peak(
            &run(
                "canary.filter",
                &[(0, 1.0), (1, 5_000.0)],
                &sine(100.0, 1.0, 9_600),
            )
            .0[4_800..],
        );
        assert!(lp_high < 0.02 && hp_low < 0.02, "{lp_high} {hp_low}");
    }

    #[test]
    fn crusher_quantizes_and_utility_folds_to_mono() {
        let (l, _) = run(
            "canary.crusher",
            &[(0, 2.0), (1, 1.0)],
            &sine(50.0, 0.8, 960),
        );
        assert!(l.iter().all(|s| [-1.0, -0.5, 0.0, 0.5, 1.0].contains(s)));
        let mut fx = effect("canary.utility", SR).unwrap();
        fx.set_param(1, 0.0);
        fx.reset();
        let (mut a, mut b) = (vec![1.0; 64], vec![0.0; 64]);
        fx.process(&mut a, &mut b);
        assert!((a[63] - 0.5).abs() < 1e-4 && (b[63] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn chorus_output_moves_with_its_lfo() {
        let (l, r) = run(
            "canary.chorus",
            &[(0, 2.0), (1, 5.0)],
            &sine(440.0, 0.5, 48_000),
        );
        let diff = l
            .iter()
            .zip(&r)
            .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(diff > 0.05, "the sides differ: {diff}");
    }
}
