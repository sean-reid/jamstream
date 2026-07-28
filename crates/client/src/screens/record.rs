//! The record sheet and the words its lamp uses. Record is the session's one
//! performance action, so it keeps a button in the status bar and a sheet
//! behind it; the lamp itself lives in the bar's centre cluster next to ON
//! AIR, because a take and a broadcast are the two states everyone in the
//! room needs to see at once.
//!
//! Nothing here echoes optimistically: the button sends the command and the
//! lamp follows the snapshot, exactly as the destinations section follows
//! its stream.

use egui::{Align, Align2, Button, Color32, Layout, RichText, Sense, Stroke, Ui, vec2};

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

/// What the centre cluster shows for the recorder: the word, its colour, and
/// the sentence on hover. None while idle, which is what keeps an idle bar
/// free of a cluster.
pub fn record_state_lamp(
    state: &RecordState,
    p: &theme::Palette,
) -> Option<(&'static str, Color32, String)> {
    let color = lamp_fill(state, p)?;
    let (label, hover) = match state {
        // Never reached: lamp_fill answers None for idle.
        RecordState::Idle => return None,
        RecordState::Recording => ("REC", "this session is being recorded".to_owned()),
        RecordState::Uploading => (
            "UPLOADING",
            "the take is on its way to storage; not done, not lost".to_owned(),
        ),
        RecordState::Failed { reason } => ("REC FAILED", reason.clone()),
    };
    Some((label, color, hover))
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
