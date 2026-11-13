//! The step sequencer (FL's channel rack matrix): one row of sixteenth-note steps
//! per track in the current pattern, and a velocity lane for the selected track.
//! Clicking a step toggles a one-step note; dragging in the lane sets velocities.

use daw_project::{Action, Note, Pattern, Project, STEP};
use egui::{Align2, Pos2, Rect, Sense, StrokeKind, Vec2};

use crate::theme::Metrics;
use crate::{header, Inputs, Workspace};

/// The note a new step plays: middle C, where a sampler's root key sits.
pub(crate) const STEP_KEY: u8 = 60;
const STEP_VELOCITY: u8 = 100;
/// Lengths the STEPS switch offers.
const LENGTHS: [u64; 4] = [16, 32, 48, 64];

#[derive(Default)]
pub(crate) struct View {
    /// The selected track's notes while a velocity drag is in progress.
    velocity: Option<(usize, Vec<Note>)>,
    scroll_y: f32,
}

/// Index of the step `note` starts in.
fn step_of(note: &Note) -> u64 {
    note.start / STEP
}

pub(crate) fn show(
    ws: &mut Workspace,
    ui: &egui::Ui,
    p: &egui::Painter,
    m: &Metrics,
    r: Rect,
    inputs: &Inputs<'_>,
) {
    let (t, u) = (ws.theme, m.u);
    let project = inputs.history.project();
    let pattern = ws
        .current_pattern(project)
        .and_then(|id| project.pattern(id));
    let steps = pattern.map_or(16, |p| p.length.div_ceil(STEP)) as usize;
    let title = match pattern {
        Some(pat) => format!("02A // PATTERN_MATRIX · {}", pat.name.to_uppercase()),
        None => "02A // PATTERN_MATRIX".into(),
    };
    let top = header(p, m, &t, r, &title);
    steps_switch(ws, ui, p, m, r, project, pattern, steps);

    let lane_h = 4.0 * u;
    let lane_top = m.snap(r.max.y - lane_h);
    let rows = Rect::from_x_y_ranges(r.x_range(), top..=lane_top);
    let row_h = 2.5 * u;
    let label_w = 10.0 * u;
    let grid_x = r.min.x + label_w;
    let gap = 0.25 * u;
    let cell = ((r.max.x - grid_x - u) / steps as f32 - gap).clamp(0.5 * u, row_h - 2.0 * gap);
    let cell_x = |s: usize| grid_x + s as f32 * (cell + gap);
    // The step under the playhead while a pattern loops.
    let playing_step = (inputs.engine.playing && !ws.song && pattern.is_some()).then(|| {
        let ticks = project.ticks_at(inputs.engine.playhead);
        (ticks / STEP) as usize % steps.max(1)
    });

    if ui.rect_contains_pointer(rows) {
        let dy = ui.input(|i| i.smooth_scroll_delta.y);
        let max = (project.tracks.len() as f32 * row_h - rows.height()).max(0.0);
        ws.seq.scroll_y = (ws.seq.scroll_y - dy).clamp(0.0, max);
    }
    let rp = p.with_clip_rect(rows);
    if project.tracks.is_empty() {
        rp.text(
            rows.center(),
            Align2::CENTER_CENTER,
            "NO CHANNELS · PICK A FILE IN 01 OR + TRACK IN 03",
            m.mono(0.6875),
            t.faint,
        );
    }
    let id = ws.current_pattern(project);
    for (ti, track) in project.tracks.iter().enumerate() {
        let y = rows.min.y + ti as f32 * row_h - ws.seq.scroll_y;
        let row = Rect::from_x_y_ranges(r.x_range(), m.snap(y)..=m.snap(y + row_h));
        if !row.intersects(rows) {
            continue;
        }
        let selected = ws.selected_track == Some(ti);
        let label = Rect::from_x_y_ranges(r.min.x..=grid_x - 0.5 * u, row.y_range());
        if selected {
            rp.rect_filled(label, 0.0, t.sel);
            rp.rect_filled(
                Rect::from_x_y_ranges(row.min.x..=row.min.x + 0.1875 * u, row.y_range()),
                0.0,
                t.accent_amber,
            );
        }
        rp.text(
            Pos2::new(r.min.x + 0.75 * u, row.center().y),
            Align2::LEFT_CENTER,
            format!("{:02}", ti + 1),
            m.mono(0.6875),
            t.faint,
        );
        p.with_clip_rect(label.intersect(rows)).text(
            Pos2::new(r.min.x + 2.25 * u, row.center().y),
            Align2::LEFT_CENTER,
            &track.name,
            m.sans(0.75),
            if track.mute { t.faint } else { t.text },
        );
        let resp = ui.interact(
            label.intersect(rows),
            ui.scope_id().with(("seq-label", ti)),
            Sense::click(),
        );
        if resp.clicked() {
            ws.selected_track = Some(ti);
            ws.selected_clip = None;
        }

        let notes = id.map_or(&[][..], |id| track.notes_in(id));
        let mut toggle = None;
        for s in 0..steps {
            let rect = m.snap_rect(Rect::from_min_size(
                Pos2::new(cell_x(s), row.center().y - cell / 2.0),
                Vec2::splat(cell),
            ));
            let on = notes.iter().any(|n| step_of(n) == s as u64);
            let resp = ui.interact(
                rect.intersect(rows),
                ui.scope_id().with(("step", ti, s)),
                Sense::click(),
            );
            if on {
                rp.rect_filled(rect, 0.0, if selected { t.accent_amber } else { t.ink });
            } else {
                // Every other beat is shaded so the four groups read at a glance.
                let fill = if s / 4 % 2 == 1 { t.sel } else { t.bg_void };
                rp.rect_filled(rect, 0.0, fill);
                let line = if resp.hovered() { t.text } else { t.line_2 };
                rp.rect_stroke(rect, 0.0, m.hair(line), StrokeKind::Inside);
            }
            if playing_step == Some(s) {
                rp.hline(
                    rect.x_range(),
                    m.snap(rect.max.y + gap / 2.0),
                    m.hair(t.text),
                );
            }
            if resp.clicked() {
                toggle = Some((s as u64, on));
            }
        }
        if let Some((s, on)) = toggle {
            let mut next: Vec<Note> = notes.to_vec();
            if on {
                next.retain(|n| step_of(n) != s);
            } else {
                next.push(Note {
                    start: s * STEP,
                    length: STEP,
                    key: STEP_KEY,
                    velocity: STEP_VELOCITY,
                });
            }
            ws.selected_track = Some(ti);
            ws.set_notes(project, ti, next);
        }
        rp.hline(r.x_range(), row.max.y, m.hair(t.line));
    }

    // Velocity lane for the selected track.
    let lane = Rect::from_x_y_ranges(r.x_range(), lane_top..=r.max.y);
    p.hline(r.x_range(), lane_top, m.hair(t.line_2));
    p.text(
        Pos2::new(r.min.x + 0.75 * u, lane.min.y + 0.75 * u),
        Align2::LEFT_CENTER,
        "VELOCITY LANE",
        m.mono(0.625),
        t.muted,
    );
    let Some(ti) = ws.selected_track.filter(|&i| i < project.tracks.len()) else {
        return;
    };
    let stored = id.map_or(&[][..], |id| project.tracks[ti].notes_in(id));
    let shown: Vec<Note> = match &ws.seq.velocity {
        Some((track, notes)) if *track == ti => notes.clone(),
        _ => stored.to_vec(),
    };
    let bars = Rect::from_x_y_ranges(
        grid_x..=r.max.x,
        lane.min.y + 0.5 * u..=lane.max.y - 0.5 * u,
    );
    for n in &shown {
        let s = step_of(n) as usize;
        if s >= steps {
            continue;
        }
        let h = bars.height() * f32::from(n.velocity) / 127.0;
        let x = cell_x(s);
        p.rect_filled(
            m.snap_rect(Rect::from_x_y_ranges(
                x..=x + cell,
                bars.max.y - h..=bars.max.y,
            )),
            0.0,
            t.accent_amber,
        );
    }
    let resp = ui.interact(
        bars,
        ui.scope_id().with("velocity"),
        Sense::click_and_drag(),
    );
    if let Some(pos) = resp
        .interact_pointer_pos()
        .filter(|_| resp.is_pointer_button_down_on())
    {
        let s = ((pos.x - grid_x) / (cell + gap)).floor().max(0.0) as u64;
        let v = ((bars.max.y - pos.y) / bars.height() * 127.0)
            .round()
            .clamp(1.0, 127.0) as u8;
        let notes = ws.seq.velocity.get_or_insert_with(|| (ti, stored.to_vec()));
        for n in notes.1.iter_mut().filter(|n| step_of(n) == s) {
            n.velocity = v;
        }
    }
    if !ui.input(|i| i.pointer.any_down()) {
        if let Some((track, notes)) = ws.seq.velocity.take() {
            if track == ti && notes != stored {
                ws.set_notes(project, track, notes);
            }
        }
    }
}

