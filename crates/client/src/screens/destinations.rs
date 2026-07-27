//! The host's destinations sheet: which platforms this session streams to,
//! what each one is doing, and the control that puts the room on air.
//!
//! Stream keys follow the wizard's credential pattern one step stricter: the
//! field is masked with no reveal, because a host is a keystroke away from
//! being on camera and a key on a shared screen is worse than a typo. The
//! character count stands in for reading it back. A sent key is wiped from
//! the input; what is left lives in the keychain and the server's memory.

use std::sync::Arc;

use egui::{Align, Align2, Button, Layout, RichText, Stroke, TextEdit, Ui, vec2};
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
/// stream comes up. The bitrate is not one of them: one encode feeds every
/// destination, so it is a line under the rows instead of the same number
/// twice.
const NAME_W: f32 = 132.0;
const STATE_W: f32 = 80.0;
const DROP_W: f32 = 100.0;

/// Any dropped frame is worth noticing; 30 is a second of video gone at the
/// pipeline's 30 fps, which earns the meter's red.
const DROPPED_AMBER: u64 = 1;
const DROPPED_RED: u64 = 30;

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
    /// The one deferred-platform line, built from the catalog so the app and
    /// the docs cannot disagree about what ships.
    deferred: String,
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
        let names: Vec<&str> = catalog
            .deferred()
            .iter()
            .map(|d| d.display_name.as_str())
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
            deferred: format!("Not shipped yet: {}.", names.join(", ")),
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
// three host sheets are one thing in three states.

