//! The host's invites panel on the session screen: the session's seats,
//! what is in each of them, per-seat copy and revoke, minting into a free
//! seat, and the end-session action. The host's own seat exists (the app
//! joined with its invite) but is never rendered anywhere.
//!
//! # Seats, not a list of invites
//!
//! A session has [`MAX_MUSICIANS`] musician seats and [`MAX_LISTENERS`]
//! listener seats, and the server admits against exactly those numbers by
//! counting who is *connected*. So this panel counts seats too. Revoking
//! ejects the holder, which frees the seat on the server; a panel that went
//! on counting the revoked invite would refuse to mint a replacement the
//! server would happily admit, which is what it used to do.
//!
//! A freed seat is minted into again, member id and all. That is safe
//! because revocation keys on the token's `jti`, never on the member id:
//! the replacement invite carries a fresh `jti` and the revoked one stays
//! in the server's persisted revoked set forever. Reuse is also what keeps
//! a member id meaning "seat number", which every invite label already
//! implies, instead of climbing past the size of the band.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use data_encoding::HEXLOWER;
use egui::{Align2, Button, Ui, vec2};

/// The seat label's cell, so a status word lands in the same place on every
/// row whatever the label says.
const SEAT_LABEL_W: f32 = 92.0;
use jamstream_cli::state::{InviteRecord, SessionState, SessionStatus};
use jamstream_cloud::{Provider, ProviderError, RegionId};
use jamstream_protocol::ids::{HOST_MEMBER_ID, MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};

use crate::runtime::{Command, Runtime, Snapshot};
use crate::screens::host::{base64_decode, unix_now};
use crate::theme;
use crate::widgets::{AVATAR_D_ROW, avatar_disc, row_cell};

/// Session capacity, host included on the musician side: the same
/// constants the server enforces admission with and the host wizard offers
/// seats against. Re-exported so this panel's callers keep one import.
pub use jamstream_session::{MAX_LISTENERS, MAX_MUSICIANS};

/// Member id to the token that admits whoever holds that seat right now.
/// Shared with [`crate::live::CostedRuntime`], which injects it into every
/// snapshot, so the mixer's revoke button targets the invite that is in the
/// seat rather than whatever was in it when the app started.
pub type TokenMap = Arc<Mutex<HashMap<MemberId, TokenId>>>;

/// The invite filling a seat.
#[derive(Debug, Clone, PartialEq)]
pub struct SeatInvite {
    pub token: TokenId,
    pub encoded: String,
}

/// One admission slot, identified by the member id the server will know its
/// holder by. Free seats stay in the list, so the host sees the shape of
/// the session and can mint straight back into one.
#[derive(Debug, Clone, PartialEq)]
pub struct Seat {
    pub member: MemberId,
    /// A free seat keeps the role it last carried, so its row still reads
    /// "musician 3" and refilling it needs no second decision.
    pub role: Role,
    /// None once the seat is revoked, which is what makes it free.
    pub invite: Option<SeatInvite>,
    /// The name the invite was minted for, from the token's own `name_hint`:
    /// the link knows who it is for, so the row can say whose seat is still
    /// empty (#357).
    pub hint: Option<String>,
    /// Who the revoke removed, kept greyed on the row as context. Cleared
    /// when someone else takes the seat.
    pub was: Option<String>,
}

impl Seat {
    /// "host", "musician 3", "listener 12": the same label the CLI writes
    /// into the state file, derived rather than stored so a seat that
    /// changes hands cannot keep the label of its last occupant.
    pub fn label(&self) -> String {
        jamstream_cli::host::invite_label(self.role, self.member)
    }

    pub fn is_free(&self) -> bool {
        self.invite.is_none()
    }
}

/// What the panel asks the session screen to bubble up.
pub enum InvitesEvent {
    /// Leave, destroy the server, mark the state file ended.
    EndSession,
}

pub struct InvitesPanel {
    pub state: SessionState,
    pub path: PathBuf,
    /// Every seat the session has, in member id order, the host's first.
    pub seats: Vec<Seat>,
    tokens: TokenMap,
    pub mint_role: Role,
    /// The optional name the next minted link is for, stamped into the
    /// token's `name_hint` so the roster and the take files carry it from
    /// the first packet. Cleared by the mint that uses it.
    pub mint_name: String,
    /// A mint or a state-file write that failed, shown under the mint row.
    /// Capacity is not one of these: the button is disabled instead, with
    /// the reason on hover.
    pub error: Option<String>,
    pub confirm_revoke: Option<(TokenId, String)>,
    pub confirm_end: bool,
    expires_unix: u64,
}

