//! The host's invites panel on the session screen: every non-host invite
//! from the shared state file, its live status, per-row copy and revoke,
//! minting new invites within capacity, and the end-session action. The
//! host's own invite exists in the state file (the app joined with it) but
//! is never rendered anywhere.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use egui::{Align2, Button, RichText, Ui, vec2};
use jamstream_cli::state::{InviteRecord, SessionState, SessionStatus};
use jamstream_cloud::{Provider, ProviderError, RegionId};
use jamstream_protocol::ids::{HOST_MEMBER_ID, MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};

use crate::runtime::{Command, Runtime, Snapshot};
use crate::screens::host::{base64_decode, unix_now};
use crate::theme;

/// Session capacity, host included on the musician side.
pub const MAX_MUSICIANS: usize = 10;
pub const MAX_LISTENERS: usize = 20;

/// One invite from the state file, with its token id decoded so revocation
/// and roster matching work.
#[derive(Debug, Clone, PartialEq)]
pub struct InviteEntry {
    pub label: String,
    pub role: Role,
    pub member: MemberId,
    pub token: TokenId,
    pub encoded: String,
}

/// What the panel asks the session screen to bubble up.
pub enum InvitesEvent {
    /// Leave, destroy the server, mark the state file ended.
    EndSession,
}

pub struct InvitesPanel {
    pub state: SessionState,
    pub path: PathBuf,
    pub entries: Vec<InviteEntry>,
    /// Tokens revoked from this app; the server keeps no queryable list,
    /// so the host's own action log is the record.
    pub revoked: Vec<TokenId>,
    pub mint_role: Role,
    pub mint_error: Option<String>,
    pub confirm_revoke: Option<(TokenId, String)>,
    pub confirm_end: bool,
    expires_unix: u64,
}

impl InvitesPanel {
    /// Decodes the invite book out of the state record. Undecodable
    /// entries are dropped with a warning rather than wedging the panel.
    pub fn new(state: SessionState, path: PathBuf) -> InvitesPanel {
        let mut entries = Vec::with_capacity(state.invites.len());
        let mut expires_unix = 0;
        for record in &state.invites {
            match Invite::decode(&record.invite) {
                Ok(invite) => {
                    expires_unix = expires_unix.max(invite.token.expires_unix);
                    entries.push(InviteEntry {
                        label: record.role.clone(),
                        role: invite.token.role,
                        member: invite.token.member_id,
                        token: invite.token.jti,
                        encoded: record.invite.clone(),
                    });
                }
                Err(err) => {
                    tracing::warn!(%err, label = %record.role, "state file invite does not decode");
                }
            }
        }
        if expires_unix == 0 {
            expires_unix = unix_now() + 12 * 3600;
        }
        InvitesPanel {
            state,
            path,
            entries,
            revoked: Vec::new(),
            mint_role: Role::Musician,
            mint_error: None,
            confirm_revoke: None,
            confirm_end: false,
            expires_unix,
        }
    }

    /// Member id to token id for every invite, host included; the
    /// [`crate::live::CostedRuntime`] wrapper injects this into snapshots
    /// so the mixer's revoke buttons have something to send.
    pub fn token_map(&self) -> HashMap<MemberId, TokenId> {
        self.entries.iter().map(|e| (e.member, e.token)).collect()
    }

    /// The rows the panel shows: everyone but the host.
    pub fn guest_entries(&self) -> impl Iterator<Item = &InviteEntry> {
        self.entries.iter().filter(|e| e.member != HOST_MEMBER_ID)
    }

    pub fn is_revoked(&self, token: TokenId) -> bool {
        self.revoked.contains(&token)
    }

    pub fn mark_revoked(&mut self, token: TokenId) {
        if !self.revoked.contains(&token) {
            self.revoked.push(token);
        }
    }

    fn count(&self, role: Role) -> usize {
        self.entries.iter().filter(|e| e.role == role).count()
    }

