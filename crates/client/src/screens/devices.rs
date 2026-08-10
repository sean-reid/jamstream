//! Audio device setup. Enumeration is fed in through [`DeviceCatalog`]:
//! the production app fills it from the platform backend, the demo and
//! the UI tests list two fake devices per direction.

use egui::{ComboBox, Ui, vec2};

use crate::runtime::{AudioFaultView, LevelsView};
use crate::theme;
use crate::widgets::{Meter, meter, pick_row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    /// Backend device id; `None` means the system default entry.
    pub id: Option<String>,
    /// Buffer size bounds in frames, where the backend reports them. The
    /// buffer picker annotates choices outside them; a device is free to
    /// deliver its own period regardless.
    pub min_buffer_frames: Option<u32>,
    pub max_buffer_frames: Option<u32>,
}

/// The label of the `id: None` entry that heads both pickers. Following the
/// system default is a different act from picking the device that happens to
/// be the default today: when the OS moves the default, the former follows it
/// and the latter stays put.
pub const SYSTEM_DEFAULT: &str = "System default";

fn system_default_entry() -> DeviceInfo {
    DeviceInfo {
        name: SYSTEM_DEFAULT.to_owned(),
        id: None,
        // Which device the default resolves to is the OS's call, so no
        // bounds can honestly be claimed for it.
        min_buffer_frames: None,
        max_buffer_frames: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCatalog {
    pub capture: Vec<DeviceInfo>,
    pub playback: Vec<DeviceInfo>,
}

impl DeviceCatalog {
    pub fn demo() -> Self {
        let dev = |name: &str| DeviceInfo {
            name: name.to_owned(),
            id: Some(format!("demo:{name}")),
            min_buffer_frames: None,
            max_buffer_frames: None,
        };
        DeviceCatalog {
            capture: vec![
                system_default_entry(),
                dev("Scarlett 2i2 input"),
                dev("Built-in microphone"),
            ],
            playback: vec![
                system_default_entry(),
                dev("Scarlett 2i2 output"),
                dev("Built-in speakers"),
            ],
        }
    }

    /// Real enumeration: the System default entry first, then every concrete
    /// device with the current default at the top of them. Index 0 is what a
    /// fresh install uses, and it follows the OS when the default moves.
    pub fn from_backend(devices: &[jamstream_audio_io::DeviceInfo]) -> Self {
        let pick = |direction| {
            let mut rows: Vec<&jamstream_audio_io::DeviceInfo> = devices
                .iter()
                .filter(|d| d.direction == direction)
                .collect();
            rows.sort_by_key(|d| !d.is_default);
            std::iter::once(system_default_entry())
                .chain(rows.into_iter().map(|d| DeviceInfo {
                    name: d.name.clone(),
                    id: Some(d.id.clone()),
                    min_buffer_frames: d.min_buffer_frames,
                    max_buffer_frames: d.max_buffer_frames,
                }))
                .collect()
        };
        DeviceCatalog {
            capture: pick(jamstream_audio_io::Direction::Capture),
            playback: pick(jamstream_audio_io::Direction::Playback),
        }
    }

    /// The index of `id` in `list`, or index 0 (the System default) when the
    /// device is not in this catalog. The bool says whether it was found, so a
    /// rescan that loses the selected device can say so instead of silently
    /// showing another name.
    pub fn find(list: &[DeviceInfo], id: &Option<String>) -> (usize, bool) {
        match list.iter().position(|d| d.id == *id) {
            Some(idx) => (idx, true),
            None => (0, false),
        }
    }
}

/// Frames per buffer at 48 kHz; the protocol runs 2.5 ms media frames.
pub const BUFFER_CHOICES: [u32; 3] = [120, 240, 480];

fn buffer_label(frames: u32) -> String {
    format!("{frames} frames ({:.1} ms)", frames as f32 / 48.0)
}

/// The buffer bounds the selected pair of devices put on a choice: the
/// stricter of the two minimums (either side padding forces the pair up) and
/// the stricter of the two maximums. `None` where neither backend reports
/// one, which is also every System default entry.
pub fn buffer_bounds(
    catalog: &DeviceCatalog,
    capture_idx: usize,
    playback_idx: usize,
) -> (Option<u32>, Option<u32>) {
    let of = |list: &[DeviceInfo], idx: usize| list.get(idx).cloned();
    let devices = [
        of(&catalog.capture, capture_idx),
        of(&catalog.playback, playback_idx),
    ];
    let min = devices
        .iter()
        .flatten()
        .filter_map(|d| d.min_buffer_frames)
        .max();
    let max = devices
        .iter()
        .flatten()
        .filter_map(|d| d.max_buffer_frames)
        .min();
    (min, max)
}

/// The annotation a buffer choice earns against the device's own bounds, or
/// `None` inside them. The choice stays clickable either way: the device
/// negotiates what it really delivers and the ring follows it, so a pick
/// below the minimum costs the minimum, and the row says so instead of
/// showing 2.5 ms while the device runs 10.
pub fn buffer_choice_note(frames: u32, bounds: (Option<u32>, Option<u32>)) -> Option<String> {
    let (min, max) = bounds;
    if let Some(min) = min
        && frames < min
    {
        return Some(format!("device delivers {min} minimum"));
    }
    if let Some(max) = max
        && frames > max
    {
        return Some(format!("device delivers {max} maximum"));
    }
    None
}

pub struct DevicesScreen {
    pub capture_idx: usize,
    pub playback_idx: usize,
    pub buffer_frames: u32,
    /// Whether an open may take the device exclusively (Windows). On by
    /// default: exclusive is the low-latency path the product exists for,
    /// but it mutes every other stream on the endpoint, so the setting and
    /// its cost are on the tab instead of being a silent policy.
    pub allow_exclusive: bool,
    /// Whether the platform's backend can open a device exclusively at all,
    /// which is WASAPI and nothing else. Set from the running platform, and a
    /// field rather than a `cfg!` at the draw site so a snapshot can render
    /// both answers from one machine.
    pub exclusive_offered: bool,
    /// What the last rescan had to say for itself: a selection that fell back
    /// to the system default because its device is gone, or a scan that
    /// failed. Shown under the pickers until the next rescan; a fallback
    /// nobody is told about is the pickers lying about what is running.
    pub rescan_note: Option<String>,
}

impl Default for DevicesScreen {
    fn default() -> Self {
        DevicesScreen {
            capture_idx: 0,
            playback_idx: 0,
            buffer_frames: BUFFER_CHOICES[0],
            allow_exclusive: true,
            exclusive_offered: cfg!(windows),
            rescan_note: None,
        }
    }
}

/// What the stream has to say for itself under the pickers: the refusal
/// reason while there is no stream, the rate disclosures while there is
/// one, what the reopen cadence is doing, whether the device keeps losing the
/// stream, and whether the playout ring is in a crackling run. All of them are
/// consequences of the pick, so they render beside the controls that made it.
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamNotes<'a> {
    pub refusal: Option<&'a str>,
    pub rate_lines: &'a [String],
    /// What the audio stream is doing wrong, while it is: the status bar's
    /// tag says there is no audio, and this is the screen with the pick that
    /// gets it back.
    pub fault: Option<AudioFaultView>,
    /// Stops this device has run up while it reads as cutting out, which is a
    /// different thing to know from a stream that is down right now: this one
    /// says the next one is coming.
    pub cutting_out: Option<u64>,
    /// Whether the playout ring is currently in a crackling run: while it
    /// holds, the fix is right here, so the notice sits beside Buffer size
    /// rather than only in the status bar's tag.
    pub crackling: bool,
}

/// The sentence a fault earns beside the pickers. A cadence still working
/// asks for nothing; a cadence that has stopped asks for the one thing that
/// starts it again, and says how many tries it took so a musician can tell a
/// device that keeps failing from a one-off.
#[must_use]
pub fn fault_line(fault: AudioFaultView) -> String {
    match fault {
        AudioFaultView::Retrying => {
            "No audio: the stream stopped and is being reopened.".to_owned()
        }
        AudioFaultView::GaveUp { tries } => format!(
            "No audio: the device did not stay open after {tries} tries. \
             Pick a device to try again."
        ),
    }
}

/// The sentence a device that keeps losing the stream earns beside the pickers.
/// The count is the fact nothing else on screen can carry: every one of those
/// stops was a gap the band heard, and each was healed before a fault could be
/// drawn. The cause is usually outside this app, so the sentence names where to
/// look before the pick that is right here.
#[must_use]
pub fn cutting_out_line(stops: u64) -> String {
    format!(
        "Cutting out: the audio stream has stopped {stops} times. \
         Check the cable, or pick another device."
    )
}

/// What only exists once you have joined something: the mouth-to-ear figure
/// buffer size is traded against, and whether your own signal is in your
/// personal mix. Both `None` outside a session.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionAudio {
    pub mouth_to_ear_ms: Option<f32>,
    /// Mirrors [`crate::runtime::Snapshot::hear_self`].
    pub hear_self: Option<bool>,
    /// Mirrors [`crate::runtime::Snapshot::offer_hear_self`].
    pub offer_hear_self: bool,
}