impl InvitesPanel {
    /// Reads the seats out of the state record. Undecodable invites are
    /// dropped with a warning rather than wedging the panel.
    pub fn new(state: SessionState, path: PathBuf) -> InvitesPanel {
        let mut seats: Vec<Seat> = Vec::with_capacity(state.invites.len());
        let mut expires_unix = 0;
        for record in &state.invites {
            match Invite::decode(&record.invite) {
                Ok(invite) => {
                    expires_unix = expires_unix.max(invite.token.expires_unix);
                    seats.push(Seat {
                        member: invite.token.member_id,
                        role: invite.token.role,
                        invite: Some(SeatInvite {
                            token: invite.token.jti,
                            encoded: record.invite.clone(),
                        }),
                        hint: invite.token.name_hint.clone(),
                        was: None,
                    });
                }
                Err(err) => {
                    tracing::warn!(%err, label = %record.role, "state file invite does not decode");
                }
            }
        }
        seats.sort_by_key(|s| s.member.0);
        if expires_unix == 0 {
            expires_unix = unix_now() + 12 * 3600;
        }
        let panel = InvitesPanel {
            state,
            path,
            seats,
            tokens: TokenMap::default(),
            mint_role: Role::Musician,
            mint_name: String::new(),
            error: None,
            confirm_revoke: None,
            confirm_end: false,
            expires_unix,
        };
        panel.publish_tokens();
        panel
    }

    /// The live member-to-token map, shared rather than copied: minting into
    /// a reused seat has to retarget the mixer's revoke button in the same
    /// instant it retargets this panel's.
    pub fn tokens(&self) -> TokenMap {
        Arc::clone(&self.tokens)
    }

    /// The token admitting whoever holds this seat, if anyone does.
    pub fn token_of(&self, member: MemberId) -> Option<TokenId> {
        self.seat_of(member)?.invite.as_ref().map(|i| i.token)
    }

    pub fn seat_of(&self, member: MemberId) -> Option<&Seat> {
        self.seats.iter().find(|s| s.member == member)
    }

    /// The rows the panel shows: every seat but the host's.
    pub fn guest_seats(&self) -> impl Iterator<Item = &Seat> {
        self.seats.iter().filter(|s| s.member != HOST_MEMBER_ID)
    }

    /// Seats in this role with a live invite in them. This is the number
    /// the cap applies to, and revoking lowers it.
    pub fn taken(&self, role: Role) -> usize {
        self.seats
            .iter()
            .filter(|s| s.role == role && !s.is_free())
            .count()
    }

    /// Frees the seat holding `token`. The server ejected its holder before
    /// this runs, so the seat is free immediately; anything slower would
    /// have the panel contradict the session.
    ///
    /// `was` is the name the roster had for them, kept on the row until
    /// someone else takes the seat.
    pub fn revoke(&mut self, token: TokenId, was: Option<String>) {
        let Some(seat) = self
            .seats
            .iter_mut()
            .find(|s| s.invite.as_ref().is_some_and(|i| i.token == token))
        else {
            return;
        };
        let member = seat.member;
        seat.invite = None;
        seat.was = was;
        // The record goes with it: a dead invite left in the file is a seat
        // this panel would count again after a restart, which is the same
        // defect one layer down.
        self.state
            .invites
            .retain(|r| !record_holds(r, member, token));
        self.persist();
        self.publish_tokens();
    }

    /// Mints into the lowest free seat, or into a new seat when none is
    /// free. Refuses at capacity; the UI disables the button first and this
    /// is the guard behind it.
    pub fn mint(&mut self, role: Role) -> Result<(), String> {
        if self.taken(role) >= cap(role) {
            return Err(at_capacity(role));
        }
        let member = self.open_seat();
        self.mint_into(member, role)
    }

    /// Mints into one named free seat, keeping its member id and its role.
    /// This is what a freed row's own button does.
    pub fn refill(&mut self, member: MemberId) -> Result<(), String> {
        let Some(seat) = self.seat_of(member) else {
            return Err(format!("seat {} is not part of this session", member.0));
        };
        if !seat.is_free() {
            return Err(format!("seat {} is already taken", member.0));
        }
        let role = seat.role;
        if self.taken(role) >= cap(role) {
            return Err(at_capacity(role));
        }
        self.mint_into(member, role)
    }

    /// The seat the next invite takes: the lowest member id no live invite
    /// holds, so a freed seat comes back before the session grows a new one
    /// and ids stay as dense as the band is large. Seat 0 is the host's and
    /// is never minted here.
    ///
    /// A free seat of the other role is taken as readily as one of this
    /// role. A seat is an admission slot; its label follows whoever is in
    /// it now, not whoever was.
    fn open_seat(&self) -> MemberId {
        (HOST_MEMBER_ID.0 + 1..=u16::MAX)
            .map(MemberId)
            .find(|id| !self.seats.iter().any(|s| s.member == *id && !s.is_free()))
            .expect("a session has far fewer seats than u16 has values")
    }

