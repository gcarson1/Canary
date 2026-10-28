//! 02B // HARDWARE_DSP_RACK: the selected track's plugin devices as modules with
//! arc dials (Styles.md 4.3), and the picker that adds one. A sampler module is
//! twice as wide: its waveform with slice points, and the pads they map to.

use std::f32::consts::PI;
use std::path::PathBuf;

use daw_dsp::MinMax;

use daw_project::Action;
use egui::{Align2, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2};

use crate::theme::Metrics;
use crate::{header, Inputs, Request, Workspace};

/// Dials shown per device; plugins with more parameters show their first ones.
const DIALS: usize = 8;

/// A plugin the user can add, as the app found it.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginChoice {
    pub name: String,
    pub vendor: String,
    /// Short format tag: `AU`, `CLAP`, `VST3`, or `CANARY` for built-ins.
    pub format: &'static str,
}

/// One device on the selected track, with live parameter values.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceView {
    pub name: String,
    pub params: Vec<ParamView>,
    /// Set for the built-in sampler.
    pub sampler: Option<SamplerView>,
    /// A third-party plugin, which may have its own editor.
    pub editor: bool,
}

/// What the rack shows of a sampler.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SamplerView {
    /// The file slices come from; its waveform is drawn when loaded.
    pub path: Option<PathBuf>,
    /// Slice starts as fractions of the sample, the first at 0.
    pub slices: Vec<f64>,
    pub layers: usize,
}

/// How a file dropped on a sampler joins it, by modifier held at the drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleDrop {
    /// No modifier: replaces every layer.
    Replace,
    /// Shift: a round-robin alternate of the last layer.
    RoundRobin,
    /// Option: a new velocity zone.
    Velocity,
    /// Command: a new key zone.
    Key,
}

