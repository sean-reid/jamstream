//! The on-air lamp: a small circle and a muted label, no border box. Lit
//! with the accent (plus a 1 px brighter ring) only while a broadcast is
//! live. Broadcast itself lands in M2, so v1 always shows it dark.

use egui::{Sense, Stroke, Ui, vec2};

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
