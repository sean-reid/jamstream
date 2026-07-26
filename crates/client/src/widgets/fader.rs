//! Vertical gain fader with an audio taper: the top quarter of the travel
//! covers +6..-6 dB, the middle half -6..-30, the bottom quarter -30..-60.
//! Drag to set, shift or alt for fine adjustment, double-click returns to
//! 0 dB, scroll steps 0.5 dB, arrow keys nudge when focused.

use egui::{
    CornerRadius, Key, Modifiers, Rect, Response, Sense, Stroke, Ui, Vec2, WidgetInfo, pos2, vec2,
};

use crate::theme;

pub const FADER_MIN_DB: f32 = -60.0;
pub const FADER_MAX_DB: f32 = 6.0;
pub const FADER_DEFAULT_DB: f32 = 0.0;

const STEP_DB: f32 = 0.5;
const FINE_STEP_DB: f32 = 0.1;
const FINE_DRAG_SCALE: f32 = 0.1;
const HANDLE_H: f32 = 10.0;

/// Tick rows on the track; 0 dB is emphasized.
const TICKS_DB: [f32; 6] = [6.0, 0.0, -6.0, -12.0, -24.0, -40.0];

/// dB to normalized travel (0 bottom, 1 top), piecewise like a console.
pub fn db_to_t(db: f32) -> f32 {
    let db = db.clamp(FADER_MIN_DB, FADER_MAX_DB);
    if db >= -6.0 {
        0.75 + (db + 6.0) / 12.0 * 0.25
    } else if db >= -30.0 {
        0.25 + (db + 30.0) / 24.0 * 0.5
    } else {
        (db + 60.0) / 30.0 * 0.25
    }
}

/// Inverse of [`db_to_t`].
pub fn t_to_db(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t >= 0.75 {
        -6.0 + (t - 0.75) / 0.25 * 12.0
    } else if t >= 0.25 {
        -30.0 + (t - 0.25) / 0.5 * 24.0
    } else {
        -60.0 + t / 0.25 * 30.0
    }
}

/// The accessible label should name the member, e.g. "Ana fader".
pub fn fader(ui: &mut Ui, label: &str, gain_db: &mut f32, size: Vec2) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let enabled = ui.is_enabled();
    let mut changed = false;

    let travel = (rect.height() - HANDLE_H).max(1.0);

    if response.double_clicked() {
        *gain_db = FADER_DEFAULT_DB;
        changed = true;
    } else if response.dragged() {
        let fine = ui.input(|i| i.modifiers.shift || i.modifiers.alt);
        let mut dt = -response.drag_delta().y / travel;
        if fine {
            dt *= FINE_DRAG_SCALE;
        }
        if dt != 0.0 {
            *gain_db = t_to_db(db_to_t(*gain_db) + dt);
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
        let p = theme::palette_of(ui);
        let ppp = ui.pixels_per_point();
        let painter = ui.painter();
        // Everything on the pixel grid so ticks and grooves stay crisp.
        let cx = rect.center().x.round_to_pixels(ppp);
        let top = (rect.top() + HANDLE_H / 2.0).round_to_pixels(ppp);
        let bottom = (rect.bottom() - HANDLE_H / 2.0).round_to_pixels(ppp);
        let y_of = |db: f32| (bottom - db_to_t(db) * (bottom - top)).round_to_pixels(ppp);

        // Ticks first, then the track groove over them.
        for db in TICKS_DB {
            let y = y_of(db);
            let (reach, stroke) = if db == 0.0 {
                (rect.width() / 2.0 - 1.0, Stroke::new(1.0, p.text_muted))
            } else {
                (rect.width() / 2.0 - 5.0, Stroke::new(1.0, p.border))
            };
            painter.line_segment([pos2(cx - reach, y), pos2(cx + reach, y)], stroke);
        }
        // Engraved track: a dark groove with a hairline edge.
        painter.line_segment([pos2(cx, top), pos2(cx, bottom)], Stroke::new(3.0, p.well));
        painter.line_segment(
            [pos2(cx, top), pos2(cx, bottom)],
            Stroke::new(1.0, p.border),
        );

        // Handle: a machined cap with a center groove; hover and drag step
        // the surface, nothing glows.
        let y = y_of(*gain_db);
        let handle = Rect::from_center_size(pos2(cx, y), vec2(rect.width() - 4.0, HANDLE_H))
            .round_to_pixels(ppp);
        let visuals = ui.style().interact(&response);
        let outline = if response.has_focus() {
            // Focus is always visible; the accent marks the active control.
            Stroke::new(1.5, p.accent)
        } else {
            Stroke::new(1.0, p.border)
        };
        let (fill, groove) = if enabled {
            (visuals.bg_fill, p.text_primary)
        } else {
            (p.surface1, p.text_muted)
        };
        painter.rect(
            handle,
            CornerRadius::same(2),
            fill,
            outline,
            egui::StrokeKind::Inside,
        );
        painter.line_segment(
            [pos2(handle.left() + 3.0, y), pos2(handle.right() - 3.0, y)],
            Stroke::new(1.0, groove),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taper_round_trips_and_hits_anchors() {
        assert_eq!(db_to_t(FADER_MAX_DB), 1.0);
        assert_eq!(db_to_t(-6.0), 0.75);
        assert_eq!(db_to_t(-30.0), 0.25);
        assert_eq!(db_to_t(FADER_MIN_DB), 0.0);
        for db in [-60.0, -40.0, -30.0, -12.0, -6.0, -3.0, 0.0, 6.0] {
            assert!((t_to_db(db_to_t(db)) - db).abs() < 1e-4, "round trip {db}");
        }
    }
}
