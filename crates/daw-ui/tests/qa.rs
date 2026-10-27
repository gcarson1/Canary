//! Styles.md section 6 QA protocol, automated: the workspace rendered at the three
//! required viewports in both themes (PNGs land in `target/qa/` for review), and
//! audits of every painted shape for corner radius and color discipline.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use daw_core::SampleData;
use daw_dsp::WaveformPyramid;
use daw_project::{Action, ClipRef, History, Track};
use daw_ui::{
    theme, DeviceView, EngineView, Inputs, ParamView, PluginChoice, Request, Theme, Workspace,
};
use egui::epaint::ColorMode;
use egui::{Color32, Shape, Vec2};
use egui_kittest::Harness;

/// Logical viewports and pixel densities: the three sizes Styles.md requires,
/// plus 1440x900 on a Retina display, where hairlines must stay one pixel.
const SIZES: [(f32, f32, f32); 4] = [
    (1440.0, 900.0, 1.0),
    (2560.0, 1440.0, 1.0),
    (3840.0, 2160.0, 1.0),
    (1440.0, 900.0, 2.0),
];

/// A small arrangement: two tracks of synthetic audio, one clip trimmed with fades.
fn fixture() -> (History, HashMap<PathBuf, WaveformPyramid>, Vec<PathBuf>) {
    let mut waveforms = HashMap::new();
    let mut add = |name: &str, f: fn(usize) -> f32| {
        let path = PathBuf::from(format!("/samples/{name}.wav"));
        let left: Vec<f32> = (0..96_000).map(f).collect();
        let right = left.iter().map(|s| s * 0.6).collect();
        let audio = Arc::new(SampleData::new(48_000, vec![left, right]).unwrap());
        waveforms.insert(path.clone(), WaveformPyramid::new(audio));
        path
    };
    let kick = add("808_Thermal_Sub", |i| {
        let t = (i % 24_000) as f32 / 48_000.0;
        (t * 55.0 * std::f32::consts::TAU).sin() * (-t * 9.0).exp()
    });
    let pad = add("Mayo_Juno_Poly", |i| {
        (i as f32 * 0.031).sin() * 0.5 + (i as f32 * 0.0007).sin() * 0.3
    });
    let clip = |source: &PathBuf, start, offset, length, fade_in, fade_out| ClipRef {
        source: source.clone(),
        start,
        offset,
        length,
        fade_in,
        fade_out,
    };
    let mut h = History::default();
    for (i, (name, clips)) in [
        (
            "808_Thermal_Sub",
            vec![
                clip(&kick, 0, 0, 96_000, 0, 0),
                clip(&kick, 192_000, 0, 96_000, 0, 4_000),
            ],
        ),
        (
            "Mayo_Juno_Poly",
            vec![clip(&pad, 96_000, 12_000, 72_000, 12_000, 12_000)],
        ),
        ("Desert_Delay_Tex", vec![]),
    ]
    .into_iter()
    .enumerate()
    {
        let mut track = Track::new(name);
        track.clips = clips;
        track.mute = i == 2;
        h.apply(Action::InsertTrack { index: i, track }).unwrap();
    }
    (h, waveforms, vec![kick, pad])
}

fn harness(size: (f32, f32, f32), theme: Theme, engine: EngineView) -> Harness<'static> {
    let (mut history, waveforms, files) = fixture();
    let mut ws = Workspace::default();
    ws.theme = theme;
    ws.selected_track = Some(0);
    ws.selected_clip = Some((1, 0));
    Harness::builder()
        .with_size(Vec2::new(size.0, size.1))
        .with_pixels_per_point(size.2)
        .with_context_setup(theme::install_fonts)
        .build_ui(move |ui| {
            full_window(ui, |ui| {
                ws.show(
                    ui,
                    Inputs {
                        history: &mut history,
                        engine: &engine,
                        waveforms: &waveforms,
                        files: &files,
                        plugins: &plugins(),
                        devices: &devices(),
                    },
                );
            })
        })
}

