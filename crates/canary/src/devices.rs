//! Plugin devices: the live plugin behind every project `Device`, kept alive
//! across plan rebuilds so plugin state survives edits.

use std::collections::{HashMap, HashSet};

use daw_core::NodeId;
use daw_graph::{AudioNode, ExecutionPlan, Source, StepId, MAX_NODES};
use daw_host::{Catalog, Format, Hosted};
use daw_project::{Device, PluginFormat, Project, Track};
use daw_ui::{DeviceView, ParamView, PluginChoice};

/// Node ids from here up belong to devices; tracks and the master use those below.
pub const FIRST_DEVICE_NODE: u32 = 2048;
/// Frames a removed plugin's handle outlives its node, so the engine has applied
/// the plan that removes the node and its garbage has been collected first.
// shortcut: a timer, not a signal; the engine does not report when a plan applied.
const RETIRE_FRAMES: u32 = 120;

struct Live {
    hosted: Box<dyn Hosted>,
    id: NodeId,
    /// The node, until a plan inserts it into the engine.
    node: Option<Box<dyn AudioNode>>,
    instrument: bool,
    /// The latency the last plan compensated for; a change needs a new plan.
    latency: u32,
}

pub struct Devices {
    catalog: Catalog,
    pub choices: Vec<PluginChoice>,
    live: HashMap<u64, Live>,
    /// Handles whose nodes a plan is removing: (handle, frames left).
    retired: Vec<(Box<dyn Hosted>, u32)>,
    /// Ids removed from the engine but not yet sent in a plan.
    removing: Vec<NodeId>,
    /// Last state of removed devices, so an undo brings back what was there.
    removed_state: HashMap<u64, Vec<u8>>,
    /// Devices whose plugin failed to load; not retried on every edit.
    failed: HashSet<u64>,
    free: Vec<u32>,
}

fn to_host(f: PluginFormat) -> Format {
    match f {
        PluginFormat::Clap => Format::Clap,
        PluginFormat::AudioUnit => Format::AudioUnit,
        PluginFormat::Vst3 => Format::Vst3,
        PluginFormat::Builtin => Format::Builtin,
    }
}

fn to_project(f: Format) -> PluginFormat {
    match f {
        Format::Clap => PluginFormat::Clap,
        Format::AudioUnit => PluginFormat::AudioUnit,
        Format::Vst3 => PluginFormat::Vst3,
        Format::Builtin => PluginFormat::Builtin,
    }
}

impl Devices {
    pub fn scan() -> Self {
        let catalog = Catalog::scan();
        let choices = catalog
            .available
            .iter()
            .map(|a| PluginChoice {
                name: a.info.name.clone(),
                vendor: a.info.vendor.clone(),
                format: match a.format {
                    Format::AudioUnit => "AU",
                    Format::Clap => "CLAP",
                    Format::Vst3 => "VST3",
                    Format::Builtin => "CANARY",
                },
            })
            .collect();
        Self {
            catalog,
            choices,
            live: HashMap::new(),
            retired: Vec::new(),
            removing: Vec::new(),
            removed_state: HashMap::new(),
            failed: HashSet::new(),
            free: (FIRST_DEVICE_NODE..MAX_NODES as u32).rev().collect(),
        }
    }

    /// Makes the plugin for picker entry `choice`, ready to insert as device `uid`.
    pub fn create(
        &mut self,
        choice: usize,
        uid: u64,
        sample_rate: f32,
        max_block: usize,
    ) -> Result<Device, String> {
        let a = self
            .catalog
            .available
            .get(choice)
            .ok_or("no such plugin")?
            .clone();
        let device = Device {
            uid,
            format: to_project(a.format),
            bundle: a.bundle.clone(),
            id: a.info.id.clone(),
            name: a.info.name.clone(),
            state: Vec::new(),
        };
        self.instantiate(&device, sample_rate, max_block)?;
        Ok(device)
    }

    fn instantiate(
        &mut self,
        d: &Device,
        sample_rate: f32,
        max_block: usize,
    ) -> Result<(), String> {
        let raw = self.free.pop().ok_or("too many devices")?;
        let made = self.catalog.instantiate(
            to_host(d.format),
            d.bundle.as_deref(),
            &d.id,
            sample_rate,
            max_block,
        );
        let (mut hosted, node) = match made {
            Ok(made) => made,
            Err(e) => {
                self.free.push(raw);
                return Err(format!("{}: {e}", d.name));
            }
        };
        let state = self
            .removed_state
            .remove(&d.uid)
            .unwrap_or_else(|| d.state.clone());
        if !state.is_empty() {
            // A plugin that rejects old state still works with its defaults.
            let _ = hosted.load_state(&state);
        }
        let instrument = !hosted.has_audio_input();
        self.live.insert(
            d.uid,
            Live {
                hosted,
                id: NodeId(raw),
                node: Some(node),
                instrument,
                latency: 0,
            },
        );
        Ok(())
    }

