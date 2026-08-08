//! Landing screen: paste an invite, host a session, and the recent
//! sessions recorded by the CLI and the host wizard.

use egui::{Key, TextEdit, Ui};
use jamstream_protocol::invite::Invite;

use crate::sweep::SweepOutcome;
use crate::theme;

/// One row from the jamstream-cli state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentSession {
    pub short_id: String,
    pub provider: String,
    pub region: String,
    /// A session that has not ended is a machine still being billed, which is
    /// why it is never one of the rows this screen drops.
    pub running: bool,
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
                running: matches!(s.status, jamstream_cli::state::SessionStatus::Running),
            })
            .collect();
        rows.reverse();
        rows
    }
}

pub enum HomeAction {
    Join(Box<Invite>),
    Host,
    /// The takes those sessions left, which outlive them.
    Takes,
    /// Stop everything tagged jamstream, in every account this computer can
    /// reach. Confirmed first: it takes down a session a band may be on.
    Sweep,
}

/// What the Recent sessions card knows about the sweep, handed over as plain
/// data every frame like the rest of this screen's inputs.
#[derive(Default, Clone, Copy)]
pub struct SweepView<'a> {
    pub busy: bool,
    pub outcome: Option<&'a SweepOutcome>,
}

#[derive(Default)]
pub struct HomeScreen {
    pub invite_text: String,
    pub error: Option<String>,
}

impl HomeScreen {
    /// `name` is the app's own display name rather than this screen's state,
    /// because a join is not the only thing that sends it: the host's
    /// auto-join announces the same one.
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        recent: &[RecentSession],
        name: &mut String,
        sweep: SweepView<'_>,
    ) -> Option<HomeAction> {
        let mut action = None;
        let room = ui.available_height();
        // The recent list is every session this computer ever recorded, so the
        // column outgrows the window on any machine that has hosted for a
        // while. The lead stays measured against the viewport so a short page
        // still sits where it always did.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                theme::focused_column(ui, 560.0, room, |ui, _| {
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
                        // The name first: it is who the roster and the take files
                        // will say you are, and it is remembered, so most days it is
                        // already filled in.
                        ui.horizontal(|ui| {
                            ui.label(theme::muted(ui, "your name"));
                            ui.add(
                                TextEdit::singleline(name)
                                    .desired_width(200.0)
                                    .char_limit(64)
                                    .hint_text("how the roster shows you"),
                            );
                        });
                        ui.add_space(theme::SPACE_XS);
                        ui.horizontal(|ui| {
                            let button_w = 52.0;
                            let field = ui.add(
                                TextEdit::singleline(&mut self.invite_text)
                                    .desired_width(ui.available_width() - button_w)
                                    .hint_text("paste an invite, jamstream://join/..."),
                            );
                            let submitted =
                                field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
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
                        if let Some(err) = self.error.clone() {
                            theme::reason(ui, err);
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
                        // Takes is the way to what those sessions recorded, and Stop
                        // strays is the way to correct them: the rows are a record of
                        // what happened, and both acts belong to the record rather
                        // than to a fourth card or a fourth thing in the top bar.
                        // Stop strays is here even with no rows, because a machine
                        // this computer never recorded is exactly the one that
                        // strands.
                        ui.horizontal(|ui| {
                            ui.label(theme::title(ui, "Recent sessions"));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add_enabled(!sweep.busy, egui::Button::new("Stop strays"))
                                        .on_hover_text(
                                            "Find and stop every jamstream machine in the accounts \
                                         this computer can reach",
                                        )
                                        .clicked()
                                    {
                                        action = Some(HomeAction::Sweep);
                                    }
                                    if !recent.is_empty() && ui.button("Takes").clicked() {
                                        action = Some(HomeAction::Takes);
                                    }
                                },
                            );
                        });
                        // Above the rows, because the button that produces it is
                        // above them too and the list is long enough to carry a
                        // report off the bottom of the window.
                        sweep_report(ui, sweep);
                        if recent.is_empty() {
                            ui.label(theme::muted(
                                ui,
                                "No sessions yet; host one or paste an invite above.",
                            ));
                        }
                        // A record of what happened, not a list of things to press.
                        // These rows have no rejoin, no end, and no click, so they
                        // carry only two type treatments: a mono id, and everything
                        // else in one muted, proportional sentence. The id is the
                        // one thing here that is a number, so it keeps the
                        // monospace; everything else is a sentence about a session
                        // that is over.
                        // Every running session, however many there are: each
                        // one is a machine still being billed, and the way to
                        // stop it must not be behind a fold. Ended ones are a
                        // record, so they fill what is left of the window and
                        // the remainder is dropped with its count said out
                        // loud.
                        // A line of slack, because the count of what was
                        // dropped is itself a line and it has to land inside
                        // the window rather than one row past it.
                        let floor = ui.ctx().viewport_rect().bottom() - row_height(ui);
                        let (running, ended): (Vec<_>, Vec<_>) =
                            recent.iter().partition(|row| row.running);
                        for row in &running {
                            session_row(ui, row);
                        }
                        let mut drawn = 0;
                        for row in &ended {
                            if ui.cursor().top() + row_height(ui) > floor {
                                break;
                            }
                            session_row(ui, row);
                            drawn += 1;
                        }
                        let dropped = ended.len() - drawn;
                        if dropped > 0 {
                            ui.label(theme::muted(
                                ui,
                                format!("{dropped} older ended sessions are not shown."),
                            ));
                        }
                    });
                });
            });
        action
    }
}

/// One row: a mono id, then one muted sentence about a session that is over.
fn session_row(ui: &mut Ui, row: &RecentSession) {
    ui.horizontal(|ui| {
        ui.label(theme::mono_muted(ui, row.short_id.clone()));
        let status = if row.running { "running" } else { "ended" };
        ui.label(theme::muted(
            ui,
            format!("{} {}, {}", row.provider, row.region, status),
        ));
    });
}

/// What one row costs, so the fill can stop before it draws past the window.
fn row_height(ui: &Ui) -> f32 {
    ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y
}

/// What the last sweep did, above the rows it corrected.
///
/// Three registers, and the difference between them is the point. The
/// headline is what happened, the notes are what it tidied, and the warnings
/// are drawn in the danger colour because every one of them is a machine or
/// an account this sweep could not account for, which is a bill.
fn sweep_report(ui: &mut Ui, sweep: SweepView<'_>) {
    if sweep.busy {
        ui.add_space(theme::SPACE_SM);
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().color(theme::palette_of(ui).text_muted));
            ui.label(theme::muted(ui, "Searching every account for machines."));
        });
        return;
    }
    let Some(outcome) = sweep.outcome else {
        return;
    };
    ui.add_space(theme::SPACE_SM);
    ui.add(egui::Label::new(theme::muted(ui, outcome.summary())).wrap());
    for note in outcome.notes() {
        ui.add(egui::Label::new(theme::muted(ui, note)).wrap());
    }
    for warning in outcome.warnings() {
        theme::reason(ui, warning);
    }
}
