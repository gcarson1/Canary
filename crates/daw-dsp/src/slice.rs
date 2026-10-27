//! Where to cut a sample into slices: at transients, on an even grid, or at
//! markers the user placed. Each function returns slice start frames, sorted,
//! starting at 0; a slice ends where the next one starts.

/// Starts at transients: 10 ms hops whose energy jumps by more than the threshold
/// over the hop before. `sensitivity` 0..=1 lowers the jump needed from 24 dB to
/// 3 dB. Hops quieter than -50 dBFS never start a slice; slices are at least 50 ms.
pub fn transients(channel: &[f32], sample_rate: f32, sensitivity: f32) -> Vec<usize> {
    let hop = ((sample_rate / 100.0) as usize).max(1);
    let min_gap = (sample_rate * 0.05) as usize;
    let jump = 24.0 - 21.0 * sensitivity.clamp(0.0, 1.0);
    let energy: Vec<f32> = channel
        .chunks(hop)
        .map(|c| 10.0 * (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32 + 1e-10).log10())
        .collect();
    let mut starts = vec![0];
    for i in 1..energy.len() {
        let at = i * hop;
        if energy[i] > -50.0
            && energy[i] - energy[i - 1] > jump
            && at - starts.last().copied().unwrap_or(0) >= min_gap
        {
            starts.push(at);
        }
    }
    starts
}

/// `count` equal slices (at least one).
pub fn grid(frames: usize, count: usize) -> Vec<usize> {
    let count = count.max(1);
    let mut starts: Vec<usize> = (0..count).map(|i| i * frames / count).collect();
    starts.dedup();
    starts
}

/// Starts at markers given as fractions of the length; out-of-range and repeated
/// markers are dropped.
pub fn markers(frames: usize, at: &[f64]) -> Vec<usize> {
    let mut starts: Vec<usize> = std::iter::once(0)
        .chain(
            at.iter()
                .filter(|f| f.is_finite() && **f > 0.0 && **f < 1.0)
                .map(|f| (f * frames as f64) as usize),
        )
        .collect();
    starts.sort_unstable();
    starts.dedup();
    starts
}

/// Moves each start but the first back to the nearest zero crossing within
/// `window` frames, so slices begin without a click.
pub fn snap_to_zero(channel: &[f32], starts: &mut [usize], window: usize) {
    for s in starts.iter_mut().skip(1) {
        let from = s.saturating_sub(window).max(1);
        if let Some(z) = (from..=*s)
            .rev()
            .find(|&i| i < channel.len() && (channel[i - 1] <= 0.0) != (channel[i] <= 0.0))
        {
            *s = z;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transients_find_each_hit_and_respect_sensitivity() {
        let sr = 48_000.0;
        // Four decaying clicks, 250 ms apart, over silence.
        let mut x = vec![0.0f32; 48_000];
        for hit in 0..4 {
            for i in 0..2_000 {
                x[hit * 12_000 + 4_800 + i] =
                    (-(i as f32) / 300.0).exp() * if i % 2 == 0 { 1.0 } else { -1.0 };
            }
        }
        let starts = transients(&x, sr, 0.5);
        assert_eq!(starts.len(), 5, "0 plus four hits: {starts:?}");
        for (k, s) in starts[1..].iter().enumerate() {
            let hit = k * 12_000 + 4_800;
            assert!(s.abs_diff(hit) <= 480, "slice {s} vs hit {hit}");
        }
        assert_eq!(
            transients(&vec![0.0; 1000], sr, 1.0),
            [0],
            "silence: one slice"
        );
        assert!(transients(&[], sr, 1.0) == [0]);
    }

    #[test]
    fn grid_and_markers_cover_the_sample() {
        assert_eq!(grid(100, 4), [0, 25, 50, 75]);
        assert_eq!(grid(2, 8), [0, 1]);
        assert_eq!(grid(100, 0), [0]);
        assert_eq!(
            markers(100, &[0.5, 0.25, 0.5, 2.0, f64::NAN, 0.0]),
            [0, 25, 50]
        );
    }

    #[test]
    fn starts_snap_back_to_zero_crossings() {
        let x = [0.5, 0.5, -0.5, -0.5, -0.5, -0.5];
        let mut starts = [0, 4];
        snap_to_zero(&x, &mut starts, 3);
        assert_eq!(starts, [0, 2]);
        let mut far = [0, 5];
        snap_to_zero(&x, &mut far, 1);
        assert_eq!(far, [0, 5], "no crossing in reach: unchanged");
    }
}
