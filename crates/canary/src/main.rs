//! Canary: the app. Wires the UI (`daw-ui`) to the engine (`daw-engine`) and the
//! project (`daw-project`): it turns the project into engine plans, loads audio
//! off the UI thread, records takes and saves the project.
//!
//! `canary [project.daw] [--samples DIR]`. Defaults to `~/Music/Canary`.

mod devices;
#[cfg(target_os = "macos")]
mod editor;
mod export;
mod take;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

use daw_core::{Clip, Command, NodeId, SampleData, SeqEvent, Telemetry, TrackParams};
use daw_dsp::WaveformPyramid;
use daw_dsp::{integrated_loudness, TruePeak};
use daw_engine::{EngineHandle, MidiConnection, MidiSender, Recorder};
use daw_graph::{
    ExecutionPlan, MasterNode, Monitor, MonitorMode, PeakTap, Preview, PreviewNode, SendNode,
    Source, StepId, TrackNode,
};
use daw_host::LayerKind;
use daw_project::{Action, ClipRef, History, Lane, Playback, Target, Track};
use daw_ui::{theme, EngineView, Inputs, Listen, Request, SampleDrop, Status, Workspace};
use devices::Devices;

/// Frames per plan buffer; larger device callbacks render in chunks.
const MAX_BLOCK: usize = 4096;
/// The reference track's node, just below the device ids; tracks stay under it.
const REFERENCE_NODE: NodeId = NodeId(devices::FIRST_DEVICE_NODE - 1);
/// Track nodes are 1..=MAX_TRACKS; send nodes take the ids after them, up to
/// the reference's.
const MAX_TRACKS: usize = 1023;
const SEND_NODES: std::ops::Range<u32> = MAX_TRACKS as u32 + 1..PREVIEW_NODE.0;
/// The browser's audition voice, inserted once and kept, like the master.
const PREVIEW_NODE: NodeId = NodeId(REFERENCE_NODE.0 - 1);
const AUDIO_EXTENSIONS: [&str; 7] = ["wav", "flac", "mp3", "m4a", "aac", "aif", "aiff"];
const MIDI_EXTENSIONS: [&str; 2] = ["mid", "midi"];
/// New projects start from this, under the samples folder.
const TEMPLATE: &str = "Templates/Default.daw";
const AUTOSAVE_EVERY: Duration = Duration::from_secs(60);

fn has_extension(path: &Path, list: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| list.contains(&e.to_ascii_lowercase().as_str()))
}

/// A finished background load.
/// A stretch render finished on a worker thread.
struct Stretched {
    track: usize,
    index: usize,
    /// The clip as it was when the render started.
    clip: ClipRef,
    original: PathBuf,
    path: PathBuf,
    ratio: f64,
    semitones: f64,
    /// Frames rendered.
    result: Result<u64, String>,
}

struct Loaded {
    path: PathBuf,
    result: Result<(Arc<SampleData>, WaveformPyramid), String>,
}

struct App {
    ws: Workspace,
    history: History,
    project_path: PathBuf,
    samples_dir: PathBuf,
    files: Vec<PathBuf>,
    engine: EngineHandle,
    _output: cpal::Stream,
    view: EngineView,
    audio: HashMap<PathBuf, Arc<SampleData>>,
    /// Sampler files already sent to load, so a failed one is not retried forever.
    sample_requests: HashSet<PathBuf>,
    waveforms: HashMap<PathBuf, WaveformPyramid>,
    /// Files being loaded, with the placements waiting on each.
    pending: HashMap<PathBuf, Vec<(Option<usize>, u64)>>,
    loads_tx: mpsc::Sender<Loaded>,
    loads_rx: mpsc::Receiver<Loaded>,
    /// Project revision the live plan was built from.
    built: Option<u64>,
    live: Vec<NodeId>,
    /// One per track, kept across rebuilds so mixer changes glide instead of
    /// rebuilding nodes; `solo` is their shared solo count.
    params: Vec<Arc<TrackParams>>,
    solo: Arc<AtomicUsize>,
    /// Each track's clips and device uids as of the live plan.
    built_clips: Vec<Vec<ClipRef>>,
    built_devices: Vec<Vec<u64>>,
    /// The track whose instrument took live MIDI in the last plan.
    built_selected: Option<usize>,
    /// What the last plan plays (PAT or SONG), and each track's notes for it.
    built_playback: Option<Playback>,
    built_sequences: Vec<Vec<SeqEvent>>,
    built_loop: Option<(u64, u64)>,
    built_clicks: Vec<SeqEvent>,
    built_automation: Vec<Vec<Lane>>,
    built_routing: Vec<Routing>,
    /// Each live send's (track, bus) and its level, set without a rebuild.
    send_levels: Vec<((usize, usize), Arc<AtomicU32>)>,
    /// Each track's output peak for the mixer, held and falling.
    meters: Vec<[f32; 2]>,
    /// The metronome switch as last sent to the engine.
    sent_metronome: bool,
    /// Live MIDI to the selected track's instrument: pads and the keyboard.
    midi: Option<Arc<Mutex<MidiSender>>>,
    _midi_in: Option<MidiConnection>,
    /// A plan the engine's queue had no room for; sent before any new one.
    queued: Option<Box<ExecutionPlan>>,
    devices: Devices,
    /// Open only while a take records, so the microphone is not held between takes.
    recorder: Option<(Recorder, cpal::Stream)>,
    /// MIDI being recorded into a pattern.
    midi_take: Option<take::MidiTake>,
    /// A mix export in progress.
    export: Option<export::Export>,
    /// The preview node until the first plan inserts it, and its control.
    preview_node: Option<Box<PreviewNode>>,
    preview: Preview,
    /// A file to audition once it has loaded.
    preview_wanted: Option<PathBuf>,
    last_autosave: Instant,
    /// The monitor mode and A/B reference to put back after an export.
    export_restore: Option<(MonitorMode, bool)>,
    /// The last export's format conversion, still running.
    converting: Option<thread::JoinHandle<Vec<String>>>,
    /// Clips (track, index) with a stretch render running.
    stretching: HashSet<(usize, usize)>,
    /// Stretched or pitched clip renders coming back from worker threads.
    stretch_tx: mpsc::Sender<Stretched>,
    stretch_rx: mpsc::Receiver<Stretched>,
    title: String,
    /// The master node until the first plan takes it; it then stays in the
    /// engine for good, so meters keep their history across edits.
    master: Option<Box<MasterNode>>,
    monitor: Arc<Monitor>,
    /// The reference file the live plan plays, and its integrated loudness.
    built_reference: Option<PathBuf>,
    /// With its true peak in dBTP.
    reference_lufs: Option<(PathBuf, f32, f32)>,
}

fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn cache_dir() -> PathBuf {
    home().join("Library/Caches/Canary/waveforms")
}

