//! Biquad filters, a peak envelope follower and decibel conversions.

use std::f32::consts::PI;

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Floors at -120 dB so silence stays finite.
pub fn gain_to_db(gain: f32) -> f32 {
    20.0 * gain.max(1e-6).log10()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    LowPass,
    HighPass,
    /// Constant 0 dB peak gain: `x + (g - 1) * band(x)` is a peaking EQ of gain `g`.
    BandPass,
}

/// A second-order filter (RBJ cookbook coefficients, transposed direct form II).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    /// A filter from normalized coefficients (`a0` = 1).
    pub fn from_coefficients(b0: f32, b1: f32, b2: f32, a1: f32, a2: f32) -> Self {
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            ..Self::default()
        }
    }

    /// Sets the response, keeping the filter's state. `freq` is clamped below
    /// Nyquist and `q` to 0.1..=20, so any input gives a stable filter.
    pub fn set(&mut self, kind: FilterKind, freq: f32, q: f32, sample_rate: f32) {
        let freq = freq.clamp(10.0, 0.49 * sample_rate);
        let w0 = 2.0 * PI * freq / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q.clamp(0.1, 20.0));
        let (b0, b1, b2) = match kind {
            FilterKind::LowPass => ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0),
            FilterKind::HighPass => ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0),
            FilterKind::BandPass => (alpha, 0.0, -alpha),
        };
        let a0 = 1.0 + alpha;
        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = -2.0 * cos / a0;
        self.a2 = (1.0 - alpha) / a0;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// Follows a level with separate attack and release times.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Envelope {
    attack: f32,
    release: f32,
    value: f32,
}

fn coef(seconds: f32, sample_rate: f32) -> f32 {
    let samples = seconds * sample_rate;
    if samples > 0.0 {
        (-1.0 / samples).exp()
    } else {
        0.0
    }
}

impl Envelope {
    pub fn set_times(&mut self, attack: f32, release: f32, sample_rate: f32) {
        self.attack = coef(attack, sample_rate);
        self.release = coef(release, sample_rate);
    }

    #[inline]
    pub fn process(&mut self, level: f32) -> f32 {
        let c = if level > self.value {
            self.attack
        } else {
            self.release
        };
        self.value = level + c * (self.value - level);
        self.value
    }

    pub fn reset(&mut self) {
        self.value = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Steady-state gain of `f` for a sine at `freq`.
    fn response(kind: FilterKind, cutoff: f32, freq: f32) -> f32 {
        let sr = 48_000.0;
        let mut f = Biquad::default();
        f.set(kind, cutoff, std::f32::consts::FRAC_1_SQRT_2, sr);
        let mut peak = 0.0f32;
        for i in 0..48_000 {
            let y = f.process((2.0 * PI * freq * i as f32 / sr).sin());
            if i > 24_000 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn filters_pass_and_stop_where_they_should() {
        assert!(response(FilterKind::LowPass, 1_000.0, 100.0) > 0.98);
        assert!(response(FilterKind::LowPass, 1_000.0, 10_000.0) < 0.02);
        assert!(response(FilterKind::HighPass, 1_000.0, 10_000.0) > 0.98);
        assert!(response(FilterKind::HighPass, 1_000.0, 100.0) < 0.02);
        let peak = response(FilterKind::BandPass, 1_000.0, 1_000.0);
        assert!((peak - 1.0).abs() < 0.01, "band peak {peak}");
    }

    #[test]
    fn out_of_range_settings_stay_stable() {
        let mut f = Biquad::default();
        f.set(FilterKind::LowPass, 1e9, 0.0, 48_000.0);
        let mut y = 0.0;
        for i in 0..10_000 {
            y = f.process(if i % 2 == 0 { 1.0 } else { -1.0 });
        }
        assert!(y.is_finite() && y.abs() < 10.0);
    }

    #[test]
    fn envelope_attacks_fast_and_releases_slow() {
        let mut e = Envelope::default();
        e.set_times(0.001, 0.1, 48_000.0);
        for _ in 0..480 {
            e.process(1.0);
        }
        assert!(e.process(1.0) > 0.99);
        for _ in 0..480 {
            e.process(0.0);
        }
        assert!(e.process(0.0) > 0.85, "released too fast");
        assert!((db_to_gain(gain_to_db(0.5)) - 0.5).abs() < 1e-6);
        assert_eq!(gain_to_db(0.0), -120.0);
    }
}
