//! Pure signal processing primitives: filters, envelopes, oscillators, delay lines. No OS or UI dependencies.

pub mod effects;
mod filter;
mod waveform;

pub use filter::{db_to_gain, gain_to_db, Biquad, Envelope, FilterKind};
pub use waveform::{MinMax, WaveformPyramid, LEVELS};

/// One-pole smoother that glides a parameter toward its target to avoid zipper noise and clicks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Smoother {
    current: f32,
    target: f32,
    coef: f32,
}

impl Smoother {
    /// Starts settled at `value`. Call `set_time` before use; until then it jumps instantly.
    pub fn new(value: f32) -> Self {
        Self {
            current: value,
            target: value,
            coef: 1.0,
        }
    }

    /// Time constant in seconds: about 63% of the way to the target after `seconds`.
    pub fn set_time(&mut self, seconds: f32, sample_rate: f32) {
        let samples = seconds * sample_rate;
        self.coef = if samples > 0.0 {
            1.0 - (-1.0 / samples).exp()
        } else {
            1.0
        };
    }

    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    pub fn current(&self) -> f32 {
        self.current
    }

    /// Advances one sample and returns the new value.
    #[inline]
    pub fn tick(&mut self) -> f32 {
        let next = self.current + (self.target - self.current) * self.coef;
        // Snap once inaudibly close, or once the step is below f32 precision and the
        // value would stall short of the target. Also keeps it out of denormals.
        self.current = if next == self.current || (self.target - next).abs() < 1e-6 {
            self.target
        } else {
            next
        };
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoother_glides_monotonically_and_settles() {
        let mut s = Smoother::new(0.0);
        s.set_time(0.01, 48_000.0);
        s.set_target(1.0);

        let first = s.tick();
        assert!(first > 0.0 && first < 0.01, "jumped to {first}");
        let mut last = first;
        for _ in 0..48_000 {
            let v = s.tick();
            assert!(v >= last);
            last = v;
        }
        assert_eq!(last, 1.0);
    }

    #[test]
    fn zero_time_jumps_immediately() {
        let mut s = Smoother::new(0.0);
        s.set_time(0.0, 48_000.0);
        s.set_target(0.7);
        assert_eq!(s.tick(), 0.7);
    }
}