fn audio_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| has_extension(p, &AUDIO_EXTENSIONS) || has_extension(p, &MIDI_EXTENSIONS))
        .collect();
    files.sort();
    files
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        project_path: PathBuf,
        samples_dir: PathBuf,
    ) -> Result<Self, String> {
        theme::install_fonts(&cc.egui_ctx);
        let ws = Workspace::default();
        theme::apply(&cc.egui_ctx, &ws.theme);

        let (mut engine, output, config) =
            daw_engine::start_output(4096).map_err(|e| e.to_string())?;
        let midi = engine.midi.take().map(|m| Arc::new(Mutex::new(m)));
        // The first MIDI input, if any; a missing or busy port only loses the keyboard.
        let midi_in = midi.clone().and_then(|m| {
            daw_engine::connect_midi(m, 0)
                .inspect_err(|e| eprintln!("MIDI input: {e}"))
                .ok()
        });
        let template = samples_dir.join(TEMPLATE);
        let mut history = if project_path.exists() {
            History::load(&project_path).map_err(|e| format!("{}: {e}", project_path.display()))?
        } else if template.exists() {
            // A new project starts from the saved template (Cmd+Shift+T).
            History::load(&template).unwrap_or_default()
        } else {
            History::default()
        };
        if history.project().sample_rate != config.sample_rate {
            if !history.project().tracks.is_empty() {
                // shortcut: converting positions between rates is not built yet; opening
                // would mix rates in one file, so refuse instead.
                return Err(format!(
                    "{} is at {} Hz but the audio device runs at {} Hz; switch the device rate to open it",
                    project_path.display(),
                    history.project().sample_rate,
                    config.sample_rate
                ));
            }
            history = History::new(daw_project::Project {
                sample_rate: config.sample_rate,
                ..history.project().clone()
            });
        }
        let (loads_tx, loads_rx) = mpsc::channel();
        let (stretch_tx, stretch_rx) = mpsc::channel();
        let (master, monitor) = MasterNode::new(config.sample_rate as f32);
        let (preview_node, preview) = PreviewNode::new();
        let mut app = Self {
            ws,
            history,
            project_path,
            files: audio_files(&samples_dir),
            samples_dir,
            engine,
            _output: output,
            view: EngineView {
                sample_rate: config.sample_rate,
                ..EngineView::default()
            },
            audio: HashMap::new(),
            sample_requests: HashSet::new(),
            waveforms: HashMap::new(),
            pending: HashMap::new(),
            loads_tx,
            loads_rx,
            built: None,
            live: Vec::new(),
            params: Vec::new(),
            solo: Arc::new(AtomicUsize::new(0)),
            built_clips: Vec::new(),
            built_devices: Vec::new(),
            built_selected: None,
            built_playback: None,
            built_sequences: Vec::new(),
            built_loop: None,
            built_clicks: Vec::new(),
            built_automation: Vec::new(),
            built_routing: Vec::new(),
            send_levels: Vec::new(),
            meters: Vec::new(),
            sent_metronome: false,
            midi,
            _midi_in: midi_in,
            queued: None,
            devices: Devices::scan(),
            recorder: None,
            midi_take: None,
            export: None,
            last_autosave: Instant::now(),
            export_restore: None,
            converting: None,
            stretching: HashSet::new(),
            stretch_tx,
            stretch_rx,
            title: String::new(),
            master: Some(Box::new(master)),
            preview_node: Some(Box::new(preview_node)),
            preview,
            preview_wanted: None,
            monitor,
            built_reference: None,
            reference_lufs: None,
        };
        let sources: Vec<PathBuf> = app
            .history
            .project()
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(|c| c.source.clone()))
            .collect();
        for source in sources {
            app.load(source, None);
        }
        app.devices.restore_ara(app.history.project().ara.clone());
        // Work from a session that ended without saving.
        let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
        if let Some(auto) = modified(&app.autosave_path()) {
            if modified(&app.project_path).is_none_or(|saved| auto > saved) {
                app.ws.status = Some(Status {
                    text: "AUTOSAVE FOUND · CMD+SHIFT+R RECOVERS IT".into(),
                    error: false,
                });
            }
        }
        Ok(app)
    }

    /// Loads, resamples and summarizes `path` on a worker thread unless it is
    /// already loaded or loading; then places it if `place` says where.
    fn load(&mut self, path: PathBuf, place: Option<(Option<usize>, u64)>) {
        if self.audio.contains_key(&path) {
            if let Some((track, at)) = place {
                self.place(&path, track, at);
            }
            return;
        }
        if let Some(waiting) = self.pending.get_mut(&path) {
            waiting.extend(place);
            return;
        }
        self.pending
            .insert(path.clone(), place.into_iter().collect());
        let (tx, rate) = (self.loads_tx.clone(), self.view.sample_rate);
        thread::spawn(move || {
            let result = daw_engine::load_audio(&path)
                .and_then(|a| daw_engine::resample(&a, rate))
                .map(|a| {
                    let w = daw_engine::cached_waveform(&path, a.clone(), &cache_dir());
                    (a, w)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Loaded { path, result });
        });
    }

    /// Adds the whole of a loaded file at `at` on `track`, or on a new track.
    fn place(&mut self, path: &Path, track: Option<usize>, at: u64) {
        let Some(audio) = self.audio.get(path) else {
            return;
        };
        let clip = ClipRef {
            source: path.to_path_buf(),
            start: at,
            offset: 0,
            length: audio.frames() as u64,
            fade_in: 0,
            fade_out: 0,
            gain: 1.0,
            reverse: false,
            stretch: None,
        };
        let tracks = &self.history.project().tracks;
        let action = match track.filter(|&t| t < tracks.len()) {
            Some(t) => Action::InsertClip {
                track: t,
                index: tracks[t].clips.len(),
                clip,
            },
            None => {
                let name = path
                    .file_stem()
                    .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                let index = tracks.len();
                self.ws.selected_track = Some(index);
                Action::Batch(vec![
                    Action::InsertTrack {
                        index,
                        track: Track::new(name),
                    },
                    Action::InsertClip {
                        track: index,
                        index: 0,
                        clip,
                    },
                ])
            }
        };
        if let Err(e) = self.history.apply(action) {
            self.error(format!("could not add {}: {e}", path.display()));
        }
    }

    fn device_uid(&self, track: usize, device: usize) -> Option<u64> {
        let tracks = &self.history.project().tracks;
        tracks.get(track)?.devices.get(device).map(|d| d.uid)
    }

    /// Loads the reference once and measures its loudness when it arrives.
    fn prepare_reference(&mut self) {
        let Some(path) = self.history.project().reference.clone() else {
            self.monitor.set_reference(false);
            return;
        };
        match self.audio.get(&path) {
            None => {
                if self.sample_requests.insert(path.clone()) {
                    self.load(path.clone(), None);
                }
            }
            Some(audio)
                if self
                    .reference_lufs
                    .as_ref()
                    .is_none_or(|(p, ..)| *p != path) =>
            {
                let right = audio.channel(audio.num_channels().min(2) - 1);
                let lufs = integrated_loudness(audio.channel(0), right, audio.sample_rate() as f32);
                let peak = [audio.channel(0), right]
                    .iter()
                    .map(|ch| TruePeak::default().process(ch))
                    .fold(0.0f32, f32::max);
                self.reference_lufs = Some((path.clone(), lufs, 20.0 * peak.log10()));
            }
            Some(_) => {}
        }
        // B only once its loudness is known, so it never plays unmatched.
        if self
            .reference_lufs
            .as_ref()
            .is_none_or(|(p, ..)| *p != path)
        {
            self.monitor.set_reference(false);
        }
    }

    /// Gives samplers the audio they wait for, loading each file once.
    fn feed_samplers(&mut self) {
        for path in self.devices.feed_samplers(&self.audio) {
            if self.sample_requests.insert(path.clone()) {
                self.load(path, None);
            }
        }
    }

    fn receive_loads(&mut self) {
        // Placing a clip mid-drag would cancel the drag; finished loads wait.
        if self.ws.is_dragging() {
            return;
        }
        while let Ok(done) = self.loads_rx.try_recv() {
            let places = self.pending.remove(&done.path).unwrap_or_default();
            match done.result {
                Ok((audio, waveform)) => {
                    self.audio.insert(done.path.clone(), audio);
                    self.waveforms.insert(done.path.clone(), waveform);
                    self.built = None; // clips waiting on this audio can now play
                    for (track, at) in places {
                        self.place(&done.path, track, at);
                    }
                }
                Err(e) => {
                    self.built = None; // an ARA restore waiting on this file can go
                    self.error(format!("could not load {}: {e}", done.path.display()));
                }
            }
        }
    }

    fn error(&mut self, text: String) {
        eprintln!("{text}");
        self.ws.status = Some(Status { text, error: true });
    }

    /// Brings the engine up to date with the project: mixer-only edits just set
    /// the live params (smoothed, no click); anything else rebuilds the plan.
    fn sync(&mut self) {
        if let Some(plan) = self.queued.take() {
            self.send(plan);
            return;
        }
        // While exporting, the plan with the taps stays: a rebuild would drop them.
        if self.export.as_ref().is_some_and(|e| e.taps.is_empty()) {
            return;
        }
        let revision = self.history.revision();
        let playback = self.ws.playback(self.history.project());
        if self.built == Some(revision)
            && !self.devices.changed()
            && self.built_selected == self.ws.selected_track
            && self.built_playback == Some(playback)
        {
            return;
        }
        let project = self.history.project();
        let sequences: Vec<Vec<SeqEvent>> = (0..project.tracks.len())
            .map(|i| project.sequence(i, playback))
            .collect();
        let clicks = metronome_clicks(project, playback);
        let rate = self.view.sample_rate as f32;
        for e in self.devices.sync(self.history.project(), rate, MAX_BLOCK) {
            self.error(e);
        }
        let tracks = &self.history.project().tracks;
        let uids = |t: &Track| t.devices.iter().map(|d| d.uid).collect::<Vec<_>>();
        let same_shape = self.built.is_some()
            && !self.devices.changed()
            && self.built_reference == self.history.project().reference
            && self.built_selected == self.ws.selected_track
            && self.built_playback == Some(playback)
            && sequences == self.built_sequences
            && clicks == self.built_clicks
            && tracks
                .iter()
                .map(|t| &t.automation)
                .eq(self.built_automation.iter())
            && routing(self.history.project()) == self.built_routing
            && loop_range(self.history.project(), playback) == self.built_loop
            && tracks.len() == self.built_clips.len()
            && tracks
                .iter()
                .zip(&self.built_clips)
                .all(|(t, c)| &t.clips == c)
            && tracks
                .iter()
                .zip(&self.built_devices)
                .all(|(t, d)| &uids(t) == d);
        if same_shape {
            self.set_params();
            self.built = Some(revision);
        } else {
            self.rebuild(playback, sequences, clicks);
        }
    }

    fn set_params(&self) {
        let project = self.history.project();
        // A soloed track keeps the buses it feeds and the sources of a soloed bus.
        let solo = project.solo_set();
        for (i, (track, params)) in project.tracks.iter().zip(&self.params).enumerate() {
            params.set_gain(track.gain);
            params.set_pan(track.pan);
            params.set_mute(track.mute);
            params.set_solo(solo.get(i).copied().unwrap_or(false));
        }
        for ((from, to), level) in &self.send_levels {
            let gain = project
                .sends_of(*from)
                .iter()
                .find(|s| s.0 == *to)
                .map_or(0.0, |s| s.1);
            level.store(gain.to_bits(), Ordering::Relaxed);
        }
    }

    /// Sends the engine a plan for the current project: per track, its instrument
    /// (if any) into a track node into its effects, all summed into a master.
    /// Track nodes are rebuilt; plugin nodes persist and keep their state.
    fn rebuild(
        &mut self,
        playback: Playback,
        sequences: Vec<Vec<SeqEvent>>,
        clicks: Vec<SeqEvent>,
    ) {
        let count = self.history.project().tracks.len().min(MAX_TRACKS);
        self.params.truncate(count);
        while self.params.len() < count {
            self.params
                .push(Arc::new(TrackParams::new(self.solo.clone())));
        }
        self.set_params();
        let mut plan = ExecutionPlan::new(MAX_BLOCK);
        for &id in &self.live {
            plan.remove(id);
        }
        self.devices.apply_changes(&mut plan);
        let project = self.history.project();
        let tracks = &project.tracks[..count];
        // Sources before the buses they feed (and sidechain keys before their use).
        let order: Vec<usize> = project
            .route_order()
            .unwrap_or_else(|| (0..project.tracks.len()).collect())
            .into_iter()
            .filter(|&i| i < count)
            .collect();
        let mut out_step: Vec<Option<StepId>> = vec![None; count];
        let mut bus_in: Vec<Vec<StepId>> = vec![Vec::new(); count];
        let mut steps = Vec::with_capacity(count);
        let mut sends = Vec::new();
        let mut send_levels = Vec::new();
        for &i in &order {
            let track = &tracks[i];
            // An ARA plug-in on the track renders its clips itself.
            let ara = self.devices.ara_track(track);
            // A looping pattern plays on its own, as in FL's PAT mode.
            let pattern_mode = matches!(playback, Playback::Pattern(_));
            let clips = track
                .clips
                .iter()
                .filter(|_| !ara && !pattern_mode)
                .filter_map(|c| {
                    let audio = self.audio.get(&c.source)?.clone();
                    let mut clip = Clip::with_window(
                        c.start, audio, c.offset, c.length, c.fade_in, c.fade_out,
                    )?;
                    clip.gain = c.gain;
                    clip.reversed = c.reverse;
                    Some(clip)
                })
                .collect();
            let id = NodeId(i as u32 + 1);
            plan.insert(id, Box::new(TrackNode::new(clips, self.params[i].clone())));
            // shortcut: notes held while the selection moves keep sounding until
            // played again; a real fix sends note-offs to the old instrument.
            let selected = self.ws.selected_track == Some(i);
            let mut device_steps = HashMap::new();
            let inputs =
                match self
                    .devices
                    .instrument_step(&mut plan, track, selected, &mut device_steps)
                {
                    Some(step) => {
                        if let Some(seq) = sequences.get(i).filter(|s| !s.is_empty()) {
                            plan.sequence(step, seq.clone());
                        }
                        vec![step]
                    }
                    None => vec![],
                };
            // A bus sums what is routed or sent to it, after any instrument.
            let feeds: Vec<StepId> = inputs.into_iter().chain(bus_in[i].drain(..)).collect();
            let inputs = if feeds.is_empty() {
                vec![]
            } else {
                (0..2)
                    .map(|channel| feeds.iter().map(|&step| Source { step, channel }).collect())
                    .collect()
            };
            let track_step = plan.add(id, inputs, 2).expect("inputs are earlier steps");
            let keys: HashMap<u64, StepId> = track
                .devices
                .iter()
                .enumerate()
                .filter_map(|(d, dev)| Some((dev.uid, out_step[project.sidechain_of(i, d)?]?)))
                .collect();
            let last =
                self.devices
                    .effects_after(&mut plan, track, track_step, &mut device_steps, &keys);
            out_step[i] = Some(last);
            // The meter reads the track after its effects.
            plan.tap(
                last,
                Box::new(PeakTap {
                    params: self.params[i].clone(),
                }),
            );
            match project.output_of(i).filter(|&j| j < count) {
                Some(j) => bus_in[j].push(last),
                None => steps.push(last),
            }
            for (j, level) in project.sends_of(i) {
                let id = NodeId(SEND_NODES.start + sends.len() as u32);
                if j >= count || !SEND_NODES.contains(&id.0) {
                    continue;
                }
                let atomic = Arc::new(AtomicU32::new(level.to_bits()));
                plan.insert(id, Box::new(SendNode::new(atomic.clone())));
                send_levels.push(((i, j), atomic));
                let from = |channel| {
                    vec![Source {
                        step: last,
                        channel,
                    }]
                };
                let step = plan
                    .add(id, vec![from(0), from(1)], 2)
                    .expect("earlier step");
                bus_in[j].push(step);
                sends.push(id);
            }
            // Song automation only plays with the song, not under a looping pattern.
            let lanes = if playback == Playback::Song {
                &track.automation[..]
            } else {
                &[]
            };
            for lane in lanes {
                let (step, param) = match lane.target {
                    Target::Gain => (Some(track_step), TrackNode::GAIN),
                    Target::Pan => (Some(track_step), TrackNode::PAN),
                    Target::Param { device, id } => (device_steps.get(&device).copied(), id),
                };
                if let Some(step) = step {
                    plan.automate(step, param, project.envelope(lane));
                }
            }
        }
        let master = NodeId(0);
        if let Some(node) = self.master.take() {
            plan.insert(master, node);
        }
        // The browser preview joins the mix, but not an export's.
        if let Some(node) = self.preview_node.take() {
            plan.insert(PREVIEW_NODE, node);
        }
        if self.export.is_none() {
            if let Ok(step) = plan.add(PREVIEW_NODE, vec![], 2) {
                steps.push(step);
            }
        }
        let sum = |channel| steps.iter().map(|&step| Source { step, channel }).collect();
        let mut inputs: Vec<Vec<Source>> = vec![sum(0), sum(1)];
        self.live = (1..=count as u32).map(NodeId).chain(sends).collect();
        self.send_levels = send_levels;
        // The reference plays in sync, into the master's spare inputs, never the mix.
        let reference = self.history.project().reference.clone();
        if let Some(audio) = reference.as_ref().and_then(|p| self.audio.get(p)) {
            let params = Arc::new(TrackParams::new(Arc::default()));
            // Equal-power center pan is -3 dB; undo it so B plays as measured.
            params.set_gain(std::f32::consts::SQRT_2);
            let clip = Clip::new(0, audio.clone());
            plan.insert(REFERENCE_NODE, Box::new(TrackNode::new(vec![clip], params)));
            let step = plan.add(REFERENCE_NODE, vec![], 2).expect("no sources");
            inputs.extend((0..2).map(|channel| vec![Source { step, channel }]));
            self.live.push(REFERENCE_NODE);
        }
        self.built_reference = reference;
        let master_step = plan
            .add(master, inputs, 2)
            .expect("sources are earlier steps");
        if let Some(e) = self.export.as_mut() {
            // Each output lags by its path's plugin delay: record that much
            // longer, then trim it off the front.
            plan.compensate();
            for (who, input) in e.taps.drain(..) {
                let step = match who {
                    None => Some(master_step),
                    Some(i) => out_step.get(i).copied().flatten(),
                };
                let Some(step) = step else { continue };
                let skip = u64::from(plan.step_latency(step).unwrap_or(0));
                let out = match who {
                    None => Some(&mut e.master),
                    Some(i) => e.stems.iter_mut().find(|s| s.0 == i).map(|s| &mut s.1),
                };
                if let Some(out) = out {
                    out.skip = skip;
                }
                plan.tap(step, daw_engine::bounce_tap(input, MAX_BLOCK, e.end + skip));
            }
        }
        let tracks = &self.history.project().tracks[..count];
        self.built_clips = tracks.iter().map(|t| t.clips.clone()).collect();
        self.built_devices = tracks
            .iter()
            .map(|t| t.devices.iter().map(|d| d.uid).collect())
            .collect();
        self.built = Some(self.history.revision());
        self.built_selected = self.ws.selected_track;
        // PAT mode loops the pattern; jump into it if the playhead is past its end.
        let looping = loop_range(self.history.project(), playback);
        // A loop that changed (mode, pattern length, tempo) may now end behind the
        // playhead, which the transport would never wrap: move it in.
        if looping != self.built_loop {
            if let Some(at) = into_loop(looping, self.view.playhead) {
                let _ = self.engine.commands.push(Command::Seek(at));
            }
        }
        self.built_playback = Some(playback);
        self.built_sequences = sequences;
        self.built_loop = looping;
        plan.set_clicks(clicks.clone());
        self.built_clicks = clicks;
        self.built_routing = routing(self.history.project());
        self.built_automation = self
            .history
            .project()
            .tracks
            .iter()
            .map(|t| t.automation.clone())
            .collect();
        if let Err(e) = self.engine.commands.push(Command::SetLoop(looping)) {
            eprintln!("engine busy, dropped {e:?}");
            self.built_loop = None; // differs next frame, so the loop is sent again
            self.built = None;
        }
        let pending = &self.pending;
        let errors = self
            .devices
            .sync_ara(self.history.project(), &self.audio, |p| {
                pending.contains_key(p)
            });
        for e in errors {
            self.error(e);
        }
        self.send(Box::new(plan));
    }

    /// Sends a plan, keeping it for next frame if the queue is full: it may carry
    /// plugin nodes that exist nowhere else.
    fn send(&mut self, plan: Box<ExecutionPlan>) {
        match self.engine.send_plan(plan) {
            Ok(()) => {}
            Err(daw_engine::SendPlanError::QueueFull(plan)) => self.queued = Some(plan),
            Err(e) => self.error(format!("plan rejected: {e}")),
        }
    }

    fn read_telemetry(&mut self) {
        let mut peak = [0.0f32; 2];
        let mut got_levels = false;
        while let Ok(t) = self.engine.telemetry.pop() {
            match t {
                Telemetry::Playhead { sample, playing } => {
                    self.view.playhead = sample;
                    self.view.playing = playing;
                }
                Telemetry::Load(load) => {
                    // Shown as a slow-falling peak, so short spikes are seen.
                    self.view.dsp_load = load.max(self.view.dsp_load * 0.97);
                }
                Telemetry::Levels { peak: p, rms } => {
                    got_levels = true;
                    peak = [peak[0].max(p[0]), peak[1].max(p[1])];
                    self.view.rms = rms;
                }
            }
        }
        if got_levels {
            self.view.peak = peak;
        }
        // Track meters fall about 20 dB a second at 60 frames a second.
        self.meters.resize(self.params.len(), [0.0; 2]);
        for (shown, params) in self.meters.iter_mut().zip(&self.params) {
            let peak = params.take_peak();
            for side in 0..2 {
                shown[side] = peak[side].max(shown[side] * 0.963);
            }
        }
        let r = self.monitor.readings();
        self.view.loudness = [r.momentary, r.short_term, r.integrated];
        self.view.true_peak = r.true_peak;
        self.view.correlation = r.correlation;
        self.view.reference_on = self.monitor.reference();
        self.view.listen = match self.monitor.mode() {
            MonitorMode::Stereo => Listen::Stereo,
            MonitorMode::Mid => Listen::Mid,
            MonitorMode::Side => Listen::Side,
        };
        // Level-match the reference to the mix: integrated once there is one.
        let mix = [r.integrated, r.short_term]
            .into_iter()
            .find(|v| v.is_finite());
        let reference = self.reference_lufs.as_ref().map(|(_, l, p)| (*l, *p));
        if let (Some(mix), Some((reference, peak))) = (mix, reference.filter(|r| r.0.is_finite())) {
            // Never boost the reference past 0 dBTP: nothing after the master limits.
            let db = (mix - reference).clamp(-24.0, (-peak).clamp(0.0, 24.0));
            self.monitor.set_reference_gain(10f32.powf(db / 20.0));
        }
        self.view.recording = self.midi_take.is_some()
            || self
                .recorder
                .as_ref()
                .is_some_and(|(r, _)| r.is_recording());
        // Live MIDI the engine played; kept only while a take records.
        while let Ok(e) = self.engine.midi_log.pop() {
            if let Some(take) = self.midi_take.as_mut() {
                take.add(e, self.history.project());
            }
        }
    }

    fn handle(&mut self, request: Request) {
        let command = |app: &mut Self, c| {
            if let Err(e) = app.engine.commands.push(c) {
                eprintln!("engine busy, dropped {e:?}");
            }
        };
        match request {
            // An export owns the transport: stopping it aborts, the rest wait.
            Request::Stop if self.export.is_some() => {
                self.abort_export("EXPORT ABORTED".into(), false)
            }
            // So is the monitor section the master is recorded after.
            Request::Play | Request::Seek(_) | Request::Listen(_) | Request::ToggleReference
                if self.export.is_some() => {}
            Request::Play => command(self, Command::Play),
            Request::Stop => command(self, Command::Stop),
            Request::Seek(at) => {
                // While a pattern loops, a seek lands inside it.
                let at = into_loop(self.built_loop, at).unwrap_or(at);
                command(self, Command::Seek(at))
            }
            Request::AddFile { track, path, .. } if has_extension(&path, &MIDI_EXTENSIONS) => {
                self.import_midi(track, &path)
            }
            Request::AddFile { track, path, at } => self.load(path, Some((track, at))),
            Request::ExportMidi { track } => self.export_midi(track),
            Request::ToggleRecord => self.toggle_record(),
            Request::Export { stems } => self.toggle_export(stems),
            Request::Preview(path) => {
                self.preview_wanted = None;
                match path {
                    Some(p) if has_extension(&p, &MIDI_EXTENSIONS) => {}
                    Some(p) => match self.audio.get(&p) {
                        Some(audio) => drop(self.preview.play(Some(audio.clone()))),
                        None => {
                            self.preview_wanted = Some(p.clone());
                            self.load(p, None);
                        }
                    },
                    None => drop(self.preview.play(None)),
                }
            }
            Request::Stretch {
                track,
                index,
                ratio,
                semitones,
            } => self.stretch_clip(track, index, ratio, semitones),
            Request::AddDevice { track, choice } => {
                let uid = self.history.new_device_uid();
                let rate = self.view.sample_rate as f32;
                match self.devices.create(choice, uid, track, rate, MAX_BLOCK) {
                    Ok(device) => {
                        let tracks = &self.history.project().tracks;
                        // One instrument per track: a second would never be scheduled.
                        let has_instrument = tracks.get(track).is_some_and(|t| {
                            t.devices.iter().any(|d| self.devices.is_instrument(d.uid))
                        });
                        if has_instrument && self.devices.is_instrument(device.uid) {
                            self.devices.discard(device.uid);
                            return self.error("this track already has an instrument".into());
                        }
                        let index = tracks.get(track).map_or(0, |t| t.devices.len());
                        let action = Action::InsertDevice {
                            track,
                            index,
                            device,
                        };
                        if let Err(e) = self.history.apply(action) {
                            self.error(format!("could not add device: {e}"));
                        }
                    }
                    Err(e) => self.error(format!("could not load plugin: {e}")),
                }
            }
            Request::SetParam {
                track,
                device,
                param,
                value,
            } => {
                if let Some(uid) = self.device_uid(track, device) {
                    self.devices.set_param(uid, param, value);
                    self.history.mark_dirty(); // so quitting saves the plugin's state
                }
            }
            Request::OpenEditor { track, device } => {
                let tracks = &self.history.project().tracks;
                let found = tracks.get(track).and_then(|t| t.devices.get(device));
                if let Some((uid, name)) = found.map(|d| (d.uid, d.name.clone())) {
                    if let Err(e) = self.devices.open_editor(uid, &name) {
                        self.error(e);
                    }
                }
            }
            Request::Listen(listen) => self.monitor.set_mode(match listen {
                Listen::Stereo => MonitorMode::Stereo,
                Listen::Mid => MonitorMode::Mid,
                Listen::Side => MonitorMode::Side,
            }),
            Request::ToggleReference => self.monitor.set_reference(!self.monitor.reference()),
            Request::ResetMeters => self.monitor.reset_meters(),
            Request::LoadSample {
                track,
                device,
                path,
                drop,
            } => {
                let kind = match drop {
                    SampleDrop::Replace => LayerKind::Replace,
                    SampleDrop::RoundRobin => LayerKind::RoundRobin,
                    SampleDrop::Velocity => LayerKind::Velocity,
                    SampleDrop::Key => LayerKind::Key,
                };
                let uid = self.device_uid(track, device);
                if let Some(s) = uid.and_then(|uid| self.devices.sampler(uid)) {
                    // Dropping a file again retries a load that failed.
                    self.sample_requests.remove(&path);
                    s.add_layer(path, kind);
                    self.history.mark_dirty();
                }
            }
            Request::Note { key, on, .. } => {
                // The pad's track is the selected one, which takes live MIDI.
                let message = if on { [0x90, key, 100] } else { [0x80, key, 0] };
                let sent = self
                    .midi
                    .as_ref()
                    .and_then(|m| m.lock().ok())
                    .is_some_and(|mut m| m.send(&message));
                if !sent {
                    self.error("pad note dropped".into());
                }
            }
            Request::SliceMarker {
                track,
                device,
                at,
                add,
            } => {
                let uid = self.device_uid(track, device);
                if let Some(s) = uid.and_then(|uid| self.devices.sampler(uid)) {
                    s.edit_marker(at, add);
                    self.history.mark_dirty();
                }
            }
        }
    }

    /// Renders clip `index` on `track` from its original file, `ratio` times as
    /// long and `semitones` higher, on a worker thread; `receive_stretches`
    /// swaps it in.
    fn stretch_clip(&mut self, track: usize, index: usize, ratio: f64, semitones: f64) {
        let Some(clip) = self
            .history
            .project()
            .tracks
            .get(track)
            .and_then(|t| t.clips.get(index))
            .cloned()
        else {
            return;
        };
        let original = clip
            .stretch
            .as_ref()
            .map_or_else(|| clip.source.clone(), |s| s.original.clone());
        let stem = original
            .file_stem()
            .map_or_else(|| "clip".into(), |s| s.to_string_lossy().into_owned());
        if !self.stretching.insert((track, index)) {
            return; // one render per clip at a time
        }
        let dir = self.samples_dir.join("Stretched");
        // The full original path and the rate in the name, so two files called
        // Kick.wav never share a render.
        let mut hash = std::hash::DefaultHasher::new();
        std::hash::Hash::hash(&(&original, self.view.sample_rate), &mut hash);
        let tag = std::hash::Hasher::finish(&hash) & 0xff_ffff;
        let path = dir.join(format!(
            "{stem} {tag:06x} x{ratio:.3} {semitones:+.0}st.wav"
        ));
        let (tx, rate) = (self.stretch_tx.clone(), self.view.sample_rate);
        self.ws.status = Some(Status {
            text: "RENDERING STRETCH".into(),
            error: false,
        });
        thread::spawn(move || {
            let result = (|| -> Result<u64, String> {
                fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let audio = daw_engine::load_audio(&original)
                    .and_then(|a| daw_engine::resample(&a, rate))
                    .map_err(|e| e.to_string())?;
                let channels: Vec<Vec<f32>> = (0..audio.num_channels())
                    .map(|c| audio.channel(c).to_vec())
                    .collect();
                let out = daw_dsp::stretch::stretch_and_pitch(&channels, ratio, semitones);
                let frames = out.first().map_or(0, Vec::len) as u64;
                daw_engine::write_wav(&path, &out, rate)?;
                Ok(frames)
            })();
            let _ = tx.send(Stretched {
                track,
                index,
                clip,
                original,
                path,
                ratio,
                semitones,
                result,
            });
        });
    }

    /// Puts finished stretch renders in place of their clips, if the clip is
    /// still the one the render started from.
    fn receive_stretches(&mut self) {
        if self.ws.is_dragging() {
            return;
        }
        while let Ok(done) = self.stretch_rx.try_recv() {
            self.stretching.remove(&(done.track, done.index));
            let frames = match done.result {
                Ok(frames) if frames > 0 => frames,
                Ok(_) => {
                    self.error("stretch made no audio; the clip is too short".into());
                    continue;
                }
                Err(e) => {
                    self.error(format!("stretch failed: {e}"));
                    continue;
                }
            };
            let tracks = &self.history.project().tracks;
            let same = tracks
                .get(done.track)
                .and_then(|t| t.clips.get(done.index))
                .is_some_and(|c| *c == done.clip);
            if !same {
                self.error("clip changed while stretching; render discarded".into());
                continue;
            }
            // Scale the window from the old render's frames to the new one's.
            let old = done.clip.stretch.as_ref().map_or(1.0, |s| s.ratio);
            let scale = done.ratio / old;
            let at = |v: u64| ((v as f64 * scale).round() as u64).min(frames);
            let offset = at(done.clip.offset).min(frames - 1);
            let length = at(done.clip.length).clamp(1, frames - offset);
            let fade_in = at(done.clip.fade_in).min(length);
            let clip = ClipRef {
                source: done.path.clone(),
                offset,
                length,
                fade_in,
                fade_out: at(done.clip.fade_out).min(length - fade_in),
                stretch: Some(daw_project::Stretch {
                    original: done.original,
                    ratio: done.ratio,
                    semitones: done.semitones,
                }),
                ..done.clip
            };
            match self.history.apply(Action::SetClip {
                track: done.track,
                index: done.index,
                clip,
            }) {
                Ok(()) => {
                    self.load(done.path, None);
                    self.ws.status = Some(Status {
                        text: "STRETCHED".into(),
                        error: false,
                    });
                }
                Err(e) => self.error(format!("stretch: {e}")),
            }
        }
    }

    /// Starts an export of the song (and with `stems`, of every track), or
    /// aborts the one running.
    fn toggle_export(&mut self, stems: bool) {
        if self.export.is_some() {
            return self.abort_export("EXPORT ABORTED".into(), false);
        }
        if self.converting.is_some() {
            return self.error("still converting the last export".into());
        }
        let project = self.history.project();
        let song_end = project.song_end();
        if song_end == 0 {
            return self.error("nothing to export".into());
        }
        let rate = project.sample_rate;
        let end = project.samples_at(song_end) + export::TAIL_SECONDS * u64::from(rate);
        let tracks: Vec<(usize, String)> = if stems {
            project
                .tracks
                .iter()
                .enumerate()
                .map(|(i, t)| (i, t.name.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let name = self
            .project_path
            .file_stem()
            .map_or_else(|| "Canary".into(), |s| s.to_string_lossy().into_owned());
        let started = export::Export::start(
            &self.samples_dir.join("Exports"),
            &name,
            &tracks,
            rate,
            end,
            self.ws.export_format,
            self.engine.playhead_clock(),
        );
        match started {
            Ok(e) => {
                self.export = Some(e);
                self.ws.song = true;
                // The mix as it is, not what the monitor section is listening to.
                self.export_restore = Some((self.monitor.mode(), self.monitor.reference()));
                self.monitor.set_mode(MonitorMode::Stereo);
                self.monitor.set_reference(false);
                self.built = None; // the next plan carries the taps
                                   // Play waits until that plan is delivered (tend_export).
                for c in [Command::Stop, Command::Seek(0)] {
                    let _ = self.engine.commands.push(c);
                }
            }
            Err(e) => self.error(format!("cannot export: {e}")),
        }
    }

    /// Stops an export, deletes its partial files and puts the monitor back.
    fn abort_export(&mut self, text: String, error: bool) {
        if let Some(e) = self.export.take() {
            let _ = self.engine.commands.push(Command::Stop);
            e.abort();
            self.built = None;
        }
        self.restore_monitor();
        self.ws.status = Some(Status { text, error });
    }

    fn restore_monitor(&mut self) {
        if let Some((mode, reference)) = self.export_restore.take() {
            self.monitor.set_mode(mode);
            self.monitor.set_reference(reference);
        }
    }

    /// Starts the export rolling once its plan is in, and ends it once the song
    /// has played through; reports a finished conversion.
    fn tend_export(&mut self) {
        if self.converting.as_ref().is_some_and(|j| j.is_finished()) {
            let job = self.converting.take().expect("checked");
            match job.join() {
                Ok(failures) if failures.is_empty() => {}
                Ok(failures) => self.error(failures.join("; ")),
                Err(_) => self.error("conversion thread failed".into()),
            }
        }
        let Some(e) = self.export.as_mut() else {
            return;
        };
        if !e.play_sent {
            if e.taps.is_empty() && self.queued.is_none() {
                let _ = self.engine.commands.push(Command::Play);
                e.play_sent = true;
            }
            return;
        }
        // A playhead from before the seek to 0 does not count as rolling.
        if self.view.playing && self.view.playhead < e.end {
            e.started = true;
        }
        if !e.started {
            return;
        }
        let end = e.record_end();
        if self.view.playhead < end {
            if !self.view.playing {
                return self.abort_export("EXPORT STOPPED EARLY · NOTHING SAVED".into(), true);
            }
            let pct = self.view.playhead * 100 / end.max(1);
            self.ws.status = Some(Status {
                text: format!("EXPORTING {pct}% · EXPORT AGAIN TO ABORT"),
                error: false,
            });
            return;
        }
        let e = self.export.take().expect("checked above");
        let _ = self.engine.commands.push(Command::Stop);
        self.built = None;
        self.restore_monitor();
        match e.finish() {
            Ok((path, job)) => {
                self.converting = Some(job);
                self.ws.status = Some(Status {
                    text: format!(
                        "EXPORTED {}",
                        path.file_name()
                            .map_or_else(String::new, |n| n.to_string_lossy().to_uppercase())
                    ),
                    error: false,
                });
            }
            Err(e) => self.error(format!("export failed: {e}")),
        }
    }

    /// Reads a MIDI file into `track` (a new one if `None`), replacing its notes
    /// in the current pattern and lengthening the pattern to whole bars to fit.
    fn import_midi(&mut self, track: Option<usize>, path: &Path) {
        let name = path
            .file_stem()
            .map_or_else(|| "MIDI".into(), |s| s.to_string_lossy().into_owned());
        let imported = fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|b| daw_project::smf::read(&b).map_err(|e| e.to_string()));
        let notes = match imported {
            Ok(notes) if !notes.is_empty() => notes,
            Ok(_) => return self.error(format!("{name}: no notes")),
            Err(e) => return self.error(format!("{name}: {e}")),
        };
        let project = self.history.project();
        let mut batch = Vec::new();
        let ti = match track.filter(|&t| t < project.tracks.len()) {
            Some(t) => t,
            None => {
                let index = project.tracks.len();
                batch.push(Action::InsertTrack {
                    index,
                    track: Track::new(name.clone()),
                });
                index
            }
        };
        let bar = project.bar_tick(1).max(1);
        let end = notes.iter().map(|n| n.end()).max().unwrap_or(1);
        let length = (end.div_ceil(bar) * bar).min(daw_project::MAX_TICKS);
        let (id, create) = self.ws.pattern_for_edit(project);
        match create {
            Some(Action::InsertPattern { index, mut pattern }) => {
                pattern.length = pattern.length.max(length);
                batch.push(Action::InsertPattern { index, pattern });
            }
            _ => {
                let index = project.patterns.iter().position(|p| p.id == id);
                if let Some(index) = index.filter(|&i| project.patterns[i].length < length) {
                    let mut pattern = project.patterns[index].clone();
                    pattern.length = length;
                    batch.push(Action::SetPattern { index, pattern });
                }
            }
        }
        batch.push(Action::SetNotes {
            track: ti,
            pattern: id,
            notes,
        });
        match self.history.apply(Action::Batch(batch)) {
            Ok(()) => {
                self.ws.selected_track = Some(ti);
                self.ws.status = Some(Status {
                    text: format!("IMPORTED {}", name.to_uppercase()),
                    error: false,
                });
            }
            Err(e) => self.error(format!("{name}: {e}")),
        }
    }

    /// Writes `track`'s notes in the current pattern to a MIDI file in the
    /// samples folder, named after the track and pattern.
    fn export_midi(&mut self, track: usize) {
        let project = self.history.project();
        let (Some(t), Some(pattern)) = (
            project.tracks.get(track),
            self.ws
                .current_pattern(project)
                .and_then(|id| project.pattern(id)),
        ) else {
            return self.error("nothing to export".into());
        };
        let notes = t.notes_in(pattern.id);
        if notes.is_empty() {
            return self.error("no notes to export".into());
        }
        let clean = |s: &str| s.replace(['/', ':'], "-");
        let file = format!("{} - {}.mid", clean(&t.name), clean(&pattern.name));
        let bytes = daw_project::smf::write(notes, project.bpm);
        match fs::write(self.samples_dir.join(&file), bytes) {
            Ok(()) => {
                self.files = audio_files(&self.samples_dir);
                self.ws.status = Some(Status {
                    text: format!("EXPORTED {}", file.to_uppercase()),
                    error: false,
                });
            }
            Err(e) => self.error(format!("{file}: {e}")),
        }
    }

    /// Starts or ends a MIDI take into the selected track's current pattern
    /// (made if there is none). The transport plays; in PAT mode the pattern
    /// loops and every pass overdubs. The take lands as one undo step.
    fn toggle_midi_take(&mut self) {
        if let Some(take) = self.midi_take.take() {
            let _ = self.engine.commands.push(Command::Stop);
            let project = self.history.project();
            let t = project.tracks.get(take.track);
            let Some(t) = t.filter(|_| project.tracks.len() == take.tracks) else {
                return self.error("take discarded: tracks were added or removed".into());
            };
            if project.pattern(take.pattern).is_none() {
                return self.error("take's pattern was removed".into());
            }
            let (track, pattern) = (take.track, take.pattern);
            let notes = take.finish(t.notes_in(pattern));
            if let Err(e) = self.history.apply(Action::SetNotes {
                track,
                pattern,
                notes,
            }) {
                self.error(format!("take not saved: {e}"));
            }
            return;
        }
        let Some(track) = self
            .ws
            .selected_track
            .filter(|&t| t < self.history.project().tracks.len())
        else {
            return self.error("select a track to record MIDI".into());
        };
        // Takes wrap at the pattern's length, which only matches a looping pattern.
        if self.ws.playback(self.history.project()) == Playback::Song {
            return self.error("switch to PAT to record MIDI".into());
        }
        let (id, create) = self.ws.pattern_for_edit(self.history.project());
        if let Some(create) = create {
            if let Err(e) = self.history.apply(create) {
                return self.error(format!("cannot record: {e}"));
            }
        }
        let length = self.history.project().pattern(id).map_or(1, |p| p.length);
        while self.engine.midi_log.pop().is_ok() {} // nothing from before the take
        let tracks = self.history.project().tracks.len();
        self.midi_take = Some(take::MidiTake::new(track, tracks, id, length));
        if self.ws.count_in {
            // One bar of the meter at the playhead, at the tempo there.
            let project = self.history.project();
            let tick = project.ticks_at(self.view.playhead);
            let meter = project.bar_beat(tick).meter;
            let beat = project.samples_at(tick + meter.beat_ticks()) - project.samples_at(tick);
            let _ = self.engine.commands.push(Command::CountIn {
                frames: beat * u64::from(meter.numerator),
                beat,
                beats_per_bar: meter.numerator,
            });
        }
        let _ = self.engine.commands.push(Command::Play);
    }

    fn toggle_record(&mut self) {
        let audio_take = self
            .recorder
            .as_ref()
            .is_some_and(|(r, _)| r.is_recording());
        if self.midi_take.is_some() || (self.ws.page != daw_ui::Page::Playlist && !audio_take) {
            return self.toggle_midi_take();
        }
        if self.recorder.is_none() {
            match daw_engine::start_input(self.view.sample_rate, self.engine.playhead_clock()) {
                Ok((recorder, stream, _)) => self.recorder = Some((recorder, stream)),
                Err(e) => return self.error(format!("cannot record: {e}")),
            }
        }
        let Some((recorder, _)) = self.recorder.as_mut() else {
            return;
        };
        if recorder.is_recording() {
            let take = match recorder.stop() {
                Ok(take) => take,
                Err(daw_engine::RecordError::Wav {
                    error,
                    take: Some(take),
                }) => {
                    self.error(format!("take cut short: {error}"));
                    take
                }
                Err(e) => {
                    self.recorder = None;
                    return self.error(format!("recording failed: {e}"));
                }
            };
            self.recorder = None; // release the microphone between takes
            self.files = audio_files(&self.samples_dir);
            // The mix plays `plan_latency` behind the transport (PDC), and the
            // performer played to what they heard.
            let start = take
                .start_sample
                .saturating_sub(u64::from(self.engine.plan_latency()));
            self.load(take.path, Some((self.ws.selected_track, start)));
        } else {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let path = self.samples_dir.join(format!("Take-{stamp}.wav"));
            match recorder.start(&path) {
                Ok(()) => {
                    let _ = self.engine.commands.push(Command::Play);
                }
                Err(e) => {
                    self.recorder = None;
                    self.error(format!("cannot record: {e}"));
                }
            }
        }
    }

    /// Saves the project; true on success.
    /// Records every plugin's and ARA document's state in the project; returns
    /// the ARA errors to show.
    fn gather_state(&mut self) -> Vec<String> {
        for (uid, state) in self.devices.states() {
            self.history.store_device_state(uid, state);
        }
        let (archives, errors) = self.devices.ara_archives(&self.history.project().ara);
        self.history.store_ara(archives);
        errors
    }

    /// Where unsaved work is written every minute.
    fn autosave_path(&self) -> PathBuf {
        self.project_path.with_extension("autosave.daw")
    }

    /// Writes unsaved edits to the autosave file once a minute.
    fn autosave(&mut self) {
        if !self.history.is_dirty()
            || self.export.is_some()
            || self.last_autosave.elapsed() < AUTOSAVE_EVERY
        {
            return;
        }
        self.last_autosave = Instant::now();
        let _ = self.gather_state();
        if let Err(e) = self.history.save_copy(&self.autosave_path()) {
            self.error(format!("autosave failed: {e}"));
        }
    }

    /// Replaces the project with its autosave (Cmd+Shift+R), as an edit away from
    /// the saved file.
    fn recover(&mut self) {
        let path = self.autosave_path();
        match History::load(&path) {
            Ok(h) if h.project().sample_rate == self.view.sample_rate => {
                self.history = h;
                self.history.mark_dirty();
                let sources: Vec<PathBuf> = self
                    .history
                    .project()
                    .tracks
                    .iter()
                    .flat_map(|t| t.clips.iter().map(|c| c.source.clone()))
                    .collect();
                for source in sources {
                    self.load(source, None);
                }
                self.built = None;
                self.ws.status = Some(Status {
                    text: "RECOVERED FROM AUTOSAVE".into(),
                    error: false,
                });
            }
            Ok(_) => self.error("autosave is at another sample rate".into()),
            Err(e) => self.error(format!("no autosave to recover: {e}")),
        }
    }

    /// Saves the project as the template new projects start from (Cmd+Shift+T).
    fn save_template(&mut self) {
        let _ = self.gather_state();
        let path = self.samples_dir.join(TEMPLATE);
        let made = path.parent().map_or(Ok(()), fs::create_dir_all);
        match made
            .map_err(|e| e.to_string())
            .and_then(|()| self.history.save_copy(&path).map_err(|e| e.to_string()))
        {
            Ok(()) => {
                self.ws.status = Some(Status {
                    text: "SAVED AS TEMPLATE".into(),
                    error: false,
                })
            }
            Err(e) => self.error(format!("template not saved: {e}")),
        }
    }

    fn save(&mut self) -> bool {
        let errors = self.gather_state();
        match self.history.save(&self.project_path) {
            Ok(()) => {
                let _ = fs::remove_file(self.autosave_path());
                self.ws.status = Some(Status {
                    text: "SAVED".into(),
                    error: false,
                });
                // After SAVED, so a plug-in that lost its edits is what shows.
                for e in errors {
                    self.error(e);
                }
                true
            }
            Err(e) => {
                self.error(format!("SAVE FAILED: {e}"));
                false
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.receive_loads();
        self.receive_stretches();
        self.preview.collect();
        if let Some(audio) = self
            .preview_wanted
            .as_ref()
            .and_then(|p| self.audio.get(p))
            .cloned()
        {
            self.preview_wanted = None;
            self.preview.play(Some(audio));
        }
        self.read_telemetry();
        self.tend_export();
        self.engine.collect_garbage();
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
            self.save();
        }
        let (recover, template) = ctx.input(|i| {
            let cs = i.modifiers.command && i.modifiers.shift;
            (
                cs && i.key_pressed(egui::Key::R),
                cs && i.key_pressed(egui::Key::T),
            )
        });
        if recover {
            self.recover();
        }
        if template {
            self.save_template();
        }
        self.autosave();
        // Quitting mid-export leaves no half-written files.
        if ctx.input(|i| i.viewport().close_requested()) && self.export.is_some() {
            self.abort_export("EXPORT ABORTED".into(), false);
        }
        // Quitting saves edits; if that fails the window stays open with the error.
        if ctx.input(|i| i.viewport().close_requested()) && self.history.is_dirty() && !self.save()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }

        self.feed_samplers();
        self.prepare_reference();
        self.devices.idle(self.queued.is_some());
        let views = self
            .ws
            .selected_track
            .and_then(|i| self.history.project().tracks.get(i))
            .map_or_else(Vec::new, |t| self.devices.views(t));
        let requests = self.ws.show(
            ui,
            Inputs {
                history: &mut self.history,
                engine: &self.view,
                waveforms: &self.waveforms,
                files: &self.files,
                plugins: &self.devices.choices,
                devices: &views,
                meters: &self.meters,
            },
        );
        for request in requests {
            self.handle(request);
        }
        if self.ws.metronome != self.sent_metronome
            && self
                .engine
                .commands
                .push(Command::SetMetronome(self.ws.metronome))
                .is_ok()
        {
            self.sent_metronome = self.ws.metronome;
        }
        self.sync();

        let name = self
            .project_path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let dirty = if self.history.is_dirty() {
            " (edited)"
        } else {
            ""
        };
        // Every viewport command repaints, so send the title only when it changes.
        let title = format!("Canary · {name}{dirty}");
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
        // Meters and the playhead move every frame while audio runs; input events
        // repaint on their own, so a stopped app idles.
        let busy = self.view.playing || self.view.recording || !self.pending.is_empty();
        ctx.request_repaint_after(Duration::from_millis(if busy { 16 } else { 250 }));
    }
}

/// What of a track's routing the plan's shape depends on: id, bus, output, the
/// buses it sends to (levels are set live) and each device's sidechain.
type Routing = (u64, bool, Option<u64>, Vec<u64>, Vec<Option<u64>>);

fn routing(project: &daw_project::Project) -> Vec<Routing> {
    project
        .tracks
        .iter()
        .map(|t| {
            let keys = t.devices.iter().map(|d| d.sidechain).collect();
            (
                t.id,
                t.bus,
                t.output,
                t.sends.iter().map(|s| s.to).collect(),
                keys,
            )
        })
        .collect()
}

/// The metronome's beats as timeline samples (`data[0] == 1` on downbeats):
/// the pattern's in PAT mode, else the song's and 64 bars past its end.
fn metronome_clicks(project: &daw_project::Project, playback: Playback) -> Vec<SeqEvent> {
    let end = match playback {
        Playback::Pattern(id) => project.pattern(id).map_or(0, |p| p.length),
        Playback::Song => {
            let bar = project.song_end().max(1);
            let bars = project.bar_beat(bar).bar + 64;
            project.bar_tick(bars).min(daw_project::MAX_TICKS)
        }
    };
    project
        .beats(end)
        .into_iter()
        .map(|(tick, down)| SeqEvent {
            at: project.samples_at(tick),
            data: [u8::from(down), 0, 0],
        })
        .collect()
}

/// Where to put a playhead at `at` so it plays inside `looping`: `None` if it
/// already is (or there is no loop), else the same spot in the loop, wrapped.
fn into_loop(looping: Option<(u64, u64)>, at: u64) -> Option<u64> {
    let (start, end) = looping?;
    (at >= end).then(|| start + (at - start) % (end - start))
}

/// The transport loop for `playback`: the whole pattern in PAT mode.
fn loop_range(project: &daw_project::Project, playback: Playback) -> Option<(u64, u64)> {
    match playback {
        Playback::Pattern(id) => project
            .pattern(id)
            .map(|p| (0, project.samples_at(p.length).max(1))),
        Playback::Song => None,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let mut project = None;
    let mut samples = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--samples" => samples = args.next().map(PathBuf::from),
            _ => project = Some(PathBuf::from(arg)),
        }
    }
    let samples_dir = samples.unwrap_or_else(|| home().join("Music/Canary"));
    fs::create_dir_all(&samples_dir)?;
    let project_path = project.unwrap_or_else(|| samples_dir.join("Untitled.daw"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Canary")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1100.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Canary",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, project_path, samples_dir)?))),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playheads_past_a_loop_wrap_into_it() {
        assert_eq!(into_loop(None, 500), None);
        assert_eq!(into_loop(Some((0, 100)), 99), None);
        assert_eq!(into_loop(Some((0, 100)), 100), Some(0));
        assert_eq!(into_loop(Some((0, 100)), 250), Some(50));
        assert_eq!(into_loop(Some((40, 100)), 130), Some(70));
    }
}
