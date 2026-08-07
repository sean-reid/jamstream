//! The host's destinations sheet: which platforms this session streams to,
//! what each one is doing, and the control that puts the room on air.
//!
//! Stream keys follow the wizard's credential pattern one step stricter: the
//! field is masked with no reveal, because a host is a keystroke away from
//! being on camera and a key on a shared screen is worse than a typo. The
//! character count stands in for reading it back. A sent key is wiped from
//! the input; what is left lives in the keychain and the server's memory.

use std::sync::Arc;

use egui::{Button, RichText, TextEdit, Ui};
use jamstream_stream::PlatformCatalog;
use zeroize::Zeroize;

use jamstream_protocol::control::MAX_STREAM_KEY_LEN;

use crate::creds::{CredStore, stream_key_field};
use crate::runtime::{
    Command, DestinationId, DestinationState, DestinationView, Runtime, Snapshot, StreamKey,
    StreamPlatform,
};
use crate::theme;
use crate::widgets::{lamp, row_cell};

/// Column widths. Every row is laid out on these, so nothing moves as a
/// stream comes up. Neither the bitrate nor the dropped count is one of them:
/// one encode feeds every destination, so both are lines under the rows
/// instead of the same number once per row.
const NAME_W: f32 = 132.0;
const STATE_W: f32 = 80.0;

/// A lost frame is video the broadcast will never have, so any of them is worth
/// noticing; 30 is a second of it at the pipeline's 30 fps, which earns the
/// meter's red.
const DROPPED_AMBER: u64 = 1;
const DROPPED_RED: u64 = 30;

/// A repeat costs nobody a picture, so one of them is not news: a machine that
/// misses a single draw deadline in an hour is fine. A second of stutter is
/// worth amber, ten seconds of it the red.
const REPEATED_AMBER: u64 = 30;
const REPEATED_RED: u64 = 300;

/// Why a key cannot be sent, if it cannot. The control plane refuses these
/// silently, so they are caught here where there is somewhere to say so.
fn refusal(key: &str) -> Option<String> {
    if key.is_empty() {
        return Some("paste the stream key first".to_owned());
    }
    if key.len() > MAX_STREAM_KEY_LEN {
        return Some(format!(
            "that key is {} characters; no platform uses one past {MAX_STREAM_KEY_LEN}",
            key.len()
        ));
    }
    None
}

/// The sentence a session with no relay leads with. A colon, because the
/// server's reason is a clause rather than a sentence and reads as one word
/// after a full stop; the stop is added only if the reason lacks one, since it
/// can be a line cloud-init wrote rather than one of ours.
fn cannot_stream_line(reason: &str) -> String {
    let tail = if reason.ends_with(['.', '!', '?']) {
        ""
    } else {
        "."
    };
    format!("This session cannot stream: {reason}{tail}")
}

/// Where each platform's key actually lives, for the button that opens it.
/// The path through the interface is in the catalog's guidance text; this is
/// just the front door.
fn key_page(platform: StreamPlatform) -> (&'static str, &'static str) {
    match platform {
        StreamPlatform::Twitch => (
            "Open the Twitch stream settings",
            "https://dashboard.twitch.tv/settings/stream",
        ),
        StreamPlatform::YouTube => ("Open YouTube Studio", "https://studio.youtube.com/"),
    }
}

/// One platform's row. The destination's actual state comes from the
/// snapshot, never from here: the pipeline is the only thing that knows
/// whether a stream is up.
struct PlatformRow {
    platform: StreamPlatform,
    display_name: String,
    guidance: String,
    /// A key is in this computer's keychain. The value itself is read only
    /// inside the send, never into this struct.
    saved: bool,
    /// The key pane is open for this row.
    entering: bool,
    /// Typed or pasted key, masked on screen, wiped the moment it is sent or
    /// the pane is closed.
    key_input: String,
    /// Keep the key in this computer's keychain after sending it.
    remember: bool,
    /// The id this panel minted for this platform, once it has asked the
    /// server for it. Kept so remove names the same destination.
    id: Option<DestinationId>,
}

impl Drop for PlatformRow {
    fn drop(&mut self) {
        self.key_input.zeroize();
    }
}

