//! Plugin devices: the live plugin behind every project `Device`, kept alive
//! across plan rebuilds so plugin state survives edits.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;

use daw_core::{NodeId, SampleData};
use daw_graph::{AudioNode, ExecutionPlan, Source, StepId, MAX_NODES};
use daw_host::ara::{self, AraClip};
use daw_host::{Catalog, Format, Hosted, Sampler};
use daw_project::{AraArchive, Device, PluginFormat, Project, Track};
use daw_ui::{DeviceView, EqView, ParamView, PluginChoice, SamplerView};

/// Analyzer points the EQ panel draws, spaced evenly in octaves.
const EQ_POINTS: usize = 192;

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
    /// An ARA plug-in: index into `Devices::ara`, and the track whose regions it
    /// was given before activation.
    ara: Option<(usize, usize)>,
}

struct Retired {
    hosted: Box<dyn Hosted>,
    frames: u32,
    /// Its ARA document and extension, released once the handle is gone.
    ara: Option<(usize, NonNull<c_void>)>,
}

pub struct Devices {
    /// One ARA document per plug-in factory. First field, so documents go before
    /// the catalog unloads the plug-in libraries.
    ara: Vec<ara::Document>,
    /// Saved ARA documents not yet given back to their plug-in: it is not
    /// loaded, or some of its clips' audio is not.
    pending_ara: Vec<AraArchive>,
    /// Saved ARA documents the plug-in failed to read; saved again unchanged
    /// rather than overwritten with what the plug-in has now.
    kept_ara: Vec<AraArchive>,
    catalog: Catalog,
    pub choices: Vec<PluginChoice>,
    live: HashMap<u64, Live>,
    /// Handles whose nodes a plan is removing: (handle, frames left).
    retired: Vec<Retired>,
    /// Ids removed from the engine but not yet sent in a plan.
    removing: Vec<NodeId>,
    /// Last state of removed devices, so an undo brings back what was there.
    removed_state: HashMap<u64, Vec<u8>>,
    /// Devices whose plugin failed to load; not retried on every edit.
    failed: HashSet<u64>,
    free: Vec<u32>,
    /// Open plugin editor windows by device uid.
    #[cfg(target_os = "macos")]
    editors: HashMap<u64, crate::editor::EditorWindow>,
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
                    _ if a.bundled => "STOCK",
                    Format::AudioUnit => "AU",
                    Format::Clap => "CLAP",
                    Format::Vst3 => "VST3",
                    Format::Builtin => "CANARY",
                },
            })
            .collect();
        Self {
            ara: Vec::new(),
            pending_ara: Vec::new(),
            kept_ara: Vec::new(),
            catalog,
            choices,
            live: HashMap::new(),
            retired: Vec::new(),
            removing: Vec::new(),
            removed_state: HashMap::new(),
            failed: HashSet::new(),
            free: (FIRST_DEVICE_NODE..MAX_NODES as u32).rev().collect(),
            #[cfg(target_os = "macos")]
            editors: HashMap::new(),
        }
    }

    /// Makes the plugin for picker entry `choice`, ready to insert as device `uid`
    /// on `track` (where an ARA plug-in renders).
    pub fn create(
        &mut self,
        choice: usize,
        uid: u64,
        track: usize,
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
            sidechain: None,
        };
        self.instantiate(&device, track, sample_rate, max_block)?;
        Ok(device)
    }

    fn instantiate(
        &mut self,
        d: &Device,
        track: usize,
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
        let made = made.and_then(|(hosted, node)| match hosted.ara_factory() {
            None => Ok(((hosted, node), None)),
            // ARA binds before setup, so make it again bound to the factory's document.
            Some(factory) => {
                drop((hosted, node));
                let doc = match self.ara.iter().position(|doc| doc.factory() == factory) {
                    Some(i) => i,
                    None => {
                        let doc = ara::Document::new(factory, "Canary", f64::from(sample_rate))?;
                        self.ara.push(doc);
                        self.ara.len() - 1
                    }
                };
                let document = &mut self.ara[doc];
                let controller = document.controller_ref();
                let made = self.catalog.instantiate_ara(
                    to_host(d.format),
                    d.bundle.as_deref(),
                    &d.id,
                    sample_rate,
                    max_block,
                    // SAFETY: the extension of this new instance, before activation.
                    (controller, &mut |ext| unsafe { document.bind(track, ext) }),
                )?;
                Ok((made, Some((doc, track))))
            }
        });
        let ((mut hosted, node), ara) = match made {
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
                ara,
            },
        );
        Ok(())
    }

    /// Matches the live plugins to the project: makes missing ones (a loaded
    /// project, an undone removal) and retires ones no longer there. Returns errors.
    pub fn sync(&mut self, project: &Project, sample_rate: f32, max_block: usize) -> Vec<String> {
        let wanted: Vec<(usize, &Device)> = project
            .tracks
            .iter()
            .enumerate()
            .flat_map(|(ti, t)| t.devices.iter().map(move |d| (ti, d)))
            .collect();
        let gone: Vec<u64> = self
            .live
            .keys()
            .filter(|uid| !wanted.iter().any(|(_, d)| d.uid == **uid))
            .copied()
            .collect();
        for uid in gone {
            self.retire(uid);
        }
        self.failed
            .retain(|uid| wanted.iter().any(|(_, d)| d.uid == *uid));
        let mut errors = Vec::new();
        for (track, d) in wanted {
            if !self.live.contains_key(&d.uid) && !self.failed.contains(&d.uid) {
                if let Err(e) = self.instantiate(d, track, sample_rate, max_block) {
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

    /// Schedules `track`'s instrument, if it has one, taking live MIDI when
    /// `live_midi` (the selected track). Its step feeds the track node.
    pub fn instrument_step(
        &self,
        plan: &mut ExecutionPlan,
        track: &Track,
        live_midi: bool,
        steps: &mut HashMap<u64, StepId>,
    ) -> Option<StepId> {
        let (uid, instrument) = track
            .devices
            .iter()
            .filter_map(|d| Some((d.uid, self.live.get(&d.uid)?)))
            .find(|(_, l)| l.instrument)?;
        let step = plan.add(instrument.id, vec![], 2).ok()?;
        steps.insert(uid, step);
        if live_midi {
            plan.route_midi(step);
        }
        // shortcut: instruments only play live MIDI, which PDC does not compensate,
        // and the track node sums its clips with the instrument; delaying the
        // others would put this track's clips early. Revisit with sequenced notes.
        plan.set_latency(step, 0);
        Some(step)
    }

    /// Chains `track`'s effects after `step` and returns the last step. Each
    /// device's step goes into `steps` by uid, for automation.
    pub fn effects_after(
        &mut self,
        plan: &mut ExecutionPlan,
        track: &Track,
        mut step: StepId,
        steps: &mut HashMap<u64, StepId>,
        keys: &HashMap<u64, StepId>,
    ) -> StepId {
        for d in &track.devices {
            let Some(l) = self.live.get_mut(&d.uid).filter(|l| !l.instrument) else {
                continue;
            };
            let from = |channel| vec![Source { step, channel }];
            let mut inputs = vec![from(0), from(1)];
            // Built-in effects take a sidechain key on inputs 2 and 3.
            if let (Some(&key), PluginFormat::Builtin) = (keys.get(&d.uid), d.format) {
                inputs.extend((0..2).map(|channel| vec![Source { step: key, channel }]));
            }
            if let Ok(next) = plan.add(l.id, inputs, 2) {
                l.latency = l.hosted.latency_samples();
                plan.set_latency(next, l.latency);
                steps.insert(d.uid, next);
                step = next;
            }
        }
        step
    }

    /// False if the device is not live or is not an instrument.
    pub fn is_instrument(&self, uid: u64) -> bool {
        self.live.get(&uid).is_some_and(|l| l.instrument)
    }

    /// Takes device `uid` out of the engine, keeping its state for an undo or a
    /// remake; its handle goes once its node has left the engine.
    fn retire(&mut self, uid: u64) {
        self.close_editor(uid);
        let Some(mut live) = self.live.remove(&uid) else {
            return;
        };
        if let Ok(state) = live.hosted.save_state() {
            self.removed_state.insert(uid, state);
        }
        if live.node.is_none() {
            self.removing.push(live.id);
        }
        self.free.push(live.id.0);
        let ara = live
            .ara
            .zip(live.hosted.ara_extension())
            .map(|((doc, _), ext)| (doc, ext));
        self.retired.push(Retired {
            hosted: live.hosted,
            frames: RETIRE_FRAMES,
            ara,
        });
    }

    /// True if an ARA plug-in on `track` renders its clips, so the track node must
    /// not play them too.
    pub fn ara_track(&self, track: &Track) -> bool {
        track
            .devices
            .iter()
            .any(|d| self.live.get(&d.uid).is_some_and(|l| l.ara.is_some()))
    }

    /// Brings every ARA document's playback regions in line with the clips of the
    /// tracks its plug-ins sit on. A renderer's regions can only be set before it
    /// is activated, so ARA plug-ins whose track gained or lost regions, or which
    /// moved to another track, are retired here and made again on the next sync.
    /// A saved document goes back to its plug-in once no clip it plays is still
    /// loading, so the plug-in finds each source its edits belong to. Returns errors.
    // shortcut: region ids are (track, index), so reordering clips recreates
    // regions and remakes the track's instances. Edits survive: ARA keeps them on
    // the audio modification (one per file), which has a stable persistent id.
    /// `loading` says whether a file's audio is still on its way; a file that
    /// failed to load does not hold the restore back (only its edits are lost).
    pub fn sync_ara(
        &mut self,
        project: &Project,
        audio: &HashMap<PathBuf, Arc<SampleData>>,
        loading: impl Fn(&Path) -> bool,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        if self.ara.is_empty() {
            return errors;
        }
        let mut clips: Vec<Vec<AraClip>> = (0..self.ara.len()).map(|_| Vec::new()).collect();
        let mut missing = vec![false; self.ara.len()];
        let mut remake = Vec::new();
        for (ti, track) in project.tracks.iter().enumerate() {
            let docs: Vec<usize> = track
                .devices
                .iter()
                .filter_map(|d| {
                    let (doc, bound) = self.live.get(&d.uid)?.ara?;
                    if bound != ti {
                        remake.push(d.uid);
                    }
                    Some(doc)
                })
                .collect();
            for doc in docs {
                missing[doc] |= track.clips.iter().any(|c| loading(&c.source));
                clips[doc].extend(track.clips.iter().enumerate().filter_map(|(ci, c)| {
                    Some(AraClip {
                        id: (ti as u64) << 32 | ci as u64,
                        source: c.source.to_string_lossy().into_owned(),
                        track: ti,
                        name: c.source.file_stem()?.to_string_lossy().into_owned(),
                        audio: audio.get(&c.source)?.clone(),
                        start: c.start,
                        offset: c.offset,
                        length: c.length,
                    })
                }));
            }
        }
        for (doc, (document, clips)) in self.ara.iter_mut().zip(&clips).enumerate() {
            let mut clips = clips.clone();
            clips.sort_by_key(|c| c.id);
            clips.dedup_by_key(|c| c.id); // two ARA devices on one track
            let stale = document.sync(&clips);
            let factory = document.factory().id();
            let saved = self.pending_ara.iter().position(|a| a.factory == factory);
            if let (Some(i), false) = (saved, missing[doc]) {
                let a = self.pending_ara.remove(i);
                if let Err(e) = document.restore(&a.archive_id, &a.bytes) {
                    let name = document.factory().name();
                    errors.push(format!("{name}: saved ARA edits not restored: {e}"));
                    self.kept_ara.push(a);
                }
            }
            for track in stale {
                remake.extend(project.tracks.get(track).into_iter().flat_map(|t| {
                    t.devices.iter().map(|d| d.uid).filter(|uid| {
                        self.live
                            .get(uid)
                            .is_some_and(|l| l.ara.is_some_and(|a| a.0 == doc))
                    })
                }));
            }
        }
        for uid in remake {
            self.retire(uid);
        }
        errors
    }

    /// Retires every live device and forgets kept state, so the next `sync`
    /// makes each one fresh from the project (a recovered autosave).
    pub fn reset(&mut self) {
        let uids: Vec<u64> = self.live.keys().copied().collect();
        for uid in uids {
            self.retire(uid);
        }
        self.removed_state.clear();
        self.failed.clear();
    }

    /// Saved ARA documents from the project, given back as their plug-ins load.
    pub fn restore_ara(&mut self, archives: Vec<AraArchive>) {
        self.pending_ara = archives;
    }

    /// Every ARA document to save, with errors; see `merge_archives`.
    pub fn ara_archives(&self, previous: &[AraArchive]) -> (Vec<AraArchive>, Vec<String>) {
        let live = self
            .ara
            .iter()
            .map(|d| {
                let stored = d.store().map_err(|e| e.to_string());
                (d.factory().id(), d.factory().name(), stored)
            })
            .collect();
        merge_archives(&self.pending_ara, &self.kept_ara, live, previous)
    }

    /// Shows device `uid`'s own editor, or brings its window forward.
    pub fn open_editor(&mut self, uid: u64, title: &str) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            if let Some(w) = self.editors.get(&uid) {
                w.front();
                return Ok(());
            }
            let l = self.live.get_mut(&uid).ok_or("device is not loaded")?;
            let window = crate::editor::EditorWindow::open(&mut *l.hosted, title)?;
            self.editors.insert(uid, window);
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (uid, title);
            Err("plugin editors need macOS".into())
        }
    }

    fn close_editor(&mut self, uid: u64) {
        #[cfg(target_os = "macos")]
        if let (Some(w), Some(l)) = (self.editors.remove(&uid), self.live.get_mut(&uid)) {
            w.close(&mut *l.hosted);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = uid;
    }

    /// Closes editors whose window the user closed and follows resize requests.
    fn tend_editors(&mut self) {
        #[cfg(target_os = "macos")]
        {
            let closed: Vec<u64> = self
                .editors
                .iter()
                .filter(|(_, w)| !w.is_open())
                .map(|(uid, _)| *uid)
                .collect();
            for uid in closed {
                self.close_editor(uid);
            }
            for (uid, w) in &self.editors {
                if let Some(size) = self
                    .live
                    .get_mut(uid)
                    .and_then(|l| l.hosted.editor_resized())
                {
                    w.resize(size);
                }
            }
        }
    }

    /// Drops a device made by `create` that never went into the project.
    pub fn discard(&mut self, uid: u64) {
        if let Some(l) = self.live.remove(&uid) {
            // Its node never reached a plan, so both halves can go now.
            self.free.push(l.id.0);
        }
    }

    /// The sampler behind device `uid`, if it is one.
    pub fn sampler(&mut self, uid: u64) -> Option<&mut Sampler> {
        self.live.get_mut(&uid)?.hosted.sampler()
    }

    /// Hands loaded audio to samplers waiting for it; returns the files still
    /// to load.
    pub fn feed_samplers(&mut self, audio: &HashMap<PathBuf, Arc<SampleData>>) -> Vec<PathBuf> {
        let mut wanted = Vec::new();
        for l in self.live.values_mut() {
            let Some(s) = l.hosted.sampler() else {
                continue;
            };
            for path in s.missing() {
                match audio.get(&path) {
                    Some(a) => s.set_audio(&path, a),
                    None => wanted.push(path),
                }
            }
        }
        wanted
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
                    // The EQ shows the selected band's dials, so it needs them all.
                    let shown = if l.hosted.scope().is_some() {
                        usize::MAX
                    } else {
                        8
                    };
                    let infos: Vec<_> = l.hosted.params().iter().take(shown).cloned().collect();
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
                editor: d.format != PluginFormat::Builtin,
                eq: self
                    .live
                    .get(&d.uid)
                    .and_then(|l| l.hosted.scope())
                    .map(|s| EqView {
                        sample_rate: s.sample_rate(),
                        spectrum: s.spectrum(EQ_POINTS),
                    }),
                sampler: self
                    .live
                    .get_mut(&d.uid)
                    .and_then(|l| l.hosted.sampler())
                    .map(|s| SamplerView {
                        path: s.main_path().map(Path::to_path_buf),
                        slices: s.slice_points(),
                        layers: s.layer_count(),
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
        self.tend_editors();
        if waiting {
            return;
        }
        for r in &mut self.retired {
            r.frames = r.frames.saturating_sub(1);
        }
        let (done, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.retired)
            .into_iter()
            .partition(|r| r.frames == 0);
        self.retired = waiting;
        for r in done {
            drop(r.hosted);
            if let Some((doc, ext)) = r.ara {
                // Only now may its regions go, if no other instance renders them.
                self.ara[doc].release(ext);
            }
        }
    }
}

/// A live ARA document at save time: factory id, plug-in name, `store` result.
type LiveArchive = (String, String, Result<(String, Vec<u8>), String>);

/// What a save writes for ARA: an archive not yet restored (`pending`) or one
/// the plug-in could not read (`kept`) is written back unchanged in place of its
/// plug-in's live document; every other live document as `store` gave it
/// (factory, name, result), or the `previous` save's archive if storing failed.
/// Each lost set of edits is an error, so the user hears of it.
fn merge_archives(
    pending: &[AraArchive],
    kept: &[AraArchive],
    live: Vec<LiveArchive>,
    previous: &[AraArchive],
) -> (Vec<AraArchive>, Vec<String>) {
    let mut errors = Vec::new();
    let mut out: Vec<AraArchive> = pending.iter().chain(kept).cloned().collect();
    for (factory, name, stored) in live {
        if out.iter().any(|a| a.factory == factory) {
            if kept.iter().any(|a| a.factory == factory) {
                errors.push(format!(
                    "{name}: ARA edits from this session not saved; the saved ones it could not read are kept"
                ));
            }
            continue;
        }
        match stored {
            Ok((archive_id, bytes)) => out.push(AraArchive {
                factory,
                archive_id,
                bytes,
            }),
            Err(e) => {
                errors.push(format!("{name}: ARA edits not saved: {e}"));
                out.extend(previous.iter().filter(|a| a.factory == factory).cloned());
            }
        }
    }
    (out, errors)
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
                sidechain: None,
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

    #[test]
    fn only_the_selected_track_instrument_takes_live_midi() {
        let mut d = Devices::scan();
        let mut track = Track::new("t");
        track.devices = vec![Device {
            uid: 1,
            format: PluginFormat::Builtin,
            bundle: None,
            id: "canary.sampler".into(),
            name: "Sampler".into(),
            state: Vec::new(),
            sidechain: None,
        }];
        let p = Project {
            tracks: vec![track],
            ..Project::default()
        };
        assert!(d.sync(&p, 48_000.0, 64).is_empty());
        for selected in [true, false] {
            let mut plan = ExecutionPlan::new(64);
            d.apply_changes(&mut plan);
            let step = d
                .instrument_step(&mut plan, &p.tracks[0], selected, &mut HashMap::new())
                .unwrap();
            assert_eq!(plan.takes_midi(step), selected);
        }
    }

    #[test]
    fn saves_keep_unrestored_archives_and_the_last_good_one() {
        let a = |factory: &str, bytes: &[u8]| AraArchive {
            factory: factory.into(),
            archive_id: "v1".into(),
            bytes: bytes.to_vec(),
        };
        let stored = |bytes: &[u8]| Ok(("v1".to_string(), bytes.to_vec()));
        let live = vec![
            ("pending".into(), "P".into(), stored(b"empty")),
            ("kept".into(), "K".into(), stored(b"new")),
            ("ok".into(), "O".into(), stored(b"now")),
            ("broken".into(), "B".into(), Err("fail".to_string())),
        ];
        let previous = [a("broken", b"last"), a("ok", b"old")];
        let (out, errors) = merge_archives(
            &[a("pending", b"saved")],
            &[a("kept", b"unread")],
            live,
            &previous,
        );
        assert_eq!(
            out,
            [
                a("pending", b"saved"),
                a("kept", b"unread"),
                a("ok", b"now"),
                a("broken", b"last")
            ]
        );
        assert_eq!(errors.len(), 2, "{errors:?}"); // K's session edits, B's store
    }
}
