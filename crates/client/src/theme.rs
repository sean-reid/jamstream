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

/// WCAG AA for text at our sizes, and for anything that carries state on its
/// own. Both floors are enforced by the tests at the end of this file.
pub const AA_TEXT: f64 = 4.5;
pub const AA_STATE: f64 = 3.0;

/// Where every right-anchored sheet sits: in from the window's right edge,
/// and far enough down to clear the top bar. Settings, Invites,
/// Destinations, and Stream mix share this anchor, so they stack in exactly
/// one place and never half-cover each other.
pub const SHEET_OFFSET: egui::Vec2 = egui::vec2(-SHEET_GAP, 56.0);

/// The gap a sheet keeps to the window's edge.
pub const SHEET_GAP: f32 = 10.0;

/// A sheet's own margin, wider than a panel's: a sheet is the surface, not
/// one panel among several on one.
pub const SHEET_PAD: i8 = 14;

/// A sheet never shrinks below this, whatever the window does. The app's
/// minimum window is 800x600 and gives more than three times as much.
const SHEET_MIN_BODY: f32 = 160.0;

/// The frame every right-anchored sheet is drawn in.
pub fn sheet_frame(p: &Palette) -> Frame {
    Frame::new()
        .fill(p.surface1)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::same(SHEET_PAD))
}

/// How tall a sheet's content may be if the sheet is to end at `clear_of`,
/// which is the window's bottom edge or the top of something it must not
/// cover. The frame's own margins come off here because the result goes to
/// `Window::fixed_size`, which sizes the content and not the frame.
///
/// Sheets need this because an egui window sizes its body from its resize
/// default, never from the screen: without a height it grows past the
/// bottom edge, and a scroll area inside one collapses to its minimum.
pub fn sheet_body_height(ctx: &Context, clear_of: f32) -> f32 {
    let top = ctx.content_rect().top() + SHEET_OFFSET.y;
    let pad = 2.0 * f32::from(SHEET_PAD);
    (clear_of - top - SHEET_GAP - pad).max(SHEET_MIN_BODY)
}

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

/// The most a column is ever pushed down from the top of its space.
const LEAD_MAX: f32 = 140.0;