/// The offer, above the checkbox it is about, for as long as the session is
/// that far apart. It names headphones itself rather than leaving that to the
/// line under the checkbox, because this is the sentence somebody reads when
/// they were not looking for the control, and hearing this mix on speakers is
/// a loop into the microphone.
pub const HEAR_SELF_OFFER: &str = "The band is far enough apart that keeping time by ear \
     gets hard. On headphones, hearing yourself through the server puts you on the band's \
     timeline instead. On speakers, leave it off.";

/// What the Audio tab asks the app to do; the tab cannot reach the platform
/// backend itself, which is what keeps every fixture off the real sound card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicesEvent {
    /// Re-enumerate the devices: something was plugged in or enabled since
    /// the last look.
    Rescan,
    /// Ask the server to include your own signal in your personal mix, or
    /// drop it back out.
    SetHearSelf(bool),
}

/// How a block is set. On its own screen each one is a panel, sitting on the
/// window surface like every other panel in the app. In the settings drawer
/// they are flat, because the drawer is already the panel and the sheets
/// never put a card inside a card: Destinations and Invites read the same
/// way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    Panel,
    Flat,
}

impl Block {
    fn show(self, ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
        match self {
            Block::Panel => {
                theme::panel(ui).show(ui, |ui| {
                    ui.set_width(ui.available_width().min(560.0));
                    add(ui);
                });
            }
            Block::Flat => add(ui),
        }
    }
}

