//! Screen routing, the top bar, settings, and demo wiring. All real layout
//! lives in `root_ui` on a plain `Ui` so egui_kittest drives the exact code
//! the eframe window runs.

use std::sync::Arc;

use egui::{Context, Frame, Ui};

use crate::demo::DemoRuntime;
use crate::live::{AudioSettings, LiveRuntime};
use crate::runtime::{ConnState, LevelsView, Runtime};
use crate::screens::devices::{DeviceCatalog, DevicesScreen};
use crate::screens::home::{HomeAction, HomeScreen, RecentSession};
use crate::screens::host::{self, HostWizard, WizardEvent, WizardStep};
use crate::screens::session::{SessionEvent, SessionScreen};
use crate::theme::{self, Theme};

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
    /// A launch was requested; execute it on the next frame so the
    /// launching step paints first.
    launch_pending: bool,
    /// Device selection last applied to the live runtime, as
    /// (capture_idx, playback_idx, buffer_frames).
    applied_audio: (usize, usize, u32),
}

impl JamApp {
    pub fn new() -> Self {
        let devices = DevicesScreen::default();
        let applied_audio = (
            devices.capture_idx,
            devices.playback_idx,
            devices.buffer_frames,
        );
        JamApp {
            theme: Theme::Dark,
            screen: Screen::Home,
            home: HomeScreen::default(),
            recent: RecentSession::load(),
            devices,
            catalog: DeviceCatalog::demo(),
            wizard: HostWizard::new(host::provider_rows_from_env()),
            session: SessionScreen::default(),
            runtime: None,
            live: None,
            settings_open: false,
            launch_pending: false,
            applied_audio,
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
                                }
                                Err(err) => self.home.error = Some(err.to_string()),
                            }
                        }
                        HomeAction::Host => {
                            self.wizard = HostWizard::new(host::provider_rows_from_env());
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
                if self.launch_pending && self.wizard.step == WizardStep::Launching {
                    let outcome = host::launch(&self.wizard);
                    self.wizard.finish_launch(outcome);
                    self.launch_pending = false;
                }
                match self.wizard.ui(ui) {
                    Some(WizardEvent::LaunchRequested) => self.launch_pending = true,
                    Some(WizardEvent::Close) => {
                        self.recent = RecentSession::load();
                        self.screen = Screen::Home;
                    }
                    None => {}
                }
            }
            Screen::Session => {
                if let Some(rt) = self.runtime.as_deref() {
                    // One snapshot pull per frame; screens never call back in.
                    let snap = rt.snapshot();
                    if let Some(SessionEvent::Left) = self.session.ui(ui, &snap, rt) {
                        self.runtime = None;
                        self.live = None;
                        self.recent = RecentSession::load();
                        self.screen = Screen::Home;
                    }
                } else {
                    self.screen = Screen::Home;
                }
            }
        }

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
                let levels = self
                    .runtime
                    .as_deref()
                    .map(|rt| rt.snapshot().levels)
                    .unwrap_or_default();
                self.devices.panels_ui(ui, &self.catalog, &levels);
            });
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
        let animating = match self.screen {
            Screen::Session | Screen::Devices => true,
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
