//! The tempo map and meter (time signature) map: how ticks become samples, and
//! where bars and beats fall.

use serde::{Deserialize, Serialize};

use crate::{Project, MAX_TICKS, PPQ};

/// From `tick` on, the tempo is `bpm` (until the next change).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TempoChange {
    pub tick: u64,
    pub bpm: f64,
}

/// From bar `bar` (0-based) on, bars hold `numerator` beats of 1/`denominator` notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeterChange {
    pub bar: u32,
    pub numerator: u8,
    pub denominator: u8,
}

impl MeterChange {
    /// Ticks in one beat of this meter.
    pub fn beat_ticks(&self) -> u64 {
        4 * PPQ / u64::from(self.denominator)
    }

    pub fn bar_ticks(&self) -> u64 {
        u64::from(self.numerator) * self.beat_ticks()
    }

    /// 1..=32 beats of a power-of-two note from 1 to 32.
    pub fn is_valid(&self) -> bool {
        (1..=32).contains(&self.numerator) && [1, 2, 4, 8, 16, 32].contains(&self.denominator)
    }
}

/// True for a list sorted by strictly rising tick, every tick after 0 and in range,
/// every tempo finite and positive.
pub fn valid_tempos(changes: &[TempoChange]) -> bool {
    changes.windows(2).all(|w| w[0].tick < w[1].tick)
        && changes
            .iter()
            .all(|c| c.tick > 0 && c.tick <= MAX_TICKS && c.bpm.is_finite() && c.bpm > 0.0)
}

/// True for a list starting at bar 0, strictly rising, every meter valid.
pub fn valid_meters(changes: &[MeterChange]) -> bool {
    changes.first().is_none_or(|c| c.bar == 0)
        && changes.windows(2).all(|w| w[0].bar < w[1].bar)
        && changes.iter().all(MeterChange::is_valid)
}

/// Where a tick falls in bars: 1-based bar and beat, and the ticks into the beat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarBeat {
    pub bar: u64,
    pub beat: u64,
    pub ticks: u64,
    /// The meter in force there.
    pub meter: MeterChange,
}

const FOUR_FOUR: MeterChange = MeterChange {
    bar: 0,
    numerator: 4,
    denominator: 4,
};