/// Pads shown per sampler (4 x 4); slices past these still play from their keys.
const PADS: usize = 16;
const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// "C3" for MIDI 60.
fn note_name(key: usize) -> String {
    format!("{}{}", NOTE_NAMES[key % 12], key as i32 / 12 - 2)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParamView {
    pub id: u32,
    pub name: String,
    pub value: f64,
    pub min: f64,
    pub max: f64,
    /// Turns logarithmically (a frequency, say); needs `min > 0`.
    pub log: bool,
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
    let y = header(p, m, &t, r, "02B // HARDWARE_DSP_RACK");
    let body = Rect::from_x_y_ranges(r.x_range(), y..=r.max.y);
    let Some(track) = ws
        .selected_track
        .filter(|&i| i < inputs.history.project().tracks.len())
    else {
        p.text(
            body.center(),
            Align2::CENTER_CENTER,
            "SELECT A TRACK IN 03",
            m.mono(0.6875),
            t.faint,
        );
        return;
    };

    // While the picker is open it owns the pointer: nothing under it reacts.
    let was_open = ws.picker.is_some();
    let module_w = 16.0 * u;
    let width = |d: &DeviceView| {
        if d.sampler.is_some() {
            2.0 * module_w
        } else {
            module_w
        }
    };
    // Module left edges, in order.
    let starts: Vec<f32> = inputs
        .devices
        .iter()
        .scan(body.min.x, |x, d| {
            let at = *x;
            *x += width(d);
            Some(at)
        })
        .collect();
    // Drag a module by its title to move it: the slot under the pointer.
    let slot = |x: f32| starts.iter().rposition(|&s| s <= x).unwrap_or(0);
    let pointer = ui.input(|i| i.pointer.interact_pos());
    // Only over the rack: a drop anywhere else cancels the move.
    let target = ws
        .device_drag
        .zip(pointer)
        .filter(|(_, at)| body.contains(*at))
        .map(|(_, at)| slot(at.x));
    let mut x = body.min.x;
    for (di, device) in inputs.devices.iter().enumerate() {
        let module = Rect::from_x_y_ranges(x..=m.snap(x + width(device)), body.y_range());
        let p = p.with_clip_rect(module.intersect(p.clip_rect()));
        let top = module.min.y + 1.25 * u;
        if ws.device_drag == Some(di) {
            p.rect_filled(module, 0.0, t.sel);
        }
        if target == Some(di) && ws.device_drag != Some(di) {
            // Insertion bar on the side the device will land.
            let from = ws.device_drag.unwrap_or(di);
            let edge = if from < di {
                module.max.x - 0.1875 * u
            } else {
                module.min.x
            };
            p.rect_filled(
                Rect::from_x_y_ranges(edge..=edge + 0.1875 * u, module.y_range()),
                0.0,
                t.accent_amber,
            );
        }
        p.text(
            Pos2::new(module.min.x + 0.75 * u, top),
            Align2::LEFT_CENTER,
            format!("{:02} // {}", di + 1, device.name.to_uppercase()),
            m.mono(0.6875),
            t.muted,
        );
        let remove = p.text(
            Pos2::new(module.max.x - 0.75 * u, top),
            Align2::RIGHT_CENTER,
            "REMOVE",
            m.mono(0.625),
            t.faint,
        );
        let resp = ui.interact(
            remove.expand(0.25 * u),
            ui.scope_id().with(("remove-device", di)),
            Sense::click(),
        );
        if resp.hovered() {
            p.hline(remove.x_range(), m.snap(remove.max.y), m.hair(t.muted));
        }
        if resp.clicked() && !was_open {
            ws.edit(Action::RemoveDevice { track, index: di });
        }
        let mut title_end = remove.min.x;
        if device.editor {
            let edit = p.text(
                Pos2::new(remove.min.x - 0.75 * u, top),
                Align2::RIGHT_CENTER,
                "EDIT",
                m.mono(0.625),
                t.faint,
            );
            let resp = ui.interact(
                edit.expand(0.25 * u),
                ui.scope_id().with(("edit-device", di)),
                Sense::click(),
            );
            if resp.hovered() {
                p.hline(edit.x_range(), m.snap(edit.max.y), m.hair(t.muted));
            }
            if resp.clicked() && !was_open {
                out.push(Request::OpenEditor { track, device: di });
            }
            title_end = edit.min.x;
        }
        let title = Rect::from_x_y_ranges(
            module.min.x..=title_end - 0.5 * u,
            module.min.y..=top + 0.75 * u,
        );
        let grab = ui.interact(
            title,
            ui.scope_id().with(("move-device", di)),
            Sense::drag(),
        );
        if grab.drag_started() && !was_open {
            ws.device_drag = Some(di);
        }
        if grab.drag_stopped() && ws.device_drag == Some(di) {
            ws.device_drag = None;
            if let Some(to) = target.filter(|&to| to != di) {
                ws.edit(Action::MoveDevice {
                    track,
                    from: di,
                    to,
                });
            }
        }

        // Dials in two rows of four, after the sampler's panel if there is one.
        let mut dials_x = module.min.x;
        if let Some(view) = &device.sampler {
            let panel = Rect::from_x_y_ranges(
                module.min.x..=module.min.x + module_w,
                top + 1.25 * u..=module.max.y,
            );
            let at = (track, di);
            sampler_panel(
                ui, &p, m, ws, panel, inputs, device, view, at, was_open, out,
            );
            p.vline(m.snap(panel.max.x), panel.y_range(), m.hair(t.line));
            dials_x = panel.max.x;
        }
        let cell = Vec2::new(module_w / 4.0, (module.height() - 2.5 * u) / 2.0);
        for (pi, param) in device.params.iter().take(DIALS).enumerate() {
            let origin = Pos2::new(
                dials_x + (pi % 4) as f32 * cell.x,
                top + 1.25 * u + (pi / 4) as f32 * cell.y,
            );
            let area = Rect::from_min_size(origin, cell);
            let id = ui.scope_id().with(("dial", di, param.id));
            if let Some(v) = dial(ui, &p, m, ws, area, param, (!was_open).then_some(id)) {
                out.push(Request::SetParam {
                    track,
                    device: di,
                    param: param.id,
                    value: v,
                });
            }
        }
        if device.params.is_empty() {
            p.text(
                module.center(),
                Align2::CENTER_CENTER,
                "NO PARAMETERS",
                m.mono(0.625),
                t.faint,
            );
        }
        p.vline(m.snap(module.max.x), body.y_range(), m.hair(t.line_2));
        x = module.max.x;
    }

    // Add-device button after the modules.
    let add = m.snap_rect(Rect::from_min_size(
        Pos2::new(x + 0.75 * u, body.min.y + 0.75 * u),
        Vec2::new(6.0 * u, 1.5 * u),
    ));
    let resp = ui.interact(add, ui.scope_id().with("add-device"), Sense::click());
    p.rect_stroke(
        add,
        0.0,
        m.hair(if resp.hovered() { t.text } else { t.line_2 }),
        StrokeKind::Inside,
    );
    p.text(
        add.center(),
        Align2::CENTER_CENTER,
        "+ DEVICE",
        m.mono(0.6875),
        t.muted,
    );
    if resp.clicked() && !was_open {
        ws.picker = Some(0.0);
    }
    if ws.picker.is_some() {
        if let Some(choice) = picker(ws, ui, m, r, inputs, was_open) {
            out.push(Request::AddDevice { track, choice });
        }
    }
}

/// The sampler's waveform with slice points (click adds one, right-click removes
/// the nearest) over its pads, and the drop target for files from 01.
#[allow(clippy::too_many_arguments)]
fn sampler_panel(
    ui: &egui::Ui,
    p: &egui::Painter,
    m: &Metrics,
    ws: &Workspace,
    r: Rect,
    inputs: &Inputs<'_>,
    device: &DeviceView,
    view: &SamplerView,
    (track, di): (usize, usize),
    covered: bool,
    out: &mut Vec<Request>,
) {
    let (t, u) = (ws.theme, m.u);
    let pad = 0.75 * u;
    let wave = m.snap_rect(Rect::from_x_y_ranges(
        r.min.x + pad..=r.max.x - pad,
        r.min.y..=r.min.y + 5.0 * u,
    ));
    p.rect_stroke(wave, 0.0, m.hair(t.line_2), StrokeKind::Inside);

    // Dropping a file from the browser loads it; the modifier picks how.
    let pointer = ui.input(|i| i.pointer.hover_pos());
    // Only where the panel is drawn: the rack clips modules that run past it.
    let over = pointer.is_some_and(|at| r.contains(at) && p.clip_rect().contains(at));
    if ws.file_drag.is_some() && over {
        p.rect_stroke(
            r.shrink(1.0),
            0.0,
            Stroke::new(0.125 * u, t.accent_amber),
            StrokeKind::Inside,
        );
    }
    if let (Some(path), true, false) = (&ws.file_drag, over, covered) {
        if ui.input(|i| i.pointer.any_released()) {
            let mods = ui.input(|i| i.modifiers);
            let drop = if mods.shift {
                SampleDrop::RoundRobin
            } else if mods.alt {
                SampleDrop::Velocity
            } else if mods.command {
                SampleDrop::Key
            } else {
                SampleDrop::Replace
            };
            out.push(Request::LoadSample {
                track,
                device: di,
                path: path.clone(),
                drop,
            });
        }
    }

    if let Some(w) = view
        .path
        .as_ref()
        .and_then(|path| inputs.waveforms.get(path))
    {
        let inner = wave.shrink(m.px);
        let cols = (inner.width() / m.px).floor().max(1.0) as usize;
        let per_px = w.audio().frames() as f64 / cols as f64;
        let mut bins = vec![MinMax::EMPTY; cols];
        w.view(0, 0.0, per_px, &mut bins);
        let (mid, half) = (inner.center().y, inner.height() * 0.4);
        let mut mesh = egui::Mesh::default();
        for (i, b) in bins.iter().enumerate().filter(|(_, b)| !b.is_empty()) {
            let x = inner.min.x + i as f32 * m.px;
            let (lo, hi) = (b.min.clamp(-1.0, 1.0), b.max.clamp(-1.0, 1.0));
            mesh.add_colored_rect(
                Rect::from_x_y_ranges(x..=x + m.px, (mid - hi * half)..=(mid - lo * half + m.px)),
                t.muted,
            );
        }
        p.add(mesh);
    }
    match view.path.as_ref() {
        None => {
            p.text(
                wave.center(),
                Align2::CENTER_CENTER,
                "DROP AUDIO FROM 01",
                m.mono(0.625),
                t.faint,
            );
        }
        Some(path) => {
            let label = format!(
                "{} // {} LAYER{}",
                crate::file_name(path).to_uppercase(),
                view.layers,
                if view.layers == 1 { "" } else { "S" }
            );
            p.text(
                Pos2::new(wave.min.x + 0.375 * u, wave.min.y + 0.5 * u),
                Align2::LEFT_CENTER,
                label,
                m.mono(0.5625),
                t.faint,
            );
        }
    }
    for (i, &at) in view.slices.iter().enumerate().skip(1) {
        let x = m.snap(wave.min.x + at as f32 * wave.width());
        p.vline(x, wave.y_range(), m.hair(t.text));
        p.text(
            Pos2::new(x + 0.25 * u, wave.max.y - 0.5 * u),
            Align2::LEFT_CENTER,
            format!("{:02}", i + 1),
            m.mono(0.5),
            t.muted,
        );
    }
    if view.path.is_some() && !covered {
        let resp = ui.interact(wave, ui.scope_id().with(("slice", di)), Sense::click());
        let at = |pos: Pos2| f64::from((pos.x - wave.min.x) / wave.width()).clamp(0.0, 1.0);
        if let Some(pos) = resp.interact_pointer_pos() {
            if resp.clicked() || resp.secondary_clicked() {
                out.push(Request::SliceMarker {
                    track,
                    device: di,
                    at: at(pos),
                    add: resp.clicked(),
                });
            }
        }
    }

    // Pads: slice mode maps slice n to ROOT + n; other modes play one sample.
    let param = |name: &str| {
        device
            .params
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.value)
    };
    let slicing = param("MODE").is_some_and(|v| v.round() == 2.0);
    let root = param("ROOT").map_or(60, |v| v.round().clamp(0.0, 127.0) as usize);
    let grid = Rect::from_x_y_ranges(wave.x_range(), wave.max.y + pad..=r.max.y - pad);
    let cell = Vec2::new(grid.width() / 4.0, grid.height() / 4.0);
    for i in 0..PADS {
        let min = grid.min + Vec2::new((i % 4) as f32 * cell.x, (3 - i / 4) as f32 * cell.y);
        let rect = m.snap_rect(Rect::from_min_size(min, cell).shrink(0.125 * u));
        let live = if slicing {
            i < view.slices.len()
        } else {
            i == 0 && view.path.is_some()
        };
        if live {
            p.rect_filled(rect, 0.0, t.sel);
        }
        p.rect_stroke(
            rect,
            0.0,
            m.hair(if live { t.line_2 } else { t.line }),
            StrokeKind::Inside,
        );
        let key = if slicing { root + i } else { root };
        if live && key <= 127 {
            p.text(
                rect.min + Vec2::splat(0.375 * u),
                Align2::LEFT_TOP,
                format!("{:02}", i + 1),
                m.mono(0.5625),
                t.text,
            );
            p.text(
                rect.max - Vec2::splat(0.375 * u),
                Align2::RIGHT_BOTTOM,
                note_name(key),
                m.mono(0.5),
                t.faint,
            );
        }
    }
}

