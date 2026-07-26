//! The host wizard: provider, region, cost preview, launch, invites. The
//! state machine is plain data with function-per-transition so it tests
//! without a Ui. The mock provider runs the full flow today; real provider
//! launching arrives with the networking pass.

use egui::{RichText, Ui, vec2};
use jamstream_cloud::{
    BootConfig, CostPreview, InstanceClass, LaunchSpec, Price, Region, SelfDestruct, rank,
    session_tag,
};
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Issuer, Token};
use jamstream_protocol::transport::generate_keypair;

use crate::theme;
use crate::widgets::{PICK_INDENT, pick_row, row_cell};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    pub name: String,
    pub available: bool,
    /// Which env credentials were found, or why the provider is unusable.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegionRow {
    pub region: Region,
    pub price: Price,
    pub worst_rtt_ms: f32,
    /// True when the latency figure is fabricated rather than probed.
    pub fabricated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOutcome {
    pub session_short: String,
    pub server_addr: String,
    /// (label, encoded invite) pairs, host first.
    pub invites: Vec<(String, String)>,
    pub state_path: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    Provider,
    Region,
    Preview,
    Launching,
    Done,
}

pub struct HostWizard {
    pub step: WizardStep,
    pub providers: Vec<ProviderRow>,
    pub selected_provider: Option<usize>,
    pub regions: Vec<RegionRow>,
    pub selected_region: Option<usize>,
    pub hours: f32,
    pub musicians: u8,
    pub listeners: u8,
    pub destinations: u8,
    pub outcome: Option<LaunchOutcome>,
}

/// What the wizard asks the app to do this frame.
pub enum WizardEvent {
    /// Perform the launch (next frame) and call [`HostWizard::finish_launch`].
    LaunchRequested,
    /// The user is done; go back to the home screen.
    Close,
}

// Pure state machine. Each transition validates its precondition and
// returns whether it happened, so tests can assert both directions.
impl HostWizard {
    pub fn new(providers: Vec<ProviderRow>) -> Self {
        HostWizard {
            step: WizardStep::Provider,
            providers,
            selected_provider: None,
            regions: Vec::new(),
            selected_region: None,
            hours: 2.0,
            musicians: 3,
            listeners: 2,
            destinations: 0,
            outcome: None,
        }
    }

    pub fn selected_provider_name(&self) -> Option<&str> {
        self.selected_provider
            .and_then(|i| self.providers.get(i))
            .map(|p| p.name.as_str())
    }

    /// Selecting an unavailable provider is refused.
    pub fn select_provider(&mut self, idx: usize) -> bool {
        if self.providers.get(idx).is_some_and(|p| p.available) {
            self.selected_provider = Some(idx);
            true
        } else {
            false
        }
    }

    /// Provider -> Region, fed with the ranked region rows.
    pub fn continue_to_region(&mut self, rows: Vec<RegionRow>) -> bool {
        if self.step == WizardStep::Provider && self.selected_provider.is_some() {
            self.regions = rows;
            self.selected_region = if self.regions.is_empty() {
                None
            } else {
                Some(0)
            };
            self.step = WizardStep::Region;
            true
        } else {
            false
        }
    }

    pub fn select_region(&mut self, idx: usize) -> bool {
        if idx < self.regions.len() {
            self.selected_region = Some(idx);
            true
        } else {
            false
        }
    }

    /// Region -> Preview.
    pub fn continue_to_preview(&mut self) -> bool {
        if self.step == WizardStep::Region && self.selected_region.is_some() {
            self.step = WizardStep::Preview;
            true
        } else {
            false
        }
    }

    pub fn preview(&self) -> Option<CostPreview> {
        let row = self.selected_region.and_then(|i| self.regions.get(i))?;
        Some(CostPreview::compute(
            &row.price,
            self.hours,
            self.musicians,
            self.destinations,
            self.listeners,
        ))
    }

