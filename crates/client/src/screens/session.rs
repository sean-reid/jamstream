//! The session screen: mixer strips on the left, chat on the right, and a
//! status bar in three zones. Below 900 px the chat collapses behind a toggle
//! so nothing overlaps.
//!
//! # The bar's three zones
//!
//! Health on the left, state in the centre, meter and action on the right.
//! The centre is the point of it: ON AIR and REC are the two states in the
//! product with consequences outside the room, so they share one cluster in
//! the middle, each in its own lamp. The cluster takes no space at all while
//! all of them are off.
//!
//! AUDITION is in there too, because "your monitor is not your monitor" is a
//! state with consequences. Every lamp in the cluster carries a shape as well
//! as a colour, so the two that can be lit at once are told apart without
//! having to tell two warm oranges apart.
//!
//! What the link is doing beyond the headline number, rtt and buffer depth
//! and each direction's loss, is on that number's hover. It is diagnostic:
//! worth reading when something sounds wrong, not worth a permanent five
//! words in the one place a musician looks mid-song. The exception is an
//! uplink the server says is losing audio, which earns a tag of its own,
//! because that is the one fault a musician cannot hear.

use egui::{
    Align, Align2, Button, FontFamily, FontId, Layout, RichText, ScrollArea, Sense, Stroke,
    TextEdit, Ui, UiBuilder, vec2,
};

use crate::runtime::{
    BroadcastView, ChatLine, Command, ConnState, FaderView, MemberView, Role, Runtime, Snapshot,
};
use crate::screens::destinations::DestinationsPanel;
use crate::screens::invites::{InvitesEvent, InvitesPanel};
use crate::screens::record::{record_sheet, record_state_lamp};
use crate::theme;
use crate::widgets::{
    AVATAR_D_STRIP, LOSSY_PCT, LampShape, Meter, avatar_disc, db_drag, fader, lamp_toggle, meter,
    pan_slider, presence_dot, state_lamp, state_lamp_width, status_dot,
};

const NARROW_BELOW_PX: f32 = 900.0;

/// The bar's word for the one direction nothing on this machine can hear. It
/// names the direction rather than the fault, because a musician whose sound
/// is not reaching the room reads a healthy screen otherwise.
pub const UPLINK_LOSS_TAG: &str = "uplink loss";

/// What the pair of compact meters needs beside the readouts: two 52 px
/// meters, their labels, the separator, and the gaps between them.
const BAR_METERS_W: f32 = 180.0;

/// What the health zone needs with its meters dropped: the connection dot,
/// the mouth-to-ear number, and the two lines of unit beside it. The bar
/// stacks rather than squeeze this.
const BAR_HEALTH_MIN_W: f32 = 132.0;

/// The gap each zone keeps from the centre cluster.
const BAR_ZONE_GAP: f32 = theme::SPACE_LG;

// Console geometry: every strip is exactly this wide regardless of content.
const STRIP_W: f32 = 104.0;
const STRIP_INNER_W: f32 = STRIP_W - 20.0;
const STRIP_GAP: f32 = 8.0;
/// The size a strip's button rows are asked for. What one draws at is
/// [`button_row_h`], which is what the console reserves, because egui floors
/// a button at its own text height plus the style's padding whatever size it
/// is added at.
const ROW_H: f32 = 22.0;
const DB_H: f32 = 16.0;
const PAN_H: f32 = 14.0;
const NAME_ROW_H: f32 = 18.0;
/// The "disconnected" note; only a disconnected member's strip carries it.
const NOTE_ROW_H: f32 = 18.0;
/// Floor on the fader. The console reserves this much per strip before it
/// draws and scrolls when the window cannot hold it, so a fader is never
/// handed less and never takes the difference out of the portrait and the
/// name above it. A host strip clears the floor at the smallest window the
/// app opens at, 800x600; scrolling is for the sizes below that.
pub const MIN_FADER_H: f32 = 32.0;
/// The panel primitive's 10 px margins around the strip's content.
const STRIP_FRAME_H: f32 = 20.0;
/// The panel's hairline, one pixel on each edge, which the frame adds around
/// the box the strip allocates inside it. The vertical reservation counts it,
/// the same as the horizontal one.
const STRIP_FRAME_STROKE_H: f32 = 2.0;
/// The hair a fader keeps off the readout above it.
const FADER_INSET_H: f32 = 2.0;
/// The gap between a strip's own rows, tighter than the app's default: eight
/// rows of a host strip at the default spacing do not leave a fader its floor
/// at 800x600, and the console would answer that by scrolling a strip's bottom
/// edge out of the window. Density buys travel here, which is the one thing a
/// fader is for.
const STRIP_ROW_GAP: f32 = theme::SPACE_SM;

// Chat columns: a monospace clock, a name gutter, then the message. The
// message column is the one thing that must never move: every line of a
// message, wrapped continuations included, shares its left edge, in both
// the wide and the narrow layout. Names past the gutter ellipsize with the
// full name one hover away, the same treatment as a strip's name.
const CHAT_TIME_W: f32 = 38.0;
const CHAT_NAME_W: f32 = 64.0;
/// The chat panel's width in the wide layout. The console measures itself
/// against what is left of the window, so this is one number both of them
/// read.
const CHAT_PANEL_W: f32 = 280.0;

/// The metronome panel's width: its widest label, "beats per bar", plus the
/// drag value beside it and the panel's own margins.
const METRONOME_W: f32 = 290.0;

pub enum SessionEvent {
    /// The user confirmed leaving; the app should drop the runtime.
    Left,
    /// Host only: leave, destroy the server, and mark the session ended.
    EndSession,
}

/// A tab in the settings drawer. Which of these exist depends on the role
/// and on whether there is a session at all, so the row is built per frame
/// rather than fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Audio,
    Broadcast,
    Invites,
    /// Where takes go: a bucket and the key that writes it. Machine-local
    /// preference rather than session state, so it is present whatever this
    /// window is showing.
    Recording,
    You,
}

impl SettingsTab {
    /// One word each; five of them wrap onto a second row at the drawer's
    /// width, which the row is built to do.
    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::Audio => "Audio",
            SettingsTab::Broadcast => "Broadcast",
            SettingsTab::Invites => "Invites",
            SettingsTab::Recording => "Recording",
            SettingsTab::You => "You",
        }
    }
}

