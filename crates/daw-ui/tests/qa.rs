//! Styles.md section 6 QA protocol, automated: the workspace rendered at the three
//! required viewports in both themes (PNGs land in `target/qa/` for review), and
//! audits of every painted shape for corner radius and color discipline.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use daw_core::SampleData;
use daw_dsp::WaveformPyramid;
use daw_project::{Action, ClipRef, History, Note, Pattern, PatternClip, Track, PPQ, STEP};
use daw_ui::{
    theme, DeviceView, EngineView, Inputs, Page, ParamView, PluginChoice, Request, SampleDrop,
    SamplerView, Theme, Workspace,
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
        gain: 1.0,
        reverse: false,
        stretch: None,
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
    // A pattern: a four-on-the-floor kick and a short bass line, placed twice.
    h.apply(Action::InsertPattern {
        index: 0,
        pattern: Pattern {
            id: 1,
            name: "Pattern 1".into(),
            length: 4 * PPQ,
        },
    })
    .unwrap();
    let note = |start, length, key, velocity| Note {
        start,
        length,
        key,
        velocity,
    };
    h.apply(Action::SetNotes {
        track: 0,
        pattern: 1,
        notes: (0..4)
            .map(|b| note(b * PPQ, STEP, 60, 70 + 15 * b as u8))
            .collect(),
    })
    .unwrap();
    h.apply(Action::SetNotes {
        track: 1,
        pattern: 1,
        notes: vec![
            note(0, 2 * STEP, 48, 100),
            note(3 * STEP, STEP, 55, 90),
            note(2 * PPQ, 4 * STEP, 51, 110),
        ],
    })
    .unwrap();
    for start in [0, 8 * PPQ] {
        let index = h.project().tracks[2].pattern_clips.len();
        h.apply(Action::InsertPatternClip {
            track: 2,
            index,
            clip: PatternClip {
                pattern: 1,
                start,
                length: 8 * PPQ,
                offset: 0,
            },
        })
        .unwrap();
    }
    // A reverb bus fed by a send from the first track and the pad's output.
    let mut bus = Track::new("Reverb_Bus");
    bus.bus = true;
    h.apply(Action::InsertTrack {
        index: 3,
        track: bus,
    })
    .unwrap();
    let bus_id = h.project().tracks[3].id;
    h.apply(Action::SetRouting {
        track: 0,
        output: None,
        sends: vec![daw_project::Send {
            to: bus_id,
            level: 0.6,
        }],
    })
    .unwrap();
    h.apply(Action::SetRouting {
        track: 1,
        output: Some(bus_id),
        sends: Vec::new(),
    })
    .unwrap();
    // A gain ride on the first track: down, a bent rise, back to unity.
    let point = |tick, value, curve| daw_project::Point { tick, value, curve };
    h.apply(Action::SetAutomation {
        track: 0,
        target: daw_project::Target::Gain,
        points: vec![
            point(0, 1.0, 0.0),
            point(2 * PPQ, 0.3, 0.6),
            point(6 * PPQ, 1.4, 0.0),
            point(9 * PPQ, 1.0, 0.0),
        ],
    })
    .unwrap();
    (h, waveforms, vec![kick, pad])
}

fn harness(
    size: (f32, f32, f32),
    theme: Theme,
    engine: EngineView,
    page: Page,
) -> Harness<'static> {
    let (mut history, waveforms, files) = fixture();
    let mut ws = Workspace::default();
    ws.theme = theme;
    ws.page = page;
    ws.song = page == Page::Playlist;
    ws.automation = Some(daw_project::Target::Gain);
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
                        devices: &[vec![sampler()], devices()].concat(),
                        meters: &[[0.4, 0.5], [0.9, 1.0], [0.0, 0.0], [0.2, 0.25]],
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
        .chain(std::iter::once(PluginChoice {
            name: "Synth".into(),
            vendor: "Canary".into(),
            format: "CANARY",
        }))
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
            sampler: None,
            editor: true,
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
            sampler: None,
            editor: false,
        },
    ]
}