/// A centered max-width column in `room` points of vertical space, pushed
/// toward the upper third by whatever height the content does not need.
/// Content inside stays left-aligned; only the column itself is centered.
///
/// `room` is a parameter because a Ui inside a scroll area cannot answer it:
/// `available_height` there is the content's, not the window's, and it reads
/// zero once anything has been laid out. The closure is handed what is left
/// after the lead, so content that has to fit knows what it has to fit in.
///
/// The lead is capped by the space actually spare, measured from the column
/// drawn last frame: a sixth of a 600 px window spent above the wizard's card
/// is what put the card's own Launch button past the bottom edge (#179).
pub fn focused_column(ui: &mut Ui, max_w: f32, room: f32, add: impl FnOnce(&mut Ui, f32)) {
    let w = ui.available_width().min(max_w);
    let pad = ((ui.available_width() - w) / 2.0).max(0.0);
    let key = ui.id().with("focused-column");
    let drawn: f32 = ui.ctx().data(|d| d.get_temp(key)).unwrap_or(0.0);
    let spare = (room - drawn - SPACE_MD).max(0.0);
    let lead = (room * 0.16).min(LEAD_MAX).min(spare);
    ui.add_space(lead);
    let column = ui.horizontal(|ui| {
        ui.add_space(pad);
        ui.vertical(|ui| {
            ui.set_width(w);
            add(ui, room - lead);
        });
    });
    ui.ctx()
        .data_mut(|d| d.insert_temp(key, column.response.rect.height()));
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

/// "$0.14" from microdollars: cents, and exactly two decimals.
///
/// Rounded to the cent before formatting, not to four decimals. The bar read
/// `$0.0133 so far`, which is four significant digits of a fraction of a cent
/// in the one readout a musician glances at mid-song (#189). A session's cost
/// is worth watching at the cent; below that it is noise that moves.
pub fn microusd(micro: u64) -> String {
    const CENT: u64 = 10_000;
    jamstream_cloud::format_microusd((micro + CENT / 2) / CENT * CENT)
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

/// Steps in which a colour is pushed toward the text colour; 32 lands within
/// a percent or two of the floor, which is closer than a hex value is wide.
const READABLE_STEPS: u8 = 32;

/// `color` stepped toward the primary text colour until it clears the AA
/// text floor on `surface`, and returned untouched when it already does.
///
/// A meter colour is data on a meter and text in a state word, and the two
/// have different floors: `meter_amber` on the light status bar is 3.14:1,
/// which is a legible lamp and an illegible label. Words go through here;
/// the lamp beside them keeps the palette value.
pub fn readable(color: Color32, surface: Color32, p: &Palette) -> Color32 {
    readable_on(color, &[surface], p)
}

/// [`readable`] against more than one surface at once, for a colour the same
/// words get set in on several of them. Stepping twice would blend from an
/// already blended colour and drift off the hue, so the loop tests every
/// surface per step instead.
fn readable_on(color: Color32, surfaces: &[Color32], p: &Palette) -> Color32 {
    let clears = |c: Color32| surfaces.iter().all(|s| contrast_ratio(c, *s) >= AA_TEXT);
    let mut out = color;
    for step in 1..=READABLE_STEPS {
        if clears(out) {
            break;
        }
        out = blend(
            color,
            p.text_primary,
            f32::from(step) / f32::from(READABLE_STEPS),
        );
    }
    out
}

/// The danger colour as *text*, for the verbatim failure reasons the app sets
/// in it: a dropped stream's, a refused avatar's, a recorder's, a provider's.
///
/// Straight off the palette, `danger` measures 4.22:1 on surface1 in dark, and
/// every one of those reasons was set in it. This is the step of it that reads
/// on every surface one can land on, which is the same treatment a state word
/// already gets.
pub fn danger_ink(p: &Palette) -> Color32 {
    readable_on(p.danger, &[p.well, p.surface0, p.surface1, p.surface2], p)
}

/// The fill and the label a destructive button carries: Leave, Revoke invite,
/// End session for everyone.
///
/// White on the danger fill measured 4.00:1 in dark, so the ink is chosen the
/// way [`selected_pair`] chooses one and the fill is stepped until the pair
/// clears the AA text floor. The button is still unmistakably the one red in
/// the palette; the step is a correction, not a repaint.
pub fn danger_pair(p: &Palette) -> (Color32, Color32) {
    let ink = if contrast_ratio(p.surface1, p.danger) >= contrast_ratio(Color32::WHITE, p.danger) {
        p.surface1
    } else {
        Color32::WHITE
    };
    (readable(p.danger, ink, p), ink)
}

/// A destructive button: the one red in the palette, with a label that reads
/// on it. Every Leave, Revoke, and End session in the app comes through here,
/// so there is one place the pair is chosen.
pub fn danger_button(p: &Palette, text: &str) -> egui::Button<'static> {
    let (fill, ink) = danger_pair(p);
    egui::Button::new(RichText::new(text.to_owned()).color(ink)).fill(fill)
}

/// Verbatim failure text: wrapped, full width, in the readable step of the
/// danger colour. Summarising an error hides the part someone can act on, so
/// every one of these is the reason its source gave.
pub fn reason(ui: &mut Ui, text: impl Into<String>) -> egui::Response {
    let color = danger_ink(palette_of(ui));
    ui.add(egui::Label::new(RichText::new(text.into()).color(color)).wrap())
}

/// The fill and the label a selected control carries: the accent, opaque,
/// stepped until whichever end of the palette reads on it clears the AA text
/// floor.
///
/// egui's own selected treatment is `selection.bg_fill`, the accent at alpha
/// 70, with accent text on top of it: 2.79:1 in the light palette, which is
/// fainter than a disabled control. That constant stays what it is for text
/// selection, where a wash is the right thing.
pub fn selected_pair(p: &Palette) -> (Color32, Color32) {
    let ink = if contrast_ratio(p.surface1, p.accent) >= contrast_ratio(p.text_primary, p.accent) {
        p.surface1
    } else {
        p.text_primary
    };
    (readable(p.accent, ink, p), ink)
}

/// A button that shows a selected state as an accent fill rather than a
/// wash. Off is the ordinary button, so only the lit form differs.
pub fn selectable(p: &Palette, text: &str, on: bool) -> egui::Button<'static> {
    if !on {
        return egui::Button::new(text.to_owned()).selected(false);
    }
    let (fill, ink) = selected_pair(p);
    egui::Button::new(RichText::new(text.to_owned()).color(ink))
        .selected(true)
        .fill(fill)
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

    /// Every colour a state word is set in, on every surface it can land on.
    /// The lamps in the status bar's cluster measured 3.14:1 (UPLOADING),
    /// 3.77:1 (ON AIR) and 3.96:1 (REC) in the light palette, and nothing
    /// failed, because the only colours under test were the two text steps.
    #[test]
    fn a_state_word_reads_on_every_surface_it_can_land_on() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            for (cname, color) in [
                ("accent", p.accent),
                ("meter_green", p.meter_green),
                ("meter_amber", p.meter_amber),
                ("meter_red", p.meter_red),
                ("danger", p.danger),
            ] {
                for (sname, surface) in [
                    ("well", p.well),
                    ("surface0", p.surface0),
                    ("surface1", p.surface1),
                    ("surface2", p.surface2),
                ] {
                    let word = readable(color, surface, p);
                    let ratio = contrast_ratio(word, surface);
                    assert!(
                        ratio >= AA_TEXT,
                        "{name} {cname} as a word on {sname} is {ratio:.2}, below AA {AA_TEXT}"
                    );
                    // Still recognisably the accent or the meter colour: the
                    // step is a correction, not a repaint.
                    assert!(
                        contrast_ratio(word, color) < 2.0,
                        "{name} {cname} on {sname} was pushed off its own hue"
                    );
                }
            }
        }
    }

    /// A selected control has to read as on, both in the label on it and
    /// against the panel it sits on. The wash it replaces was 2.79:1 in
    /// light and 4.31:1 in dark, on five surfaces at once.
    #[test]
    fn a_selected_control_reads_as_on_in_both_palettes() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            let (fill, ink) = selected_pair(p);
            let label = contrast_ratio(ink, fill);
            assert!(
                label >= AA_TEXT,
                "{name} selected label is {label:.2}, below AA {AA_TEXT}"
            );
            for (sname, surface) in [
                ("surface0", p.surface0),
                ("surface1", p.surface1),
                ("surface2", p.surface2),
            ] {
                let ratio = contrast_ratio(fill, surface);
                assert!(
                    ratio >= AA_STATE,
                    "{name} selected fill on {sname} is {ratio:.2}, below {AA_STATE} for a \
                     control that carries state"
                );
            }
            // The state is the accent's, not a fourth colour: an opaque wash
            // of something else would pass the floors and say nothing.
            assert!(
                contrast_ratio(fill, p.accent) < 2.0,
                "{name} selected fill is no longer the accent"
            );
        }
    }

    /// The colour every verbatim failure reason in the app is set in, on every
    /// surface one lands on. `danger` itself measured 4.22:1 on surface1 in
    /// dark, and the test above could not see it because it only ever asked
    /// about the two text steps (#192).
    #[test]
    fn a_failure_reason_reads_on_every_surface_it_can_land_on() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            let ink = danger_ink(p);
            for (sname, surface) in [
                ("well", p.well),
                ("surface0", p.surface0),
                ("surface1", p.surface1),
                ("surface2", p.surface2),
            ] {
                let ratio = contrast_ratio(ink, surface);
                assert!(
                    ratio >= AA_TEXT,
                    "{name} a failure reason on {sname} is {ratio:.2}, below AA {AA_TEXT}"
                );
            }
            assert!(
                contrast_ratio(ink, p.danger) < 2.0,
                "{name} failure reasons are no longer the danger colour"
            );
        }
    }

    /// Leave, Revoke invite, and End session for everyone. White on the danger
    /// fill measured 4.00:1 in dark, on the three buttons in the product that
    /// take something away from other people.
    #[test]
    fn a_destructive_button_reads_in_both_palettes() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            let (fill, ink) = danger_pair(p);
            let label = contrast_ratio(ink, fill);
            assert!(
                label >= AA_TEXT,
                "{name} destructive label is {label:.2}, below AA {AA_TEXT}"
            );
            for (sname, surface) in [
                ("surface0", p.surface0),
                ("surface1", p.surface1),
                ("surface2", p.surface2),
            ] {
                let ratio = contrast_ratio(fill, surface);
                assert!(
                    ratio >= AA_STATE,
                    "{name} destructive fill on {sname} is {ratio:.2}, below {AA_STATE}"
                );
            }
            // Still the one red, not a fourth colour that would pass and say
            // nothing.
            assert!(
                contrast_ratio(fill, p.danger) < 2.0,
                "{name} destructive fill is no longer the danger colour"
            );
        }
    }

    /// The cost ticker's own doc says two decimals. It rendered `$0.0133`.
    #[test]
    fn the_cost_readout_is_cents_and_two_decimals() {
        assert_eq!(microusd(0), "$0.00");
        assert_eq!(microusd(13_300), "$0.01");
        assert_eq!(microusd(4_999), "$0.00");
        assert_eq!(microusd(5_000), "$0.01");
        assert_eq!(microusd(140_000), "$0.14");
        assert_eq!(microusd(1_234_567), "$1.23");
        for micro in [0u64, 1, 999, 13_300, 26_790, 1_234_567, 9_999_999] {
            let text = microusd(micro);
            let decimals = text.split_once('.').expect("a decimal point").1.len();
            assert_eq!(decimals, 2, "{micro} rendered as {text}");
        }
    }

    /// `crates/broadcast/src/palette.rs` spells the dark palette a second
    /// time, in a crate with no dependency path back to this one, and the app
    /// is the only crate that sees both. A member is supposed to look the same
    /// on their strip and on their stream card, and the docs say so, so the
    /// hexes are held together here the way `avatar::disc_color` is.
    ///
    /// The pairs are listed by hand because consts cannot be enumerated. What
    /// keeps the list complete is the count: it is read out of the broadcast
    /// source itself, so an entry added there and not paired here fails rather
    /// than going unchecked.
    #[test]
    fn the_stage_palette_is_this_palette() {
        use jamstream_broadcast::palette as stage;

        let pairs: [(&str, Color32, stage::Rgb); 10] = [
            ("well", DARK.well, stage::WELL),
            ("surface0", DARK.surface0, stage::SURFACE0),
            ("surface1", DARK.surface1, stage::SURFACE1),
            ("text_primary", DARK.text_primary, stage::TEXT_PRIMARY),
            ("text_muted", DARK.text_muted, stage::TEXT_MUTED),
            ("accent", DARK.accent, stage::ACCENT),
            ("meter_green", DARK.meter_green, stage::METER_GREEN),
            ("meter_amber", DARK.meter_amber, stage::METER_AMBER),
            ("meter_red", DARK.meter_red, stage::METER_RED),
            ("border", DARK.border, stage::BORDER),
        ];
        for (name, ours, theirs) in pairs {
            assert_eq!(
                [ours.r(), ours.g(), ours.b()],
                theirs,
                "{name} is a different colour on the stream card than in the app"
            );
        }

        let declared = include_str!("../../broadcast/src/palette.rs")
            .lines()
            .filter(|line| line.starts_with("pub const ") && line.contains(": Rgb = ["))
            .count();
        assert_eq!(
            declared,
            pairs.len(),
            "the stage palette declares {declared} colours and this test pairs {}",
            pairs.len()
        );
    }

    #[test]
    fn contrast_ratio_sanity() {
        let black = Color32::from_rgb(0, 0, 0);
        let white = Color32::from_rgb(255, 255, 255);
        assert!((contrast_ratio(black, white) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(white, white) - 1.0).abs() < 0.01);
    }
}
