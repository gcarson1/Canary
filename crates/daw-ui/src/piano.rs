//! The piano roll: the selected track's notes in the current pattern. Draw on
//! empty space to add a note, drag a note to move it, drag its right edge to
//! resize, right-click to delete, Cmd-drag to box-select, Shift-click to add to
//! the selection. The slice tool splits a note. Keys: Delete, Cmd+A, Q quantize,
//! arrows transpose (Shift: octave) and nudge, Cmd+D duplicate, L legato,
//! [ and ] velocity down/up 10%, H humanize.
//! A velocity lane sits underneath. Each gesture is one undoable edit.

use daw_project::{edit, Note, Project, PPQ, STEP};
use egui::{Align2, Key, Pos2, Rect, Sense, StrokeKind};

use crate::theme::Metrics;
use crate::{header, Inputs, Request, Snap, Tool, Workspace};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Move,
    Resize,
    Select,
    Velocity,
}

struct Gesture {
    kind: Kind,
    track: usize,
    pattern: Option<u64>,
    /// The notes and selection when the gesture began.
    base: Vec<Note>,
    sel: Vec<Note>,
    from: Pos2,
    /// What the notes and selection become on release.
    preview: Vec<Note>,
    preview_sel: Vec<Note>,
    /// Any other edit while dragging cancels the gesture.
    revision: u64,
}

pub(crate) struct View {
    /// Tick at the left edge of the grid.
    start: f64,
    /// Ticks per point.
    zoom: f64,
    /// Points scrolled down from the top of key 127.
    top: f32,
    selected: Vec<Note>,
    gesture: Option<Gesture>,
    /// Length new notes get: the last one drawn or resized.
    length: u64,
    /// Scrolled to the middle of the keyboard once.
    placed: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            start: 0.0,
            zoom: 4.0,
            top: 0.0,
            selected: Vec::new(),
            gesture: None,
            length: STEP,
            placed: false,
        }
    }
}

impl View {
    pub(crate) fn is_dragging(&self) -> bool {
        self.gesture.is_some()
    }
}

/// Grid length in ticks for the snap setting (1 when off).
pub(crate) fn snap_ticks(snap: Snap) -> u64 {
    match snap {
        Snap::Sixteenth => STEP,
        Snap::Eighth => 2 * STEP,
        Snap::Bar => 4 * PPQ,
        Snap::Off => 1,
    }
}

fn is_black(key: u8) -> bool {
    matches!(key % 12, 1 | 3 | 6 | 8 | 10)
}

