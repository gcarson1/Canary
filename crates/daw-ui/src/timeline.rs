//! The playlist: ruler, track lanes, clips with waveforms, playhead. Clips move
//! (also across tracks), trim at their edges and split with the slice tool; every
//! change is one undoable `Action` applied when the gesture ends.

use std::sync::Arc;

use daw_core::{Clip, SampleData};
use daw_dsp::MinMax;
use daw_project::{Action, ClipRef, PatternClip, Project, Track};
use egui::{Align2, Pos2, Rect, Sense, StrokeKind};

use crate::theme::Metrics;
use crate::{EngineView, Inputs, Request, Snap, Tool, Workspace};

/// Scroll and zoom of the playlist, plus the gesture in progress.
pub(crate) struct View {
    /// Timeline sample at the left edge of the lanes.
    start: f64,
    /// Samples per point.
    zoom: f64,
    /// Lanes scrolled up by this many points.
    scroll_y: f32,
    drag: Option<Drag>,
    pattern_drag: Option<PatternDrag>,
    /// Playhead last frame, to page only when it crosses the right edge.
    last_playhead: u64,
}

impl Default for View {
    fn default() -> Self {
        Self {
            start: 0.0,
            zoom: 400.0,
            scroll_y: 0.0,
            drag: None,
            pattern_drag: None,
            last_playhead: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Grab {
    Body,
    Start,
    End,
}

struct Drag {
    track: usize,
    index: usize,
    grab: Grab,
    /// Samples between the pointer and the clip start when the drag began.
    offset: f64,
    /// Where the clip would land if released now.
    preview: ClipRef,
    to_track: usize,
    /// Project revision at the start; any other edit cancels the drag, since its
    /// indices may no longer point at the clip.
    revision: u64,
}

/// A pattern clip being moved or trimmed; positions in ticks.
struct PatternDrag {
    track: usize,
    index: usize,
    grab: Grab,
    /// Ticks between the pointer and the clip start when the drag began.
    offset: f64,
    preview: PatternClip,
    to_track: usize,
    revision: u64,
}

/// Where a pattern clip's edge lands when dragged to tick `at`: the start edge
/// trims into the pattern (moving `offset`), the end edge sets the length.
/// Both keep at least one grid step.
fn trim_pattern(clip: &PatternClip, grab: Grab, at: u64, grid: u64) -> PatternClip {
    let min = grid.max(1);
    match grab {
        Grab::Start => {
            let earliest = clip.start.saturating_sub(clip.offset);
            let at = at.clamp(earliest, clip.end().saturating_sub(min).max(earliest));
            PatternClip {
                start: at,
                length: clip.end() - at,
                offset: clip.offset + at - clip.start,
                ..*clip
            }
        }
        _ => PatternClip {
            length: at.max(clip.start + min) - clip.start,
            ..*clip
        },
    }
}

/// Snap grid length in samples, or `None` for no snapping.
fn grid(snap: Snap, bpm: f64, sample_rate: u32) -> Option<f64> {
    let beat = 60.0 / bpm * f64::from(sample_rate);
    match snap {
        Snap::Sixteenth => Some(beat / 4.0),
        Snap::Eighth => Some(beat / 2.0),
        Snap::Bar => Some(beat * 4.0),
        Snap::Off => None,
    }
}

impl View {
    pub(crate) fn is_dragging(&self) -> bool {
        self.drag.is_some() || self.pattern_drag.is_some()
    }

    fn x_of(&self, lanes: Rect, sample: f64) -> f32 {
        lanes.min.x + ((sample - self.start) / self.zoom) as f32
    }

    fn sample_at(&self, lanes: Rect, x: f32) -> f64 {
        (self.start + f64::from(x - lanes.min.x) * self.zoom).max(0.0)
    }
}

/// `sample` rounded to the snap grid, never below zero.
pub(crate) fn snap_f(sample: f64, snap: Snap, bpm: f64, sample_rate: u32) -> f64 {
    match grid(snap, bpm, sample_rate) {
        Some(g) => (sample / g).round() * g,
        None => sample,
    }
    .max(0.0)
}

/// 1-based bar, beat and sixteenth of `sample` in 4/4.
pub(crate) fn bar_beat_step(sample: u64, bpm: f64, sample_rate: u32) -> (u64, u64, u64) {
    let sixteenths = (sample as f64 / (60.0 / bpm * f64::from(sample_rate) / 4.0)) as u64;
    (
        sixteenths / 16 + 1,
        sixteenths / 4 % 4 + 1,
        sixteenths % 4 + 1,
    )
}

/// True while the transport plays through one of the track's clips and it is
/// not muted: the manifest's activity LED.
pub(crate) fn sounding(track: &Track, engine: &EngineView) -> bool {
    engine.playing
        && !track.mute
        && track
            .clips
            .iter()
            .any(|c| (c.start..c.start + c.length).contains(&engine.playhead))
}

/// The engine's view of a saved clip, for edits that must respect its audio.
fn core_clip(c: &ClipRef, audio: &Arc<SampleData>) -> Option<Clip> {
    Clip::with_window(
        c.start,
        audio.clone(),
        c.offset,
        c.length,
        c.fade_in,
        c.fade_out,
    )
}

fn clip_ref(source: &ClipRef, c: &Clip) -> ClipRef {
    ClipRef {
        source: source.source.clone(),
        start: c.start_sample,
        offset: c.offset_samples(),
        length: c.length_samples(),
        fade_in: c.fade_in_samples(),
        fade_out: c.fade_out_samples(),
    }
}

pub(crate) fn show(
    ws: &mut Workspace,
    ui: &egui::Ui,
    p: &egui::Painter,
    m: &Metrics,
    r: Rect,
    inputs: &Inputs<'_>,
    out: &mut Vec<Request>,
) {
    let (t, u) = (ws.theme, m.u);
    let project = inputs.history.project();
    let (bpm, rate) = (project.bpm, inputs.engine.sample_rate);
    let label_w = 7.5 * u;
    let ruler_h = 1.75 * u;
    let lane_h = 4.0 * u;
    let ruler = Rect::from_x_y_ranges(
        m.snap(r.min.x + label_w)..=r.max.x,
        r.min.y..=m.snap(r.min.y + ruler_h),
    );
    let lanes = Rect::from_x_y_ranges(ruler.min.x..=r.max.x, ruler.max.y..=r.max.y);
    let labels = Rect::from_x_y_ranges(r.min.x..=ruler.min.x, ruler.max.y..=r.max.y);
    let clip_p = p.with_clip_rect(lanes);
    let label_p = p.with_clip_rect(labels);

    // Scroll: wheel pans, cmd + wheel or pinch zooms around the pointer.
    let hover = ui.rect_contains_pointer(r);
    if hover {
        // egui turns cmd + wheel into zoom_delta itself.
        let (delta, zoom, pointer) =
            ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.pointer.hover_pos()));
        let v = &mut ws.timeline;
        let factor = 1.0 / zoom.max(1e-3);
        if factor != 1.0 {
            let x = pointer.map_or(lanes.center().x, |p| p.x);
            let anchor = v.sample_at(lanes, x);
            v.zoom = (v.zoom * f64::from(factor)).clamp(4.0, 1e5);
            v.start = (anchor - f64::from(x - lanes.min.x) * v.zoom).max(0.0);
        } else {
            v.start = (v.start - f64::from(delta.x) * v.zoom).max(0.0);
            let max_scroll = (project.tracks.len() as f32 * lane_h - lanes.height()).max(0.0);
            v.scroll_y = (v.scroll_y - delta.y).clamp(0.0, max_scroll);
        }
    }

    // Ruler: bar numbers; click or drag to seek.
    let v = &ws.timeline;
    let beat = 60.0 / bpm * f64::from(rate);
    let bar = beat * 4.0;
    let first_bar = (v.start / bar).floor().max(0.0) as u64;
    let bars_visible = (f64::from(lanes.width()) * v.zoom / bar).ceil() as u64 + 1;
    // Fewer labels when bars are narrow, so numbers never collide.
    let bar_px = (bar / v.zoom) as f32;
    let every = [1, 2, 4, 8, 16, 32, 64, 128]
        .into_iter()
        .find(|&n| bar_px * n as f32 >= 3.0 * u)
        .unwrap_or(256);
    for b in first_bar..first_bar + bars_visible {
        let x = m.snap(v.x_of(lanes, b as f64 * bar));
        if x > lanes.max.x {
            break;
        }
        if b % every != 0 {
            continue;
        }
        clip_p.vline(x, lanes.y_range(), m.hair(t.line_2));
        {
            p.with_clip_rect(ruler).text(
                Pos2::new(x + 0.25 * u, ruler.center().y),
                Align2::LEFT_CENTER,
                format!("{}", b + 1),
                m.mono(0.625),
                t.faint,
            );
        }
        if bar_px >= 4.0 * u {
            for q in 1..4 {
                let x = m.snap(v.x_of(lanes, b as f64 * bar + q as f64 * beat));
                clip_p.vline(x, lanes.y_range(), m.hair(t.line));
            }
        }
    }
    p.hline(r.x_range(), m.snap(ruler.max.y), m.hair(t.line));
    p.vline(m.snap(ruler.min.x), r.y_range(), m.hair(t.line));
    let seek = ui.interact(ruler, ui.scope_id().with("ruler"), Sense::click_and_drag());
    if seek.clicked() || seek.dragged() {
        if let Some(pos) = seek.interact_pointer_pos() {
            let at = v.sample_at(lanes, pos.x);
            out.push(Request::Seek(snap_f(at, ws.snap, bpm, rate) as u64));
        }
    }

    if project.tracks.is_empty() {
        p.text(
            lanes.center(),
            Align2::CENTER_CENTER,
            "NO TRACKS · PICK A FILE IN 01 OR + TRACK IN 03",
            m.mono(0.6875),
            t.faint,
        );
    }

    // Lanes and labels.
    let lane_rect = |i: usize, v: &View| {
        let y = lanes.min.y + i as f32 * lane_h - v.scroll_y;
        Rect::from_x_y_ranges(lanes.x_range(), m.snap(y)..=m.snap(y + lane_h))
    };
    for (i, track) in project.tracks.iter().enumerate() {
        let lane = lane_rect(i, &ws.timeline);
        let label = Rect::from_x_y_ranges(labels.x_range(), lane.y_range());
        if ws.selected_track == Some(i) {
            label_p.rect_filled(label, 0.0, t.sel);
        }
        label_p.text(
            Pos2::new(label.min.x + 0.75 * u, label.center().y),
            Align2::LEFT_CENTER,
            format!("{:02}", i + 1),
            m.mono(0.6875),
            t.faint,
        );
        label_p.text(
            Pos2::new(label.min.x + 2.25 * u, label.center().y),
            Align2::LEFT_CENTER,
            &track.name,
            m.sans(0.75),
            if track.mute { t.faint } else { t.text },
        );
        label_p.hline(labels.x_range(), lane.max.y, m.hair(t.line));
        clip_p.hline(lanes.x_range(), lane.max.y, m.hair(t.line));
        let resp = ui.interact(
            label.intersect(labels),
            ui.scope_id().with(("lane-label", i)),
            Sense::click(),
        );
        if resp.clicked() {
            ws.selected_track = Some(i);
            ws.selected_clip = None;
        }
    }

    // Clips.
    let edge = 0.375 * u;
    let lanes_resp = ui.interact(lanes, ui.scope_id().with("lanes"), Sense::click_and_drag());
    let pointer = lanes_resp.interact_pointer_pos().or(lanes_resp.hover_pos());
    // A drag starts only after the pointer has moved a few points, so hit-test
    // where the button went down, or a quick edge pull misses the edge.
    let press = ui.input(|i| i.pointer.press_origin());
    let probe = if lanes_resp.drag_started() {
        press.or(pointer)
    } else {
        pointer
    };
    // Any other edit (undo, delete) may have moved the clip under the drag.
    let stale = ws.timeline.drag.as_ref().is_some_and(|d| {
        d.revision != inputs.history.revision()
            || project
                .tracks
                .get(d.track)
                .and_then(|t| t.clips.get(d.index))
                .is_none()
    });
    if stale {
        ws.timeline.drag = None;
    }
    let mut hit = None;
    for (ti, track) in project.tracks.iter().enumerate() {
        for (ci, clip) in track.clips.iter().enumerate() {
            let dragging = ws
                .timeline
                .drag
                .as_ref()
                .is_some_and(|d| (d.track, d.index) == (ti, ci));
            let (shown, lane_i) = match &ws.timeline.drag {
                Some(d) if dragging => (&d.preview, d.to_track),
                _ => (clip, ti),
            };
            let lane = lane_rect(lane_i, &ws.timeline);
            let x0 = ws.timeline.x_of(lanes, shown.start as f64);
            let x1 = ws.timeline.x_of(lanes, (shown.start + shown.length) as f64);
            let rect = m.snap_rect(Rect::from_x_y_ranges(
                x0..=x1.max(x0 + m.px),
                lane.min.y + m.px..=lane.max.y - m.px,
            ));
            if probe.is_some_and(|p| rect.contains(p)) && !dragging {
                let grab = match probe {
                    Some(p) if p.x - rect.min.x < edge => Grab::Start,
                    Some(p) if rect.max.x - p.x < edge => Grab::End,
                    _ => Grab::Body,
                };
                hit = Some((ti, ci, grab));
            }
            if !rect.intersects(lanes) {
                continue;
            }
            draw_clip(ws, &clip_p, m, rect, shown, inputs);
            if ws.selected_clip == Some((ti, ci)) {
                clip_p.rect_stroke(rect, 0.0, m.hair(t.accent_amber), StrokeKind::Inside);
            }
        }
    }

    // Pattern clips draw over audio and take the pointer first.
    let stale = ws.timeline.pattern_drag.as_ref().is_some_and(|d| {
        d.revision != inputs.history.revision()
            || project
                .tracks
                .get(d.track)
                .and_then(|t| t.pattern_clips.get(d.index))
                .is_none()
    });
    if stale {
        ws.timeline.pattern_drag = None;
    }
    let mut phit = None;
    for (ti, track) in project.tracks.iter().enumerate() {
        for (ci, clip) in track.pattern_clips.iter().enumerate() {
            let dragging = ws
                .timeline
                .pattern_drag
                .as_ref()
                .is_some_and(|d| (d.track, d.index) == (ti, ci));
            let (shown, lane_i) = match &ws.timeline.pattern_drag {
                Some(d) if dragging => (&d.preview, d.to_track),
                _ => (clip, ti),
            };
            let lane = lane_rect(lane_i, &ws.timeline);
            let x0 = ws
                .timeline
                .x_of(lanes, project.samples_at(shown.start) as f64);
            let x1 = ws
                .timeline
                .x_of(lanes, project.samples_at(shown.end()) as f64);
            let rect = m.snap_rect(Rect::from_x_y_ranges(
                x0..=x1.max(x0 + m.px),
                lane.min.y + m.px..=lane.max.y - m.px,
            ));
            if probe.is_some_and(|p| rect.contains(p)) && !dragging {
                let grab = match probe {
                    Some(p) if p.x - rect.min.x < edge => Grab::Start,
                    Some(p) if rect.max.x - p.x < edge => Grab::End,
                    _ => Grab::Body,
                };
                phit = Some((ti, ci, grab));
            }
            if !rect.intersects(lanes) {
                continue;
            }
            draw_pattern_clip(ws, &clip_p, m, rect, shown, project);
            if ws.selected_pattern_clip == Some((ti, ci)) {
                clip_p.rect_stroke(rect, 0.0, m.hair(t.accent_amber), StrokeKind::Inside);
            }
        }
    }
    if phit.is_some() {
        hit = None;
    }
    let grid_ticks = crate::piano::snap_ticks(ws.snap);
    let snap_tick = |sample: f64| {
        let tick = project.ticks_at(sample.max(0.0) as u64);
        (tick + grid_ticks / 2) / grid_ticks * grid_ticks
    };
    if let (Some((_, _, grab)), true) = (phit, lanes_resp.hovered()) {
        ui.ctx().set_cursor_icon(match grab {
            Grab::Body => egui::CursorIcon::Grab,
            _ => egui::CursorIcon::ResizeHorizontal,
        });
    }
    if lanes_resp.drag_started() && ws.tool == Tool::Draw {
        let at = probe.map(|p| ws.timeline.sample_at(lanes, p.x));
        if let (Some((ti, ci, grab)), Some(at)) = (phit, at) {
            let clip = project.tracks[ti].pattern_clips[ci];
            ws.selected_pattern_clip = Some((ti, ci));
            ws.selected_clip = None;
            ws.selected_track = Some(ti);
            ws.timeline.pattern_drag = Some(PatternDrag {
                track: ti,
                index: ci,
                grab,
                offset: project.ticks_at(at as u64) as f64 - clip.start as f64,
                preview: clip,
                to_track: ti,
                revision: inputs.history.revision(),
            });
        }
    }
    let scroll_y = ws.timeline.scroll_y;
    let pointer_at = pointer.map(|p| (p, ws.timeline.sample_at(lanes, p.x)));
    if let (Some((pos, at)), Some(d)) = (pointer_at, ws.timeline.pattern_drag.as_mut()) {
        let original = project.tracks[d.track].pattern_clips[d.index];
        match d.grab {
            Grab::Body => {
                let tick = project.ticks_at(at as u64) as f64 - d.offset;
                let start = project.samples_at(tick.max(0.0) as u64) as f64;
                d.preview.start = snap_tick(start);
                let lane = ((pos.y - lanes.min.y + scroll_y) / lane_h).floor();
                d.to_track = (lane.max(0.0) as usize).min(project.tracks.len() - 1);
            }
            grab => d.preview = trim_pattern(&original, grab, snap_tick(at), grid_ticks),
        }
    }
    if lanes_resp.drag_stopped() {
        if let Some(d) = ws.timeline.pattern_drag.take() {
            let action = if d.to_track == d.track {
                Action::SetPatternClip {
                    track: d.track,
                    index: d.index,
                    clip: d.preview,
                }
            } else {
                let index = project.tracks[d.to_track].pattern_clips.len();
                ws.selected_pattern_clip = Some((d.to_track, index));
                ws.selected_track = Some(d.to_track);
                Action::Batch(vec![
                    Action::RemovePatternClip {
                        track: d.track,
                        index: d.index,
                    },
                    Action::InsertPatternClip {
                        track: d.to_track,
                        index,
                        clip: d.preview,
                    },
                ])
            };
            ws.edit(action);
        }
    }
    if lanes_resp.clicked() {
        if let Some((ti, ci, _)) = phit {
            ws.selected_pattern_clip = Some((ti, ci));
            ws.selected_clip = None;
            ws.selected_track = Some(ti);
        } else {
            ws.selected_pattern_clip = None;
        }
    }
    // Double-click empty lane space to place the current pattern there.
    if lanes_resp.double_clicked() && hit.is_none() && phit.is_none() {
        let pattern = ws
            .current_pattern(project)
            .and_then(|id| project.pattern(id));
        if let (Some(pat), Some(pos)) = (pattern, pointer) {
            let lane = ((pos.y - lanes.min.y + ws.timeline.scroll_y) / lane_h).floor();
            let row = lane.max(0.0) as usize;
            if row < project.tracks.len() {
                let at = ws.timeline.sample_at(lanes, pos.x);
                let start = project.ticks_at(at as u64) / grid_ticks * grid_ticks;
                let index = project.tracks[row].pattern_clips.len();
                ws.selected_pattern_clip = Some((row, index));
                ws.selected_clip = None;
                ws.edit(Action::InsertPatternClip {
                    track: row,
                    index,
                    clip: PatternClip {
                        pattern: pat.id,
                        start,
                        length: pat.length,
                        offset: 0,
                    },
                });
            }
        }
    }

    // Gestures.
    if let (Some((_, _, grab)), true) = (hit, lanes_resp.hovered()) {
        let icon = match grab {
            Grab::Body if ws.tool == Tool::Slice => egui::CursorIcon::Text,
            Grab::Body => egui::CursorIcon::Grab,
            _ => egui::CursorIcon::ResizeHorizontal,
        };
        ui.ctx().set_cursor_icon(icon);
    }
    let pointer_sample = pointer.map(|p| ws.timeline.sample_at(lanes, p.x));
    if lanes_resp.drag_started() && ws.tool == Tool::Draw {
        let at = probe.map(|p| ws.timeline.sample_at(lanes, p.x));
        if let (Some((ti, ci, grab)), Some(at)) = (hit, at) {
            let clip = project.tracks[ti].clips[ci].clone();
            ws.selected_clip = Some((ti, ci));
            ws.selected_track = Some(ti);
            ws.timeline.drag = Some(Drag {
                track: ti,
                index: ci,
                grab,
                offset: at - clip.start as f64,
                preview: clip,
                to_track: ti,
                revision: inputs.history.revision(),
            });
        }
    }
    if let (Some(at), Some(pos)) = (pointer_sample, pointer) {
        if let Some(d) = ws.timeline.drag.as_mut() {
            let original = &project.tracks[d.track].clips[d.index];
            let audio = inputs
                .waveforms
                .get(&original.source)
                .map(|w| w.audio().clone());
            match d.grab {
                Grab::Body => {
                    d.preview.start = snap_f(at - d.offset, ws.snap, bpm, rate) as u64;
                    let lane = ((pos.y - lanes.min.y + ws.timeline.scroll_y) / lane_h).floor();
                    d.to_track = (lane.max(0.0) as usize).min(project.tracks.len() - 1);
                }
                grab => {
                    if let Some(mut c) = audio.as_ref().and_then(|a| core_clip(original, a)) {
                        let to = snap_f(at, ws.snap, bpm, rate) as u64;
                        if grab == Grab::Start {
                            c.trim_start(to);
                        } else {
                            c.trim_end(to);
                        }
                        d.preview = clip_ref(original, &c);
                    }
                }
            }
        }
    }
    if lanes_resp.drag_stopped() {
        if let Some(d) = ws.timeline.drag.take() {
            let action = if d.to_track == d.track {
                Action::SetClip {
                    track: d.track,
                    index: d.index,
                    clip: d.preview,
                }
            } else {
                let index = project.tracks[d.to_track].clips.len();
                ws.selected_clip = Some((d.to_track, index));
                ws.selected_track = Some(d.to_track);
                Action::Batch(vec![
                    Action::RemoveClip {
                        track: d.track,
                        index: d.index,
                    },
                    Action::InsertClip {
                        track: d.to_track,
                        index,
                        clip: d.preview,
                    },
                ])
            };
            ws.edit(action);
        }
    }
    if lanes_resp.clicked() {
        match (hit, ws.tool, pointer_sample) {
            (Some((ti, ci, _)), Tool::Slice, Some(at)) => {
                let clip = &project.tracks[ti].clips[ci];
                let at = snap_f(at, ws.snap, bpm, rate) as u64;
                let split = inputs
                    .waveforms
                    .get(&clip.source)
                    .and_then(|w| core_clip(clip, w.audio()))
                    .and_then(|c| c.split(at));
                if let Some((l, r)) = split {
                    ws.edit(Action::Batch(vec![
                        Action::SetClip {
                            track: ti,
                            index: ci,
                            clip: clip_ref(clip, &l),
                        },
                        Action::InsertClip {
                            track: ti,
                            index: ci + 1,
                            clip: clip_ref(clip, &r),
                        },
                    ]));
                }
            }
            (Some((ti, ci, _)), _, _) => {
                ws.selected_clip = Some((ti, ci));
                ws.selected_track = Some(ti);
            }
            (None, _, _) => ws.selected_clip = None,
        }
    }

    // Playhead over everything in the lanes.
    let x = ws.timeline.x_of(lanes, inputs.engine.playhead as f64);
    if (lanes.min.x..=lanes.max.x).contains(&x) {
        let x = m.snap(x);
        p.vline(x, ruler.min.y..=lanes.max.y, m.hair(t.text));
        p.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(x - 0.3125 * u, ruler.min.y),
                Pos2::new(x + 0.3125 * u, ruler.min.y),
                Pos2::new(x, ruler.min.y + 0.4375 * u),
            ],
            t.text,
            egui::Stroke::NONE,
        ));
    }
    // Page forward when the playing playhead crosses the right edge; the view is
    // otherwise free to pan while it plays.
    let v = &mut ws.timeline;
    let right = v.start + f64::from(lanes.width()) * v.zoom;
    let ph = inputs.engine.playhead;
    if inputs.engine.playing
        && v.drag.is_none()
        && (v.last_playhead as f64) < right
        && ph as f64 >= right
    {
        v.start = ph as f64;
    }
    v.last_playhead = ph;
}

