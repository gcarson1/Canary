//! Canary: the app. Wires the UI (`daw-ui`) to the engine (`daw-engine`) and the
//! project (`daw-project`): it turns the project into engine plans, loads audio
//! off the UI thread, records takes and saves the project.
//!
//! `canary [project.daw] [--samples DIR]`. Defaults to `~/Music/Canary`.

mod devices;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{mpsc, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

use daw_core::{Clip, Command, NodeId, SampleData, Telemetry, TrackParams};
use daw_dsp::WaveformPyramid;
use daw_engine::{EngineHandle, Recorder};
use daw_graph::{ExecutionPlan, GainNode, Source, TrackNode};
use daw_project::{Action, ClipRef, History, Track};
use daw_ui::{theme, EngineView, Inputs, Request, Status, Workspace};
use devices::Devices;

/// Frames per plan buffer; larger device callbacks render in chunks.
const MAX_BLOCK: usize = 4096;
const AUDIO_EXTENSIONS: [&str; 7] = ["wav", "flac", "mp3", "m4a", "aac", "aif", "aiff"];

/// A finished background load.
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
    /// A plan the engine's queue had no room for; sent before any new one.
    queued: Option<Box<ExecutionPlan>>,
    devices: Devices,
    /// Open only while a take records, so the microphone is not held between takes.
    recorder: Option<(Recorder, cpal::Stream)>,
    title: String,
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
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        })
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

        let (engine, output, config) = daw_engine::start_output(4096).map_err(|e| e.to_string())?;
        let mut history = if project_path.exists() {
            History::load(&project_path).map_err(|e| format!("{}: {e}", project_path.display()))?
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
            queued: None,
            devices: Devices::scan(),
            recorder: None,
            title: String::new(),
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
                Err(e) => self.error(format!("could not load {}: {e}", done.path.display())),
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
        let revision = self.history.revision();
        if self.built == Some(revision) {
            return;
        }
        let rate = self.view.sample_rate as f32;
        for e in self.devices.sync(self.history.project(), rate, MAX_BLOCK) {
            self.error(e);
        }
        let tracks = &self.history.project().tracks;
        let uids = |t: &Track| t.devices.iter().map(|d| d.uid).collect::<Vec<_>>();
        let same_shape = self.built.is_some()
            && !self.devices.changed()
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
            self.rebuild();
        }
    }

    fn set_params(&self) {
        for (track, params) in self.history.project().tracks.iter().zip(&self.params) {
            params.set_gain(track.gain);
            params.set_pan(track.pan);
            params.set_mute(track.mute);
            params.set_solo(track.solo);
        }
    }

    /// Sends the engine a plan for the current project: per track, its instrument
    /// (if any) into a track node into its effects, all summed into a master.
    /// Track nodes are rebuilt; plugin nodes persist and keep their state.
    fn rebuild(&mut self) {
        let count = self
            .history
            .project()
            .tracks
            .len()
            .min(devices::FIRST_DEVICE_NODE as usize - 1);
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
        let tracks = &self.history.project().tracks[..count];
        let mut steps = Vec::with_capacity(count);
        for (i, track) in tracks.iter().enumerate() {
            let clips = track
                .clips
                .iter()
                .filter_map(|c| {
                    let audio = self.audio.get(&c.source)?.clone();
                    Clip::with_window(c.start, audio, c.offset, c.length, c.fade_in, c.fade_out)
                })
                .collect();
            let id = NodeId(i as u32 + 1);
            plan.insert(id, Box::new(TrackNode::new(clips, self.params[i].clone())));
            let inputs = match self.devices.instrument_step(&mut plan, track) {
                Some(step) => (0..2)
                    .map(|channel| vec![Source { step, channel }])
                    .collect(),
                None => vec![],
            };
            let track_step = plan.add(id, inputs, 2).expect("inputs are earlier steps");
            steps.push(self.devices.effects_after(&mut plan, track, track_step));
        }
        let master = NodeId(0);
        plan.insert(master, Box::new(GainNode { gain: 1.0 }));
        let sum = |channel| steps.iter().map(|&step| Source { step, channel }).collect();
        plan.add(master, vec![sum(0), sum(1)], 2)
            .expect("sources are earlier steps");
        self.live = (0..=count as u32).map(NodeId).collect();
        self.built_clips = tracks.iter().map(|t| t.clips.clone()).collect();
        self.built_devices = tracks
            .iter()
            .map(|t| t.devices.iter().map(|d| d.uid).collect())
            .collect();
        self.built = Some(self.history.revision());
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
        self.view.recording = self
            .recorder
            .as_ref()
            .is_some_and(|(r, _)| r.is_recording());
    }

    fn handle(&mut self, request: Request) {
        let command = |app: &mut Self, c| {
            if let Err(e) = app.engine.commands.push(c) {
                eprintln!("engine busy, dropped {e:?}");
            }
        };
        match request {
            Request::Play => command(self, Command::Play),
            Request::Stop => command(self, Command::Stop),
            Request::Seek(at) => command(self, Command::Seek(at)),
            Request::AddFile { track, path, at } => self.load(path, Some((track, at))),
            Request::ToggleRecord => self.toggle_record(),
            Request::AddDevice { track, choice } => {
                let uid = self.history.new_device_uid();
                let rate = self.view.sample_rate as f32;
                match self.devices.create(choice, uid, rate, MAX_BLOCK) {
                    Ok(device) => {
                        let tracks = &self.history.project().tracks;
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
                let uid = self
                    .history
                    .project()
                    .tracks
                    .get(track)
                    .and_then(|t| t.devices.get(device))
                    .map(|d| d.uid);
                if let Some(uid) = uid {
                    self.devices.set_param(uid, param, value);
                }
            }
        }
    }

    fn toggle_record(&mut self) {
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
            self.load(take.path, Some((self.ws.selected_track, take.start_sample)));
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
    fn save(&mut self) -> bool {
        for (uid, state) in self.devices.states() {
            self.history.store_device_state(uid, state);
        }
        match self.history.save(&self.project_path) {
            Ok(()) => {
                self.ws.status = Some(Status {
                    text: "SAVED".into(),
                    error: false,
                });
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
        self.read_telemetry();
        self.engine.collect_garbage();
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
            self.save();
        }
        // Quitting saves edits; if that fails the window stays open with the error.
        if ctx.input(|i| i.viewport().close_requested()) && self.history.is_dirty() && !self.save()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }

        self.devices.idle();
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
            },
        );
        for request in requests {
            self.handle(request);
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