/// `STEPS [16] [32] [48] [64]` at the header's right: the pattern's length.
#[allow(clippy::too_many_arguments)]
fn steps_switch(
    ws: &mut Workspace,
    ui: &egui::Ui,
    p: &egui::Painter,
    m: &Metrics,
    r: Rect,
    project: &Project,
    pattern: Option<&Pattern>,
    steps: usize,
) {
    let (t, u) = (ws.theme, m.u);
    let cy = r.min.y + u;
    let mut x = r.max.x - 0.75 * u;
    let mut clicked = None;
    for &n in LENGTHS.iter().rev() {
        let label = format!("{n}");
        let g = p.layout_no_wrap(label.clone(), m.mono(0.625), t.text);
        let rect = m.snap_rect(Rect::from_min_size(
            Pos2::new(x - g.size().x - 0.75 * u, cy - 0.625 * u),
            Vec2::new(g.size().x + 0.75 * u, 1.25 * u),
        ));
        let active = steps == n as usize && pattern.is_some();
        let resp = ui.interact(rect, ui.scope_id().with(("steps", n)), Sense::click());
        if active {
            p.rect_filled(rect, 0.0, t.accent_amber);
        } else {
            let line = if resp.hovered() { t.text } else { t.line_2 };
            p.rect_stroke(rect, 0.0, m.hair(line), StrokeKind::Inside);
        }
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            m.mono(0.625),
            if active { t.ink_text } else { t.muted },
        );
        if resp.clicked() {
            clicked = Some(n);
        }
        x = rect.min.x - 0.25 * u;
    }
    p.text(
        Pos2::new(x - 0.25 * u, cy),
        Align2::RIGHT_CENTER,
        "STEPS",
        m.mono(0.625),
        t.faint,
    );
    if let Some(n) = clicked {
        let (id, create) = ws.pattern_for_edit(project);
        let index = project.patterns.iter().position(|q| q.id == id);
        match (create, index) {
            (Some(Action::InsertPattern { index, mut pattern }), _) => {
                pattern.length = n * STEP;
                ws.edit(Action::InsertPattern { index, pattern });
            }
            (None, Some(index)) => {
                let mut pattern = project.patterns[index].clone();
                pattern.length = n * STEP;
                ws.edit(Action::SetPattern { index, pattern });
            }
            _ => {}
        }
    }
}