/// A sampler in slice mode with four slices of a loaded file and two layers.
fn sampler() -> DeviceView {
    let params = [
        ("MODE", 0.0, 2.0, 2.0),
        ("SLICER", 0.0, 2.0, 1.0),
        ("SENS", 0.0, 1.0, 0.5),
        ("GRID", 1.0, 64.0, 4.0),
        ("ATTACK", 0.0, 500.0, 1.0),
        ("RELEASE", 1.0, 2000.0, 100.0),
        ("ROOT", 0.0, 127.0, 36.0),
        ("GAIN", -24.0, 12.0, 0.0),
    ];
    DeviceView {
        name: "Sampler".into(),
        params: params
            .iter()
            .enumerate()
            .map(|(i, &(name, min, max, value))| ParamView {
                id: i as u32,
                name: name.into(),
                value,
                min,
                max,
                log: false,
            })
            .collect(),
        editor: false,
        sampler: Some(SamplerView {
            path: Some(PathBuf::from("/samples/808_Thermal_Sub.wav")),
            slices: vec![0.0, 0.25, 0.5, 0.75],
            layers: 2,
        }),
    }
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
        loudness: [-12.4, -13.1, -14.0],
        true_peak: 0.4, // over: the readout turns red
        correlation: 0.62,
        listen: daw_ui::Listen::Mid,
        reference_on: true,
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
        for (size, page) in SIZES.into_iter().flat_map(|s| {
            [
                Page::Playlist,
                Page::Sequencer,
                Page::PianoRoll,
                Page::Mixer,
            ]
            .map(|p| (s, p))
        }) {
            let mut h = harness(size, theme, playing(), page);
            h.run();
            let (mut colors, mut rounded) = (Vec::new(), 0);
            for clipped in &h.output().shapes {
                audit(&clipped.shape, &mut colors, &mut rounded);
            }
            assert_eq!(rounded, 0, "{name} {size:?} {page:?}: rounded corners");
            let stray: Vec<_> = colors
                .iter()
                .filter(|c| c.a() != 0 && !allowed.contains(c))
                .collect();
            assert!(
                stray.is_empty(),
                "{name} {size:?} {page:?}: off-palette colors {stray:?}"
            );

            let image = h.render().expect("wgpu render");
            image
                .save(dir.join(format!(
                    "{}-{name}-{}x{}@{}x.png",
                    format!("{page:?}").to_lowercase(),
                    size.0,
                    size.1,
                    size.2
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
    rig_from(Workspace::default(), rack)
}

/// The rig with `ws` (track 0 selected) and `rack` as its devices.
fn rig_from(mut ws: Workspace, rack: Vec<DeviceView>) -> Harness<'static, Rig> {
    let (history, waveforms, files) = fixture();
    let engine = EngineView::default();
    ws.selected_track = Some(0);
    Harness::builder()
        .with_size(Vec2::new(1440.0, 900.0))
        // 60 FPS, so two clicks a few frames apart make a double-click.
        .with_step_dt(1.0 / 60.0)
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
                            meters: &[],
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
    assert_eq!(tracks.len(), 4, "one Backspace removes one thing");
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
            sidechain: None,
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

#[test]
fn sampler_takes_dropped_files_and_slice_clicks() {
    // The sampler is 32u = 512pt wide from x 280; its waveform spans x 292..524,
    // y 592..672, with the pads below it.
    let mut h = rig_with(vec![sampler()]);
    h.run();
    click(&mut h, egui::Pos2::new(408.0, 630.0));
    let marker = h.state().requests.iter().find_map(|r| match r {
        Request::SliceMarker {
            track: 0,
            device: 0,
            at,
            add: true,
        } => Some(*at),
        _ => None,
    });
    assert!(
        marker.is_some_and(|at| (at - 0.5).abs() < 0.02),
        "{:?}",
        h.state().requests
    );

    // Drag the first browser file onto the pads and let go.
    let file = egui::Pos2::new(100.0, 104.0);
    h.hover_at(file);
    press(&mut h, file, true);
    h.run();
    let pads = egui::Pos2::new(420.0, 780.0);
    for step in 1..=10 {
        h.hover_at(file + (pads - file) * step as f32 / 10.0);
        h.run();
    }
    std::fs::create_dir_all("../../target/qa").unwrap();
    h.render()
        .unwrap()
        .save("../../target/qa/sampler-drop.png")
        .unwrap();
    press(&mut h, pads, false);
    h.run();
    assert!(
        h.state().requests.iter().any(|r| matches!(
            r,
            Request::LoadSample { track: 0, device: 0, path, drop: SampleDrop::Replace }
                if path.ends_with("808_Thermal_Sub.wav")
        )),
        "{:?}",
        h.state().requests
    );
    assert!(
        !h.state()
            .requests
            .iter()
            .any(|r| matches!(r, Request::AddFile { .. })),
        "a drag is not a click"
    );
}

#[test]
fn pressing_a_pad_plays_its_key_until_release() {
    // Pad 01 (slice 1, ROOT 36) is the bottom-left cell of the pad grid.
    let mut h = rig_with(vec![sampler()]);
    h.run();
    let pad = egui::Pos2::new(310.0, 850.0);
    h.hover_at(pad);
    h.run();
    press(&mut h, pad, true);
    h.run();
    let notes = |h: &Harness<'_, Rig>| -> Vec<(u8, bool)> {
        h.state()
            .requests
            .iter()
            .filter_map(|r| match *r {
                Request::Note { track: 0, key, on } => Some((key, on)),
                _ => None,
            })
            .collect()
    };
    assert_eq!(notes(&h), [(36, true)], "{:?}", h.state().requests);
    press(&mut h, pad, false);
    h.run();
    assert_eq!(notes(&h), [(36, true), (36, false)]);
}

#[test]
fn dock_switches_listening_and_takes_a_reference_drop() {
    // Dock: x 1100..1440, y 520..900; body inset 12pt, monitor strip at y 864..888.
    let mut h = rig();
    h.run();
    click(&mut h, egui::Pos2::new(1140.0, 876.0)); // MID, after ST
    assert!(
        h.state()
            .requests
            .contains(&Request::Listen(daw_ui::Listen::Mid)),
        "{:?}",
        h.state().requests
    );

    let file = egui::Pos2::new(100.0, 104.0);
    h.hover_at(file);
    press(&mut h, file, true);
    h.run();
    let dock = egui::Pos2::new(1250.0, 700.0);
    for step in 1..=10 {
        h.hover_at(file + (dock - file) * step as f32 / 10.0);
        h.run();
    }
    press(&mut h, dock, false);
    h.run();
    let reference = h.state().history.project().reference.clone();
    assert!(
        reference.is_some_and(|p| p.ends_with("808_Thermal_Sub.wav")),
        "dropped file is the reference"
    );
    h.run();
    std::fs::create_dir_all("../../target/qa").unwrap();
    h.render()
        .unwrap()
        .save("../../target/qa/dock-reference.png")
        .unwrap();
    click(&mut h, egui::Pos2::new(1130.0, 600.0)); // the LUFS_I readout
    assert!(h.state().requests.contains(&Request::ResetMeters));
}

#[test]
fn tabs_switch_views_and_steps_toggle_notes() {
    let mut h = rig();
    h.run();
    // SEQUENCER tab, then step 2 of row 3 (cells are 32pt every 36pt from x 440).
    click(&mut h, egui::Pos2::new(140.0, 28.0));
    click(&mut h, egui::Pos2::new(492.0, 228.0));
    let notes = h.state().history.project().tracks[2].notes_in(1).to_vec();
    assert_eq!(
        notes,
        [Note {
            start: STEP,
            length: STEP,
            key: 60,
            velocity: 100
        }]
    );
    // Clicking it again clears it.
    click(&mut h, egui::Pos2::new(492.0, 228.0));
    assert!(h.state().history.project().tracks[2].notes_in(1).is_empty());
}

#[test]
fn piano_roll_draws_moves_and_deletes_notes() {
    let mut h = rig();
    h.run();
    click(&mut h, egui::Pos2::new(300.0, 28.0)); // PIANO ROLL
                                                 // Grid from x 344 at 4 ticks per point; key 72 sits at y 292..304.
    let at = egui::Pos2::new(470.0, 298.0);
    click(&mut h, at);
    let drawn = Note {
        start: 480,
        length: STEP,
        key: 72,
        velocity: 100,
    };
    let got = h.state().history.project().tracks[0].notes_in(1).to_vec();
    assert!(got.contains(&drawn), "{got:?}");
    assert!(
        h.state().requests.contains(&Request::Note {
            track: 0,
            key: 72,
            on: true
        }),
        "a new note sounds"
    );
    // Drag it a beat (240pt) right and one key up.
    h.hover_at(at + Vec2::new(4.0, 0.0));
    h.run();
    press(&mut h, at + Vec2::new(4.0, 0.0), true);
    h.run();
    for step in 1..=10 {
        h.hover_at(at + Vec2::new(4.0 + 24.0 * step as f32, -1.2 * step as f32));
        h.run();
    }
    press(&mut h, at + Vec2::new(244.0, -12.0), false);
    h.run();
    let moved = Note {
        start: 480 + PPQ,
        key: 73,
        ..drawn
    };
    let notes = h.state().history.project().tracks[0].notes_in(1).to_vec();
    assert!(
        notes.contains(&moved) && !notes.contains(&drawn),
        "{notes:?}"
    );
    // The moved note is selected: Backspace deletes it and nothing else.
    h.key_press(egui::Key::Backspace);
    h.run();
    let after = h.state().history.project().tracks[0].notes_in(1).to_vec();
    assert_eq!(after.len(), notes.len() - 1);
    assert!(!after.contains(&moved));
    assert_eq!(
        h.state().history.project().tracks.len(),
        4,
        "no track deleted"
    );
}

#[test]
fn double_click_places_the_pattern_and_delete_removes_it() {
    let mut h = rig();
    h.run();
    // Row 2 (y 188..252) is empty at x 900 = sample 200 000 = tick 8 000.
    let at = egui::Pos2::new(900.0, 220.0);
    click(&mut h, at);
    click(&mut h, at);
    let clips = h.state().history.project().tracks[1].pattern_clips.clone();
    assert_eq!(
        clips,
        [PatternClip {
            pattern: 1,
            start: 7_920,
            length: 4 * PPQ,
            offset: 0
        }]
    );
    h.key_press(egui::Key::Backspace);
    h.run();
    let project = h.state().history.project();
    assert!(project.tracks[1].pattern_clips.is_empty());
    assert_eq!(project.tracks[1].clips.len(), 1, "the audio clip stays");
}

#[test]
fn automation_lane_adds_and_removes_points() {
    let mut ws = Workspace::default();
    ws.automation = Some(daw_project::Target::Pan);
    let mut h = rig_from(ws, devices());
    h.run();
    // Track 1's lane is y 124..188 (area 130..182); x 520 is tick 1 920.
    let at = egui::Pos2::new(520.0, 156.0);
    click(&mut h, at);
    let lane = h.state().history.project().tracks[0].automation.clone();
    assert_eq!(lane.len(), 2, "pan joins the fixture's gain lane");
    let pan = &lane
        .iter()
        .find(|l| l.target == daw_project::Target::Pan)
        .unwrap()
        .points;
    assert_eq!(pan.len(), 1);
    assert_eq!(pan[0].tick, 1_920);
    assert!(
        pan[0].value.abs() < 0.05,
        "middle of the lane is center pan"
    );
    // The clip under the lane was not selected or moved by the click.
    assert_eq!(h.state().history.project().tracks[0].clips[0].start, 0);
    // Right-click the point: the lane goes.
    h.hover_at(at);
    h.run();
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        h.run();
    }
    let lanes = &h.state().history.project().tracks[0].automation;
    assert!(lanes.iter().all(|l| l.target != daw_project::Target::Pan));
}

#[test]
fn plus_synth_adds_a_track_with_the_built_in_synth() {
    let mut h = rig();
    h.run();
    // SEQUENCER, then + SYNTH under the four 40pt rows from y 128.
    click(&mut h, egui::Pos2::new(140.0, 28.0));
    click(&mut h, egui::Pos2::new(310.0, 300.0));
    let tracks = &h.state().history.project().tracks;
    assert_eq!(tracks.len(), 5);
    assert_eq!(tracks[4].name, "Synth 5");
    assert!(h.state().requests.contains(&Request::AddDevice {
        track: 4,
        choice: 3
    }));
}

#[test]
fn clip_keys_reverse_and_step_the_gain_of_the_selected_clip() {
    let mut h = rig();
    h.run();
    click(&mut h, egui::Pos2::new(700.0, 230.0)); // the pad clip on row 2
    h.key_press(egui::Key::R);
    h.run();
    h.key_press(egui::Key::ArrowUp);
    h.run();
    let clip = &h.state().history.project().tracks[1].clips[0];
    assert!(clip.reverse);
    assert!(
        (20.0 * clip.gain.log10() - 1.0).abs() < 1e-3,
        "{}",
        clip.gain
    );
    assert!(h.state().history.project().tracks[0]
        .clips
        .iter()
        .all(|c| !c.reverse));
}

#[test]
fn shift_dragging_the_ruler_draws_a_loop_and_l_toggles_it() {
    let mut h = rig();
    h.run();
    // Ruler y 96..124 from x 400; 400 samples per point, a bar is 96 000.
    h.event(egui::Event::ModifiersChanged(egui::Modifiers::SHIFT));
    let from = egui::Pos2::new(400.0 + 240.0, 110.0); // bar 2
    h.hover_at(from);
    h.run();
    press(&mut h, from, true);
    h.run();
    for step in 1..=12 {
        h.hover_at(from + Vec2::new(40.0 * step as f32, 0.0));
        h.run();
    }
    press(&mut h, from + Vec2::new(480.0, 0.0), false); // bar 4
    h.run();
    h.event(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
    h.run();
    let p = h.state().history.project();
    assert_eq!(p.loop_region, Some((4 * PPQ, 12 * PPQ)));
    assert!(p.loop_on);
    h.key_press(egui::Key::L);
    h.run();
    assert!(!h.state().history.project().loop_on);
}
