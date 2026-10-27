//! Built-in stereo effects: compressor, saturator, dynamic EQ and de-esser.
//! Each processes in place; dry/wet, level matching and delta live in the graph's
//! `EffectNode`, so every effect gets them the same way.

use crate::filter::{db_to_gain, gain_to_db, Biquad, Envelope, FilterKind};
use crate::Smoother;

/// Glide time for levels set by a knob (threshold, makeup, drive), so a turn
/// never steps the output.
const GLIDE_SECONDS: f32 = 0.02;

/// One parameter: what the user turns. Values are in the units the name says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamSpec {
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    /// Turns logarithmically (frequencies).
    pub log: bool,
}

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

/// A stereo effect. Real-time safe: `set_param` and `process` never allocate.
pub trait Effect: Send {
    fn params(&self) -> &'static [ParamSpec];
    /// Called before the first block and whenever a value changes; values are
    /// already clamped to the spec's range.
    fn set_param(&mut self, index: usize, value: f32);
    fn set_sample_rate(&mut self, sample_rate: f32);
    fn process(&mut self, left: &mut [f32], right: &mut [f32]);
    /// Clears signal state and jumps every gliding knob to its value.
    fn reset(&mut self);
}

/// The built-in effects by id, for the device picker: (id, display name).
pub const EFFECTS: [(&str, &str); 4] = [
    ("canary.compressor", "Compressor"),
    ("canary.saturator", "Saturator"),
    ("canary.dynamic_eq", "Dynamic EQ"),
    ("canary.deesser", "De-esser"),
];

/// Makes the effect with `id` (see `EFFECTS`), set to its defaults.
pub fn effect(id: &str, sample_rate: f32) -> Option<Box<dyn Effect>> {
    let mut fx: Box<dyn Effect> = match id {
        "canary.compressor" => Box::new(Compressor::default()),
        "canary.saturator" => Box::new(Saturator::default()),
        "canary.dynamic_eq" => Box::new(DynamicEq::default()),
        "canary.deesser" => Box::new(DeEsser::default()),
        _ => return None,
    };
    fx.set_sample_rate(sample_rate);
    for (i, p) in fx.params().iter().enumerate() {
        fx.set_param(i, p.default);
    }
    fx.reset();
    Some(fx)
}

/// Feed-forward compressor with a 6 dB soft knee and stereo-linked peak detection.
#[derive(Debug, Default)]
pub struct Compressor {
    threshold: Smoother,
    ratio: f32,
    attack: f32,
    release: f32,
    makeup: Smoother,
    sample_rate: f32,
    env: Envelope,
}

const KNEE_DB: f32 = 6.0;

/// Gain change in dB (zero or negative) for a level `over` dB above threshold.
fn reduction(over: f32, ratio: f32) -> f32 {
    let slope = 1.0 / ratio - 1.0;
    if 2.0 * over < -KNEE_DB {
        0.0
    } else if 2.0 * over.abs() <= KNEE_DB {
        slope * (over + KNEE_DB / 2.0).powi(2) / (2.0 * KNEE_DB)
    } else {
        slope * over
    }
}

impl Compressor {
    const PARAMS: [ParamSpec; 5] = [
        spec("THRESH", -60.0, 0.0, -18.0),
        spec("RATIO", 1.0, 20.0, 4.0),
        spec("ATTACK", 0.1, 100.0, 10.0),
        spec("RELEASE", 10.0, 1000.0, 120.0),
        spec("MAKEUP", 0.0, 24.0, 0.0),
    ];

    fn update_times(&mut self) {
        let sr = self.sample_rate;
        self.env
            .set_times(self.attack / 1000.0, self.release / 1000.0, sr);
    }
}

impl Effect for Compressor {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.threshold.set_target(value),
            1 => self.ratio = value.max(1.0),
            2 => self.attack = value,
            3 => self.release = value,
            4 => self.makeup.set_target(value),
            _ => {}
        }
        self.update_times();
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.threshold.set_time(GLIDE_SECONDS, sample_rate);
        self.makeup.set_time(GLIDE_SECONDS, sample_rate);
        self.update_times();
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let level = self.env.process(l.abs().max(r.abs()));
            let over = gain_to_db(level) - self.threshold.tick();
            let db = reduction(over, self.ratio) + self.makeup.tick();
            let g = db_to_gain(db);
            *l *= g;
            *r *= g;
        }
    }
    fn reset(&mut self) {
        self.env.reset();
        self.threshold.settle();
        self.makeup.settle();
    }
}