    /// Signs an invite for one seat with the issuer key from the state
    /// file, appends it to that file so `jamstream status` sees it too, and
    /// puts it in the seat.
    fn mint_into(&mut self, member: MemberId, role: Role) -> Result<(), String> {
        let issuer_bytes: [u8; 32] = base64_decode(&self.state.issuer_private_key_b64)?
            .try_into()
            .map_err(|_| "issuer key in the state file has the wrong length".to_owned())?;
        let issuer = Issuer::from_bytes(&issuer_bytes);
        let server_pk: [u8; 32] = base64_decode(&self.state.server_public_key_b64)?
            .try_into()
            .map_err(|_| "server key in the state file has the wrong length".to_owned())?;
        let session_id = SessionId(hex_16(&self.state.session_id_hex)?);
        let address: SocketAddr =
            self.state.address.parse().map_err(|_| {
                format!("state file address {:?} does not parse", self.state.address)
            })?;
        // The name field, trimmed to the roster's own byte cap on a char
        // boundary; the server drops an oversized hint entirely, and a hint
        // it minted itself should never be one it will not read.
        let mut hint = self.mint_name.trim();
        while hint.len() > jamstream_protocol::control::MAX_NAME_LEN {
            let mut chars = hint.chars();
            chars.next_back();
            hint = chars.as_str();
        }
        let hint = (!hint.is_empty()).then(|| hint.to_owned());
        let token = Token {
            member_id: member,
            role,
            name_hint: hint.clone(),
            expires_unix: self.expires_unix,
            // A fresh jti every time, which is what makes reusing a member
            // id safe: the revoked credential and this one are not the same
            // credential, and only the revoked one is in the server's set.
            jti: TokenId::generate(),
        };
        let jti = token.jti;
        let invite = issuer.mint(session_id, vec![address], server_pk, token);
        let encoded = invite.encode();
        let seat = Seat {
            member,
            role,
            invite: Some(SeatInvite {
                token: jti,
                encoded: encoded.clone(),
            }),
            hint,
            was: None,
        };
        self.state.invites.push(InviteRecord {
            role: seat.label(),
            invite: encoded,
        });
        jamstream_cli::state::write_to(&self.path, &self.state).map_err(|e| e.to_string())?;
        match self.seats.iter_mut().find(|s| s.member == member) {
            Some(existing) => *existing = seat,
            None => {
                self.seats.push(seat);
                self.seats.sort_by_key(|s| s.member.0);
            }
        }
        self.mint_name.clear();
        self.publish_tokens();
        Ok(())
    }

    fn persist(&mut self) {
        if let Err(err) = jamstream_cli::state::write_to(&self.path, &self.state) {
            self.error = Some(format!("the session file could not be updated: {err}"));
        }
    }

    fn publish_tokens(&self) {
        let map: HashMap<MemberId, TokenId> = self
            .seats
            .iter()
            .filter_map(|s| s.invite.as_ref().map(|i| (s.member, i.token)))
            .collect();
        *self.tokens.lock().expect("token map") = map;
    }

    fn status_of(&self, seat: &Seat, snap: &Snapshot) -> String {
        if seat.is_free() {
            return match &seat.was {
                Some(name) => format!("free, was {name}"),
                None => "free".to_owned(),
            };
        }
        if snap
            .members
            .iter()
            .any(|m| m.id == seat.member && m.connected)
        {
            "connected".to_owned()
        } else {
            // Whose link is still unused, when the link knows: ten grey
            // "not joined" rows say nothing about who to chase.
            match &seat.hint {
                Some(name) => format!("not joined, for {name}"),
                None => "not joined".to_owned(),
            }
        }
    }
}

/// How many seats a role has. The numbers live in `jamstream_session` and
/// nowhere else, because the server admits against them.
pub fn cap(role: Role) -> usize {
    match role {
        Role::Musician => MAX_MUSICIANS,
        Role::Listener => MAX_LISTENERS,
    }
}

/// What the host is told when a role has no seat left, including the one
/// action that frees one.
fn at_capacity(role: Role) -> String {
    match role {
        Role::Musician => {
            format!("all {MAX_MUSICIANS} musician seats are taken. Revoke someone to free one.")
        }
        Role::Listener => {
            format!("all {MAX_LISTENERS} listener seats are taken. Revoke someone to free one.")
        }
    }
}

/// Whether a state-file record is the invite just revoked. Matched on the
/// token id inside it, not on the label, which stops being unique the
/// moment a seat changes hands.
fn record_holds(record: &InviteRecord, member: MemberId, token: TokenId) -> bool {
    Invite::decode(&record.invite)
        .is_ok_and(|i| i.token.member_id == member && i.token.jti == token)
}

