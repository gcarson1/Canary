//! Character effects ported from Airwindows (Chris Johnson, MIT; see
//! THIRD_PARTY_NOTICES.md): ToTape6, Density2 and PurestDrive. Knobs run 0..1
//! as in the originals; their dry/wet knobs are left to the node's MIX, and
//! their 32-bit dither is left out. Processing stays in f64 like the originals.

use std::f64::consts::{FRAC_PI_2, PI};

use crate::effects::{Effect, ParamSpec};

const fn knob(name: &'static str, default: f32) -> ParamSpec {
    ParamSpec {
        name,
        min: 0.0,
        max: 1.0,
        default,
        log: false,
    }
}

/// Airwindows' "starved or boosted" sine shaping, `density` passes of it.
fn shape(mut x: f64, density: f64, out: f64) -> f64 {
    let mut count = density;
    while count > 1.0 {
        let b = (x.abs() * FRAC_PI_2).min(FRAC_PI_2).sin();
        x = if x > 0.0 { b } else { -b };
        count -= 1.0;
    }
    let mut b = (x.abs() * FRAC_PI_2).min(FRAC_PI_2);
    b = if density > 0.0 {
        b.sin()
    } else {
        1.0 - b.cos()
    };
    if x > 0.0 {
        x * (1.0 - out) + b * out
    } else {
        x * (1.0 - out) - b * out
    }
}

// ---------------------------------------------------------------- Density2

#[derive(Default, Clone, Copy)]
struct DensitySide {
    last: [f64; 3],
    ata: [f64; 3],
    last_diff: f64,
    iir_a: f64,
    iir_b: f64,
}

/// Density2: saturation that thickens (DENSITY above 0.2) or thins (below),
/// with a high-pass on what it saturates and an OUTPUT trim.
#[derive(Default)]
pub struct Density2 {
    sr: f32,
    values: [f32; 3],
    sides: [DensitySide; 2],
}

const DENSITY_PARAMS: [ParamSpec; 3] = [
    knob("DENSITY", 0.2),
    knob("HIGHPASS", 0.0),
    knob("OUTPUT", 1.0),
];

impl Effect for Density2 {
    fn params(&self) -> &'static [ParamSpec] {
        &DENSITY_PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        if let Some(v) = self.values.get_mut(index) {
            *v = value;
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let scale = f64::from(self.sr.max(1.0)) / 44_100.0;
        let [a, b, c] = self.values.map(f64::from);
        let mut density = a * 5.0 - 1.0;
        let mut out = density.abs();
        while out > 1.0 {
            out -= 1.0;
        }
        density *= density.abs();
        let iir = b.powi(3) / scale;
        const ATA: f64 = 0.915_965_594_177_219;
        for (side, buf) in [left, right].into_iter().enumerate() {
            let s = &mut self.sides[side];
            for x in buf.iter_mut() {
                let dry = f64::from(*x);
                let mut input = dry;
                let mut half =
                    (input + s.last[0] + (-s.last[1] + s.last[2]) * 0.041_421_356_237_309_5) / 2.0;
                let half_dry = half;
                s.last = [input, s.last[0], s.last[1]];
                s.iir_b = s.iir_b * (1.0 - iir) + half * iir;
                half -= s.iir_b;
                half = shape(half, density, out);
                s.ata[2] = half - half_dry;
                s.ata[0] *= ATA;
                s.ata[1] *= ATA;
                s.ata[1] += s.ata[2];
                s.ata[0] -= s.ata[2];
                s.ata[2] = s.ata[1];
                let half_diff = s.ata[2] * ATA;
                s.iir_a = s.iir_a * (1.0 - iir) + input * iir;
                input -= s.iir_a;
                input = shape(input, density, out);
                s.ata[2] = input - dry;
                s.ata[0] *= ATA;
                s.ata[1] *= ATA;
                s.ata[0] += s.ata[2];
                s.ata[1] -= s.ata[2];
                s.ata[2] = s.ata[0];
                let diff = s.ata[2] * ATA;
                let y = (dry + (diff + half_diff + s.last_diff) / 1.187) * c;
                s.last_diff = diff / 2.0;
                *x = flush(y) as f32;
            }
        }
    }
    fn reset(&mut self) {
        self.sides = Default::default();
    }
}

