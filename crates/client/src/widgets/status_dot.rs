//! Two dots that are built alike and say different things.
//!
//! [`status_dot`] is the quality of *your* link: green under 25 ms, amber
//! under 40, red above or on a lossy link, muted when not connected. It is one
//! dot in one place, and its colour is a level, which is what earns it the
//! meter's vocabulary.
//!
//! [`presence_dot`] is whether somebody else is in the room, which is all a
//! member's strip actually knows. Per-member rtt and loss arrive with the
//! Stats control message; until they do, a strip that painted your own
//! numbers in their colour had every dot in the console reading green
//! whatever the far end was doing. There are up to ten of these on screen at
//! once, so the resting state is nearly silent and the exception is the one
//! that reads.

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

/// How present a strip's member is, at the resting state's own volume.
///
/// Being here is the assumption, so it is nearly silent: a filled dot barely
/// off the panel. Ten bright dots in a healthy band all said nothing happened,
/// and the one state worth catching mid-song was the quiet one (#254). Gone is
/// the ring, at the step this console uses for things that are off, alongside
/// the whole strip greying out.
///
/// Still colourless. Green is the meter's vocabulary and encodes a level, so a
/// green dot beside Ana would claim a per-member link quality this side cannot
/// measure, which is the defect #174 fixed. When per-member rtt and loss arrive
/// with the Stats control message the dot has a real level and the meter
/// colours become legitimate; the amber the issue wants for the unresponsive
/// window needs a per-member last-heard signal that does not exist yet, and is
/// deliberately not faked here.
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
        ui.painter()
            .circle_filled(rect.center(), 4.0, present_ink(p));
    } else {
        // Muted, not the hairline: gone is the exception, and it is the one of
        // the two that has to read without being looked for.
        ui.painter()
            .circle_stroke(rect.center(), 3.5, Stroke::new(1.25, p.text_muted));
    }
    response.on_hover_text(if connected {
        "in the session; per-member link quality arrives with a protocol update"
    } else {
        PRESENCE_AWAY
    });
}

/// The resting dot's fill: the panel nudged a fifth of the way toward the text
/// colour. Present, and quieter than anything else in the strip.
pub fn present_ink(p: &theme::Palette) -> egui::Color32 {
    theme::blend(p.surface1, p.text_primary, 0.2)
}
