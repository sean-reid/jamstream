//! The session screen: mixer strips on the left, chat on the right, and a
//! status bar where latency, cost, and connection quality live in the same
//! place every session. Below 900 px the chat collapses behind a toggle so
//! nothing overlaps.

use egui::{
    Align, Align2, Button, CornerRadius, Layout, RichText, ScrollArea, Sense, Stroke, TextEdit, Ui,
    vec2,
};

use crate::runtime::{
    BroadcastView, ChatLine, Command, ConnState, FaderView, MemberView, Role, Runtime, Snapshot,
};
use crate::screens::destinations::{DestinationsPanel, on_air_indicator};
use crate::screens::invites::{InvitesEvent, InvitesPanel};
use crate::theme;
use crate::widgets::{
    AVATAR_D_STRIP, Meter, avatar_disc, db_drag, fader, lamp_toggle, meter, on_air, pan_slider,
    status_dot,
};

const NARROW_BELOW_PX: f32 = 900.0;

/// A host's bar stacks into two rows below this. The one-row form needs the
/// readouts without their meters, 425, beside the full set of controls,
/// 655: three sheet toggles, the lamp, the session id, the timer, the cost,
/// and Leave.
const BAR_STACK_BELOW_PX: f32 = 1100.0;

/// What the pair of compact meters needs beside the readouts: two 52 px
/// meters, their labels, the separator, and the gaps between them.
const BAR_METERS_W: f32 = 180.0;

// Console geometry: every strip is exactly this wide regardless of content.
const STRIP_W: f32 = 104.0;
const STRIP_INNER_W: f32 = STRIP_W - 20.0;
const STRIP_GAP: f32 = 8.0;
const ROW_H: f32 = 22.0;
const DB_H: f32 = 16.0;
const PAN_H: f32 = 14.0;
const METER_SLOT_H: f32 = 12.0;
const NAME_ROW_H: f32 = 18.0;
/// The "disconnected" note; only a disconnected member's strip carries it.
const NOTE_ROW_H: f32 = 18.0;
/// Floor on the fader. The console reserves this much per strip before it
/// draws and scrolls when the window cannot hold it, so a fader is never
/// handed less and never takes the difference out of the portrait and the
/// name above it. A host strip clears the floor at the smallest window the
/// app opens at, 800x600; scrolling is for the sizes below that.
const MIN_FADER_H: f32 = 32.0;
/// The panel primitive's 10 px margins around the strip's content.
const STRIP_FRAME_H: f32 = 20.0;

// Chat columns: a monospace clock, a name gutter, then the message. The
// message column is the one thing that must never move: every line of a
// message, wrapped continuations included, shares its left edge, in both
// the wide and the narrow layout. Names past the gutter ellipsize with the
// full name one hover away, the same treatment as a strip's name.
const CHAT_TIME_W: f32 = 38.0;
const CHAT_NAME_W: f32 = 64.0;

pub enum SessionEvent {
    /// The user confirmed leaving; the app should drop the runtime.
    Left,
    /// Host only: leave, destroy the server, and mark the session ended.
    EndSession,
}

#[derive(Default)]
pub struct SessionScreen {
    pub chat_input: String,
    pub confirm_leave: bool,
    /// Member pending revoke confirmation, with the display name.
    pub confirm_revoke: Option<(crate::runtime::MemberId, String)>,
    /// Narrow layout only: chat shown instead of the mixer.
    pub chat_open: bool,
    /// Host sessions launched by this app carry the invite book; plain
    /// joins have none and show no panel.
    pub invites: Option<InvitesPanel>,
    pub invites_open: bool,
    /// Host only: the stream mix sheet. Snapshots without a broadcast view
    /// never show the toggle, so this stays false for everyone else.
    pub broadcast_open: bool,
    /// Host sessions this app launched carry the destinations sheet; it needs
    /// the credential store, so plain joins have none and show no toggle,
    /// exactly like the invites panel.
    pub destinations: Option<DestinationsPanel>,
    pub destinations_open: bool,
}