fn plugins() -> Vec<PluginChoice> {
    ["AUDelay", "AUMatrixReverb", "DLSMusicDevice"]
        .iter()
        .map(|n| PluginChoice {
            name: n.to_string(),
            vendor: "Apple".into(),
            format: "AU",
        })
        .collect()
}

/// Two effects on the selected track with parameters at different values.
fn devices() -> Vec<DeviceView> {
    let param = |id: u32, name: &str, value: f64| ParamView {
        id,
        name: name.into(),
        value,
        min: 0.0,
        max: 100.0,
        log: false,
    };
    vec![
        DeviceView {
            name: "AUDelay".into(),
            params: vec![
                param(0, "Dry Wet Mix", 50.0),
                param(1, "Delay Time", 10.0),
                param(2, "Feedback", 0.0),
                param(3, "Lowpass", 100.0),
            ],
        },
        DeviceView {
            // A full built-in module: eight dials, a log-scaled frequency first.
            name: "Dynamic EQ".into(),
            params: [
                ("FREQ", 20.0, 20_000.0, 200.0, true),
                ("Q", 0.3, 10.0, 1.0, false),
                ("THRESH", -60.0, 0.0, -24.0, false),
                ("RANGE", 0.0, 24.0, 12.0, false),
                ("LOWCUT", 0.0, 1.0, 1.0, false),
                ("MIX", 0.0, 100.0, 100.0, false),
                ("MATCH", 0.0, 1.0, 1.0, false),
                ("DELTA", 0.0, 1.0, 0.0, false),
            ]
            .iter()
            .enumerate()
            .map(|(i, &(name, min, max, value, log))| ParamView {
                id: i as u32,
                name: name.into(),
                value,
                min,
                max,
                log,
            })
            .collect(),
        },
    ]
}

/// The harness insets the ui by a margin; the app draws edge to edge, so do that.
fn full_window(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let rect = ui.ctx().content_rect();
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        ui.set_clip_rect(rect);
        add(ui);
    });
}

fn playing() -> EngineView {
    EngineView {
        playhead: 120_000,
        playing: true,
        peak: [0.71, 1.02], // right channel clipped
        rms: [0.25, 0.3],
        ..EngineView::default()
    }
}

/// Every color a shape paints with, with its corner radius check alongside.
fn audit(shape: &Shape, colors: &mut Vec<Color32>, rounded: &mut usize) {
    match shape {
        Shape::Vec(shapes) => shapes.iter().for_each(|s| audit(s, colors, rounded)),
        Shape::Rect(r) => {
            *rounded += usize::from(r.corner_radius != egui::CornerRadius::ZERO);
            colors.extend([r.fill, r.stroke.color]);
        }
        Shape::LineSegment { stroke, .. } => colors.push(stroke.color),
        Shape::Circle(c) => colors.extend([c.fill, c.stroke.color]),
        Shape::Path(p) => {
            colors.push(p.fill);
            if let ColorMode::Solid(c) = p.stroke.color {
                colors.push(c);
            }
        }
        Shape::Text(t) => {
            colors.push(t.fallback_color);
            colors.extend(t.galley.job.sections.iter().map(|s| s.format.color));
            colors.extend(t.override_text_color);
        }
        Shape::Mesh(m) => colors.extend(m.vertices.iter().map(|v| v.color)),
        Shape::Noop => {}
        other => panic!("unaudited shape kind: {other:?}"),
    }
}

#[test]
fn every_viewport_and_theme_renders_and_passes_the_audits() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/qa");
    std::fs::create_dir_all(&dir).unwrap();
    for (theme, name) in [(Theme::DARK, "dark"), (Theme::LIGHT, "light")] {
        let allowed = theme.tokens();
        for size in SIZES {
            let mut h = harness(size, theme, playing());
            h.run();
            let (mut colors, mut rounded) = (Vec::new(), 0);
            for clipped in &h.output().shapes {
                audit(&clipped.shape, &mut colors, &mut rounded);
            }
            assert_eq!(rounded, 0, "{name} {size:?}: rounded corners");
            let stray: Vec<_> = colors
                .iter()
                .filter(|c| c.a() != 0 && !allowed.contains(c))
                .collect();
            assert!(
                stray.is_empty(),
                "{name} {size:?}: off-palette colors {stray:?}"
            );

            let image = h.render().expect("wgpu render");
            image
                .save(dir.join(format!(
                    "workspace-{name}-{}x{}@{}x.png",
                    size.0, size.1, size.2
                )))
                .unwrap();
        }
    }
}

