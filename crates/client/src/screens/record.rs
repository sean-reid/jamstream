//! The record sheet and its lamp. The status bar has no room for a fifth
//! button (it overlapped itself at 1280 once already), so Record follows
//! the pattern Destinations established: a host-only sheet carrying the
//! control, and a lamp beside the on-air lamp that everyone in the room
//! sees whenever a take is running, uploading, or failed.
//!
//! Nothing here echoes optimistically: the button sends the command and
//! the lamp follows the snapshot, exactly as the destinations sheet
//! follows its stream.

use egui::{Align, Align2, Button, Color32, Layout, RichText, Sense, Stroke, Ui, WidgetInfo, vec2};

use crate::runtime::{Command, RecordState, Runtime, Snapshot};
use crate::theme;

/// The lamp's fill, in the meter's color language: red while a take is
/// captured, which is the color musicians expect on a record lamp; amber
/// while the upload drains, in progress rather than wrong; the danger
/// color when the recorder failed. Idle paints the dark raised lamp.
fn lamp_fill(state: &RecordState, p: &theme::Palette) -> Option<Color32> {
    match state {
        RecordState::Idle => None,
        RecordState::Recording => Some(p.meter_red),
        RecordState::Uploading => Some(p.meter_amber),
        RecordState::Failed { .. } => Some(p.danger),
    }
}

/// One record lamp at `center`, the on-air lamp's construction in the
/// state's own color, so the two read as siblings in the bar.
fn paint_lamp(ui: &Ui, center: egui::Pos2, fill: Option<Color32>) {
    let p = theme::palette_of(ui);
    match fill {
        Some(color) => ui.painter().circle(
            center,
            4.0,
            color,
            Stroke::new(1.0, theme::blend(color, p.text_primary, 0.45)),
        ),
        None => ui
            .painter()
            .circle(center, 4.0, p.surface2, Stroke::new(1.0, p.border)),
    };
}

/// The reminder the whole room gets, beside the on-air lamp: nothing while
/// idle, then the lamp and the state's word. A failure carries the
/// recorder's reason verbatim on hover, because a recording that fails
/// quietly is worse than one that never started.
pub fn record_indicator(ui: &mut Ui, snap: &Snapshot) {
    let state = &snap.record.state;
    let p = theme::palette_of(ui);
    let (label, color, hover) = match state {
        RecordState::Idle => return,
        RecordState::Recording => (
            "recording",
            p.text_primary,
            "this session is being recorded".to_owned(),
        ),
        RecordState::Uploading => (
            "uploading",
            p.text_primary,
            "the take is on its way to storage; not done, not lost".to_owned(),
        ),
        RecordState::Failed { reason } => ("take failed", p.danger, reason.clone()),
    };
    let font = egui::FontId::new(11.5, egui::FontFamily::Proportional);
    let text_w = ui.fonts_mut(|f| {
        f.layout_no_wrap(label.to_owned(), font.clone(), Color32::PLACEHOLDER)
            .size()
            .x
    });
    let (rect, response) = ui.allocate_exact_size(vec2(13.0 + text_w, 16.0), Sense::hover());
    // The lamp is painted, so its state reaches a screen reader only if it
    // is said out loud here, the same as the on-air lamp.
    response.widget_info(|| WidgetInfo::labeled(egui::WidgetType::Label, true, label));
    if ui.is_rect_visible(rect) {
        paint_lamp(
            ui,
            egui::pos2(rect.left() + 5.0, rect.center().y),
            lamp_fill(state, p),
        );
        ui.painter().text(
            egui::pos2(rect.left() + 13.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font,
            color,
        );
    }
    response.on_hover_text(hover);
}

/// The host's record sheet: the take's state, whether stems are being
/// captured, and the one control that starts or ends a take. Everyone else
/// gets the lamp; only this sheet gets the button.
pub fn record_sheet(ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime, open: &mut bool) {
    egui::Window::new("Record")
        .title_bar(false)
        .frame(theme::sheet_frame(theme::palette_of(ui)))
        .anchor(Align2::RIGHT_TOP, theme::SHEET_OFFSET)
        .fixed_size(vec2(384.0, 0.0))
        .resizable(false)
        .show(ui.ctx(), |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::title(ui, "Record"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        *open = false;
                    }
                });
            });
            ui.label(theme::muted(
                ui,
                "The take is the mix listeners hear, kept in the session's storage.",
            ));
            ui.add_space(theme::SPACE_SM);
            state_row(ui, snap);
            ui.add_space(theme::SPACE_SM);
            ui.label(
                theme::muted(
                    ui,
                    if snap.record.stems {
                        "Capturing the mix and a stereo stem per member, chosen at launch."
                    } else {
                        "Capturing the mix only; stems were not enabled at launch."
                    },
                )
                .small(),
            );
            ui.add_space(theme::SPACE_MD);
            ui.separator();
            control_row(ui, snap, rt);
        });
}

/// The lamp, the state's word, and, when the recorder failed, its reason
/// verbatim and full width: summarizing an error hides the part someone
/// can act on.
fn state_row(ui: &mut Ui, snap: &Snapshot) {
    let p = theme::palette_of(ui);
    let state = &snap.record.state;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(11.0, 16.0), Sense::hover());
        if ui.is_rect_visible(rect) {
            paint_lamp(
                ui,
                egui::pos2(rect.left() + 5.0, rect.center().y),
                lamp_fill(state, p),
            );
        }
        let (word, color) = match state {
            RecordState::Idle => ("idle", p.text_muted),
            RecordState::Recording => ("recording", p.meter_red),
            RecordState::Uploading => ("uploading", p.meter_amber),
            RecordState::Failed { .. } => ("failed", p.danger),
        };
        ui.label(RichText::new(word).color(color));
        if matches!(state, RecordState::Uploading) {
            // Its own state, said plainly: the take left the session and
            // is neither done nor lost until the lamp goes dark.
            ui.label(theme::muted(ui, "the take is safe once this clears"));
        }
    });
    if let RecordState::Failed { reason } = state {
        // The reason the recorder gave, verbatim, the treatment a dropped
        // stream gets on the destinations sheet.
        ui.add(egui::Label::new(RichText::new(reason.clone()).color(p.danger)).wrap());
    }
}

/// Record or Stop, one at a time, with what the press does beside it. The
/// press sends the command and nothing more; the state above answers it.
fn control_row(ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime) {
    ui.horizontal(|ui| match &snap.record.state {
        RecordState::Recording => {
            if ui.button("Stop").clicked() {
                rt.send(Command::StopRecord);
            }
            ui.label(theme::muted(
                ui,
                "Stop ends the take and starts its upload.",
            ));
        }
        RecordState::Uploading => {
            ui.add_enabled(false, Button::new("Record"))
                .on_disabled_hover_text("the last take is still uploading");
            ui.label(theme::muted(
                ui,
                "Record comes back once the upload finishes.",
            ));
        }
        RecordState::Idle | RecordState::Failed { .. } => {
            if ui.button("Record").clicked() {
                rt.send(Command::StartRecord);
            }
            ui.label(theme::muted(
                ui,
                "Everyone in the session sees the lamp while a take runs.",
            ));
        }
    });
}