/// Soft tanh saturation with first-order antiderivative anti-aliasing (ADAA),
/// a bias for even harmonics, and a 10 Hz DC blocker.
// shortcut: first-order ADAA is a half-sample average, so it also rolls off the
// top octave (-2 dB at 10 kHz at 48 kHz) and comb-filters against dry at MIX
// below 100%. Move to 2x oversampling (and report its latency) if that is heard.
#[derive(Debug, Default)]
pub struct Saturator {
    /// Per channel: drive (linear) and bias, gliding to the knob.
    glide: [(Smoother, Smoother); 2],
    /// Per channel: last driven input and its ln(cosh), DC blocker input and
    /// output, and the last bias with its tanh.
    state: [Channel; 2],
    dc_coef: f32,
}

#[derive(Debug, Default, Clone, Copy)]
struct Channel {
    x1: f64,
    f1: f64,
    dc_in: f32,
    dc_out: f32,
    bias: f64,
    rest: f64,
}

/// ln(cosh(x)), the antiderivative of tanh, without overflow for large |x|.
fn ln_cosh(x: f64) -> f64 {
    let a = x.abs();
    a + (-2.0 * a).exp().ln_1p() - std::f64::consts::LN_2
}

impl Saturator {
    const PARAMS: [ParamSpec; 2] = [spec("DRIVE", 0.0, 36.0, 12.0), spec("BIAS", 0.0, 0.5, 0.0)];

    fn channel(&mut self, ch: usize, samples: &mut [f32]) {
        let (drive, bias) = &mut self.glide[ch];
        let mut c = self.state[ch];
        for s in samples {
            let b = f64::from(bias.tick());
            if b != c.bias {
                // Only while the bias knob glides: tanh is the costly part.
                (c.bias, c.rest) = (b, b.tanh());
            }
            let x = f64::from(*s) * f64::from(drive.tick()) + b;
            let d = x - c.x1;
            // Each sample's ln(cosh) is kept for the next: one per sample, not two.
            let f = ln_cosh(x);
            let y = if d.abs() < 1e-5 {
                (0.5 * (x + c.x1)).tanh()
            } else {
                (f - c.f1) / d
            };
            (c.x1, c.f1) = (x, f);
            let y = (y - c.rest) as f32;
            c.dc_out = y - c.dc_in + self.dc_coef * c.dc_out;
            c.dc_in = y;
            *s = c.dc_out;
        }
        self.state[ch] = c;
    }
}