#[derive(Default)]
pub struct SessionScreen {
    pub chat_input: String,
    pub confirm_leave: bool,
    /// Member pending revoke confirmation, with the display name.
    pub confirm_revoke: Option<(crate::runtime::MemberId, String)>,
    /// Narrow layout only: chat shown instead of the mixer.
    pub chat_open: bool,
    /// Host sessions launched by this app carry the invite book; plain joins
    /// have none, and the settings drawer shows them no Invites tab.
    pub invites: Option<InvitesPanel>,
    /// Host sessions this app launched carry the destinations panel; it needs
    /// the credential store, so plain joins have none, exactly like the
    /// invite book.
    pub destinations: Option<DestinationsPanel>,
    /// Host only: the record sheet. It needs nothing but the runtime, so it
    /// hangs off the snapshot's `is_host` rather than a panel of its own;
    /// everyone else gets the lamp in the bar.
    pub record_open: bool,
    /// What the launch's retention call left this session with, when it left
    /// it with anything worth saying. Refreshed from the Recording tab every
    /// frame the session is on screen, so this screen holds no second answer;
    /// the record sheet shows it beside what the take is capturing.
    pub retention_note: Option<crate::screens::recording::RetentionNote>,
    /// Set for the frame the record sheet opens. It shares [`theme::SHEET_OFFSET`]
    /// with the settings drawer, so the app closes the drawer instead of
    /// letting the wider sheet stick out past the drawer's left edge.
    pub took_the_sheet_anchor: bool,
    /// Where the status bar starts, from this frame. The settings drawer is
    /// drawn after the screen and stops here, so it never covers the
    /// mouth-to-ear readout or the meters a musician adjusts against.
    pub status_bar_top: Option<f32>,
    /// The settings drawer is over the chat panel this frame. It is the full
    /// height of the window between the bars and wider than the panel, so it
    /// hides the conversation and leaves the message field showing under its
    /// bottom edge: a lone text field with a message placeholder and no
    /// messages, two surfaces overlapping.
    /// The panel keeps its place in the layout and draws nothing while it is
    /// covered, so opening the drawer moves no strip.
    pub chat_covered: bool,
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
            } else if self
                .destinations
                .as_mut()
                .is_some_and(DestinationsPanel::close_key_entry)
            {
                // The key pane is inside the drawer, so it goes before it.
            } else if self.record_open {
                // Closing the sheet is navigation: the take keeps running.
                self.record_open = false;
            } else if narrow && self.chat_open {
                self.chat_open = false;
            }
        }

        let bar = egui::Panel::bottom(egui::Id::new("session-status"))
            .show_separator_line(true)
            .show(ui, |ui| self.status_bar(ui, snap));
        self.status_bar_top = Some(bar.response.rect.top());

        if !narrow {
            egui::Panel::right(egui::Id::new("session-chat"))
                .resizable(false)
                .exact_size(CHAT_PANEL_W)
                .show(ui, |ui| {
                    // The panel keeps its width whatever is over it, so
                    // opening the drawer moves no strip; it draws nothing
                    // while the drawer is over it, because the only part of
                    // it that reached past the drawer's bottom edge was the
                    // message field. The narrow layout is not covered
                    // this way: there the chat is the whole content and the
                    // drawer takes less than half of it.
                    if !self.chat_covered {
                        self.chat_ui(ui, snap, rt, true);
                    }
                });
            self.mixer_ui(ui, snap, rt);
        } else {
            // The toggle is symmetric and stationary: same place, same
            // label, state shown by the active fill. One click back to the
            // faders, always.
            ui.horizontal(|ui| {
                let p = theme::palette_of(ui);
                if ui
                    .add(theme::selectable(p, "Chat", self.chat_open))
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

        if self.record_open && snap.is_host {
            record_sheet(
                ui,
                snap,
                rt,
                self.retention_note.as_ref(),
                &mut self.record_open,
            );
        }

        self.confirm_windows(ui, rt, &mut event);
        event
    }

    /// True while this screen has something of its own that Escape has to
    /// leave first. The settings drawer is the outer surface of the two, so
    /// the app hands the key to the screen while any of these is up, so a
    /// confirmation raised from a strip takes Escape before the drawer it sits
    /// on top of.
    ///
    /// The narrow chat toggle is deliberately not in here: it is a view
    /// switch, not something entered, and the drawer sits on top of it.
    pub fn has_inner_overlay(&self) -> bool {
        self.confirm_leave
            || self.confirm_revoke.is_some()
            || self
                .invites
                .as_ref()
                .is_some_and(|p| p.confirm_revoke.is_some() || p.confirm_end)
            || self
                .destinations
                .as_ref()
                .is_some_and(DestinationsPanel::entering_key)
            || self.record_open
    }

    /// Which settings tabs this session has to offer, in order. A tab that
    /// would open an empty panel is not offered at all: a plain join has no
    /// invite book and nothing to stream, so it sees the machine-local tabs and
    /// no gap where the others were.
    pub fn settings_tabs(&self, snap: &Snapshot) -> Vec<SettingsTab> {
        let mut tabs = vec![SettingsTab::Audio];
        if snap.is_host && (snap.broadcast.is_some() || self.destinations.is_some()) {
            tabs.push(SettingsTab::Broadcast);
        }
        if snap.is_host && self.invites.is_some() {
            tabs.push(SettingsTab::Invites);
        }
        // Whether this session records was fixed at launch, but the bucket it
        // would record to belongs to the computer, so the tab is here in a
        // session as well as before one.
        tabs.push(SettingsTab::Recording);
        tabs.push(SettingsTab::You);
        tabs
    }

    /// The Broadcast tab: what listeners hear, then where it goes. That order
    /// is the signal's own, and it is the order a host sets them up in.
    pub fn broadcast_tab(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime) {
        if snap.broadcast.is_some() {
            self.stream_mix_ui(ui, snap, rt);
        }
        if let Some(panel) = &mut self.destinations {
            if snap.broadcast.is_some() {
                ui.add_space(theme::SPACE_XL);
            }
            panel.ui(ui, snap, rt);
        }
    }

    /// The Invites tab, which exists only for a host this app launched.
    pub fn invites_tab(
        &mut self,
        ui: &mut Ui,
        snap: &Snapshot,
        rt: &dyn Runtime,
    ) -> Option<SessionEvent> {
        let panel = self.invites.as_mut()?;
        panel
            .ui(ui, snap, rt)
            .map(|InvitesEvent::EndSession| SessionEvent::EndSession)
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
                theme::reason(ui, format!("removed from the session: {reason}"));
            }
            ConnState::TimedOut => {
                theme::reason(ui, "connection lost: no packets for 10 seconds");
            }
            // Not danger: nothing is wrong and nothing was lost. The seat
            // this invite names is occupied, the client is still trying, and
            // one leaving musician is enough.
            ConnState::SessionFull => {
                let p = theme::palette_of(ui);
                ui.label(
                    RichText::new("the session is full; waiting for a seat to free")
                        .color(p.meter_amber),
                );
            }
            ConnState::Joined | ConnState::Idle => {}
        }

        // A device that will not run is a genuine problem: the session is up,
        // the strips are drawn, and this musician is silent in both directions.
        // A swap mid-song cannot be silence with nothing on screen, so the
        // reason goes above the strips, in the danger step that reads on the
        // panel, never the accent, which means live.
        //
        // The device's words first and unprefixed, because a reason wrapped in
        // a sentence of ours reads "no audio device is running: unsupported
        // audio configuration: ...", repeating "no audio device is running"
        // from the muted line below. What it costs and what to do about it go
        // underneath, in the muted step, the way the session-full and
        // no-probe lines do it.
        if let Some(reason) = &snap.device_error {
            theme::reason(ui, reason.clone());
            ui.label(theme::muted(
                ui,
                "No audio device is running, so nobody can hear you and you cannot hear the room. \
                 The Audio tab in Settings picks another device.",
            ));
        }

        let musicians: Vec<MemberView> = snap
            .members
            .iter()
            .filter(|m| m.role == Role::Musician)
            .cloned()
            .collect();
        // Outer strip width includes the 1 px frame stroke on each side.
        let row_w = strips_w(musicians.len());
        // The strips are as tall as the tallest of them needs to be, so a
        // window too short for one scrolls down as well as sideways. Measured
        // out here because the vertical bar takes its width from the console,
        // and the console shows whole strips only.
        let needed = musicians
            .iter()
            .map(|m| strip_h_for(ui, STRIP_ROW_GAP, m, snap.is_host))
            .fold(0.0_f32, f32::max);
        let bar = ui.spacing().scroll.bar_width + ui.spacing().scroll.bar_inner_margin;
        let scroll_h = ui.available_height();
        // The console extends sideways past the window; the lower panel stays
        // within it. Never part of a strip: a strip cut down the middle leaves
        // a musician's name broken mid-word and a Mute they cannot press, with
        // a horizontal scrollbar as the only cue.
        let room = ui.available_width() - if needed > scroll_h - bar { bar } else { 0.0 };
        let visible_w = whole_strips_w(room, musicians.len());
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
        // strip, and down when the window is too short for a whole strip. Its
        // viewport is a whole number of strips wide, so what scrolls into it
        // is a whole strip and never the left half of one.
        ScrollArea::both()
            .id_salt("mixer-scroll")
            .max_width(visible_w)
            .show(ui, |ui| {
                // Leave room for the horizontal scrollbar when the row
                // overflows.
                let hbar = if overflow { 10.0 } else { 2.0 };
                // Every strip is as tall as the tallest one needs to be, so
                // the rows line up across the console.
                let strip_h = (ui.available_height() - hbar).max(needed);
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
                    // The gap the reservation was computed with; the rows
                    // stack from the bottom edge up, so a strip whose spacing
                    // and reservation disagreed would take the difference out
                    // of the fader.
                    ui.spacing_mut().item_spacing.y = STRIP_ROW_GAP;
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
            // Presence, not link quality: the only per-member fact this side
            // has is whether they are here. Your own rtt and loss belong
            // nowhere near a strip, where they would read as that member's.
            presence_dot(ui, member.connected, member.quiet);
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
        let row = button_row_h(ui);
        ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
            if snap.is_host {
                if member.is_you {
                    // Reserves the revoke row so slots align across strips.
                    ui.allocate_exact_size(vec2(STRIP_INNER_W, row), Sense::hover());
                } else if ui
                    .add_sized(vec2(STRIP_INNER_W, row), Button::new("Revoke"))
                    .clicked()
                {
                    self.confirm_revoke = Some((member.id, member.name.clone()));
                }
            }
            if member.is_you {
                // Your uplink has no fader; monitoring yourself happens
                // locally on your interface, not through the server mix.
                ui.add_enabled_ui(false, |ui| {
                    mute_button(ui, &mut muted, vec2(STRIP_INNER_W, row), MUTE_MONITOR_HOVER);
                    pan_row(ui, &format!("{} pan", member.name), &mut pan);
                    db_readout(ui, gain, false);
                    let fader_h = (ui.available_height() - FADER_INSET_H).max(0.0);
                    fader(ui, &label, &mut gain, vec2(STRIP_INNER_W, fader_h))
                        .on_disabled_hover_text(
                            "your own channel: self monitoring is local, not part of the mix",
                        );
                });
            } else {
                if mute_button(ui, &mut muted, vec2(STRIP_INNER_W, row), MUTE_MONITOR_HOVER) {
                    changed = true;
                }
                changed |= pan_row(ui, &format!("{} pan", member.name), &mut pan);
                db_readout(ui, gain, true);
                let fader_h = (ui.available_height() - FADER_INSET_H).max(0.0);
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

    /// The host's broadcast mix: one compact row per musician, the host's own
    /// channel included (listeners hear it too), plus the audition switch.
    /// Deliberately unlike the monitor strips: horizontal rows, so the two
    /// mixes are never mistaken for each other.
    fn stream_mix_ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime) {
        let Some(broadcast) = &snap.broadcast else {
            return;
        };
        ui.label(theme::title(ui, "Stream mix"));
        ui.label(
            theme::muted(
                ui,
                "What listeners and the stream hear. Your monitor mix is unaffected.",
            )
            .small(),
        );
        ui.add_space(theme::SPACE_SM);
        for member in snap.members.iter().filter(|m| m.role == Role::Musician) {
            stream_mix_row(ui, member, broadcast, rt);
        }
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
        ui.add(egui::Label::new(
            theme::muted(
                ui,
                "While on, your monitor is the stream mix, your own voice included.",
            )
            .small(),
        ));
    }

    fn metronome_ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime, _row_w: f32) {
        theme::panel(ui).show(ui, |ui| {
            // As wide as its own content, not as wide as however many
            // musicians are in the band: tracking the strip row would leave
            // four short rows in the corner of a 1500 px box at ten musicians,
            // its right edge under the settings drawer.
            ui.set_width(METRONOME_W.min(ui.available_width()));
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
                        // The same word for the same fact: a host's row and a
                        // musician's row use the same label for this
                        // control's state.
                        let word = if enabled { "on" } else { "off" };
                        changed |= ui.checkbox(&mut enabled, word).changed();
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
    /// The zones are placed rather than flowed: the cluster is centred on the
    /// bar, and health and controls get the halves on either side of it. Two
    /// rules keep each zone inside its own half: health drops its meters below
    /// [`BAR_METERS_W`], and the bar stacks into two rows when either zone
    /// needs more than a half. For a host that lands at about 820 px with the
    /// cluster empty, 880 with one lamp lit and 930 with both, measured by
    /// `the_one_row_threshold_is_where_the_zones_stop_fitting`; a musician's
    /// narrower controls keep one row down past the 800 px minimum window.
    fn status_bar(&mut self, ui: &mut Ui, snap: &Snapshot) {
        ui.add_space(theme::SPACE_SM);
        let cluster_w = state_cluster_width(ui, snap);
        let controls_w = self.controls_width(ui, snap);
        let full = ui.available_width();
        // A centred cluster splits what is left into two equal halves, so one
        // row needs the WIDER of the two zones to fit in a half, not the sum
        // of them to fit in the whole. Summing was wrong and it showed: at 800
        // the id and the timer were drawn straight through both lamps, because
        // a right-to-left layout given a rect too narrow for its content runs
        // out of the left edge of it rather than clipping.
        let half = (full - cluster_w) / 2.0 - BAR_ZONE_GAP;
        let one_row = controls_w <= half && BAR_HEALTH_MIN_W <= half;
        if one_row {
            let row_h = ui.spacing().interact_size.y.max(28.0);
            let (rect, _) = ui.allocate_exact_size(vec2(full, row_h), Sense::hover());
            let centre = rect.center().x;
            let cluster = egui::Rect::from_min_max(
                egui::pos2(centre - cluster_w / 2.0, rect.top()),
                egui::pos2(centre + cluster_w / 2.0, rect.bottom()),
            );
            let health = rect.with_max_x(cluster.left() - BAR_ZONE_GAP);
            let controls = rect.with_min_x(cluster.right() + BAR_ZONE_GAP);
            ui.scope_builder(UiBuilder::new().max_rect(health), |ui| {
                ui.horizontal_centered(|ui| status_readouts(ui, snap));
            });
            if cluster_w > 0.0 {
                ui.scope_builder(UiBuilder::new().max_rect(cluster), |ui| {
                    ui.horizontal_centered(|ui| state_cluster(ui, snap));
                });
            }
            ui.scope_builder(UiBuilder::new().max_rect(controls), |ui| {
                ui.horizontal_centered(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        self.status_controls(ui, snap);
                    });
                });
            });
        } else {
            // Two rows, and the cluster keeps the readouts' company rather
            // than the controls': both halves of that row are about what the
            // session is doing, not what to press.
            ui.horizontal(|ui| {
                status_readouts(ui, snap);
                state_cluster(ui, snap);
            });
            ui.add_space(theme::SPACE_SM);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.status_controls(ui, snap);
                });
            });
        }
        ui.add_space(theme::SPACE_SM);
    }

    /// The action zone, right to left: the way out, a divider, the one
    /// performance action, and the meter a host is spending against. Leave
    /// keeps its own colour and never sits flush against Record: one ends the
    /// session for you, the other is pressed between songs.
    fn status_controls(&mut self, ui: &mut Ui, snap: &Snapshot) {
        let p = theme::palette_of(ui);
        if ui.add(theme::danger_button(p, "Leave")).clicked() {
            self.confirm_leave = true;
        }
        bar_divider(ui);
        // Accent means the live state, so it follows the take and not the
        // sheet: lighting it for an open sheet with no take running would
        // claim a take exists, in the same colour as the ON AIR lamp.
        // The sheet being on screen is what says the sheet is open.
        let capturing = matches!(snap.record.state, crate::runtime::RecordState::Recording);
        if snap.is_host
            && ui
                .add(theme::selectable(p, "Record", capturing))
                .on_hover_text(if capturing {
                    "a take is running; open the sheet to end it"
                } else {
                    "start or end a take"
                })
                .clicked()
        {
            self.record_open = !self.record_open;
            // The sheet and the settings drawer share one anchor.
            self.took_the_sheet_anchor = self.record_open;
        }
        if let Some(cost) = &snap.cost {
            ui.label(theme::mono(
                ui,
                format!("{} so far", theme::microusd(cost.accrued_microusd)),
            ));
            ui.label(theme::mono_muted(ui, elapsed(cost.elapsed_secs)));
        }
        ui.label(theme::mono_muted(ui, snap.session_short.clone()))
            .on_hover_text("this session's id");
    }

    /// What the action zone needs, measured rather than guessed, because the
    /// one-row decision is made before any of it is drawn.
    fn controls_width(&self, ui: &mut Ui, snap: &Snapshot) -> f32 {
        let gap = ui.spacing().item_spacing.x;
        let pad = 2.0 * ui.spacing().button_padding.x;
        let mut w = button_text_width(ui, "Leave") + pad + gap + SEPARATOR_W;
        if snap.is_host {
            w += gap + button_text_width(ui, "Record") + pad;
        }
        if let Some(cost) = &snap.cost {
            let money = format!("{} so far", theme::microusd(cost.accrued_microusd));
            w += gap + mono_text_width(ui, &money);
            w += gap + mono_text_width(ui, &elapsed(cost.elapsed_secs));
        }
        w + gap + mono_text_width(ui, &snap.session_short)
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
                        if ui.add(theme::danger_button(p, "Leave session")).clicked() {
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
                        "Revoke {name}'s invite? They will be disconnected, their invite stops \
                         working, and their seat is free again."
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.confirm_revoke = None;
                        }
                        let p = theme::palette_of(ui);
                        if ui.add(theme::danger_button(p, "Revoke invite")).clicked() {
                            // Token comes from the current snapshot; host only.
                            let token = rt
                                .snapshot()
                                .members
                                .iter()
                                .find(|m| m.id == member_id)
                                .and_then(|m| m.token);
                            if let Some(token) = token {
                                rt.send(Command::Revoke(token));
                                // Free the seat in the same act, so the
                                // invites panel never counts a seat the
                                // server has already emptied.
                                if let Some(panel) = &mut self.invites {
                                    panel.revoke(token, Some(name.clone()));
                                }
                            }
                            self.confirm_revoke = None;
                        }
                    });
                });
        }
    }
}

