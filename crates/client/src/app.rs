//! Screen routing, the top bar, settings, and demo wiring. All real layout
//! lives in `root_ui` on a plain `Ui` so egui_kittest drives the exact code
//! the eframe window runs.

use std::sync::Arc;

use egui::{Context, Frame, Ui};

use crate::avatar;
use crate::creds::{self, CredStore, EnvReader, KeyringStore};
use crate::demo::DemoRuntime;
use crate::exec::{Executor, Job};
use crate::live::{AudioSettings, CostedRuntime, LiveError, LiveRuntime};
use crate::picker::{Pick, Picked};
use crate::runtime::{AvatarHandle, Command, ConnState, Runtime, Snapshot};
use crate::screens::destinations::DestinationsPanel;
use crate::screens::devices::{Block, DeviceCatalog, DeviceInfo, DevicesEvent, DevicesScreen};
use crate::screens::home::{HomeAction, HomeScreen, RecentSession};
use crate::screens::host::{HostWizard, LaunchOutcome, WizardEvent};
use crate::screens::invites::{self, InvitesPanel};
use crate::screens::recording::RecordingPanel;
use crate::screens::session::{SessionEvent, SessionScreen, SettingsTab};
use crate::screens::takes::TakesScreen;
use crate::theme::{self, Theme};
use crate::widgets::{AVATAR_D_STRIP, avatar_disc, sweep_avatar_textures};

/// The settings drawer's content width. Every row inside it fits, so the
/// drawer is this wide and no wider whatever the window does.
const DRAWER_W: f32 = 340.0;

/// How the app turns an invite into a live session.
///
/// A parameter rather than a call to [`LiveRuntime::join`], because that opens
/// the platform's sound card and there is nothing to unplug in a test. Both
/// ways into a session, the wizard's auto-join and the home screen's Join, go
/// through this one, so a test can drive either of them over the offline WAV
/// backend and get the app's real wiring: the [`CostedRuntime`] wrapper, the
/// invite book's token map, the panels the host screen hangs off.
///
/// It is what closes #218. `enter_hosted_session` had no test caller at all and
/// `wizard_local.rs` re-implemented its body by hand, so the app could have
/// stopped wrapping in `CostedRuntime`, losing the cost meter and leaving the
/// mixer's Revoke pointing at nothing, with the suite green.
pub type Joiner = Arc<
    dyn Fn(&jamstream_protocol::invite::Invite, AudioSettings) -> Result<LiveRuntime, LiveError>
        + Send
        + Sync,
>;

/// The production joiner: the real sound card, through the platform backend.
pub fn system_joiner() -> Joiner {
    Arc::new(|invite, settings| LiveRuntime::join(invite, settings, jamstream_audio_io::backend()))
}

/// How the app enumerates audio devices, for the same reason [`Joiner`] is a
/// parameter: enumeration reaches the platform's sound system, and a test
/// pressing Rescan has no interface to unplug. Production asks the backend;
/// tests hand in a catalog of their own.
pub type Enumerator = Arc<dyn Fn() -> Result<DeviceCatalog, String> + Send + Sync>;

