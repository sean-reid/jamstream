//! The on-air lamp: a small circle and a muted label, no border box. Lit
//! with the accent (plus a 1 px brighter ring) only while a broadcast is
//! live. Broadcast itself lands in M2, so v1 always shows it dark.
//! [`lamp_toggle`] is the clickable sibling: same language, same lamp,
//! used where a state is switched rather than only shown.

use egui::{Response, Sense, Stroke, Ui, WidgetInfo, vec2};

use crate::theme;

pub fn on_air(ui: &mut Ui, live: bool) {
    let font = egui::FontId::new(11.5, egui::FontFamily::Proportional);
    let (rect, response) = ui.allocate_exact_size(vec2(48.0, 16.0), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = theme::palette_of(ui);
    let painter = ui.painter();
    let center = egui::pos2(rect.left() + 5.0, rect.center().y);
    if live {
        painter.circle(
            center,
            4.0,
            p.accent,
            Stroke::new(1.0, theme::blend(p.accent, p.text_primary, 0.45)),
        );
    } else {
        painter.circle(center, 4.0, p.surface2, Stroke::new(1.0, p.border));
    }
    painter.text(
        egui::pos2(rect.left() + 13.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        "on air",
        font,
        p.text_muted,
    );
    response.on_hover_text(if live {
        "broadcast is live"
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
    let painter = ui.painter();
    let center = egui::pos2(rect.left() + 5.0, rect.center().y);
    if lit {
        painter.circle(
            center,
            4.0,
            p.accent,
            Stroke::new(1.0, theme::blend(p.accent, p.text_primary, 0.45)),
        );
    } else {
        // Hover steps the lamp surface the way buttons step theirs.
        let fill = if response.hovered() {
            theme::blend(p.surface2, p.text_primary, 0.08)
        } else {
            p.surface2
        };
        painter.circle(center, 4.0, fill, Stroke::new(1.0, p.border));
    }
    let color = if lit { p.text_primary } else { p.text_muted };
    painter.text(
        egui::pos2(rect.left() + 15.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        color,
    );
    response
}