fn key_name(key: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!(
        "{}{}",
        NAMES[usize::from(key % 12)],
        i32::from(key / 12) - 1
    )
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
    let track = ws.selected_track.filter(|&i| i < project.tracks.len());
    let pattern_id = ws.current_pattern(project);
    let pattern = pattern_id.and_then(|id| project.pattern(id));
    let title = match (track, pattern) {
        (Some(ti), Some(pat)) => format!(
            "02A // PIANO_ROLL · {} · {}",
            project.tracks[ti].name.to_uppercase(),
            pat.name.to_uppercase()
        ),
        (Some(ti), None) => format!(
            "02A // PIANO_ROLL · {}",
            project.tracks[ti].name.to_uppercase()
        ),
        _ => "02A // PIANO_ROLL".into(),
    };
    let top = header(p, m, &t, r, &title);
    let Some(ti) = track else {
        p.text(
            r.center(),
            Align2::CENTER_CENTER,
            "NO TRACK SELECTED · PICK A CHANNEL IN 03",
            m.mono(0.6875),
            t.faint,
        );
        ws.roll.gesture = None;
        return;
    };
    let stored: &[Note] = pattern_id.map_or(&[], |id| project.tracks[ti].notes_in(id));
    let length = pattern.map_or(4 * PPQ, |p| p.length);
    let grid_ticks = snap_ticks(ws.snap);

    let keys_w = 4.0 * u;
    let ruler_h = 1.25 * u;
    let lane_h = 4.0 * u;
    let kh = 0.75 * u;
    let lane_top = m.snap(r.max.y - lane_h);
    let ruler = Rect::from_x_y_ranges(
        m.snap(r.min.x + keys_w)..=r.max.x,
        top..=m.snap(top + ruler_h),
    );
    let grid = Rect::from_x_y_ranges(ruler.min.x..=r.max.x, ruler.max.y..=lane_top);
    let keys = Rect::from_x_y_ranges(r.min.x..=ruler.min.x, grid.y_range());
    let lane = Rect::from_x_y_ranges(grid.x_range(), lane_top..=r.max.y);

    let v = &mut ws.roll;
    let max_top = (128.0 * kh - grid.height()).max(0.0);
    if !v.placed {
        // C6 at the top: the middle octaves in view.
        v.top = ((127 - 84) as f32 * kh).min(max_top);
        v.placed = true;
    }
    if ui.rect_contains_pointer(grid.union(keys)) {
        let (delta, zoom, pointer) =
            ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.pointer.hover_pos()));
        let factor = 1.0 / zoom.max(1e-3);
        if factor != 1.0 {
            let x = pointer.map_or(grid.center().x, |p| p.x);
            let anchor = v.start + f64::from(x - grid.min.x) * v.zoom;
            v.zoom = (v.zoom * f64::from(factor)).clamp(0.25, 200.0);
            v.start = (anchor - f64::from(x - grid.min.x) * v.zoom).max(0.0);
        } else {
            v.start = (v.start - f64::from(delta.x) * v.zoom).max(0.0);
            v.top = (v.top - delta.y).clamp(0.0, max_top);
        }
    }
    let (start, zoom, scroll) = (v.start, v.zoom, v.top);
    let x_of = |tick: f64| grid.min.x + ((tick - start) / zoom) as f32;
    let tick_at = |x: f32| (start + f64::from(x - grid.min.x) * zoom).max(0.0);
    let y_of = |key: u8| grid.min.y - scroll + f32::from(127 - key) * kh;
    let key_at =
        |y: f32| (127.0 - ((y - grid.min.y + scroll) / kh).floor()).clamp(0.0, 127.0) as u8;

    // Drop selections that an undo or another edit took away.
    ws.roll.selected.retain(|n| stored.contains(n));
    let stale = ws.roll.gesture.as_ref().is_some_and(|g| {
        g.revision != inputs.history.revision() || g.track != ti || g.pattern != pattern_id
    });
    if stale {
        ws.roll.gesture = None;
    }

    // Grid: key rows, beats and bars, the region past the pattern's end.
    let gp = p.with_clip_rect(grid);
    for key in 0..=127u8 {
        let y = y_of(key);
        if y > grid.max.y || y + kh < grid.min.y {
            continue;
        }
        let row = Rect::from_x_y_ranges(grid.x_range(), m.snap(y)..=m.snap(y + kh));
        if is_black(key) {
            gp.rect_filled(row, 0.0, t.sel);
        }
        let line = if key % 12 == 0 { t.line_2 } else { t.line };
        gp.hline(grid.x_range(), row.max.y, m.hair(line));
    }
    let end_x = x_of(length as f64);
    if end_x < grid.max.x {
        gp.rect_filled(
            Rect::from_x_y_ranges(end_x.max(grid.min.x)..=grid.max.x, grid.y_range()),
            0.0,
            t.bg_panel,
        );
    }
    let beat_px = PPQ as f32 / zoom as f32;
    let first = (start / PPQ as f64).floor() as u64;
    let rp = p.with_clip_rect(ruler);
    for b in first.. {
        let x = x_of((b * PPQ) as f64);
        if x > grid.max.x {
            break;
        }
        let bar = b % 4 == 0;
        if bar || beat_px >= 0.75 * u {
            gp.vline(
                m.snap(x),
                grid.y_range(),
                m.hair(if bar { t.line_2 } else { t.line }),
            );
        }
        if bar {
            rp.text(
                Pos2::new(x + 0.25 * u, ruler.center().y),
                Align2::LEFT_CENTER,
                format!("{}", b / 4 + 1),
                m.mono(0.5625),
                t.faint,
            );
        }
    }
    p.hline(r.x_range(), m.snap(ruler.max.y), m.hair(t.line));
    p.vline(m.snap(ruler.min.x), top..=r.max.y, m.hair(t.line));

    // Notes, as previewed by a gesture in progress.
    let (shown, shown_sel): (Vec<Note>, Vec<Note>) = match &ws.roll.gesture {
        Some(g) if g.kind != Kind::Select => (g.preview.clone(), g.preview_sel.clone()),
        _ => (stored.to_vec(), ws.roll.selected.clone()),
    };
    let note_rect = |n: &Note| {
        m.snap_rect(Rect::from_x_y_ranges(
            x_of(n.start as f64)..=x_of(n.end() as f64).max(x_of(n.start as f64) + 2.0 * m.px),
            y_of(n.key)..=y_of(n.key) + kh,
        ))
    };
    for n in &shown {
        let rect = note_rect(n);
        if !rect.intersects(grid) {
            continue;
        }
        let selected = shown_sel.contains(n);
        gp.rect_filled(rect, 0.0, if selected { t.accent_amber } else { t.muted });
        gp.rect_stroke(rect, 0.0, m.hair(t.bg_void), StrokeKind::Inside);
    }
    if let Some(g) = ws.roll.gesture.as_ref().filter(|g| g.kind == Kind::Select) {
        if let Some(now) = ui.input(|i| i.pointer.hover_pos()) {
            gp.rect_stroke(
                Rect::from_two_pos(g.from, now),
                0.0,
                m.hair(t.accent_amber),
                StrokeKind::Inside,
            );
        }
    }
    // Playhead while the pattern loops.
    if inputs.engine.playing && !ws.song && pattern.is_some() {
        let tick = project.ticks_at(inputs.engine.playhead) % length.max(1);
        let x = x_of(tick as f64);
        if (grid.min.x..=grid.max.x).contains(&x) {
            p.vline(m.snap(x), ruler.min.y..=r.max.y, m.hair(t.text));
        }
    }

    // Keyboard: press a key to hear it.
    let kp = p.with_clip_rect(keys);
    let held = ws
        .held_note
        .filter(|(track, _)| *track == ti)
        .map(|(_, k)| k);
    for key in 0..=127u8 {
        let y = y_of(key);
        if y > keys.max.y || y + kh < keys.min.y {
            continue;
        }
        let row = Rect::from_x_y_ranges(keys.x_range(), m.snap(y)..=m.snap(y + kh));
        let fill = if held == Some(key) {
            t.accent_amber
        } else if is_black(key) {
            t.bg_void
        } else {
            t.bg_panel
        };
        kp.rect_filled(row, 0.0, fill);
        kp.hline(keys.x_range(), row.max.y, m.hair(t.line));
        if key % 12 == 0 {
            kp.text(
                Pos2::new(keys.max.x - 0.25 * u, row.center().y),
                Align2::RIGHT_CENTER,
                key_name(key),
                m.mono(0.5),
                t.faint,
            );
        }
    }
    let kresp = ui.interact(
        keys,
        ui.scope_id().with("roll-keys"),
        Sense::click_and_drag(),
    );
    if kresp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
        if let Some(pos) = kresp.interact_pointer_pos() {
            out.push(Request::Note {
                track: ti,
                key: key_at(pos.y),
                on: true,
            });
        }
    }

    // Grid gestures.
    let gresp = ui.interact(
        grid,
        ui.scope_id().with("roll-grid"),
        Sense::click_and_drag(),
    );
    let pointer = ui.input(|i| i.pointer.hover_pos());
    let (pressed, released, mods) = ui.input(|i| {
        (
            i.pointer.primary_pressed(),
            !i.pointer.primary_down(),
            i.modifiers,
        )
    });
    let hit = |pos: Pos2| {
        stored
            .iter()
            .rev()
            .find(|n| note_rect(n).contains(pos))
            .copied()
    };
    if let (true, true, Some(pos)) = (pressed, gresp.hovered(), pointer) {
        let tick = tick_at(pos.x) as u64;
        let key = key_at(pos.y);
        let begin = |kind, base: Vec<Note>, sel: Vec<Note>| Gesture {
            kind,
            track: ti,
            pattern: pattern_id,
            preview: base.clone(),
            preview_sel: sel.clone(),
            base,
            sel,
            from: pos,
            revision: inputs.history.revision(),
        };
        let v = &mut ws.roll;
        match hit(pos) {
            _ if mods.command => v.gesture = Some(begin(Kind::Select, Vec::new(), Vec::new())),
            Some(n) if ws.tool == Tool::Slice => {
                let at = (tick + grid_ticks / 2) / grid_ticks * grid_ticks;
                if at > n.start && at < n.end() {
                    let mut notes: Vec<Note> =
                        stored.iter().filter(|&&x| x != n).copied().collect();
                    notes.push(Note {
                        length: at - n.start,
                        ..n
                    });
                    notes.push(Note {
                        start: at,
                        length: n.end() - at,
                        ..n
                    });
                    v.selected.clear();
                    ws.set_notes(project, ti, notes);
                }
            }
            Some(n) => {
                if mods.shift {
                    if !v.selected.contains(&n) {
                        v.selected.push(n);
                    }
                } else if !v.selected.contains(&n) {
                    v.selected = vec![n];
                }
                let near_end = note_rect(&n).max.x - pos.x < 0.375 * u;
                let kind = if near_end { Kind::Resize } else { Kind::Move };
                v.gesture = Some(begin(kind, stored.to_vec(), v.selected.clone()));
                if kind == Kind::Move {
                    out.push(Request::Note {
                        track: ti,
                        key: n.key,
                        on: true,
                    });
                }
            }
            None if ws.tool == Tool::Draw && tick < length => {
                let note = Note {
                    start: tick / grid_ticks * grid_ticks,
                    length: v.length,
                    key,
                    velocity: 100,
                };
                let mut base = stored.to_vec();
                base.push(note);
                v.selected = vec![note];
                v.gesture = Some(begin(Kind::Move, base, vec![note]));
                out.push(Request::Note {
                    track: ti,
                    key,
                    on: true,
                });
            }
            None => v.selected.clear(),
        }
    }
    if gresp.secondary_clicked() {
        if let Some(n) = pointer.and_then(hit) {
            ws.roll.selected.retain(|&x| x != n);
            let notes = stored.iter().filter(|&&x| x != n).copied().collect();
            ws.set_notes(project, ti, notes);
        }
    }

    // Velocity lane: drag over notes to set how hard they play.
    p.hline(r.x_range(), lane_top, m.hair(t.line_2));
    p.text(
        Pos2::new(r.min.x + 0.5 * u, lane.min.y + 0.75 * u),
        Align2::LEFT_CENTER,
        "VEL",
        m.mono(0.625),
        t.muted,
    );
    let bars = Rect::from_x_y_ranges(lane.x_range(), lane.min.y + 0.5 * u..=lane.max.y - 0.5 * u);
    let lp = p.with_clip_rect(lane);
    for n in &shown {
        let x = m.snap(x_of(n.start as f64));
        let h = bars.height() * f32::from(n.velocity) / 127.0;
        let color = if shown_sel.contains(n) {
            t.accent_amber
        } else {
            t.muted
        };
        lp.vline(
            x,
            bars.max.y - h..=bars.max.y,
            egui::Stroke::new(0.125 * u, color),
        );
        lp.rect_filled(
            Rect::from_center_size(Pos2::new(x, bars.max.y - h), egui::Vec2::splat(0.375 * u)),
            0.0,
            color,
        );
    }
    let lresp = ui.interact(
        bars,
        ui.scope_id().with("roll-velocity"),
        Sense::click_and_drag(),
    );
    if let (true, true, Some(pos)) = (pressed, lresp.hovered(), pointer) {
        ws.roll.gesture = Some(Gesture {
            kind: Kind::Velocity,
            track: ti,
            pattern: pattern_id,
            base: stored.to_vec(),
            sel: ws.roll.selected.clone(),
            preview: stored.to_vec(),
            preview_sel: ws.roll.selected.clone(),
            from: pos,
            revision: inputs.history.revision(),
        });
    }

    // Gesture in progress, then its release.
    if let (Some(g), Some(pos)) = (ws.roll.gesture.as_mut(), pointer) {
        let dt = (tick_at(pos.x) - tick_at(g.from.x)) / grid_ticks as f64;
        let dt = dt.round() as i64 * grid_ticks as i64;
        let dk = (key_at(pos.y) as i32 - key_at(g.from.y) as i32) * i32::from(g.kind == Kind::Move);
        match g.kind {
            Kind::Move => (g.preview, g.preview_sel) = edit::shift(&g.base, &g.sel, dt, dk),
            Kind::Resize => {
                (g.preview, g.preview_sel) = edit::map_notes(&g.base, &g.sel, |n| {
                    let end = (n.end() as i64 + dt).max(n.start as i64 + grid_ticks as i64);
                    Some(Note {
                        length: end as u64 - n.start,
                        ..n
                    })
                });
            }
            Kind::Velocity => {
                let vel = ((bars.max.y - pos.y) / bars.height() * 127.0)
                    .round()
                    .clamp(1.0, 127.0) as u8;
                let under: Vec<Note> = g
                    .preview
                    .iter()
                    .filter(|n| (x_of(n.start as f64) - pos.x).abs() < 0.5 * u)
                    .filter(|n| g.sel.is_empty() || g.preview_sel.contains(n))
                    .copied()
                    .collect();
                if !under.is_empty() {
                    let (notes, _) =
                        edit::map_notes(&g.preview, &under, |n| Some(Note { velocity: vel, ..n }));
                    let sel = g
                        .preview_sel
                        .iter()
                        .map(|s| {
                            if under.contains(s) {
                                Note {
                                    velocity: vel,
                                    ..*s
                                }
                            } else {
                                *s
                            }
                        })
                        .collect();
                    g.preview = notes;
                    g.preview_sel = sel;
                }
            }
            Kind::Select => {}
        }
    }
    if released {
        if let Some(g) = ws.roll.gesture.take() {
            match g.kind {
                Kind::Select => {
                    if let Some(pos) = pointer {
                        let area = Rect::from_two_pos(g.from, pos);
                        ws.roll.selected = stored
                            .iter()
                            .filter(|n| note_rect(n).intersects(area))
                            .copied()
                            .collect();
                    }
                }
                _ => {
                    if g.kind == Kind::Resize {
                        if let Some(n) = g.preview_sel.first() {
                            ws.roll.length = n.length;
                        }
                    }
                    ws.roll.selected = g.preview_sel;
                    if g.preview != stored {
                        ws.set_notes(project, ti, g.preview);
                    }
                }
            }
        }
    }

    keys_pressed(ws, ui, project, ti, stored, grid_ticks);
}

