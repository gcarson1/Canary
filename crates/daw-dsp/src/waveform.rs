use std::sync::Arc;

use daw_core::SampleData;

/// Frames per bin at each pyramid level, finest first. Each level is 4x the last.
pub const LEVELS: [usize; 4] = [64, 256, 1024, 4096];

/// First bytes of a `.pek` cache file (format version 1).
const PEK_MAGIC: [u8; 4] = *b"PEK1";

/// Lowest and highest sample in a stretch of audio. `EMPTY` (min > max) means
/// the stretch held no audio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MinMax {
    pub min: f32,
    pub max: f32,
}

impl MinMax {
    pub const EMPTY: MinMax = MinMax {
        min: f32::INFINITY,
        max: f32::NEG_INFINITY,
    };

    pub fn is_empty(&self) -> bool {
        self.min > self.max
    }

    fn merge(self, other: MinMax) -> MinMax {
        MinMax {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }

    fn of(samples: &[f32]) -> MinMax {
        samples.iter().fold(MinMax::EMPTY, |m, &s| MinMax {
            min: m.min.min(s),
            max: m.max.max(s),
        })
    }
}

/// Mip-mapped min/max summary of a piece of audio, for drawing its waveform at
/// any zoom in time proportional to the pixels drawn, not the frames covered.
///
/// Built once per `SampleData` on a background thread. Clip edits never touch the
/// audio, so they never invalidate it: views are asked for in source frames.
pub struct WaveformPyramid {
    audio: Arc<SampleData>,
    /// `levels[l][channel][bin]`, with bins of `LEVELS[l]` frames.
    levels: Vec<Vec<Box<[MinMax]>>>,
}

impl WaveformPyramid {
    pub fn new(audio: Arc<SampleData>) -> Self {
        let finest: Vec<Box<[MinMax]>> = (0..audio.num_channels())
            .map(|ch| {
                audio
                    .channel(ch)
                    .chunks(LEVELS[0])
                    .map(MinMax::of)
                    .collect()
            })
            .collect();
        let mut levels = vec![finest];
        for _ in 1..LEVELS.len() {
            let coarser = levels
                .last()
                .unwrap()
                .iter()
                .map(|bins| {
                    bins.chunks(4)
                        .map(|c| c.iter().fold(MinMax::EMPTY, |m, &b| m.merge(b)))
                        .collect()
                })
                .collect();
            levels.push(coarser);
        }
        Self { audio, levels }
    }

    pub fn audio(&self) -> &Arc<SampleData> {
        &self.audio
    }

    /// The bins as a `.pek` cache file: magic, channel and frame counts, then every
    /// level's bins as little-endian `f32` min/max pairs.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::from(PEK_MAGIC);
        bytes.extend((self.audio.num_channels() as u32).to_le_bytes());
        bytes.extend((self.audio.frames() as u64).to_le_bytes());
        for bin in self.levels.iter().flatten().flat_map(|bins| bins.iter()) {
            bytes.extend(bin.min.to_le_bytes());
            bytes.extend(bin.max.to_le_bytes());
        }
        bytes
    }

    /// Rebuilds a pyramid of `audio` from `to_bytes` output. `None` if the bytes are
    /// not a `.pek` of audio with this channel and frame count, so a stale or
    /// corrupt cache is rebuilt rather than trusted.
    pub fn from_bytes(audio: Arc<SampleData>, bytes: &[u8]) -> Option<Self> {
        let (channels, frames) = (audio.num_channels(), audio.frames());
        let header = bytes.get(..16)?;
        if header[..4] != PEK_MAGIC
            || u32::from_le_bytes(header[4..8].try_into().ok()?) as usize != channels
            || u64::from_le_bytes(header[8..16].try_into().ok()?) != frames as u64
        {
            return None;
        }
        let mut pairs = bytes[16..].chunks_exact(8).map(|p| MinMax {
            min: f32::from_le_bytes(p[..4].try_into().unwrap()),
            max: f32::from_le_bytes(p[4..].try_into().unwrap()),
        });
        let mut bins_at = frames;
        let mut levels = Vec::with_capacity(LEVELS.len());
        for (l, &bin) in LEVELS.iter().enumerate() {
            // Level 0 bins frames; each coarser level bins 4 of the level below.
            bins_at = if l == 0 {
                frames.div_ceil(bin)
            } else {
                bins_at.div_ceil(4)
            };
            let level = (0..channels)
                .map(|_| pairs.by_ref().take(bins_at).collect::<Box<[MinMax]>>())
                .collect::<Vec<_>>();
            if level.iter().any(|bins| bins.len() != bins_at) {
                return None;
            }
            levels.push(level);
        }
        if pairs.next().is_some() || (bytes.len() - 16) % 8 != 0 {
            return None;
        }
        Some(Self { audio, levels })
    }