impl PlatformRow {
    fn close_entry(&mut self) {
        self.entering = false;
        self.key_input.zeroize();
        self.key_input.clear();
    }
}

/// What one row is doing, resolved against the current snapshot.
enum RowState<'a> {
    /// No key anywhere and nothing configured.
    NoKey,
    /// A key is on this computer, so one click configures the destination.
    KeySaved,
    /// The server has been asked and has not reported back yet.
    Waiting,
    /// The server's own view of this destination.
    Reported(&'a DestinationView),
}

pub struct DestinationsPanel {
    rows: Vec<PlatformRow>,
    creds: Arc<dyn CredStore>,
    /// Ids are minted here and never reused inside a session, so a status
    /// for a destination that was removed can never be read as its
    /// replacement.
    next_id: u16,
    /// The encode every destination shares, spelled out once.
    encode: String,
    error: Option<String>,
}

impl DestinationsPanel {
    pub fn new(creds: Arc<dyn CredStore>) -> DestinationsPanel {
        let catalog = PlatformCatalog::bundled();
        let rows = [StreamPlatform::Twitch, StreamPlatform::YouTube]
            .into_iter()
            .filter_map(|platform| {
                let spec = catalog.get(platform)?;
                Some(PlatformRow {
                    platform,
                    display_name: spec.display_name.clone(),
                    guidance: spec.key_acquisition.clone(),
                    saved: creds
                        .get(stream_key_field(platform).0, stream_key_field(platform).1)
                        .is_some(),
                    entering: false,
                    key_input: String::new(),
                    remember: true,
                    id: None,
                })
            })
            .collect();
        let (video, audio) = (catalog.video(), catalog.audio());
        DestinationsPanel {
            rows,
            creds,
            next_id: 0,
            encode: format!(
                "One {}x{} {} fps encode at {} kbps feeds every destination.",
                video.width,
                video.height,
                video.fps,
                video.kbps + audio.kbps
            ),
            error: None,
        }
    }

    /// The sheet with one platform's key pane already open and `key` in its
    /// field. The app has no path to this: it exists so a snapshot can hold
    /// the one surface where a key is present at all, and prove it is masked.
    #[doc(hidden)]
    pub fn with_key_entry(
        creds: Arc<dyn CredStore>,
        platform: StreamPlatform,
        key: &str,
    ) -> DestinationsPanel {
        let mut panel = DestinationsPanel::new(creds);
        if let Some(row) = panel.rows.iter_mut().find(|r| r.platform == platform) {
            row.entering = true;
            row.key_input = key.to_owned();
        }
        panel
    }

    fn mint_id(&mut self) -> DestinationId {
        let id = DestinationId(self.next_id);
        self.next_id += 1;
        id
    }