impl DevicesScreen {
    /// The audio blocks, as the settings drawer's Audio tab.
    ///
    /// These controls live in the drawer's Audio tab and nowhere else, so
    /// they are reachable from every screen rather than only from a snapshot
    /// fixture, which would make the baseline a picture of dead code.
    ///
    /// Buffer size and the input meter come first because they are what a
    /// musician reaches for mid session, by ear and by meter, while hearing
    /// yourself is a call made once and the device pickers are set once too.
    /// Whatever is last is what a short window puts behind a scroll, so the
    /// order is the priority.
    ///
    /// `session` carries the two facts that only exist once you have joined
    /// something. `notes` is what the stream reports about the pick: the
    /// refusal while there is no stream, the rate disclosures while there is
    /// one, and whether the ring is crackling. All of it belongs beside the
    /// controls that made it rather than only over the mixer they cannot see
    /// with this drawer open.
    pub fn audio_ui(
        &mut self,
        ui: &mut Ui,
        block: Block,
        catalog: &DeviceCatalog,
        levels: &LevelsView,
        session: SessionAudio,
        notes: StreamNotes<'_>,
    ) -> Option<DevicesEvent> {
        self.buffer_ui(ui, block, catalog, session.mouth_to_ear_ms, notes.crackling);
        ui.add_space(theme::SPACE_MD);
        input_level_ui(ui, block, levels);
        let hear_self_event = session.hear_self.and_then(|on| {
            ui.add_space(theme::SPACE_MD);
            hear_self_ui(ui, block, on, session.offer_hear_self)
        });
        ui.add_space(theme::SPACE_MD);
        let devices_event = self.devices_ui(ui, block, catalog, notes);
        hear_self_event
            .map(DevicesEvent::SetHearSelf)
            .or(devices_event)
    }

