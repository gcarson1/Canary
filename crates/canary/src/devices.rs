//! Plugin devices: the live plugin behind every project `Device`, kept alive
//! across plan rebuilds so plugin state survives edits.

use std::collections::HashMap;

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
    free: Vec<u32>,
}

fn to_host(f: PluginFormat) -> Format {
    match f {
        PluginFormat::Clap => Format::Clap,
        PluginFormat::AudioUnit => Format::AudioUnit,
        PluginFormat::Vst3 => Format::Vst3,
    }
}

fn to_project(f: Format) -> PluginFormat {
    match f {
        Format::Clap => PluginFormat::Clap,
        Format::AudioUnit => PluginFormat::AudioUnit,
        Format::Vst3 => PluginFormat::Vst3,
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
        let mut errors = Vec::new();
        for d in wanted {
            if !self.live.contains_key(&d.uid) {
                if let Err(e) = self.instantiate(d, sample_rate, max_block) {
                    errors.push(e);
                }
            }
        }
        errors
    }

    /// True if `plan` must be rebuilt to match: devices were added or removed.
    pub fn changed(&self) -> bool {
        !self.removing.is_empty() || self.live.values().any(|l| l.node.is_some())
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
        Some(step)
    }

    /// Chains `track`'s effects after `step` and returns the last step.
    pub fn effects_after(
        &self,
        plan: &mut ExecutionPlan,
        track: &Track,
        mut step: StepId,
    ) -> StepId {
        for d in &track.devices {
            let Some(l) = self.live.get(&d.uid).filter(|l| !l.instrument) else {
                continue;
            };
            let from = |channel| vec![Source { step, channel }];
            if let Ok(next) = plan.add(l.id, vec![from(0), from(1)], 2) {
                step = next;
            }
        }
        step
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

    /// Main-thread plugin work, and dropping retired handles once their nodes are gone.
    pub fn idle(&mut self) {
        for l in self.live.values_mut() {
            l.hosted.idle();
        }
        for (_, frames) in &mut self.retired {
            *frames = frames.saturating_sub(1);
        }
        self.retired.retain(|(_, frames)| *frames > 0);
    }
}
