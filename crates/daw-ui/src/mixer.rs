//! The MIXER view: a channel strip per track with output routing, sends to
//! buses, a pan dial, a ladder fader beside a peak meter, and mute/solo. Fader
//! and dial drags are one undo step each (`History::apply_gesture`).

use daw_project::{Action, Send, Track, TrackProps};
use egui::{Align2, Pos2, Rect, Sense, StrokeKind, Vec2};

use crate::rack::{dial, ParamView};
use crate::theme::Metrics;
use crate::{header, Edit, Inputs, Workspace};

/// Fader travel, in dB: -60 at the bottom (then silence), +6 at the top.
const FADER_MIN_DB: f32 = -60.0;
const FADER_MAX_DB: f32 = 6.0;
/// Sends shown per strip.
const SENDS: usize = 4;

fn gain_to_pos(gain: f32) -> f32 {
    if gain <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * gain.log10();
    ((db - FADER_MIN_DB) / (FADER_MAX_DB - FADER_MIN_DB)).clamp(0.0, 1.0)
}

fn pos_to_gain(pos: f32) -> f32 {
    if pos <= 0.0 {
        return 0.0;
    }
    let db = FADER_MIN_DB + pos.clamp(0.0, 1.0) * (FADER_MAX_DB - FADER_MIN_DB);
    10f32.powf(db / 20.0)
}

