//! Horizontal pan slider: drag to place, shift or alt for fine moves,
//! double-click recenters, arrow keys nudge when focused.

use egui::{
    CornerRadius, Key, Modifiers, Rect, Response, Sense, Stroke, Ui, WidgetInfo, pos2, vec2,
};

use crate::theme;

const STEP: f32 = 0.05;
const FINE_DRAG_SCALE: f32 = 0.1;
const HANDLE_W: f32 = 8.0;

/// The accessible label should name the member, e.g. "Ana pan".
pub fn pan_slider(ui: &mut Ui, label: &str, pan: &mut f32) -> Response {
    let desired = vec2(64.0, 14.0);
    let (rect, mut response) = ui.allocate_exact_size(desired, Sense::click_and_drag());
    let enabled = ui.is_enabled();
    let mut changed = false;

    let travel = rect.width() - HANDLE_W;

    if response.double_clicked() {
        *pan = 0.0;
        changed = true;
    } else if response.dragged() {
        let fine = ui.input(|i| i.modifiers.shift || i.modifiers.alt);
        let mut delta = response.drag_delta().x * 2.0 / travel;
        if fine {
            delta *= FINE_DRAG_SCALE;
        }
        if delta != 0.0 {
            *pan += delta;
            changed = true;
        }
    }
    if response.clicked() {
        response.request_focus();
    }
    if response.has_focus() {
        let (left, right) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowLeft),
                i.consume_key(Modifiers::NONE, Key::ArrowRight),
            )
        });
        if left {
            *pan -= STEP;
            changed = true;
        }
        if right {
            *pan += STEP;
            changed = true;
        }
    }
    if changed {
        *pan = pan.clamp(-1.0, 1.0);
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::slider(enabled, f64::from(*pan), label));

    if ui.is_rect_visible(rect) {
        let p = theme::palette_of(ui);
        let painter = ui.painter();
        let cy = rect.center().y;
        let left = rect.left() + HANDLE_W / 2.0;
        let right = rect.right() - HANDLE_W / 2.0;
        painter.line_segment(
            [pos2(left, cy), pos2(right, cy)],
            Stroke::new(2.0, p.border),
        );
        // Center notch.
        painter.line_segment(
            [
                pos2(rect.center().x, cy - 4.0),
                pos2(rect.center().x, cy + 4.0),
            ],
            Stroke::new(1.0, p.text_muted),
        );
        let t = (pan.clamp(-1.0, 1.0) + 1.0) / 2.0;
        let x = left + t * (right - left);
        let handle = Rect::from_center_size(pos2(x, cy), vec2(HANDLE_W, 12.0));
        let visuals = ui.style().interact(&response);
        let outline = if response.has_focus() {
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
    }
    response
}
