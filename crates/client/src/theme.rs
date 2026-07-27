//! Palette, type, spacing, and the one panel primitive. Dark is the
//! default; light is maintained, not an afterthought. Every text/surface
//! pair must clear WCAG AA (4.5:1); the unit test below enforces it.

use std::sync::Arc;

use egui::{
    Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame, Id,
    Margin, RichText, Stroke, Style, TextStyle, Ui, Visuals,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

pub struct Palette {
    /// Engraved wells: text inputs and meter tracks, one step below the
    /// window so they read inset without an outline.
    pub well: Color32,
    /// Window background, the darkest step.
    pub surface0: Color32,
    /// Panel fill, the one panel primitive.
    pub surface1: Color32,
    /// Raised controls: buttons, handles, inputs.
    pub surface2: Color32,
    pub text_primary: Color32,
    pub text_muted: Color32,
    /// The single warm accent: active, live, on air. Never decoration.
    pub accent: Color32,
    pub meter_green: Color32,
    pub meter_amber: Color32,
    pub meter_red: Color32,
    /// Destructive actions and genuine problems only.
    pub danger: Color32,
    /// Hairline borders; depth comes from the surface steps, not borders.
    pub border: Color32,
}

// The two palettes below are the single source of truth for JamStream's
// colors, including the ones outside this crate: scripts/render-palette.sh
// parses them out of this file and generates the documentation site's CSS
// custom properties (site/theme/css/palette.css), the docs theme-color
// meta tag, and the app icon's fill values. CI (the palette job in
// .github/workflows/docs-check.yml) runs that script with --check and
// fails when a change here has not been propagated.
//
// The parse is a small awk over these two blocks, so keep the format
// exactly what rustfmt produces: one field per line, spelled
// `name: Color32::from_rgb(0x.., 0x.., 0x..),`, inside a
// `pub const NAME: Palette = Palette {` ... `};` block. Computed or
// aliased entries would be invisible to the generator.
pub const DARK: Palette = Palette {
    well: Color32::from_rgb(0x0b, 0x0c, 0x0d),
    surface0: Color32::from_rgb(0x12, 0x13, 0x14),
    surface1: Color32::from_rgb(0x1b, 0x1d, 0x1f),
    surface2: Color32::from_rgb(0x27, 0x2a, 0x2d),
    text_primary: Color32::from_rgb(0xe8, 0xea, 0xec),
    text_muted: Color32::from_rgb(0x9e, 0xa4, 0xaa),
    accent: Color32::from_rgb(0xf5, 0x92, 0x2b),
    meter_green: Color32::from_rgb(0x40, 0xc0, 0x57),
    meter_amber: Color32::from_rgb(0xfa, 0xb0, 0x05),
    meter_red: Color32::from_rgb(0xfa, 0x52, 0x52),
    danger: Color32::from_rgb(0xe0, 0x4a, 0x4a),
    border: Color32::from_rgb(0x34, 0x37, 0x3b),
};

pub const LIGHT: Palette = Palette {
    well: Color32::from_rgb(0xe0, 0xe2, 0xe4),
    surface0: Color32::from_rgb(0xef, 0xf0, 0xf1),
    surface1: Color32::from_rgb(0xf8, 0xf9, 0xfa),
    // Light raised controls step darker, the inverse of dark; a white chip
    // on a white panel would need a border, and borders are for panels.
    surface2: Color32::from_rgb(0xe7, 0xe9, 0xec),
    text_primary: Color32::from_rgb(0x1b, 0x1d, 0x1f),
    text_muted: Color32::from_rgb(0x53, 0x58, 0x5e),
    accent: Color32::from_rgb(0xd9, 0x48, 0x0f),
    meter_green: Color32::from_rgb(0x2f, 0x9e, 0x44),
    meter_amber: Color32::from_rgb(0xe8, 0x59, 0x0c),
    meter_red: Color32::from_rgb(0xe0, 0x31, 0x31),
    danger: Color32::from_rgb(0xc9, 0x2a, 0x2a),
    border: Color32::from_rgb(0xd3, 0xd6, 0xd9),
};

// Spacing scale, px. Densities like a hardware panel: tight but aligned.
// Every gap in the app comes off this scale, never egui's defaults.
pub const SPACE_XS: f32 = 2.0;
pub const SPACE_SM: f32 = 4.0;
pub const SPACE_MD: f32 = 8.0;
pub const SPACE_LG: f32 = 12.0;
pub const SPACE_XL: f32 = 20.0;

/// Uniform corner radius; tight radii read as a tool.
pub const RADIUS: u8 = 3;

/// The emphasis family: Public Sans semibold. Fonts set with
/// `Context::set_fonts` only activate on the next pass, so this falls back
/// to the proportional family for the one frame where the named family is
/// not bound yet (kittest runs the ui closure inside the first pass).
pub fn semibold(ui: &Ui) -> FontFamily {
    let family = FontFamily::Name("semibold".into());
    if ui.fonts(|f| f.definitions().families.contains_key(&family)) {
        family
    } else {
        FontFamily::Proportional
    }
}

pub fn palette(theme: Theme) -> &'static Palette {
    match theme {
        Theme::Dark => &DARK,
        Theme::Light => &LIGHT,
    }
}