/// A pattern clip: its name, a marker at each repeat, and a sketch of every
/// track's notes in it.
fn draw_pattern_clip(
    ws: &Workspace,
    p: &egui::Painter,
    m: &Metrics,
    rect: Rect,
    clip: &PatternClip,
    project: &Project,
) {
    let (t, u) = (ws.theme, m.u);
    p.rect_filled(rect, 0.0, t.sel);
    p.rect_stroke(rect, 0.0, m.hair(t.line_2), StrokeKind::Inside);
    let title_h = 1.125 * u;
    let title = Rect::from_x_y_ranges(rect.x_range(), rect.min.y..=m.snap(rect.min.y + title_h));
    p.hline(rect.x_range(), title.max.y, m.hair(t.line_2));
    let Some(pattern) = project.pattern(clip.pattern) else {
        return;
    };
    let cp = p.with_clip_rect(rect.intersect(p.clip_rect()));
    cp.text(
        Pos2::new(rect.min.x + 0.375 * u, title.center().y),
        Align2::LEFT_CENTER,
        pattern.name.to_uppercase(),
        m.mono(0.5625),
        t.text,
    );
    let body = Rect::from_x_y_ranges(rect.x_range(), title.max.y + m.px..=rect.max.y - m.px);
    let notes: Vec<_> = project
        .tracks
        .iter()
        .flat_map(|tr| tr.notes_in(clip.pattern))
        .collect();
    let (lo, hi) = notes
        .iter()
        .fold((127u8, 0u8), |(lo, hi), n| (lo.min(n.key), hi.max(n.key)));
    let span = f32::from(hi.saturating_sub(lo)).max(11.0);
    let zoom = ws.timeline.zoom;
    let x_of = |tick: u64| {
        let s = project.samples_at(tick) as f64 - project.samples_at(clip.start) as f64;
        rect.min.x + (s / zoom) as f32
    };
    // Repeat k of the pattern starts at clip tick k * length - offset % length.
    let first = clip.offset % pattern.length;
    // Start at the first repeat that reaches the visible area, stop past it.
    let visible = project.ticks_at(
        (f64::from(p.clip_rect().min.x - rect.min.x) * zoom).max(0.0) as u64
            + project.samples_at(clip.start),
    ) as i128;
    let mut rep = clip.start as i128 - first as i128;
    if visible > rep + pattern.length as i128 {
        rep += (visible - rep) / pattern.length as i128 * pattern.length as i128
            - pattern.length as i128;
    }
    while rep < clip.end() as i128 && x_of(rep.max(0) as u64) <= p.clip_rect().max.x {
        if rep > clip.start as i128 {
            cp.vline(m.snap(x_of(rep as u64)), body.y_range(), m.hair(t.line_2));
        }
        for n in &notes {
            let on = rep + n.start as i128;
            let off = (rep + n.end().min(pattern.length) as i128).min(clip.end() as i128);
            if on < clip.start as i128 || on >= off {
                continue;
            }
            let y =
                body.max.y - 0.25 * u - (body.height() - 0.5 * u) * f32::from(n.key - lo) / span;
            let (x0, x1) = (x_of(on as u64), x_of(off as u64));
            if x0 > p.clip_rect().max.x {
                break;
            }
            cp.hline(x0..=x1.max(x0 + 2.0 * m.px), m.snap(y), m.hair(t.muted));
        }
        rep += pattern.length as i128;
    }
}

