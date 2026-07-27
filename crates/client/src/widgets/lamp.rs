//! The lamp: one small circle, lit with the accent (plus a 1 px brighter
//! ring) while a state is on, a dark raised surface while it is off. Three
//! callers, one treatment. [`lamp`] is the bare indicator, [`on_air`] pairs
//! it with the words that matter most in a session, and [`lamp_toggle`] is
//! the clickable sibling for a state that is switched rather than only shown.

use egui::{Response, Sense, Stroke, Ui, WidgetInfo, vec2};

use crate::theme;

/// Lamp diameter and the space one occupies inline.
const LAMP_R: f32 = 4.0;
const LAMP_W: f32 = 11.0;

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

pub fn on_air(ui: &mut Ui, live: bool) {
    let font = egui::FontId::new(11.5, egui::FontFamily::Proportional);
    let (rect, response) = ui.allocate_exact_size(vec2(48.0, 16.0), Sense::hover());
    // The lamp is painted, so its state reaches a screen reader only if it is
    // said out loud here. It is the one session state that has to.
    let name = if live { "on air" } else { "not on air" };
    response.widget_info(|| WidgetInfo::labeled(egui::WidgetType::Label, true, name));
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = theme::palette_of(ui);
    paint(
        ui,
        egui::pos2(rect.left() + 5.0, rect.center().y),
        live,
        false,
    );
    // Live steps the label to primary as well as lighting the lamp: on air is
    // the one session state that has to read from across the room.
    let color = if live { p.text_primary } else { p.text_muted };
    ui.painter().text(
        egui::pos2(rect.left() + 13.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        "on air",
        font,
        color,
    );
    response.on_hover_text(if live {
        "this session is being broadcast"
    } else {
        "no broadcast running"
    });
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
