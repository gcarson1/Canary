//! The metronome's click: a short, decaying sine burst, higher on the downbeat.

use std::f32::consts::TAU;

/// One click voice. A new click restarts it. Allocation-free.
#[derive(Debug, Clone, Copy, Default)]
pub struct Click {
    phase: f32,
    step: f32,
    level: f32,
    decay: f32,
}

/// Peak level of a click, about -9 dBFS: audible over a mix, never clipping it.
const LEVEL: f32 = 0.35;

impl Click {
    /// Starts a click: 1.5 kHz when `accent` (a downbeat), else 1 kHz, fading by
    /// 60 dB over about 40 ms.
    pub fn trigger(&mut self, accent: bool, sample_rate: f32) {
        let freq = if accent { 1_500.0 } else { 1_000.0 };
        self.phase = 0.0;
        self.step = TAU * freq / sample_rate;
        self.level = LEVEL;
        self.decay = (-6.9 / (0.04 * sample_rate)).exp();
    }

    /// The next sample; silence once the click has died away.
    pub fn tick(&mut self) -> f32 {
        if self.level < 1e-4 {
            self.level = 0.0;
            return 0.0;
        }
        let s = self.phase.sin() * self.level;
        self.phase = (self.phase + self.step) % TAU;
        self.level *= self.decay;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_ring_briefly_then_fall_silent() {
        let mut c = Click::default();
        assert_eq!(c.tick(), 0.0);
        c.trigger(true, 48_000.0);
        let burst: Vec<f32> = (0..4_800).map(|_| c.tick()).collect();
        let peak = burst.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.3 && peak <= LEVEL);
        assert!(
            burst[4_000..].iter().all(|s| s.abs() < 1e-3),
            "gone by 100 ms"
        );
        assert!((0..10_000).map(|_| c.tick()).all(|s| s == 0.0));
    }
}