/// A 270° arc dial: drag up or down to change. Returns the new value while dragged.
fn dial(
    ui: &egui::Ui,
    p: &egui::Painter,
    m: &Metrics,
    ws: &Workspace,
    area: Rect,
    param: &ParamView,
    // None while something (the picker) covers the dial: draw it, don't take input.
    id: Option<egui::Id>,
) -> Option<f64> {
    let (t, u) = (ws.theme, m.u);
    let radius = 1.125 * u;
    let center = Pos2::new(area.center().x, area.min.y + 0.5 * u + radius);
    let span = (param.max - param.min).max(f64::MIN_POSITIVE);
    // Position on the dial (0..=1) and back; log ranges spread octaves evenly.
    let log = param.log && param.min > 0.0;
    let to_frac = |v: f64| {
        let f = if log {
            (v / param.min).ln() / (param.max / param.min).ln()
        } else {
            (v - param.min) / span
        };
        f.clamp(0.0, 1.0)
    };
    let from_frac = |f: f64| {
        if log {
            param.min * (param.max / param.min).powf(f)
        } else {
            param.min + f * span
        }
    };
    let frac = to_frac(param.value) as f32;
    // From 225° (lower left) clockwise through the top to -45° (lower right).
    let angle = |f: f32| PI * 1.25 - f * PI * 1.5;
    let arc = |from: f32, to: f32| -> Vec<Pos2> {
        let steps = 32;
        (0..=steps)
            .map(|i| {
                let a = angle(from + (to - from) * i as f32 / steps as f32);
                Pos2::new(center.x + radius * a.cos(), center.y - radius * a.sin())
            })
            .collect()
    };
    p.add(Shape::line(arc(0.0, 1.0), Stroke::new(0.125 * u, t.line_2)));
    if frac > 0.0 {
        p.add(Shape::line(
            arc(0.0, frac),
            Stroke::new(0.15625 * u, t.accent_amber),
        ));
    }
    let a = angle(frac);
    p.line_segment(
        [
            center,
            Pos2::new(center.x + radius * a.cos(), center.y - radius * a.sin()),
        ],
        Stroke::new(0.125 * u, t.text),
    );
    let readout = p.text(
        Pos2::new(center.x, center.y + radius + 0.625 * u),
        Align2::CENTER_CENTER,
        format!("{:.2}", param.value),
        m.mono(0.75),
        t.text,
    );
    // Eight mono characters fit a dial's cell.
    let name: String = param.name.chars().take(8).collect();
    p.text(
        Pos2::new(center.x, readout.max.y + 0.5 * u),
        Align2::CENTER_CENTER,
        name.to_uppercase(),
        m.mono(0.5625),
        t.faint,
    );

    let hit = Rect::from_center_size(center, Vec2::splat(radius * 2.4));
    let resp = ui.interact(hit, id?, Sense::drag());
    if resp.dragged() {
        let dy = f64::from(-resp.drag_delta().y);
        // Full range over 200 points; hold shift for fine control.
        let fine = if ui.input(|i| i.modifiers.shift) {
            0.1
        } else {
            1.0
        };
        // max() then min(), not clamp(): a bad range must not panic the UI.
        let v = from_frac(to_frac(param.value) + dy / 200.0 * fine)
            .max(param.min)
            .min(param.max);
        return (v != param.value).then_some(v);
    }
    None
}