    fn row_state<'a>(&self, index: usize, snap: &'a Snapshot) -> RowState<'a> {
        let row = &self.rows[index];
        if let Some(view) = snap.stream.of_platform(row.platform) {
            return RowState::Reported(view);
        }
        match (row.id, row.saved) {
            (Some(_), _) => RowState::Waiting,
            (None, true) => RowState::KeySaved,
            (None, false) => RowState::NoKey,
        }
    }

    /// Sends the key for `index` to the server and forgets it here. The
    /// caller has already decided the key is worth sending; this is the only
    /// function in the app that moves one.
    fn configure(&mut self, index: usize, key: StreamKey, rt: &dyn Runtime) {
        if let Some(reason) = refusal(key.expose()) {
            self.error = Some(reason);
            return;
        }
        let id = self.mint_id();
        let row = &mut self.rows[index];
        row.id = Some(id);
        row.close_entry();
        self.error = None;
        rt.send(Command::AddDestination {
            id,
            platform: self.rows[index].platform,
            key,
        });
    }

    /// The key pane's Save: send it, and store it for next time if asked.
    fn save_typed_key(&mut self, index: usize, rt: &dyn Runtime) {
        let typed = self.rows[index].key_input.trim().to_owned();
        // Checked before the keychain write, so a key the wire would refuse
        // is not left saved on this computer.
        if let Some(reason) = refusal(&typed) {
            self.error = Some(reason);
            return;
        }
        if self.rows[index].remember {
            let field = stream_key_field(self.rows[index].platform);
            match self.creds.set(field.0, field.1, &typed) {
                Ok(()) => self.rows[index].saved = true,
                // The key still works for this session; only the keychain
                // failed, and saying so beats a silent loss.
                Err(err) => {
                    self.error = Some(format!(
                        "the key works for this session but saving it on this computer failed: {err}"
                    ));
                }
            }
        }
        self.configure(index, StreamKey::new(typed), rt);
    }

    /// The saved-key path: read from the keychain straight into the send, so
    /// the plaintext never lands in this panel's state.
    fn use_saved_key(&mut self, index: usize, rt: &dyn Runtime) {
        let field = stream_key_field(self.rows[index].platform);
        match self.creds.get(field.0, field.1) {
            Some(key) => self.configure(index, StreamKey::new(key), rt),
            None => {
                self.rows[index].saved = false;
                self.rows[index].entering = true;
                self.error = Some(
                    "the saved key is gone from this computer's keychain; paste it again"
                        .to_owned(),
                );
            }
        }
    }

    fn forget_key(&mut self, index: usize) {
        let field = stream_key_field(self.rows[index].platform);
        self.creds.delete(field.0, field.1);
        self.rows[index].saved = false;
        self.error = None;
    }

    fn remove(&mut self, index: usize, id: DestinationId, rt: &dyn Runtime) {
        self.rows[index].id = None;
        self.error = None;
        rt.send(Command::RemoveDestination(id));
    }

    /// True while a key pane is open, which is a state Escape has to leave
    /// before it leaves the drawer the pane is in.
    pub fn entering_key(&self) -> bool {
        self.rows.iter().any(|r| r.entering)
    }

    /// Closes any open key pane, wiping what was typed, and says whether
    /// there was one. Escape reaches this; so does Cancel, one row at a time.
    pub fn close_key_entry(&mut self) -> bool {
        let mut closed = false;
        for row in &mut self.rows {
            if row.entering {
                row.close_entry();
                closed = true;
            }
        }
        if closed {
            self.error = None;
        }
        closed
    }

    /// Destinations the server knows about or has been asked about. Going
    /// live with none configured does nothing, so the button says so.
    fn configured_count(&self, snap: &Snapshot) -> usize {
        (0..self.rows.len())
            .filter(|i| {
                matches!(
                    self.row_state(*i, snap),
                    RowState::Waiting | RowState::Reported(_)
                )
            })
            .count()
    }
}

// Rendering: the same sheet treatment as invites and the stream mix, so the
// host sheets are one thing in several states.