/// Palette matching the visuals currently applied to this ui.
pub fn palette_of(ui: &Ui) -> &'static Palette {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

/// Bundled fonts only; egui's defaults are explicitly not used.
fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::empty();
    fonts.font_data.insert(
        "public-sans".to_owned(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/PublicSans-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "public-sans-semibold".to_owned(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/PublicSans-SemiBold.ttf"
        ))),
    );
    fonts.font_data.insert(
        "plex-mono".to_owned(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/IBMPlexMono-Regular.ttf"
        ))),
    );
    fonts
        .families
        .insert(FontFamily::Proportional, vec!["public-sans".to_owned()]);
    fonts
        .families
        .insert(FontFamily::Monospace, vec!["plex-mono".to_owned()]);
    fonts.families.insert(
        FontFamily::Name("semibold".into()),
        vec!["public-sans-semibold".to_owned()],
    );
    fonts
}

/// Applies fonts (once per context) and the full style for `theme`.
/// Cheap to call every frame; it re-applies style only on theme change.
pub fn apply(ctx: &Context, theme: Theme) {
    let fonts_key = Id::new("jamstream-fonts");
    if ctx.data(|d| d.get_temp::<bool>(fonts_key)).is_none() {
        ctx.set_fonts(fonts());
        ctx.data_mut(|d| d.insert_temp(fonts_key, true));
    }
    let theme_key = Id::new("jamstream-theme");
    let applied: Option<u8> = ctx.data(|d| d.get_temp(theme_key));
    let tag = matches!(theme, Theme::Dark) as u8;
    if applied != Some(tag) {
        let egui_theme = match theme {
            Theme::Dark => egui::Theme::Dark,
            Theme::Light => egui::Theme::Light,
        };
        ctx.set_theme(egui_theme);
        ctx.set_style_of(egui_theme, style(theme));
        ctx.data_mut(|d| d.insert_temp(theme_key, tag));
    }
}