impl SessionScreen {
    pub fn ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime) -> Option<SessionEvent> {
        let mut event = None;
        let narrow = ui.available_width() < NARROW_BELOW_PX;

        // Escape always steps back out of the innermost entered state, so
        // nothing on this screen can trap the user.
        let escape = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if escape {
            let panel_confirming = self
                .invites
                .as_ref()
                .is_some_and(|p| p.confirm_revoke.is_some() || p.confirm_end);
            if self.confirm_leave {
                self.confirm_leave = false;
            } else if self.confirm_revoke.is_some() {
                self.confirm_revoke = None;
            } else if panel_confirming {
                if let Some(panel) = &mut self.invites {
                    panel.confirm_revoke = None;
                    panel.confirm_end = false;
                }
            } else if self.broadcast_open {
                // Closing the sheet is navigation; audition keeps playing
                // until it is switched off.
                self.broadcast_open = false;
            } else if self.destinations_open {
                // Same rule: closing the sheet is navigation and never takes
                // the session off air.
                self.destinations_open = false;
            } else if self.invites_open {
                self.invites_open = false;
            } else if narrow && self.chat_open {
                self.chat_open = false;
            }
        }

        egui::Panel::bottom(egui::Id::new("session-status"))
            .show_separator_line(true)
            .show(ui, |ui| self.status_bar(ui, snap));

        if !narrow {
            egui::Panel::right(egui::Id::new("session-chat"))
                .resizable(false)
                .exact_size(280.0)
                .show(ui, |ui| self.chat_ui(ui, snap, rt, true));
            self.mixer_ui(ui, snap, rt);
        } else {
            // The toggle is symmetric and stationary: same place, same
            // label, state shown by the active fill. One click back to the
            // faders, always.
            ui.horizontal(|ui| {
                if ui
                    .add(Button::new("Chat").selected(self.chat_open))
                    .clicked()
                {
                    self.chat_open = !self.chat_open;
                }
            });
            if self.chat_open {
                self.chat_ui(ui, snap, rt, false);
            } else {
                self.mixer_ui(ui, snap, rt);
            }
        }

        if self.invites_open
            && snap.is_host
            && let Some(panel) = &mut self.invites
            && let Some(InvitesEvent::EndSession) = panel.ui(ui, snap, rt, &mut self.invites_open)
        {
            event = Some(SessionEvent::EndSession);
        }

        if self.broadcast_open && snap.broadcast.is_some() {
            self.stream_mix_ui(ui, snap, rt);
        }

        if self.destinations_open
            && snap.is_host
            && let Some(panel) = &mut self.destinations
        {
            panel.ui(ui, snap, rt, &mut self.destinations_open);
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

        let musicians: Vec<MemberView> = snap
            .members
            .iter()
            .filter(|m| m.role == Role::Musician)
            .cloned()
            .collect();
        // Outer strip width includes the 1 px frame stroke on each side.
        let n = musicians.len() as f32;
        let row_w = n * (STRIP_W + 2.0) + (n - 1.0).max(0.0) * STRIP_GAP;
        // The console extends sideways past the window; the lower panel
        // stays within it.
        let visible_w = ui.available_width();
        let lower_w = row_w.min(visible_w);
        let overflow = row_w > visible_w;

        // Listeners, the self-monitoring note, and the metronome live under
        // the strips; the strips own the rest of the vertical space.
        egui::Panel::bottom(egui::Id::new("mixer-lower"))
            .show_separator_line(false)
            .frame(egui::Frame::new())
            .show(ui, |ui| {
                ui.add_space(theme::SPACE_SM);
                let listeners: Vec<&MemberView> = snap
                    .members
                    .iter()
                    .filter(|m| m.role == Role::Listener && m.connected)
                    .collect();
                let names = listeners
                    .iter()
                    .map(|l| l.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let line = match listeners.len() {
                    0 => "no listeners connected".to_owned(),
                    1 => format!("1 listener connected: {names}"),
                    n => format!("{n} listeners connected: {names}"),
                };
                // One quiet line; the full roster lives in the tooltip.
                ui.set_max_width(lower_w);
                let response = ui.add(egui::Label::new(theme::muted(ui, line)).truncate());
                if listeners.len() > 1 {
                    response.on_hover_text(names);
                }
                ui.add_space(theme::SPACE_SM);
                self.metronome_ui(ui, snap, rt, lower_w);
            });

        // The console scrolls in both directions: sideways past the last
        // strip, and down when the window is too short for a whole strip.
        ScrollArea::both().id_salt("mixer-scroll").show(ui, |ui| {
            // Leave room for the scrollbar when the row overflows.
            let bar = if overflow { 10.0 } else { 2.0 };
            let gap = ui.spacing().item_spacing.y;
            // Every strip is as tall as the tallest one needs to be, so
            // the rows line up across the console.
            let needed = musicians
                .iter()
                .map(|m| strip_content_h(gap, m, snap.is_host) + STRIP_FRAME_H)
                .fold(0.0_f32, f32::max);
            let strip_h = (ui.available_height() - bar).max(needed);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = STRIP_GAP;
                for member in &musicians {
                    self.strip_ui(ui, member, snap, rt, strip_h);
                }
            });
        });
    }

    fn strip_ui(
        &mut self,
        ui: &mut Ui,
        member: &MemberView,
        snap: &Snapshot,
        rt: &dyn Runtime,
        strip_h: f32,
    ) {
        let frame = theme::panel(ui).show(ui, |ui| {
            // An exact box, not a minimum: the rows inside stack from the
            // bottom, so they need a bottom edge that does not move.
            ui.allocate_ui_with_layout(
                vec2(STRIP_INNER_W, (strip_h - STRIP_FRAME_H).max(0.0)),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(STRIP_INNER_W);
                    self.strip_body(ui, member, snap, rt);
                },
            );
        });
        if member.is_you {
            frame
                .response
                .on_hover_text("your own channel: self monitoring is local, not part of the mix");
        }
    }

    fn strip_body(&mut self, ui: &mut Ui, member: &MemberView, snap: &Snapshot, rt: &dyn Runtime) {
        // The portrait sits above the name row, centered in the strip. Its
        // slot is allocated whether or not an avatar has arrived, so a
        // picture landing mid-session moves nothing below it.
        ui.allocate_ui_with_layout(
            vec2(STRIP_INNER_W, AVATAR_D_STRIP),
            Layout::top_down(Align::Center),
            |ui| {
                avatar_disc(
                    ui,
                    &member.name,
                    member.avatar.as_ref(),
                    AVATAR_D_STRIP,
                    !member.connected,
                )
                .on_hover_text(member.name.clone());
            },
        );
        ui.horizontal(|ui| {
            status_dot(ui, member.connected, snap.stats.rtt_ms, snap.stats.loss_pct);
            // Long names truncate inside the fixed strip; the full name is
            // one hover away.
            // Reserve room for the tag plus item spacing so the "you"
            // strip stays exactly as wide as every other strip.
            let you_w = if member.is_you { 34.0 } else { 0.0 };
            let name_w = (ui.available_width() - you_w).max(10.0);
            let response = ui
                .allocate_ui_with_layout(
                    vec2(name_w, NAME_ROW_H),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.set_min_width(name_w);
                        ui.add(
                            egui::Label::new(RichText::new(member.name.clone()).strong())
                                .truncate(),
                        )
                    },
                )
                .inner;
            if member.name.chars().count() > 8 {
                response.on_hover_text(member.name.clone());
            }
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
        let label = format!("{} fader", member.name);

        // Fixed rows stack from the bottom so their positions are identical
        // in every strip; the fader takes the exact remainder.
        ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
            meter_slot(ui);
            if snap.is_host {
                if member.is_you {
                    // Reserves the revoke row so slots align across strips.
                    ui.allocate_exact_size(vec2(STRIP_INNER_W, ROW_H), Sense::hover());
                } else if ui
                    .add_sized(vec2(STRIP_INNER_W, ROW_H), Button::new("Revoke"))
                    .clicked()
                {
                    self.confirm_revoke = Some((member.id, member.name.clone()));
                }
            }
            if member.is_you {
                // Your uplink has no fader; monitoring yourself happens
                // locally on your interface, not through the server mix.
                ui.add_enabled_ui(false, |ui| {
                    mute_button(ui, &mut muted, STRIP_INNER_W, MUTE_MONITOR_HOVER);
                    pan_row(ui, &format!("{} pan", member.name), &mut pan);
                    db_readout(ui, gain, false);
                    let fader_h = (ui.available_height() - 2.0).max(0.0);
                    fader(ui, &label, &mut gain, vec2(STRIP_INNER_W, fader_h))
                        .on_disabled_hover_text(
                            "your own channel: self monitoring is local, not part of the mix",
                        );
                });
            } else {
                if mute_button(ui, &mut muted, STRIP_INNER_W, MUTE_MONITOR_HOVER) {
                    changed = true;
                }
                changed |= pan_row(ui, &format!("{} pan", member.name), &mut pan);
                db_readout(ui, gain, true);
                let fader_h = (ui.available_height() - 2.0).max(0.0);
                changed |= fader(ui, &label, &mut gain, vec2(STRIP_INNER_W, fader_h)).changed();
            }
        });
        if changed {
            rt.send(Command::SetFader {
                member: member.id,
                gain_db: gain,
                pan,
                muted,
            });
        }
    }

    /// The host's broadcast mix sheet: one compact row per musician, the
    /// host's own channel included (listeners hear it too), plus the
    /// audition switch. Deliberately unlike the monitor strips: horizontal
    /// rows on a sheet, so the two mixes are never mistaken for each other.
    fn stream_mix_ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime) {
        let Some(broadcast) = &snap.broadcast else {
            return;
        };
        let panel = {
            let p = theme::palette_of(ui);
            egui::Frame::new()
                .fill(p.surface1)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(CornerRadius::same(theme::RADIUS))
                .inner_margin(egui::Margin::same(14))
        };
        egui::Window::new("Stream mix")
            .title_bar(false)
            .frame(panel)
            .anchor(Align2::RIGHT_TOP, vec2(-10.0, 56.0))
            .fixed_size(vec2(384.0, 0.0))
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.horizontal(|ui| {
                    ui.label(theme::title(ui, "Stream mix"));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            self.broadcast_open = false;
                        }
                    });
                });
                ui.label(theme::muted(
                    ui,
                    "What listeners and the stream hear. Your monitor mix is unaffected.",
                ));
                ui.add_space(theme::SPACE_SM);
                egui::Grid::new("stream-mix-grid")
                    .num_columns(4)
                    .spacing(vec2(theme::SPACE_MD, 4.0))
                    .show(ui, |ui| {
                        for member in snap.members.iter().filter(|m| m.role == Role::Musician) {
                            stream_mix_row(ui, member, broadcast, rt);
                            ui.end_row();
                        }
                    });
                ui.add_space(theme::SPACE_MD);
                ui.separator();
                let lit = broadcast.audition;
                let response = lamp_toggle(ui, "audition stream mix", lit).on_hover_text(if lit {
                    "you are hearing the stream mix; switch off to get your monitor mix back"
                } else {
                    "swap your monitor for the exact mix listeners hear, your own voice included"
                });
                if response.clicked() {
                    rt.send(Command::SetBroadcastAudition(!lit));
                }
                ui.label(theme::muted(
                    ui,
                    "While on, your monitor is the stream mix, your own voice included.",
                ));
            });
    }

    fn metronome_ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime, row_w: f32) {
        theme::panel(ui).show(ui, |ui| {
            // Margins and stroke add 22; the panel's outer edge lines up
            // with the strip row above it.
            ui.set_width((row_w - 22.0).max(240.0));
            ui.label(theme::title(ui, "Metronome"));
            ui.add_space(theme::SPACE_XS);
            let m = snap.metronome;
            if snap.is_host {
                let mut bpm = m.bpm;
                let mut beats = m.beats_per_bar;
                let mut enabled = m.enabled;
                let mut changed = false;
                egui::Grid::new("metronome-grid")
                    .num_columns(2)
                    .spacing(vec2(theme::SPACE_LG, 4.0))
                    .show(ui, |ui| {
                        ui.label(theme::muted(ui, "tempo"));
                        changed |=
                            theme::mono_drag(ui, egui::DragValue::new(&mut bpm).range(30..=300))
                                .changed();
                        ui.end_row();
                        ui.label(theme::muted(ui, "beats per bar"));
                        changed |=
                            theme::mono_drag(ui, egui::DragValue::new(&mut beats).range(1..=16))
                                .changed();
                        ui.end_row();
                        ui.label(theme::muted(ui, "click"));
                        changed |= ui.checkbox(&mut enabled, "enabled").changed();
                        ui.end_row();
                    });
                if changed {
                    rt.send(Command::SetMetronome {
                        bpm,
                        beats_per_bar: beats,
                        enabled,
                    });
                }
            } else {
                egui::Grid::new("metronome-grid")
                    .num_columns(2)
                    .spacing(vec2(theme::SPACE_LG, 4.0))
                    .show(ui, |ui| {
                        ui.label(theme::muted(ui, "tempo"));
                        ui.label(theme::mono(ui, format!("{}", m.bpm)));
                        ui.end_row();
                        ui.label(theme::muted(ui, "beats per bar"));
                        ui.label(theme::mono(ui, format!("{}", m.beats_per_bar)));
                        ui.end_row();
                        ui.label(theme::muted(ui, "click"));
                        ui.label(theme::mono(ui, if m.enabled { "on" } else { "off" }));
                        ui.end_row();
                    });
            }
            let mut hear = m.you_hear_click;
            if ui.checkbox(&mut hear, "hear the click").changed() {
                rt.send(Command::SetClick(hear));
            }
        });
    }

    fn chat_ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime, titled: bool) {
        // In the narrow layout the toggle already says where you are.
        if titled {
            ui.label(theme::title(ui, "Chat"));
        }
        let input_height = 28.0;
        let list_height = (ui.available_height() - input_height).max(0.0);
        ScrollArea::vertical()
            .id_salt("chat-scroll")
            .max_height(list_height)
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &snap.chat {
                    chat_line(ui, line);
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
    ///
    /// A host's bar carries three sheet toggles, the cost ticker, and the
    /// timer on top of everything a musician sees, and this bar may never
    /// overlap. Two rules keep it apart: below [`BAR_STACK_BELOW_PX`] a
    /// host's controls take a row of their own, and on one row the controls
    /// are measured first so the readouts get only what is left over.
    fn status_bar(&mut self, ui: &mut Ui, snap: &Snapshot) {
        ui.add_space(theme::SPACE_SM);
        if snap.is_host && ui.available_width() < BAR_STACK_BELOW_PX {
            ui.horizontal(|ui| status_readouts(ui, snap));
            ui.add_space(theme::SPACE_SM);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.status_controls(ui, snap);
                });
            });
        } else {
            egui::containers::Sides::new().shrink_left().show(
                ui,
                |ui| status_readouts(ui, snap),
                |ui| self.status_controls(ui, snap),
            );
        }
        ui.add_space(theme::SPACE_SM);
    }

    /// The control half: the three host sheets, the cost ticker, the on air
    /// lamp, and the way out. Laid out right to left by the caller.
    fn status_controls(&mut self, ui: &mut Ui, snap: &Snapshot) {
        {
            {
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
                if snap.is_host
                    && self.invites.is_some()
                    && ui
                        .add(Button::new("Invites").selected(self.invites_open))
                        .clicked()
                {
                    self.invites_open = !self.invites_open;
                    // The host sheets share one anchor; only one is ever
                    // open.
                    if self.invites_open {
                        self.broadcast_open = false;
                        self.destinations_open = false;
                    }
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
                // The lamp everyone in the room can see, host or not.
                on_air(ui, snap.stream.on_air());
                // Both toggles sit with the lamp: they are about what leaves
                // the session, not what anyone monitors.
                if snap.is_host
                    && self.destinations.is_some()
                    && ui
                        .add(Button::new("Destinations").selected(self.destinations_open))
                        .clicked()
                {
                    self.destinations_open = !self.destinations_open;
                    if self.destinations_open {
                        self.invites_open = false;
                        self.broadcast_open = false;
                    }
                }
                if snap.broadcast.is_some()
                    && ui
                        .add(Button::new("Stream mix").selected(self.broadcast_open))
                        .clicked()
                {
                    self.broadcast_open = !self.broadcast_open;
                    if self.broadcast_open {
                        self.invites_open = false;
                        self.destinations_open = false;
                    }
                }
            }
        }
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
                                // Keep the invites panel's local record in
                                // step with strip-side revocations.
                                if let Some(panel) = &mut self.invites {
                                    panel.mark_revoked(token);
                                }
                            }
                            self.confirm_revoke = None;
                        }
                    });
                });
        }
    }
}