fn draw_clip(
    ws: &Workspace,
    p: &egui::Painter,
    m: &Metrics,
    rect: Rect,
    clip: &ClipRef,
    inputs: &Inputs<'_>,
) {
    let (t, u) = (ws.theme, m.u);
    p.rect_filled(rect, 0.0, t.bg_rack);
    let title_h = 1.125 * u;
    let title = Rect::from_x_y_ranges(rect.x_range(), rect.min.y..=m.snap(rect.min.y + title_h));
    p.hline(rect.x_range(), title.max.y, m.hair(t.line_2));
    let name = clip
        .source
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    p.with_clip_rect(rect.intersect(p.clip_rect())).text(
        Pos2::new(rect.min.x + 0.375 * u, title.center().y),
        Align2::LEFT_CENTER,
        name,
        m.sans(0.625),
        t.text,
    );

    let body = Rect::from_x_y_ranges(rect.x_range(), title.max.y..=rect.max.y);
    let visible = body.intersect(p.clip_rect());
    if let Some(w) = inputs
        .waveforms
        .get(&clip.source)
        .filter(|_| visible.is_positive())
    {
        let channels = w.audio().num_channels().min(2);
        let zoom = ws.timeline.zoom;
        let cols = (visible.width() / m.px).ceil() as usize;
        let mut bins = vec![MinMax::EMPTY; cols];
        // Source frame under the first visible column.
        let first = clip.offset as f64 + f64::from(visible.min.x - rect.min.x) * zoom;
        // One mesh per clip, a one-pixel quad per column, not a shape per column.
        let mut mesh = egui::Mesh::default();
        for ch in 0..channels {
            let band_h = body.height() / channels as f32;
            let mid = body.min.y + band_h * (ch as f32 + 0.5);
            let half = band_h * 0.45;
            w.view(ch, first, zoom * f64::from(m.px), &mut bins);
            let end = (clip.offset + clip.length) as f64;
            for (i, b) in bins.iter().enumerate() {
                let frame = first + i as f64 * zoom * f64::from(m.px);
                if b.is_empty() || frame >= end {
                    continue;
                }
                let x = visible.min.x + i as f32 * m.px;
                let (lo, hi) = (b.min.clamp(-1.0, 1.0), b.max.clamp(-1.0, 1.0));
                mesh.add_colored_rect(
                    Rect::from_x_y_ranges(
                        x..=x + m.px,
                        (mid - hi * half)..=(mid - lo * half + m.px),
                    ),
                    t.muted,
                );
            }
        }
        p.add(mesh);
    }
    // Fades as straight guides from the clip edge to full level.
    for (fade, left) in [(clip.fade_in, true), (clip.fade_out, false)] {
        if fade == 0 {
            continue;
        }
        let w = (fade as f64 / ws.timeline.zoom) as f32;
        let (a, b) = if left {
            (
                Pos2::new(body.min.x, body.max.y),
                Pos2::new(body.min.x + w, body.min.y),
            )
        } else {
            (
                Pos2::new(body.max.x - w, body.min.y),
                Pos2::new(body.max.x, body.max.y),
            )
        };
        p.line_segment([a, b], m.hair(t.faint));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trimming_a_pattern_clip_keeps_a_grid_step_and_the_pattern_start() {
        let clip = PatternClip {
            pattern: 1,
            start: 1_000,
            length: 2_000,
            offset: 300,
        };
        // The start edge can open back to the pattern's own start, no further.
        let t = trim_pattern(&clip, Grab::Start, 0, 240);
        assert_eq!((t.start, t.length, t.offset), (700, 2_300, 0));
        let t = trim_pattern(&clip, Grab::Start, 1_500, 240);
        assert_eq!(
            (t.start, t.length, t.offset, t.end()),
            (1_500, 1_500, 800, 3_000)
        );
        // Neither edge closes the clip below one grid step.
        let t = trim_pattern(&clip, Grab::Start, 9_000, 240);
        assert_eq!((t.start, t.length), (2_760, 240));
        let t = trim_pattern(&clip, Grab::End, 0, 240);
        assert_eq!((t.start, t.length), (1_000, 240));
    }

    #[test]
    fn clock_counts_bars_beats_and_sixteenths_from_one() {
        // 120 BPM at 48 kHz: a beat is 24 000 samples, a sixteenth 6 000.
        assert_eq!(bar_beat_step(0, 120.0, 48_000), (1, 1, 1));
        assert_eq!(bar_beat_step(6_000, 120.0, 48_000), (1, 1, 2));
        assert_eq!(bar_beat_step(24_000, 120.0, 48_000), (1, 2, 1));
        assert_eq!(bar_beat_step(96_000 + 30_000, 120.0, 48_000), (2, 2, 2));
    }

    #[test]
    fn snapping_rounds_to_the_grid_and_off_keeps_the_sample() {
        assert_eq!(snap_f(6_100.0, Snap::Sixteenth, 120.0, 48_000), 6_000.0);
        assert_eq!(snap_f(11_900.0, Snap::Eighth, 120.0, 48_000), 12_000.0);
        assert_eq!(snap_f(50_000.0, Snap::Bar, 120.0, 48_000), 96_000.0);
        assert_eq!(snap_f(123.0, Snap::Off, 120.0, 48_000), 123.0);
    }
}