/// One compact broadcast row: name, dB drag-value, pan, mute, on one line at
/// the drawer's width with ten of them possible. Everything is enabled, the
/// host's own channel included; listeners hear that too. Any change sends the
/// row's full fader state.
fn stream_mix_row(ui: &mut Ui, member: &MemberView, broadcast: &BroadcastView, rt: &dyn Runtime) {
    /// The four cells, sized so the row fits the drawer with the gaps between
    /// them: a name that truncates with the full one on hover, and three
    /// controls that may not shrink, because a 40 px fader is not a control.
    const NAME_W: f32 = 84.0;
    const GAIN_W: f32 = 58.0;
    const MUTE_W: f32 = 52.0;
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

    let mut gain = view.gain_db;
    let mut pan = view.pan;
    let mut muted = view.muted;
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            vec2(NAME_W, ROW_H),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.set_min_width(NAME_W);
                let you_w = if member.is_you { 30.0 } else { 0.0 };
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
                // The cell truncates past about eight characters here, so the
                // hover is the full name from that point on.
                if member.name.chars().count() > 8 {
                    response.on_hover_text(member.name.clone());
                }
                if member.is_you {
                    ui.label(theme::muted(ui, "you").small());
                }
            },
        );
        changed |= db_drag(
            ui,
            &format!("{} stream gain", member.name),
            &mut gain,
            vec2(GAIN_W, ROW_H),
        )
        .changed();
        changed |= pan_slider(ui, &format!("{} stream pan", member.name), &mut pan).changed();
        changed |= mute_button(ui, &mut muted, vec2(MUTE_W, ROW_H), MUTE_STREAM_HOVER);
    });
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

