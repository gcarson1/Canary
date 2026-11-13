//! Automation: breakpoint envelopes that move a track's gain, pan or any device
//! parameter over the song, in ticks.

use daw_core::AutoPoint;
use serde::{Deserialize, Serialize};

use crate::{Project, MAX_TICKS};

/// What an automation lane moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Target {
    /// Track gain, linear 0..=2.
    Gain,
    /// Track pan, -1..=1.
    Pan,
    /// Parameter `id` of the device with uid `device`, in the parameter's own units.
    Param { device: u64, id: u32 },
}

impl Target {
    /// The range a value must lie in, where the project knows it.
    pub fn range(&self) -> Option<(f64, f64)> {
        match self {
            Target::Gain => Some((0.0, 2.0)),
            Target::Pan => Some((-1.0, 1.0)),
            Target::Param { .. } => None,
        }
    }
}

/// One breakpoint. `curve` bends the segment to the next point: 0 is a straight
/// line, toward +1 it rises late (slow then fast), toward -1 early.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub tick: u64,
    pub value: f64,
    #[serde(default)]
    pub curve: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lane {
    pub target: Target,
    /// By rising tick, at most one per tick.
    pub points: Vec<Point>,
}

/// True for points sorted by strictly rising tick, in range, with finite values
/// (inside the target's range where it has one) and curves in -1..=1.
pub fn valid_points(target: Target, points: &[Point]) -> bool {
    let range = target.range().unwrap_or((f64::MIN, f64::MAX));
    points.windows(2).all(|w| w[0].tick < w[1].tick)
        && points.iter().all(|p| {
            p.tick <= MAX_TICKS
                && p.value.is_finite()
                && (range.0..=range.1).contains(&p.value)
                && (-1.0..=1.0).contains(&p.curve)
        })
}

impl Project {
    /// `track`'s lane for `target`, if it has points.
    pub fn lane(&self, track: usize, target: Target) -> Option<&Lane> {
        self.tracks
            .get(track)?
            .automation
            .iter()
            .find(|l| l.target == target)
    }

    /// A lane's points at timeline samples, for the engine.
    pub fn envelope(&self, lane: &Lane) -> Vec<AutoPoint> {
        lane.points
            .iter()
            .map(|p| AutoPoint {
                at: self.samples_at(p.tick),
                value: p.value,
                curve: p.curve,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_validate_order_range_and_curve() {
        let p = |tick, value, curve| Point { tick, value, curve };
        assert!(valid_points(
            Target::Gain,
            &[p(0, 0.0, 0.0), p(5, 2.0, 1.0)]
        ));
        assert!(!valid_points(Target::Gain, &[p(0, 2.5, 0.0)]));
        assert!(!valid_points(
            Target::Pan,
            &[p(5, 0.0, 0.0), p(5, 0.1, 0.0)]
        ));
        assert!(!valid_points(Target::Pan, &[p(5, 0.0, 1.5)]));
        let param = Target::Param { device: 1, id: 3 };
        assert!(valid_points(param, &[p(0, 20_000.0, 0.0)]));
        assert!(!valid_points(param, &[p(0, f64::NAN, 0.0)]));
    }
}