    /// Matches the live plugins to the project: makes missing ones (a loaded
    /// project, an undone removal) and retires ones no longer there. Returns errors.
    pub fn sync(&mut self, project: &Project, sample_rate: f32, max_block: usize) -> Vec<String> {
        let wanted: Vec<&Device> = project.tracks.iter().flat_map(|t| &t.devices).collect();
        let gone: Vec<u64> = self
            .live
            .keys()
            .filter(|uid| !wanted.iter().any(|d| d.uid == **uid))
            .copied()
            .collect();
        for uid in gone {
            let mut live = self.live.remove(&uid).expect("listed above");
            if let Ok(state) = live.hosted.save_state() {
                self.removed_state.insert(uid, state);
            }
            if live.node.is_none() {
                self.removing.push(live.id);
            }
            self.free.push(live.id.0);
            self.retired.push((live.hosted, RETIRE_FRAMES));
        }
        self.failed
            .retain(|uid| wanted.iter().any(|d| d.uid == *uid));
        let mut errors = Vec::new();
        for d in wanted {
            if !self.live.contains_key(&d.uid) && !self.failed.contains(&d.uid) {
                if let Err(e) = self.instantiate(d, sample_rate, max_block) {
                    self.failed.insert(d.uid);
                    errors.push(e);
                }
            }
        }
        errors
    }

    /// True if `plan` must be rebuilt to match: devices were added or removed, or
    /// an effect's latency changed (its settings or loaded state).
    pub fn changed(&self) -> bool {
        !self.removing.is_empty()
            || self.live.values().any(|l| {
                l.node.is_some() || (!l.instrument && l.hosted.latency_samples() != l.latency)
            })
    }

    /// Puts pending inserts and removals into `plan`. Call once per plan sent.
    pub fn apply_changes(&mut self, plan: &mut ExecutionPlan) {
        for id in self.removing.drain(..) {
            plan.remove(id);
        }
        for live in self.live.values_mut() {
            if let Some(node) = live.node.take() {
                plan.insert(live.id, node);
            }
        }
    }

    /// Schedules `track`'s instrument, if it has one, taking live MIDI. Its step
    /// feeds the track node.
    pub fn instrument_step(&self, plan: &mut ExecutionPlan, track: &Track) -> Option<StepId> {
        let live = track.devices.iter().filter_map(|d| self.live.get(&d.uid));
        let instrument = live.clone().find(|l| l.instrument)?;
        let step = plan.add(instrument.id, vec![], 2).ok()?;
        plan.route_midi(step);
        // shortcut: instruments only play live MIDI, which PDC does not compensate,
        // and the track node sums its clips with the instrument; delaying the
        // others would put this track's clips early. Revisit with sequenced notes.
        plan.set_latency(step, 0);
        Some(step)
    }

    /// Chains `track`'s effects after `step` and returns the last step.
    pub fn effects_after(
        &mut self,
        plan: &mut ExecutionPlan,
        track: &Track,
        mut step: StepId,
    ) -> StepId {
        for d in &track.devices {
            let Some(l) = self.live.get_mut(&d.uid).filter(|l| !l.instrument) else {
                continue;
            };
            let from = |channel| vec![Source { step, channel }];
            if let Ok(next) = plan.add(l.id, vec![from(0), from(1)], 2) {
                l.latency = l.hosted.latency_samples();
                plan.set_latency(next, l.latency);
                step = next;
            }
        }
        step
    }

    /// False if the device is not live or is not an instrument.
    pub fn is_instrument(&self, uid: u64) -> bool {
        self.live.get(&uid).is_some_and(|l| l.instrument)
    }

    /// Drops a device made by `create` that never went into the project.
    pub fn discard(&mut self, uid: u64) {
        if let Some(l) = self.live.remove(&uid) {
            // Its node never reached a plan, so both halves can go now.
            self.free.push(l.id.0);
        }
    }