impl DestinationsPanel {
    /// The destinations section of the Broadcast tab. Laid out in the
    /// drawer's width rather than a sheet's: every row stacks its actions
    /// under the name instead of beside it, so nothing here needs a wider
    /// column than the drawer has.
    pub fn ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime) {
        ui.label(theme::title(ui, "Destinations"));
        ui.label(
            theme::muted(
                ui,
                "Where this session streams. Keys are never shown and never \
                 written to the server's disk.",
            )
            .small(),
        );
        // A session whose relay never came up cannot stream anywhere, whatever
        // key is pasted, so it says so here rather than letting a host paste
        // one and find out from a failed destination (#440). The rows stay
        // visible: a saved key is still worth seeing, and Forget key still has
        // to work.
        if let Some(reason) = snap.stream.unavailable_reason() {
            ui.add_space(theme::SPACE_XS);
            theme::reason(ui, cannot_stream_line(reason));
            // No advice here on purpose. A cloud session is fixed by starting
            // another one and a local session by installing the tooling, and
            // this side cannot tell which it is looking at; the streaming
            // guide can say both.
            ui.label(
                theme::muted(
                    ui,
                    "The relay runs on the session's own machine, so nothing on \
                     this computer changes it.",
                )
                .small(),
            );
        }
        ui.add_space(theme::SPACE_SM);
        for index in 0..self.rows.len() {
            // A gap between platforms wider than the gap inside one, so the
            // two lines of a row read as one thing rather than four lines.
            if index > 0 {
                ui.add_space(theme::SPACE_LG);
            }
            self.row_ui(ui, index, snap, rt);
        }
        ui.add_space(theme::SPACE_SM);
        ui.label(theme::muted(ui, self.encode.clone()).small());
        frame_counts(ui, snap);
        if let Some(err) = self.error.clone() {
            ui.add_space(theme::SPACE_XS);
            theme::reason(ui, err);
        }
        ui.add_space(theme::SPACE_MD);
        ui.separator();
        self.go_live_ui(ui, snap, rt);
    }

    /// One platform, stacked to fit the drawer: the lamp, the name, and what
    /// it is doing on the first line, then the numbers and the actions on the
    /// second. Both lines keep fixed cells, so nothing moves as a stream
    /// comes up.
    fn row_ui(&mut self, ui: &mut Ui, index: usize, snap: &Snapshot, rt: &dyn Runtime) {
        let p = theme::palette_of(ui);
        let state = self.row_state(index, snap);
        let live = matches!(
            state,
            RowState::Reported(DestinationView {
                state: DestinationState::Live,
                ..
            })
        );
        let (word, color) = match &state {
            RowState::NoKey => ("no key", p.text_muted),
            RowState::KeySaved => ("key saved", p.text_muted),
            RowState::Waiting => ("asking", p.text_muted),
            RowState::Reported(view) => match &view.state {
                DestinationState::Idle => ("ready", p.text_primary),
                DestinationState::Connecting => ("connecting", p.text_muted),
                DestinationState::Live => ("live", p.accent),
                DestinationState::Failed { .. } => ("failed", p.danger),
            },
        };
        let reason = match &state {
            RowState::Reported(DestinationView {
                state: DestinationState::Failed { reason },
                ..
            }) => Some(reason.clone()),
            _ => None,
        };
        let configured_id = match &state {
            RowState::Reported(view) => Some(view.id),
            RowState::Waiting => self.rows[index].id,
            _ => None,
        };

        let mut action = None;
        ui.horizontal(|ui| {
            row_cell(ui, NAME_W, |ui| {
                lamp(ui, live).on_hover_text(if live {
                    "this destination is on air"
                } else {
                    "this destination is not on air"
                });
                ui.label(RichText::new(self.rows[index].display_name.clone()).strong());
            });
            row_cell(ui, STATE_W, |ui| {
                // The row's lamp carries the palette colour; the word is text
                // and takes the step of it that reads on the drawer.
                ui.label(RichText::new(word).color(theme::readable(color, p.surface1, p)));
            });
        });
        // The second line: this row's actions, left to right in reading order
        // under the platform they belong to.
        //
        // Not a right_to_left layout: that reverses them on screen, so a row
        // reads "Forget key" before "Use saved key" while the invites panel
        // builds the same pair the other way round, and it pins an
        // unconfigured platform's lone "Add key" to the drawer's right edge.
        ui.horizontal(|ui| {
            match configured_id {
                Some(id) => {
                    if ui
                        .button("Remove")
                        .on_hover_text(if live {
                            "stops this destination; every other one keeps streaming"
                        } else {
                            "drops this destination before it goes live"
                        })
                        .clicked()
                    {
                        action = Some(RowAction::Remove(id));
                    }
                }
                None => {
                    // A key leads nowhere on a session that cannot stream, so
                    // the two controls that send one are off. Forget key is
                    // not: a key on this computer is still a key on this
                    // computer, and deleting it must always work.
                    let can_stream = snap.stream.unavailable_reason().is_none();
                    let dead_end = "this session cannot stream, so a key would go nowhere";
                    if self.rows[index].saved {
                        if ui
                            .add_enabled(can_stream, Button::new("Use saved key"))
                            .on_disabled_hover_text(dead_end)
                            .clicked()
                        {
                            action = Some(RowAction::UseSaved);
                        }
                        if ui
                            .button("Forget key")
                            .on_hover_text("deletes it from this computer's keychain")
                            .clicked()
                        {
                            action = Some(RowAction::Forget);
                        }
                    // Deliberately not a selected button: the open pane
                    // below is the state, and the accent belongs to air.
                    } else if ui
                        .add_enabled(can_stream, Button::new("Add key"))
                        .on_disabled_hover_text(dead_end)
                        .clicked()
                    {
                        action = Some(RowAction::ToggleEntry);
                    }
                }
            }
        });
        if let Some(reason) = reason {
            // The reason the pipeline gave, verbatim and full width. It never
            // contains a key; the server strips that by construction.
            //
            // Capped, because it is usually ffmpeg's own sentence rather than
            // one of ours, and a quoted diagnosis is that program's length.
            // Left uncapped it sizes the drawer to whatever the encoder felt
            // like saying.
            theme::reason_capped(ui, ("destination-reason", index), reason);
        }
        if self.rows[index].entering {
            self.entry_ui(ui, index, rt);
        }
        match action {
            Some(RowAction::Remove(id)) => self.remove(index, id, rt),
            Some(RowAction::UseSaved) => self.use_saved_key(index, rt),
            Some(RowAction::Forget) => self.forget_key(index),
            Some(RowAction::ToggleEntry) => {
                let opening = !self.rows[index].entering;
                self.rows[index].close_entry();
                self.rows[index].entering = opening;
                self.error = None;
            }
            None => {}
        }
    }

    /// The key pane: where the key comes from, then the masked field. No
    /// reveal: the character count is how a paste gets checked. Indented under
    /// its row rather than framed, because a panel inside a panel is a card in
    /// a card.
    fn entry_ui(&mut self, ui: &mut Ui, index: usize, rt: &dyn Runtime) {
        let mut save = false;
        let mut cancel = false;
        ui.indent(("destination-key", index), |ui| {
            ui.label(theme::muted(ui, self.rows[index].guidance.clone()));
            let (label, url) = key_page(self.rows[index].platform);
            if ui.button(label).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
            ui.add_space(theme::SPACE_SM);
            // The label sits above the field rather than beside it: a key is
            // long, the drawer is narrow, and the field is the one control
            // here that must never be the thing that gets squeezed.
            let label = ui.label(theme::muted(ui, "stream key")).id;
            let field = ui.add(
                TextEdit::singleline(&mut self.rows[index].key_input)
                    .desired_width(f32::INFINITY)
                    .password(true)
                    .hint_text("paste the key"),
            );
            // A masked field has no visible text to take a name from, so it
            // takes one from the label above it. Its accessible role is a
            // password input and its accessible value is the mask, so the key
            // is not in the accessibility tree either.
            // Enter saves, as it does in the join field and the chat field. A
            // pasted key ends in a keystroke either way; making this one the
            // odd field out is how a host learns to distrust the return key.
            save = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            field.labelled_by(label);
            // A masked field cannot be proofread, so the count stands in for
            // reading it back.
            ui.label(theme::mono_muted(
                ui,
                format!(
                    "{} characters",
                    self.rows[index].key_input.trim().chars().count()
                ),
            ));
            ui.checkbox(
                &mut self.rows[index].remember,
                "keep this key in this computer's keychain",
            );
            ui.add_space(theme::SPACE_SM);
            ui.horizontal(|ui| {
                save |= ui.button("Save key").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if save {
            self.save_typed_key(index, rt);
        }
        if cancel {
            self.rows[index].close_entry();
            self.error = None;
        }
    }

    /// The one control that puts the room on air, and the one that takes it
    /// off. No confirmation on either: going live is a deliberate press with
    /// the lamp as its receipt, and a host who needs the stream to stop needs
    /// it to stop now, not after a dialog.
    fn go_live_ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime) {
        let running = snap.stream.destinations.iter().any(|d| {
            matches!(
                d.state,
                DestinationState::Live | DestinationState::Connecting
            )
        });
        let configured = self.configured_count(snap);
        // The control and its counts on one line, the sentence wrapped under
        // them: at the drawer's width a button and a sentence side by side is
        // how a row runs off the edge.
        let mut note = None;
        ui.horizontal(|ui| {
            if running {
                if ui.button("Stop streaming").clicked() {
                    rt.send(Command::StopStream);
                }
                ui.label(theme::mono(
                    ui,
                    format!("{} on air", snap.stream.live_count()),
                ));
            } else {
                let can_stream = snap.stream.unavailable_reason().is_none();
                if ui
                    .add_enabled(configured > 0 && can_stream, Button::new("Go live"))
                    .on_disabled_hover_text(if can_stream {
                        "add a key first"
                    } else {
                        "this session has no broadcast relay to stream through"
                    })
                    .clicked()
                {
                    rt.send(Command::StartStream);
                }
                note = Some(match (can_stream, configured) {
                    (false, _) => "No broadcast relay, so nothing can go on air.".to_owned(),
                    (true, 0) => "Add a key to stream somewhere.".to_owned(),
                    (true, 1) => "Everyone in the session sees the on air lamp.".to_owned(),
                    (true, n) => format!("{n} destinations, one at a time or all at once."),
                });
            }
            // Counted in both states: every destination failing at once puts
            // the section back on Go live, and that is exactly when the count
            // must not disappear.
            let failed = snap.stream.failed_count();
            if failed > 0 {
                let p = theme::palette_of(ui);
                ui.label(
                    RichText::new(format!("{failed} failed"))
                        .monospace()
                        .color(theme::danger_ink(p)),
                );
            }
        });
        if let Some(note) = note {
            ui.add(egui::Label::new(theme::muted(ui, note).small()).wrap());
        }
    }
}

/// The dropped count, once, under the rows.
///
/// It is one counter for the whole encode, so a per-row copy of it was a shared
/// number rendered as though it belonged to one destination, which invites
/// exactly the wrong fix: removing a destination does nothing about it (#264).
/// The bitrate is a line under the rows for the same reason, and this is the
/// same shape of fact.
///
/// Two counts, because one could not say which of two opposite things a host
/// was looking at (#278). A repeat means the machine ran out of time to draw a
/// frame and sent the last picture again: nothing is missing, the audio stays
/// in step, and the cost is a stutter. A loss means the encoder's queue refused
/// a frame, so the video is that many pictures short of its audio and the
/// machine has already failed to deliver. The first says it is struggling; the
/// second says it is too late.
fn frame_counts(ui: &mut Ui, snap: &Snapshot) {
    // Every destination reports the same pair; taking the largest of each is
    // how this stays right if one row's status is a frame behind another's.
    let destinations = &snap.stream.destinations;
    if destinations.is_empty() {
        return;
    }
    let repeated = destinations.iter().map(|d| d.repeated_frames).max();
    let dropped = destinations.iter().map(|d| d.dropped_frames).max();
    let (Some(repeated), Some(dropped)) = (repeated, dropped) else {
        return;
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = theme::SPACE_SM;
        // Repeats first: they are the common case and the early warning.
        count(
            ui,
            repeated,
            "repeated",
            REPEATED_AMBER,
            REPEATED_RED,
            "frames the machine had no time to draw, sent again as the last picture: nothing \
             is missing and the sound stays in step, but the video stutters and the machine \
             is at its limit",
        );
        ui.label(theme::mono_muted(ui, "·"));
        // "dropped" keeps the word every streamer already has for a frame that
        // never arrived. It is only now true of this counter: until #278 it
        // covered repeats too, which are the opposite reading.
        count(
            ui,
            dropped,
            "dropped",
            DROPPED_AMBER,
            DROPPED_RED,
            "frames the encoder would not take, so the video is that many pictures short of \
             the sound. One count for the one encode every destination shares, so removing a \
             destination does not bring it down",
        );
    });
}

/// One frame count in the meter's own colour language: this is a health
/// reading, so it runs muted, amber, red.
fn count(ui: &mut Ui, n: u64, word: &str, amber: u64, red: u64, hover: &str) {
    let p = theme::palette_of(ui);
    let color = if n >= red {
        p.meter_red
    } else if n >= amber {
        p.meter_amber
    } else {
        p.text_muted
    };
    ui.label(
        RichText::new(format!("{n} {word}"))
            .monospace()
            .color(theme::readable(color, p.surface1, p)),
    )
    .on_hover_text(hover);
}

enum RowAction {
    Remove(DestinationId),
    UseSaved,
    Forget,
    ToggleEntry,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::MemStore;
    use crate::demo::{DemoRuntime, FROZEN_FRAME, RecordingRuntime};

    fn panel() -> (Arc<MemStore>, DestinationsPanel) {
        let store = Arc::new(MemStore::default());
        let panel = DestinationsPanel::new(store.clone());
        (store, panel)
    }

    fn recorder() -> Arc<RecordingRuntime<DemoRuntime>> {
        Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
            FROZEN_FRAME,
            true,
        )))
    }

    #[test]
    fn both_shipped_platforms_get_a_row_and_nothing_else_does() {
        let (_, panel) = panel();
        let names: Vec<&str> = panel.rows.iter().map(|r| r.display_name.as_str()).collect();
        assert_eq!(names, vec!["Twitch", "YouTube Live"]);
        for row in &panel.rows {
            assert!(
                !row.guidance.is_empty(),
                "{} has no guidance",
                row.platform.as_str()
            );
        }
        // The catalog still records the platforms that do not ship, with
        // their reasons, but the sheet is for the ones a host can use: a
        // list of absent features is not something anyone can act on.
        assert!(
            !PlatformCatalog::bundled().deferred().is_empty(),
            "the catalog is still the record of what does not ship"
        );
    }

    #[test]
    fn saving_a_key_sends_it_once_and_keeps_none_of_it() {
        let (store, mut panel) = panel();
        let rt = recorder();
        panel.rows[0].key_input = "live_000000_fakefakefake".to_owned();
        panel.save_typed_key(0, &*rt);

        // Sent exactly once, with the id this panel minted.
        let sent: Vec<Command> = rt.commands();
        assert_eq!(sent.len(), 1);
        match &sent[0] {
            Command::AddDestination { id, platform, key } => {
                assert_eq!(*id, DestinationId(0));
                assert_eq!(*platform, StreamPlatform::Twitch);
                assert_eq!(key.expose(), "live_000000_fakefakefake");
            }
            other => panic!("expected AddDestination, got {other:?}"),
        }
        // Nothing of it is left in the panel.
        assert!(panel.rows[0].key_input.is_empty());
        assert!(panel.error.is_none());
        // And it is in the keychain because the row asked for that.
        let field = stream_key_field(StreamPlatform::Twitch);
        assert_eq!(
            store.get(field.0, field.1).as_deref(),
            Some("live_000000_fakefakefake")
        );
    }

    #[test]
    fn declining_to_remember_leaves_the_keychain_empty() {
        let (store, mut panel) = panel();
        let rt = recorder();
        panel.rows[1].remember = false;
        panel.rows[1].key_input = "0000-fake-fake-fake".to_owned();
        panel.save_typed_key(1, &*rt);
        assert_eq!(rt.commands().len(), 1);
        let field = stream_key_field(StreamPlatform::YouTube);
        assert_eq!(store.get(field.0, field.1), None);
        assert!(!panel.rows[1].saved);
    }

    #[test]
    fn a_saved_key_is_read_straight_into_the_send() {
        let (store, _) = panel();
        let field = stream_key_field(StreamPlatform::Twitch);
        store
            .set(field.0, field.1, "live_000000_saved")
            .expect("set");
        let mut panel = DestinationsPanel::new(store.clone());
        assert!(panel.rows[0].saved, "a stored key must show as saved");
        let rt = recorder();
        panel.use_saved_key(0, &*rt);
        match &rt.commands()[0] {
            Command::AddDestination { key, .. } => assert_eq!(key.expose(), "live_000000_saved"),
            other => panic!("expected AddDestination, got {other:?}"),
        }
        // The plaintext never entered the panel's own state.
        assert!(panel.rows[0].key_input.is_empty());
    }

    #[test]
    fn a_saved_key_that_vanished_asks_for_it_again_and_sends_nothing() {
        let (store, _) = panel();
        let field = stream_key_field(StreamPlatform::Twitch);
        store
            .set(field.0, field.1, "live_000000_saved")
            .expect("set");
        let mut panel = DestinationsPanel::new(store.clone());
        store.delete(field.0, field.1);
        let rt = recorder();
        panel.use_saved_key(0, &*rt);
        assert!(
            rt.commands().is_empty(),
            "nothing may be sent without a key"
        );
        assert!(panel.rows[0].entering, "the key pane must open");
        assert!(panel.error.is_some());
    }

    #[test]
    fn an_empty_or_oversized_key_is_refused_before_the_wire() {
        let (store, mut panel) = panel();
        let rt = recorder();
        panel.rows[0].key_input = "   ".to_owned();
        panel.save_typed_key(0, &*rt);
        assert!(rt.commands().is_empty());
        assert!(panel.error.as_deref().is_some_and(|e| e.contains("paste")));

        panel.rows[0].key_input = "k".repeat(MAX_STREAM_KEY_LEN + 1);
        panel.save_typed_key(0, &*rt);
        assert!(
            rt.commands().is_empty(),
            "an oversized key must not be sent"
        );
        assert!(
            panel
                .error
                .as_deref()
                .is_some_and(|e| e.contains(&MAX_STREAM_KEY_LEN.to_string())),
            "the error must name the limit: {:?}",
            panel.error
        );
        // Refused before the keychain write, so nothing is left saved.
        let field = stream_key_field(StreamPlatform::Twitch);
        assert!(!panel.rows[0].saved);
        assert_eq!(store.get(field.0, field.1), None);
    }

    #[test]
    fn forgetting_a_key_clears_the_keychain_slot() {
        let (store, _) = panel();
        let field = stream_key_field(StreamPlatform::Twitch);
        store
            .set(field.0, field.1, "live_000000_fake")
            .expect("set");
        let mut panel = DestinationsPanel::new(store.clone());
        panel.forget_key(0);
        assert_eq!(store.get(field.0, field.1), None);
        assert!(!panel.rows[0].saved);
    }

    #[test]
    fn ids_are_minted_once_and_never_reused() {
        let (_, mut panel) = panel();
        let rt = recorder();
        panel.rows[0].key_input = "live_000000_fake".to_owned();
        panel.save_typed_key(0, &*rt);
        panel.rows[1].key_input = "0000-fake".to_owned();
        panel.save_typed_key(1, &*rt);
        let ids: Vec<DestinationId> = rt
            .commands()
            .into_iter()
            .filter_map(|c| match c {
                Command::AddDestination { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![DestinationId(0), DestinationId(1)]);

        // Remove the first and configure it again: a fresh id, so a late
        // status for the old one cannot be read as the new one.
        panel.remove(0, DestinationId(0), &*rt);
        panel.rows[0].key_input = "live_000000_fake2".to_owned();
        panel.save_typed_key(0, &*rt);
        let last = rt
            .commands()
            .into_iter()
            .filter_map(|c| match c {
                Command::AddDestination { id, .. } => Some(id),
                _ => None,
            })
            .next_back();
        assert_eq!(last, Some(DestinationId(2)));
    }

    /// The reason is the server's, so the line around it has to survive one
    /// that already ends in a full stop as well as one that does not.
    #[test]
    fn the_cannot_stream_line_reads_as_one_sentence() {
        assert_eq!(
            cannot_stream_line("the broadcast tooling could not be downloaded"),
            "This session cannot stream: the broadcast tooling could not be downloaded."
        );
        assert_eq!(
            cannot_stream_line("the relay is gone."),
            "This session cannot stream: the relay is gone."
        );
    }

    /// The row's state is the server's, not the panel's: whatever this panel
    /// asked for, what it shows is the last status.
    #[test]
    fn the_snapshot_decides_what_a_row_shows() {
        let (_, mut panel) = panel();
        let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
        let snap = rt.snapshot();
        assert!(matches!(panel.row_state(0, &snap), RowState::NoKey));
        assert_eq!(panel.configured_count(&snap), 0);

        panel.rows[0].id = Some(DestinationId(7));
        assert!(matches!(panel.row_state(0, &snap), RowState::Waiting));
        assert_eq!(panel.configured_count(&snap), 1);

        rt.set_destinations(&[(
            StreamPlatform::Twitch,
            DestinationState::Failed {
                reason: "pusher exited: connection refused".to_owned(),
            },
        )]);
        let snap = rt.snapshot();
        match panel.row_state(0, &snap) {
            RowState::Reported(view) => {
                assert!(matches!(view.state, DestinationState::Failed { .. }));
            }
            _ => panic!("a reported destination must win over the panel's own record"),
        }
        assert_eq!(snap.stream.failed_count(), 1);
        assert!(!snap.stream.on_air(), "a failed destination is not on air");
    }
}
