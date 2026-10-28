//! macOS DAW UI per Styles.md: the 4-zone shell (top bar and transport, asset
//! browser, playlist timeline over the DSP rack, sound manifest over the master
//! dock), drawn with egui. Edits go into `daw_project::History`; anything the
//! engine must do comes back as `Request`s. This crate never touches audio.

mod rack;
pub mod theme;
mod timeline;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use daw_dsp::WaveformPyramid;
use daw_project::{Action, History, Track, TrackProps};
use egui::{Align2, Color32, Key, Pos2, Rect, Sense, Shape, StrokeKind, Vec2};

pub use rack::{DeviceView, ParamView, PluginChoice, SampleDrop, SamplerView};
use theme::Metrics;
pub use theme::Theme;

/// What the engine is doing, read from telemetry each frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineView {
    pub playhead: u64,
    pub playing: bool,
    pub recording: bool,
    pub sample_rate: u32,
    /// Master peak and RMS, linear, left and right.
    pub peak: [f32; 2],
    pub rms: [f32; 2],
    /// Mix loudness, LUFS: momentary, short-term, integrated. -inf when silent.
    pub loudness: [f32; 3],
    /// Highest true peak since the meters were reset, dBTP.
    pub true_peak: f32,
    /// Stereo correlation, -1 to +1.
    pub correlation: f32,
    pub listen: Listen,
    /// The reference plays instead of the mix.
    pub reference_on: bool,
}

/// What the master outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Listen {
    #[default]
    Stereo,
    Mid,
    Side,
}

impl Default for EngineView {
    fn default() -> Self {
        Self {
            playhead: 0,
            playing: false,
            recording: false,
            sample_rate: 48_000,
            peak: [0.0; 2],
            rms: [0.0; 2],
            loudness: [f32::NEG_INFINITY; 3],
            true_peak: f32::NEG_INFINITY,
            correlation: 0.0,
            listen: Listen::Stereo,
            reference_on: false,
        }
    }
}

/// Things only the app can do: drive the engine or load audio.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Play,
    Stop,
    Seek(u64),
    ToggleRecord,
    /// Load `path` and place it at `at` on `track`, or on a new track if `None`.
    AddFile {
        track: Option<usize>,
        path: PathBuf,
        at: u64,
    },
    /// Add `Inputs::plugins[choice]` to the end of `track`'s devices.
    AddDevice {
        track: usize,
        choice: usize,
    },
    /// Set a parameter on `track`'s device `device` (its index in the track).
    SetParam {
        track: usize,
        device: usize,
        param: u32,
        value: f64,
    },
    /// A file dropped on a sampler device.
    LoadSample {
        track: usize,
        device: usize,
        path: PathBuf,
        drop: SampleDrop,
    },
    /// Show the plugin's own editor window.
    OpenEditor {
        track: usize,
        device: usize,
    },
    Listen(Listen),
    /// Switch between the mix and the reference (A/B).
    ToggleReference,
    /// Clear integrated loudness and the true-peak hold.
    ResetMeters,
    /// Add a slice point at `at` (a fraction of the sample), or remove the nearest.
    SliceMarker {
        track: usize,
        device: usize,
        at: f64,
        add: bool,
    },
}

/// Everything the UI reads besides its own state.
pub struct Inputs<'a> {
    pub history: &'a mut History,
    pub engine: &'a EngineView,
    /// Pyramids of loaded audio, by source path. Clips whose audio is missing draw
    /// without a waveform and cannot be trimmed.
    pub waveforms: &'a HashMap<PathBuf, WaveformPyramid>,
    /// Files listed in the asset browser.
    pub files: &'a [PathBuf],
    /// Plugins the device picker offers.
    pub plugins: &'a [PluginChoice],
    /// The selected track's devices, in order, with live parameter values.
    pub devices: &'a [DeviceView],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Draw,
    Slice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snap {
    Sixteenth,
    Eighth,
    Bar,
    Off,
}

/// A project change made while drawing, applied once the frame is drawn.
enum Edit {
    Apply(Action),
    Undo,
    Redo,
}

/// One line of app status in the top bar, such as a failed save.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub text: String,
    /// Shown in `bad` red; otherwise muted.
    pub error: bool,
}

