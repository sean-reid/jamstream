//! Connection quality dot: green under 25 ms, amber under 40, red above or
//! on a lossy link, muted when not connected.

use egui::{Sense, Ui, vec2};

use crate::theme;

const LOSSY_PCT: f32 = 2.0;

pub fn status_dot(ui: &mut Ui, connected: bool, rtt_ms: Option<f32>, loss_pct: f32) {
    let (rect, response) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = theme::palette_of(ui);
    let color = match (connected, rtt_ms) {
        (false, _) | (true, None) => p.text_muted,
        (true, Some(rtt)) => {
            if loss_pct > LOSSY_PCT || rtt >= 40.0 {
                p.meter_red
            } else if rtt >= 25.0 {
                p.meter_amber
            } else {
                p.meter_green
            }
        }
    };
    ui.painter().circle_filled(rect.center(), 4.0, color);
    let hover = match (connected, rtt_ms) {
        (false, _) => "not connected".to_owned(),
        (true, None) => "no round trip sample yet".to_owned(),
        (true, Some(rtt)) => format!("rtt {rtt:.1} ms, loss {loss_pct:.1}%"),
    };
    response.on_hover_text(hover);
}