/// The production enumerator, shared by startup and the Rescan button so both
/// build the same catalog.
pub fn system_enumerator() -> Enumerator {
    Arc::new(|| {
        jamstream_audio_io::backend()
            .devices()
            .map(|devices| DeviceCatalog::from_backend(&devices))
            .map_err(|err| err.to_string())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Where the window is. There is no Devices screen: audio settings live in
/// the drawer's Audio tab, which is reachable from everywhere, and the
/// full-screen route was set from nowhere but a snapshot fixture (#191).
pub enum Screen {
    Home,
    HostWizard,
    Session,
    /// The takes past sessions left. Off Home rather than inside a session,
    /// because a take outlives the session that made it.
    Takes,
}

impl Screen {
    fn title(self) -> &'static str {
        match self {
            Screen::Home => "home",
            Screen::HostWizard => "host a session",
            Screen::Session => "session",
            Screen::Takes => "takes",
        }
    }
}

pub struct JamApp {
    pub theme: Theme,
    pub screen: Screen,
    pub home: HomeScreen,
    pub recent: Vec<RecentSession>,
    pub devices: DevicesScreen,
    pub catalog: DeviceCatalog,
    pub wizard: HostWizard,
    /// The Recording tab: the bucket takes go to and the key that writes it.
    /// One per app rather than one per session, because it is a setting for this
    /// computer. What the wizard needs from it is handed over as plain data
    /// every frame, so the wizard holds no second copy of the answer.
    pub recording: RecordingPanel,
    pub session: SessionScreen,
    /// The Takes screen. Holds the keychain behind it, because fetching a take
    /// needs the storage key the Recording tab saved, and it is the whole point
    /// that the app does not send a host to a terminal for a key it has.
    pub takes: TakesScreen,
    pub runtime: Option<Box<dyn Runtime>>,
    /// Concrete handle to the live runtime when one is active; device
    /// changes go through it (the [`Runtime`] contract has no device
    /// commands, by design: device setup is app plumbing, not session
    /// state).
    pub live: Option<Arc<LiveRuntime>>,
    pub settings_open: bool,
    /// Which drawer tab is showing. Kept for as long as the app runs, so a
    /// host working through destinations reopens on Broadcast rather than back
    /// on Audio, and deliberately not persisted: a fresh launch opens on
    /// Audio, which is what a musician sets up first.
    pub settings_tab: SettingsTab,
    /// Why the last avatar load failed, shown inline under the row.
    pub avatar_error: Option<String>,
    /// The picture picked this run: its file name and what it became, for
    /// the line under the row. There is no application settings store to
    /// keep it in, so it lasts for this run of the app; joining a session
    /// announces it.
    avatar_picture: Option<avatar::Picture>,
    /// The file dialog, while one is open.
    avatar_dialog: Option<Pick>,
    /// The avatar you picked, decoded for the settings disc.
    pub own_avatar: Option<AvatarHandle>,
    /// The same avatar's fitted bytes, kept so a join can announce it.
    own_avatar_bytes: Option<Vec<u8>>,
    /// Device selection last handed to the live runtime, by id rather than by
    /// picker index: a rescan reorders the catalog without moving the
    /// selection, and comparing indexes across that would reopen the stream
    /// for nothing.
    applied_audio: AudioSettings,
    /// How Rescan re-enumerates; see [`Enumerator`].
    pub enumerate: Enumerator,
    /// Where the audio setup is remembered between launches, `None` in tests
    /// and in-memory apps so no fixture reads or rewrites the developer's
    /// own (#328).
    pub settings_path: Option<std::path::PathBuf>,
    /// End-session teardown in flight; a progress sheet shows until the
    /// provider confirms the instance is gone.
    ending: Option<Job<Result<(), String>>>,
    /// How a join is performed; see [`Joiner`].
    pub join: Joiner,
    creds: Arc<dyn CredStore>,
    env: EnvReader,
    exec: Arc<Executor>,
}

impl JamApp {
    /// Builds the app around a credential store and an environment reader.
    /// Both are parameters rather than defaults on purpose. [`KeyringStore`]
    /// talks to the operating system's keychain, and the host wizard reads
    /// it while it is being constructed to decide which providers are ready;
    /// a test that got one by default would prompt the developer running it
    /// for their real cloud tokens and stream keys, and could write to them.
    /// Tests call [`JamApp::in_memory`]; production calls
    /// [`JamApp::with_system_devices`], which is the only place in the crate
    /// that names `KeyringStore`.
    ///
    /// `prefs` is where the Recording tab keeps this computer's bucket, and it
    /// is a parameter for the same reason: `None` keeps the preferences in this
    /// process, so a test neither reads nor overwrites the developer's own.
    pub fn new(
        creds: Arc<dyn CredStore>,
        env: EnvReader,
        prefs: Option<std::path::PathBuf>,
    ) -> Self {
        let devices = DevicesScreen::default();
        let exec = Arc::new(Executor::new());
        let mut app = JamApp {
            theme: Theme::Dark,
            screen: Screen::Home,
            home: HomeScreen::default(),
            recent: RecentSession::load(),
            devices,
            catalog: DeviceCatalog::demo(),
            wizard: HostWizard::new(Arc::clone(&creds), Arc::clone(&env), Arc::clone(&exec)),
            recording: RecordingPanel::new(
                Arc::clone(&creds),
                Arc::clone(&env),
                Arc::clone(&exec),
                prefs,
            ),
            session: SessionScreen::default(),
            takes: TakesScreen::new(Arc::clone(&creds), Arc::clone(&env), Arc::clone(&exec)),
            runtime: None,
            live: None,
            settings_open: false,
            settings_tab: SettingsTab::Audio,
            avatar_error: None,
            avatar_picture: None,
            avatar_dialog: None,
            own_avatar: None,
            own_avatar_bytes: None,
            applied_audio: AudioSettings::default(),
            enumerate: Arc::new(|| Ok(DeviceCatalog::demo())),
            settings_path: None,
            ending: None,
            join: system_joiner(),
            creds,
            env,
            exec,
        };
        app.applied_audio = app.audio_settings();
        app
    }

    /// An app whose credentials and environment live in this process and
    /// nowhere else. The constructor for tests and for any surface that
    /// must not read what the developer has stored.
    pub fn in_memory() -> Self {
        let env: EnvReader = Arc::new(|_: &str| None);
        Self::new(Arc::new(creds::MemStore::default()), env, None)
    }

    /// The production entry point: the real keychain and the real
    /// environment, with the device pickers fed from the platform audio
    /// backend instead of the demo catalog.
    pub fn with_system_devices() -> Self {
        // A preferences path that cannot be resolved is not worth refusing to
        // start over: the Recording tab then keeps its bucket for this run and
        // says why it could not remember it.
        let prefs = crate::prefs::path();
        let mut app = Self::new(
            Arc::new(KeyringStore::system()),
            creds::system_env(),
            prefs.as_ref().ok().cloned(),
        );
        if let Err(err) = prefs {
            app.recording.error = Some(err);
        }
        app.enumerate = system_enumerator();
        match (app.enumerate)() {
            Ok(catalog) => app.catalog = catalog,
            Err(err) => {
                tracing::warn!(%err, "device enumeration failed");
                app.catalog = DeviceCatalog {
                    capture: Vec::new(),
                    playback: Vec::new(),
                };
            }
        }
        app.settings_path = crate::prefs::app_path().ok();
        app.restore_audio_prefs();
        app.applied_audio = app.audio_settings();
        app
    }

    /// Puts the saved audio setup back on the pickers: each device by id and
    /// only when the catalog still holds it, so a selection whose interface
    /// is unplugged today quietly stays on the system default, and the buffer
    /// only when it is one of the picker's own choices.
    pub fn restore_audio_prefs(&mut self) {
        let Some(path) = &self.settings_path else {
            return;
        };
        let prefs = crate::prefs::AppPrefs::load_from(path);
        if prefs.capture_id.is_some() {
            let (idx, found) = DeviceCatalog::find(&self.catalog.capture, &prefs.capture_id);
            if found {
                self.devices.capture_idx = idx;
            }
        }
        if prefs.playback_id.is_some() {
            let (idx, found) = DeviceCatalog::find(&self.catalog.playback, &prefs.playback_id);
            if found {
                self.devices.playback_idx = idx;
            }
        }
        if let Some(frames) = prefs.buffer_frames
            && crate::screens::devices::BUFFER_CHOICES.contains(&frames)
        {
            self.devices.buffer_frames = frames;
        }
        if let Some(allow) = prefs.allow_exclusive {
            self.devices.allow_exclusive = allow;
        }
    }

    /// Writes the audio setup where the next launch reads it. Failure is a
    /// log line: losing a remembered picker beats interrupting a session.
    fn persist_audio_prefs(&self) {
        let Some(path) = &self.settings_path else {
            return;
        };
        let settings = self.audio_settings();
        let prefs = crate::prefs::AppPrefs {
            capture_id: settings.capture_id,
            playback_id: settings.playback_id,
            buffer_frames: Some(settings.buffer_frames),
            allow_exclusive: Some(settings.allow_exclusive),
        };
        if let Err(err) = prefs.save_to(path) {
            tracing::warn!(%err, "audio preferences not saved");
        }
    }

    /// Rescan: re-enumerate and keep the selection by device id. A selected
    /// device the new catalog no longer holds falls back to the System
    /// default entry with a note under the pickers saying so; falling back
    /// with the old name still showing was the pickers lying (#325). The
    /// note also covers the scan itself failing, which otherwise looks like
    /// a button that does nothing.
    pub fn rescan_devices(&mut self) {
        let selected_capture = self.catalog.capture.get(self.devices.capture_idx).cloned();
        let selected_playback = self
            .catalog
            .playback
            .get(self.devices.playback_idx)
            .cloned();
        match (self.enumerate)() {
            Ok(catalog) => {
                self.catalog = catalog;
                let mut lost = Vec::new();
                let mut place = |list: &[DeviceInfo], selected: Option<DeviceInfo>| {
                    let Some(selected) = selected else {
                        return 0;
                    };
                    let (idx, found) = DeviceCatalog::find(list, &selected.id);
                    if !found {
                        lost.push(selected.name);
                    }
                    idx
                };
                self.devices.capture_idx = place(&self.catalog.capture, selected_capture);
                self.devices.playback_idx = place(&self.catalog.playback, selected_playback);
                self.devices.rescan_note = match lost.as_slice() {
                    [] => None,
                    names => Some(format!(
                        "{} is no longer present; using the system default",
                        names.join(" and ")
                    )),
                };
            }
            Err(err) => {
                self.devices.rescan_note = Some(format!("device scan failed: {err}"));
            }
        }
    }

    /// `--demo`: straight into a live fake session as the host. It reaches
    /// no platform and provisions nothing, so it keeps everything in memory
    /// rather than touching the real keychain.
    pub fn demo() -> Self {
        let mut app = Self::in_memory();
        app.runtime = Some(Box::new(DemoRuntime::host()));
        app.session.destinations =
            Some(DestinationsPanel::new(Arc::new(creds::MemStore::default())));
        app.screen = Screen::Session;
        app
    }

    /// The device and buffer selection as the live runtime consumes it.
    fn audio_settings(&self) -> AudioSettings {
        AudioSettings {
            capture_id: self
                .catalog
                .capture
                .get(self.devices.capture_idx)
                .and_then(|d| d.id.clone()),
            playback_id: self
                .catalog
                .playback
                .get(self.devices.playback_idx)
                .and_then(|d| d.id.clone()),
            buffer_frames: self.devices.buffer_frames,
            allow_exclusive: self.devices.allow_exclusive,
        }
    }

    /// The wizard finished a real launch: record what the bucket agreed to,
    /// auto-join with the host invite (member 0, never rendered anywhere),
    /// wrap the runtime so snapshots carry the cost meter and the invite
    /// book's token ids, and land on the session screen with the invites
    /// panel already open so the next act is sharing the links.
    ///
    /// Public so a test can drive the real thing. Every early return in here
    /// is a state a host can end up in, and a test that reimplemented the
    /// body would agree with itself about all of them.
    pub fn enter_hosted_session(&mut self, outcome: LaunchOutcome) {
        // First, and outside the join: what the bucket agreed to is true
        // whether or not this host gets into the session they just paid for,
        // and a retention choice nothing is enforcing has to survive every
        // early return below it.
        self.recording.applied = outcome.retention.clone();
        let invite = outcome
            .state
            .invites
            .first()
            .and_then(|record| jamstream_protocol::invite::Invite::decode(&record.invite).ok());
        let Some(invite) = invite else {
            self.wizard.launch_error =
                Some("the session launched but its host invite does not decode".to_owned());
            return;
        };
        let settings = self.audio_settings();
        match (self.join)(&invite, settings) {
            Ok(rt) => {
                let rt = Arc::new(rt);
                self.live = Some(Arc::clone(&rt));
                let panel = InvitesPanel::new(outcome.state.clone(), outcome.state_path.clone());
                let costed = CostedRuntime::new(
                    rt,
                    outcome.state.hourly_microusd,
                    outcome.state.created_unix,
                    panel.tokens(),
                );
                self.runtime = Some(Box::new(costed));
                self.session = SessionScreen::default();
                self.session.invites = Some(panel);
                // A session with nobody in it: open the drawer on the tab
                // holding the links, because sharing them is the next act.
                self.settings_open = true;
                self.settings_tab = SettingsTab::Invites;
                // Hosting is the only role that can stream, so the panel
                // exists only here. It reads the keychain for saved stream
                // keys on construction and holds no key of its own.
                self.session.destinations = Some(DestinationsPanel::new(Arc::clone(&self.creds)));
                self.recent = RecentSession::load();
                self.screen = Screen::Session;
                self.announce_own_avatar();
            }
            Err(err) => {
                self.wizard.launch_error = Some(format!(
                    "the server is running but joining it failed: {err}. \
                     End it with: jamstream end --last"
                ));
            }
        }
    }

    /// Host chose "end session for everyone": leave the session, then
    /// destroy the instance and mark the state file ended on the executor.
    ///
    /// Public for the same reason as [`JamApp::enter_hosted_session`]: it takes
    /// the invite book out of the screen, resolves the provider from the state
    /// file's own provider name, and drops the runtime, and a test that
    /// reimplemented that would agree with itself about all three.
    /// [`JamApp::poll_ending`] is how a caller waits for the teardown.
    pub fn end_session(&mut self) {
        if let Some(rt) = self.runtime.as_deref() {
            rt.send(Command::Leave);
        }
        if let Some(panel) = self.session.invites.take() {
            let provider = creds::build_provider(&panel.state.provider, &*self.creds, &self.env);
            let state = panel.state;
            let path = panel.path;
            self.ending = Some(self.exec.run(async move {
                let provider = provider?;
                invites::end_session(provider.as_ref(), state, path).await
            }));
        }
        self.runtime = None;
        self.live = None;
        self.recent = RecentSession::load();
        self.screen = Screen::Home;
    }

    /// The teardown's result, once it has one, and `None` while it is still
    /// running or was never started. Called once per frame by
    /// [`JamApp::ending_progress`]; also how a test waits for an end to finish.
    pub fn poll_ending(&mut self) -> Option<Result<(), String>> {
        let result = self.ending.as_mut()?.poll()?;
        self.ending = None;
        self.recent = RecentSession::load();
        Some(result)
    }

    /// True while the teardown is in flight.
    pub fn ending(&self) -> bool {
        self.ending.is_some()
    }

    /// Progress sheet for the teardown; a failure lands on the home screen
    /// with the provider's error.
    fn ending_progress(&mut self, ctx: &Context) {
        if let Some(result) = self.poll_ending() {
            if let Err(err) = result {
                self.home.error = Some(format!("ending the session failed: {err}"));
            }
            return;
        }
        if !self.ending() {
            return;
        }
        egui::Window::new("Ending session")
            .title_bar(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 120.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().color(theme::palette_of(ui).text_muted));
                    ui.label("Ending the session; the server is being destroyed.");
                });
            });
    }

    pub fn root_ui(&mut self, ui: &mut Ui) {
        egui::Panel::top(egui::Id::new("app-top")).show(ui, |ui| {
            ui.add_space(theme::SPACE_SM);
            ui.horizontal(|ui| {
                theme::wordmark(ui, 15.0);
                ui.separator();
                ui.label(theme::muted(ui, self.screen.title()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        self.settings_open = !self.settings_open;
                        if self.settings_open {
                            // One right-anchored sheet at a time; see
                            // `close_the_other_sheet`.
                            self.session.record_open = false;
                            self.session.took_the_sheet_anchor = false;
                        }
                    }
                    if self.screen != Screen::Home
                        && self.screen != Screen::Session
                        && ui.button("Home").clicked()
                    {
                        self.screen = Screen::Home;
                        self.recent = RecentSession::load();
                    }
                });
            });
            ui.add_space(theme::SPACE_SM);
        });

        // Before the sheet draws, so a picture that landed while the dialog
        // was open shows in the same frame it arrived.
        self.poll_avatar_pick();
        // Polled here rather than in the tab, so a bucket check that finishes
        // while the host is looking at another tab lands anyway.
        self.recording.poll();
        // Escape is consumed here, ahead of the screen, even though the sheet
        // is drawn after it: the session screen closes its own sheets on
        // Escape, and the innermost thing entered has to be the first thing
        // left. Which is why the drawer takes the key only when the screen has
        // nothing inside it waiting to be left: a confirmation standing over
        // the drawer is inside it, and closing the drawer under one was the
        // wrong end of the ladder (#180).
        if self.settings_open
            && !self.session_overlay_open()
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.settings_open = false;
        }

        match self.screen {
            Screen::Home => {
                if let Some(action) = self.home.ui(ui, &self.recent) {
                    match action {
                        HomeAction::Join(invite) => {
                            let settings = self.audio_settings();
                            match (self.join)(&invite, settings) {
                                Ok(rt) => {
                                    let rt = Arc::new(rt);
                                    self.live = Some(Arc::clone(&rt));
                                    self.runtime = Some(Box::new(rt));
                                    self.session = SessionScreen::default();
                                    self.screen = Screen::Session;
                                    self.announce_own_avatar();
                                }
                                Err(err) => self.home.error = Some(err.to_string()),
                            }
                        }
                        HomeAction::Takes => {
                            self.screen = Screen::Takes;
                            self.takes.reload();
                        }
                        HomeAction::Host => {
                            self.wizard = HostWizard::new(
                                Arc::clone(&self.creds),
                                Arc::clone(&self.env),
                                Arc::clone(&self.exec),
                            );
                            self.screen = Screen::HostWizard;
                        }
                    }
                }
            }
            Screen::Takes => self.takes.ui(ui),
            Screen::HostWizard => {
                // What this computer can record to, handed over as plain data
                // every frame: a bucket saved in the Recording tab is armable
                // in the wizard without leaving it, and the wizard keeps no
                // copy that could go stale.
                let setup = self.recording.setup(self.wizard.selected_provider_kind());
                self.wizard.set_recording_setup(setup);
                if let Some(WizardEvent::Launched(outcome)) = self.wizard.ui(ui) {
                    self.enter_hosted_session(*outcome);
                }
            }
            Screen::Session => {
                // Handed over as plain data every frame, the way the wizard is
                // handed the recording setup: the Recording tab owns the
                // launch's retention answer and the record sheet shows it, so
                // the screen keeps no second copy that could go stale.
                self.session.retention_note = self.recording.retention_note();
                // The drawer is drawn after the screen and covers the chat
                // panel, so the panel is told before it draws rather than
                // leaving its message field showing under the drawer's bottom
                // edge (#286).
                self.session.chat_covered = self.settings_open;
                if let Some(rt) = self.runtime.as_deref() {
                    // One snapshot pull per frame; screens never call back in.
                    let snap = rt.snapshot();
                    match self.session.ui(ui, &snap, rt) {
                        Some(SessionEvent::Left) => {
                            self.runtime = None;
                            self.live = None;
                            self.recent = RecentSession::load();
                            self.screen = Screen::Home;
                        }
                        Some(SessionEvent::EndSession) => self.end_session(),
                        None => {}
                    }
                } else {
                    self.screen = Screen::Home;
                }
            }
        }

        self.ending_progress(ui.ctx());
        self.close_the_other_sheet();
        // Last, so a session has already drawn its status bar and the drawer
        // knows where to stop. Ending the session is reached from in here now,
        // so the drawer answers with the same event the screen does.
        if let Some(SessionEvent::EndSession) = self.settings_drawer(ui.ctx()) {
            self.end_session();
        }

        // Device picks apply immediately: mid-session the live runtime
        // reopens its stream, otherwise the selection just waits for the
        // next join. Compared by id, not picker index, so a rescan that only
        // reordered the catalog does not reopen a healthy stream.
        let selected = self.audio_settings();
        if selected != self.applied_audio {
            self.applied_audio = selected.clone();
            if let Some(live) = &self.live {
                live.reconfigure_audio(selected);
            }
            self.persist_audio_prefs();
        }

        // Every surface has had its turn: whatever avatar texture nothing
        // drew this frame belongs to a member who left, or to a picture that
        // was replaced. Free it.
        sweep_avatar_textures(ui.ctx());
    }

    /// The record sheet and the settings drawer are both anchored to the right
    /// edge under the top bar, and the sheet is the wider of the two, so with
    /// both open a 44 px sliver of chopped words stuck out to the left of the
    /// drawer with a truncated Stop still clickable in it (#175). Whichever
    /// was opened last keeps the anchor; this is the half that runs after the
    /// screen, so the sheet's turn is the one recorded on the way past.
    fn close_the_other_sheet(&mut self) {
        if std::mem::take(&mut self.session.took_the_sheet_anchor) {
            self.settings_open = false;
        }
    }

    /// Whether the session screen has a confirmation, a key pane, or the
    /// record sheet up. Only meaningful on the session screen; every other
    /// screen has nothing that could sit over the drawer.
    fn session_overlay_open(&self) -> bool {
        self.screen == Screen::Session && self.runtime.is_some() && self.session.has_inner_overlay()
    }

    /// What the drawer has to stay above: the session's status bar, which
    /// carries the mouth-to-ear readout and the input and output meters.
    /// Buffer size and input level are adjusted against exactly those, so a
    /// sheet that covered them would hide the instruments it is being read
    /// against. Screens without a bar give it the window's bottom edge.
    fn drawer_floor(&self, ctx: &Context) -> f32 {
        let bottom = ctx.content_rect().bottom();
        match self.screen {
            Screen::Session => self.session.status_bar_top.unwrap_or(bottom),
            _ => bottom,
        }
    }

    /// Settings as a drawer down the right side, under the top bar and above
    /// the status bar, so the session stays readable beside it. Escape or
    /// Close dismisses it.
    ///
    /// The drawer takes the whole height it is allowed rather than the height
    /// its content wants, because at 800x600, the smallest window the app
    /// opens, the content is taller than the window. The body scrolls inside
    /// it, so what a short window puts out of sight is whatever the body puts
    /// last, and never the buffer picks or the input meter.
    fn settings_drawer(&mut self, ctx: &Context) -> Option<SessionEvent> {
        if !self.settings_open {
            return None;
        }
        let body_h = theme::sheet_body_height(ctx, self.drawer_floor(ctx));
        let mut event = None;
        egui::Window::new("Settings")
            .title_bar(false)
            .frame(theme::sheet_frame(theme::palette(self.theme)))
            .anchor(egui::Align2::RIGHT_TOP, theme::SHEET_OFFSET)
            .fixed_size(egui::vec2(DRAWER_W, body_h))
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(theme::title(ui, "Settings"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            self.settings_open = false;
                        }
                    });
                });
                ui.add_space(theme::SPACE_SM);
                let tabs = self.settings_tabs();
                self.tab_row(ui, &tabs);
                ui.add_space(theme::SPACE_SM);
                // The header and the tab row stay put and only the panel
                // scrolls, so Close and the way between tabs are never what
                // scrolled away. The salt is the tab's own, so each panel
                // keeps its own offset and opens at its own top.
                egui::ScrollArea::vertical()
                    .id_salt(("settings-scroll", self.settings_tab.label()))
                    .auto_shrink([false, false])
                    .show(ui, |ui| event = self.settings_body(ui));
            });
        event
    }

    /// The tabs this app can show right now. Session-scoped ones exist only
    /// inside a session, and only for a host: from the home screen there is no
    /// broadcast to mix and no seat to invite anyone into.
    fn settings_tabs(&self) -> Vec<SettingsTab> {
        match (self.screen, self.runtime.as_deref()) {
            (Screen::Session, Some(rt)) => self.session.settings_tabs(&rt.snapshot()),
            _ => vec![SettingsTab::Audio, SettingsTab::Recording, SettingsTab::You],
        }
    }

    /// The tab row, and the guard that a remembered tab cannot outlive the
    /// thing it showed: leaving a session drops Broadcast and Invites, and a
    /// drawer still pointing at one would open on nothing.
    ///
    /// Wrapped rather than one row: a host has five tabs and the drawer is
    /// 340 px, so the row takes a second line rather than pushing a tab off the
    /// edge or shortening every label to fit the widest case.
    fn tab_row(&mut self, ui: &mut Ui, tabs: &[SettingsTab]) {
        if !tabs.contains(&self.settings_tab) {
            self.settings_tab = SettingsTab::Audio;
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = theme::SPACE_SM;
            let p = theme::palette_of(ui);
            for tab in tabs {
                if ui
                    .add(theme::selectable(p, tab.label(), *tab == self.settings_tab))
                    .clicked()
                {
                    self.settings_tab = *tab;
                }
            }
        });
    }

    /// The selected tab's panel, and nothing else: one thing at a time in a
    /// drawer this narrow beats five sections in one scroll.
    fn settings_body(&mut self, ui: &mut Ui) -> Option<SessionEvent> {
        let snap = self.runtime.as_deref().map(|rt| rt.snapshot());
        match self.settings_tab {
            SettingsTab::Audio => {
                let levels = snap.as_ref().map(|s| s.levels).unwrap_or_default();
                let m2e = snap.as_ref().and_then(|s| s.stats.mouth_to_ear_ms);
                let refusal = snap.as_ref().and_then(|s| s.device_error.as_deref());
                let event =
                    self.devices
                        .audio_ui(ui, Block::Flat, &self.catalog, &levels, m2e, refusal);
                if let Some(DevicesEvent::Rescan) = event {
                    self.rescan_devices();
                }
                None
            }
            SettingsTab::Broadcast => {
                let rt = self.runtime.as_deref()?;
                let snap = snap?;
                self.session.broadcast_tab(ui, &snap, rt);
                None
            }
            SettingsTab::Invites => {
                let rt = self.runtime.as_deref()?;
                let snap = snap?;
                self.session.invites_tab(ui, &snap, rt)
            }
            SettingsTab::Recording => {
                self.recording.ui(ui);
                None
            }
            SettingsTab::You => {
                self.avatar_ui(ui, snap.as_ref());
                ui.add_space(theme::SPACE_XL);
                ui.label(theme::title(ui, "Theme"));
                for (value, label) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
                    let response =
                        crate::widgets::pick_row(ui, label, self.theme == value, true, |ui| {
                            ui.label(label);
                        });
                    if response.clicked() {
                        self.theme = value;
                    }
                }
                None
            }
        }
    }

    /// "Your avatar": the disc as everyone else sees it, the picker, and
    /// Remove. Under them, either what the picked file became or, before
    /// anything is picked, what will happen to one. Every refusal names its
    /// own reason on the spot.
    fn avatar_ui(&mut self, ui: &mut Ui, snap: Option<&Snapshot>) {
        ui.label(theme::title(ui, "Your avatar"));
        let me = snap.and_then(|s| s.members.iter().find(|m| m.is_you));
        // Outside a session there is no name to hash a hue from; "you" is
        // the honest placeholder rather than a fake initial.
        let name = me.map_or("you".to_owned(), |m| m.name.clone());
        // The local pick wins while it is in hand: it is what the session
        // will carry, and it shows before any roster round trip.
        let handle = self
            .own_avatar
            .clone()
            .or_else(|| me.and_then(|m| m.avatar.clone()));
        let picking = self.avatar_dialog.is_some();
        let mut choose = false;
        let mut remove = false;
        ui.horizontal(|ui| {
            avatar_disc(ui, &name, handle.as_ref(), AVATAR_D_STRIP, false);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    choose = ui
                        .add_enabled(!picking, egui::Button::new("Choose picture"))
                        .clicked();
                    remove = ui
                        .add_enabled(handle.is_some(), egui::Button::new("Remove"))
                        .clicked();
                });
                match &self.avatar_picture {
                    Some(picture) => {
                        ui.add(egui::Label::new(theme::muted(ui, picture.file.clone())).truncate());
                        let (w, h) = picture.fitted;
                        let line = if picture.source == picture.fitted {
                            format!("{w}x{h}")
                        } else {
                            let (sw, sh) = picture.source;
                            format!("{sw}x{sh} fitted to {w}x{h}")
                        };
                        ui.label(theme::mono_muted(ui, line));
                    }
                    None => {
                        ui.label(theme::muted(
                            ui,
                            format!(
                                "PNG or JPEG. A photo is cropped square and fitted to \
                                 {d}x{d}.",
                                d = avatar::FIT_DIM
                            ),
                        ));
                    }
                }
            });
        });
        if let Some(err) = self.avatar_error.clone() {
            theme::reason(ui, err);
        }
        if handle.is_some() {
            ui.label(theme::muted(
                ui,
                "Removing applies here and on your next join; the session keeps the \
                 picture you already sent.",
            ));
        }
        if choose {
            self.avatar_error = None;
            self.avatar_dialog = Some(Pick::picture());
        }
        if remove {
            self.remove_avatar();
        }
    }

    /// Takes the picked file when the dialog thread is done with it. The
    /// picture is already fitted and decoded by then; all that is left is to
    /// show it and tell the session.
    fn poll_avatar_pick(&mut self) {
        let Some(dialog) = &mut self.avatar_dialog else {
            return;
        };
        let Some(picked) = dialog.poll() else {
            return;
        };
        self.avatar_dialog = None;
        match picked {
            Picked::Cancelled => {}
            Picked::Loaded(picture) => self.set_avatar(*picture),
            Picked::Failed(err) => self.avatar_error = Some(err),
        }
    }

    /// Reads, fits, and decodes `path` on this thread, then applies it. The
    /// picker does the same work on its own thread; this is the way in for a
    /// test, which cannot open a dialog.
    pub fn load_avatar_from(&mut self, path: impl AsRef<std::path::Path>) {
        self.avatar_error = None;
        match avatar::load(path.as_ref()) {
            Ok(picture) => self.set_avatar(picture),
            Err(err) => self.avatar_error = Some(err),
        }
    }

    /// Shows the picture and announces it. The bytes are the fitted ones, so
    /// they are inside the transfer layer's caps by construction.
    fn set_avatar(&mut self, picture: avatar::Picture) {
        self.avatar_error = None;
        self.own_avatar = Some(picture.handle.clone());
        if let Some(rt) = self.runtime.as_deref() {
            rt.send(Command::SetOwnAvatar(Some(picture.bytes.clone())));
        }
        self.own_avatar_bytes = Some(picture.bytes.clone());
        self.avatar_picture = Some(picture);
    }

    fn remove_avatar(&mut self) {
        self.avatar_error = None;
        self.avatar_picture = None;
        self.own_avatar = None;
        self.own_avatar_bytes = None;
        if let Some(rt) = self.runtime.as_deref() {
            rt.send(Command::SetOwnAvatar(None));
        }
    }

    /// Announces the avatar picked before this session started. Called right
    /// after a join, the one moment the runtime is new and knows nothing.
    fn announce_own_avatar(&self) {
        if let (Some(bytes), Some(rt)) = (&self.own_avatar_bytes, self.runtime.as_deref()) {
            rt.send(Command::SetOwnAvatar(Some(bytes.clone())));
        }
    }
}