/// Gesture ids: one per strip control, so each drag is its own undo step.
fn gesture(track: usize, control: u64) -> u64 {
    (track as u64) << 8 | control
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
    let top = header(p, m, &t, r, "02A // MIXER");
    let body = Rect::from_x_y_ranges(r.x_range(), top..=r.max.y);
    let strip_w = 6.0 * u;
    let total = (project.tracks.len() as f32 + 1.0) * strip_w;
    if ui.rect_contains_pointer(body) {
        let d = ui.input(|i| i.smooth_scroll_delta);
        ws.mixer_scroll -= if d.x != 0.0 { d.x } else { d.y };
    }
    ws.mixer_scroll = ws.mixer_scroll.clamp(0.0, (total - body.width()).max(0.0));
    let buses: Vec<(u64, &Track)> = project
        .tracks
        .iter()
        .filter(|t| t.bus)
        .map(|t| (t.id, t))
        .collect();

    let mut x = body.min.x - ws.mixer_scroll;
    for (ti, track) in project.tracks.iter().enumerate() {
        let strip = Rect::from_x_y_ranges(x..=m.snap(x + strip_w), body.y_range());
        x = strip.max.x;
        if !strip.intersects(body) {
            continue;
        }
        let sp = p.with_clip_rect(strip.intersect(body));
        let selected = ws.selected_track == Some(ti);
        if selected {
            sp.rect_filled(strip, 0.0, t.sel);
            sp.rect_filled(
                Rect::from_x_y_ranges(strip.x_range(), strip.min.y..=strip.min.y + 0.125 * u),
                0.0,
                t.accent_amber,
            );
        }
        sp.vline(strip.max.x, strip.y_range(), m.hair(t.line));
        let pad = 0.5 * u;
        let mut y = strip.min.y + 0.75 * u;
        // Index, name, and the BUS tag.
        sp.text(
            Pos2::new(strip.min.x + pad, y),
            Align2::LEFT_CENTER,
            format!("{:02}", ti + 1),
            m.mono(0.625),
            t.faint,
        );
        if track.bus {
            sp.text(
                Pos2::new(strip.max.x - pad, y),
                Align2::RIGHT_CENTER,
                "BUS",
                m.mono(0.5625),
                t.accent_cyan,
            );
        }
        y += 1.0 * u;
        let name = sp.text(
            Pos2::new(strip.min.x + pad, y),
            Align2::LEFT_CENTER,
            &track.name,
            m.sans(0.6875),
            if track.mute { t.faint } else { t.text },
        );
        let name_resp = ui.interact(
            Rect::from_x_y_ranges(strip.x_range(), strip.min.y..=name.max.y + 0.25 * u)
                .intersect(body),
            ui.scope_id().with(("strip", ti)),
            Sense::click(),
        );
        if name_resp.clicked() {
            ws.selected_track = Some(ti);
        }
        y += u;
        sp.hline(strip.x_range(), m.snap(y), m.hair(t.line));
        y += 0.25 * u;

        // Output: click to step through the master and every other bus.
        let out_name = project
            .output_of(ti)
            .map_or("MST".to_string(), |j| short(&project.tracks[j].name));
        let row = Rect::from_x_y_ranges(strip.min.x + pad..=strip.max.x - pad, y..=y + 1.25 * u);
        let resp = ui.interact(
            row.intersect(body),
            ui.scope_id().with(("output", ti)),
            Sense::click(),
        );
        sp.rect_stroke(
            row,
            0.0,
            m.hair(if resp.hovered() { t.text } else { t.line_2 }),
            StrokeKind::Inside,
        );
        sp.text(
            row.center(),
            Align2::CENTER_CENTER,
            format!("→ {out_name}"),
            m.mono(0.5625),
            t.muted,
        );
        if resp.clicked() {
            let choices: Vec<Option<u64>> = std::iter::once(None)
                .chain(
                    buses
                        .iter()
                        .filter(|(id, _)| *id != track.id)
                        .map(|b| Some(b.0)),
                )
                .collect();
            let now = choices.iter().position(|&c| c == track.output).unwrap_or(0);
            // The next choice on that does not loop a bus back into itself.
            let next = (1..choices.len())
                .map(|k| Action::SetRouting {
                    track: ti,
                    output: choices[(now + k) % choices.len()],
                    sends: track.sends.clone(),
                })
                .find(|a| a.applies_to(project));
            if let Some(action) = next {
                ws.edit(action);
            }
        }
        y = row.max.y + 0.375 * u;

        // Sends: drag a row up or down to set its level; double-click clears it.
        for (k, (bus_id, bus)) in buses
            .iter()
            .filter(|(id, _)| *id != track.id)
            .take(SENDS)
            .enumerate()
        {
            let row =
                Rect::from_x_y_ranges(strip.min.x + pad..=strip.max.x - pad, y..=y + 1.125 * u);
            let level = track
                .sends
                .iter()
                .find(|s| s.to == *bus_id)
                .map_or(0.0, |s| s.level);
            let filled = row.min.x + row.width() * (level / 2.0).clamp(0.0, 1.0);
            sp.rect_filled(
                Rect::from_x_y_ranges(row.min.x..=filled, row.y_range()),
                0.0,
                t.bg_void,
            );
            if level > 0.0 {
                sp.rect_filled(
                    Rect::from_x_y_ranges(row.min.x..=filled, row.max.y - 0.125 * u..=row.max.y),
                    0.0,
                    t.accent_cyan,
                );
            }
            sp.text(
                Pos2::new(row.min.x + 0.25 * u, row.center().y),
                Align2::LEFT_CENTER,
                short(&bus.name),
                m.mono(0.5),
                t.muted,
            );
            sp.text(
                Pos2::new(row.max.x - 0.25 * u, row.center().y),
                Align2::RIGHT_CENTER,
                format!("{level:.2}"),
                m.mono(0.5),
                if level > 0.0 { t.text } else { t.faint },
            );
            let resp = ui.interact(
                row.intersect(body),
                ui.scope_id().with(("send", ti, k)),
                Sense::click_and_drag(),
            );
            let mut set = None;
            if resp.dragged() {
                let d = resp.drag_delta();
                set = Some((level + (d.x - d.y) / 100.0).clamp(0.0, 2.0));
            }
            if resp.double_clicked() {
                set = Some(0.0);
            }
            if let Some(level) = set {
                let mut sends: Vec<Send> = track
                    .sends
                    .iter()
                    .filter(|s| s.to != *bus_id)
                    .copied()
                    .collect();
                if level > 0.0 {
                    sends.push(Send { to: *bus_id, level });
                }
                let action = Action::SetRouting {
                    track: ti,
                    output: track.output,
                    sends,
                };
                ws.edits
                    .push(Edit::Gesture(action, gesture(ti, 10 + k as u64)));
            }
            y = row.max.y + 0.25 * u;
        }
        // Every strip reserves the same send rows, so faders line up.
        let rows = buses.len().min(SENDS) as f32;
        y = y.max(strip.min.y + 4.0 * u + rows * 1.375 * u + 0.5 * u);
        sp.hline(strip.x_range(), m.snap(y), m.hair(t.line));

        // Pan.
        let pan_area = Rect::from_x_y_ranges(strip.x_range(), y..=y + 4.25 * u);
        let pan = ParamView {
            id: 0,
            name: "PAN".into(),
            value: f64::from(track.pan),
            min: -1.0,
            max: 1.0,
            log: false,
        };
        let pan_id = ui.scope_id().with(("pan", ti));
        if let Some(v) = dial(ui, &sp, m, ws, pan_area, &pan, Some(pan_id)) {
            let props = TrackProps {
                pan: v as f32,
                ..TrackProps::of(track)
            };
            ws.edits.push(Edit::Gesture(
                Action::SetTrackProps { index: ti, props },
                gesture(ti, 1),
            ));
        }
        y = pan_area.max.y;

        // Fader and meter, over the gain readout and M/S.
        let bottom = strip.max.y - 3.25 * u;
        let lane = Rect::from_x_y_ranges(strip.x_range(), y + 0.5 * u..=bottom);
        let trough_x = lane.center().x - 0.75 * u;
        let trough = Rect::from_x_y_ranges(
            trough_x - 0.1875 * u..=trough_x + 0.1875 * u,
            lane.y_range(),
        );
        sp.rect_filled(trough, 0.0, t.bg_void);
        for db in [6.0, 0.0, -6.0, -12.0, -24.0, -48.0] {
            let ty = lane.max.y - lane.height() * gain_to_pos(10f32.powf(db / 20.0));
            let ty = m.snap(ty);
            sp.hline(
                trough.min.x - 0.5 * u..=trough.min.x - 0.125 * u,
                ty,
                m.hair(t.faint),
            );
            sp.hline(
                trough.max.x + 0.125 * u..=trough.max.x + 0.5 * u,
                ty,
                m.hair(t.faint),
            );
        }
        let cap_y = lane.max.y - lane.height() * gain_to_pos(track.gain);
        let cap = m.snap_rect(Rect::from_center_size(
            Pos2::new(trough_x, cap_y),
            Vec2::new(1.875 * u, 0.625 * u),
        ));
        sp.rect_filled(cap, 0.0, t.text);
        sp.hline(
            cap.x_range(),
            m.snap(cap.center().y),
            egui::Stroke::new(0.09375 * u, t.bg_void),
        );
        let resp = ui.interact(
            lane.intersect(body),
            ui.scope_id().with(("fader", ti)),
            Sense::click_and_drag(),
        );
        if resp.dragged() {
            let fine = if ui.input(|i| i.modifiers.shift) {
                0.1
            } else {
                1.0
            };
            let pos = gain_to_pos(track.gain) - resp.drag_delta().y / lane.height() * fine;
            let props = TrackProps {
                gain: pos_to_gain(pos),
                ..TrackProps::of(track)
            };
            ws.edits.push(Edit::Gesture(
                Action::SetTrackProps { index: ti, props },
                gesture(ti, 2),
            ));
        }
        if resp.double_clicked() {
            let props = TrackProps {
                gain: 1.0,
                ..TrackProps::of(track)
            };
            ws.edit(Action::SetTrackProps { index: ti, props });
        }
        // Peak meter: green, amber in the last 3 dB, red at clipping.
        let peak = inputs.meters.get(ti).copied().unwrap_or([0.0; 2]);
        for (side, level) in peak.iter().enumerate() {
            let mx = trough.max.x + 1.0 * u + side as f32 * 0.5 * u;
            let well = Rect::from_x_y_ranges(mx..=mx + 0.375 * u, lane.y_range());
            sp.rect_filled(well, 0.0, t.bg_void);
            let h = lane.height() * gain_to_pos(*level);
            if h > 0.0 {
                let color = if *level >= 1.0 {
                    t.bad
                } else if *level >= 10f32.powf(-3.0 / 20.0) {
                    t.accent_amber
                } else {
                    t.good
                };
                sp.rect_filled(
                    Rect::from_x_y_ranges(well.x_range(), well.max.y - h..=well.max.y),
                    0.0,
                    color,
                );
            }
        }
        let db = if track.gain > 0.0 {
            format!("{:+.1}", 20.0 * track.gain.log10())
        } else {
            "-INF".into()
        };
        sp.text(
            Pos2::new(strip.center().x, bottom + 0.75 * u),
            Align2::CENTER_CENTER,
            db,
            m.mono(0.6875),
            t.text,
        );
        let side = 1.25 * u;
        for (k, (label, on, fill)) in [("M", track.mute, t.ink), ("S", track.solo, t.accent_amber)]
            .into_iter()
            .enumerate()
        {
            let b = m.snap_rect(Rect::from_min_size(
                Pos2::new(
                    strip.center().x - side - 0.125 * u + k as f32 * (side + 0.25 * u),
                    bottom + 1.5 * u,
                ),
                Vec2::splat(side),
            ));
            let resp = ui.interact(
                b.intersect(body),
                ui.scope_id().with((label, "strip", ti)),
                Sense::click(),
            );
            if on {
                sp.rect_filled(b, 0.0, fill);
            } else {
                sp.rect_stroke(b, 0.0, m.hair(t.line_2), StrokeKind::Inside);
            }
            sp.text(
                b.center(),
                Align2::CENTER_CENTER,
                label,
                m.mono(0.5625),
                if on { t.ink_text } else { t.muted },
            );
            if resp.clicked() {
                let mut props = TrackProps::of(track);
                if k == 0 {
                    props.mute ^= true;
                } else {
                    props.solo ^= true;
                }
                ws.edit(Action::SetTrackProps { index: ti, props });
            }
        }
    }

    // + BUS after the strips.
    let add = p.with_clip_rect(body).text(
        Pos2::new(x + 0.75 * u, body.min.y + 0.75 * u),
        Align2::LEFT_CENTER,
        "+ BUS",
        m.mono(0.6875),
        t.muted,
    );
    let resp = ui.interact(
        add.expand(0.25 * u).intersect(body),
        ui.scope_id().with("add-bus"),
        Sense::click(),
    );
    if resp.hovered() {
        p.hline(add.x_range(), m.snap(add.max.y), m.hair(t.muted));
    }
    if resp.clicked() {
        let index = project.tracks.len();
        let buses = project.tracks.iter().filter(|t| t.bus).count();
        let mut track = Track::new(format!("Bus {}", buses + 1));
        track.bus = true;
        ws.edit(Action::InsertTrack { index, track });
        ws.selected_track = Some(index);
    }
}

/// Six characters of a name, for the narrow strip.
fn short(name: &str) -> String {
    name.chars().take(6).collect::<String>().to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fader_positions_and_gains_round_trip() {
        assert_eq!(gain_to_pos(0.0), 0.0);
        assert_eq!(pos_to_gain(0.0), 0.0);
        assert!((gain_to_pos(1.0) - 60.0 / 66.0).abs() < 1e-6);
        for g in [0.01, 0.5, 1.0, 1.9] {
            assert!((pos_to_gain(gain_to_pos(g)) - g).abs() < 1e-4);
        }
        assert_eq!(gain_to_pos(10.0), 1.0, "clamped at +6 dB");
    }
}