/// The health zone: the connection dot, the one number worth a glance
/// mid-song, and the input and output meters when there is room for them.
fn status_readouts(ui: &mut Ui, snap: &Snapshot) {
    status_dot(ui, &snap.stats);
    latency_readout(ui, snap);
    // The compact meters are the first thing to go when the bar gets tight.
    // What is left here is the room the zone was given, so this is the real
    // question: do the meters fit in it, or not.
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

/// Mouth to ear, the headline number, with everything else the link reports
/// on its hover: rtt, each jitter buffer's depth, and loss in each
/// direction. Those are what someone reads when the sound is wrong, and
/// reading them is a deliberate act; carrying them permanently cost the bar
/// the space its two lamps now use.
fn latency_readout(ui: &mut Ui, snap: &Snapshot) {
    let s = &snap.stats;
    let p = theme::palette_of(ui);
    let m2e = s
        .mouth_to_ear_ms()
        .map_or("--.-".to_owned(), |v| format!("{v:>4.1}"));
    let group = ui.horizontal(|ui| {
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
        // The converting tag persists for as long as a direction rides the
        // boundary converter: the device's own rate sits beside the number
        // its cost is inside of. Muted, because it is a fact, not a fault.
        //
        // Both rates and the verb, because a bare "44.1 kHz" beside the
        // latency number reads as the rate the session is running at, which
        // is the one thing it is not: sessions are 48 kHz always, and 44.1 is
        // what this machine's device is clocked at. Stacked like the ms
        // label above, so saying it costs almost no width in the bar.
        if let Some(device) = s.rate.and_then(|r| r.resampled_rate()) {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(RichText::new("converting").size(9.5).color(p.text_muted));
                ui.label(
                    RichText::new(format!(
                        "{} to {} kHz",
                        crate::runtime::khz(device),
                        crate::runtime::khz(jamstream_protocol::SAMPLE_RATE)
                    ))
                    .size(9.5)
                    .color(p.text_muted),
                );
            });
        }
        // A different question from the one below, and both answers can be
        // true at once: that one is whether anybody can hear this musician
        // right now, this one is whether the device will hold. It goes first
        // of the two because "no audio cutting out" reads as one phrase that
        // means the opposite of both of them.
        if s.cutting_out.is_some() {
            ui.label(
                RichText::new("cutting out")
                    .size(9.5)
                    .color(theme::danger_ink(p)),
            );
        }
        // Persists for as long as this computer has no audio stream, in the
        // same danger ink for the same reason: nobody can hear a musician
        // whose device is down. Whether the cadence is still trying and what
        // to do about it are on the Audio tab, beside the pickers.
        if snap.audio_fault.is_some() {
            ui.label(
                RichText::new("no audio")
                    .size(9.5)
                    .color(theme::danger_ink(p)),
            );
        }
        // Persists for as long as the playout ring is in a crackling run: in
        // danger ink rather than converting's muted one, because this is a
        // fault. The fix is a Buffer size pick, not something to say here, so
        // the word is bare and the remedy lives on the Audio tab instead.
        if s.crackling {
            ui.label(
                RichText::new("crackling")
                    .size(9.5)
                    .color(theme::danger_ink(p)),
            );
        }
        // Persists for as long as the server's last window says the band is
        // losing this musician's audio, in danger ink because nothing they
        // hear will tell them: their own monitoring, the meters and the
        // downlink all read healthy while the room hears a broken instrument.
        // The rate itself and the other direction are on the hover; a person
        // mid-song needs the direction first.
        if s.uplink_loss_pct.is_some_and(|pct| pct > LOSSY_PCT) {
            ui.label(
                RichText::new(UPLINK_LOSS_TAG)
                    .size(9.5)
                    .color(theme::danger_ink(p)),
            );
        }
    });
    ui.interact(
        group.response.rect,
        ui.id().with("latency-detail"),
        Sense::hover(),
    )
    .on_hover_text(latency_hover(s));
}

