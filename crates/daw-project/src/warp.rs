//! Warping: a clip whose audio follows the tempo map. Its source's beats are
//! pinned to ticks (by warp markers, and past the last one by the source's own
//! tempo), so a tempo change re-renders the audio to land on the same beats.

use serde::{Deserialize, Serialize};

use crate::{Project, PPQ};

/// How a warped clip's source maps onto the song's beats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Warp {
    /// The tick the source's first frame sits on.
    pub tick: u64,
    /// The source's tempo, for the audio past the last marker.
    pub bpm: f64,
    /// (source frame, beats after `tick`), rising in both; (0, 0) is implied.
    #[serde(default)]
    pub markers: Vec<(u64, f64)>,
}

/// One stretch of a warped render: source frames `from..to` become `frames`
/// frames on the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    pub from: u64,
    pub to: u64,
    pub frames: u64,
}

impl Warp {
    /// True for a usable tempo and markers rising in frame and beat.
    pub fn is_valid(&self) -> bool {
        let mut last = (0u64, 0.0f64);
        crate::bpm_ok(self.bpm)
            && self.markers.iter().all(|&(f, b)| {
                let ok = f > last.0 && b.is_finite() && b > last.1;
                last = (f, b);
                ok
            })
    }

    /// The breakpoints (source frame, beat), from the start through the end
    /// of `source_frames` (extrapolated at the source tempo).
    fn points(&self, source_frames: u64, rate: u32) -> Vec<(u64, f64)> {
        let per_frame = self.bpm / 60.0 / f64::from(rate.max(1));
        let mut points = vec![(0, 0.0)];
        points.extend(self.markers.iter().filter(|m| m.0 < source_frames));
        let &(f, b) = points.last().expect("starts with one");
        if source_frames > f {
            points.push((source_frames, b + (source_frames - f) as f64 * per_frame));
        }
        points
    }

    /// The source frame at `beats` after `tick` (beats past the end clamp).
    pub fn source_at(&self, beats: f64, source_frames: u64, rate: u32) -> u64 {
        let points = self.points(source_frames, rate);
        let seg = points.windows(2).find(|w| beats < w[1].1);
        match seg {
            Some(w) => {
                let t = ((beats - w[0].1) / (w[1].1 - w[0].1)).clamp(0.0, 1.0);
                w[0].0 + ((w[1].0 - w[0].0) as f64 * t).round() as u64
            }
            None => source_frames,
        }
    }

    /// Adds a marker pinning source frame `frame` to `beats`, replacing one
    /// that would cross it. Returns false (changing nothing) if the result
    /// would not be valid.
    pub fn pin(&mut self, frame: u64, beats: f64) -> bool {
        let mut markers: Vec<(u64, f64)> = self
            .markers
            .iter()
            .copied()
            .filter(|&(f, b)| (f < frame) == (b < beats) && f != frame)
            .collect();
        markers.push((frame, beats));
        markers.sort_by_key(|m| m.0);
        let next = Self {
            markers,
            ..self.clone()
        };
        if next.is_valid() {
            *self = next;
        }
        self.markers.contains(&(frame, beats))
    }
}

impl Project {
    /// How to render `source_frames` of a warped source onto this project's
    /// tempo map: pieces in order, split at markers and at tempo changes, each
    /// as long on the timeline as its beats last there.
    pub fn warp_pieces(&self, warp: &Warp, source_frames: u64) -> Vec<Piece> {
        let ticks = |beats: f64| warp.tick + (beats * PPQ as f64).round() as u64;
        let points = warp.points(source_frames, self.sample_rate);
        let changes: Vec<u64> = self.tempos.iter().map(|c| c.tick).collect();
        let mut pieces = Vec::new();
        for w in points.windows(2) {
            let ((f0, b0), (f1, b1)) = (w[0], w[1]);
            let (t0, t1) = (ticks(b0), ticks(b1));
            // Cut where the tempo changes, so each piece has one ratio.
            let cuts = std::iter::once(t0)
                .chain(changes.iter().copied().filter(|&c| c > t0 && c < t1))
                .chain(std::iter::once(t1));
            let cuts: Vec<u64> = cuts.collect();
            let frame_at = |t: u64| {
                let span = (t1 - t0).max(1) as f64;
                f0 + ((f1 - f0) as f64 * (t - t0) as f64 / span).round() as u64
            };
            for c in cuts.windows(2) {
                let (from, to) = (frame_at(c[0]), frame_at(c[1]));
                let frames = self.samples_at(c[1]) - self.samples_at(c[0]);
                if to > from && frames > 0 {
                    pieces.push(Piece { from, to, frames });
                }
            }
        }
        pieces
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TempoChange;

    #[test]
    fn a_warped_clip_follows_tempo_changes_and_markers() {
        // 120 BPM at 48 kHz: a beat is 24 000 samples.
        let mut p = Project::default();
        let w = Warp {
            tick: 0,
            bpm: 120.0,
            markers: Vec::new(),
        };
        // At its own tempo, the source plays as it is.
        let same = p.warp_pieces(&w, 96_000);
        assert_eq!(
            same,
            [Piece {
                from: 0,
                to: 96_000,
                frames: 96_000
            }]
        );
        // From beat 2 the song halves its tempo: the second half lasts twice as long.
        p.tempos = vec![TempoChange {
            tick: 2 * PPQ,
            bpm: 60.0,
        }];
        let pieces = p.warp_pieces(&w, 96_000);
        assert_eq!(pieces.len(), 2);
        assert_eq!((pieces[0].to, pieces[0].frames), (48_000, 48_000));
        assert_eq!((pieces[1].from, pieces[1].frames), (48_000, 96_000));
        // A marker: the source's frame 30 000 lands on beat 1, so the first part
        // squeezes and the rest stretches to beat 2.
        let mut m = w.clone();
        assert!(m.pin(30_000, 1.0));
        p.tempos.clear();
        let pieces = p.warp_pieces(&m, 54_000);
        assert_eq!(
            pieces.iter().map(|x| x.frames).collect::<Vec<_>>(),
            [24_000, 24_000]
        );
        assert_eq!(m.source_at(1.0, 54_000, 48_000), 30_000);
        assert_eq!(m.source_at(0.5, 54_000, 48_000), 15_000);
        // A marker that would cross another replaces it.
        assert!(m.pin(20_000, 1.5));
        assert_eq!(m.markers, [(20_000, 1.5)]);
        assert!(!m.pin(0, 0.5), "frame 0 is the start");
        assert!(!Warp { bpm: 0.0, ..w }.is_valid());
    }
}