    /// Mints one more invite with the issuer key from the state file and
    /// appends it to the same file, so `jamstream status` sees it too.
    /// Refuses past capacity (10 musicians including the host, 20
    /// listeners).
    pub fn mint(&mut self, role: Role) -> Result<(), String> {
        match role {
            Role::Musician if self.count(Role::Musician) >= MAX_MUSICIANS => {
                return Err(format!(
                    "the session is at its capacity of {MAX_MUSICIANS} musicians"
                ));
            }
            Role::Listener if self.count(Role::Listener) >= MAX_LISTENERS => {
                return Err(format!(
                    "the session is at its capacity of {MAX_LISTENERS} listeners"
                ));
            }
            _ => {}
        }
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
        let member = MemberId(
            self.entries
                .iter()
                .map(|e| e.member.0)
                .max()
                .unwrap_or(HOST_MEMBER_ID.0)
                + 1,
        );
        let token = Token {
            member_id: member,
            role,
            name_hint: None,
            expires_unix: self.expires_unix,
            jti: TokenId::generate(),
        };
        let jti = token.jti;
        let invite = issuer.mint(session_id, vec![address], server_pk, token);
        let label = jamstream_cli::host::invite_label(role, member);
        let encoded = invite.encode();
        self.state.invites.push(InviteRecord {
            role: label.clone(),
            invite: encoded.clone(),
        });
        jamstream_cli::state::write_to(&self.path, &self.state).map_err(|e| e.to_string())?;
        self.entries.push(InviteEntry {
            label,
            role,
            member,
            token: jti,
            encoded,
        });
        Ok(())
    }

    fn status_of(&self, entry: &InviteEntry, snap: &Snapshot) -> &'static str {
        if self.is_revoked(entry.token) {
            "revoked"
        } else if snap
            .members
            .iter()
            .any(|m| m.id == entry.member && m.connected)
        {
            "connected"
        } else {
            "not joined"
        }
    }
}

/// Ends the session the way `jamstream end` does: destroy the instance
/// (already-gone is fine), verify nothing tagged remains, and rewrite the
/// state file as ended.
pub async fn end_session(
    provider: Box<dyn Provider>,
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
    let remaining = provider
        .list_tagged(Some(&state.session_id_hex))
        .await
        .map_err(|e| e.to_string())?;
    if !remaining.is_empty() {
        return Err(format!(
            "{} instance(s) still listed after destroy; run jamstream sweep",
            remaining.len()
        ));
    }
    state.status = SessionStatus::Ended;
    state.ended_unix = Some(unix_now());
    jamstream_cli::state::write_to(&path, &state).map_err(|e| e.to_string())?;
    Ok(())
}

fn hex_16(text: &str) -> Result<[u8; 16], String> {
    if text.len() != 32 {
        return Err("session id in the state file has the wrong length".to_owned());
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16)
            .map_err(|_| "session id in the state file is not hex".to_owned())?;
    }
    Ok(out)
}

// Rendering: a sheet anchored under the top bar on the right, the same
// treatment as the settings sheet, so it never covers the status readout.