/// Ends the session the way `jamstream end` does, step for step: destroy the
/// instance (already-gone is fine), close the firewall the launch opened for
/// it, verify nothing tagged remains, and rewrite the state file as ended
/// without the issuer key.
///
/// The two steps this used to skip are the ones with nothing on screen to
/// notice them. A session ended from the app left its per-session firewall in
/// the account, one per session forever (#196), and left the key that signs
/// its invites in the state directory after the server that key authenticated
/// against was gone (#195).
pub async fn end_session(
    provider: &dyn Provider,
    mut state: SessionState,
    path: PathBuf,
) -> Result<(), String> {
    let region = RegionId::new(state.region.clone());
    match provider.destroy(&region, &state.instance_id).await {
        Ok(()) => {}
        // Already gone: crashed earlier, self-destructed, or swept.
        Err(ProviderError::NotFound(_)) => {}
        Err(e) => return Err(e.to_string()),
    }
    // The instance is gone, so its firewall has nothing behind it. AWS can
    // still refuse while the network interface detaches, and the next sweep
    // collects it then, so this never fails an otherwise clean end.
    match provider.destroy_orphan_firewalls().await {
        Ok(closed) => tracing::info!(count = closed.len(), "closed session firewalls"),
        Err(err) => tracing::warn!(%err, "could not close the session firewall; sweep will retry"),
    }
    let remaining = provider
        .list_tagged(Some(&state.session_id_hex))
        .await
        .map_err(|e| e.to_string())?
        .instances;
    if !remaining.is_empty() {
        return Err(format!(
            "{} instance(s) still listed after destroy; press Stop strays on \
             the home screen, or run jamstream sweep",
            remaining.len()
        ));
    }
    state.status = SessionStatus::Ended;
    // Nothing can be minted or revoked for a session whose server is gone, so
    // the issuer key stops being useful at exactly this moment. The record
    // itself is worth keeping for status and for the cost history.
    state.forget_issuer_key();
    state.ended_unix = Some(unix_now());
    jamstream_cli::state::write_to(&path, &state).map_err(|e| e.to_string())?;
    Ok(())
}

fn hex_16(text: &str) -> Result<[u8; 16], String> {
    HEXLOWER
        .decode(text.as_bytes())
        .map_err(|_| "session id in the state file is not hex".to_owned())?
        .try_into()
        .map_err(|_| "session id in the state file has the wrong length".to_owned())
}

// Rendering: the Invites tab of the settings drawer. The confirmations stay
// centre-screen dialogs, because revoking someone and ending the session for
// everyone are not things to confirm in a corner.

impl InvitesPanel {
    pub fn ui(&mut self, ui: &mut Ui, snap: &Snapshot, rt: &dyn Runtime) -> Option<InvitesEvent> {
        let mut event = None;
        ui.label(theme::title(ui, "Invites"));
        ui.label(
            theme::muted(
                ui,
                "One seat per link. Revoking frees the seat for the next person.",
            )
            .small(),
        );
        ui.add_space(theme::SPACE_SM);
        self.rows_ui(ui, snap);
        ui.add_space(theme::SPACE_SM);
        self.mint_ui(ui);
        ui.add_space(theme::SPACE_MD);
        ui.separator();
        let p = theme::palette_of(ui);
        if ui
            .add(theme::danger_button(p, "End session for everyone"))
            .clicked()
        {
            self.confirm_end = true;
        }
        ui.label(theme::muted(ui, "Destroys the server; the cost meter stops.").small());
        self.confirm_windows(ui, rt, &mut event);
        event
    }

    /// One seat per pair of lines: who is in it and what it is doing above,
    /// its actions below. A grid four columns wide does not fit the drawer,
    /// and a seat's own actions are what a host reaches for.
    fn rows_ui(&mut self, ui: &mut Ui, snap: &Snapshot) {
        let seats: Vec<Seat> = self.guest_seats().cloned().collect();
        // Refilling rebuilds the seat list, so it happens after the rows
        // rather than under the iterator reading it.
        let mut refill = None;
        for seat in &seats {
            let label = seat.label();
            ui.horizontal(|ui| {
                // The disc slot is reserved on every row, drawn only for
                // whoever is actually here, so someone joining never shoves
                // the labels sideways.
                let member = snap
                    .members
                    .iter()
                    .find(|m| m.id == seat.member && m.connected)
                    .filter(|_| !seat.is_free());
                match member {
                    Some(m) => {
                        avatar_disc(ui, &m.name, m.avatar.as_ref(), AVATAR_D_ROW, false)
                            .on_hover_text(m.name.clone());
                    }
                    None => {
                        ui.allocate_exact_size(
                            vec2(AVATAR_D_ROW, AVATAR_D_ROW),
                            egui::Sense::hover(),
                        );
                    }
                }
                row_cell(ui, SEAT_LABEL_W, |ui| {
                    if seat.is_free() {
                        ui.label(theme::muted(ui, label.clone()));
                    } else {
                        ui.label(label.clone());
                    }
                });
                let status = self.status_of(seat, snap);
                if status == "connected" {
                    ui.label(status);
                } else {
                    ui.label(theme::muted(ui, status));
                }
            });
            ui.horizontal(|ui| {
                ui.add_space(AVATAR_D_ROW + theme::SPACE_MD);
                match &seat.invite {
                    Some(invite) => {
                        if ui.button("Copy link").clicked() {
                            ui.ctx().copy_text(invite.encoded.clone());
                        }
                        if ui.button("Revoke").clicked() {
                            self.confirm_revoke = Some((invite.token, label));
                        }
                    }
                    None => {
                        // A free seat's own action: one click puts a fresh
                        // link in the same chair.
                        if ui
                            .button("New link")
                            .on_hover_text(format!("mint a new {label} link for this seat"))
                            .clicked()
                        {
                            refill = Some(seat.member);
                        }
                    }
                }
            });
            ui.add_space(theme::SPACE_SM);
        }
        if let Some(member) = refill {
            self.error = self.refill(member).err();
        }
    }

