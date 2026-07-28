//! The lamp: one small circle, lit with the accent (plus a 1 px brighter
//! ring) while a state is on, a dark raised surface while it is off.
//! [`lamp`] is the bare indicator, [`lamp_toggle`] is the clickable sibling
//! for a state that is switched rather than only shown, and [`state_lamp`]
//! is the loud one the status bar's centre cluster is built from.

use egui::{Color32, FontId, Response, Sense, Stroke, Ui, WidgetInfo, vec2};

use crate::theme;

/// Lamp diameter and the space one occupies inline.
const LAMP_R: f32 = 4.0;
const LAMP_W: f32 = 11.0;

/// The centre cluster's lamp: bigger circle, bigger type, and the label in
/// the lamp's own colour rather than the muted step, because these are the
/// two states that have to read from across the room.
const STATE_LAMP_R: f32 = 5.0;
/// Where the label starts, measured from the lamp's left edge.
const STATE_LABEL_X: f32 = 17.0;
const STATE_LAMP_SIZE: f32 = 13.0;

/// Paints one lamp at `center`. Lit is the accent with a brighter rim, which
/// is the only place in the app a circle carries the accent.
fn paint(ui: &Ui, center: egui::Pos2, lit: bool, hovered: bool) {
    let p = theme::palette_of(ui);
    let painter = ui.painter();
    if lit {
        painter.circle(
            center,
            LAMP_R,
            p.accent,
            Stroke::new(1.0, theme::blend(p.accent, p.text_primary, 0.45)),
        );
    } else {
        // Hover steps the lamp surface the way buttons step theirs.
        let fill = if hovered {
            theme::blend(p.surface2, p.text_primary, 0.08)
        } else {
            p.surface2
        };
        painter.circle(center, LAMP_R, fill, Stroke::new(1.0, p.border));
    }
}

/// A bare lamp: no label, for rows that already name what they are.
pub fn lamp(ui: &mut Ui, lit: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(LAMP_W, 16.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        paint(
            ui,
            egui::pos2(rect.left() + LAMP_R + 1.0, rect.center().y),
            lit,
            false,
        );
    }
    response
}

/// The font the centre cluster's labels are set in.
fn state_font(ui: &Ui) -> FontId {
    FontId::new(12.5, theme::semibold(ui))
}

/// What one lit state costs the cluster horizontally.
pub fn state_lamp_width(ui: &mut Ui, label: &str) -> f32 {
    let font = state_font(ui);
    let text_w = ui.fonts_mut(|f| {
        f.layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER)
            .size()
            .x
    });
    STATE_LABEL_X + text_w
}

/// One lit state in the status bar's centre cluster: a bigger lamp and its
/// label, both in `color`. Only ever drawn for a state that is actually on,
/// so there is no unlit form and an idle bar shows no cluster at all.
///
/// The circle carries the palette colour and the label carries the step of it
/// that reads as text on the bar: ON AIR set in the light accent measured
/// 3.77:1 against the bar and looked switched off.
pub fn state_lamp(ui: &mut Ui, label: &str, color: Color32) -> Response {
    let font = state_font(ui);
    let width = state_lamp_width(ui, label);
    let (rect, response) = ui.allocate_exact_size(vec2(width, STATE_LAMP_SIZE), Sense::hover());
    // The lamp is painted, so its state reaches a screen reader only if it is
    // said out loud here. These are the two that have to.
    response.widget_info(|| WidgetInfo::labeled(egui::WidgetType::Label, true, label));
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let p = theme::palette_of(ui);
    ui.painter().circle(
        egui::pos2(rect.left() + STATE_LAMP_R + 1.0, rect.center().y),
        STATE_LAMP_R,
        color,
        Stroke::new(1.0, theme::blend(color, p.text_primary, 0.45)),
    );
    ui.painter().text(
        egui::pos2(rect.left() + STATE_LABEL_X, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        theme::readable(color, p.surface0, p),
    );
    response
}

/// A clickable lamp with its label: the circle lights with the accent while
/// on. The label is the accessible name; the caller flips its state on
/// click and adds hover text.
pub fn lamp_toggle(ui: &mut Ui, label: &str, lit: bool) -> Response {
    let font = egui::FontId::new(12.5, egui::FontFamily::Proportional);
    let text_w = ui.fonts_mut(|f| {
        f.layout_no_wrap(label.to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
            .size()
            .x
    });
    let (rect, response) = ui.allocate_exact_size(vec2(15.0 + text_w, 18.0), Sense::click());
    response.widget_info(|| WidgetInfo::selected(egui::WidgetType::Checkbox, true, lit, label));
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let p = theme::palette_of(ui);
    paint(
        ui,
        egui::pos2(rect.left() + 5.0, rect.center().y),
        lit,
        response.hovered(),
    );
    let color = if lit { p.text_primary } else { p.text_muted };
    ui.painter().text(
        egui::pos2(rect.left() + 15.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        color,
    );
    response
}