    /// Preview -> Launching. Only the mock is wired end to end in this pass.
    pub fn begin_launch(&mut self) -> bool {
        if self.step == WizardStep::Preview
            && self.selected_region.is_some()
            && self.selected_provider_name() == Some("mock")
        {
            self.step = WizardStep::Launching;
            true
        } else {
            false
        }
    }

    /// Launching -> Done.
    pub fn finish_launch(&mut self, outcome: LaunchOutcome) -> bool {
        if self.step == WizardStep::Launching {
            self.outcome = Some(outcome);
            self.step = WizardStep::Done;
            true
        } else {
            false
        }
    }

    /// One step back; Launching and Done do not go back.
    pub fn back(&mut self) -> bool {
        match self.step {
            WizardStep::Region => {
                self.step = WizardStep::Provider;
                true
            }
            WizardStep::Preview => {
                self.step = WizardStep::Region;
                true
            }
            _ => false,
        }
    }
}

/// Provider availability from the process environment, via the same
/// `resolve` seam the CLI uses.
pub fn provider_rows_from_env() -> Vec<ProviderRow> {
    jamstream_cli::providers::KNOWN_PROVIDERS
        .iter()
        .map(|name| match jamstream_cli::providers::resolve(name) {
            Ok(_) => ProviderRow {
                name: (*name).to_owned(),
                available: true,
                detail: match *name {
                    "mock" => "runs locally, no credentials needed".to_owned(),
                    "aws" => "credentials found (AWS_ACCESS_KEY_ID)".to_owned(),
                    "digitalocean" => "credentials found (DIGITALOCEAN_TOKEN)".to_owned(),
                    "gcp" => "credentials found in environment".to_owned(),
                    _ => "credentials found".to_owned(),
                },
            },
            Err(err) => ProviderRow {
                name: (*name).to_owned(),
                available: false,
                detail: err.to_string(),
            },
        })
        .collect()
}

/// Region rows for a provider, ranked by the shared solver. Latency is
/// fabricated in this pass (the deterministic per-region figure the CLI
/// uses for the mock); real probes arrive with networking.
pub fn region_rows(provider_name: &str) -> Result<Vec<RegionRow>, String> {
    let provider = jamstream_cli::providers::resolve(provider_name).map_err(|e| e.to_string())?;
    let regions = provider.regions();
    let mut candidates: Vec<(Region, Price)> = Vec::with_capacity(regions.len());
    for region in &regions {
        let price = block_on(provider.price(&region.id)).map_err(|e| e.to_string())?;
        candidates.push((region.clone(), price));
    }
    let matrix = jamstream_cli::host::mock_matrix(&regions);
    Ok(rank(&matrix, &candidates)
        .into_iter()
        .map(|score| RegionRow {
            region: score.region,
            price: score.price,
            worst_rtt_ms: score.worst_rtt_ms,
            fabricated: true,
        })
        .collect())
}

const SESSION_PORT: u16 = 43210;
const MAX_HOURS: u64 = 12;
// The mock accepts placeholders; a real provider requires the verified
// artifact, which is why only the mock launches in this pass.
const PLACEHOLDER_ARTIFACT_URL: &str = "https://artifacts.invalid/jamstreamd";
const PLACEHOLDER_ARTIFACT_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Launches the selected (mock) provider, mints invites, and records the
/// session in the CLI state directory so it shows under recent sessions.
pub fn launch(wiz: &HostWizard) -> LaunchOutcome {
    match try_launch(wiz) {
        Ok(outcome) => outcome,
        Err(err) => LaunchOutcome {
            session_short: String::new(),
            server_addr: String::new(),
            invites: Vec::new(),
            state_path: None,
            error: Some(err),
        },
    }
}