/// The latency number's hover, line by line: the link's own figures, what each
/// direction's device buffer costs inside the number, then what the device
/// stream got from the OS. Which WASAPI mode won and how each direction reaches
/// the session rate are the facts that move this number between days on the same
/// machine; a platform that reports neither adds no line, because a made-up
/// answer is worse than none.
fn latency_hover(s: &crate::runtime::StatsView) -> String {
    let rtt = s.rtt_ms.map_or("--".to_owned(), |v| format!("{v:.1}"));
    let mut text = format!(
        "rtt {rtt} ms, charged for both legs, low if somebody's link is worse\n{}\n{}",
        s.jitter_lines().join("\n"),
        s.loss_lines().join("\n")
    );
    // Per direction rather than one device figure: they answer different
    // questions, the playout cushion is the larger of the two, and the Buffer
    // size pick moves both.
    if let Some(device) = s.device_buffers {
        for line in device.lines() {
            text.push('\n');
            text.push_str(&line);
        }
    }
    match s.device_mode {
        Some(crate::runtime::DeviceModeView::Exclusive) => {
            text.push_str("\nexclusive mode device, about 10 ms");
        }
        Some(crate::runtime::DeviceModeView::Shared) => {
            text.push_str("\nshared mode device, 20 to 30 ms");
        }
        None => {}
    }
    if let Some(rate) = &s.rate {
        for line in rate.lines() {
            text.push('\n');
            text.push_str(&line);
        }
    }
    text
}

