//! The on-air lamp. Lit with the accent only while a broadcast is live;
//! otherwise a dark, bordered placeholder holding its place in the bar.
//! Broadcast itself lands in M2, so v1 always shows it dark.

use egui::{CornerRadius, Sense, Stroke, Ui, vec2};

use crate::theme;

pub fn on_air(ui: &mut Ui, live: bool) {
    let (rect, response) = ui.allocate_exact_size(vec2(56.0, 20.0), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = theme::palette_of(ui);
    let painter = ui.painter();
    let (fill, text_color) = if live {
        (p.accent, p.surface0)
    } else {
        (p.surface1, p.text_muted)
    };
    painter.rect(
        rect,
        CornerRadius::same(theme::RADIUS),
        fill,
        Stroke::new(1.0, if live { p.accent } else { p.border }),
        egui::StrokeKind::Inside,
    );
    let family = theme::semibold(ui);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "on air",
        egui::FontId::new(11.0, family),
        text_color,
    );
    response.on_hover_text(if live {
        "broadcast is live"
    } else {
        "no broadcast running"
    });
}
