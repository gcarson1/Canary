//! Canary: the app. Wires the UI (`daw-ui`) to the engine (`daw-engine`) and the
//! project (`daw-project`): it turns the project into engine plans, loads audio
//! off the UI thread, records takes and saves the project.
//!
//! `canary [project.daw] [--samples DIR]`. Defaults to `~/Music/Canary`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{mpsc, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

use daw_core::{Clip, Command, NodeId, SampleData, Telemetry, TrackParams};
use daw_dsp::WaveformPyramid;
use daw_engine::{EngineHandle, Recorder};
use daw_graph::{ExecutionPlan, GainNode, Source, TrackNode, MAX_NODES};
use daw_project::{Action, ClipRef, History, Track};
use daw_ui::{theme, EngineView, Inputs, Request, Workspace};

/// Frames per plan buffer; larger device callbacks render in chunks.
const MAX_BLOCK: usize = 4096;
const AUDIO_EXTENSIONS: [&str; 7] = ["wav", "flac", "mp3", "m4a", "aac", "aif", "aiff"];

/// A finished background load, and where to put the clip if anywhere.
struct Loaded {
    path: PathBuf,
    result: Result<(Arc<SampleData>, WaveformPyramid), String>,
    place: Option<(Option<usize>, u64)>,
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
    loading: Vec<PathBuf>,
    loads_tx: mpsc::Sender<Loaded>,
    loads_rx: mpsc::Receiver<Loaded>,
    /// Project revision the live plan was built from.
    built: Option<u64>,
    live: Vec<NodeId>,
    recorder: Option<(Recorder, cpal::Stream)>,
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
        if history.project().tracks.is_empty()
            && history.project().sample_rate != config.sample_rate
        {
            // shortcut: positions in a non-empty project at another rate are not converted yet
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
            loading: Vec::new(),
            loads_tx,
            loads_rx,
            built: None,
            live: Vec::new(),
            recorder: None,
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
        if self.loading.contains(&path) && place.is_none() {
            return;
        }
        self.loading.push(path.clone());
        let (tx, rate) = (self.loads_tx.clone(), self.view.sample_rate);
        thread::spawn(move || {
            let result = daw_engine::load_audio(&path)
                .and_then(|a| daw_engine::resample(&a, rate))
                .map(|a| {
                    let w = daw_engine::cached_waveform(&path, a.clone(), &cache_dir());
                    (a, w)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Loaded {
                path,
                result,
                place,
            });
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
            eprintln!("could not add {}: {e}", path.display());
        }
    }

    fn receive_loads(&mut self) {
        while let Ok(done) = self.loads_rx.try_recv() {
            self.loading.retain(|p| p != &done.path);
            match done.result {
                Ok((audio, waveform)) => {
                    self.audio.insert(done.path.clone(), audio);
                    self.waveforms.insert(done.path.clone(), waveform);
                    self.built = None; // clips waiting on this audio can now play
                    if let Some((track, at)) = done.place {
                        self.place(&done.path, track, at);
                    }
                }
                Err(e) => eprintln!("could not load {}: {e}", done.path.display()),
            }
        }
    }

    /// Sends the engine a plan for the current project: one track node per track
    /// summed into a master. Node state resets on rebuild.
    fn rebuild(&mut self) {
        let project = self.history.project();
        let tracks = &project.tracks[..project.tracks.len().min(MAX_NODES - 1)];
        let mut plan = ExecutionPlan::new(MAX_BLOCK);
        for &id in &self.live {
            plan.remove(id);
        }
        let solo = Arc::new(AtomicUsize::new(0));
        let mut steps = Vec::with_capacity(tracks.len());
        for (i, track) in tracks.iter().enumerate() {
            let params = TrackParams::new(solo.clone());
            params.set_gain(track.gain);
            params.set_pan(track.pan);
            params.set_mute(track.mute);
            params.set_solo(track.solo);
            let clips = track
                .clips
                .iter()
                .filter_map(|c| {
                    let audio = self.audio.get(&c.source)?.clone();
                    Clip::with_window(c.start, audio, c.offset, c.length, c.fade_in, c.fade_out)
                })
                .collect();
            let id = NodeId(i as u32 + 1);
            plan.insert(id, Box::new(TrackNode::new(clips, Arc::new(params))));
            steps.push(plan.add(id, vec![], 2).expect("no inputs, two outputs"));
        }
        let master = NodeId(0);
        plan.insert(master, Box::new(GainNode { gain: 1.0 }));
        let sum = |channel| steps.iter().map(|&step| Source { step, channel }).collect();
        plan.add(master, vec![sum(0), sum(1)], 2)
            .expect("sources are earlier steps");
        match self.engine.send_plan(Box::new(plan)) {
            Ok(()) => {
                self.live = (0..=tracks.len() as u32).map(NodeId).collect();
                self.built = Some(self.history.revision());
            }
            // Queue full: try again next frame.
            Err(daw_engine::SendPlanError::QueueFull(_)) => {}
            Err(e) => eprintln!("plan rejected: {e}"),
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
        }
    }

    fn toggle_record(&mut self) {
        if self.recorder.is_none() {
            match daw_engine::start_input(self.view.sample_rate, self.engine.playhead_clock()) {
                Ok((recorder, stream, _)) => self.recorder = Some((recorder, stream)),
                Err(e) => return eprintln!("cannot record: {e}"),
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
                    eprintln!("take cut short: {error}");
                    take
                }
                Err(e) => return eprintln!("recording failed: {e}"),
            };
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
                Err(e) => eprintln!("cannot record: {e}"),
            }
        }
    }

    fn save(&mut self) {
        match self.history.save(&self.project_path) {
            Ok(()) => {}
            Err(e) => eprintln!("save failed: {e}"),
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

        let requests = self.ws.show(
            ui,
            Inputs {
                history: &mut self.history,
                engine: &self.view,
                waveforms: &self.waveforms,
                files: &self.files,
            },
        );
        for request in requests {
            self.handle(request);
        }
        if self.built != Some(self.history.revision()) {
            self.rebuild();
        }

        let name = self
            .project_path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let dirty = if self.history.is_dirty() {
            " (edited)"
        } else {
            ""
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
            "Canary · {name}{dirty}"
        )));
        // Meters and the playhead move every frame while audio runs; input events
        // repaint on their own, so a stopped app idles.
        let busy = self.view.playing || self.view.recording || !self.loading.is_empty();
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
