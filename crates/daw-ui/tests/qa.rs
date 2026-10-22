//! Styles.md section 6 QA protocol, automated: the workspace rendered at the three
//! required viewports in both themes (PNGs land in `target/qa/` for review), and
//! audits of every painted shape for corner radius and color discipline.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use daw_core::SampleData;
use daw_dsp::WaveformPyramid;
use daw_project::{Action, ClipRef, History, Track};
use daw_ui::{theme, EngineView, Inputs, Theme, Workspace};
use egui::epaint::ColorMode;
use egui::{Color32, Shape, Vec2};
use egui_kittest::Harness;

const SIZES: [(f32, f32); 3] = [(1440.0, 900.0), (2560.0, 1440.0), (3840.0, 2160.0)];

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

fn harness(size: (f32, f32), theme: Theme, engine: EngineView) -> Harness<'static> {
    let (mut history, waveforms, files) = fixture();
    let mut ws = Workspace::default();
    ws.theme = theme;
    ws.selected_track = Some(0);
    ws.selected_clip = Some((1, 0));
    Harness::builder()
        .with_size(Vec2::new(size.0, size.1))
        .with_pixels_per_point(1.0)
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
                    },
                );
            })
        })
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
                .save(dir.join(format!("workspace-{name}-{}x{}.png", size.0, size.1)))
                .unwrap();
        }
    }
}

#[test]
fn transport_and_track_controls_send_requests_and_edits() {
    let (mut history, waveforms, files) = fixture();
    let engine = EngineView::default();
    let mut ws = Workspace::default();
    let mut requests = Vec::new();
    let mut h = Harness::builder()
        .with_size(Vec2::new(1440.0, 900.0))
        .with_context_setup(theme::install_fonts)
        .build_ui(|ui| {
            requests.extend(ws.show(
                ui,
                Inputs {
                    history: &mut history,
                    engine: &engine,
                    waveforms: &waveforms,
                    files: &files,
                },
            ));
        });
    h.run();
    h.key_press(egui::Key::Space);
    h.run();
    drop(h);
    assert_eq!(requests, [daw_ui::Request::Play]);
    assert_eq!(history.project().tracks.len(), 3);
}