/// Zeroes what would turn denormal in the filters' state.
fn flush(x: f64) -> f64 {
    if x.abs() < 1e-30 {
        0.0
    } else {
        x
    }
}

// ---------------------------------------------------------------- PurestDrive

/// PurestDrive: a sine saturation that eases off on quiet or polarity-flipping
/// samples, so highs stay clean.
#[derive(Default)]
pub struct PurestDrive {
    drive: f64,
    previous: [f64; 2],
}

const DRIVE_PARAMS: [ParamSpec; 1] = [knob("DRIVE", 0.5)];

impl Effect for PurestDrive {
    fn params(&self) -> &'static [ParamSpec] {
        &DRIVE_PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        if index == 0 {
            self.drive = f64::from(value);
        }
    }
    fn set_sample_rate(&mut self, _sample_rate: f32) {}
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        for (side, buf) in [left, right].into_iter().enumerate() {
            for x in buf.iter_mut() {
                let dry = f64::from(*x);
                let sat = dry.sin();
                let apply = ((self.previous[side] + sat).abs() / 2.0) * self.drive;
                self.previous[side] = dry.sin();
                *x = (dry * (1.0 - apply) + sat * apply) as f32;
            }
        }
    }
    fn reset(&mut self) {
        self.previous = [0.0; 2];
    }
}

// ---------------------------------------------------------------- ToTape6

/// One interleaved biquad's coefficients and state, Airwindows-style.
#[derive(Default, Clone, Copy)]
struct Bq {
    b0: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z: [f64; 2],
}

impl Bq {
    /// Band-pass at `freq` (a fraction of the rate) with resonance `q`.
    fn band(freq: f64, q: f64) -> Self {
        let k = (PI * freq).tan();
        let norm = 1.0 / (1.0 + k / q + k * k);
        let b0 = k / q * norm;
        Self {
            b0,
            b2: -b0,
            a1: 2.0 * (k * k - 1.0) * norm,
            a2: (1.0 - k / q + k * k) * norm,
            z: [0.0; 2],
        }
    }

    fn tick(&mut self, x: f64) -> f64 {
        let y = x * self.b0 + self.z[0];
        self.z[0] = -y * self.a1 + self.z[1];
        self.z[1] = x * self.b2 - y * self.a2;
        y
    }

    fn retune(&mut self, new: Bq) {
        let z = self.z;
        *self = new;
        self.z = z;
    }
}

/// Per side: the two alternating (flip) sets of roller, head bump and filters.
#[derive(Default, Clone, Copy)]
struct TapeSide {
    roller: [f64; 2],
    bump: [f64; 2],
    bump_bq: [Bq; 2],
    tone_bq: [Bq; 2],
    last: f64,
}

const FLUTTER_LEN: usize = 501;

/// ToTape6: tape saturation with soft high rolloff, head bump, flutter and a
/// soft clip. INPUT and OUTPUT are ±12 dB around the middle; like the original
/// it runs about 4 dB hot at low levels (MATCH on the module evens that out).
pub struct ToTape6 {
    sr: f32,
    values: [f32; 5],
    sides: [TapeSide; 2],
    flutter: [[f64; FLUTTER_LEN]; 2],
    gcount: usize,
    rateof: f64,
    sweep: f64,
    nextmax: f64,
    flip: bool,
    noise: u32,
}

impl Default for ToTape6 {
    fn default() -> Self {
        Self {
            sr: 0.0,
            values: TAPE_PARAMS.map(|p| p.default),
            sides: Default::default(),
            flutter: [[0.0; FLUTTER_LEN]; 2],
            gcount: 0,
            rateof: 0.5,
            sweep: PI,
            nextmax: 0.5,
            flip: false,
            noise: 0x1234_5678,
        }
    }
}

const TAPE_PARAMS: [ParamSpec; 5] = [
    knob("INPUT", 0.5),
    knob("SOFTEN", 0.5),
    knob("HEADBUMP", 0.5),
    knob("FLUTTER", 0.5),
    knob("OUTPUT", 0.5),
];

