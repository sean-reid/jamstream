//! Two dots that are built alike and say different things.
//!
//! [`status_dot`] is the quality of *your* link: green under 25 ms, amber
//! under 40, red above or on a lossy link, muted when not connected. It is one
//! dot in one place, and its colour is a level, which is what earns it the
//! meter's vocabulary.
//!
//! [`presence_dot`] is whether somebody else is in the room, and whether the
//! server has heard from them lately, which is all a member's strip actually
//! knows. Per-member rtt and loss arrive with the Stats control message; until
//! they do, a strip that painted your own numbers in their colour had every dot
//! in the console reading green whatever the far end was doing. There are up to
//! ten of these on screen at once, so the resting state is nearly silent and
//! the exception is the one that reads.

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
/// only if their state is said here, and this is also how a test tells the
/// three states apart without reading pixels.
pub const PRESENCE_HERE: &str = "in the session";
pub const PRESENCE_QUIET: &str = "gone quiet";
pub const PRESENCE_AWAY: &str = "not connected";

/// How present a strip's member is, at the resting state's own volume.
///
/// Being here is the assumption, so it is nearly silent: a filled dot barely
/// off the panel. Ten bright dots in a healthy band all said nothing happened,
/// and the one state worth catching mid-song was the quiet one. Gone is
/// the ring, at the step this console uses for things that are off, alongside
/// the whole strip greying out.
///
/// `quiet` is the window in between: the server has not heard from this
/// member for `MEMBER_QUIET_AFTER_MS`, and will not give up on them for
/// eight seconds more. Without an indicator, a stall and a musician playing
/// would look identical for that whole window, which is the state worth
/// catching mid-song. It is still filled, because they are still connected
/// and still holding their seat; the shape carries here against gone, the
/// way the strip's grey-out does, and the amber is the exception inside
/// here.
///
/// Amber and not green. Green is the meter's vocabulary and encodes a level, so
/// a green dot beside Ana would claim a per-member link quality this side
/// cannot measure. This amber claims no level: it is
/// the colour `theme::style` already sets as the app's warning ink, and it is
/// reporting a fact the server asserted. When per-member rtt and loss do arrive
/// with the Stats control message the dot has a real level and the rest of the
/// meter's colours become legitimate too.
pub fn presence_dot(ui: &mut Ui, connected: bool, quiet: bool) {
    let (rect, response) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    // Gone wins over quiet. The server clears the flag when it drops a member,
    // so the pair cannot arrive this way round, and a dot reading both at once
    // would be the one that is wrong.
    let word = match (connected, quiet) {
        (false, _) => PRESENCE_AWAY,
        (true, true) => PRESENCE_QUIET,
        (true, false) => PRESENCE_HERE,
    };
    response.widget_info(|| WidgetInfo::labeled(egui::WidgetType::Label, true, word));
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = theme::palette_of(ui);
    if connected {
        match presence_ink(p, quiet) {
            // The lamp's own lit form, rim and all, because that is what this
            // is: the one indicator in the strip that is on rather than
            // resting. The rim is also the cue that survives the colour
            // being missed.
            (fill, Some(rim)) => {
                ui.painter()
                    .circle(rect.center(), 4.0, fill, Stroke::new(1.0, rim));
            }
            (fill, None) => {
                ui.painter().circle_filled(rect.center(), 4.0, fill);
            }
        }
    } else {
        // Muted, not the hairline: gone is the exception, and it has to read
        // without being looked for.
        ui.painter()
            .circle_stroke(rect.center(), 3.5, Stroke::new(1.25, p.text_muted));
    }
    response.on_hover_text(match (connected, quiet) {
        (false, _) => PRESENCE_AWAY.to_owned(),
        (true, true) => format!(
            "{PRESENCE_QUIET}: the server has heard nothing for over {} seconds, and still \
             holds their seat",
            jamstream_session::MEMBER_QUIET_AFTER_MS / 1000
        ),
        (true, false) => {
            "in the session; per-member link quality arrives with a protocol update".to_owned()
        }
    });
}

/// The resting dot's fill: the panel nudged a fifth of the way toward the text
/// colour. Present, and quieter than anything else in the strip.
pub fn present_ink(p: &theme::Palette) -> egui::Color32 {
    theme::blend(p.surface1, p.text_primary, 0.2)
}

/// A connected member's dot: its fill, and the rim a lit lamp carries. `None`
/// is the resting dot, which is flat. In one place so the test below can ask
/// for both without reading pixels off a render.
///
/// The quiet fill is the amber stepped the way a state word is. A lamp with a
/// label beside it can keep the palette value, because the word carries the
/// meaning; this dot has no label, and its neighbour two strips over is the
/// resting dot, so the pair is what has to read. Straight off the palette they
/// measure 2.25:1 apart in light, against 5.09:1 in dark. Stepping costs
/// nothing in dark, where the amber already clears the floor and comes back
/// untouched.
fn presence_ink(p: &theme::Palette, quiet: bool) -> (egui::Color32, Option<egui::Color32>) {
    if quiet {
        let ink = theme::readable(p.meter_amber, p.surface1, p);
        (ink, Some(theme::blend(ink, p.text_primary, 0.45)))
    } else {
        (present_ink(p), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{AA_STATE, DARK, LIGHT, contrast_ratio};

    /// The quiet dot is the one thing on a strip that has to be noticed without
    /// being looked for, and it has no label of its own, so two pairs have to
    /// read: the dot against the panel it sits on, and the dot against the
    /// resting dot two strips over. Both are held to `AA_STATE`, the floor for
    /// something that carries state on its own, not to the text floor.
    ///
    /// Measured: 9.08:1 and 5.09:1 in dark, 4.54:1 and 3.00:1 in light. Light
    /// is where it bites, and it is why the amber is stepped rather than taken
    /// straight off the palette, which lands at 2.25:1 against the resting dot.
    ///
    /// The last two assertions check that colour is not the only difference,
    /// and that the step does not turn the warning into some fourth colour.
    /// The lit dot carries a rim and the resting one does not, so the two are
    /// different marks before they are different colours, which is the same
    /// argument `LampShape` settles for the status bar's cluster.
    #[test]
    fn the_quiet_dot_reads_against_the_panel_and_is_not_only_a_colour() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            let (quiet, quiet_rim) = presence_ink(p, true);
            let (here, here_rim) = presence_ink(p, false);
            let on_panel = contrast_ratio(quiet, p.surface1);
            assert!(
                on_panel >= AA_STATE,
                "{name} quiet dot on the strip is {on_panel:.2}, below {AA_STATE}"
            );
            let apart = contrast_ratio(quiet, here);
            assert!(
                apart >= AA_STATE,
                "{name} quiet and resting dots are {apart:.2} apart, which is one dot"
            );
            assert!(
                quiet_rim.is_some() && here_rim.is_none(),
                "{name} quiet and resting dots differ by colour alone"
            );
            assert!(
                contrast_ratio(quiet, p.meter_amber) < 2.0,
                "{name} the quiet dot is no longer the warning amber"
            );
            // Present is still colourless: the panel nudged toward the strip's
            // own text colour, not a level. A green dot would claim a
            // per-member reading this side cannot measure.
            assert_eq!(here, theme::blend(p.surface1, p.text_primary, 0.2));
        }
    }
}
