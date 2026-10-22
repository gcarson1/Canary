//! 02B // HARDWARE_DSP_RACK: the selected track's plugin devices as modules with
//! arc dials (Styles.md 4.3), and the picker that adds one.

use std::f32::consts::PI;

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
    /// Short format tag: `AU`, `CLAP` or `VST3`.
    pub format: &'static str,
}

/// One device on the selected track, with live parameter values.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceView {
    pub name: String,
    pub params: Vec<ParamView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParamView {
    pub id: u32,
    pub name: String,
    pub value: f64,
    pub min: f64,
    pub max: f64,
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

    let module_w = 16.0 * u;
    let mut x = body.min.x;
    for (di, device) in inputs.devices.iter().enumerate() {
        let module = Rect::from_x_y_ranges(x..=m.snap(x + module_w), body.y_range());
        let p = p.with_clip_rect(module.intersect(p.clip_rect()));
        let top = module.min.y + 1.25 * u;
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
        if resp.clicked() {
            ws.edit(Action::RemoveDevice { track, index: di });
        }

        // Dials in two rows of four.
        let cell = Vec2::new(module_w / 4.0, (module.height() - 2.5 * u) / 2.0);
        for (pi, param) in device.params.iter().take(DIALS).enumerate() {
            let origin = Pos2::new(
                module.min.x + (pi % 4) as f32 * cell.x,
                top + 1.25 * u + (pi / 4) as f32 * cell.y,
            );
            let area = Rect::from_min_size(origin, cell);
            let id = ui.scope_id().with(("dial", di, param.id));
            if let Some(v) = dial(ui, &p, m, ws, area, param, id) {
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
    if resp.clicked() {
        ws.picker = Some(0.0);
    }
    if ws.picker.is_some() {
        picker(ws, ui, m, r, inputs, track, out);
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
    id: egui::Id,
) -> Option<f64> {
    let (t, u) = (ws.theme, m.u);
    let radius = 1.125 * u;
    let center = Pos2::new(area.center().x, area.min.y + 0.5 * u + radius);
    let span = (param.max - param.min).max(f64::MIN_POSITIVE);
    let frac = ((param.value - param.min) / span).clamp(0.0, 1.0) as f32;
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
    let resp = ui.interact(hit, id, Sense::drag());
    if resp.dragged() {
        let dy = f64::from(-resp.drag_delta().y);
        // Full range over 200 points; hold shift for fine control.
        let fine = if ui.input(|i| i.modifiers.shift) {
            0.1
        } else {
            1.0
        };
        let v = (param.value + dy / 200.0 * span * fine).clamp(param.min, param.max);
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
    track: usize,
    out: &mut Vec<Request>,
) {
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
    let radius = egui::CornerRadius::same(8);
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
    let close = ui.input(|i| i.key_pressed(egui::Key::Escape))
        || ui.input(|i| i.pointer.primary_clicked())
            && ui
                .input(|i| i.pointer.interact_pos())
                .is_some_and(|p| !modal.contains(p));
    ws.picker = if chosen.is_some() || close {
        None
    } else {
        Some(scroll)
    };
    if let Some(choice) = chosen {
        out.push(Request::AddDevice { track, choice });
    }
}
