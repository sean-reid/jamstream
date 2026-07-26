//! Screen routing, the top bar, settings, and demo wiring. All real layout
//! lives in `root_ui` on a plain `Ui` so egui_kittest drives the exact code
//! the eframe window runs.

use egui::{Context, Frame, RichText, Ui};

use crate::demo::DemoRuntime;
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
    pub settings_open: bool,
    /// A launch was requested; execute it on the next frame so the
    /// launching step paints first.
    launch_pending: bool,
}

impl JamApp {
    pub fn new() -> Self {
        JamApp {
            theme: Theme::Dark,
            screen: Screen::Home,
            home: HomeScreen::default(),
            recent: RecentSession::load(),
            devices: DevicesScreen::default(),
            catalog: DeviceCatalog::demo(),
            wizard: HostWizard::new(host::provider_rows_from_env()),
            session: SessionScreen::default(),
            runtime: None,
            settings_open: false,
            launch_pending: false,
        }
    }

    /// `--demo`: straight into a live fake session as the host.
    pub fn demo() -> Self {
        let mut app = Self::new();
        app.runtime = Some(Box::new(DemoRuntime::host()));
        app.screen = Screen::Session;
        app
    }

    pub fn root_ui(&mut self, ui: &mut Ui) {
        egui::Panel::top(egui::Id::new("app-top")).show(ui, |ui| {
            ui.add_space(theme::SPACE_SM);
            ui.horizontal(|ui| {
                let wordmark = egui::FontId::new(15.0, theme::semibold(ui));
                ui.label(RichText::new("jamstream").font(wordmark));
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
                        HomeAction::Join(_invite) => {
                            // Real joining lands with the networking pass;
                            // a valid invite opens the demo session so the
                            // whole surface is walkable today.
                            self.runtime = Some(Box::new(DemoRuntime::musician()));
                            self.session = SessionScreen::default();
                            self.screen = Screen::Session;
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
                        self.recent = RecentSession::load();
                        self.screen = Screen::Home;
                    }
                } else {
                    self.screen = Screen::Home;
                }
            }
        }
    }

    fn current_levels(&self) -> LevelsView {
        self.runtime
            .as_deref()
            .map(|rt| rt.snapshot().levels)
            .unwrap_or_default()
    }

    fn settings_window(&mut self, ctx: &Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("Theme");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.theme, Theme::Dark, "dark");
                    ui.radio_value(&mut self.theme, Theme::Light, "light");
                });
                ui.add_space(theme::SPACE_SM);
                ui.label("Devices");
                let levels = self
                    .runtime
                    .as_deref()
                    .map(|rt| rt.snapshot().levels)
                    .unwrap_or_default();
                self.devices.ui(ui, &self.catalog, &levels);
            });
        self.settings_open = open;
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