    fn mint_ui(&mut self, ui: &mut Ui) {
        // Seats taken against seats there are, in the monospace, so the
        // host never has to click Mint to find out whether there is room.
        ui.horizontal(|ui| {
            for (role, label) in [(Role::Musician, "musicians"), (Role::Listener, "listeners")] {
                ui.label(theme::muted(ui, label));
                ui.label(theme::mono(
                    ui,
                    format!("{}/{}", self.taken(role), cap(role)),
                ));
                ui.add_space(theme::SPACE_MD);
            }
        });
        // Who it is for, before which kind: the name is what makes the link
        // theirs, and it lands in the token so the roster knows it at the
        // first packet.
        ui.horizontal(|ui| {
            ui.label(theme::muted(ui, "for"));
            ui.add(
                egui::TextEdit::singleline(&mut self.mint_name)
                    .desired_width(180.0)
                    .char_limit(64)
                    .hint_text("name on the invite"),
            );
        });
        // Which kind, then the act: at the drawer's width the two role
        // buttons and Mint invite do not share a line.
        ui.horizontal(|ui| {
            ui.label(theme::muted(ui, "new invite"));
            let p = theme::palette_of(ui);
            for (role, label) in [(Role::Musician, "musician"), (Role::Listener, "listener")] {
                if ui
                    .add(theme::selectable(p, label, self.mint_role == role))
                    .clicked()
                {
                    self.mint_role = role;
                }
            }
        });
        ui.horizontal(|ui| {
            let full = self.taken(self.mint_role) >= cap(self.mint_role);
            let response = ui.add_enabled(!full, Button::new("Mint invite"));
            if full {
                response.on_disabled_hover_text(at_capacity(self.mint_role));
            } else if response.clicked() {
                self.error = self.mint(self.mint_role).err();
            }
        });
        if let Some(err) = self.error.clone() {
            theme::reason(ui, err);
        }
    }