fn style(theme: Theme) -> Style {
    let p = palette(theme);
    let mut style = Style {
        // Headings stay in the proportional family: the named semibold
        // family is applied per widget via `semibold`, which can fall back
        // safely during the first pass.
        text_styles: [
            (
                TextStyle::Heading,
                FontId::new(16.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(13.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.5, FontFamily::Monospace),
            ),
        ]
        .into(),
        ..Style::default()
    };

    // Our own spacing scale; egui's defaults are a tell.
    style.spacing.item_spacing = egui::vec2(SPACE_MD, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 4.0);
    style.spacing.menu_margin = Margin::same(SPACE_MD as i8);
    style.spacing.window_margin = Margin::same(14);
    style.spacing.indent = 14.0;
    style.spacing.interact_size = egui::vec2(40.0, 20.0);
    style.spacing.icon_width = 14.0;
    style.spacing.icon_width_inner = 8.0;
    style.spacing.tooltip_width = 360.0;
    // Scrollbars: thin, solid, surface-stepped; nothing floats or fades.
    style.spacing.scroll = egui::style::ScrollStyle {
        bar_width: 6.0,
        handle_min_length: 24.0,
        bar_inner_margin: 2.0,
        bar_outer_margin: 0.0,
        ..egui::style::ScrollStyle::solid()
    };

    let mut v = match theme {
        Theme::Dark => Visuals::dark(),
        Theme::Light => Visuals::light(),
    };
    v.panel_fill = p.surface0;
    v.window_fill = p.surface1;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(RADIUS);
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.popup_shadow = egui::epaint::Shadow::NONE;
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v.extreme_bg_color = p.well;
    v.faint_bg_color = p.surface1;
    v.hyperlink_color = p.text_primary;
    v.warn_fg_color = p.meter_amber;
    v.error_fg_color = p.danger;
    v.selection.bg_fill =
        Color32::from_rgba_unmultiplied(p.accent.r(), p.accent.g(), p.accent.b(), 70);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.text_cursor.stroke.color = p.text_primary;

    let radius = CornerRadius::same(RADIUS);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = radius;
        // Nothing scales on hover.
        w.expansion = 0.0;
        w.fg_stroke = Stroke::new(1.0, p.text_primary);
        // Controls are flat surface steps, not outlined boxes; the border
        // is reserved for panels and text wells. Hover and press step the
        // surface, focus draws the accent.
        w.bg_stroke = Stroke::NONE;
    }
    // Separators and other passive strokes stay a hairline.
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.bg_fill = p.surface1;
    v.widgets.noninteractive.weak_bg_fill = p.surface1;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text_primary);
    v.widgets.inactive.bg_fill = p.surface2;
    v.widgets.inactive.weak_bg_fill = p.surface2;
    v.widgets.hovered.bg_fill = lift(p.surface2, theme, 10);
    v.widgets.hovered.weak_bg_fill = lift(p.surface2, theme, 10);
    v.widgets.active.bg_fill = lift(p.surface2, theme, 18);
    v.widgets.active.weak_bg_fill = lift(p.surface2, theme, 18);
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.open.bg_fill = p.surface2;
    v.widgets.open.weak_bg_fill = p.surface2;

    style.visuals = v;
    style
}

/// The wordmark lockup: "jamstream" in the semibold, slightly tightened,
/// with the single amber tuning dot. The one brand mark in the product.
pub fn wordmark(ui: &mut Ui, size: f32) {
    let p = palette_of(ui);
    let mut job = egui::text::LayoutJob::default();
    job.append(
        "jamstream",
        0.0,
        egui::TextFormat {
            font_id: FontId::new(size, semibold(ui)),
            color: p.text_primary,
            extra_letter_spacing: -size * 0.015,
            ..Default::default()
        },
    );
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let dot_r = (size * 0.10).max(2.0);
    let dot_gap = size * 0.28;
    let width = galley.size().x + dot_gap + dot_r * 2.0;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(width, galley.size().y), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    ui.painter()
        .galley(rect.min, galley.clone(), p.text_primary);
    // The dot sits at x-height center, like a channel lamp next to a label.
    let center = egui::pos2(
        rect.min.x + galley.size().x + dot_gap + dot_r,
        rect.min.y + size * 0.62,
    );
    ui.painter().circle_filled(center, dot_r, p.accent);
}

