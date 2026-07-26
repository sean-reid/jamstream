//! The session screen: mixer strips on the left, chat on the right, and a
//! status bar where latency, cost, and connection quality live in the same
//! place every session. Below 900 px the chat collapses behind a toggle so
//! nothing overlaps.

use egui::{Align, Align2, Button, Layout, RichText, ScrollArea, TextEdit, Ui, vec2};

use crate::runtime::{Command, ConnState, MemberView, Role, Runtime, Snapshot};
use crate::theme;
use crate::widgets::{Meter, fader, meter, on_air, pan_slider, status_dot};

const NARROW_BELOW_PX: f32 = 900.0;

pub enum SessionEvent {
    /// The user confirmed leaving; the app should drop the runtime.
    Left,
}

#[derive(Default)]
pub struct SessionScreen {
    pub chat_input: String,
    pub confirm_leave: bool,
    /// Member pending revoke confirmation, with the display name.
    pub confirm_revoke: Option<(crate::runtime::MemberId, String)>,
    /// Narrow layout only: chat shown instead of the mixer.
    pub chat_open: bool,
}

impl SessionScreen {
    pub fn ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime) -> Option<SessionEvent> {
        let mut event = None;
        let narrow = ui.available_width() < NARROW_BELOW_PX;

        egui::Panel::bottom(egui::Id::new("session-status"))
            .show_separator_line(true)
            .show(ui, |ui| self.status_bar(ui, snap));

        if !narrow {
            egui::Panel::right(egui::Id::new("session-chat"))
                .resizable(false)
                .exact_size(280.0)
                .show(ui, |ui| self.chat_ui(ui, snap, rt));
            self.mixer_ui(ui, snap, rt);
        } else {
            ui.horizontal(|ui| {
                let label = if self.chat_open {
                    "Show mixer"
                } else {
                    "Show chat"
                };
                if ui.button(label).clicked() {
                    self.chat_open = !self.chat_open;
                }
            });
            if self.chat_open {
                self.chat_ui(ui, snap, rt);
            } else {
                self.mixer_ui(ui, snap, rt);
            }
        }

        self.confirm_windows(ui, rt, &mut event);
        event
    }

    fn mixer_ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime) {
        match &snap.stats.state {
            ConnState::Connecting => {
                ui.label(theme::muted(
                    ui,
                    format!("connecting to {}", snap.server_addr),
                ));
            }
            ConnState::Ejected(reason) => {
                let p = theme::palette_of(ui);
                ui.label(
                    RichText::new(format!("removed from the session: {reason}")).color(p.danger),
                );
            }
            ConnState::TimedOut => {
                let p = theme::palette_of(ui);
                ui.label(
                    RichText::new("connection lost: no packets for 10 seconds").color(p.danger),
                );
            }
            ConnState::Joined | ConnState::Idle => {}
        }

        ScrollArea::vertical()
            .id_salt("mixer-scroll")
            .show(ui, |ui| {
                ui.add_space(theme::SPACE_SM);
                ui.horizontal_top(|ui| {
                    for member in snap.members.iter().filter(|m| m.role == Role::Musician) {
                        self.strip_ui(ui, member, snap, rt);
                    }
                });
                ui.add_space(theme::SPACE_SM);
                let listeners: Vec<&MemberView> = snap
                    .members
                    .iter()
                    .filter(|m| m.role == Role::Listener && m.connected)
                    .collect();
                let line = match listeners.len() {
                    0 => "no listeners connected".to_owned(),
                    1 => format!("1 listener connected: {}", listeners[0].name),
                    n => format!(
                        "{n} listeners connected: {}",
                        listeners
                            .iter()
                            .map(|l| l.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                };
                ui.label(theme::muted(ui, line));
                ui.add_space(theme::SPACE_SM);
                self.metronome_ui(ui, snap, rt);
            });
    }

    fn strip_ui(&mut self, ui: &mut Ui, member: &MemberView, snap: &Snapshot, rt: &dyn Runtime) {
        theme::panel(ui).show(ui, |ui| {
            ui.set_width(96.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    status_dot(ui, member.connected, snap.stats.rtt_ms, snap.stats.loss_pct);
                    ui.label(RichText::new(member.name.clone()).strong());
                    if member.is_you {
                        ui.label(theme::muted(ui, "you"));
                    }
                });
                if !member.connected {
                    ui.label(theme::muted(ui, "disconnected"));
                }

                let mut gain = member.fader.gain_db;
                let mut pan = member.fader.pan;
                let mut muted = member.fader.muted;
                let mut changed = false;

                if member.is_you {
                    // Your uplink has no fader; monitoring yourself happens
                    // locally on your interface, not through the server mix.
                    ui.add_enabled_ui(false, |ui| {
                        fader(ui, &format!("{} fader", member.name), &mut gain);
                    });
                    ui.label(theme::mono_muted(ui, format!("{gain:+.1} dB")));
                    ui.label(theme::muted(ui, "self monitoring is local"));
                } else {
                    changed |= fader(ui, &format!("{} fader", member.name), &mut gain).changed();
                    ui.label(theme::mono(ui, format!("{gain:+.1} dB")));
                    changed |= pan_slider(ui, &format!("{} pan", member.name), &mut pan).changed();
                    let mute_label = if muted { "Unmute" } else { "Mute" };
                    if ui
                        .add(Button::new(mute_label).min_size(vec2(60.0, 0.0)))
                        .clicked()
                    {
                        muted = !muted;
                        changed = true;
                    }
                    if snap.is_host && ui.button("Revoke").clicked() {
                        self.confirm_revoke = Some((member.id, member.name.clone()));
                    }
                }
                if changed {
                    rt.send(Command::SetFader {
                        member: member.id,
                        gain_db: gain,
                        pan,
                        muted,
                    });
                }
            });
        });
    }

    fn metronome_ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime) {
        theme::panel(ui).show(ui, |ui| {
            ui.set_width(280.0);
            ui.label("Metronome");
            let m = snap.metronome;
            if snap.is_host {
                let mut bpm = m.bpm;
                let mut beats = m.beats_per_bar;
                let mut enabled = m.enabled;
                let mut changed = false;
                ui.horizontal(|ui| {
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut bpm)
                                .range(30..=300)
                                .suffix(" bpm"),
                        )
                        .changed();
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut beats)
                                .range(1..=16)
                                .suffix(" beats per bar"),
                        )
                        .changed();
                    changed |= ui.checkbox(&mut enabled, "enabled").changed();
                });
                if changed {
                    rt.send(Command::SetMetronome {
                        bpm,
                        beats_per_bar: beats,
                        enabled,
                    });
                }
            } else {
                let state = if m.enabled { "on" } else { "off" };
                ui.label(theme::mono_muted(
                    ui,
                    format!("{} bpm, {} beats per bar, {state}", m.bpm, m.beats_per_bar),
                ));
            }
            let mut hear = m.you_hear_click;
            if ui.checkbox(&mut hear, "hear the click").changed() {
                rt.send(Command::SetClick(hear));
            }
        });
    }

    fn chat_ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime) {
        ui.label("Chat");
        let input_height = 28.0;
        let list_height = (ui.available_height() - input_height).max(0.0);
        ScrollArea::vertical()
            .id_salt("chat-scroll")
            .max_height(list_height)
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &snap.chat {
                    ui.horizontal_wrapped(|ui| {
                        let secs = line.at_ms / 1000;
                        ui.label(theme::mono_muted(
                            ui,
                            format!("{:02}:{:02}", secs / 60, secs % 60),
                        ));
                        ui.label(RichText::new(line.from_name.clone()).strong());
                        ui.label(line.text.clone());
                    });
                }
            });
        let field = ui.add(
            TextEdit::singleline(&mut self.chat_input)
                .desired_width(f32::INFINITY)
                .hint_text("message"),
        );
        let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if submitted && !self.chat_input.trim().is_empty() {
            rt.send(Command::SendChat(self.chat_input.trim().to_owned()));
            self.chat_input.clear();
            field.request_focus();
        }
    }

    /// Everything numeric here is monospace so the bar never wobbles.
    fn status_bar(&mut self, ui: &mut Ui, snap: &Snapshot) {
        ui.add_space(theme::SPACE_SM);
        ui.horizontal(|ui| {
            let s = &snap.stats;
            status_dot(
                ui,
                matches!(s.state, ConnState::Joined),
                s.rtt_ms,
                s.loss_pct,
            );
            // Mouth to ear is the headline number.
            let m2e = s
                .mouth_to_ear_ms
                .map_or("--".to_owned(), |v| format!("{v:.1} ms"));
            ui.label(
                RichText::new(m2e)
                    .monospace()
                    .size(15.0)
                    .color(theme::palette_of(ui).text_primary),
            );
            ui.label(theme::muted(ui, "mouth to ear"));
            ui.separator();
            let rtt = s.rtt_ms.map_or("--".to_owned(), |v| format!("{v:.1}"));
            ui.label(theme::mono(ui, format!("rtt {rtt} ms")));
            ui.label(theme::mono(
                ui,
                format!("buffer {}/{}", s.jitter_depth, s.jitter_target),
            ));
            ui.label(theme::mono(ui, format!("loss {:.1}%", s.loss_pct)));
            // The compact meters are the first thing to go when the bar
            // gets tight; nothing may overlap.
            if ui.available_width() > 420.0 {
                ui.separator();
                ui.label(theme::muted(ui, "in"));
                meter(
                    ui,
                    "bar-in",
                    snap.levels.input_peak,
                    snap.levels.input_rms,
                    vec2(52.0, 10.0),
                    Meter::Horizontal,
                );
                ui.label(theme::muted(ui, "out"));
                meter(
                    ui,
                    "bar-out",
                    snap.levels.output_peak,
                    snap.levels.output_rms,
                    vec2(52.0, 10.0),
                    Meter::Horizontal,
                );
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let p = theme::palette_of(ui);
                if ui
                    .add(
                        Button::new(RichText::new("Leave").color(egui::Color32::WHITE))
                            .fill(p.danger),
                    )
                    .clicked()
                {
                    self.confirm_leave = true;
                }
                if let Some(cost) = &snap.cost {
                    ui.label(theme::mono(
                        ui,
                        format!("{} so far", theme::microusd(cost.accrued_microusd)),
                    ));
                    ui.label(theme::mono_muted(
                        ui,
                        format!(
                            "{:02}:{:02}:{:02}",
                            cost.elapsed_secs / 3600,
                            (cost.elapsed_secs / 60) % 60,
                            cost.elapsed_secs % 60
                        ),
                    ));
                }
                ui.label(theme::mono_muted(ui, snap.session_short.clone()));
                // Reserved for broadcast (M2); always dark in v1.
                on_air(ui, false);
            });
        });
        ui.add_space(theme::SPACE_SM);
    }

    fn confirm_windows(&mut self, ui: &mut Ui, rt: &dyn Runtime, event: &mut Option<SessionEvent>) {
        if self.confirm_leave {
            egui::Window::new("Leave session")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .show(ui.ctx(), |ui| {
                    ui.label(
                        "Leave this session? The server keeps running until the host ends it.",
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.confirm_leave = false;
                        }
                        let p = theme::palette_of(ui);
                        if ui
                            .add(
                                Button::new(
                                    RichText::new("Leave session").color(egui::Color32::WHITE),
                                )
                                .fill(p.danger),
                            )
                            .clicked()
                        {
                            rt.send(Command::Leave);
                            self.confirm_leave = false;
                            *event = Some(SessionEvent::Left);
                        }
                    });
                });
        }
        if let Some((member_id, name)) = self.confirm_revoke.clone() {
            egui::Window::new("Revoke invite")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .show(ui.ctx(), |ui| {
                    ui.label(format!(
                        "Revoke {name}'s invite? They will be disconnected and their invite stops working."
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.confirm_revoke = None;
                        }
                        let p = theme::palette_of(ui);
                        if ui
                            .add(
                                Button::new(RichText::new("Revoke invite").color(egui::Color32::WHITE))
                                    .fill(p.danger),
                            )
                            .clicked()
                        {
                            // Token comes from the current snapshot; host only.
                            let token = rt
                                .snapshot()
                                .members
                                .iter()
                                .find(|m| m.id == member_id)
                                .and_then(|m| m.token);
                            if let Some(token) = token {
                                rt.send(Command::Revoke(token));
                            }
                            self.confirm_revoke = None;
                        }
                    });
                });
        }
    }
}