    pub fn set_param(&mut self, uid: u64, id: u32, value: f64) {
        if let Some(l) = self.live.get_mut(&uid) {
            l.hosted.set_param(id, value);
        }
    }

    /// The rack's view of `track`'s devices.
    pub fn views(&mut self, track: &Track) -> Vec<DeviceView> {
        track
            .devices
            .iter()
            .map(|d| DeviceView {
                name: d.name.clone(),
                params: self.live.get_mut(&d.uid).map_or_else(Vec::new, |l| {
                    let infos: Vec<_> = l.hosted.params().iter().take(8).cloned().collect();
                    infos
                        .into_iter()
                        .map(|p| ParamView {
                            value: l.hosted.param_value(p.id).unwrap_or(p.default),
                            id: p.id,
                            name: p.name,
                            min: p.min,
                            max: p.max,
                            log: p.log,
                        })
                        .collect()
                }),
            })
            .collect()
    }

    /// Every live device's current state, for saving.
    pub fn states(&mut self) -> Vec<(u64, Vec<u8>)> {
        self.live
            .iter_mut()
            .filter_map(|(uid, l)| l.hosted.save_state().ok().map(|s| (*uid, s)))
            .collect()
    }

    /// Main-thread plugin work, and dropping retired handles once their nodes are
    /// gone. `waiting` holds the timer while a plan has not reached the engine.
    pub fn idle(&mut self, waiting: bool) {
        for l in self.live.values_mut() {
            l.hosted.idle();
        }
        if waiting {
            return;
        }
        for (_, frames) in &mut self.retired {
            *frames = frames.saturating_sub(1);
        }
        self.retired.retain(|(_, frames)| *frames > 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daw_graph::LiveNodes;
    use daw_project::Track;

    const DELAY: &str = "aufx:dely:appl";

    fn project(uids: &[u64]) -> Project {
        let mut track = Track::new("t");
        track.devices = uids
            .iter()
            .map(|&uid| Device {
                uid,
                format: PluginFormat::AudioUnit,
                bundle: None,
                id: DELAY.into(),
                name: "AUDelay".into(),
                state: Vec::new(),
            })
            .collect();
        Project {
            tracks: vec![track],
            ..Project::default()
        }
    }

    /// Each plan's node changes must be valid against the engine's live set in
    /// the order sent, including when a removed device's node id is reused.
    #[test]
    fn plans_stay_valid_through_add_remove_and_id_reuse() {
        let mut d = Devices::scan();
        let mut live = LiveNodes::default();
        let send = |d: &mut Devices, live: &mut LiveNodes| {
            let mut plan = ExecutionPlan::new(64);
            d.apply_changes(&mut plan);
            *live = live.check(&plan).expect("plan accepted");
        };

        assert!(d.sync(&project(&[1]), 48_000.0, 64).is_empty());
        assert!(d.changed());
        send(&mut d, &mut live);
        assert!(!d.changed());
        let first = d.live[&1].id;

        // Remove device 1 and add device 2 in one sync: 2 takes 1's node id.
        assert!(d.sync(&project(&[2]), 48_000.0, 64).is_empty());
        assert_eq!(d.live[&2].id, first, "freed id reused");
        send(&mut d, &mut live); // remove + insert of the same id in one plan
        assert!(live.is_live(first));

        // Removing everything leaves the id free and the engine without it.
        d.sync(&project(&[]), 48_000.0, 64);
        send(&mut d, &mut live);
        assert!(!live.is_live(first));
        assert_eq!(d.retired.len(), 2, "handles wait for their nodes to go");
        for _ in 0..RETIRE_FRAMES {
            d.idle(true); // a plan still queued: the timer holds
        }
        assert_eq!(d.retired.len(), 2);
        for _ in 0..RETIRE_FRAMES {
            d.idle(false);
        }
        assert!(d.retired.is_empty());
    }

    #[test]
    fn a_missing_plugin_fails_once_not_on_every_sync() {
        let mut d = Devices::scan();
        let mut p = project(&[7]);
        p.tracks[0].devices[0].id = "aufx:none:none".into();
        assert_eq!(d.sync(&p, 48_000.0, 64).len(), 1);
        assert!(d.sync(&p, 48_000.0, 64).is_empty(), "not retried");
        d.sync(&project(&[]), 48_000.0, 64);
        assert_eq!(
            d.sync(&p, 48_000.0, 64).len(),
            1,
            "retried once it comes back"
        );
    }
}