/// One compact broadcast row: name, dB drag-value, pan, mute. Everything
/// is enabled, the host's own channel included; listeners hear that too.
/// Any change sends the row's full fader state.
fn stream_mix_row(ui: &mut Ui, member: &MemberView, broadcast: &BroadcastView, rt: &dyn Runtime) {
    const NAME_W: f32 = 116.0;
    let view = broadcast
        .faders
        .iter()
        .find(|(id, _)| *id == member.id)
        .map_or(
            FaderView {
                gain_db: 0.0,
                pan: 0.0,
                muted: false,
            },
            |(_, f)| *f,
        );

    ui.allocate_ui_with_layout(
        vec2(NAME_W, ROW_H),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_width(NAME_W);
            let you_w = if member.is_you { 34.0 } else { 0.0 };
            let name_w = (NAME_W - you_w).max(10.0);
            let response = ui
                .allocate_ui_with_layout(
                    vec2(name_w, 18.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(member.name.clone()).strong())
                                .truncate(),
                        )
                    },
                )
                .inner;
            if member.name.chars().count() > 12 {
                response.on_hover_text(member.name.clone());
            }
            if member.is_you {
                ui.label(theme::muted(ui, "you"));
            }
        },
    );

    let mut gain = view.gain_db;
    let mut pan = view.pan;
    let mut muted = view.muted;
    let mut changed = false;
    changed |= db_drag(
        ui,
        &format!("{} stream gain", member.name),
        &mut gain,
        vec2(68.0, ROW_H),
    )
    .changed();
    changed |= pan_slider(ui, &format!("{} stream pan", member.name), &mut pan).changed();
    changed |= mute_button(ui, &mut muted, 52.0, MUTE_STREAM_HOVER);
    if changed {
        rt.send(Command::SetBroadcastFader {
            member: member.id,
            gain_db: gain,
            pan,
            muted,
        });
    }
}

