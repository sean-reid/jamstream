//! Audio device setup. Enumeration is fed in through [`DeviceCatalog`]:
//! the production app fills it from the platform backend, the demo and
//! the UI tests list two fake devices per direction.

use egui::{ComboBox, Ui, vec2};

use crate::runtime::{AudioFaultView, CushionView, LevelsView};
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
    /// Whether the playout ring may hold more than the buffer size asks for. On
    /// by default, because the machines that need the help cannot be told apart
    /// from the ones that do not until the ring runs low. Off pins the depth, so
    /// the pick is the latency exactly.
    pub auto_cushion: bool,
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
            auto_cushion: true,
            exclusive_offered: cfg!(windows),
            rescan_note: None,
        }
    }
}

/// What the stream has to say for itself under the pickers: the refusal
/// reason while there is no stream, the rate disclosures while there is
/// one, what the reopen cadence is doing, whether the device keeps losing the
/// stream, whether the playout ring is in a crackling run, and what depth the
/// cushion is holding. All of them are consequences of the pick, so they render
/// beside the controls that made it.
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
    /// What the playout cushion is holding under the buffer size, and whether
    /// anything is holding it there. `None` while there is no stream, which is
    /// also when nobody is paying for a cushion.
    pub cushion: Option<CushionView>,
}

/// The one line the buffer choices get about the depth under them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferNote {
    pub text: String,
    /// Whether it wants a pick: the danger ink for the state somebody has to
    /// act on, the muted step for a depth doing its job unaided.
    pub asks: bool,
}

/// The notice a crackling run earns beside the choices. It asks for no size of
/// its own: depth is already being added, and the only reopen worth a musician
/// mid-song is the one [`buffer_note`] asks for once that is out of room.
pub const CRACKLING_NOTICE: &str = "Crackling: this device is not keeping up. Extra depth is \
     going in to cover it, and asks for a bigger size here if that is not enough.";

/// The single sentence under the buffer choices, and nothing stacked under it.
///
/// Nothing at all is the common case, and the one this block is tightest for: a
/// machine holding what its size asks for has nothing to report that the size on
/// the row above and the figure below do not already say.
///
/// A pinned depth the ring keeps outrunning speaks first, because the remedy is
/// the box directly above and costs nothing, while every other remedy on this
/// tab costs a device reopen. Then a depth out of room, the only one of the rest
/// that asks for something and the one whose reopen ends the crackling it would
/// otherwise sit beside. Then a crackling run, which is the same story told from
/// what a musician can hear. Then the depth saying what it is holding past the
/// pick, which is latency nobody chose.
#[must_use]
pub fn buffer_note(cushion: Option<CushionView>, crackling: bool) -> Option<BufferNote> {
    match (cushion, crackling) {
        (Some(cushion), dry) if !cushion.auto && (cushion.out_of_room || dry) => {
            Some(pinned_note())
        }
        (Some(cushion), _) if cushion.out_of_room => Some(out_of_room_note(cushion)),
        (_, true) => Some(BufferNote {
            text: CRACKLING_NOTICE.to_owned(),
            asks: true,
        }),
        (Some(cushion), false) if cushion.deepened() => Some(deepened_note(cushion)),
        _ => None,
    }
}

/// What a pinned depth says once the ring keeps outrunning it: what is happening,
/// and the one remedy that costs nobody a reopen.
///
/// It names no buffer size, because the offer to pay for a longer device callback
/// belongs after a depth that was allowed to grow has run out of room, and this
/// one was never allowed to grow. Somebody who ticks the box and still runs close
/// at the ceiling gets that offer next.
fn pinned_note() -> BufferNote {
    BufferNote {
        text: "Still coming close to breaking up at what this size asks for. Adding \
               depth automatically is what would cover it."
            .to_owned(),
        asks: true,
    }
}

/// What extra depth says while it is holding some: how much, over what, and why
/// it went in. In milliseconds over frames because both are already on the rows
/// above, and it never names a second kind of buffer: the reading is the pick
/// plus this, and a musician who wants the total has the figure underneath.
fn deepened_note(cushion: CushionView) -> BufferNote {
    BufferNote {
        text: format!(
            "Holding {:.1} ms more than {} frames asks for: the audio kept coming close \
             to breaking up.",
            cushion.extra_ms(),
            cushion.callback_frames
        ),
        asks: false,
    }
}