/// UI state that is not part of the project: view, tools, selection, drags.
pub struct Workspace {
    pub theme: Theme,
    pub tool: Tool,
    pub snap: Snap,
    pub selected_track: Option<usize>,
    pub selected_clip: Option<(usize, usize)>,
    pub status: Option<Status>,
    timeline: timeline::View,
    /// Tempo while its readout is being dragged; applied on release.
    tempo_drag: Option<f64>,
    /// Clip indicator per side, latched until clicked.
    clip_latch: [bool; 2],
    edits: Vec<Edit>,
    /// The device picker's scroll offset while it is open.
    picker: Option<f32>,
    /// The rack device being dragged to a new slot.
    device_drag: Option<usize>,
    /// An asset browser file being dragged (onto a sampler).
    file_drag: Option<PathBuf>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            theme: Theme::DARK,
            tool: Tool::Draw,
            snap: Snap::Sixteenth,
            selected_track: None,
            selected_clip: None,
            status: None,
            timeline: timeline::View::default(),
            tempo_drag: None,
            clip_latch: [false; 2],
            edits: Vec::new(),
            picker: None,
            device_drag: None,
            file_drag: None,
        }
    }
}

/// Zone rectangles for a window, per Styles.md section 3.
struct Zones {
    top: Rect,
    browser: Rect,
    preview: Rect,
    tools: Rect,
    timeline: Rect,
    rack: Rect,
    manifest: Rect,
    dock: Rect,
}

impl Zones {
    fn new(r: Rect, m: &Metrics) -> Self {
        let u = m.u;
        let snap = |v: f32| m.snap(v) - m.px / 2.0;
        let top = Rect::from_min_max(r.min, Pos2::new(r.max.x, snap(r.min.y + 3.5 * u)));
        let body_h = r.max.y - top.max.y;
        // The rack and dock are 25u tall when the window allows, else under half.
        let lower = (25.0 * u).min(body_h * 0.45);
        let split = snap(r.max.y - lower);
        let left = snap(r.min.x + 17.5 * u);
        let right = snap(r.max.x - 21.25 * u);
        let tools_bottom = snap(top.max.y + 2.5 * u);
        let preview_top = snap(r.max.y - (10.0 * u).min(body_h * 0.3));
        Zones {
            top,
            browser: Rect::from_x_y_ranges(r.min.x..=left, top.max.y..=preview_top),
            preview: Rect::from_x_y_ranges(r.min.x..=left, preview_top..=r.max.y),
            tools: Rect::from_x_y_ranges(left..=right, top.max.y..=tools_bottom),
            timeline: Rect::from_x_y_ranges(left..=right, tools_bottom..=split),
            rack: Rect::from_x_y_ranges(left..=right, split..=r.max.y),
            manifest: Rect::from_x_y_ranges(right..=r.max.x, top.max.y..=split),
            dock: Rect::from_x_y_ranges(right..=r.max.x, split..=r.max.y),
        }
    }
}

/// Lowercase-free technical header (`01 // ASSET_BROWSER`) at the top of a zone.
fn header(p: &egui::Painter, m: &Metrics, t: &Theme, zone: Rect, text: &str) -> f32 {
    let h = 2.0 * m.u;
    p.text(
        Pos2::new(zone.min.x + 0.75 * m.u, zone.min.y + h / 2.0),
        Align2::LEFT_CENTER,
        text,
        m.mono(0.6875),
        t.muted,
    );
    let y = m.snap(zone.min.y + h);
    p.hline(zone.x_range(), y, m.hair(t.line));
    y
}