/// Keyboard edits on the selection (or every note when nothing is selected).
fn keys_pressed(
    ws: &mut Workspace,
    ui: &egui::Ui,
    project: &Project,
    ti: usize,
    stored: &[Note],
    grid: u64,
) {
    if ui.ctx().text_edit_focused() || ws.roll.gesture.is_some() {
        return;
    }
    let (mods, pressed) = ui.input(|i| {
        let keys: Vec<Key> = i
            .events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Key {
                    key, pressed: true, ..
                } => Some(*key),
                _ => None,
            })
            .collect();
        (i.modifiers, keys)
    });
    let sel = ws.roll.selected.clone();
    for key in pressed {
        let edited = match key {
            Key::Delete | Key::Backspace if !sel.is_empty() => Some((
                stored
                    .iter()
                    .filter(|n| !sel.contains(n))
                    .copied()
                    .collect(),
                Vec::new(),
            )),
            Key::A if mods.command => {
                ws.roll.selected = stored.to_vec();
                None
            }
            Key::Escape => {
                ws.roll.selected.clear();
                None
            }
            Key::Q if !mods.command => Some(edit::quantize(stored, &sel, grid, false)),
            Key::ArrowUp => Some(edit::transpose(
                stored,
                &sel,
                if mods.shift { 12 } else { 1 },
            )),
            Key::ArrowDown => Some(edit::transpose(
                stored,
                &sel,
                if mods.shift { -12 } else { -1 },
            )),
            Key::ArrowRight => Some(edit::shift(stored, &sel, grid as i64, 0)),
            Key::ArrowLeft => Some(edit::shift(stored, &sel, -(grid as i64), 0)),
            Key::D if mods.command => Some(edit::duplicate(stored, &sel, grid.max(STEP))),
            Key::L if !mods.command => Some(edit::legato(stored, &sel)),
            Key::OpenBracket => Some(edit::scale_velocity(stored, &sel, 0.9)),
            Key::CloseBracket => Some(edit::scale_velocity(stored, &sel, 1.1)),
            Key::H if !mods.command => {
                // A fresh seed each press, from the notes themselves.
                let seed = stored.iter().fold(0x9e37_79b9u64, |h, n| {
                    h.rotate_left(5) ^ n.start ^ u64::from(n.key) << 32
                });
                Some(edit::humanize(stored, &sel, grid / 8, 8, seed))
            }
            _ => None,
        };
        if let Some((notes, selection)) = edited {
            ws.roll.selected = selection;
            if notes != stored {
                ws.set_notes(project, ti, notes);
            }
            // One edit per frame: the next key acts on the edited notes.
            break;
        }
    }
}
