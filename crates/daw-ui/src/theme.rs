//! Styles.md tokens ("Tactile Brutalist Telemetry"): colors, the fluid `u` scale,
//! fonts, and pixel-snapped hairlines.

use std::sync::Arc;

use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, Pos2, Rect, Stroke};

const INTER: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.otf");
const INTER_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Inter-SemiBold.otf");
const MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");
const MONO_BOLD: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf");

/// Section 2.3 color tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub bg_void: Color32,
    pub bg_panel: Color32,
    pub bg_rack: Color32,
    pub sel: Color32,
    pub line: Color32,
    pub line_2: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub ink: Color32,
    pub ink_text: Color32,
    pub accent_amber: Color32,
    pub accent_cyan: Color32,
    pub good: Color32,
    pub bad: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

impl Theme {
    pub const DARK: Theme = Theme {
        bg_void: hex(0x0B0B0D),
        bg_panel: hex(0x161618),
        bg_rack: hex(0x232326),
        sel: hex(0x1D1D21),
        line: hex(0x2E2E33),
        line_2: hex(0x3E3E45),
        text: hex(0xF4F4F6),
        muted: hex(0x9A9AA2),
        faint: hex(0x5E5E66),
        ink: hex(0xF4F4F6),
        ink_text: hex(0x0B0B0D),
        accent_amber: hex(0xE5853C),
        accent_cyan: hex(0x4CC3B5),
        good: hex(0x56D38A),
        bad: hex(0xF04E3E),
    };

    pub const LIGHT: Theme = Theme {
        bg_void: hex(0xF4F4F5),
        bg_panel: hex(0xFFFFFF),
        bg_rack: hex(0xFAFAFA),
        sel: hex(0xF0F0F2),
        line: hex(0xEBEBEE),
        line_2: hex(0xD9D9DE),
        text: hex(0x09090B),
        muted: hex(0x6E6E78),
        faint: hex(0xA1A1AA),
        ink: hex(0x18181B),
        ink_text: hex(0xFFFFFF),
        accent_amber: hex(0xD96B27),
        accent_cyan: hex(0x0D9488),
        good: hex(0x15803D),
        bad: hex(0xC42A1C),
    };

    /// Every token, for the QA color-discipline check.
    pub fn tokens(&self) -> [Color32; 15] {
        [
            self.bg_void,
            self.bg_panel,
            self.bg_rack,
            self.sel,
            self.line,
            self.line_2,
            self.text,
            self.muted,
            self.faint,
            self.ink,
            self.ink_text,
            self.accent_amber,
            self.accent_cyan,
            self.good,
            self.bad,
        ]
    }
}

/// The root length unit (`1u`) for a viewport `width` points wide: 16pt up to
/// 1440pt, then growing with the viewport to at most 26pt at 4K (section 2.1).
pub fn unit(width: f32) -> f32 {
    if width <= 1440.0 {
        16.0
    } else {
        (0.45 * width / 100.0 + 9.5).clamp(16.0, 26.0)
    }
}

/// Inter for labels; JetBrains Mono for every number. Mono digits are all one
/// width, so readouts never jitter (the `tnum` rule).
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::empty();
    for (name, bytes) in [
        ("inter", INTER),
        ("inter-semibold", INTER_SEMIBOLD),
        ("mono", MONO),
        ("mono-bold", MONO_BOLD),
    ] {
        fonts
            .font_data
            .insert(name.into(), Arc::new(FontData::from_static(bytes)));
    }
    let family = |fonts: &mut FontDefinitions, family: FontFamily, first: &str| {
        // Mono covers glyphs Inter lacks and the other way round.
        let fallback = if first.starts_with("mono") {
            "inter"
        } else {
            "mono"
        };
        fonts
            .families
            .insert(family, vec![first.into(), fallback.into()]);
    };
    family(&mut fonts, FontFamily::Proportional, "inter");
    family(&mut fonts, FontFamily::Monospace, "mono");
    family(
        &mut fonts,
        FontFamily::Name("sans-bold".into()),
        "inter-semibold",
    );
    family(
        &mut fonts,
        FontFamily::Name("mono-bold".into()),
        "mono-bold",
    );
    ctx.set_fonts(fonts);
}

/// Points egui's own fills and strokes at the theme tokens with square corners,
/// for anything the framework draws itself (panel backgrounds, focus outlines).
pub fn apply(ctx: &egui::Context, t: &Theme) {
    let mut v = if t.bg_void == Theme::DARK.bg_void {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.panel_fill = t.bg_void;
    v.window_fill = t.bg_panel;
    v.extreme_bg_color = t.bg_void;
    v.faint_bg_color = t.bg_panel;
    v.code_bg_color = t.bg_void;
    v.window_stroke = Stroke::new(1.0, t.line);
    v.selection.bg_fill = t.accent_amber;
    v.selection.stroke = Stroke::new(1.0, t.ink_text);
    v.hyperlink_color = t.accent_cyan;
    v.warn_fg_color = t.accent_amber;
    v.error_fg_color = t.bad;
    v.window_corner_radius = egui::CornerRadius::same(8); // modal overlays only
    v.menu_corner_radius = egui::CornerRadius::ZERO;
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = egui::CornerRadius::ZERO;
        w.bg_fill = t.bg_panel;
        w.weak_bg_fill = t.bg_panel;
        w.bg_stroke = Stroke::new(1.0, t.line_2);
        w.fg_stroke = Stroke::new(1.0, t.text);
    }
    ctx.set_visuals(v);
}

/// Sizes and fonts at the current `u`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub u: f32,
    /// One physical pixel, in points.
    pub px: f32,
    ppp: f32,
}

impl Metrics {
    pub fn new(width: f32, pixels_per_point: f32) -> Self {
        Self {
            u: unit(width),
            px: 1.0 / pixels_per_point,
            ppp: pixels_per_point,
        }
    }

    pub fn sans(&self, size_u: f32) -> FontId {
        FontId::new(size_u * self.u, FontFamily::Proportional)
    }

    pub fn sans_bold(&self, size_u: f32) -> FontId {
        FontId::new(size_u * self.u, FontFamily::Name("sans-bold".into()))
    }

    pub fn mono(&self, size_u: f32) -> FontId {
        FontId::new(size_u * self.u, FontFamily::Monospace)
    }

    pub fn mono_bold(&self, size_u: f32) -> FontId {
        FontId::new(size_u * self.u, FontFamily::Name("mono-bold".into()))
    }

    /// Centre of the physical pixel at or below `v`, so a 1px line stays crisp.
    pub fn snap(&self, v: f32) -> f32 {
        ((v * self.ppp).floor() + 0.5) / self.ppp
    }

    /// A one-physical-pixel stroke.
    pub fn hair(&self, color: Color32) -> Stroke {
        Stroke::new(self.px, color)
    }

    /// Pixel-aligned rectangle (edges on pixel boundaries).
    pub fn snap_rect(&self, r: Rect) -> Rect {
        let f = |v: f32| (v * self.ppp).round() / self.ppp;
        Rect::from_min_max(
            Pos2::new(f(r.min.x), f(r.min.y)),
            Pos2::new(f(r.max.x), f(r.max.y)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_is_16_up_to_1440_and_capped_at_26() {
        assert_eq!(unit(1280.0), 16.0);
        assert_eq!(unit(1440.0), 16.0);
        assert!(unit(2560.0) > 20.0 && unit(2560.0) < 21.5);
        assert_eq!(unit(3840.0), 26.0);
        assert_eq!(unit(8000.0), 26.0);
    }
}