impl DestinationsPanel {
    pub fn ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime, open: &mut bool) {
        let panel = {
            let p = theme::palette_of(ui);
            egui::Frame::new()
                .fill(p.surface1)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(egui::CornerRadius::same(theme::RADIUS))
                .inner_margin(egui::Margin::same(14))
        };
        egui::Window::new("Destinations")
            .title_bar(false)
            .frame(panel)
            .anchor(Align2::RIGHT_TOP, vec2(-10.0, 56.0))
            .fixed_size(vec2(460.0, 0.0))
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.horizontal(|ui| {
                    ui.label(theme::title(ui, "Destinations"));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            *open = false;
                        }
                    });
                });
                ui.label(theme::muted(
                    ui,
                    "Where this session streams. Keys are never shown and never \
                     written to the server's disk.",
                ));
                ui.add_space(theme::SPACE_SM);
                for index in 0..self.rows.len() {
                    self.row_ui(ui, index, snap, rt);
                }
                ui.add_space(theme::SPACE_SM);
                ui.label(theme::muted(ui, self.encode.clone()).small());
                ui.label(theme::muted(ui, self.deferred.clone()).small());
                if let Some(err) = &self.error {
                    let p = theme::palette_of(ui);
                    ui.add_space(theme::SPACE_XS);
                    ui.label(RichText::new(err.clone()).color(p.danger));
                }
                ui.add_space(theme::SPACE_MD);
                ui.separator();
                self.go_live_ui(ui, snap, rt);
            });
    }

    /// One platform: lamp, name, what it is doing, the numbers, its actions,
    /// and the key pane when it is open. Every row is laid out on the same
    /// columns whatever state it is in, so nothing moves as a stream comes up.
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
        // The count only exists once the server reports the destination.
        let dropped = match &state {
            RowState::Reported(view) => Some(view.dropped_frames),
            _ => None,
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
                ui.label(RichText::new(word).color(color));
            });
            row_cell(ui, DROP_W, |ui| {
                if let Some(n) = dropped {
                    // The meter's own color language: dropped frames are the
                    // health reading, so they run muted, amber, red.
                    let color = if n >= DROPPED_RED {
                        p.meter_red
                    } else if n >= DROPPED_AMBER {
                        p.meter_amber
                    } else {
                        p.text_muted
                    };
                    ui.label(
                        RichText::new(format!("{n} dropped"))
                            .monospace()
                            .color(color),
                    )
                    .on_hover_text("video frames the broadcast had to skip");
                }
            });
            ui.with_layout(
                Layout::right_to_left(Align::Center),
                |ui| match configured_id {
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
                        if self.rows[index].saved {
                            if ui.button("Use saved key").clicked() {
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
                        } else if ui.button("Add key").clicked() {
                            action = Some(RowAction::ToggleEntry);
                        }
                    }
                },
            );
        });
        if let Some(reason) = reason {
            // The reason the pipeline gave, verbatim and full width. It never
            // contains a key; the server strips that by construction.
            ui.horizontal(|ui| {
                ui.add_space(NAME_W);
                ui.add(egui::Label::new(RichText::new(reason).color(p.danger)).wrap());
            });
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
            ui.horizontal(|ui| {
                let mut label = None;
                row_cell(ui, 88.0, |ui| {
                    label = Some(ui.label(theme::muted(ui, "stream key")).id);
                });
                let field = ui.add(
                    TextEdit::singleline(&mut self.rows[index].key_input)
                        .desired_width(200.0)
                        .password(true)
                        .hint_text("paste the key"),
                );
                // A masked field has no visible text to take a name from, so
                // it takes one from the label beside it. Its accessible role
                // is a password input and its accessible value is the mask,
                // so the key is not in the accessibility tree either.
                if let Some(label) = label {
                    field.labelled_by(label);
                }
                // A masked field cannot be proofread, so the count stands in
                // for reading it back.
                ui.label(theme::mono_muted(
                    ui,
                    format!(
                        "{} characters",
                        self.rows[index].key_input.trim().chars().count()
                    ),
                ));
            });
            ui.checkbox(
                &mut self.rows[index].remember,
                "keep this key in this computer's keychain",
            );
            ui.add_space(theme::SPACE_SM);
            ui.horizontal(|ui| {
                save = ui.button("Save key").clicked();
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
                if ui
                    .add_enabled(configured > 0, Button::new("Go live"))
                    .clicked()
                {
                    rt.send(Command::StartStream);
                }
                ui.label(theme::muted(
                    ui,
                    match configured {
                        0 => "Add a key to stream somewhere.".to_owned(),
                        1 => "Everyone in the session sees the on air lamp.".to_owned(),
                        n => format!("{n} destinations, one at a time or all at once."),
                    },
                ));
            }
            // Counted in both states: every destination failing at once puts
            // the sheet back on Go live, and that is exactly when the count
            // must not disappear.
            let failed = snap.stream.failed_count();
            if failed > 0 {
                let p = theme::palette_of(ui);
                ui.label(
                    RichText::new(format!("{failed} failed"))
                        .monospace()
                        .color(p.danger),
                );
            }
        });
    }
}

enum RowAction {
    Remove(DestinationId),
    UseSaved,
    Forget,
    ToggleEntry,
}

/// The reminder the whole room gets: the lamp plus what it is doing, beside
/// the mouth-to-ear readout, for as long as anything is on air. Not host
/// only, and not hidden behind a sheet.
pub fn on_air_indicator(ui: &mut Ui, snap: &Snapshot) {
    let live = snap.stream.live_count();
    let failed = snap.stream.failed_count();
    if live == 0 && failed == 0 {
        return;
    }
    let p = theme::palette_of(ui);
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        if live > 0 {
            ui.label(theme::mono(ui, format!("{live} live")))
                .on_hover_text("destinations currently receiving the broadcast");
        }
        if failed > 0 {
            ui.label(
                RichText::new(format!("{failed} failed"))
                    .monospace()
                    .color(p.danger),
            )
            .on_hover_text("a destination stopped; open destinations for the reason");
        }
    });
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
    fn both_shipped_platforms_get_a_row_and_the_deferred_ones_a_line() {
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
        // The catalog's four deferred platforms, named, so the app and the
        // docs cannot disagree about what ships.
        assert!(panel.deferred.contains("Facebook Live"));
        assert!(panel.deferred.contains("Kick"));
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
