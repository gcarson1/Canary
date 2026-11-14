//! A lock-free window onto a signal, for analyzers: the audio thread writes the
//! newest samples, the UI reads them back and asks for their spectrum. A read
//! that races a write may mix old and new samples, which an analyzer can ignore.

use std::f32::consts::PI;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Samples kept, a power of two above `FFT_LEN` so a read has room to race.
const KEEP: usize = 4096;
/// Samples one spectrum looks at: 43 ms at 48 kHz.
const FFT_LEN: usize = 2048;
/// Where the spectrum's points start and end.
pub const SPECTRUM_LOW_HZ: f32 = 20.0;
pub const SPECTRUM_HIGH_HZ: f32 = 20_000.0;

pub struct Scope {
    samples: Box<[AtomicU32]>,
    written: AtomicUsize,
    sample_rate: AtomicU32,
}

impl Scope {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            samples: (0..KEEP).map(|_| AtomicU32::new(0)).collect(),
            written: AtomicUsize::new(0),
            sample_rate: AtomicU32::new(sample_rate.to_bits()),
        }
    }

    pub fn sample_rate(&self) -> f32 {
        f32::from_bits(self.sample_rate.load(Ordering::Relaxed))
    }

    pub fn set_sample_rate(&self, sample_rate: f32) {
        self.sample_rate
            .store(sample_rate.to_bits(), Ordering::Relaxed);
    }

    /// Audio thread, one writer: appends the mid (mono) signal of a block.
    pub fn write(&self, left: &[f32], right: &[f32]) {
        let start = self.written.load(Ordering::Relaxed);
        let mut at = start;
        for (l, r) in left.iter().zip(right) {
            self.samples[at % KEEP].store((0.5 * (l + r)).to_bits(), Ordering::Relaxed);
            at = at.wrapping_add(1);
        }
        self.written.store(at, Ordering::Release);
    }

    /// The level of the latest samples at `points` frequencies spaced evenly in
    /// octaves from 20 Hz to 20 kHz, in dBFS: a full-scale sine reads 0 dB.
    /// Each point takes the loudest FFT bin between it and its neighbours.
    pub fn spectrum(&self, points: usize) -> Vec<f32> {
        let end = self.written.load(Ordering::Acquire);
        let mut re: Vec<f32> = (0..FFT_LEN)
            .map(|i| {
                let at = end.wrapping_sub(FFT_LEN - i) % KEEP;
                let hann = 0.5 - 0.5 * (2.0 * PI * i as f32 / FFT_LEN as f32).cos();
                f32::from_bits(self.samples[at].load(Ordering::Relaxed)) * hann
            })
            .collect();
        let mut im = vec![0.0; FFT_LEN];
        fft(&mut re, &mut im);
        // A Hann-windowed sine of amplitude A peaks at A * N / 4.
        let scale = 4.0 / FFT_LEN as f32;
        let bin_hz = self.sample_rate() / FFT_LEN as f32;
        let ratio = (SPECTRUM_HIGH_HZ / SPECTRUM_LOW_HZ).powf(1.0 / (points.max(2) - 1) as f32);
        (0..points)
            .map(|i| {
                let f = SPECTRUM_LOW_HZ * ratio.powi(i as i32);
                // Points above Nyquist (low sample rates) read the top bin.
                let hi = ((f * ratio.sqrt() / bin_hz).ceil() as usize).min(FFT_LEN / 2);
                let lo = ((f / ratio.sqrt() / bin_hz).floor() as usize).clamp(1, hi);
                let peak = (lo..=hi)
                    .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt())
                    .fold(0.0f32, f32::max);
                crate::gain_to_db(peak * scale)
            })
            .collect()
    }
}

/// In-place radix-2 FFT; `re.len()` must be a power of two.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let (s, c) = (-2.0 * PI / len as f32).sin_cos();
        for start in (0..n).step_by(len) {
            let (mut wr, mut wi) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * wr - im[b] * wi;
                let ti = re[b] * wi + im[b] * wr;
                (re[b], im[b]) = (re[a] - tr, im[a] - ti);
                re[a] += tr;
                im[a] += ti;
                (wr, wi) = (wr * c - wi * s, wr * s + wi * c);
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sine_reads_its_level_at_its_frequency_only() {
        let sr = 48_000.0;
        let scope = Scope::new(sr);
        let x: Vec<f32> = (0..4_800)
            .map(|i| 0.5 * (2.0 * PI * 1_000.0 * i as f32 / sr).sin())
            .collect();
        for block in x.chunks(64) {
            scope.write(block, block);
        }
        let points = 121;
        let s = scope.spectrum(points);
        // Points step 1/40 decade from 20 Hz: 1 kHz is point 68.
        let at_1k = s[68];
        assert!((at_1k + 6.0).abs() < 1.5, "{at_1k} dB at 1 kHz");
        assert!(s[20] < -60.0 && s[110] < -60.0, "{} {}", s[20], s[110]);
        // At 16 kHz the top points lie above Nyquist: no panic.
        Scope::new(16_000.0).spectrum(points);
    }
}