/// One chat line in three columns: clock, name gutter, message. The message
/// is a wrapping label in the width that remains, so its continuation lines
/// hang to the same left edge instead of sliding under the clock.
fn chat_line(ui: &mut Ui, line: &ChatLine) {
    ui.horizontal_top(|ui| {
        let secs = line.at_ms / 1000;
        ui.add_sized(
            vec2(CHAT_TIME_W, 18.0),
            egui::Label::new(theme::mono_muted(
                ui,
                format!("{:02}:{:02}", secs / 60, secs % 60),
            ))
            .selectable(false),
        );
        let name = ui
            .allocate_ui_with_layout(
                vec2(CHAT_NAME_W, 18.0),
                Layout::left_to_right(Align::Min),
                |ui| {
                    ui.set_min_width(CHAT_NAME_W);
                    ui.add(
                        egui::Label::new(RichText::new(line.from_name.clone()).strong()).truncate(),
                    )
                },
            )
            .inner;
        if line.from_name.chars().count() > 8 {
            name.on_hover_text(line.from_name.clone());
        }
        ui.add(egui::Label::new(line.text.clone()).wrap());
    });
}

/// The instrument half: connection, the headline latency, what is leaving
/// the room, and the link numbers.
fn status_readouts(ui: &mut Ui, snap: &Snapshot) {
    {
        let s = &snap.stats;
        status_dot(
            ui,
            matches!(s.state, ConnState::Joined),
            s.rtt_ms,
            s.loss_pct,
        );
        // Mouth to ear is the headline number: an instrument readout,
        // fixed-width digits so nothing wobbles.
        let p = theme::palette_of(ui);
        let m2e = s
            .mouth_to_ear_ms
            .map_or("--.-".to_owned(), |v| format!("{v:>4.1}"));
        ui.label(
            RichText::new(m2e)
                .monospace()
                .size(21.0)
                .color(p.text_primary),
        );
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(
                RichText::new("ms")
                    .monospace()
                    .size(9.5)
                    .color(p.text_muted),
            );
            ui.label(RichText::new("mouth to ear").size(9.5).color(p.text_muted));
        });
        // The audition reminder lives beside the headline readout so
        // the host can never forget what they are hearing.
        if snap.broadcast.as_ref().is_some_and(|b| b.audition) {
            audition_indicator(ui);
        }
        // Same place, same reason: what is leaving the room, for
        // everyone in it, whether or not a sheet is open.
        on_air_indicator(ui, snap);
        ui.separator();
        let rtt = s.rtt_ms.map_or("--".to_owned(), |v| format!("{v:.1}"));
        ui.label(theme::mono(ui, format!("rtt {rtt} ms")));
        ui.label(theme::mono(
            ui,
            format!("buffer {}/{}", s.jitter_depth, s.jitter_target),
        ));
        ui.label(theme::mono(ui, format!("loss {:.1}%", s.loss_pct)));
        // The compact meters are the first thing to go when the bar
        // gets tight. What is left here is the room the controls did
        // not take, so this is the real question: do the meters fit
        // beside everything else, or not.
        if ui.available_width() > BAR_METERS_W {
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
    }
}

