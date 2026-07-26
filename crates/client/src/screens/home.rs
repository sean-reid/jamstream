//! Landing screen: paste an invite, host a session, and the recent
//! sessions recorded by the CLI and the host wizard.

use egui::{Key, RichText, TextEdit, Ui};
use jamstream_protocol::invite::Invite;

use crate::theme;

/// One row from the jamstream-cli state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentSession {
    pub short_id: String,
    pub provider: String,
    pub region: String,
    pub status: String,
}

impl RecentSession {
    /// Reads every session state file the CLI knows about, newest first.
    pub fn load() -> Vec<RecentSession> {
        let mut rows: Vec<RecentSession> = jamstream_cli::state::list()
            .unwrap_or_default()
            .into_iter()
            .map(|(_, s)| RecentSession {
                short_id: s.session_id_hex.chars().take(8).collect(),
                provider: s.provider,
                region: s.region,
                status: match s.status {
                    jamstream_cli::state::SessionStatus::Running => "running".to_owned(),
                    jamstream_cli::state::SessionStatus::Ended => "ended".to_owned(),
                },
            })
            .collect();
        rows.reverse();
        rows
    }
}

pub enum HomeAction {
    Join(Box<Invite>),
    Host,
}

#[derive(Default)]
pub struct HomeScreen {
    pub invite_text: String,
    pub error: Option<String>,
}

impl HomeScreen {
    pub fn ui(&mut self, ui: &mut Ui, recent: &[RecentSession]) -> Option<HomeAction> {
        let mut action = None;
        theme::focused_column(ui, 560.0, |ui| {
            theme::wordmark(ui, 26.0);
            ui.add_space(theme::SPACE_XS);
            ui.label(theme::muted(
                ui,
                "Play music together over a server that exists only while you play.",
            ));
            ui.add_space(theme::SPACE_XL);

            theme::panel(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(theme::title(ui, "Join a session"));
                ui.horizontal(|ui| {
                    let button_w = 52.0;
                    let field = ui.add(
                        TextEdit::singleline(&mut self.invite_text)
                            .desired_width(ui.available_width() - button_w)
                            .hint_text("paste an invite, jamstream://join/..."),
                    );
                    let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if ui.button("Join").clicked() || submitted {
                        match Invite::decode(&self.invite_text) {
                            Ok(invite) => {
                                self.error = None;
                                action = Some(HomeAction::Join(Box::new(invite)));
                            }
                            Err(err) => self.error = Some(err.to_string()),
                        }
                    }
                });
                if let Some(err) = &self.error {
                    let p = theme::palette_of(ui);
                    ui.label(RichText::new(err.clone()).color(p.danger));
                }
            });
            ui.add_space(theme::SPACE_MD);

            theme::panel(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(theme::title(ui, "Host a session"));
                ui.label(theme::muted(
                    ui,
                    "Launches a short-lived server near your group and mints the invites.",
                ));
                ui.add_space(theme::SPACE_XS);
                if ui.button("Host a session").clicked() {
                    action = Some(HomeAction::Host);
                }
            });
            ui.add_space(theme::SPACE_MD);

            theme::panel(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(theme::title(ui, "Recent sessions"));
                if recent.is_empty() {
                    ui.label(theme::muted(
                        ui,
                        "No sessions yet; host one or paste an invite above.",
                    ));
                } else {
                    for row in recent {
                        ui.horizontal(|ui| {
                            ui.label(theme::mono(ui, row.short_id.clone()));
                            ui.label(theme::muted(ui, format!("{} {}", row.provider, row.region)));
                            ui.label(theme::mono_muted(ui, row.status.clone()));
                        });
                    }
                }
            });
        });
        action
    }
}
