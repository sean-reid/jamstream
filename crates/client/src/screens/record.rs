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
use crate::screens::recording::RetentionNote;
use crate::theme;
use crate::widgets::LampShape;

/// The lamp's fill and shape.
///
/// The recorder keeps one colour through the whole take, `meter_red`, which is
/// what musicians expect on a record lamp. Capturing is filled; the upload
/// draining is the same red hollowed out, because it is the same take
/// finishing. It cannot be `meter_amber`: that is 1.25:1 against the accent
/// the ON AIR lamp beside it carries, leaving two states with very different
/// consequences told apart by their words alone.
///
/// A failed recorder takes the danger colour and a ring, because nothing is
/// being captured. It cannot be a filled danger disc: STREAM FAILED already is
/// one, they can be lit at the same time, and two different failures that look
/// identical is the same defect one layer over.
fn lamp_look(state: &RecordState, p: &theme::Palette) -> Option<(Color32, LampShape)> {
    match state {
        RecordState::Idle => None,
        RecordState::Recording => Some((p.meter_red, LampShape::Filled)),
        RecordState::Uploading => Some((p.meter_red, LampShape::Ring)),
        RecordState::Failed { .. } => Some((p.danger, LampShape::Ring)),
    }
}

/// What the centre cluster shows for the recorder: the word, its lamp, and the
/// sentence on hover. None while idle, which is what keeps an idle bar free of
/// a cluster.
pub fn record_state_lamp(
    state: &RecordState,
    p: &theme::Palette,
) -> Option<(&'static str, Color32, LampShape, String)> {
    let (color, shape) = lamp_look(state, p)?;
    let (label, hover) = match state {
        // Never reached: lamp_look answers None for idle.
        RecordState::Idle => return None,
        RecordState::Recording => ("REC", "this session is being recorded".to_owned()),
        RecordState::Uploading => (
            "UPLOADING",
            "the take is on its way to storage; not done, not lost".to_owned(),
        ),
        RecordState::Failed { reason } => ("REC FAILED", reason.clone()),
    };
    Some((label, color, shape, hover))
}

/// One record lamp at `center`, the cluster lamp's construction at the inline
/// size, so the sheet and the bar say the same thing the same way.
fn paint_lamp(ui: &Ui, center: egui::Pos2, look: Option<(Color32, LampShape)>) {
    let p = theme::palette_of(ui);
    match look {
        Some((color, LampShape::Filled)) => ui.painter().circle(
            center,
            4.0,
            color,
            Stroke::new(1.0, theme::blend(color, p.text_primary, 0.45)),
        ),
        Some((color, LampShape::Ring)) => {
            ui.painter()
                .circle_stroke(center, 3.25, Stroke::new(1.5, color))
        }
        None => ui
            .painter()
            .circle(center, 4.0, p.surface2, Stroke::new(1.0, p.border)),
    };
}

/// The host's record sheet: the take's state, whether stems are being
/// captured, and the one control that starts or ends a take. Everyone else
/// gets the lamp; only this sheet gets the button.
pub fn record_sheet(
    ui: &mut Ui,
    snap: &Snapshot,
    rt: &dyn Runtime,
    retention: Option<&RetentionNote>,
    open: &mut bool,
) {
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
            // What happens to the take after it lands, when that is not what
            // the host asked for. Beside the line saying what is captured,
            // because both are facts about this take set at launch, and here
            // rather than only in Settings because this is the sheet a host
            // reads before pressing Record.
            if let Some(note) = retention {
                let p = theme::palette_of(ui);
                ui.add_space(theme::SPACE_SM);
                ui.add(
                    egui::Label::new(
                        RichText::new(note.text.clone())
                            .color(note.color(p))
                            .small(),
                    )
                    .wrap(),
                );
            }
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
                lamp_look(state, p),
            );
        }
        // The word takes the lamp's own colour, so the sheet and the bar cannot
        // disagree about what state the recorder is in.
        let word = match state {
            RecordState::Idle => "idle",
            RecordState::Recording => "recording",
            RecordState::Uploading => "uploading",
            RecordState::Failed { .. } => "failed",
        };
        let color = lamp_look(state, p).map_or(p.text_muted, |(color, _)| color);
        // The lamp beside it keeps the palette colour, which is data; the word
        // is text and takes the step of that colour that reads on the sheet.
        ui.label(RichText::new(word).color(theme::readable(color, p.surface1, p)));
        if matches!(state, RecordState::Uploading) {
            // Its own state, said plainly: the take left the session and
            // is neither done nor lost until the lamp goes dark.
            ui.label(theme::muted(ui, "the take is safe once this clears"));
        }
    });
    if let RecordState::Failed { reason } = state {
        // The reason the recorder gave, verbatim, the treatment a dropped
        // stream gets on the destinations sheet.
        theme::reason(ui, reason.clone());
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
