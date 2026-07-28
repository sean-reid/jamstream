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
use crate::picker::{Pick, Picked};
use crate::runtime::{AvatarHandle, Command, ConnState, Runtime, Snapshot};
use crate::screens::destinations::DestinationsPanel;
use crate::screens::devices::{Block, DeviceCatalog, DevicesScreen};
use crate::screens::home::{HomeAction, HomeScreen, RecentSession};
use crate::screens::host::{HostWizard, LaunchOutcome, WizardEvent};
use crate::screens::invites::{self, InvitesPanel};
use crate::screens::session::{SessionEvent, SessionScreen};
use crate::theme::{self, Theme};
use crate::widgets::{AVATAR_D_STRIP, avatar_disc, sweep_avatar_textures};

/// The settings drawer's content width. Every row inside it fits, so the
/// drawer is this wide and no wider whatever the window does.
const DRAWER_W: f32 = 340.0;

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
    /// Builds the app around a credential store and an environment reader.
    /// Both are parameters rather than defaults on purpose. [`KeyringStore`]
    /// talks to the operating system's keychain, and the host wizard reads
    /// it while it is being constructed to decide which providers are ready;
    /// a test that got one by default would prompt the developer running it
    /// for their real cloud tokens and stream keys, and could write to them.
    /// Tests call [`JamApp::in_memory`]; production calls
    /// [`JamApp::with_system_devices`], which is the only place in the crate
    /// that names `KeyringStore`.
    pub fn new(creds: Arc<dyn CredStore>, env: EnvReader) -> Self {
        let devices = DevicesScreen::default();
        let applied_audio = (
            devices.capture_idx,
            devices.playback_idx,
            devices.buffer_frames,
        );
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
            avatar_error: None,
            avatar_picture: None,
            avatar_dialog: None,
            own_avatar: None,
            own_avatar_bytes: None,
            applied_audio,
            ending: None,
            creds,
            env,
            exec,
        }
    }

    /// An app whose credentials and environment live in this process and
    /// nowhere else. The constructor for tests and for any surface that
    /// must not read what the developer has stored.
    pub fn in_memory() -> Self {
        let env: EnvReader = Arc::new(|_: &str| None);
        Self::new(Arc::new(creds::MemStore::default()), env)
    }

    /// The production entry point: the real keychain and the real
    /// environment, with the device pickers fed from the platform audio
    /// backend instead of the demo catalog.
    pub fn with_system_devices() -> Self {
        let mut app = Self::new(Arc::new(KeyringStore), creds::system_env());
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
                    panel.tokens(),
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

        // Before the sheet draws, so a picture that landed while the dialog
        // was open shows in the same frame it arrived.
        self.poll_avatar_pick();
        // Escape is consumed here, ahead of the screen, even though the sheet
        // is drawn after it: the session screen closes its own sheets on
        // Escape, and the innermost thing entered has to be the first thing
        // left.
        if self.settings_open
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
                let snap = self.runtime.as_deref().map(|rt| rt.snapshot());
                let levels = snap.as_ref().map(|s| s.levels).unwrap_or_default();
                let m2e = snap.and_then(|s| s.stats.mouth_to_ear_ms);
                self.devices.ui(ui, &self.catalog, &levels, m2e);
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
        // Last, so a session has already drawn its status bar and the drawer
        // knows where to stop.
        self.settings_drawer(ui.ctx());

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
    fn settings_drawer(&mut self, ctx: &Context) {
        if !self.settings_open {
            return;
        }
        let body_h = theme::sheet_body_height(ctx, self.drawer_floor(ctx));
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
                // The header stays put and the rest scrolls, so Close is
                // never the control that scrolled away.
                egui::ScrollArea::vertical()
                    .id_salt("settings-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.settings_body(ui));
            });
    }

    /// Ordered by how often a control is reached for: the two a musician
    /// touches mid session, then the devices behind them, then the things
    /// set once.
    fn settings_body(&mut self, ui: &mut Ui) {
        let snap = self.runtime.as_deref().map(|rt| rt.snapshot());
        let levels = snap.as_ref().map(|s| s.levels).unwrap_or_default();
        let m2e = snap.as_ref().and_then(|s| s.stats.mouth_to_ear_ms);
        self.devices
            .audio_ui(ui, Block::Flat, &self.catalog, &levels, m2e);
        ui.add_space(theme::SPACE_MD);
        self.avatar_ui(ui, snap.as_ref());
        ui.add_space(theme::SPACE_MD);
        ui.label(theme::title(ui, "Theme"));
        for (value, label) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
            let response = crate::widgets::pick_row(ui, label, self.theme == value, true, |ui| {
                ui.label(label);
            });
            if response.clicked() {
                self.theme = value;
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
        if let Some(err) = &self.avatar_error {
            let p = theme::palette_of(ui);
            ui.label(egui::RichText::new(err.clone()).color(p.danger));
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