/// Drives the workspace at 1440x900 (u = 16pt) with scripted input.
struct Rig {
    history: History,
    requests: Vec<Request>,
}

fn press(h: &mut Harness<'_, Rig>, pos: egui::Pos2, pressed: bool) {
    h.event(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    });
}

/// Press, a frame, release, a frame: a click as egui sees one.
fn click(h: &mut Harness<'_, Rig>, pos: egui::Pos2) {
    h.hover_at(pos);
    h.run();
    press(h, pos, true);
    h.run();
    press(h, pos, false);
    h.run();
}

fn rig() -> Harness<'static, Rig> {
    rig_with(devices())
}

/// The rig with `rack` as the selected track's devices.
fn rig_with(rack: Vec<DeviceView>) -> Harness<'static, Rig> {
    let (history, waveforms, files) = fixture();
    let engine = EngineView::default();
    let mut ws = Workspace::default();
    ws.selected_track = Some(0);
    Harness::builder()
        .with_size(Vec2::new(1440.0, 900.0))
        .with_context_setup(theme::install_fonts)
        .build_ui_state(
            move |ui, rig: &mut Rig| {
                full_window(ui, |ui| {
                    let out = ws.show(
                        ui,
                        Inputs {
                            history: &mut rig.history,
                            engine: &engine,
                            waveforms: &waveforms,
                            files: &files,
                            plugins: &plugins(),
                            devices: &rack,
                        },
                    );
                    rig.requests.extend(out);
                })
            },
            Rig {
                history,
                requests: Vec::new(),
            },
        )
}

#[test]
fn space_plays_once_per_press() {
    let mut h = rig();
    h.run();
    h.key_press(egui::Key::Space);
    h.run();
    assert_eq!(h.state().requests, [Request::Play]);
}

#[test]
fn dragging_a_clip_moves_it_across_lanes_and_delete_mid_drag_is_safe() {
    // Lanes start at x = 280 + 120 = 400 and y = 56 + 40 + 28 = 124, 64pt each;
    // 400 samples per point. Track 0's first clip spans x 400..640.
    let mut h = rig();
    h.run();
    let start = egui::Pos2::new(500.0, 165.0);
    h.hover_at(start);
    press(&mut h, start, true);
    h.run();
    for step in 1..=10 {
        h.hover_at(start + Vec2::new(2.0 * step as f32, 6.0 * step as f32));
        h.run();
    }
    let end = start + Vec2::new(20.0, 60.0); // one lane down, 8 000 samples right
    press(&mut h, end, false);
    h.run();
    let tracks = &h.state().history.project().tracks;
    assert_eq!(tracks[0].clips.len(), 1, "moved off track 0");
    assert_eq!(tracks[1].clips.len(), 2, "landed on track 1");
    assert_eq!(tracks[1].clips[1].start, 6_000, "snapped to the 1/16 grid");

    // Start dragging the same clip, delete it mid-drag, then let go.
    let before = tracks[1].clips[0].clone();
    let grab = egui::Pos2::new(410.0, 230.0);
    h.hover_at(grab);
    press(&mut h, grab, true);
    h.run();
    for step in 1..=5 {
        h.hover_at(grab + Vec2::new(4.0 * step as f32, 0.0));
        h.run();
    }
    h.key_press(egui::Key::Backspace);
    h.run();
    press(&mut h, grab + Vec2::new(40.0, 0.0), false);
    h.run();
    let tracks = &h.state().history.project().tracks;
    assert_eq!(
        tracks[1].clips,
        [before],
        "only the dragged clip went; no other clip was overwritten"
    );
    assert_eq!(tracks.len(), 3, "one Backspace removes one thing");
}