/// Nudges a surface toward the text color for hover/pressed states.
fn lift(c: Color32, theme: Theme, amount: i16) -> Color32 {
    let d = match theme {
        Theme::Dark => amount,
        Theme::Light => -amount,
    };
    let ch = |x: u8| (x as i16 + d).clamp(0, 255) as u8;
    Color32::from_rgb(ch(c.r()), ch(c.g()), ch(c.b()))
}

/// The one panel primitive: surface1, hairline border, uniform radius.
pub fn panel(ui: &Ui) -> Frame {
    let p = palette_of(ui);
    Frame::new()
        .fill(p.surface1)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::same(10))
}

/// Linear blend of `b` into `a`; `t` in 0..1. Used for tinted surfaces.
pub fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
}

/// A centered max-width column anchored in the upper third of the screen.
/// Content inside stays left-aligned; only the column itself is centered.
pub fn focused_column(ui: &mut Ui, max_w: f32, add: impl FnOnce(&mut Ui)) {
    let w = ui.available_width().min(max_w);
    let pad = ((ui.available_width() - w) / 2.0).max(0.0);
    ui.add_space((ui.available_height() * 0.16).min(140.0));
    ui.horizontal(|ui| {
        ui.add_space(pad);
        ui.vertical(|ui| {
            ui.set_width(w);
            add(ui);
        });
    });
}

/// Secondary text at the muted step.
pub fn muted(ui: &Ui, text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(palette_of(ui).text_muted)
}

/// Panel title: the semibold at body size, one treatment everywhere.
pub fn title(ui: &Ui, text: impl Into<String>) -> RichText {
    RichText::new(text.into())
        .font(FontId::new(13.5, semibold(ui)))
        .color(palette_of(ui).text_primary)
}

/// Monospace with the primary text color; every changing number goes
/// through here so meters and tickers do not wobble.
pub fn mono(ui: &Ui, text: impl Into<String>) -> RichText {
    RichText::new(text.into())
        .monospace()
        .color(palette_of(ui).text_primary)
}

/// Monospace at the muted step.
pub fn mono_muted(ui: &Ui, text: impl Into<String>) -> RichText {
    RichText::new(text.into())
        .monospace()
        .color(palette_of(ui).text_muted)
}

/// DragValue in monospace, so editable numerals match displayed ones.
pub fn mono_drag(ui: &mut Ui, drag: egui::DragValue<'_>) -> egui::Response {
    ui.scope(|ui| {
        ui.style_mut().override_font_id = Some(FontId::new(12.5, FontFamily::Monospace));
        ui.add(drag)
    })
    .inner
}

/// "$0.14" from microdollars, always two decimals, trimmed beyond that.
/// Rounded to four decimals first so tickers never show raw microdollars.
pub fn microusd(micro: u64) -> String {
    jamstream_cloud::format_microusd((micro + 50) / 100 * 100)
}

/// WCAG relative luminance of an sRGB color.
fn relative_luminance(c: Color32) -> f64 {
    let lin = |v: u8| {
        let s = v as f64 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// WCAG contrast ratio between two colors, >= 1.0.
pub fn contrast_ratio(a: Color32, b: Color32) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_on_every_surface_passes_aa() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            for (sname, surface) in [
                ("well", p.well),
                ("surface0", p.surface0),
                ("surface1", p.surface1),
                ("surface2", p.surface2),
            ] {
                for (tname, text) in [
                    ("text_primary", p.text_primary),
                    ("text_muted", p.text_muted),
                ] {
                    let ratio = contrast_ratio(text, surface);
                    assert!(
                        ratio >= 4.5,
                        "{name} {tname} on {sname} is {ratio:.2}, below AA 4.5"
                    );
                }
            }
        }
    }

    #[test]
    fn contrast_ratio_sanity() {
        let black = Color32::from_rgb(0, 0, 0);
        let white = Color32::from_rgb(255, 255, 255);
        assert!((contrast_ratio(black, white) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(white, white) - 1.0).abs() < 0.01);
    }
}