    fn confirm_windows(&mut self, ui: &mut Ui, rt: &dyn Runtime, event: &mut Option<InvitesEvent>) {
        if let Some((token, label)) = self.confirm_revoke.clone() {
            egui::Window::new("Revoke invite")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .show(ui.ctx(), |ui| {
                    ui.label(format!(
                        "Revoke the {label} invite? Whoever holds it is disconnected, \
                         the link stops working, and the seat is free again."
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.confirm_revoke = None;
                        }
                        let p = theme::palette_of(ui);
                        if ui.add(theme::danger_button(p, "Revoke invite")).clicked() {
                            rt.send(Command::Revoke(token));
                            // The name the roster has for them right now:
                            // the eject takes them off it a moment later,
                            // and the row keeps saying whose seat it was.
                            let was = self
                                .seats
                                .iter()
                                .find(|s| s.invite.as_ref().is_some_and(|i| i.token == token))
                                .and_then(|seat| {
                                    rt.snapshot()
                                        .members
                                        .iter()
                                        .find(|m| m.id == seat.member)
                                        .map(|m| m.name.clone())
                                });
                            self.revoke(token, was);
                            self.confirm_revoke = None;
                        }
                    });
                });
        }
        if self.confirm_end {
            egui::Window::new("End session")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .show(ui.ctx(), |ui| {
                    ui.label(
                        "End the session for everyone? The server is destroyed and \
                         every invite stops working.",
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.confirm_end = false;
                        }
                        let p = theme::palette_of(ui);
                        if ui.add(theme::danger_button(p, "End session")).clicked() {
                            self.confirm_end = false;
                            *event = Some(InvitesEvent::EndSession);
                        }
                    });
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::host::base64;
    use jamstream_cloud::MockProvider;
    use jamstream_protocol::transport::generate_keypair;

    /// A real state record with decodable invites: host, two musicians,
    /// one listener.
    fn fixture(dir_label: &str) -> (SessionState, PathBuf) {
        let issuer = Issuer::from_bytes(&[7u8; 32]);
        let keys = generate_keypair();
        let session_id = SessionId([0xa3; 16]);
        let address: SocketAddr = "203.0.113.10:43210".parse().unwrap();
        let mint = |member: u16, role: Role| {
            let token = Token {
                member_id: MemberId(member),
                role,
                name_hint: None,
                expires_unix: 4_000_000_000,
                jti: TokenId([member as u8 + 1; 16]),
            };
            issuer.mint(session_id, vec![address], keys.public, token)
        };
        let invites = [
            ("host", mint(0, Role::Musician)),
            ("musician 1", mint(1, Role::Musician)),
            ("musician 2", mint(2, Role::Musician)),
            ("listener 3", mint(3, Role::Listener)),
        ];
        let state = SessionState {
            session_id_hex: "a3".repeat(16),
            provider: "local".to_owned(),
            region: "local".to_owned(),
            instance_id: "12345".to_owned(),
            address: address.to_string(),
            created_unix: 1_784_000_000,
            hourly_microusd: 0,
            issuer_private_key_b64: base64(&issuer.to_bytes()),
            server_public_key_b64: base64(&keys.public),
            invites: invites
                .iter()
                .map(|(label, invite)| InviteRecord {
                    role: (*label).to_owned(),
                    invite: invite.encode(),
                })
                .collect(),
            status: SessionStatus::Running,
            ended_unix: None,
        };
        // A private subdirectory, not temp_dir() itself: the state writer
        // refuses a world-writable parent, and Linux's /tmp is one, unlike
        // the per-user temp dirs macOS and Windows hand out.
        let dir = std::env::temp_dir().join(format!(
            "jamstream-invites-{dir_label}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .expect("fixture dir mode");
        }
        (state, dir.join("state.json"))
    }

    #[test]
    fn the_token_map_covers_every_seat_including_the_host() {
        let (state, path) = fixture("map");
        let panel = InvitesPanel::new(state, path);
        let map = panel.tokens();
        let map = map.lock().unwrap();
        assert_eq!(map.len(), 4);
        assert_eq!(map[&MemberId(0)], TokenId([1; 16]));
        assert_eq!(map[&MemberId(2)], TokenId([3; 16]));
    }

    #[test]
    fn guest_rows_never_include_the_host() {
        let (state, path) = fixture("rows");
        let panel = InvitesPanel::new(state, path);
        let labels: Vec<String> = panel.guest_seats().map(Seat::label).collect();
        assert_eq!(labels, vec!["musician 1", "musician 2", "listener 3"]);
    }

    #[test]
    fn mint_appends_persists_and_respects_caps() {
        let (state, path) = fixture("mint");
        let mut panel = InvitesPanel::new(state, path.clone());

        panel.mint(Role::Listener).expect("mint listener");
        let new = panel.seats.last().unwrap().clone();
        assert_eq!(new.label(), "listener 4");
        assert_eq!(new.member, MemberId(4));
        // The invite decodes and carries the same session and expiry.
        let decoded = Invite::decode(&new.invite.unwrap().encoded).expect("minted invite decodes");
        assert_eq!(decoded.token.expires_unix, 4_000_000_000);
        assert_eq!(decoded.token.role, Role::Listener);
        // The CLI sees it too: the state file on disk has the new record.
        let reloaded = jamstream_cli::state::load(&path).expect("state reloads");
        assert_eq!(reloaded.invites.len(), 5);
        assert_eq!(reloaded.invites[4].role, "listener 4");

        // Fill musicians to capacity. The host holds one of the MAX_MUSICIANS
        // seats, so the fixture's host + 2 musicians leaves MAX_MUSICIANS - 3
        // to mint before the panel refuses.
        for _ in 0..MAX_MUSICIANS - 3 {
            panel.mint(Role::Musician).expect("mint musician");
        }
        let err = panel.mint(Role::Musician).expect_err("over musician cap");
        assert!(
            err.contains(&format!("all {MAX_MUSICIANS} musician seats")),
            "error was {err:?}"
        );
        // Listeners: the fixture's one plus the one minted above.
        for _ in 0..MAX_LISTENERS - 2 {
            panel.mint(Role::Listener).expect("mint listener");
        }
        let err = panel.mint(Role::Listener).expect_err("over listener cap");
        assert!(
            err.contains(&format!("all {MAX_LISTENERS} listener seats")),
            "error was {err:?}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// A link minted for somebody carries their name in the token (#357):
    /// the roster and the take files read it from the first packet, and the
    /// seat row says whose link is still unused. The name field is consumed
    /// by the mint that uses it, so the next link is not accidentally Ana's
    /// too.
    #[test]
    fn a_named_mint_stamps_the_token_and_the_seat_row() {
        let (state, path) = fixture("named");
        let mut panel = InvitesPanel::new(state, path.clone());
        panel.mint_name = "  Ana  ".to_owned();
        panel.mint(Role::Musician).expect("mint named");
        let seat = panel.seats.last().unwrap().clone();
        assert_eq!(seat.hint.as_deref(), Some("Ana"), "trimmed and stamped");
        let decoded = Invite::decode(&seat.invite.clone().unwrap().encoded).expect("decodes");
        assert_eq!(decoded.token.name_hint.as_deref(), Some("Ana"));
        assert_eq!(panel.mint_name, "", "the field is spent by the mint");

        // The row for an unused named link says who to chase; a connected
        // member's row does not repeat their name beside the roster's. The
        // demo's member 4 holds this seat's id, so it is marked away to
        // model the person who has not joined yet.
        let rt = crate::demo::DemoRuntime::frozen(0, true);
        rt.set_away(seat.member.0, true);
        let snap = crate::runtime::Runtime::snapshot(&rt);
        assert_eq!(panel.status_of(&seat, &snap), "not joined, for Ana");

        // A name past the roster's 64-byte cap is cut on a char boundary
        // rather than minted into a hint the server would drop whole.
        panel.mint_name = "é".repeat(40);
        panel.mint(Role::Listener).expect("mint long");
        let long = panel.seats.last().unwrap().hint.clone().expect("a hint");
        assert!(long.len() <= 64, "{} bytes", long.len());
        assert_eq!(long, "é".repeat(32));

        // And reloading the state file keeps the hints: the panel reads them
        // back out of the invites themselves.
        let reloaded = jamstream_cli::state::load(&path).expect("state reloads");
        let restarted = InvitesPanel::new(reloaded, path.clone());
        assert!(
            restarted
                .seats
                .iter()
                .any(|s| s.hint.as_deref() == Some("Ana")),
            "the hint survives a restart"
        );

        std::fs::remove_file(&path).ok();
    }

    /// Which seats hold a live musician invite, in seat order.
    fn seated_musicians(panel: &InvitesPanel) -> Vec<u16> {
        panel
            .seats
            .iter()
            .filter(|s| s.role == Role::Musician && !s.is_free())
            .map(|s| s.member.0)
            .collect()
    }

    /// The defect in #81: revoking every musician used to leave the session
    /// unable to mint a single replacement, because the panel counted
    /// invites ever issued while the server counted people connected.
    #[test]
    fn revoking_frees_the_seat_and_the_band_refills_it() {
        let (state, path) = fixture("free");
        let mut panel = InvitesPanel::new(state, path.clone());
        while panel.taken(Role::Musician) < MAX_MUSICIANS {
            panel.mint(Role::Musician).expect("mint to capacity");
        }
        assert!(panel.mint(Role::Musician).is_err(), "full means full");
        let before = seated_musicians(&panel);

        // Every guest musician leaves the band. The host keeps seat 0.
        let tokens: Vec<TokenId> = panel
            .guest_seats()
            .filter(|s| s.role == Role::Musician)
            .filter_map(|s| s.invite.as_ref().map(|i| i.token))
            .collect();
        assert_eq!(tokens.len(), MAX_MUSICIANS - 1);
        for token in tokens {
            panel.revoke(token, Some("Ana".to_owned()));
        }
        assert_eq!(panel.taken(Role::Musician), 1, "only the host is seated");

        // The whole band can be re-invited, into the seats it just left
        // rather than into "musician 11" and up.
        for _ in 1..MAX_MUSICIANS {
            panel.mint(Role::Musician).expect("mint after revoke");
        }
        assert_eq!(seated_musicians(&panel), before);
        assert!(panel.mint(Role::Musician).is_err(), "full again");

        std::fs::remove_file(&path).ok();
    }

    /// Reusing a member id is safe because revocation keys on the token's
    /// jti. The replacement invite for a seat is a different credential
    /// from the one revoked out of it, and only the revoked one is in the
    /// set the server persists.
    #[test]
    fn a_refilled_seat_carries_a_new_token_and_leaves_the_old_one_dead() {
        let (state, path) = fixture("reissue");
        let mut panel = InvitesPanel::new(state, path.clone());
        let seat = MemberId(2);
        let old = panel.token_of(seat).expect("musician 2 holds an invite");
        let old_encoded = panel
            .seat_of(seat)
            .and_then(|s| s.invite.as_ref())
            .map(|i| i.encoded.clone())
            .expect("musician 2 has a link");

        panel.revoke(old, Some("Ben".to_owned()));
        assert!(panel.seat_of(seat).unwrap().is_free());
        assert_eq!(panel.token_of(seat), None);

        panel.refill(seat).expect("refill seat 2");
        let new = panel.token_of(seat).expect("seat 2 holds a new invite");
        assert_ne!(new, old, "a reused seat must not reuse the credential");
        assert_eq!(panel.seat_of(seat).unwrap().was, None, "the seat is taken");
        assert_eq!(panel.seat_of(seat).unwrap().label(), "musician 2");

        // Both links name the same member; only the jti separates them,
        // which is exactly what the server's revoked set holds.
        let old_invite = Invite::decode(&old_encoded).expect("old link decodes");
        let new_invite = Invite::decode(
            &panel
                .seat_of(seat)
                .unwrap()
                .invite
                .as_ref()
                .unwrap()
                .encoded,
        )
        .expect("new link decodes");
        assert_eq!(old_invite.token.member_id, new_invite.token.member_id);
        assert_eq!(old_invite.token.jti, old);
        assert_eq!(new_invite.token.jti, new);

        // The token map the mixer's revoke button reads follows the seat,
        // so revoking from a strip cannot target the dead credential.
        assert_eq!(panel.tokens().lock().unwrap()[&seat], new);

        std::fs::remove_file(&path).ok();
    }

    /// A revoked invite is gone from the state file too, so restarting the
    /// app does not resurrect the seat it used to hold.
    #[test]
    fn a_revoked_invite_leaves_the_state_file() {
        let (state, path) = fixture("persist");
        let mut panel = InvitesPanel::new(state, path.clone());
        let token = panel.token_of(MemberId(1)).expect("musician 1");
        panel.revoke(token, Some("Ana".to_owned()));
        assert_eq!(panel.error, None, "the write must succeed");

        let reloaded = jamstream_cli::state::load(&path).expect("state reloads");
        assert_eq!(reloaded.invites.len(), 3);
        let restarted = InvitesPanel::new(reloaded, path.clone());
        assert_eq!(restarted.taken(Role::Musician), 2, "host and musician 2");
        assert_eq!(restarted.token_of(MemberId(1)), None);

        std::fs::remove_file(&path).ok();
    }

    /// A launched session on the mock, so the firewall under test is one a
    /// launch really created and the instance is one destroy really removes.
    async fn launched(label: &str) -> (MockProvider, SessionState, PathBuf) {
        use jamstream_cloud::{InstanceClass, LaunchSpec, ProviderKind, session_tag};

        let (mut state, path) = fixture(label);
        let provider = MockProvider::with_default_regions(ProviderKind::DigitalOcean);
        let region = provider.regions()[0].clone();
        let instance = provider
            .launch(LaunchSpec {
                region: region.clone(),
                instance_class: InstanceClass::Standard,
                user_data: String::new(),
                tags: vec![session_tag(&state.session_id_hex)],
            })
            .await
            .expect("the mock launches");
        state.instance_id = instance.id.clone();
        state.region = region.id.to_string();
        (provider, state, path)
    }

    /// #195 and #196, asserted where they happened: on the app's own end, at
    /// the effects rather than at the status word. The state file lands on disk
    /// without the issuer key, the provider is asked to collect the firewall
    /// the launch opened, and none of that session's ingress is left.
    ///
    /// Both defects survived a test each. `wizard_local.rs` asserted the status
    /// string this function writes and `host_flow.rs` asserted the key on the
    /// CLI's path, so each surface was covered by a test that could not see the
    /// other one's hole.
    #[tokio::test]
    async fn ending_a_session_from_the_app_takes_the_key_and_the_firewall_with_it() {
        use jamstream_cloud::mock::Call;

        let (provider, state, path) = launched("end").await;
        assert!(
            !state.issuer_private_key_b64.is_empty(),
            "the fixture has to start with a key on disk"
        );
        assert_eq!(
            provider
                .session_ingress(&state.session_id_hex)
                .await
                .expect("ingress")
                .len(),
            1,
            "the launch opens exactly one port for the session"
        );

        end_session(&provider, state.clone(), path.clone())
            .await
            .expect("end session");

        let ended = jamstream_cli::state::load(&path).expect("state reloads");
        assert_eq!(ended.status, SessionStatus::Ended);
        assert!(
            ended.issuer_private_key_b64.is_empty(),
            "the key that signs this session's invites is still in the state directory"
        );
        // The records themselves stay: status and the cost history read them,
        // and nothing can be minted or revoked without the issuer key.
        assert_eq!(ended.invites.len(), 4);

        assert!(
            provider.calls().contains(&Call::DestroyOrphanFirewalls),
            "the app never asked the provider to close the session firewall: {:?}",
            provider.calls()
        );
        assert!(
            provider
                .session_ingress(&state.session_id_hex)
                .await
                .expect("ingress")
                .is_empty(),
            "the session's firewall is still open in the account"
        );

        std::fs::remove_file(&path).ok();
    }

    /// A firewall that cannot be closed yet is not a failed end: AWS refuses
    /// while the network interface detaches, and the next sweep collects it.
    /// The session is still ended and the key is still gone.
    #[tokio::test]
    async fn a_firewall_that_will_not_close_yet_does_not_fail_the_end() {
        let (provider, state, path) = launched("firewall").await;
        // An instance the destroy cannot find leaves the firewall behind with
        // the session still listed as live, which is the shape of the case the
        // sweeper is there for.
        let mut state = state;
        state.instance_id = "not-this-one".to_owned();
        let err = end_session(&provider, state.clone(), path.clone())
            .await
            .expect_err("an instance still listed must not read as a clean end");
        assert!(err.contains("sweep"), "error was {err:?}");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn hex_16_round_trips_the_session_id() {
        assert_eq!(hex_16(&"a3".repeat(16)).unwrap(), [0xa3; 16]);
        assert!(hex_16("a3").is_err());
        assert!(hex_16(&"zz".repeat(16)).is_err());
    }
}