impl ToTape6 {
    fn retune(&mut self) {
        let scale = f64::from(self.sr.max(1.0)) / 44_100.0;
        let bump = Bq::band(0.007 / scale, 0.0009);
        let tone = Bq::band(0.032 / scale, 0.0007);
        for s in &mut self.sides {
            s.bump_bq.iter_mut().for_each(|b| b.retune(bump));
            s.tone_bq.iter_mut().for_each(|b| b.retune(tone));
        }
    }
}

impl Effect for ToTape6 {
    fn params(&self) -> &'static [ParamSpec] {
        &TAPE_PARAMS
    }
    fn set_param(&mut self, index: usize, value: f32) {
        if let Some(v) = self.values.get_mut(index) {
            *v = value;
        }
    }
    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sr = sample_rate;
        self.retune();
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let scale = f64::from(self.sr.max(1.0)) / 44_100.0;
        let [a, b, c, d, e] = self.values.map(f64::from);
        let input_gain = 10f64.powf((a - 0.5) * 24.0 / 20.0);
        let roll = (1.0 - b * b * 0.45) / scale;
        let bump_control = c * 0.25 * input_gain;
        let bump_freq = 0.12 / scale;
        let depth = d * d * scale * 70.0;
        let flutter_trim = 0.0024 * d * d / scale;
        let output_gain = 10f64.powf((e - 0.5) * 24.0 / 20.0);
        const REFCLIP: f64 = 0.99;
        const SOFTNESS: f64 = 0.618_033_988_749_894_8;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let dry = [f64::from(*l), f64::from(*r)];
            let mut x = dry;
            if input_gain < 1.0 {
                x = x.map(|v| v * input_gain);
            }
            self.noise ^= self.noise << 13;
            self.noise ^= self.noise >> 17;
            self.noise ^= self.noise << 5;
            let randy = f64::from(self.noise) / f64::from(u32::MAX);
            let g = self.gcount;
            self.flutter[0][g] = x[0];
            self.flutter[1][g] = x[1];
            if depth != 0.0 {
                let offset = depth + depth * self.rateof * self.rateof * self.sweep.sin();
                let (whole, frac) = (offset.floor() as usize, offset - offset.floor());
                let at = |i: usize| (g + i) % (FLUTTER_LEN - 1);
                for (v, line) in x.iter_mut().zip(&self.flutter) {
                    *v = line[at(whole)] * (1.0 - frac) + line[at(whole + 1)] * frac;
                }
                self.rateof = self.rateof * (1.0 - flutter_trim) + self.nextmax * flutter_trim;
                self.sweep += self.rateof * flutter_trim;
                if self.sweep >= 2.0 * PI {
                    self.sweep -= PI;
                    self.nextmax = 0.24 + randy * 0.74;
                }
            }
            self.gcount = if g == 0 { FLUTTER_LEN - 2 } else { g - 1 };
            let k = usize::from(self.flip);
            for (side, v) in x.iter_mut().enumerate() {
                let s = &mut self.sides[side];
                let vib_dry = *v;
                s.roller[k] = s.roller[k] * (1.0 - roll) + *v * roll;
                let highs = *v - s.roller[k];
                let mut bump = s.bump[k] + *v * 0.05;
                bump -= bump * bump * bump * bump_freq;
                bump = bump.sin();
                bump = s.bump_bq[k].tick(bump).clamp(-1.0, 1.0).asin();
                s.bump[k] = bump;
                *v = s.tone_bq[k].tick(v.sin()).clamp(-1.0, 1.0).asin();
                let ground = vib_dry - *v;
                if input_gain > 1.0 {
                    *v *= input_gain;
                }
                let soften = 1.0 - (highs.abs() * FRAC_PI_2).min(FRAC_PI_2).cos();
                if highs > 0.0 {
                    *v -= soften;
                } else if highs < 0.0 {
                    *v += soften;
                }
                let suppress = (1.0 - v.abs()) * 0.00013;
                for bump in &mut s.bump {
                    if *bump > suppress {
                        *bump -= suppress;
                    }
                    if *bump < -suppress {
                        *bump += suppress;
                    }
                }
                *v += (s.bump[0] + s.bump[1]) * bump_control;
                *v = v.clamp(-1.0, 1.0);
                let mojo = v.abs().powf(0.25);
                if mojo > 0.0 {
                    *v = (*v * mojo * PI * 0.5).sin() / mojo;
                }
                *v += ground;
                *v *= output_gain;
                // ADClip: a soft knee into the clip, from the last sample.
                if s.last >= REFCLIP {
                    s.last = if *v < REFCLIP {
                        REFCLIP * SOFTNESS + *v * (1.0 - SOFTNESS)
                    } else {
                        REFCLIP
                    };
                }
                if s.last <= -REFCLIP {
                    s.last = if *v > -REFCLIP {
                        -REFCLIP * SOFTNESS + *v * (1.0 - SOFTNESS)
                    } else {
                        -REFCLIP
                    };
                }
                if *v > REFCLIP {
                    *v = if s.last < REFCLIP {
                        REFCLIP * SOFTNESS + s.last * (1.0 - SOFTNESS)
                    } else {
                        REFCLIP
                    };
                }
                if *v < -REFCLIP {
                    *v = if s.last > -REFCLIP {
                        -REFCLIP * SOFTNESS + s.last * (1.0 - SOFTNESS)
                    } else {
                        -REFCLIP
                    };
                }
                s.last = *v;
                *v = flush(v.clamp(-REFCLIP, REFCLIP));
            }
            self.flip = !self.flip;
            (*l, *r) = (x[0] as f32, x[1] as f32);
        }
    }
    fn reset(&mut self) {
        let sr = self.sr;
        *self = Self {
            sr,
            values: self.values,
            ..Self::default()
        };
        self.retune();
    }
}