    fn buffer_ui(
        &mut self,
        ui: &mut Ui,
        block: Block,
        catalog: &DeviceCatalog,
        mouth_to_ear_ms: Option<f32>,
        crackling: bool,
    ) {
        let bounds = buffer_bounds(catalog, self.capture_idx, self.playback_idx);
        block.show(ui, |ui| {
            ui.label(theme::title(ui, "Buffer size"));
            ui.label(theme::muted(
                ui,
                "Smaller buffers cut latency; pick the smallest that stays clean.",
            ));
            ui.add_space(theme::SPACE_XS);
            for frames in BUFFER_CHOICES {
                let label = buffer_label(frames);
                let selected = self.buffer_frames == frames;
                let note = buffer_choice_note(frames, bounds);
                let response = pick_row(ui, &label, selected, true, |ui| {
                    ui.label(label.clone());
                    if let Some(note) = &note {
                        ui.label(theme::muted(ui, note.clone()).small());
                    }
                });
                if response.clicked() {
                    self.buffer_frames = frames;
                }
            }
            // Beside the choices it names, for as long as the run holds: the
            // status bar's tag says a fault exists, and this is the screen
            // somebody opens to act on it.
            if crackling {
                ui.add_space(theme::SPACE_SM);
                theme::reason(
                    ui,
                    "Crackling: this device is not keeping up. Try the next size up.",
                );
            }
            // The number the choice is being traded against. The capture
            // buffer is one of the four terms in mouth to ear, so this moves
            // with the pick, and the same figure in the same monospace is in
            // the status bar. Outside a session nothing has been measured,
            // and a placeholder would be an instrument reading a made-up
            // number, so the row is absent instead.
            if let Some(ms) = mouth_to_ear_ms {
                ui.add_space(theme::SPACE_SM);
                ui.horizontal(|ui| {
                    ui.label(theme::mono(ui, format!("{ms:.1} ms")));
                    ui.label(theme::muted(ui, "mouth to ear"));
                });
            }
        });
    }