/// The persistent audition reminder: a lit lamp and its sentence, beside
/// the mouth-to-ear readout for as long as audition is on.
fn audition_indicator(ui: &mut Ui) {
    let p = theme::palette_of(ui);
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        let (rect, _) = ui.allocate_exact_size(vec2(9.0, 16.0), Sense::hover());
        if ui.is_rect_visible(rect) {
            ui.painter().circle(
                egui::pos2(rect.left() + 4.0, rect.center().y),
                4.0,
                p.accent,
                Stroke::new(1.0, theme::blend(p.accent, p.text_primary, 0.45)),
            );
        }
        ui.label(theme::muted(ui, "hearing stream mix"))
            .on_hover_text("audition is on: your monitor carries what listeners hear");
    });
}

/// What one strip's content needs vertically: the portrait, the name row,
/// the disconnected note when there is one, every fixed row, and a fader
/// that is still a fader. Items are separated by `gap`, so the count of
/// them decides the spacing.
///
/// The mixer reserves this before drawing, because the lower rows stack
/// from the bottom edge upward: a fader handed less than it asked for used
/// to run its track back up through the name and the portrait.
fn strip_content_h(gap: f32, member: &MemberView, is_host: bool) -> f32 {
    let mut rows = AVATAR_D_STRIP + NAME_ROW_H + MIN_FADER_H + DB_H + PAN_H + ROW_H + METER_SLOT_H;
    let mut count = 7.0;
    if !member.connected {
        rows += NOTE_ROW_H;
        count += 1.0;
    }
    if is_host {
        rows += ROW_H;
        count += 1.0;
    }
    rows + (count - 1.0) * gap
}