impl Workspace {
    /// Draws one frame into `ui` and applies the edits it makes. Returns what the
    /// app must do. Call [`theme::install_fonts`] once on the context first.
    pub fn show(&mut self, ui: &mut egui::Ui, inputs: Inputs<'_>) -> Vec<Request> {
        let mut out = Vec::new();
        let full = ui.max_rect();
        let m = Metrics::new(full.width(), ui.ctx().pixels_per_point());
        let t = self.theme;
        if ui.visuals().panel_fill != t.bg_void {
            theme::apply(ui.ctx(), &t);
        }
        let z = Zones::new(full, &m);
        let p = ui.painter().clone();

        for (zone, fill) in [
            (z.top, t.bg_panel),
            (z.browser, t.bg_panel),
            (z.preview, t.bg_panel),
            (z.tools, t.bg_void),
            (z.timeline, t.bg_void),
            (z.rack, t.bg_rack),
            (z.manifest, t.bg_panel),
            (z.dock, t.bg_rack),
        ] {
            p.rect_filled(zone, 0.0, fill);
        }
        // Full-bleed hairlines between zones; nothing is boxed.
        p.hline(full.x_range(), m.snap(z.top.max.y), m.hair(t.line));
        for x in [z.browser.max.x, z.manifest.min.x] {
            p.vline(m.snap(x), z.top.max.y..=full.max.y, m.hair(t.line));
        }
        p.hline(z.tools.x_range(), m.snap(z.tools.max.y), m.hair(t.line));
        p.hline(z.rack.x_range(), m.snap(z.rack.min.y), m.hair(t.line_2));
        p.hline(z.dock.x_range(), m.snap(z.dock.min.y), m.hair(t.line_2));
        p.hline(z.preview.x_range(), m.snap(z.preview.min.y), m.hair(t.line));

        self.keys(ui, &inputs, &mut out);
        self.top_bar(ui, &p, &m, z.top, &inputs, &mut out);
        self.browser(ui, &p, &m, z.browser, &inputs, &mut out);
        let y = header(&p, &m, &t, z.preview, "SPECTRAL_PREVIEW");
        let well = Rect::from_x_y_ranges(z.preview.x_range(), y..=z.preview.max.y);
        p.text(
            well.center(),
            Align2::CENTER_CENTER,
            "NO SIGNAL",
            m.mono(0.625),
            t.faint,
        );
        self.tool_strip(ui, &p, &m, z.tools);
        timeline::show(self, ui, &p, &m, z.timeline, &inputs, &mut out);
        rack::show(self, ui, &p, &m, z.rack, &inputs, &mut out);
        // A module removed mid-drag never sees its drag stop.
        if !ui.input(|i| i.pointer.any_down()) {
            self.device_drag = None;
        }
        self.manifest(ui, &p, &m, z.manifest, inputs.history, inputs.engine);
        self.dock(ui, &p, &m, z.dock, &inputs, &mut out);
        if let Some(path) = &self.file_drag {
            // The file follows the pointer; the rack takes the drop.
            if let Some(at) = ui.input(|i| i.pointer.hover_pos()) {
                let fp = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    ui.scope_id().with("file-drag"),
                ));
                fp.text(
                    at + Vec2::new(0.75 * m.u, 0.0),
                    Align2::LEFT_CENTER,
                    file_name(path),
                    m.mono(0.6875),
                    t.accent_amber,
                );
            }
            if ui.input(|i| i.pointer.any_released()) {
                self.file_drag = None;
            }
        }

        for edit in std::mem::take(&mut self.edits) {
            match edit {
                // A stale edit (its target was removed this frame) just does nothing.
                Edit::Apply(action) => drop(inputs.history.apply(action)),
                Edit::Undo => drop(inputs.history.undo()),
                Edit::Redo => drop(inputs.history.redo()),
            }
        }
        self.clear_selection(inputs.history);
        out
    }

    /// True while a clip is being dragged; the app holds back its own edits then.
    pub fn is_dragging(&self) -> bool {
        self.timeline.is_dragging() || self.device_drag.is_some() || self.file_drag.is_some()
    }

    /// Queues a project edit for the end of the frame.
    fn edit(&mut self, action: Action) {
        self.edits.push(Edit::Apply(action));
    }

    fn keys(&mut self, ui: &egui::Ui, inputs: &Inputs<'_>, out: &mut Vec<Request>) {
        // Text fields own the keyboard while focused.
        if ui.ctx().text_edit_focused() {
            return;
        }
        // Space on a focused button presses that button; it must not also toggle
        // the transport.
        let button_focused = ui.ctx().memory(|mem| mem.focused().is_some());
        let (space, undo, redo, delete) = ui.input(|i| {
            let cmd = i.modifiers.command;
            // Space and Delete act once per press: a held key must not toggle the
            // transport or delete one track after another.
            let fresh = |k: Key| {
                i.events.iter().any(|e| {
                    matches!(e, egui::Event::Key { key, pressed: true, repeat: false, .. } if *key == k)
                })
            };
            (
                !button_focused && fresh(Key::Space),
                cmd && !i.modifiers.shift && i.key_pressed(Key::Z),
                cmd && i.modifiers.shift && i.key_pressed(Key::Z),
                fresh(Key::Delete) || fresh(Key::Backspace),
            )
        });
        if space {
            out.push(if inputs.engine.playing {
                Request::Stop
            } else {
                Request::Play
            });
        }
        if undo {
            self.edits.push(Edit::Undo);
        }
        if redo {
            self.edits.push(Edit::Redo);
        }
        if delete {
            let action = match (self.selected_clip, self.selected_track) {
                (Some((track, index)), _) => Some(Action::RemoveClip { track, index }),
                (None, Some(index)) => Some(Action::RemoveTrack { index }),
                _ => None,
            };
            if let Some(action) = action {
                self.edit(action);
                self.selected_clip = None;
                self.selected_track = None;
            }
        }
    }

    /// Drops selections that no longer point at anything.
    fn clear_selection(&mut self, h: &History) {
        let tracks = &h.project().tracks;
        if self.selected_track.is_some_and(|i| i >= tracks.len()) {
            self.selected_track = None;
        }
        if self
            .selected_clip
            .is_some_and(|(t, c)| tracks.get(t).is_none_or(|t| c >= t.clips.len()))
        {
            self.selected_clip = None;
        }
    }

    fn top_bar(
        &mut self,
        ui: &egui::Ui,
        p: &egui::Painter,
        m: &Metrics,
        r: Rect,
        inputs: &Inputs<'_>,
        out: &mut Vec<Request>,
    ) {
        let (t, u, cy) = (self.theme, m.u, r.center().y);
        let mut x = r.min.x + u;
        p.rect_filled(
            Rect::from_center_size(Pos2::new(x + 0.25 * u, cy), Vec2::splat(0.5 * u)),
            0.0,
            t.accent_amber,
        );
        x += u;
        let name = p.text(
            Pos2::new(x, cy),
            Align2::LEFT_CENTER,
            "CANARY",
            m.mono_bold(0.8125),
            t.text,
        );
        x = name.max.x + 1.5 * u;
        // The only view built so far; its tab carries the 2pt active bar.
        let tab = p.text(
            Pos2::new(x, cy),
            Align2::LEFT_CENTER,
            "PLAYLIST",
            m.sans_bold(0.75),
            t.text,
        );
        p.rect_filled(
            Rect::from_x_y_ranges(tab.x_range(), (r.max.y - 0.125 * u)..=r.max.y),
            0.0,
            t.accent_amber,
        );

        // Center: tempo, clock, transport.
        let engine = inputs.engine;
        let bpm = self.tempo_drag.unwrap_or(inputs.history.project().bpm);
        let mut x = r.center().x - 12.0 * u;
        let bpm_rect = p.text(
            Pos2::new(x, cy),
            Align2::LEFT_CENTER,
            format!("{bpm:6.2}"),
            m.mono(1.125),
            t.text,
        );
        let bpm_resp = ui.interact(
            bpm_rect.expand(0.25 * u),
            ui.scope_id().with("bpm"),
            Sense::drag(),
        );
        if bpm_resp.dragged() {
            let dy = bpm_resp.drag_delta().y as f64;
            self.tempo_drag = Some((bpm - dy * 0.25).clamp(20.0, 400.0));
        }
        if bpm_resp.drag_stopped() {
            if let Some(bpm) = self.tempo_drag.take() {
                self.edit(Action::SetTempo((bpm * 100.0).round() / 100.0));
            }
        }
        x = bpm_rect.max.x + 0.375 * u;
        let label = p.text(
            Pos2::new(x, cy),
            Align2::LEFT_CENTER,
            "BPM",
            m.mono(0.6875),
            t.faint,
        );
        x = label.max.x + 1.5 * u;
        let (bar, beat, step) = timeline::bar_beat_step(engine.playhead, bpm, engine.sample_rate);
        let clock = p.text(
            Pos2::new(x, cy),
            Align2::LEFT_CENTER,
            format!("{bar:03} : {beat:02} : {step:02}"),
            m.mono(1.125),
            t.text,
        );
        x = clock.max.x + 1.5 * u;

        let side = 2.25 * u;
        let button = |x: f32, id: &str| {
            let rect = m.snap_rect(Rect::from_min_size(
                Pos2::new(x, cy - side / 2.0),
                Vec2::splat(side),
            ));
            (
                rect,
                ui.interact(rect, ui.scope_id().with(id), Sense::click()),
            )
        };
        let (stop, stop_resp) = button(x, "stop");
        p.rect_stroke(stop, 0.0, m.hair(t.line_2), StrokeKind::Inside);
        p.rect_filled(
            Rect::from_center_size(stop.center(), Vec2::splat(0.625 * u)),
            0.0,
            t.muted,
        );
        if stop_resp.clicked() {
            out.push(Request::Stop);
        }
        let (play, play_resp) = button(stop.max.x + 0.5 * u, "play");
        let (fill, icon) = if engine.playing {
            (t.ink, t.ink_text)
        } else {
            (t.bg_panel, t.text)
        };
        p.rect_filled(play, 0.0, fill);
        p.rect_stroke(
            play,
            0.0,
            m.hair(if engine.playing { t.ink } else { t.line_2 }),
            StrokeKind::Inside,
        );
        let c = play.center();
        let s = 0.375 * u;
        p.add(Shape::convex_polygon(
            vec![
                Pos2::new(c.x - s * 0.8, c.y - s),
                Pos2::new(c.x + s, c.y),
                Pos2::new(c.x - s * 0.8, c.y + s),
            ],
            icon,
            egui::Stroke::NONE,
        ));
        if play_resp.clicked() {
            out.push(if engine.playing {
                Request::Stop
            } else {
                Request::Play
            });
        }
        let (rec, rec_resp) = button(play.max.x + 0.5 * u, "rec");
        if engine.recording {
            p.rect_filled(rec, 0.0, t.bad);
            p.circle_filled(rec.center(), 0.3125 * u, t.ink_text);
        } else {
            p.rect_stroke(rec, 0.0, m.hair(t.line_2), StrokeKind::Inside);
            p.circle_filled(rec.center(), 0.3125 * u, t.muted);
        }
        if rec_resp.clicked() {
            out.push(Request::ToggleRecord);
        }

        // Right: device rate and the dirty-project square.
        let dirty = Rect::from_center_size(Pos2::new(r.max.x - 1.25 * u, cy), Vec2::splat(0.5 * u));
        if inputs.history.is_dirty() {
            p.rect_filled(dirty, 0.0, t.text);
        } else {
            p.rect_stroke(dirty, 0.0, m.hair(t.line_2), StrokeKind::Inside);
        }
        let rate = p.text(
            Pos2::new(dirty.min.x - 0.75 * u, cy),
            Align2::RIGHT_CENTER,
            format!("{:.1} kHz", engine.sample_rate as f32 / 1000.0),
            m.mono(0.75),
            t.muted,
        );
        if let Some(status) = &self.status {
            let color = if status.error { t.bad } else { t.muted };
            p.text(
                Pos2::new(rate.min.x - 1.5 * u, cy),
                Align2::RIGHT_CENTER,
                &status.text,
                m.mono(0.6875),
                color,
            );
        }
    }

    fn browser(
        &mut self,
        ui: &egui::Ui,
        p: &egui::Painter,
        m: &Metrics,
        r: Rect,
        inputs: &Inputs<'_>,
        out: &mut Vec<Request>,
    ) {
        let (t, u) = (self.theme, m.u);
        let mut y = header(p, m, &t, r, "01 // ASSET_BROWSER");
        if inputs.files.is_empty() {
            p.text(
                Pos2::new(r.min.x + 0.75 * u, y + u),
                Align2::LEFT_CENTER,
                "NO AUDIO FILES IN FOLDER",
                m.mono(0.625),
                t.faint,
            );
        }
        let row_h = 2.0 * u;
        for (i, path) in inputs.files.iter().enumerate() {
            if y + row_h > r.max.y {
                break;
            }
            let row = Rect::from_x_y_ranges(r.x_range(), y..=m.snap(y + row_h));
            let resp = ui.interact(
                row,
                ui.scope_id().with(("file", i)),
                Sense::click_and_drag(),
            );
            if resp.drag_started() {
                self.file_drag = Some(path.clone());
            }
            let name = file_name(path);
            let label = p.text(
                Pos2::new(r.min.x + 0.75 * u, row.center().y),
                Align2::LEFT_CENTER,
                name,
                m.sans(0.75),
                if resp.hovered() { t.text } else { t.muted },
            );
            if resp.hovered() {
                p.hline(label.x_range(), m.snap(label.max.y), m.hair(t.text));
            }
            let nudge = if resp.hovered() { 0.25 * u } else { 0.0 };
            p.text(
                Pos2::new(r.max.x - 0.75 * u + nudge - 0.25 * u, row.center().y),
                Align2::RIGHT_CENTER,
                "→",
                m.mono(0.75),
                t.faint,
            );
            p.hline(r.x_range(), m.snap(row.max.y), m.hair(t.line));
            if resp.clicked() {
                out.push(Request::AddFile {
                    track: self.selected_track,
                    path: path.clone(),
                    at: timeline::snap_f(
                        inputs.engine.playhead as f64,
                        self.snap,
                        inputs.history.project().bpm,
                        inputs.engine.sample_rate,
                    ) as u64,
                });
            }
            y = row.max.y;
        }
    }

    fn tool_strip(&mut self, ui: &egui::Ui, p: &egui::Painter, m: &Metrics, r: Rect) {
        let (t, u, cy) = (self.theme, m.u, r.center().y);
        let mut x = r.min.x + 0.75 * u;
        let seg = |x: f32, label: &str, active: bool, id: &str| -> (f32, bool) {
            let g = p.layout_no_wrap(label.into(), m.mono(0.6875), t.text);
            let rect = m.snap_rect(Rect::from_min_size(
                Pos2::new(x, cy - 0.75 * u),
                Vec2::new(g.size().x + u, 1.5 * u),
            ));
            let resp = ui.interact(rect, ui.scope_id().with(id), Sense::click());
            if active {
                p.rect_filled(rect, 0.0, t.accent_amber);
            } else {
                p.rect_stroke(
                    rect,
                    0.0,
                    m.hair(if resp.hovered() { t.text } else { t.line_2 }),
                    StrokeKind::Inside,
                );
            }
            let color = if active { t.ink_text } else { t.muted };
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                label,
                m.mono(0.6875),
                color,
            );
            (rect.max.x, resp.clicked())
        };
        for (tool, label) in [(Tool::Draw, "DRAW"), (Tool::Slice, "SLICE")] {
            let (end, clicked) = seg(x, label, self.tool == tool, label);
            if clicked {
                self.tool = tool;
            }
            x = end + 0.25 * u;
        }
        x += 0.75 * u;
        let label = p.text(
            Pos2::new(x, cy),
            Align2::LEFT_CENTER,
            "GRID SNAP",
            m.mono(0.625),
            t.faint,
        );
        x = label.max.x + 0.5 * u;
        for (snap, label) in [
            (Snap::Sixteenth, "1/16"),
            (Snap::Eighth, "1/8"),
            (Snap::Bar, "BAR"),
            (Snap::Off, "OFF"),
        ] {
            let (end, clicked) = seg(x, label, self.snap == snap, label);
            if clicked {
                self.snap = snap;
            }
            x = end + 0.25 * u;
        }
    }

    fn manifest(
        &mut self,
        ui: &egui::Ui,
        p: &egui::Painter,
        m: &Metrics,
        r: Rect,
        history: &History,
        engine: &EngineView,
    ) {
        let (t, u) = (self.theme, m.u);
        let mut y = header(p, m, &t, r, "03 // SOUND_MANIFEST");
        let row_h = 2.875 * u;
        let tracks = &history.project().tracks;
        let mut edit = None;
        for (i, track) in tracks.iter().enumerate() {
            if y + row_h > r.max.y - 2.0 * u {
                break;
            }
            let row = Rect::from_x_y_ranges(r.x_range(), y..=m.snap(y + row_h));
            let resp = ui.interact(row, ui.scope_id().with(("track", i)), Sense::click());
            if self.selected_track == Some(i) {
                p.rect_filled(row, 0.0, t.sel);
                p.rect_filled(
                    Rect::from_x_y_ranges(row.min.x..=row.min.x + 0.1875 * u, row.y_range()),
                    0.0,
                    t.accent_amber,
                );
            }
            let cy = row.center().y;
            let mut x = row.min.x + 0.75 * u;
            p.text(
                Pos2::new(x, cy),
                Align2::LEFT_CENTER,
                format!("{:02}", i + 1),
                m.mono(0.75),
                t.faint,
            );
            x += 1.75 * u;
            let led = m.snap_rect(Rect::from_min_size(
                Pos2::new(x, cy - 0.375 * u),
                Vec2::splat(0.75 * u),
            ));
            if timeline::sounding(track, engine) {
                p.rect_filled(led, 0.0, t.good);
            } else if track.mute {
                // Muted: dashed border (section 4.4).
                let r = led.shrink(m.px / 2.0);
                let ring = [
                    r.left_top(),
                    r.right_top(),
                    r.right_bottom(),
                    r.left_bottom(),
                    r.left_top(),
                ];
                p.extend(Shape::dashed_line(
                    &ring,
                    m.hair(t.line_2),
                    2.0 * m.px,
                    2.0 * m.px,
                ));
            } else {
                p.rect_filled(led, 0.0, t.bg_void);
                p.rect_stroke(led, 0.0, m.hair(t.line_2), StrokeKind::Inside);
            }
            x = led.max.x + 0.75 * u;
            let name_color = if track.mute { t.faint } else { t.text };
            p.text(
                Pos2::new(x, cy),
                Align2::LEFT_CENTER,
                &track.name,
                m.sans(0.8125),
                name_color,
            );

            // Right side: M, S, bus badge.
            let badge = p.text(
                Pos2::new(row.max.x - 0.75 * u, cy),
                Align2::RIGHT_CENTER,
                format!("M{:02}", i + 1),
                m.mono(0.6875),
                t.muted,
            );
            let side = 1.25 * u;
            let toggle = |right: f32, label: &str, on: bool, fill: Color32| {
                let rect = m.snap_rect(Rect::from_min_size(
                    Pos2::new(right - side, cy - side / 2.0),
                    Vec2::splat(side),
                ));
                let resp = ui.interact(rect, ui.scope_id().with((label, i)), Sense::click());
                if on {
                    p.rect_filled(rect, 0.0, fill);
                } else {
                    p.rect_stroke(rect, 0.0, m.hair(t.line_2), StrokeKind::Inside);
                }
                let color = if on { t.ink_text } else { t.muted };
                p.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    label,
                    m.mono(0.625),
                    color,
                );
                (rect.min.x, resp.clicked())
            };
            let (left, solo) = toggle(badge.min.x - 0.75 * u, "S", track.solo, t.accent_amber);
            let (_, mute) = toggle(left - 0.25 * u, "M", track.mute, t.ink);
            if mute || solo {
                let mut props = TrackProps::of(track);
                props.mute ^= mute;
                props.solo ^= solo;
                edit = Some(Action::SetTrackProps { index: i, props });
            } else if resp.clicked() {
                self.selected_track = Some(i);
                self.selected_clip = None;
            }
            p.hline(r.x_range(), m.snap(row.max.y), m.hair(t.line));
            y = row.max.y;
        }
        // Add-track text action under the rows.
        let add = p.text(
            Pos2::new(r.min.x + 0.75 * u, y + u),
            Align2::LEFT_CENTER,
            "+ TRACK",
            m.mono(0.6875),
            t.muted,
        );
        let resp = ui.interact(
            add.expand(0.25 * u),
            ui.scope_id().with("add-track"),
            Sense::click(),
        );
        if resp.hovered() {
            p.hline(add.x_range(), m.snap(add.max.y), m.hair(t.muted));
        }
        if resp.clicked() {
            let index = tracks.len();
            edit = Some(Action::InsertTrack {
                index,
                track: Track::new(format!("Track {}", index + 1)),
            });
            self.selected_track = Some(index);
        }
        if let Some(action) = edit {
            self.edit(action);
        }
        if tracks.is_empty() {
            p.text(
                Pos2::new(r.min.x + 0.75 * u, y + 2.5 * u),
                Align2::LEFT_CENTER,
                "NO CHANNELS LOADED · PICK A FILE IN 01",
                m.mono(0.625),
                t.faint,
            );
        }
    }

    fn dock(
        &mut self,
        ui: &egui::Ui,
        p: &egui::Painter,
        m: &Metrics,
        r: Rect,
        inputs: &Inputs<'_>,
        out: &mut Vec<Request>,
    ) {
        let engine = inputs.engine;
        let (t, u) = (self.theme, m.u);
        let y = header(p, m, &t, r, "04 // CONSOLE=80 MASTER_BUS");
        let body = Rect::from_x_y_ranges(
            r.min.x + 0.75 * u..=r.max.x - 0.75 * u,
            y + 0.75 * u..=r.max.y - 0.75 * u,
        );
        for side in 0..2 {
            if engine.peak[side] >= 1.0 {
                self.clip_latch[side] = true;
            }
        }
        // Monitor strip along the bottom: what you hear, and the A/B reference.
        let strip_y = body.max.y - 1.5 * u;
        let mut x = body.min.x;
        let seg = |x: f32, label: &str, active: bool, id: &str| {
            let g = p.layout_no_wrap(label.into(), m.mono(0.625), t.text);
            let rect = m.snap_rect(Rect::from_min_size(
                Pos2::new(x, strip_y),
                Vec2::new(g.size().x + 0.75 * u, 1.5 * u),
            ));
            let resp = ui.interact(rect, ui.scope_id().with(id), Sense::click());
            if active {
                p.rect_filled(rect, 0.0, t.accent_amber);
            } else {
                let line = if resp.hovered() { t.text } else { t.line_2 };
                p.rect_stroke(rect, 0.0, m.hair(line), StrokeKind::Inside);
            }
            let color = if active { t.ink_text } else { t.muted };
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                label,
                m.mono(0.625),
                color,
            );
            (rect.max.x, resp)
        };
        for (listen, label) in [
            (Listen::Stereo, "ST"),
            (Listen::Mid, "MID"),
            (Listen::Side, "SIDE"),
        ] {
            let (end, resp) = seg(x, label, engine.listen == listen, label);
            if resp.clicked() {
                out.push(Request::Listen(listen));
            }
            x = end + 0.25 * u;
        }
        let reference = inputs.history.project().reference.as_deref();
        let (end, resp) = seg(x + 0.5 * u, "A/B REF", engine.reference_on, "ab-ref");
        if resp.clicked() && reference.is_some() {
            out.push(Request::ToggleReference);
        }
        if resp.secondary_clicked() && reference.is_some() {
            self.edit(Action::SetReference(None));
        }
        let name = reference.map_or_else(|| "DROP REFERENCE FROM 01".into(), file_name);
        let meters_left = body.max.x - 2.0 * 1.375 * u - 2.0 * u;
        p.with_clip_rect(Rect::from_x_y_ranges(end..=meters_left, r.y_range()))
            .text(
                Pos2::new(end + 0.5 * u, strip_y + 0.75 * u),
                Align2::LEFT_CENTER,
                name.to_uppercase(),
                m.mono(0.5625),
                t.faint,
            );
        // A file dropped on the dock becomes the reference.
        let hovered = ui
            .input(|i| i.pointer.hover_pos())
            .is_some_and(|at| r.contains(at));
        if let Some(path) = self.file_drag.clone().filter(|_| hovered) {
            p.rect_stroke(
                r.shrink(1.0),
                0.0,
                egui::Stroke::new(0.125 * u, t.accent_amber),
                StrokeKind::Inside,
            );
            if ui.input(|i| i.pointer.any_released()) {
                self.edit(Action::SetReference(Some(path)));
            }
        }

        // Giant stacked readouts on the left: integrated loudness (click to
        // reset), true-peak hold, then the short windows and correlation.
        let lufs = |v: f32| {
            if v.is_finite() {
                format!("{v:.1}")
            } else {
                "-INF".into()
            }
        };
        let mut ty = body.min.y;
        let size = ((strip_y - body.min.y) / u / 2.0 - 2.0).clamp(1.5, 4.25);
        for (label, value, id) in [
            ("LUFS_I // CLICK RESETS", lufs(engine.loudness[2]), "lufs-i"),
            ("TRUE_PEAK_DBTP", lufs(engine.true_peak), "true-peak"),
        ] {
            p.text(
                Pos2::new(body.min.x, ty),
                Align2::LEFT_TOP,
                label,
                m.mono(0.625),
                t.muted,
            );
            let g = p.text(
                Pos2::new(body.min.x - 0.15 * u, ty + 0.75 * u),
                Align2::LEFT_TOP,
                value,
                // fs_68 (4.25u) when the dock is tall enough for both readouts.
                m.mono_bold(size),
                // Red is for clipping only (Styles.md 1.3).
                if id == "true-peak" && engine.true_peak > 0.0 {
                    t.bad
                } else {
                    t.text
                },
            );
            let hit = Rect::from_min_max(Pos2::new(body.min.x, ty), g.max);
            if ui
                .interact(hit, ui.scope_id().with(id), Sense::click())
                .clicked()
            {
                out.push(Request::ResetMeters);
            }
            ty = g.max.y + 0.5 * u;
        }
        p.text(
            Pos2::new(body.min.x, ty),
            Align2::LEFT_TOP,
            format!(
                "M {}  S {}  CORR {:+.2}",
                lufs(engine.loudness[0]),
                lufs(engine.loudness[1]),
                engine.correlation
            ),
            m.mono(0.625),
            t.muted,
        );

        // Dual meters on the right with a dB scale.
        let w = 1.375 * u;
        let meter_top = body.min.y;
        let right = body.max.x;
        let to_y = |dbfs: f32| {
            let f = ((dbfs + 60.0) / 60.0).clamp(0.0, 1.0);
            body.max.y - f * (body.max.y - meter_top)
        };
        for (i, side) in [1, 0].into_iter().enumerate() {
            let x1 = right - i as f32 * (w + 0.125 * u); // sp_0h stereo gap
            let trough = m.snap_rect(Rect::from_x_y_ranges(x1 - w..=x1, meter_top..=body.max.y));
            p.rect_filled(trough, 0.0, t.bg_void);
            p.rect_stroke(trough, 0.0, m.hair(t.line_2), StrokeKind::Inside);
            let level = 20.0 * engine.peak[side].max(1e-6).log10();
            let inner = trough.shrink(m.px * 2.0);
            let top_y = to_y(level).max(inner.min.y);
            if top_y < inner.max.y {
                // Green to -3 dB, amber headroom above it.
                let amber_y = to_y(-3.0);
                let green =
                    Rect::from_x_y_ranges(inner.x_range(), top_y.max(amber_y)..=inner.max.y);
                p.rect_filled(green, 0.0, t.good);
                if top_y < amber_y {
                    p.rect_filled(
                        Rect::from_x_y_ranges(inner.x_range(), top_y..=amber_y),
                        0.0,
                        t.accent_amber,
                    );
                }
            }
            let cap = Rect::from_x_y_ranges(trough.x_range(), meter_top..=meter_top + 0.625 * u);
            let resp = ui.interact(cap, ui.scope_id().with(("clip", side)), Sense::click());
            if resp.clicked() {
                self.clip_latch[side] = false;
            }
            if self.clip_latch[side] {
                p.rect_filled(cap, 0.0, t.bad);
            }
        }
        let scale_x = right - 2.0 * w - 0.75 * u;
        for mark in [0.0, -3.0, -6.0, -12.0, -24.0, -48.0] {
            p.text(
                Pos2::new(scale_x, to_y(mark)),
                Align2::RIGHT_CENTER,
                format!("{mark:.0}"),
                m.mono(0.625),
                t.faint,
            );
        }
    }
}

/// The browser shows names, not paths.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}