/// The one state that asks for something: no depth this size can hold stops the
/// ring running close, so the answer is a longer device callback. It names the
/// next size up and what taking it costs, because the reopen is a hole in what
/// the band hears and never somebody else's call to make.
fn out_of_room_note(cushion: CushionView) -> BufferNote {
    let deepest = "Still coming close to breaking up at the most this size can hold.";
    BufferNote {
        text: match next_buffer_up(cushion.callback_frames) {
            Some(frames) => format!(
                "{deepest} {} is the next size up: more latency, and the reopen costs \
                 the band a few hundred milliseconds of you.",
                buffer_label(frames)
            ),
            None => format!(
                "{deepest} There is no larger size to move to, so this machine cannot \
                 feed this device on time."
            ),
        },
        asks: true,
    }
}

/// The buffer choice above `frames`, or `None` at the top of the ladder.
/// Measured against the callback the device is delivering rather than the pick,
/// so a device already delivering longer than it was asked for is never offered
/// a size it is past.
fn next_buffer_up(frames: usize) -> Option<u32> {
    BUFFER_CHOICES
        .into_iter()
        .find(|choice| *choice as usize > frames)
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
        self.buffer_ui(ui, block, catalog, session.mouth_to_ear_ms, notes);
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
        notes: StreamNotes<'_>,
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
            // Under the choices, because what it adds is added on top of one of
            // them. The size itself stays pickable whatever this says: nothing
            // in the app ever moves it, and the sentence at the ceiling asks for
            // a bigger one.
            ui.add_space(theme::SPACE_SM);
            ui.checkbox(&mut self.auto_cushion, "Add extra depth automatically");
            // Beside the choices they are about, for as long as each holds: the
            // status bar's tag says a fault exists, and this is the screen
            // somebody opens to act on it. One sentence at a time, because the
            // depth being held, a run somebody can hear, and the offer of a
            // longer callback are one story at three moments, and three
            // sentences of it under one control is worse than any of them.
            if let Some(note) = buffer_note(notes.cushion, notes.crackling) {
                ui.add_space(theme::SPACE_SM);
                if note.asks {
                    theme::reason(ui, note.text);
                } else {
                    ui.label(theme::muted(ui, note.text));
                }
            }
            // The number the choice is being traded against. This pick sets
            // two of the terms in mouth to ear, the capture buffer and the
            // playout depth, so the figure moves with the pick, and the same
            // figure in the same monospace is in the status bar.
            // Outside a session nothing has been measured,
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

    fn cushion(held: usize, callback: usize, out_of_room: bool) -> CushionView {
        CushionView {
            held_frames: held,
            base_frames: 2 * callback,
            callback_frames: callback,
            out_of_room,
            auto: true,
        }
    }

    /// The same depth with the box unticked, which is the only way it can hold
    /// the base cushion and be out of room at once.
    fn pinned_cushion(callback: usize, out_of_room: bool) -> CushionView {
        CushionView {
            auto: false,
            ..cushion(2 * callback, callback, out_of_room)
        }
    }

    /// One sentence under the choices, whatever is true at once. A depth out of
    /// room speaks over a crackling run, because the pick it asks for is what
    /// ends that run; the run speaks over a depth report, which is the same
    /// story before anybody could hear it.
    #[test]
    fn the_buffer_control_says_one_of_these_things_and_never_two() {
        for crackling in [true, false] {
            let note = buffer_note(Some(cushion(480, 120, true)), crackling)
                .expect("a depth out of room has to say so");
            assert!(note.asks, "the one state that wants a pick asks for it");
            assert!(
                note.text
                    .contains("240 frames (5.0 ms) is the next size up"),
                "{}",
                note.text
            );
            assert!(
                !note.text.contains("Crackling"),
                "two sentences at once: {}",
                note.text
            );
        }
        // A run somebody can hear, over a depth still working on it.
        for depth in [
            Some(cushion(360, 120, false)),
            Some(cushion(240, 120, false)),
            None,
        ] {
            assert_eq!(
                buffer_note(depth, true).map(|n| n.text).as_deref(),
                Some(CRACKLING_NOTICE)
            );
        }
        // And no stream is nothing held and nothing to say about it.
        assert!(buffer_note(None, false).is_none());
    }

    /// A machine holding what its size asks for says nothing at all, ticked or
    /// pinned: the size is on the row above and the latency is under it, and a
    /// second noun for held audio beside a buffer size is what made somebody ask
    /// whether the size was automatic too.
    ///
    /// A depth past the pick says how much more and why, in the units already on
    /// the rows: milliseconds over frames, and never a figure for the total,
    /// which the latency row under it carries.
    #[test]
    fn nothing_is_said_until_the_depth_leaves_the_pick_behind() {
        for at_rest in [cushion(240, 120, false), pinned_cushion(120, false)] {
            assert_eq!(
                buffer_note(Some(at_rest), false),
                None,
                "{at_rest:?} is the pick and nothing else, so there is nothing to say"
            );
        }
        let deeper = buffer_note(Some(cushion(360, 120, false)), false).expect("a depth to report");
        assert!(!deeper.asks, "a depth doing its job is not a fault");
        assert_eq!(
            deeper.text,
            "Holding 2.5 ms more than 120 frames asks for: the audio kept coming close \
             to breaking up."
        );
        // The size named is the one on the row, and the figure is what was added
        // rather than the total: 480 samples over a 240-frame callback is 5 ms on
        // top of the 10 the size asks for.
        let bigger = buffer_note(Some(cushion(720, 240, false)), false).expect("a depth to report");
        assert_eq!(
            bigger.text,
            "Holding 5.0 ms more than 240 frames asks for: the audio kept coming close \
             to breaking up."
        );
    }

    /// A pinned depth the ring keeps outrunning, from a reading and from a run
    /// somebody can hear. Both say the same thing, because the remedy is the
    /// same: the box above, which costs nothing. Neither names a buffer size,
    /// because paying for a device reopen is the answer after a depth that was
    /// allowed to grow has run out of room, and this one never grew.
    #[test]
    fn a_pinned_depth_the_ring_outruns_asks_for_the_box_and_not_for_a_reopen() {
        for (what, crackling, out_of_room) in [
            ("a reading", false, true),
            ("a run somebody can hear", true, false),
            ("both at once", true, true),
        ] {
            let note = buffer_note(Some(pinned_cushion(120, out_of_room)), crackling)
                .unwrap_or_else(|| panic!("{what} under a pinned depth has to say something"));
            assert!(note.asks, "{what}: this is a state that needs somebody");
            assert_eq!(
                note.text,
                "Still coming close to breaking up at what this size asks for. Adding \
                 depth automatically is what would cover it.",
                "{what}"
            );
            assert!(
                !note.text.contains("frames"),
                "{what}: no size may be named where no reopen is being asked for: {}",
                note.text
            );
        }
        // A depth that was allowed to grow keeps the offer it had: the pinned
        // sentence is a state of its own rather than a replacement.
        let offered = buffer_note(Some(cushion(480, 120, true)), true).expect("the offer");
        assert!(
            offered
                .text
                .contains("240 frames (5.0 ms) is the next size up"),
            "{}",
            offered.text
        );
    }

    /// The size the offer names is the next one above the callback the device is
    /// delivering, not above the pick: a device already handing over 480 frames
    /// has nowhere left to go, and offering a size it is past would be a reopen
    /// that changes nothing.
    #[test]
    fn the_offer_names_the_next_size_above_what_the_device_delivers() {
        let note = buffer_note(Some(cushion(960, 240, true)), false).expect("an offer");
        assert!(note.text.contains("480 frames (10.0 ms)"), "{}", note.text);
        let top = buffer_note(Some(cushion(1920, 480, true)), false).expect("an offer");
        assert!(top.asks, "the top of the ladder still wants somebody");
        assert!(
            top.text.contains("no larger size to move to"),
            "{}",
            top.text
        );
        assert!(
            !top.text.contains("frames ("),
            "no size to name, so none may be named: {}",
            top.text
        );
    }

    /// The platform decides whether the exclusive question gets asked, and a
    /// launch never decides otherwise: only a fixture rendering the other
    /// platform's layout sets this by hand.
    #[test]
    fn the_exclusive_control_is_offered_where_a_backend_can_answer_for_it() {
        assert_eq!(DevicesScreen::default().exclusive_offered, cfg!(windows));
    }
}
