//! Screen routing, the top bar, settings, and demo wiring. All real layout
//! lives in `root_ui` on a plain `Ui` so egui_kittest drives the exact code
//! the eframe window runs.

use std::sync::Arc;

use egui::{Context, Frame, Ui};

use crate::avatar;
use crate::creds::{self, CredStore, EnvReader, KeyringStore};
use crate::demo::DemoRuntime;
use crate::exec::{Executor, Job};
use crate::live::{AudioSettings, CostedRuntime, LiveRuntime};
use crate::runtime::{AvatarHandle, Command, ConnState, LevelsView, Runtime, Snapshot};
use crate::screens::destinations::DestinationsPanel;
use crate::screens::devices::{DeviceCatalog, DevicesScreen};
use crate::screens::home::{HomeAction, HomeScreen, RecentSession};
use crate::screens::host::{HostWizard, LaunchOutcome, WizardEvent};
use crate::screens::invites::{self, InvitesPanel};
use crate::screens::session::{SessionEvent, SessionScreen};
use crate::theme::{self, Theme};
use crate::widgets::{AVATAR_D_STRIP, avatar_disc, sweep_avatar_textures};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Devices,
    HostWizard,
    Session,
}

impl Screen {
    fn title(self) -> &'static str {
        match self {
            Screen::Home => "home",
            Screen::Devices => "devices",
            Screen::HostWizard => "host a session",
            Screen::Session => "session",
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
    pub session: SessionScreen,
    pub runtime: Option<Box<dyn Runtime>>,
    /// Concrete handle to the live runtime when one is active; device
    /// changes go through it (the [`Runtime`] contract has no device
    /// commands, by design: device setup is app plumbing, not session
    /// state).
    pub live: Option<Arc<LiveRuntime>>,
    pub settings_open: bool,
    /// The path typed into the settings sheet's avatar row. There is no
    /// application settings store to keep it in, so a chosen avatar lasts
    /// for this run of the app; joining a session announces it.
    pub avatar_path: String,
    /// Why the last avatar load failed, shown inline under the row.
    pub avatar_error: Option<String>,
    /// The avatar you picked, decoded for the settings disc.
    pub own_avatar: Option<AvatarHandle>,
    /// The same avatar's file bytes, kept so a join can announce it.
    own_avatar_bytes: Option<Vec<u8>>,
    /// Device selection last applied to the live runtime, as
    /// (capture_idx, playback_idx, buffer_frames).
    applied_audio: (usize, usize, u32),
    /// End-session teardown in flight; a progress sheet shows until the
    /// provider confirms the instance is gone.
    ending: Option<Job<Result<(), String>>>,
    creds: Arc<dyn CredStore>,
    env: EnvReader,
    exec: Arc<Executor>,
}

impl JamApp {
    pub fn new() -> Self {
        let devices = DevicesScreen::default();
        let applied_audio = (
            devices.capture_idx,
            devices.playback_idx,
            devices.buffer_frames,
        );
        let creds: Arc<dyn CredStore> = Arc::new(KeyringStore);
        let env = creds::system_env();
        let exec = Arc::new(Executor::new());
        JamApp {
            theme: Theme::Dark,
            screen: Screen::Home,
            home: HomeScreen::default(),
            recent: RecentSession::load(),
            devices,
            catalog: DeviceCatalog::demo(),
            wizard: HostWizard::new(Arc::clone(&creds), Arc::clone(&env), Arc::clone(&exec)),
            session: SessionScreen::default(),
            runtime: None,
            live: None,
            settings_open: false,
            avatar_path: String::new(),
            avatar_error: None,
            own_avatar: None,
            own_avatar_bytes: None,
            applied_audio,
            ending: None,
            creds,
            env,
            exec,
        }
    }

