//! Two dots that are built alike and say different things.
//!
//! [`status_dot`] is the quality of *your* link: green under 25 ms, amber
//! under 40, red above or on a lossy link, muted when not connected.
//! [`presence_dot`] is whether somebody else is in the room, which is all a
//! member's strip actually knows. Per-member rtt and loss arrive with the
//! Stats control message; until they do, a strip that painted your own
//! numbers in their colour had every dot in the console reading green
//! whatever the far end was doing.

use egui::{Sense, Stroke, Ui, WidgetInfo, vec2};

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

/// What one presence dot says, out loud. Painted dots reach a screen reader
/// only if their state is said here, and this one is also how a test tells the
/// two states apart without reading pixels.
pub const PRESENCE_HERE: &str = "in the session";
pub const PRESENCE_AWAY: &str = "not connected";

/// Presence, and nothing about the link: filled while a member is in the
/// session, an empty ring once they are gone. Deliberately colourless, since
/// colour on a strip is a claim about a connection this side cannot measure.
pub fn presence_dot(ui: &mut Ui, connected: bool) {
    let (rect, response) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    let word = if connected {
        PRESENCE_HERE
    } else {
        PRESENCE_AWAY
    };
    response.widget_info(|| WidgetInfo::labeled(egui::WidgetType::Label, true, word));
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = theme::palette_of(ui);
    if connected {
        ui.painter().circle_filled(rect.center(), 4.0, p.text_muted);
    } else {
        ui.painter()
            .circle_stroke(rect.center(), 3.5, Stroke::new(1.0, p.border));
    }
    response.on_hover_text(if connected {
        "in the session; per-member link quality arrives with a protocol update"
    } else {
        PRESENCE_AWAY
    });
}