/// The centre cluster: every state that changes what leaves this room or what
/// you are hearing, side by side in the middle of the bar. Nothing is drawn
/// while all of them are off, so an idle session has a calm bar and a live one
/// cannot be misread.
fn state_cluster(ui: &mut Ui, snap: &Snapshot) {
    for entry in cluster_entries(theme::palette_of(ui), snap) {
        state_lamp(ui, entry.label, entry.color, entry.shape).on_hover_text(entry.hover);
    }
}

/// What the cluster will occupy, or zero when nothing is lit. Measured before
/// the bar is laid out, because the cluster's width decides how much room the
/// zones on either side of it get.
fn state_cluster_width(ui: &mut Ui, snap: &Snapshot) -> f32 {
    let entries = cluster_entries(theme::palette_of(ui), snap);
    if entries.is_empty() {
        return 0.0;
    }
    let gap = ui.spacing().item_spacing.x;
    let labels: Vec<&'static str> = entries.iter().map(|e| e.label).collect();
    labels
        .iter()
        .map(|label| state_lamp_width(ui, label))
        .sum::<f32>()
        + gap * (labels.len() as f32 - 1.0)
}

/// One lit state in the cluster.
struct ClusterEntry {
    label: &'static str,
    color: egui::Color32,
    shape: LampShape,
    hover: String,
}

/// The cluster's contents, in a fixed order so a lamp never moves as another
/// comes and goes: broadcast, then the take, then what the host is monitoring,
/// then the failure count that says a destination stopped.
///
/// Takes the palette rather than a Ui so the rules the cluster is built on can
/// be asserted without rendering; see
/// `no_two_lamps_that_light_together_look_alike`.
fn cluster_entries(p: &theme::Palette, snap: &Snapshot) -> Vec<ClusterEntry> {
    let mut entries = Vec::new();
    let live = snap.stream.live_count();
    if live > 0 {
        entries.push(ClusterEntry {
            label: "ON AIR",
            color: p.accent,
            shape: LampShape::Filled,
            hover: match live {
                1 => "this session is being broadcast to 1 destination".to_owned(),
                n => format!("this session is being broadcast to {n} destinations"),
            },
        });
    }
    if let Some((label, color, shape, hover)) = record_state_lamp(&snap.record.state, p) {
        entries.push(ClusterEntry {
            label,
            color,
            shape,
            hover,
        });
    }
    // Audition belongs in the cluster as a lamp like the others, not in the
    // health zone, which is reserved for link quality. Its ring takes no
    // colour: nothing is wrong and nothing extra is leaving the room.
    if snap.broadcast.as_ref().is_some_and(|b| b.audition) {
        entries.push(ClusterEntry {
            label: "AUDITION",
            color: p.text_primary,
            shape: LampShape::Ring,
            hover: "your monitor is the stream mix, your own voice included".to_owned(),
        });
    }
    if snap.stream.failed_count() > 0 {
        entries.push(ClusterEntry {
            label: "STREAM FAILED",
            color: p.danger,
            shape: LampShape::Filled,
            hover: "a destination stopped; the reason is in Settings, under Broadcast".to_owned(),
        });
    }
    entries
}

/// A separator's own width in a horizontal layout.
const SEPARATOR_W: f32 = 6.0;

