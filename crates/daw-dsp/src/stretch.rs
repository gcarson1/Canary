//! Offline time-stretch and pitch-shift (WSOLA): longer or shorter audio at the
//! same pitch, or a new pitch at the same length. Runs on a worker thread.

use std::f32::consts::PI;

/// Analysis/synthesis frame, in samples (at 48 kHz, about 43 ms).
const FRAME: usize = 2048;
const HOP: usize = FRAME / 2;
/// How far a frame may slide to line up with the last one.
const TOLERANCE: usize = 512;
/// Correlation is measured on every `DECIMATE`th sample, for speed.
const DECIMATE: usize = 4;

/// Stretches every channel by `ratio` (2.0 = twice as long) without changing
/// pitch. All channels use the same frame placements, so the stereo image holds.
/// `ratio` is clamped to 0.25..=4.
pub fn time_stretch(channels: &[Vec<f32>], ratio: f64) -> Vec<Vec<f32>> {
    stretch_by(channels, ratio.clamp(0.25, 4.0))
}

/// `time_stretch` without the user-facing limits (pitch shifting stretches
/// past them before resampling back).
fn stretch_by(channels: &[Vec<f32>], ratio: f64) -> Vec<Vec<f32>> {
    let ratio = ratio.clamp(0.1, 10.0);
    let len = channels.first().map_or(0, Vec::len);
    let out_len = (len as f64 * ratio).round() as usize;
    if len == 0 || (ratio - 1.0).abs() < 1e-9 {
        return channels.to_vec();
    }
    let mono: Vec<f32> = (0..len)
        .map(|i| channels.iter().map(|c| c[i]).sum::<f32>())
        .collect();
    let window: Vec<f32> = (0..FRAME)
        .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / FRAME as f32).cos())
        .collect();
    let analysis_hop = HOP as f64 / ratio;
    let mut out = vec![vec![0.0f32; out_len + FRAME]; channels.len()];
    let mut norm = vec![0.0f32; out_len + FRAME];
    // Where the previous frame was taken from: the next should continue it.
    let mut prev: Option<usize> = None;
    let mut k = 0usize;
    loop {
        let out_pos = k * HOP;
        if out_pos >= out_len {
            break;
        }
        let nominal = (k as f64 * analysis_hop) as usize;
        let at = match prev {
            None => nominal.min(len.saturating_sub(1)),
            Some(p) => best_offset(&mono, p + HOP, nominal, len),
        };
        for (ch, o) in channels.iter().zip(out.iter_mut()) {
            for (i, w) in window.iter().enumerate() {
                let x = ch.get(at + i).copied().unwrap_or(0.0);
                o[out_pos + i] += x * w;
            }
        }
        for (i, w) in window.iter().enumerate() {
            norm[out_pos + i] += w;
        }
        prev = Some(at);
        k += 1;
    }
    for o in &mut out {
        for (x, n) in o.iter_mut().zip(&norm) {
            if *n > 1e-3 {
                *x /= n;
            }
        }
        o.truncate(out_len);
    }
    out
}

/// The input position near `nominal` whose frame best matches the natural
/// continuation at `target` (cross-correlation on a decimated mono mix).
fn best_offset(mono: &[f32], target: usize, nominal: usize, len: usize) -> usize {
    let lo = nominal.saturating_sub(TOLERANCE);
    let hi = (nominal + TOLERANCE).min(len.saturating_sub(1));
    let sample = |i: usize| mono.get(i).copied().unwrap_or(0.0);
    let (mut best, mut best_score) = (nominal.min(len.saturating_sub(1)), f32::MIN);
    let mut cand = lo;
    while cand <= hi {
        let mut score = 0.0;
        let mut i = 0;
        while i < HOP {
            score += sample(target + i) * sample(cand + i);
            i += DECIMATE;
        }
        if score > best_score {
            best_score = score;
            best = cand;
        }
        cand += 2;
    }
    best
}

/// Resamples by reading every `step`th position (linear interpolation), so
/// `step` 2.0 halves the length and doubles the pitch.
fn resample(channel: &[f32], step: f64, out_len: usize) -> Vec<f32> {
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * step;
            let j = pos as usize;
            let frac = (pos - j as f64) as f32;
            let a = channel.get(j).copied().unwrap_or(0.0);
            let b = channel.get(j + 1).copied().unwrap_or(0.0);
            a + (b - a) * frac
        })
        .collect()
}

/// Stretches by `ratio` and shifts pitch by `semitones` (±24) in one pass: the
/// result is `ratio` times as long, `semitones` higher or lower.
pub fn stretch_and_pitch(channels: &[Vec<f32>], ratio: f64, semitones: f64) -> Vec<Vec<f32>> {
    let shift = 2f64.powf(semitones.clamp(-24.0, 24.0) / 12.0);
    let len = channels.first().map_or(0, Vec::len);
    let out_len = (len as f64 * ratio.clamp(0.25, 4.0)).round() as usize;
    if (shift - 1.0).abs() < 1e-9 {
        return time_stretch(channels, ratio);
    }
    // Stretch further by the shift, then read it faster by the same factor.
    let long = stretch_by(channels, ratio.clamp(0.25, 4.0) * shift);
    let step = long
        .first()
        .map_or(1.0, |l| l.len() as f64 / out_len.max(1) as f64);
    long.iter().map(|c| resample(c, step, out_len)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * PI * freq * i as f32 / 48_000.0).sin() * 0.5)
            .collect()
    }

    /// Rising zero crossings per second, a frequency estimate.
    fn crossings(x: &[f32]) -> f32 {
        let n = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        n as f32 / (x.len() as f32 / 48_000.0)
    }

    #[test]
    fn stretching_changes_length_not_pitch() {
        let input = vec![sine(440.0, 48_000)];
        for ratio in [0.5, 1.5, 2.0] {
            let out = time_stretch(&input, ratio);
            assert_eq!(out[0].len(), (48_000.0 * ratio) as usize);
            let mid = &out[0][4_096..out[0].len() - 4_096];
            let f = crossings(mid);
            assert!((f - 440.0).abs() < 8.0, "ratio {ratio}: {f} Hz");
            assert!(mid.iter().all(|s| s.abs() < 0.6), "no overlap blow-up");
        }
    }

    #[test]
    fn pitch_shift_keeps_length_and_moves_frequency() {
        let input = vec![sine(220.0, 48_000), sine(220.0, 48_000)];
        let out = stretch_and_pitch(&input, 1.0, 12.0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), 48_000);
        let f = crossings(&out[0][4_096..44_000]);
        assert!((f - 440.0).abs() < 10.0, "{f} Hz");
        assert_eq!(stretch_and_pitch(&input, 1.0, 0.0), input);
    }
}
