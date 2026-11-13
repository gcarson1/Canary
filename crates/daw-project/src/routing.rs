//! Mixer routing: buses, track outputs, sends and sidechains, by track id.

use serde::{Deserialize, Serialize};

use crate::Project;

/// A post-fader send from a track to a bus.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Send {
    /// The bus's track id.
    pub to: u64,
    /// Linear, 0..=2.
    pub level: f32,
}

impl Project {
    pub fn track_index(&self, id: u64) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == id)
    }

    /// Where track `i`'s audio goes: a bus's index, or `None` for the master.
    /// An output to a missing or non-bus track goes to the master.
    pub fn output_of(&self, i: usize) -> Option<usize> {
        let to = self.tracks.get(i)?.output?;
        self.track_index(to)
            .filter(|&j| j != i && self.tracks[j].bus)
    }

    /// Track `i`'s sends that reach a bus: (bus index, level).
    pub fn sends_of(&self, i: usize) -> Vec<(usize, f32)> {
        let Some(t) = self.tracks.get(i) else {
            return Vec::new();
        };
        t.sends
            .iter()
            .filter_map(|s| {
                let j = self
                    .track_index(s.to)
                    .filter(|&j| j != i && self.tracks[j].bus)?;
                Some((j, s.level))
            })
            .collect()
    }

    /// The track a device's sidechain listens to, if it exists and is not the
    /// device's own track.
    pub fn sidechain_of(&self, track: usize, device: usize) -> Option<usize> {
        let id = self.tracks.get(track)?.devices.get(device)?.sidechain?;
        self.track_index(id).filter(|&j| j != track)
    }

    /// Every track index, each after all the tracks that feed it (by output,
    /// send or sidechain), or `None` if the routing has a cycle.
    pub fn route_order(&self) -> Option<Vec<usize>> {
        let n = self.tracks.len();
        let mut feeds: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut waiting = vec![0usize; n];
        for i in 0..n {
            let targets = self
                .output_of(i)
                .into_iter()
                .chain(self.sends_of(i).into_iter().map(|s| s.0));
            for j in targets {
                feeds[i].push(j);
                waiting[j] += 1;
            }
            for d in 0..self.tracks[i].devices.len() {
                if let Some(j) = self.sidechain_of(i, d) {
                    feeds[j].push(i);
                    waiting[i] += 1;
                }
            }
        }
        let mut order: Vec<usize> = (0..n).filter(|&i| waiting[i] == 0).collect();
        let mut k = 0;
        while k < order.len() {
            for &j in &feeds[order[k]] {
                waiting[j] -= 1;
                if waiting[j] == 0 {
                    order.push(j);
                }
            }
            k += 1;
        }
        (order.len() == n).then_some(order)
    }

    /// A track id not used in the project.
    pub fn new_track_id(&self) -> u64 {
        self.tracks
            .iter()
            .map(|t| t.id.saturating_add(1))
            .max()
            .unwrap_or(1)
            .max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, History, Track};

    fn track(id: u64, bus: bool) -> Track {
        Track {
            id,
            bus,
            ..Track::new(format!("T{id}"))
        }
    }

    #[test]
    fn order_puts_sources_before_buses_and_finds_cycles() {
        let mut p = Project {
            tracks: vec![track(1, true), track(2, false), track(3, true)],
            ..Project::default()
        };
        p.tracks[1].output = Some(1);
        p.tracks[1].sends.push(Send { to: 3, level: 0.5 });
        p.tracks[2].output = Some(1); // bus 3 into bus 1
        let order = p.route_order().unwrap();
        let pos = |i| order.iter().position(|&x| x == i).unwrap();
        assert!(pos(1) < pos(2) && pos(2) < pos(0));
        assert_eq!(p.output_of(1), Some(0));
        assert_eq!(p.sends_of(1), [(2, 0.5)]);
        // A send to a track that is not a bus, or to itself, goes nowhere.
        p.tracks[1].sends.push(Send { to: 2, level: 1.0 });
        assert_eq!(p.sends_of(1).len(), 1);
        p.tracks[0].output = Some(3);
        assert_eq!(p.route_order(), None, "1 -> 3 -> 1");
    }

    #[test]
    fn routing_edits_reject_cycles_and_new_tracks_get_ids() {
        let mut h = History::default();
        for i in 0..3 {
            let mut t = Track::new(format!("T{i}"));
            t.bus = i > 0;
            h.apply(Action::InsertTrack { index: i, track: t }).unwrap();
        }
        let ids: Vec<u64> = h.project().tracks.iter().map(|t| t.id).collect();
        assert_eq!(ids, [1, 2, 3], "fresh ids");
        let route = |track, output: Option<u64>| Action::SetRouting {
            track,
            output,
            sends: Vec::new(),
        };
        h.apply(route(1, Some(3))).unwrap();
        assert_eq!(
            h.apply(route(2, Some(2))),
            Err(crate::ActionError::BadRoute),
            "bus 3 -> bus 2 -> bus 3"
        );
        h.apply(Action::SetRouting {
            track: 0,
            output: Some(2),
            sends: vec![Send { to: 3, level: 0.7 }],
        })
        .unwrap();
        assert!(h.project().route_order().is_some());
        assert!(h.undo());
        assert_eq!(h.project().tracks[0].output, None);
    }
}