    fn devices_ui(
        &mut self,
        ui: &mut Ui,
        block: Block,
        catalog: &DeviceCatalog,
        notes: StreamNotes<'_>,
    ) -> Option<DevicesEvent> {
        let mut event = None;
        block.show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::title(ui, "Devices"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("Rescan")
                        .on_hover_text(
                            "look for devices again; an interface plugged in after \
                             launch appears here",
                        )
                        .clicked()
                    {
                        event = Some(DevicesEvent::Rescan);
                    }
                });
            });
            // The pickers take the width that is left rather than a fixed
            // 280 px. A row wider than its container widens the sheet around
            // it, which is how the settings sheet came to be half again as
            // wide as the width it asks for.
            let combo_w =
                (ui.available_width() - DEVICE_LABEL_W - theme::SPACE_LG).clamp(140.0, 280.0);
            egui::Grid::new("device-grid")
                .num_columns(2)
                .min_col_width(DEVICE_LABEL_W)
                .spacing(vec2(theme::SPACE_LG, 6.0))
                .show(ui, |ui| {
                    ui.label(theme::muted(ui, "Capture"));
                    device_combo(
                        ui,
                        "capture",
                        &catalog.capture,
                        &mut self.capture_idx,
                        combo_w,
                    );
                    ui.end_row();
                    ui.label(theme::muted(ui, "Playback"));
                    device_combo(
                        ui,
                        "playback",
                        &catalog.playback,
                        &mut self.playback_idx,
                        combo_w,
                    );
                    ui.end_row();
                });
            // The WASAPI backend is the only reader of `allow_exclusive`; on
            // every other platform the question has no answer to give, so
            // the control is absent rather than a checkbox nothing reads.
            if self.exclusive_offered {
                ui.add_space(theme::SPACE_SM);
                ui.checkbox(
                    &mut self.allow_exclusive,
                    "Allow exclusive access (lowest latency)",
                );
                ui.label(
                    theme::muted(
                        ui,
                        "Exclusive mutes other apps on the device while a session runs. \
                         Off shares it with them and adds 10 to 20 ms.",
                    )
                    .small(),
                );
            }
            // How the running stream reaches 48 kHz, when that is anything
            // other than the device's own clock: the consequence of the pick
            // belongs beside the pickers that made it. Muted, not a warning;
            // the stream is working.
            for line in notes.rate_lines {
                ui.add_space(theme::SPACE_XS);
                ui.label(theme::muted(ui, line.clone()).small());
            }
            // What the last rescan did to the selection, before the refusal:
            // a device that vanished is why the picker reads System default
            // now, and saying so is the difference between a fallback and a
            // picker that quietly changed its answer.
            if let Some(note) = &self.rescan_note {
                ui.add_space(theme::SPACE_XS);
                theme::reason(ui, note.clone());
            }
            // The pick that will not open, in the device's own words, under the
            // picker that made it. No sentence of ours around it: the pickers
            // are right above it, so what it is about is not in question.
            if let Some(reason) = notes.refusal {
                ui.add_space(theme::SPACE_XS);
                theme::reason(ui, reason);
            }
            // Under the refusal, because a device that gave a reason has
            // already given it: what is left to say is whether anything is
            // still being tried, and the pick that ends the wait is here.
            // Opens with the word the status bar's tag carries, so the tag
            // and the sentence read as one thing.
            if let Some(fault) = notes.fault {
                ui.add_space(theme::SPACE_XS);
                theme::reason(ui, fault_line(fault));
            }
            // Last, because it is the only one of these that outlives the
            // moment: the stream may be running as this is read, and what it
            // says is that it will not keep running.
            if let Some(stops) = notes.cutting_out {
                ui.add_space(theme::SPACE_XS);
                theme::reason(ui, cutting_out_line(stops));
            }
        });
        event
    }
}

/// The label column in the device grid: "Capture" and "Playback".
const DEVICE_LABEL_W: f32 = 66.0;

/// The checkbox that asks the server to fold your own signal into your
/// personal mix. `Some` only when it changed this frame, carrying the value
/// to send.
fn hear_self_ui(ui: &mut Ui, block: Block, hear_self: bool, offer: bool) -> Option<bool> {
    let mut enabled = hear_self;
    let mut changed = false;
    block.show(ui, |ui| {
        // Above the control, in the primary ink: the two lines below are
        // standing facts about the control and are muted for it, and this is
        // not one of those, nor is it the danger ink, which is for faults.
        if offer {
            ui.label(HEAR_SELF_OFFER);
            ui.add_space(theme::SPACE_XS);
        }
        changed = ui
            .checkbox(&mut enabled, "Hear yourself through the server")
            .changed();
        // The requirement sits against the checkbox and the reason for wanting
        // it comes after. Nothing asks whether these are headphones, so this
        // line is the only warning there is, and the line somebody reads is the
        // one nearest the control they just touched.
        ui.label(
            theme::muted(
                ui,
                "Needs headphones: playing this mix through speakers loops your \
                 own signal straight back into the microphone.",
            )
            .small(),
        );
        ui.label(
            theme::muted(
                ui,
                "Your own sound joins the mix with everyone else's, so the gap you \
                 hear becomes the difference between two uplinks instead of the \
                 whole network path.",
            )
            .small(),
        );
    });
    changed.then_some(enabled)
}