#[cfg(test)]
mod tests {
    use crate::effects::effect;

    const SR: f32 = 48_000.0;

    fn run(id: &str, params: &[(usize, f32)], amp: f32) -> Vec<f32> {
        let mut fx = effect(id, SR).unwrap();
        for &(i, v) in params {
            fx.set_param(i, v);
        }
        fx.reset();
        let x: Vec<f32> = (0..48_000)
            .map(|i| amp * (std::f32::consts::TAU * 220.0 * i as f32 / SR).sin())
            .collect();
        let (mut l, mut r) = (x.clone(), x);
        for (a, b) in l.chunks_mut(256).zip(r.chunks_mut(256)) {
            fx.process(a, b);
        }
        l
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0, |m, s| m.max(s.abs()))
    }

    #[test]
    fn tape_stays_under_its_clip_and_passes_quiet_audio_near_unity() {
        let hot = run("canary.totape", &[(0, 1.0)], 0.9);
        assert!(hot.iter().all(|s| s.is_finite()) && peak(&hot) <= 0.99 + 1e-6);
        // No flutter, no head bump (which lifts 220 Hz on purpose).
        let quiet = run("canary.totape", &[(2, 0.0), (3, 0.0)], 0.05);
        // Its "mojo" stage lifts quiet audio by about pi/2 (+4 dB), as in the
        // original; OUTPUT trims it.
        let gain = peak(&quiet[24_000..]) / 0.05;
        assert!((1.3..1.8).contains(&gain), "{gain}");
    }

    #[test]
    fn density_and_drive_saturate_loud_and_leave_quiet() {
        // Density's default (0.2) is neutral; PurestDrive eases off when quiet.
        for (id, set) in [("canary.density", 0.2), ("canary.purestdrive", 0.6)] {
            let quiet = run(id, &[(0, set)], 0.01);
            assert!(
                (peak(&quiet[24_000..]) / 0.01 - 1.0).abs() < 0.2,
                "{id} quiet"
            );
            let loud = run(id, &[(0, 1.0)], 1.0);
            assert!(loud.iter().all(|s| s.is_finite()), "{id}");
            // Saturation fattens a sine: its crest factor drops.
            let tail = &loud[24_000..];
            let rms = (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt();
            assert!(peak(tail) / rms < 1.38, "{id}: crest {}", peak(tail) / rms);
        }
    }
}
