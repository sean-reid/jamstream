//! The one radio treatment in the app: a selectable row with a filled
//! accent dot, hover lift, and a selected surface step. Replaces egui's
//! radio everywhere a choice is made.

use egui::{Response, Ui, vec2};

use crate::theme;

/// Height of one selectable row.
pub const PICK_ROW_H: f32 = 30.0;
/// Diameter of the selection dot.
pub const PICK_DOT: f32 = 14.0;
/// Left padding plus dot plus gap; header rows indent by this to align.
pub const PICK_INDENT: f32 = 8.0 + PICK_DOT + 8.0;

/// A selectable row. Cells are laid out by the caller inside the row ui;
/// use [`row_cell`] for fixed-width columns.
pub fn pick_row(
    ui: &mut Ui,
    label: &str,
    selected: bool,
    enabled: bool,
    cells: impl FnOnce(&mut Ui),
) -> Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(
        vec2(width, PICK_ROW_H),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let p = theme::palette_of(ui);
    if selected {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(theme::RADIUS), p.surface2);
    } else if enabled && response.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(theme::RADIUS),
            theme::blend(p.surface1, p.surface2, 0.5),
        );
    }
    let dot = egui::pos2(rect.left() + 8.0 + PICK_DOT / 2.0, rect.center().y);
    if selected {
        ui.painter().circle_filled(dot, PICK_DOT / 2.0, p.accent);
    } else {
        let stroke = if enabled { p.text_muted } else { p.border };
        ui.painter().circle(
            dot,
            PICK_DOT / 2.0 - 0.5,
            p.well,
            egui::Stroke::new(1.0, stroke),
        );
    }
    let content =
        egui::Rect::from_min_max(egui::pos2(rect.left() + PICK_INDENT, rect.top()), rect.max);
    let mut row_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(content)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    if !enabled {
        row_ui.disable();
    }
    cells(&mut row_ui);
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::RadioButton, enabled, selected, label)
    });
    response
}

/// One fixed-width, left-aligned cell inside a pick row.
pub fn row_cell(ui: &mut Ui, width: f32, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(
        vec2(width, PICK_ROW_H),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(vec2(width, PICK_ROW_H));
            add(ui);
        },
    );
}