fn try_launch(wiz: &HostWizard) -> Result<LaunchOutcome, String> {
    let provider_name = wiz.selected_provider_name().ok_or("no provider selected")?;
    let row = wiz
        .selected_region
        .and_then(|i| wiz.regions.get(i))
        .ok_or("no region selected")?;
    let provider = jamstream_cli::providers::resolve(provider_name).map_err(|e| e.to_string())?;

    let session_id = SessionId::generate();
    let session_hex: String = session_id.0.iter().map(|b| format!("{b:02x}")).collect();
    let issuer = Issuer::generate();
    let server_keys = generate_keypair();
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let expires_unix = now_unix + MAX_HOURS * 3600;

    let boot = BootConfig {
        artifact_url: PLACEHOLDER_ARTIFACT_URL.to_owned(),
        artifact_sha256: PLACEHOLDER_ARTIFACT_SHA256.to_owned(),
        server_private_key_b64: base64(&server_keys.private),
        issuer_public_key_b64: base64(issuer.public_key().as_bytes()),
        session_id_hex: session_hex.clone(),
        port: SESSION_PORT,
        idle_shutdown_min: 10,
        max_duration_min: (MAX_HOURS * 60) as u32,
        self_destruct: SelfDestruct::AwsShutdown,
    };
    let spec = LaunchSpec {
        region: row.region.clone(),
        instance_class: InstanceClass::Standard,
        user_data: jamstream_cloud::cloudinit::render(&boot),
        tags: vec![session_tag(&session_hex)],
    };
    let instance = block_on(provider.launch(spec)).map_err(|e| e.to_string())?;
    let ip = instance.public_ip.ok_or("instance reported no public ip")?;
    let address = std::net::SocketAddr::new(ip, SESSION_PORT);

    let mut invites: Vec<(String, String)> = Vec::new();
    let mut mint = |member: u16, role: Role| {
        let token = Token {
            member_id: MemberId(member),
            role,
            name_hint: None,
            expires_unix,
            jti: TokenId::generate(),
        };
        let invite = issuer.mint(session_id, vec![address], server_keys.public, token);
        invites.push((
            jamstream_cli::host::invite_label(role, MemberId(member)),
            invite.encode(),
        ));
    };
    mint(0, Role::Musician);
    for m in 1..=u16::from(wiz.musicians) {
        mint(m, Role::Musician);
    }
    for l in 0..u16::from(wiz.listeners) {
        mint(u16::from(wiz.musicians) + 1 + l, Role::Listener);
    }

    let state = jamstream_cli::state::SessionState {
        session_id_hex: session_hex.clone(),
        provider: provider_name.to_owned(),
        region: row.region.id.to_string(),
        instance_id: instance.id.clone(),
        address: address.to_string(),
        created_unix: now_unix,
        hourly_microusd: row.price.hourly_microusd,
        issuer_private_key_b64: base64(&issuer.to_bytes()),
        server_public_key_b64: base64(&server_keys.public),
        invites: invites
            .iter()
            .map(|(label, invite)| jamstream_cli::state::InviteRecord {
                role: label.clone(),
                invite: invite.clone(),
            })
            .collect(),
        status: jamstream_cli::state::SessionStatus::Running,
        ended_unix: None,
    };
    let state_path = jamstream_cli::state::save(&state)
        .map(|p| p.display().to_string())
        .ok();

    Ok(LaunchOutcome {
        session_short: session_hex.chars().take(8).collect(),
        server_addr: address.to_string(),
        invites,
        state_path,
        error: None,
    })
}