// No Default impl on purpose: which credential store the app gets is a
// choice, and `JamApp::default()` would be a silent way back to the real
// keychain. Callers name `in_memory` or `with_system_devices`.

impl eframe::App for JamApp {
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        theme::apply(ctx, self.theme);
        // Meters, the cost ticker, and connection quality move while a
        // session or the input meter is on screen.
        // A file dialog is a second window the frame loop cannot see, so
        // repaint until its thread answers.
        let animating = self.ending.is_some()
            || self.avatar_dialog.is_some()
            || self.recording.busy()
            || match self.screen {
                Screen::Session => true,
                Screen::HostWizard => self.wizard.busy() || self.settings_open,
                Screen::Takes => self.takes.busy() || self.settings_open,
                _ => self.settings_open,
            };
        if animating {
            ctx.request_repaint();
        }
        // A session that ended from the far side falls back to home.
        if self.screen == Screen::Session
            && let Some(rt) = self.runtime.as_deref()
            && matches!(rt.snapshot().stats.state, ConnState::Idle)
        {
            self.runtime = None;
            self.live = None;
            self.screen = Screen::Home;
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let fill = theme::palette(self.theme).surface0;
        egui::CentralPanel::default_margins()
            .frame(Frame::new().fill(fill).inner_margin(egui::Margin::same(10)))
            .show(ui, |ui| self.root_ui(ui));
    }
}