impl Project {
    /// Tempo segments: (start tick, bpm), the first at tick 0 at `self.bpm`.
    fn segments(&self) -> impl Iterator<Item = (u64, f64)> + '_ {
        std::iter::once((0, self.bpm)).chain(self.tempos.iter().map(|c| (c.tick, c.bpm)))
    }

    fn samples_per_tick(&self, bpm: f64) -> f64 {
        60.0 * f64::from(self.sample_rate) / (bpm * PPQ as f64)
    }

    /// Timeline samples at tick `ticks`, through the tempo map.
    pub fn samples_at(&self, ticks: u64) -> u64 {
        let mut samples = 0.0;
        let mut seg = self.segments().peekable();
        while let Some((start, bpm)) = seg.next() {
            let end = seg.peek().map_or(u64::MAX, |n| n.0).min(ticks);
            if end <= start {
                break;
            }
            samples += (end - start) as f64 * self.samples_per_tick(bpm);
        }
        samples.round() as u64
    }

    /// The tick at timeline sample `sample` (rounded down).
    pub fn ticks_at(&self, sample: u64) -> u64 {
        let target = sample as f64;
        let mut at = 0.0;
        let mut seg = self.segments().peekable();
        while let Some((start, bpm)) = seg.next() {
            let per = self.samples_per_tick(bpm);
            match seg.peek() {
                Some(&(next, _)) if at + (next - start) as f64 * per <= target => {
                    at += (next - start) as f64 * per;
                }
                _ => return start + ((target - at) / per + 1e-6).floor() as u64,
            }
        }
        0
    }

    /// The tempo at `tick`.
    pub fn bpm_at(&self, tick: u64) -> f64 {
        self.segments()
            .take_while(|s| s.0 <= tick)
            .last()
            .map_or(self.bpm, |s| s.1)
    }

    /// Meters in force, starting at bar 0 (4/4 when none is set).
    pub fn meters(&self) -> impl Iterator<Item = MeterChange> + '_ {
        let first = self.meters.first().copied().unwrap_or(FOUR_FOUR);
        std::iter::once(first).chain(self.meters.iter().skip(1).copied())
    }

    /// The tick where 0-based bar `bar` starts.
    pub fn bar_tick(&self, bar: u64) -> u64 {
        let (mut tick, mut from) = (0u64, 0u64);
        let mut meters = self.meters().peekable();
        while let Some(m) = meters.next() {
            let until = meters
                .peek()
                .map_or(u64::MAX, |n| u64::from(n.bar))
                .min(bar);
            if until <= from {
                break;
            }
            tick = tick.saturating_add((until - from).saturating_mul(m.bar_ticks()));
            from = until;
        }
        tick
    }

    /// Every beat before tick `end`, with `true` on each bar's first beat.
    pub fn beats(&self, end: u64) -> Vec<(u64, bool)> {
        let mut out = Vec::new();
        let mut bar = 0u64;
        loop {
            let start = self.bar_tick(bar);
            if start >= end {
                return out;
            }
            let meter = self.bar_beat(start).meter;
            for b in 0..u64::from(meter.numerator) {
                let tick = start + b * meter.beat_ticks();
                if tick >= end {
                    return out;
                }
                out.push((tick, b == 0));
            }
            bar += 1;
        }
    }

    /// The tick where the last clip or pattern clip ends.
    pub fn song_end(&self) -> u64 {
        self.tracks
            .iter()
            .flat_map(|t| {
                t.clips
                    .iter()
                    .map(|c| self.ticks_at(c.start.saturating_add(c.length)) + 1)
                    .chain(t.pattern_clips.iter().map(|c| c.end()))
            })
            .max()
            .unwrap_or(0)
            .min(MAX_TICKS)
    }

    /// Bar, beat and ticks into the beat at `tick`.
    pub fn bar_beat(&self, tick: u64) -> BarBeat {
        let (mut start, mut bar) = (0u64, 0u64);
        let mut meters = self.meters().peekable();
        while let Some(m) = meters.next() {
            let next = meters.peek().copied();
            let span = next.map(|n| (u64::from(n.bar) - bar) * m.bar_ticks());
            match span {
                Some(span) if tick >= start + span => {
                    start += span;
                    bar = u64::from(next.expect("peeked").bar);
                }
                _ => {
                    let into = tick - start;
                    let (b, rest) = (into / m.bar_ticks(), into % m.bar_ticks());
                    return BarBeat {
                        bar: bar + b + 1,
                        beat: rest / m.beat_ticks() + 1,
                        ticks: rest % m.beat_ticks(),
                        meter: m,
                    };
                }
            }
        }
        unreachable!("meters() always yields one")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Project {
        // 120 BPM at 48 kHz: 25 samples per tick; 60 BPM from bar 2 (tick 3840).
        Project {
            tempos: vec![TempoChange {
                tick: 4 * PPQ,
                bpm: 60.0,
            }],
            ..Project::default()
        }
    }

    #[test]
    fn ticks_and_samples_follow_tempo_changes_both_ways() {
        let p = project();
        assert_eq!(p.samples_at(PPQ), 24_000);
        assert_eq!(p.samples_at(4 * PPQ), 96_000);
        assert_eq!(p.samples_at(5 * PPQ), 96_000 + 48_000);
        for t in [0, 1, PPQ, 4 * PPQ - 1, 4 * PPQ, 4 * PPQ + 1, 9 * PPQ + 7] {
            assert_eq!(p.ticks_at(p.samples_at(t)), t, "tick {t}");
        }
        assert_eq!(p.bpm_at(0), 120.0);
        assert_eq!(p.bpm_at(4 * PPQ), 60.0);
    }

    #[test]
    fn bars_and_beats_follow_meter_changes() {
        let mut p = Project::default();
        assert_eq!(p.bar_tick(2), 8 * PPQ);
        // 4/4 for two bars, then 7/8.
        p.meters = vec![
            FOUR_FOUR,
            MeterChange {
                bar: 2,
                numerator: 7,
                denominator: 8,
            },
        ];
        let eighth = PPQ / 2;
        assert_eq!(p.bar_tick(3), 8 * PPQ + 7 * eighth);
        let at = p.bar_beat(8 * PPQ + 7 * eighth + 3 * eighth + 5);
        assert_eq!((at.bar, at.beat, at.ticks), (4, 4, 5));
        assert_eq!(at.meter.numerator, 7);
        assert_eq!(p.bar_beat(PPQ + 1).beat, 2);
        assert!(valid_meters(&p.meters));
        assert!(!valid_meters(&p.meters[1..]), "must start at bar 0");
        assert!(!valid_tempos(&[TempoChange { tick: 0, bpm: 90.0 }]));
        let beats = p.beats(8 * PPQ + PPQ);
        assert_eq!(beats.len(), 8 + 2);
        assert_eq!(beats[8], (8 * PPQ, true));
        assert_eq!(beats[9], (8 * PPQ + eighth, false));
    }
}