/// Standard base64 with padding; enough to fill the CLI state schema
/// without pulling an encoding crate into the client.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Minimal executor for provider futures. The mock resolves without real
/// io, so this never actually parks; it exists so the UI thread can call
/// async provider methods without a runtime dependency.
fn block_on<F: Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn noop_raw_waker() -> RawWaker {
        fn clone(_: *const ()) -> RawWaker {
            noop_raw_waker()
        }
        fn noop(_: *const ()) {}
        RawWaker::new(
            std::ptr::null(),
            &RawWakerVTable::new(clone, noop, noop, noop),
        )
    }
    let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = Box::pin(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(out) => return out,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

// Rendering. One focused card per step: the step counter and title live
// inside the card, back and continue at the bottom. Numbers are monospace
// throughout.

impl HostWizard {
    pub fn ui(&mut self, ui: &mut Ui) -> Option<WizardEvent> {
        let mut event = None;
        theme::focused_column(ui, 600.0, |ui| {
            theme::panel(ui)
                .inner_margin(egui::Margin::same(16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let title = match self.step {
                        WizardStep::Provider => "Where should the session server run?".to_owned(),
                        WizardStep::Region => "Pick a region".to_owned(),
                        WizardStep::Preview => "Cost preview".to_owned(),
                        WizardStep::Launching => format!(
                            "Launching in {}",
                            self.selected_region
                                .and_then(|i| self.regions.get(i))
                                .map(|r| r.region.id.to_string())
                                .unwrap_or_default()
                        ),
                        WizardStep::Done => match &self.outcome {
                            Some(o) if o.error.is_none() => {
                                format!("Session {} is running", o.session_short)
                            }
                            _ => "Launch failed".to_owned(),
                        },
                    };
                    let num = match self.step {
                        WizardStep::Provider => 1,
                        WizardStep::Region => 2,
                        WizardStep::Preview => 3,
                        WizardStep::Launching | WizardStep::Done => 4,
                    };
                    ui.label(theme::muted(ui, format!("Step {num} of 4")).small());
                    ui.add_space(theme::SPACE_XS);
                    let title_font = egui::FontId::new(16.0, theme::semibold(ui));
                    ui.label(RichText::new(title).font(title_font));
                    ui.add_space(theme::SPACE_LG);
                    match self.step {
                        WizardStep::Provider => self.provider_ui(ui),
                        WizardStep::Region => self.region_ui(ui),
                        WizardStep::Preview => {
                            if let Some(e) = self.preview_ui(ui) {
                                event = Some(e);
                            }
                        }
                        WizardStep::Launching => self.launching_ui(ui),
                        WizardStep::Done => {
                            if let Some(e) = self.done_ui(ui) {
                                event = Some(e);
                            }
                        }
                    }
                });
        });
        event
    }

    fn provider_ui(&mut self, ui: &mut Ui) {
        for i in 0..self.providers.len() {
            let row = self.providers[i].clone();
            let response = pick_row(
                ui,
                &row.name,
                self.selected_provider == Some(i),
                row.available,
                |ui| {
                    row_cell(ui, 110.0, |ui| {
                        ui.label(row.name.clone());
                    });
                    ui.label(theme::muted(ui, row.detail.clone()));
                },
            );
            if response.clicked() {
                self.select_provider(i);
            }
        }
        ui.add_space(theme::SPACE_LG);
        ui.horizontal(|ui| {
            let can_continue = self.selected_provider.is_some();
            if ui
                .add_enabled(can_continue, egui::Button::new("Continue"))
                .clicked()
                && let Some(name) = self.selected_provider_name().map(str::to_owned)
            {
                match region_rows(&name) {
                    Ok(rows) => {
                        self.continue_to_region(rows);
                    }
                    Err(err) => {
                        ui.label(RichText::new(err).color(theme::palette_of(ui).danger));
                    }
                }
            }
        });
    }

    fn region_ui(&mut self, ui: &mut Ui) {
        ui.label(theme::muted(ui, "Latency and price carry equal weight."));
        if self.regions.iter().any(|r| r.fabricated) {
            ui.label(theme::muted(
                ui,
                "Latency figures are fabricated; real network probes arrive with the networking pass.",
            ));
        }
        ui.add_space(theme::SPACE_MD);
        let cols = [130.0, 90.0, 110.0, 90.0];
        ui.horizontal(|ui| {
            ui.add_space(PICK_INDENT);
            for (label, w) in ["region", "worst rtt", "hourly", "egress"].iter().zip(cols) {
                row_cell(ui, w, |ui| {
                    ui.label(theme::muted(ui, *label).small());
                });
            }
        });
        for i in 0..self.regions.len() {
            let row = self.regions[i].clone();
            let rtt = if row.worst_rtt_ms.is_finite() {
                format!("{:.0} ms", row.worst_rtt_ms)
            } else {
                "no probe".to_owned()
            };
            let cells = [
                row.region.id.to_string(),
                rtt,
                row.price.hourly_display(),
                row.price.egress_display(),
            ];
            let response = pick_row(
                ui,
                &cells[0].clone(),
                self.selected_region == Some(i),
                true,
                |ui| {
                    row_cell(ui, cols[0], |ui| {
                        ui.label(cells[0].clone());
                    });
                    for (cell, w) in cells[1..].iter().zip(&cols[1..]) {
                        row_cell(ui, *w, |ui| {
                            ui.label(theme::mono(ui, cell.clone()));
                        });
                    }
                },
            );
            if response.clicked() {
                self.select_region(i);
            }
        }
        ui.add_space(theme::SPACE_LG);
        ui.horizontal(|ui| {
            if ui.button("Back").clicked() {
                self.back();
            }
            let can_continue = self.selected_region.is_some();
            if ui
                .add_enabled(can_continue, egui::Button::new("Continue"))
                .clicked()
            {
                self.continue_to_preview();
            }
        });
    }

    fn preview_ui(&mut self, ui: &mut Ui) -> Option<WizardEvent> {
        let mut event = None;
        egui::Grid::new("preview-params")
            .num_columns(2)
            .min_col_width(230.0)
            .spacing(vec2(theme::SPACE_LG, 4.0))
            .show(ui, |ui| {
                ui.label(theme::muted(ui, "hours"));
                theme::mono_drag(
                    ui,
                    egui::DragValue::new(&mut self.hours)
                        .range(0.5..=12.0)
                        .speed(0.5)
                        .suffix(" h"),
                );
                ui.end_row();
                ui.label(theme::muted(ui, "musicians"));
                theme::mono_drag(ui, egui::DragValue::new(&mut self.musicians).range(1..=8));
                ui.end_row();
                ui.label(theme::muted(ui, "listeners"));
                theme::mono_drag(ui, egui::DragValue::new(&mut self.listeners).range(0..=30));
                ui.end_row();
                ui.label(theme::muted(ui, "stream destinations"));
                theme::mono_drag(
                    ui,
                    egui::DragValue::new(&mut self.destinations).range(0..=2),
                );
                ui.end_row();
            });
        ui.add_space(theme::SPACE_MD);
        if let Some(preview) = self.preview() {
            egui::Grid::new("preview-grid")
                .num_columns(2)
                .min_col_width(230.0)
                .spacing(vec2(theme::SPACE_LG, 4.0))
                .show(ui, |ui| {
                    for item in &preview.line_items {
                        ui.label(item.label.clone());
                        let amount = if item.microusd < 0 {
                            format!("-{}", theme::microusd(item.microusd.unsigned_abs()))
                        } else {
                            theme::microusd(item.microusd as u64)
                        };
                        ui.label(theme::mono(ui, amount));
                        ui.end_row();
                    }
                    ui.label(RichText::new("Total (estimate)").strong());
                    ui.label(theme::mono(ui, theme::microusd(preview.total_microusd)));
                    ui.end_row();
                });
            ui.add_space(theme::SPACE_XS);
            ui.label(theme::muted(
                ui,
                "The meter runs until you end the session; this is an estimate, not a cap.",
            ));
        }
        ui.add_space(theme::SPACE_SM);
        let is_mock = self.selected_provider_name() == Some("mock");
        if !is_mock {
            ui.label(theme::muted(
                ui,
                "Launching on a real provider arrives with the networking pass; the mock runs the full flow.",
            ));
        }
        ui.add_space(theme::SPACE_SM);
        ui.horizontal(|ui| {
            if ui.button("Back").clicked() {
                self.back();
            }
            if ui
                .add_enabled(is_mock, egui::Button::new("Launch"))
                .clicked()
                && self.begin_launch()
            {
                event = Some(WizardEvent::LaunchRequested);
            }
        });
        event
    }

    fn launching_ui(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().color(theme::palette_of(ui).text_muted));
            ui.label(theme::muted(
                ui,
                "Booting the VM, verifying the server artifact, opening the session port.",
            ));
        });
    }

    fn done_ui(&mut self, ui: &mut Ui) -> Option<WizardEvent> {
        let mut event = None;
        let outcome = self.outcome.clone()?;
        if let Some(err) = &outcome.error {
            ui.label(RichText::new(err.clone()).color(theme::palette_of(ui).danger));
            ui.add_space(theme::SPACE_LG);
            if ui.button("Back to home").clicked() {
                event = Some(WizardEvent::Close);
            }
            return event;
        }
        ui.horizontal(|ui| {
            ui.label(theme::muted(ui, "server"));
            ui.label(theme::mono(ui, outcome.server_addr.clone()));
        });
        ui.add_space(theme::SPACE_SM);
        ui.label("Invites, one per person; send each to exactly one player.");
        egui::Grid::new("invite-grid")
            .num_columns(3)
            .spacing(vec2(theme::SPACE_LG, 4.0))
            .show(ui, |ui| {
                for (label, invite) in &outcome.invites {
                    ui.label(label.clone());
                    let shown: String = if invite.len() > 40 {
                        format!("{}...", &invite[..40])
                    } else {
                        invite.clone()
                    };
                    ui.label(theme::mono_muted(ui, shown));
                    if ui
                        .add_sized(
                            vec2(160.0, 22.0),
                            egui::Button::new(format!("Copy {label} invite")),
                        )
                        .clicked()
                    {
                        ui.ctx().copy_text(invite.clone());
                    }
                    ui.end_row();
                }
            });
        ui.add_space(theme::SPACE_SM);
        if let Some(path) = &outcome.state_path {
            ui.label(theme::muted(ui, format!("Session recorded at {path}.")));
        }
        ui.label(theme::muted(
            ui,
            "Cost accrues per second from now; end the session to stop the meter.",
        ));
        ui.add_space(theme::SPACE_LG);
        if ui.button("Back to home").clicked() {
            event = Some(WizardEvent::Close);
        }
        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamstream_cloud::{ProviderKind, RegionId};

    fn providers() -> Vec<ProviderRow> {
        vec![
            ProviderRow {
                name: "mock".to_owned(),
                available: true,
                detail: "runs locally, no credentials needed".to_owned(),
            },
            ProviderRow {
                name: "aws".to_owned(),
                available: false,
                detail: "AWS_ACCESS_KEY_ID is not set".to_owned(),
            },
        ]
    }

    fn region_row(id: &str, hourly: u64, rtt: f32) -> RegionRow {
        RegionRow {
            region: Region {
                provider: ProviderKind::Aws,
                id: RegionId::new(id),
                display: id.to_owned(),
                country: "US".to_owned(),
            },
            price: Price {
                hourly_microusd: hourly,
                egress_microusd_per_gb: 90_000,
                included_egress_gb: 0,
            },
            worst_rtt_ms: rtt,
            fabricated: true,
        }
    }

    #[test]
    fn cannot_continue_without_a_provider() {
        let mut w = HostWizard::new(providers());
        assert!(!w.continue_to_region(vec![region_row("mock-east", 16_800, 21.0)]));
        assert_eq!(w.step, WizardStep::Provider);
    }

    #[test]
    fn unavailable_provider_is_refused() {
        let mut w = HostWizard::new(providers());
        assert!(!w.select_provider(1));
        assert!(w.selected_provider.is_none());
        assert!(w.select_provider(0));
        assert_eq!(w.selected_provider_name(), Some("mock"));
    }

    #[test]
    fn happy_path_walks_all_steps() {
        let mut w = HostWizard::new(providers());
        assert!(w.select_provider(0));
        assert!(w.continue_to_region(vec![
            region_row("mock-east", 16_800, 21.0),
            region_row("mock-west", 12_000, 34.0),
        ]));
        assert_eq!(w.step, WizardStep::Region);
        // The top-ranked region is preselected.
        assert_eq!(w.selected_region, Some(0));
        assert!(w.select_region(1));
        assert!(w.continue_to_preview());
        assert_eq!(w.step, WizardStep::Preview);
        let preview = w.preview().expect("preview");
        assert!(preview.total_microusd > 0);
        assert!(w.begin_launch());
        assert_eq!(w.step, WizardStep::Launching);
        assert!(w.finish_launch(LaunchOutcome {
            session_short: "a3f29c41".to_owned(),
            server_addr: "10.0.0.1:43210".to_owned(),
            invites: vec![("host".to_owned(), "jamstream://join/AAAA".to_owned())],
            state_path: None,
            error: None,
        }));
        assert_eq!(w.step, WizardStep::Done);
    }

    #[test]
    fn back_walks_but_never_from_launch() {
        let mut w = HostWizard::new(providers());
        assert!(!w.back());
        w.select_provider(0);
        w.continue_to_region(vec![region_row("mock-east", 16_800, 21.0)]);
        w.continue_to_preview();
        assert!(w.back());
        assert_eq!(w.step, WizardStep::Region);
        assert!(w.back());
        assert_eq!(w.step, WizardStep::Provider);
        // Selections survive going back.
        assert_eq!(w.selected_provider_name(), Some("mock"));
    }

    #[test]
    fn launch_requires_the_mock_provider() {
        let mut w = HostWizard::new(vec![ProviderRow {
            name: "aws".to_owned(),
            available: true,
            detail: "credentials found (AWS_ACCESS_KEY_ID)".to_owned(),
        }]);
        w.select_provider(0);
        w.continue_to_region(vec![region_row("us-east-1", 16_800, 21.0)]);
        w.continue_to_preview();
        assert!(!w.begin_launch());
        assert_eq!(w.step, WizardStep::Preview);
    }

    #[test]
    fn finish_launch_only_applies_while_launching() {
        let mut w = HostWizard::new(providers());
        assert!(!w.finish_launch(LaunchOutcome {
            session_short: String::new(),
            server_addr: String::new(),
            invites: Vec::new(),
            state_path: None,
            error: None,
        }));
        assert_eq!(w.step, WizardStep::Provider);
    }

    #[test]
    fn mock_launch_end_to_end() {
        // Redirect the CLI state dir so the test leaves nothing behind.
        let dir =
            std::env::temp_dir().join(format!("jamstream-client-wizard-{}", std::process::id()));
        // Serialize access to the env var across test threads.
        unsafe { std::env::set_var(jamstream_cli::state::STATE_DIR_ENV, &dir) };

        let mut w = HostWizard::new(providers());
        w.select_provider(0);
        let rows = region_rows("mock").expect("mock regions");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.fabricated));
        w.continue_to_region(rows);
        w.continue_to_preview();
        w.begin_launch();
        let outcome = launch(&w);
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.session_short.len(), 8);
        // host + 3 musicians + 2 listeners.
        assert_eq!(outcome.invites.len(), 6);
        assert_eq!(outcome.invites[0].0, "host");
        // Every invite decodes.
        for (_, encoded) in &outcome.invites {
            jamstream_protocol::invite::Invite::decode(encoded).expect("invite decodes");
        }
        assert!(outcome.state_path.is_some());
        w.finish_launch(outcome);
        assert_eq!(w.step, WizardStep::Done);

        std::fs::remove_dir_all(&dir).ok();
        unsafe { std::env::remove_var(jamstream_cli::state::STATE_DIR_ENV) };
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xfb, 0xff, 0x00]), "+/8A");
    }
}