fn input_level_ui(ui: &mut Ui, block: Block, levels: &LevelsView) {
    block.show(ui, |ui| {
        ui.label(theme::title(ui, "Input level"));
        // The meter spans the full width and its line sits under it. Side by
        // side, the two needed more width than the drawer has.
        let w = ui.available_width();
        meter(
            ui,
            "devices-input",
            levels.input_peak,
            levels.input_rms,
            vec2(w, 12.0),
            Meter::Horizontal,
        );
        ui.label(theme::muted(ui, "speak or play to check the meter moves"));
    });
}

fn device_combo(ui: &mut Ui, id: &str, devices: &[DeviceInfo], selected: &mut usize, width: f32) {
    if devices.is_empty() {
        ui.label(theme::muted(ui, "no devices found"));
        return;
    }
    *selected = (*selected).min(devices.len() - 1);
    ComboBox::from_id_salt(id)
        .width(width)
        .selected_text(devices[*selected].name.clone())
        .show_ui(ui, |ui| {
            for (i, dev) in devices.iter().enumerate() {
                ui.selectable_value(selected, i, dev.name.clone());
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(min: Option<u32>, max: Option<u32>) -> DeviceInfo {
        DeviceInfo {
            name: "dev".to_owned(),
            id: Some("dev".to_owned()),
            min_buffer_frames: min,
            max_buffer_frames: max,
        }
    }

    /// The pair's bounds are the stricter of each side's, because a stream
    /// runs both directions at once and either one padding forces the pair.
    #[test]
    fn the_pairs_bounds_are_the_stricter_of_the_two_sides() {
        let catalog = DeviceCatalog {
            capture: vec![dev(Some(480), Some(4800))],
            playback: vec![dev(Some(120), Some(960))],
        };
        assert_eq!(buffer_bounds(&catalog, 0, 0), (Some(480), Some(960)));
        // A side with no report leaves the other side's bound in charge.
        let catalog = DeviceCatalog {
            capture: vec![dev(None, None)],
            playback: vec![dev(Some(240), None)],
        };
        assert_eq!(buffer_bounds(&catalog, 0, 0), (Some(240), None));
        // Nothing reported is nothing claimed, which is the demo catalog and
        // every System default entry.
        assert_eq!(buffer_bounds(&DeviceCatalog::demo(), 0, 0), (None, None));
    }

    /// The buffer-bounds annotation: "120 frames (2.5 ms)" picked on a
    /// device whose period is 480 gets an annotation naming the 480 it will
    /// really get, and choices inside the bounds stay unannotated.
    #[test]
    fn choices_outside_the_devices_bounds_say_what_the_device_delivers() {
        let bounds = (Some(480), Some(4800));
        assert_eq!(
            buffer_choice_note(120, bounds).as_deref(),
            Some("device delivers 480 minimum")
        );
        assert_eq!(buffer_choice_note(480, bounds), None);
        assert_eq!(
            buffer_choice_note(9600, (None, Some(4800))).as_deref(),
            Some("device delivers 4800 maximum")
        );
        assert_eq!(buffer_choice_note(120, (None, None)), None);
    }

    /// The platform decides whether the exclusive question gets asked, and a
    /// launch never decides otherwise: only a fixture rendering the other
    /// platform's layout sets this by hand.
    #[test]
    fn the_exclusive_control_is_offered_where_a_backend_can_answer_for_it() {
        assert_eq!(DevicesScreen::default().exclusive_offered, cfg!(windows));
    }
}