impl Effect for Saturator {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        for (drive, bias) in &mut self.glide {
            match index {
                0 => drive.set_target(db_to_gain(value)),
                1 => bias.set_target(value),
                _ => {}
            }
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.dc_coef = (-2.0 * std::f32::consts::PI * 10.0 / sample_rate).exp();
        for (drive, bias) in &mut self.glide {
            drive.set_time(GLIDE_SECONDS, sample_rate);
            bias.set_time(GLIDE_SECONDS, sample_rate);
        }
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        self.channel(0, left);
        self.channel(1, right);
    }
    fn reset(&mut self) {
        self.state = Default::default();
        for (drive, bias) in &mut self.glide {
            drive.settle();
            bias.settle();
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum BandMode {
    /// Cuts a bell around the frequency.
    #[default]
    Bell,
    /// Cuts below the frequency.
    LowCut,
    /// Cuts above the frequency (de-essing).
    HighCut,
}

/// One band that cuts itself when its own level passes the threshold: 4:1 above
/// threshold, never more than `range` dB. Output is `x - (1 - g) * band(x)`, so at
/// no reduction the signal passes untouched. Cut bands are `x - HP(x)` (lows) and
/// `x - LP(x)` (highs), which split exactly: full reduction leaves a clean filter.
#[derive(Debug, Default)]
struct DynamicBand {
    mode: BandMode,
    freq: f32,
    q: f32,
    threshold: Smoother,
    range: Smoother,
    sample_rate: f32,
    filters: [Biquad; 2],
    env: Envelope,
}

impl DynamicBand {
    fn update(&mut self) {
        let (kind, q) = match self.mode {
            BandMode::Bell => (FilterKind::BandPass, self.q),
            BandMode::LowCut => (FilterKind::HighPass, std::f32::consts::FRAC_1_SQRT_2),
            BandMode::HighCut => (FilterKind::LowPass, std::f32::consts::FRAC_1_SQRT_2),
        };
        for f in &mut self.filters {
            f.set(kind, self.freq, q, self.sample_rate);
        }
    }

    fn set_sample_rate(&mut self, sample_rate: f32, attack: f32, release: f32) {
        self.sample_rate = sample_rate;
        self.env.set_times(attack, release, sample_rate);
        self.threshold.set_time(GLIDE_SECONDS, sample_rate);
        self.range.set_time(GLIDE_SECONDS, sample_rate);
        self.update();
    }

    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let [fl, fr] = &mut self.filters;
        let bell = self.mode == BandMode::Bell;
        let band = |f: &mut Biquad, x: f32| if bell { f.process(x) } else { x - f.process(x) };
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let (bl, br) = (band(fl, *l), band(fr, *r));
            let level = self.env.process(bl.abs().max(br.abs()));
            let over = (gain_to_db(level) - self.threshold.tick()).max(0.0);
            let cut = 1.0 - db_to_gain(-(over * 0.75).min(self.range.tick()));
            *l -= cut * bl;
            *r -= cut * br;
        }
    }

    fn reset(&mut self) {
        self.filters.iter_mut().for_each(Biquad::reset);
        self.env.reset();
        self.threshold.settle();
        self.range.settle();
    }
}

/// A dynamic bell, or a low cut that engages only when the lows pass the threshold.
#[derive(Debug, Default)]
pub struct DynamicEq {
    band: DynamicBand,
}

impl DynamicEq {
    const PARAMS: [ParamSpec; 5] = [
        freq("FREQ", 20.0, 20_000.0, 200.0),
        spec("Q", 0.3, 10.0, 1.0),
        spec("THRESH", -60.0, 0.0, -24.0),
        spec("RANGE", 0.0, 24.0, 12.0),
        // 0 bell, 1 low cut.
        spec("LOWCUT", 0.0, 1.0, 0.0),
    ];
}

impl Effect for DynamicEq {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        let b = &mut self.band;
        match index {
            0 => b.freq = value,
            1 => b.q = value,
            2 => b.threshold.set_target(value),
            3 => b.range.set_target(value),
            4 => {
                b.mode = if value >= 0.5 {
                    BandMode::LowCut
                } else {
                    BandMode::Bell
                }
            }
            _ => {}
        }
        b.update();
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.band.set_sample_rate(sample_rate, 0.001, 0.08);
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        self.band.process(left, right);
    }
    fn reset(&mut self) {
        self.band.reset();
    }
}

/// Cuts the highs above the frequency while they pass the threshold.
#[derive(Debug)]
pub struct DeEsser {
    band: DynamicBand,
}

impl Default for DeEsser {
    fn default() -> Self {
        Self {
            band: DynamicBand {
                mode: BandMode::HighCut,
                ..DynamicBand::default()
            },
        }
    }
}

impl DeEsser {
    const PARAMS: [ParamSpec; 3] = [
        freq("FREQ", 2_000.0, 12_000.0, 6_000.0),
        spec("THRESH", -60.0, 0.0, -30.0),
        spec("RANGE", 0.0, 24.0, 12.0),
    ];
}