/// The one divider in the bar, between Leave and everything routine beside
/// it. Painted rather than `ui.separator()`, which takes its height from the
/// row and reads as a hairline lost in the gap; this is meant to be seen.
fn bar_divider(ui: &mut Ui) {
    let p = theme::palette_of(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(SEPARATOR_W, 22.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        let x = rect.center().x.round() + 0.5;
        ui.painter().line_segment(
            [
                egui::pos2(x, rect.top() + 2.0),
                egui::pos2(x, rect.bottom() - 2.0),
            ],
            Stroke::new(1.0, p.border),
        );
    }
}

/// "00:47:32" from seconds, the timer's one format.
fn elapsed(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

fn text_width(ui: &mut Ui, text: &str, font: FontId) -> f32 {
    ui.fonts_mut(|f| {
        f.layout_no_wrap(text.to_owned(), font, egui::Color32::PLACEHOLDER)
            .size()
            .x
    })
}

fn button_text_width(ui: &mut Ui, text: &str) -> f32 {
    text_width(ui, text, FontId::new(13.5, FontFamily::Proportional))
}

fn mono_text_width(ui: &mut Ui, text: &str) -> f32 {
    text_width(ui, text, FontId::new(12.5, FontFamily::Monospace))
}

/// The outer width of one strip: the fixed box plus the panel's hairline on
/// each side.
const STRIP_OUTER_W: f32 = STRIP_W + 2.0;

/// What `count` strips take side by side, gaps included.
fn strips_w(count: usize) -> f32 {
    count as f32 * STRIP_OUTER_W + (count.max(1) - 1) as f32 * STRIP_GAP
}

/// The width to hand the console: as many whole strips as `room` holds, and
/// never part of one more. A strip cut down the middle is a name broken
/// mid-word and a Mute that cannot be pressed. One strip always, so a window
/// narrower than a single one clips rather than vanishes.
fn whole_strips_w(room: f32, count: usize) -> f32 {
    let per = STRIP_OUTER_W + STRIP_GAP;
    let fits = ((room + STRIP_GAP) / per).floor().max(1.0) as usize;
    strips_w(fits.min(count.max(1)))
}

/// What a strip's button rows actually draw at. egui floors a button at its
/// text height plus the style's vertical padding whatever size it is added
/// at, so the console asks rather than assumes: it reserved a flat 22, both
/// rows drew 24, and the fader absorbed the difference.
fn button_row_h(ui: &Ui) -> f32 {
    let h = (ui.text_style_height(&egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.y)
        .max(ui.spacing().interact_size.y)
        .max(ROW_H);
    // Rounded up to the pixel grid, which is where the row it draws lands. A
    // reservation short by a third of a pixel is still short.
    let ppp = ui.pixels_per_point();
    (h * ppp).ceil() / ppp
}

/// What one strip needs vertically: the portrait, the name row, the
/// disconnected note when there is one, every fixed row at the height it
/// really draws, and a fader that is still a fader. Items are separated by
/// `gap`, so the count of them decides the spacing, and the frame's own
/// margins and hairline come off the outside.
///
/// The mixer reserves this before drawing, because the lower rows stack
/// from the bottom edge upward: a fader handed less than it asked for used
/// to run its track back up through the name and the portrait, and once that
/// was fixed it took the shortfall out of its own travel instead.
fn strip_h_for(ui: &Ui, gap: f32, member: &MemberView, is_host: bool) -> f32 {
    let row = button_row_h(ui);
    let mut rows = AVATAR_D_STRIP + NAME_ROW_H + MIN_FADER_H + DB_H + PAN_H + row;
    let mut count = 6.0;
    if !member.connected {
        rows += NOTE_ROW_H;
        count += 1.0;
    }
    if is_host {
        rows += row;
        count += 1.0;
    }
    rows + (count - 1.0) * gap + FADER_INSET_H + STRIP_FRAME_H + STRIP_FRAME_STROKE_H
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

/// Fixed-size mute button; state is shown by fill, the label never moves.
fn mute_button(ui: &mut Ui, muted: &mut bool, size: egui::Vec2, hover: [&str; 2]) -> bool {
    let p = theme::palette_of(ui);
    // Fill only; a stroke would change the button height and shift the
    // rows above it in the bottom-up stack.
    let mut button = Button::new("Mute");
    if *muted {
        let t = if ui.visuals().dark_mode { 0.45 } else { 0.22 };
        button = Button::new(RichText::new("Mute").color(p.text_primary))
            .fill(theme::blend(p.surface2, p.danger, t));
    }
    let response =
        ui.add_sized(size, button)
            .on_hover_text(if *muted { hover[0] } else { hover[1] });
    if response.clicked() {
        *muted = !*muted;
        true
    } else {
        false
    }
}

// No per-member meter slot. An empty gauge on every strip reads as a readout
// that is broken rather than one that does not exist yet, so the strips keep
// the room until the Stats control message carries per-member levels; at that
// point the slot comes back with something in it.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::{DemoRuntime, FROZEN_FRAME};
    use crate::runtime::{DestinationState, RecordState, StreamPlatform};
    use crate::theme::{DARK, LIGHT};

    /// The console's width rule, over every width a window can leave it: what
    /// it takes is always a whole number of strips, never more room than there
    /// is, and never one strip fewer than the room holds.
    #[test]
    fn the_console_takes_whole_strips_and_all_of_them_that_fit() {
        let per = STRIP_OUTER_W + STRIP_GAP;
        // Half-pixel steps from nothing to less than ten strips, so the answer
        // is never capped by the roster.
        for half_px in 0..2_000u32 {
            let room = f32::from(half_px as u16) / 2.0;
            let w = whole_strips_w(room, 10);
            let strips = ((w + STRIP_GAP) / per).round();
            assert!(
                (strips * per - STRIP_GAP - w).abs() < 0.01,
                "{room} px gave {w}, which is {strips} strips and a fraction"
            );
            assert!(strips >= 1.0, "{room} px gave no strip at all");
            // Room for less than one strip clips one rather than drawing none.
            assert!(
                w <= room.max(STRIP_OUTER_W),
                "{room} px gave {w}, wider than the room"
            );
            assert!(
                w + per > room,
                "{room} px gave {w}, leaving room for a strip it did not draw"
            );
        }
        // And the roster is the other cap: four musicians are four strips.
        assert_eq!(whole_strips_w(4_000.0, 4), strips_w(4));
    }

    /// Every combination of cluster states a session can be in at once.
    fn every_cluster() -> Vec<(String, Snapshot)> {
        let mut out = Vec::new();
        for record in [
            None,
            Some(RecordState::Recording),
            Some(RecordState::Uploading),
            Some(RecordState::Failed {
                reason: "multipart upload aborted".to_owned(),
            }),
        ] {
            for stream in [
                &[][..],
                &[(StreamPlatform::Twitch, DestinationState::Live)][..],
                &[
                    (StreamPlatform::Twitch, DestinationState::Live),
                    (
                        StreamPlatform::YouTube,
                        DestinationState::Failed {
                            reason: "rtmp connection refused".to_owned(),
                        },
                    ),
                ][..],
            ] {
                for audition in [false, true] {
                    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
                    rt.set_destinations(stream);
                    if let Some(state) = record.clone() {
                        rt.set_record(state, false);
                    }
                    if audition {
                        rt.send(Command::SetBroadcastAudition(true));
                    }
                    let take = record.as_ref().map_or("idle", |_| "a take");
                    let label = format!(
                        "record {take}, {} destinations, audition {audition}",
                        stream.len()
                    );
                    out.push((label, rt.snapshot()));
                }
            }
        }
        out
    }

    /// Two lamps lit at once may never be the same lamp.
    ///
    /// ON AIR and UPLOADING sit a warm orange 1.25:1 apart and can be lit
    /// together, so colour alone cannot tell them apart. Shape is the cue
    /// that survives two hues nobody can separate at 5 px.
    #[test]
    fn no_two_lamps_that_light_together_look_alike() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            for (what, snap) in every_cluster() {
                let entries = cluster_entries(p, &snap);
                for (i, a) in entries.iter().enumerate() {
                    for b in &entries[i + 1..] {
                        assert!(
                            a.shape != b.shape || a.color != b.color,
                            "{name}, {what}: {} and {} are the same lamp",
                            a.label,
                            b.label
                        );
                    }
                }
            }
        }
    }

    /// The pair the audit actually found, named on its own so a future edit
    /// that gives them the same shape again fails on the reason rather than on
    /// a generic pairing rule.
    #[test]
    fn on_air_and_uploading_differ_in_shape() {
        let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
        rt.set_destinations(&[(StreamPlatform::Twitch, DestinationState::Live)]);
        rt.set_record(RecordState::Uploading, false);
        let snap = rt.snapshot();
        for p in [&DARK, &LIGHT] {
            let entries = cluster_entries(p, &snap);
            let labels: Vec<&str> = entries.iter().map(|e| e.label).collect();
            assert_eq!(labels, vec!["ON AIR", "UPLOADING"]);
            assert_eq!(entries[0].shape, LampShape::Filled);
            assert_eq!(entries[1].shape, LampShape::Ring);
        }
    }

    /// The hover behind the mouth-to-ear number names the sharing mode the
    /// device stream got and how each direction reaches the session rate,
    /// because those are what move the number between days on one machine.
    /// A platform that reports neither gets no line: `--` prose would be an
    /// instrument reading a made-up value.
    #[test]
    fn the_latency_hover_names_the_mode_and_the_rate_outcomes() {
        use crate::runtime::{DeviceModeView, RateOutcomeView, RateOutcomesView};

        let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
        let base = latency_hover(&rt.snapshot().stats);
        assert!(base.contains("rtt") && base.contains("buffer") && base.contains("loss"));
        assert!(
            base.contains("charged for both legs, low if somebody's link is worse"),
            "the rtt line carries the assumption the figure is built on: {base:?}"
        );
        assert!(
            !base.contains("mode") && !base.contains("converting"),
            "an unreported mode must add nothing: {base:?}"
        );

        rt.set_device_mode(Some(DeviceModeView::Shared));
        rt.set_rate(Some(RateOutcomesView {
            capture: RateOutcomeView::Resampled {
                device: 44_100,
                added_ms: 3.2,
            },
            playback: RateOutcomeView::OsConverted { device: 44_100 },
        }));
        let shared = latency_hover(&rt.snapshot().stats);
        assert!(
            shared.contains("shared mode device, 20 to 30 ms"),
            "{shared:?}"
        );
        assert!(
            shared.contains("converting capture 44.1 kHz to 48 kHz (+3.2 ms)"),
            "{shared:?}"
        );
        assert!(
            shared.contains("the OS is converting playback to this device's 44.1 kHz"),
            "{shared:?}"
        );

        rt.set_device_mode(Some(DeviceModeView::Exclusive));
        rt.set_rate(Some(RateOutcomesView {
            capture: RateOutcomeView::Native,
            playback: RateOutcomeView::Native,
        }));
        let exclusive = latency_hover(&rt.snapshot().stats);
        assert!(
            exclusive.contains("exclusive mode device, about 10 ms"),
            "{exclusive:?}"
        );
        assert!(
            !exclusive.contains("converting"),
            "native directions are not news: {exclusive:?}"
        );

        // The clock-set rung reads as the move it was, and macOS reports no
        // sharing mode, so the rate line stands alone.
        rt.set_device_mode(None);
        rt.set_rate(Some(RateOutcomesView {
            capture: RateOutcomeView::ClockSet { from: 44_100 },
            playback: RateOutcomeView::Native,
        }));
        let moved = latency_hover(&rt.snapshot().stats);
        assert!(
            moved.contains("moved the capture device to 48 kHz (was 44.1)"),
            "{moved:?}"
        );
        assert!(!moved.contains("mode device"), "{moved:?}");
    }

    /// The device buffers are two terms of the number, one per direction, and
    /// the larger of them is the playout cushion. The hover names each, because
    /// "device buffer" as one figure cannot say which end of the path a musician
    /// is paying for, and a Buffer size pick moves both.
    #[test]
    fn the_hover_prices_each_direction_of_the_device() {
        use crate::runtime::DeviceBuffersView;

        let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
        let mut s = rt.snapshot().stats;
        s.device_buffers = None;
        assert!(
            !latency_hover(&s).contains("cushion"),
            "with no stream there are no buffers to price"
        );

        s.device_buffers = Some(DeviceBuffersView {
            capture_ms: 10.0,
            playout_ms: 20.0,
        });
        let hover = latency_hover(&s);
        assert!(hover.contains("capture buffer 10.0 ms"), "{hover:?}");
        assert!(hover.contains("playout cushion 20.0 ms"), "{hover:?}");
    }

    /// The two directions mean opposite things: uplink loss is audio the band
    /// is not hearing, downlink loss is audio this machine is not playing, and
    /// the more urgent one is the one nothing on this side can hear. So the
    /// hover names both and says whose sound each is. One figure, whichever
    /// figure, cannot answer "can they hear me".
    #[test]
    fn the_hover_says_which_direction_is_losing() {
        let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
        let mut s = rt.snapshot().stats;
        for (up, down) in [(7.5, 0.1), (0.1, 7.5)] {
            s.uplink_loss_pct = Some(up);
            s.downlink_loss_pct = Some(down);
            let hover = latency_hover(&s);
            let line = |direction: &str| {
                hover
                    .lines()
                    .find(|l| l.starts_with(direction))
                    .unwrap_or_else(|| panic!("no {direction} line in {hover:?}"))
                    .to_owned()
            };
            let uplink = line("uplink loss");
            let downlink = line("downlink loss");
            assert!(
                uplink.contains(&format!("{up:.1}%")) && !uplink.contains(&format!("{down:.1}%")),
                "the uplink line must carry the uplink's own rate: {uplink:?}"
            );
            assert!(
                downlink.contains(&format!("{down:.1}%"))
                    && !downlink.contains(&format!("{up:.1}%")),
                "the downlink line must carry the downlink's own rate: {downlink:?}"
            );
            assert!(
                uplink.contains("band") && downlink.contains("you miss"),
                "each line must say whose sound it is: {uplink:?} {downlink:?}"
            );
        }

        // A direction with no figure yet says so rather than reading clean:
        // "0.0%" from a report that never arrived is an invented reading.
        s.uplink_loss_pct = None;
        let quiet = latency_hover(&s);
        assert!(quiet.contains("uplink loss --%"), "{quiet:?}");
    }

    /// Audition is a lamp in the cluster, not a dot in the health zone.
    #[test]
    fn audition_is_a_lamp_in_the_cluster() {
        let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
        rt.send(Command::SetBroadcastAudition(true));
        let snap = rt.snapshot();
        let entries = cluster_entries(&DARK, &snap);
        let labels: Vec<&str> = entries.iter().map(|e| e.label).collect();
        assert_eq!(labels, vec!["AUDITION"]);
        // Never the accent: the accent is what says this room is on air, and
        // audition changes only what the host hears.
        assert_ne!(entries[0].color, DARK.accent);
    }
}