/// Fixed-width monospace dB readout; the width never shifts with digits.
fn db_readout(ui: &mut Ui, gain_db: f32, primary: bool) {
    let text = if gain_db <= -59.95 {
        "-inf dB".to_owned()
    } else {
        format!("{gain_db:+.1} dB")
    };
    let rich = if primary {
        theme::mono(ui, text)
    } else {
        theme::mono_muted(ui, text)
    };
    ui.add_sized(
        vec2(STRIP_INNER_W, DB_H),
        egui::Label::new(rich).selectable(false),
    );
}

/// Pan slider centered in the strip; returns whether it changed.
fn pan_row(ui: &mut Ui, label: &str, pan: &mut f32) -> bool {
    let mut changed = false;
    ui.allocate_ui_with_layout(
        vec2(STRIP_INNER_W, PAN_H),
        Layout::top_down(Align::Center),
        |ui| {
            changed = pan_slider(ui, label, pan).changed();
        },
    );
    changed
}

/// Hover wording per mix; index 0 while muted, 1 while live.
const MUTE_MONITOR_HOVER: [&str; 2] = ["muted in your monitor mix", "mute in your monitor mix"];
const MUTE_STREAM_HOVER: [&str; 2] = [
    "muted for listeners and the stream",
    "mute for listeners and the stream",
];