impl Effect for DeEsser {
    fn params(&self) -> &'static [ParamSpec] {
        &Self::PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        let b = &mut self.band;
        match index {
            0 => b.freq = value,
            1 => b.threshold.set_target(value),
            2 => b.range.set_target(value),
            _ => {}
        }
        b.update();
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.band.set_sample_rate(sample_rate, 0.0005, 0.06);
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        self.band.process(left, right);
    }
    fn reset(&mut self) {
        self.band.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    const SR: f32 = 48_000.0;

    /// Peak of the last half of `fx` run on a stereo sine.
    fn run(fx: &mut dyn Effect, freq: f32, amp: f32) -> f32 {
        let sine: Vec<f32> = (0..SR as usize)
            .map(|i| amp * (2.0 * PI * freq * i as f32 / SR).sin())
            .collect();
        let (mut l, mut r) = (sine.clone(), sine);
        fx.process(&mut l, &mut r);
        assert_eq!(l, r, "stereo channels stay identical");
        l[l.len() / 2..].iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    fn make(id: &str) -> Box<dyn Effect> {
        effect(id, SR).unwrap()
    }

    #[test]
    fn every_id_makes_an_effect_with_sane_defaults() {
        for (id, _) in EFFECTS {
            let fx = make(id);
            for p in fx.params() {
                assert!(
                    p.min < p.max && (p.min..=p.max).contains(&p.default),
                    "{id}"
                );
                assert!(p.name.len() <= 8, "{} fits a dial", p.name);
            }
        }
        assert!(effect("canary.nope", SR).is_none());
    }

    #[test]
    fn compressor_follows_its_curve() {
        let mut c = make("canary.compressor");
        // Below threshold (and knee): untouched.
        assert!((run(&mut *c, 1_000.0, 0.05) - 0.05).abs() < 1e-3);
        // 0 dBFS peak, threshold -18, ratio 4: 18 dB over becomes 4.5, so -13.5 dB.
        c.reset();
        let out = gain_to_db(run(&mut *c, 1_000.0, 1.0));
        // Peak detection ripples between cycles, so allow 1 dB.
        assert!((out + 13.5).abs() < 1.0, "got {out} dB");
        assert_eq!(reduction(-10.0, 4.0), 0.0);
        assert!(reduction(0.0, 4.0) < 0.0, "inside the knee");
    }

    #[test]
    fn saturator_is_bounded_and_adds_harmonics_without_dc() {
        let mut s = make("canary.saturator");
        s.set_param(0, 36.0);
        s.set_param(1, 0.5);
        let sine: Vec<f32> = (0..SR as usize)
            .map(|i| (2.0 * PI * 100.0 * i as f32 / SR).sin())
            .collect();
        let (mut l, mut r) = (sine.clone(), sine);
        s.process(&mut l, &mut r);
        let tail = &l[l.len() / 2..];
        assert!(tail.iter().all(|x| x.is_finite() && x.abs() < 2.1));
        let mean = tail.iter().sum::<f32>() / tail.len() as f32;
        assert!(mean.abs() < 0.01, "dc {mean}");
        // Near-square wave: far louder than the 0 dB sine's RMS.
        let rms = (tail.iter().map(|x| x * x).sum::<f32>() / tail.len() as f32).sqrt();
        assert!(rms > 0.8, "rms {rms}");
    }

    #[test]
    fn dynamic_eq_cuts_only_loud_band_content() {
        let mut eq = make("canary.dynamic_eq");
        eq.set_param(0, 1_000.0);
        // Quiet in-band: untouched. Loud in-band: cut by up to RANGE (12 dB).
        assert!((run(&mut *eq, 1_000.0, 0.01) - 0.01).abs() < 1e-3);
        eq.reset();
        let loud = gain_to_db(run(&mut *eq, 1_000.0, 1.0));
        assert!((-12.5..-6.0).contains(&loud), "loud band at {loud} dB");
        // Loud out of band: barely touched.
        eq.reset();
        assert!(run(&mut *eq, 10_000.0, 1.0) > 0.8);
    }

    #[test]
    fn low_cut_engages_on_loud_lows_and_deesser_on_loud_highs() {
        let mut eq = make("canary.dynamic_eq");
        eq.set_param(0, 200.0);
        eq.set_param(4, 1.0);
        assert!(run(&mut *eq, 50.0, 1.0) < 0.5, "loud lows cut");
        eq.reset();
        assert!(
            (run(&mut *eq, 50.0, 0.01) - 0.01).abs() < 1e-3,
            "quiet lows kept"
        );

        let mut d = make("canary.deesser");
        assert!(run(&mut *d, 9_000.0, 1.0) < 0.5, "loud ess cut");
        d.reset();
        assert!(run(&mut *d, 200.0, 1.0) > 0.95, "lows untouched");
    }
}
