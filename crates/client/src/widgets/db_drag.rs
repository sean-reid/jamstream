//! Compact dB drag-value: the instrument numeric readout made adjustable,
//! for rows too tight for a fader. Vertical drag to set, shift or alt for
//! fine adjustment, double-click returns to 0 dB, scroll and arrow keys
//! step 0.5 dB. Same range and default as the fader.

use egui::{CornerRadius, Key, Modifiers, Response, Sense, Stroke, Ui, Vec2, WidgetInfo};

use super::fader::{FADER_DEFAULT_DB, FADER_MAX_DB, FADER_MIN_DB};
use crate::theme;

const STEP_DB: f32 = 0.5;
const FINE_STEP_DB: f32 = 0.1;
/// dB per pixel of vertical drag; a 66 px pull covers half the range.
const DRAG_DB_PER_PX: f32 = 0.5;
const FINE_DRAG_SCALE: f32 = 0.1;

/// The accessible label should name the member and the mix, e.g.
/// "Ana stream gain".
pub fn db_drag(ui: &mut Ui, label: &str, gain_db: &mut f32, size: Vec2) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let enabled = ui.is_enabled();
    let mut changed = false;

    if response.double_clicked() {
        *gain_db = FADER_DEFAULT_DB;
        changed = true;
    } else if response.dragged() {
        let fine = ui.input(|i| i.modifiers.shift || i.modifiers.alt);
        let mut delta = -response.drag_delta().y * DRAG_DB_PER_PX;
        if fine {
            delta *= FINE_DRAG_SCALE;
        }
        if delta != 0.0 {
            *gain_db += delta;
            changed = true;
        }
    }
    if response.clicked() {
        response.request_focus();
    }
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            *gain_db += STEP_DB * scroll.signum();
            changed = true;
        }
    }
    if response.has_focus() {
        let (up, down, fine) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp)
                    || i.consume_key(Modifiers::SHIFT, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown)
                    || i.consume_key(Modifiers::SHIFT, Key::ArrowDown),
                i.modifiers.shift,
            )
        });
        let step = if fine { FINE_STEP_DB } else { STEP_DB };
        if up {
            *gain_db += step;
            changed = true;
        }
        if down {
            *gain_db -= step;
            changed = true;
        }
    }
    if changed {
        *gain_db = gain_db.clamp(FADER_MIN_DB, FADER_MAX_DB);
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::slider(enabled, f64::from(*gain_db), label));

    if ui.is_rect_visible(rect) {
        use egui::emath::GuiRounding;
        let rect = rect.round_to_pixels(ui.pixels_per_point());
        let p = theme::palette_of(ui);
        // An engraved well like the text inputs; focus draws the accent.
        let outline = if response.has_focus() {
            Stroke::new(1.5, p.accent)
        } else {
            Stroke::new(1.0, p.border)
        };
        ui.painter().rect(
            rect,
            CornerRadius::same(theme::RADIUS),
            p.well,
            outline,
            egui::StrokeKind::Inside,
        );
        let text = if *gain_db <= -59.95 {
            "-inf dB".to_owned()
        } else {
            format!("{:+.1} dB", *gain_db)
        };
        let color = if enabled {
            p.text_primary
        } else {
            p.text_muted
        };
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::new(12.5, egui::FontFamily::Monospace),
            color,
        );
    }
    response.on_hover_text("drag to set, double-click for 0 dB, shift for fine")
}