#[test]
fn picker_adds_a_device_and_dials_set_params() {
    // Rack: x 280..1100, y 520..900 (45% of the body), header 32pt. The two
    // devices are 16u = 256pt wide, so + DEVICE (96 x 24) starts at x 804, y 564.
    let mut h = rig();
    h.run();
    let add = egui::Pos2::new(852.0, 576.0);
    click(&mut h, add);
    std::fs::create_dir_all("../../target/qa").unwrap();
    h.render()
        .unwrap()
        .save("../../target/qa/picker-open.png")
        .unwrap();
    // The picker (rack height - 2u, centred: y 536..884) lists 32pt rows from
    // y 568; the second row is AUMatrixReverb.
    let row = egui::Pos2::new(700.0, 568.0 + 32.0 + 16.0);
    click(&mut h, row);
    assert!(
        h.state().requests.contains(&Request::AddDevice {
            track: 0,
            choice: 1
        }),
        "{:?}",
        h.state().requests
    );

    // Drag the first dial up: Dry Wet Mix rises from 50.
    // First dial: cell 64pt wide from x 280; centre y = 552 + 20 + 20 + 8 + 18.
    let dial = egui::Pos2::new(312.0, 618.0);
    h.hover_at(dial);
    press(&mut h, dial, true);
    h.run();
    for step in 1..=5 {
        h.hover_at(dial - Vec2::new(0.0, 4.0 * step as f32));
        h.run();
    }
    press(&mut h, dial - Vec2::new(0.0, 20.0), false);
    h.run();
    let set = h.state().requests.iter().find_map(|r| match r {
        Request::SetParam {
            track: 0,
            device: 0,
            param: 0,
            value,
        } => Some(*value),
        _ => None,
    });
    assert!(set.is_some_and(|v| v > 50.0), "{:?}", h.state().requests);
}

#[test]
fn picker_opens_on_a_track_with_no_devices() {
    // With no modules, + DEVICE sits at the rack's left: x 292..388, y 564..588,
    // outside the modal. The click that opens the picker must not close it.
    let mut h = rig_with(Vec::new());
    h.run();
    click(&mut h, egui::Pos2::new(340.0, 576.0));
    click(&mut h, egui::Pos2::new(700.0, 568.0 + 16.0)); // first row: AUDelay
    assert!(
        h.state().requests.contains(&Request::AddDevice {
            track: 0,
            choice: 0
        }),
        "{:?}",
        h.state().requests
    );
}

#[test]
fn dragging_a_module_title_moves_the_device() {
    use daw_project::{Action, Device, PluginFormat};
    let mut h = rig();
    for (index, id) in ["a", "b"].into_iter().enumerate() {
        let device = Device {
            uid: index as u64 + 100,
            format: PluginFormat::Builtin,
            bundle: None,
            id: id.into(),
            name: id.into(),
            state: Vec::new(),
        };
        let insert = Action::InsertDevice {
            track: 0,
            index,
            device,
        };
        h.state_mut().history.apply(insert).unwrap();
    }
    h.run();
    // First module's title row: x 280..~480, y 552..584. Drop on the second
    // module (x 536..792).
    let grab = egui::Pos2::new(330.0, 570.0);
    h.hover_at(grab);
    press(&mut h, grab, true);
    h.run();
    for step in 1..=10 {
        h.hover_at(grab + Vec2::new(30.0 * step as f32, 0.0));
        h.run();
    }
    std::fs::create_dir_all("../../target/qa").unwrap();
    h.render()
        .unwrap()
        .save("../../target/qa/rack-drag.png")
        .unwrap();
    press(&mut h, grab + Vec2::new(300.0, 0.0), false);
    h.run();
    let ids: Vec<&str> = h.state().history.project().tracks[0]
        .devices
        .iter()
        .map(|d| d.id.as_str())
        .collect();
    assert_eq!(ids, ["b", "a"]);
}
