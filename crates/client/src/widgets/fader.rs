//! Vertical gain fader. Drag to set, shift or alt for fine adjustment,
//! double-click returns to 0 dB, scroll steps 0.5 dB, arrow keys nudge
//! when focused.

use egui::{
    CornerRadius, Key, Modifiers, Rect, Response, Sense, Stroke, Ui, Vec2, WidgetInfo, pos2, vec2,
};

use crate::theme;

pub const FADER_MIN_DB: f32 = -60.0;
pub const FADER_MAX_DB: f32 = 12.0;
pub const FADER_DEFAULT_DB: f32 = 0.0;

const STEP_DB: f32 = 0.5;
const FINE_STEP_DB: f32 = 0.1;
const FINE_DRAG_SCALE: f32 = 0.1;
const HANDLE: Vec2 = vec2(22.0, 10.0);

/// The accessible label should name the member, e.g. "Ana fader".
pub fn fader(ui: &mut Ui, label: &str, gain_db: &mut f32) -> Response {
    let desired = vec2(26.0, 128.0);
    let (rect, mut response) = ui.allocate_exact_size(desired, Sense::click_and_drag());
    let enabled = ui.is_enabled();
    let mut changed = false;

    let travel = rect.height() - HANDLE.y;
    let db_per_px = (FADER_MAX_DB - FADER_MIN_DB) / travel;

    if response.double_clicked() {
        *gain_db = FADER_DEFAULT_DB;
        changed = true;
    } else if response.dragged() {
        let fine = ui.input(|i| i.modifiers.shift || i.modifiers.alt);
        let mut delta = -response.drag_delta().y * db_per_px;
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
        let p = theme::palette_of(ui);
        let painter = ui.painter();
        let cx = rect.center().x;
        let top = rect.top() + HANDLE.y / 2.0;
        let bottom = rect.bottom() - HANDLE.y / 2.0;

        // Track, with a tick at 0 dB.
        painter.line_segment(
            [pos2(cx, top), pos2(cx, bottom)],
            Stroke::new(2.0, p.border),
        );
        let zero_t = (FADER_DEFAULT_DB - FADER_MIN_DB) / (FADER_MAX_DB - FADER_MIN_DB);
        let zero_y = bottom - zero_t * (bottom - top);
        painter.line_segment(
            [pos2(cx - 6.0, zero_y), pos2(cx + 6.0, zero_y)],
            Stroke::new(1.0, p.text_muted),
        );

        // Handle.
        let t = (gain_db.clamp(FADER_MIN_DB, FADER_MAX_DB) - FADER_MIN_DB)
            / (FADER_MAX_DB - FADER_MIN_DB);
        let y = bottom - t * (bottom - top);
        let handle = Rect::from_center_size(pos2(cx, y), HANDLE);
        let visuals = ui.style().interact(&response);
        let outline = if response.has_focus() {
            // Focus is always visible; the accent marks the active control.
            Stroke::new(1.5, p.accent)
        } else {
            visuals.bg_stroke
        };
        painter.rect(
            handle,
            CornerRadius::same(2),
            visuals.bg_fill,
            outline,
            egui::StrokeKind::Inside,
        );
        painter.line_segment(
            [pos2(handle.left() + 4.0, y), pos2(handle.right() - 4.0, y)],
            Stroke::new(1.0, p.text_primary),
        );
    }
    response
}