impl InvitesPanel {
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        snap: &Snapshot,
        rt: &dyn Runtime,
        open: &mut bool,
    ) -> Option<InvitesEvent> {
        let mut event = None;
        let panel = {
            let p = theme::palette_of(ui);
            egui::Frame::new()
                .fill(p.surface1)
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(egui::CornerRadius::same(theme::RADIUS))
                .inner_margin(egui::Margin::same(14))
        };
        egui::Window::new("Invites")
            .title_bar(false)
            .frame(panel)
            .anchor(Align2::RIGHT_TOP, vec2(-10.0, 56.0))
            .fixed_size(vec2(430.0, 0.0))
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.horizontal(|ui| {
                    ui.label(theme::title(ui, "Invites"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            *open = false;
                        }
                    });
                });
                ui.label(theme::muted(
                    ui,
                    "Each link admits one person. Send each musician their own.",
                ));
                ui.add_space(theme::SPACE_SM);
                self.rows_ui(ui, snap);
                ui.add_space(theme::SPACE_SM);
                self.mint_ui(ui);
                ui.add_space(theme::SPACE_MD);
                ui.separator();
                let p = theme::palette_of(ui);
                if ui
                    .add(
                        Button::new(
                            RichText::new("End session for everyone").color(egui::Color32::WHITE),
                        )
                        .fill(p.danger),
                    )
                    .clicked()
                {
                    self.confirm_end = true;
                }
                ui.label(theme::muted(
                    ui,
                    "Destroys the server; the cost meter stops.",
                ));
            });
        self.confirm_windows(ui, rt, &mut event);
        event
    }

    fn rows_ui(&mut self, ui: &mut Ui, snap: &Snapshot) {
        let rows: Vec<InviteEntry> = self.guest_entries().cloned().collect();
        egui::Grid::new("invites-grid")
            .num_columns(4)
            .min_col_width(72.0)
            .spacing(vec2(theme::SPACE_LG, 4.0))
            .show(ui, |ui| {
                for entry in &rows {
                    ui.label(entry.label.clone());
                    let status = self.status_of(entry, snap);
                    match status {
                        "connected" => ui.label(status),
                        _ => ui.label(theme::muted(ui, status)),
                    };
                    let revoked = self.is_revoked(entry.token);
                    if ui.add_enabled(!revoked, Button::new("Copy link")).clicked() {
                        ui.ctx().copy_text(entry.encoded.clone());
                    }
                    if ui.add_enabled(!revoked, Button::new("Revoke")).clicked() {
                        self.confirm_revoke = Some((entry.token, entry.label.clone()));
                    }
                    ui.end_row();
                }
            });
    }

    fn mint_ui(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(theme::muted(ui, "new invite"));
            for (role, label) in [(Role::Musician, "musician"), (Role::Listener, "listener")] {
                if ui
                    .add(Button::new(label).selected(self.mint_role == role))
                    .clicked()
                {
                    self.mint_role = role;
                }
            }
            if ui.button("Mint invite").clicked() {
                self.mint_error = self.mint(self.mint_role).err();
            }
        });
        if let Some(err) = &self.mint_error {
            let p = theme::palette_of(ui);
            ui.label(RichText::new(err.clone()).color(p.danger));
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
                        "Revoke the {label} invite? Whoever holds it is disconnected \
                         and the link stops working."
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.confirm_revoke = None;
                        }
                        let p = theme::palette_of(ui);
                        if ui
                            .add(
                                Button::new(
                                    RichText::new("Revoke invite").color(egui::Color32::WHITE),
                                )
                                .fill(p.danger),
                            )
                            .clicked()
                        {
                            rt.send(Command::Revoke(token));
                            self.mark_revoked(token);
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
                        if ui
                            .add(
                                Button::new(
                                    RichText::new("End session").color(egui::Color32::WHITE),
                                )
                                .fill(p.danger),
                            )
                            .clicked()
                        {
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
        let path = std::env::temp_dir().join(format!(
            "jamstream-invites-{dir_label}-{}.json",
            std::process::id()
        ));
        (state, path)
    }

    #[test]
    fn token_map_covers_every_invite_including_the_host() {
        let (state, path) = fixture("map");
        let panel = InvitesPanel::new(state, path);
        let map = panel.token_map();
        assert_eq!(map.len(), 4);
        assert_eq!(map[&MemberId(0)], TokenId([1; 16]));
        assert_eq!(map[&MemberId(2)], TokenId([3; 16]));
    }

    #[test]
    fn guest_rows_never_include_the_host() {
        let (state, path) = fixture("rows");
        let panel = InvitesPanel::new(state, path);
        let labels: Vec<&str> = panel.guest_entries().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, vec!["musician 1", "musician 2", "listener 3"]);
    }

    #[test]
    fn mint_appends_persists_and_respects_caps() {
        let (state, path) = fixture("mint");
        let mut panel = InvitesPanel::new(state, path.clone());

        panel.mint(Role::Listener).expect("mint listener");
        let new = panel.entries.last().unwrap().clone();
        assert_eq!(new.label, "listener 4");
        assert_eq!(new.member, MemberId(4));
        // The invite decodes and carries the same session and expiry.
        let decoded = Invite::decode(&new.encoded).expect("minted invite decodes");
        assert_eq!(decoded.token.expires_unix, 4_000_000_000);
        assert_eq!(decoded.token.role, Role::Listener);
        // The CLI sees it too: the state file on disk has the new record.
        let reloaded = jamstream_cli::state::load(&path).expect("state reloads");
        assert_eq!(reloaded.invites.len(), 5);
        assert_eq!(reloaded.invites[4].role, "listener 4");

        // Fill musicians to capacity (host + 2 exist, cap 10).
        for _ in 0..7 {
            panel.mint(Role::Musician).expect("mint musician");
        }
        let err = panel.mint(Role::Musician).expect_err("over musician cap");
        assert!(err.contains("10 musicians"), "error was {err:?}");
        // Listeners: 2 exist, cap 20.
        for _ in 0..18 {
            panel.mint(Role::Listener).expect("mint listener");
        }
        let err = panel.mint(Role::Listener).expect_err("over listener cap");
        assert!(err.contains("20 listeners"), "error was {err:?}");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn revocation_is_tracked_locally() {
        let (state, path) = fixture("revoke");
        let mut panel = InvitesPanel::new(state, path);
        let token = panel.entries[1].token;
        assert!(!panel.is_revoked(token));
        panel.mark_revoked(token);
        panel.mark_revoked(token);
        assert!(panel.is_revoked(token));
        assert_eq!(panel.revoked.len(), 1);
    }

    #[test]
    fn hex_16_round_trips_the_session_id() {
        assert_eq!(hex_16(&"a3".repeat(16)).unwrap(), [0xa3; 16]);
        assert!(hex_16("a3").is_err());
        assert!(hex_16(&"zz".repeat(16)).is_err());
    }
}