/// The plugin list, as a modal over the rack: the one place with rounded corners.
fn picker(
    ws: &mut Workspace,
    ui: &egui::Ui,
    m: &Metrics,
    rack: Rect,
    inputs: &Inputs<'_>,
    // False on the frame it opens: the click that opened it must not close it.
    was_open: bool,
) -> Option<usize> {
    let (t, u) = (ws.theme, m.u);
    let p = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        ui.scope_id().with("picker"),
    ));
    let modal = Rect::from_center_size(
        rack.center(),
        Vec2::new(
            (32.0 * u).min(rack.width() - 2.0 * u),
            (rack.height() - 2.0 * u).max(8.0 * u),
        ),
    );
    // 8pt at 1u = 16pt (0.5u), scaled like everything else.
    let radius = egui::CornerRadius::same((0.5 * u).round().min(255.0) as u8);
    p.rect_filled(modal, radius, t.bg_panel);
    p.rect_stroke(modal, radius, m.hair(t.line_2), StrokeKind::Inside);
    p.text(
        Pos2::new(modal.min.x + 0.75 * u, modal.min.y + u),
        Align2::LEFT_CENTER,
        "ADD DEVICE // ESC TO CLOSE",
        m.mono(0.6875),
        t.muted,
    );
    let list = Rect::from_x_y_ranges(
        modal.x_range(),
        m.snap(modal.min.y + 2.0 * u)..=modal.max.y - 0.5 * u,
    );
    p.hline(modal.x_range(), list.min.y, m.hair(t.line));
    let row_h = 2.0 * u;
    let max_scroll = (inputs.plugins.len() as f32 * row_h - list.height()).max(0.0);
    let mut scroll = ws.picker.unwrap_or(0.0);
    if ui.rect_contains_pointer(list) {
        scroll = (scroll - ui.input(|i| i.smooth_scroll_delta.y)).clamp(0.0, max_scroll);
    }
    let lp = p.with_clip_rect(list);
    if inputs.plugins.is_empty() {
        lp.text(
            list.center(),
            Align2::CENTER_CENTER,
            "NO PLUGINS FOUND",
            m.mono(0.625),
            t.faint,
        );
    }
    let mut chosen = None;
    for (i, choice) in inputs.plugins.iter().enumerate() {
        let y = list.min.y + i as f32 * row_h - scroll;
        if y + row_h < list.min.y || y > list.max.y {
            continue;
        }
        let row = Rect::from_x_y_ranges(list.x_range(), y..=m.snap(y + row_h)).intersect(list);
        let resp = ui.interact(row, ui.scope_id().with(("plugin", i)), Sense::click());
        if resp.hovered() {
            lp.rect_filled(row, 0.0, t.sel);
        }
        let cy = y + row_h / 2.0;
        let name = lp.text(
            Pos2::new(list.min.x + 0.75 * u, cy),
            Align2::LEFT_CENTER,
            &choice.name,
            m.sans(0.75),
            t.text,
        );
        lp.text(
            Pos2::new(name.max.x + 0.75 * u, cy),
            Align2::LEFT_CENTER,
            &choice.vendor,
            m.sans(0.6875),
            t.muted,
        );
        lp.text(
            Pos2::new(list.max.x - 0.75 * u, cy),
            Align2::RIGHT_CENTER,
            choice.format,
            m.mono(0.625),
            t.faint,
        );
        lp.hline(list.x_range(), m.snap(y + row_h), m.hair(t.line));
        if resp.clicked() {
            chosen = Some(i);
        }
    }
    let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
    let outside = ui.input(|i| i.pointer.primary_clicked())
        && ui
            .input(|i| i.pointer.interact_pos())
            .is_some_and(|p| !modal.contains(p));
    let close = was_open && (escape || outside);
    ws.picker = if chosen.is_some() || close {
        None
    } else {
        Some(scroll)
    };
    chosen
}