    /// Fills `out[i]` with the min/max of source frames
    /// `start + i * frames_per_pixel .. start + (i + 1) * frames_per_pixel` of
    /// `channel`, or `MinMax::EMPTY` past the end of the audio. For a trimmed clip,
    /// `start` is in source frames, so add `Clip::offset_samples`.
    ///
    /// Reads the coarsest level whose bins are no wider than a pixel, so a pixel can
    /// take in up to one extra bin at each edge. Panics if `channel` is out of range.
    pub fn view(&self, channel: usize, start: f64, frames_per_pixel: f64, out: &mut [MinMax]) {
        let samples = self.audio.channel(channel);
        if !start.is_finite() {
            out.fill(MinMax::EMPTY);
            return;
        }
        let frames = samples.len();
        let spp = frames_per_pixel.max(f64::MIN_POSITIVE);
        let level = LEVELS.iter().rposition(|&bin| bin as f64 <= spp);
        for (i, px) in out.iter_mut().enumerate() {
            let a = (start + i as f64 * spp).floor().max(0.0) as usize;
            let b = ((start + (i + 1) as f64 * spp).floor().max(0.0) as usize)
                .max(a.saturating_add(1))
                .min(frames);
            *px = if a >= b {
                MinMax::EMPTY
            } else if let Some(l) = level {
                let bin = LEVELS[l];
                let bins = &self.levels[l][channel];
                bins[a / bin..b.div_ceil(bin)]
                    .iter()
                    .fold(MinMax::EMPTY, |m, &x| m.merge(x))
            } else {
                MinMax::of(&samples[a..b])
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brute-force min/max straight from the samples.
    fn exact(samples: &[f32], a: usize, b: usize) -> MinMax {
        MinMax::of(&samples[a..b.min(samples.len())])
    }

    fn pyramid(frames: usize) -> WaveformPyramid {
        // deterministic noise-ish signal with a lone spike at frame 5000
        let mut x: Vec<f32> = (0..frames)
            .map(|i| ((i * 7919) % 1000) as f32 / 1000.0 - 0.5)
            .collect();
        x[5000] = 0.9;
        WaveformPyramid::new(Arc::new(SampleData::new(48_000, vec![x]).unwrap()))
    }

    #[test]
    fn every_zoom_matches_the_samples_on_bin_aligned_pixels() {
        let p = pyramid(100_003);
        let samples = p.audio().channel(0).to_vec();
        for spp in [1.0, 10.0, 64.0, 256.0, 1000.0, 1024.0, 4096.0, 16384.0] {
            let mut out = vec![MinMax::EMPTY; 20];
            p.view(0, 0.0, spp, &mut out);
            for (i, px) in out.iter().enumerate() {
                let a = (i as f64 * spp) as usize;
                let b = ((i + 1) as f64 * spp) as usize;
                if a >= samples.len() {
                    assert!(px.is_empty());
                    continue;
                }
                let bin = LEVELS
                    .iter()
                    .rev()
                    .find(|&&b| b as f64 <= spp)
                    .copied()
                    .unwrap_or(1);
                // pixel may take in at most the bins straddling its edges
                let want = exact(&samples, a, b);
                let wide = exact(&samples, a / bin * bin, b.div_ceil(bin) * bin);
                assert!(px.min <= want.min && px.max >= want.max, "spp {spp} px {i}");
                assert!(px.min >= wide.min && px.max <= wide.max, "spp {spp} px {i}");
            }
        }
    }

    #[test]
    fn spike_survives_whole_file_zoom_and_tail_is_empty() {
        let p = pyramid(100_003);
        let mut out = vec![MinMax::EMPTY; 10];
        p.view(0, 0.0, 20_000.0, &mut out);
        assert_eq!(out[0].max, 0.9);
        assert!(!out[5].is_empty(), "partial last pixel still has audio");
        assert!(out[6..].iter().all(MinMax::is_empty));
        p.view(0, -50.0, 10.0, &mut out[..2]);
        assert!(!out[0].is_empty(), "negative start clamps to frame 0");
        for (start, spp) in [
            (0.0, 1e20),
            (1e20, 10.0),
            (0.0, f64::INFINITY),
            (f64::NAN, 10.0),
        ] {
            p.view(0, start, spp, &mut out[..2]);
            assert!(out[1].is_empty(), "start {start} spp {spp}");
        }
    }

    #[test]
    fn pek_bytes_round_trip_and_reject_mismatches() {
        let p = pyramid(10_001);
        let bytes = p.to_bytes();
        let back = WaveformPyramid::from_bytes(p.audio().clone(), &bytes).unwrap();
        assert_eq!(back.levels, p.levels);

        let other = Arc::new(SampleData::new(48_000, vec![vec![0.0; 10_000]]).unwrap());
        assert!(
            WaveformPyramid::from_bytes(other, &bytes).is_none(),
            "frame count"
        );
        let audio = p.audio().clone();
        assert!(WaveformPyramid::from_bytes(audio.clone(), &bytes[..bytes.len() - 8]).is_none());
        assert!(WaveformPyramid::from_bytes(audio.clone(), &bytes[..bytes.len() - 3]).is_none());
        let mut extra = bytes.clone();
        extra.extend([0; 8]);
        assert!(WaveformPyramid::from_bytes(audio.clone(), &extra).is_none());
        let mut magic = bytes;
        magic[0] = b'X';
        assert!(WaveformPyramid::from_bytes(audio, &magic).is_none());
    }
}