/// Fixed-width mute button; state is shown by fill, the label never moves.
fn mute_button(ui: &mut Ui, muted: &mut bool, width: f32, hover: [&str; 2]) -> bool {
    let p = theme::palette_of(ui);
    // Fill only; a stroke would change the button height and shift the
    // rows above it in the bottom-up stack.
    let mut button = Button::new("Mute");
    if *muted {
        let t = if ui.visuals().dark_mode { 0.45 } else { 0.22 };
        button = Button::new(RichText::new("Mute").color(p.text_primary))
            .fill(theme::blend(p.surface2, p.danger, t));
    }
    let response = ui
        .add_sized(vec2(width, ROW_H), button)
        .on_hover_text(if *muted { hover[0] } else { hover[1] });
    if response.clicked() {
        *muted = !*muted;
        true
    } else {
        false
    }
}

/// Reserved slot for the per-member meter; protocol support arrives with
/// the Stats follow-up. Outlined so it reads reserved, not broken.
fn meter_slot(ui: &mut Ui) {
    let (rect, response) =
        ui.allocate_exact_size(vec2(STRIP_INNER_W, METER_SLOT_H), Sense::hover());
    if ui.is_rect_visible(rect) {
        use egui::emath::GuiRounding;
        let rect = rect.round_to_pixels(ui.pixels_per_point());
        let p = theme::palette_of(ui);
        ui.painter().rect(
            rect,
            CornerRadius::same(2),
            p.surface0,
            Stroke::new(1.0, p.border),
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            "meter",
            egui::FontId::new(9.0, egui::FontFamily::Proportional),
            p.text_muted,
        );
    }
    response.on_hover_text("per-member meters arrive with a protocol update");
}