    /// The production entry point: like [`new`](Self::new) but with the
    /// device pickers fed from the platform audio backend instead of the
    /// demo catalog.
    pub fn with_system_devices() -> Self {
        let mut app = Self::new();
        match jamstream_audio_io::backend().devices() {
            Ok(devices) => app.catalog = DeviceCatalog::from_backend(&devices),
            Err(err) => {
                tracing::warn!(%err, "device enumeration failed");
                app.catalog = DeviceCatalog {
                    capture: Vec::new(),
                    playback: Vec::new(),
                };
            }
        }
        app
    }

    /// `--demo`: straight into a live fake session as the host.
    pub fn demo() -> Self {
        let mut app = Self::new();
        app.runtime = Some(Box::new(DemoRuntime::host()));
        // The demo reaches no platform, so its destinations sheet keeps keys
        // in memory for the run instead of touching the real keychain.
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
        }
    }

    /// The wizard finished a real launch: auto-join with the host invite
    /// (member 0, never rendered anywhere), wrap the runtime so snapshots
    /// carry the cost meter and the invite book's token ids, and land on
    /// the session screen with the invites panel already open so the next
    /// act is sharing the links.
    fn enter_hosted_session(&mut self, outcome: LaunchOutcome) {
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
        match LiveRuntime::join(&invite, settings, jamstream_audio_io::backend()) {
            Ok(rt) => {
                let rt = Arc::new(rt);
                self.live = Some(Arc::clone(&rt));
                let panel = InvitesPanel::new(outcome.state.clone(), outcome.state_path.clone());
                let costed = CostedRuntime::new(
                    rt,
                    outcome.state.hourly_microusd,
                    outcome.state.created_unix,
                    panel.token_map(),
                );
                self.runtime = Some(Box::new(costed));
                self.session = SessionScreen::default();
                self.session.invites = Some(panel);
                self.session.invites_open = true;
                // Hosting is the only role that can stream, so the sheet
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
    fn end_session(&mut self) {
        if let Some(rt) = self.runtime.as_deref() {
            rt.send(Command::Leave);
        }
        if let Some(panel) = self.session.invites.take() {
            let provider = creds::build_provider(&panel.state.provider, &*self.creds, &self.env);
            let state = panel.state;
            let path = panel.path;
            self.ending = Some(
                self.exec
                    .run(async move { invites::end_session(provider?, state, path).await }),
            );
        }
        self.runtime = None;
        self.live = None;
        self.recent = RecentSession::load();
        self.screen = Screen::Home;
    }

    /// Progress sheet for the teardown; a failure lands on the home screen
    /// with the provider's error.
    fn ending_progress(&mut self, ctx: &Context) {
        let Some(job) = &mut self.ending else {
            return;
        };
        if let Some(result) = job.poll() {
            self.ending = None;
            if let Err(err) = result {
                self.home.error = Some(format!("ending the session failed: {err}"));
            }
            self.recent = RecentSession::load();
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

        self.settings_window(ui.ctx());

        match self.screen {
            Screen::Home => {
                if let Some(action) = self.home.ui(ui, &self.recent) {
                    match action {
                        HomeAction::Join(invite) => {
                            let settings = self.audio_settings();
                            match LiveRuntime::join(
                                &invite,
                                settings,
                                jamstream_audio_io::backend(),
                            ) {
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
            Screen::Devices => {
                let levels = self.current_levels();
                self.devices.ui(ui, &self.catalog, &levels);
            }
            Screen::HostWizard => {
                if let Some(WizardEvent::Launched(outcome)) = self.wizard.ui(ui) {
                    self.enter_hosted_session(*outcome);
                }
            }
            Screen::Session => {
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

        // Device picks apply immediately: mid-session the live runtime
        // reopens its stream, otherwise the selection just waits for the
        // next join.
        let selected = (
            self.devices.capture_idx,
            self.devices.playback_idx,
            self.devices.buffer_frames,
        );
        if selected != self.applied_audio {
            self.applied_audio = selected;
            if let Some(live) = &self.live {
                live.reconfigure_audio(self.audio_settings());
            }
        }

        // Every surface has had its turn: whatever avatar texture nothing
        // drew this frame belongs to a member who left, or to a picture that
        // was replaced. Free it.
        sweep_avatar_textures(ui.ctx());
    }

    fn current_levels(&self) -> LevelsView {
        self.runtime
            .as_deref()
            .map(|rt| rt.snapshot().levels)
            .unwrap_or_default()
    }

    /// Settings as a compact sheet anchored top right, under the top bar:
    /// it never covers the mixer strips or the status readout, and Escape
    /// or its own Close button dismisses it.
    fn settings_window(&mut self, ctx: &Context) {
        if !self.settings_open {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.settings_open = false;
            return;
        }
        let panel = {
            let p = theme::palette(self.theme);
            egui::Frame::new()
                .fill(p.surface1)
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(egui::CornerRadius::same(theme::RADIUS))
                .inner_margin(egui::Margin::same(14))
        };
        egui::Window::new("Settings")
            .title_bar(false)
            .frame(panel)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-10.0, 56.0))
            .fixed_size(egui::vec2(340.0, 0.0))
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
                ui.add_space(theme::SPACE_SM);
                let snap = self.runtime.as_deref().map(|rt| rt.snapshot());
                self.avatar_ui(ui, snap.as_ref());
                ui.add_space(theme::SPACE_SM);
                let levels = snap.map(|s| s.levels).unwrap_or_default();
                self.devices.panels_ui(ui, &self.catalog, &levels);
            });
    }

    /// "Your avatar": the disc as everyone else sees it, a path field, and
    /// the two actions. There is no file dialog in this app, so the path is
    /// pasted; every refusal names its own reason on the spot.
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
        let mut load = false;
        let mut remove = false;
        ui.horizontal(|ui| {
            avatar_disc(ui, &name, handle.as_ref(), AVATAR_D_STRIP, false);
            ui.vertical(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.avatar_path)
                        .desired_width(206.0)
                        .hint_text("path to a PNG or JPEG"),
                );
                ui.horizontal(|ui| {
                    load = ui.button("Load").clicked();
                    remove = ui
                        .add_enabled(handle.is_some(), egui::Button::new("Remove"))
                        .clicked();
                });
            });
        });
        if let Some(err) = &self.avatar_error {
            let p = theme::palette_of(ui);
            ui.label(egui::RichText::new(err.clone()).color(p.danger));
        }
        ui.label(theme::muted(
            ui,
            format!(
                "PNG or JPEG up to {} KB. Removing applies here and on your next join; \
                 the session keeps the picture you already sent.",
                avatar::MAX_BYTES / 1024
            ),
        ));
        if load {
            self.load_avatar();
        }
        if remove {
            self.remove_avatar();
        }
    }

    /// Reads the pasted path and validates it against the same caps the
    /// transfer layer enforces, so a refusal happens here with a specific
    /// message instead of silently on the wire.
    fn load_avatar(&mut self) {
        self.avatar_error = None;
        let path = self.avatar_path.trim().to_owned();
        if path.is_empty() {
            self.avatar_error = Some("type or paste the path to a PNG or JPEG file".to_owned());
            return;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                self.avatar_error = Some(format!("{path} could not be read: {err}"));
                return;
            }
        };
        match avatar::decode(avatar::local_key(&bytes), &bytes) {
            Ok(handle) => {
                self.own_avatar = Some(handle);
                if let Some(rt) = self.runtime.as_deref() {
                    rt.send(Command::SetOwnAvatar(Some(bytes.clone())));
                }
                self.own_avatar_bytes = Some(bytes);
            }
            Err(err) => self.avatar_error = Some(err.to_string()),
        }
    }

    fn remove_avatar(&mut self) {
        self.avatar_error = None;
        self.avatar_path.clear();
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

impl Default for JamApp {
    fn default() -> Self {
        Self::new()
    }
}

impl eframe::App for JamApp {
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        theme::apply(ctx, self.theme);
        // Meters, the cost ticker, and connection quality move while a
        // session or the input meter is on screen.
        let animating = self.ending.is_some()
            || match self.screen {
                Screen::Session | Screen::Devices => true,
                Screen::HostWizard => self.wizard.busy() || self.settings_open,
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
