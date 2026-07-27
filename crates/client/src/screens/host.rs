//! The host wizard: provider (with credential setup), region, cost
//! preview, launch, then straight into the session. The state machine is
//! plain data with function-per-transition so it tests without a Ui; all
//! network work runs on the app's background executor and the UI polls the
//! results once per frame.
//!
//! Providers are the real ones: local (this computer, free, no account)
//! and the three clouds. The host's own invite is minted but never shown
//! anywhere; the app auto-joins with it and the invites panel on the
//! session screen carries everyone else's.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{RichText, Ui, vec2};
use jamstream_cloud::{
    BootConfig, CostPreview, InstanceClass, LaunchSpec, Price, ProbeMatrix, Provider, ProviderKind,
    Region, RegionId, SelfDestruct, rank, session_tag,
};
use jamstream_protocol::ids::SessionId;
use jamstream_protocol::invite::{Invite, Issuer};
use jamstream_protocol::transport::generate_keypair;
use jamstream_session::client::{ClientCore, ClientState};
// The session shape, defined once for the CLI and this wizard alike; see
// jamstream_session::limits.
use jamstream_session::{
    DEFAULT_HOURS, DEFAULT_IDLE_MIN, DEFAULT_LISTENERS, DEFAULT_MAX_HOURS, DEFAULT_MUSICIANS,
    MAX_LISTENERS, MAX_MUSICIANS,
};

use crate::creds::{self, CredStore, EnvReader};
use crate::exec::{Executor, Job};
use crate::theme;
use crate::widgets::{PICK_INDENT, pick_row, row_cell};

/// Wizard providers in presentation order: local first (no account), then
/// DigitalOcean as the recommended cloud, then the rest. The mock provider
/// is deliberately absent; `--demo` exercises the fake session elsewhere.
pub const WIZARD_PROVIDERS: &[&str] = &["local", "digitalocean", "aws", "gcp"];

const SESSION_PORT: u16 = 43210;
const IP_WAIT_CAP: Duration = Duration::from_secs(180);
const IP_POLL_PERIOD: Duration = Duration::from_secs(2);
const HANDSHAKE_CAP: Duration = Duration::from_secs(60);

/// The local provider consumes the flat config, which carries no artifact
/// fields; these placeholders fill the BootConfig struct for it. Cloud
/// launches need the real artifact url and hash, because the VM downloads
/// and verifies the binary at boot: release builds carry them pinned at
/// compile time, development builds take them from the advanced fields.
const PLACEHOLDER_ARTIFACT_URL: &str = "https://artifacts.invalid/jamstreamd";
const PLACEHOLDER_ARTIFACT_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStatus {
    /// Local: works with zero setup.
    NoAccountNeeded,
    /// Credentials found in the keychain or the environment.
    Ready,
    /// Selecting the row opens the inline setup pane.
    SetupNeeded,
}

impl ProviderStatus {
    pub fn label(self) -> &'static str {
        match self {
            ProviderStatus::NoAccountNeeded => "no account needed",
            ProviderStatus::Ready => "ready",
            ProviderStatus::SetupNeeded => "setup needed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    pub name: String,
    pub status: ProviderStatus,
    /// One factual clause about what picking this means.
    pub hint: String,
}

/// Provider rows from the credential store with the environment fallback.
pub fn provider_rows(creds: &dyn CredStore, env: &EnvReader) -> Vec<ProviderRow> {
    WIZARD_PROVIDERS
        .iter()
        .map(|name| {
            let status = if *name == "local" {
                ProviderStatus::NoAccountNeeded
            } else if creds::build_provider(name, creds, env).is_ok() {
                ProviderStatus::Ready
            } else {
                ProviderStatus::SetupNeeded
            };
            let hint = match *name {
                "local" => "this computer; free, LAN or port-forwarded guests",
                "digitalocean" => "recommended cloud: one token, transfer included",
                "aws" => "more setup; fine if you already use AWS",
                "gcp" => "most setup steps; egress billed on top",
                _ => "",
            };
            ProviderRow {
                name: (*name).to_owned(),
                status,
                hint: hint.to_owned(),
            }
        })
        .collect()
}

/// Typed-but-unsaved credentials for the inline setup panes. Saved to the
/// keychain only after a successful check.
#[derive(Default)]
pub struct SetupFields {
    pub do_token: String,
    pub aws_access_key_id: String,
    pub aws_secret_access_key: String,
    pub gcp_json: String,
    pub gcp_path: String,
    /// Text fields render masked; this is the explicit reveal.
    pub reveal: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegionRow {
    pub region: Region,
    pub price: Price,
    /// Probed from this computer; infinite when the region never answered.
    pub worst_rtt_ms: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchPhase {
    Launching,
    WaitingForAddress,
    CheckingReachability,
}

/// What a successful launch hands the app: the exact state record the CLI
/// writes (shared schema, shared directory) and where it landed. The host
/// invite is `state.invites[0]`; the app joins with it and never shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchOutcome {
    pub state: jamstream_cli::state::SessionState,
    pub state_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    Provider,
    Region,
    Preview,
    Launching,
}

/// What the wizard asks the app to do this frame.
pub enum WizardEvent {
    /// The session is up and recorded; join with the host invite.
    Launched(Box<LaunchOutcome>),
}

pub struct HostWizard {
    pub step: WizardStep,
    pub providers: Vec<ProviderRow>,
    pub selected_provider: Option<usize>,
    pub setup: SetupFields,
    pub setup_open: bool,
    pub check_result: Option<Result<(), String>>,
    pub regions: Vec<RegionRow>,
    pub regions_error: Option<String>,
    pub selected_region: Option<usize>,
    pub hours: f32,
    /// Musician seats including the host's own, exactly as `--musicians`
    /// means it: 1 is a solo session, [`MAX_MUSICIANS`] is the cap the
    /// server enforces.
    pub musicians: u8,
    pub listeners: u8,
    pub destinations: u8,
    /// Minutes with no musicians connected before the server exits, and the
    /// hard cap on session length. Both are shown and editable on the
    /// preview step rather than baked in, because they are the guardrails
    /// that decide what a forgotten session costs.
    pub idle_min: u32,
    pub max_hours: u32,
    /// The server artifact pinned into this build, read once at
    /// construction. When present (every release build) the wizard shows
    /// no artifact fields at all: cloud launches silently use the pinned
    /// pair, and the preview step carries one quiet line saying the server
    /// download is verified. When absent (development builds) the advanced
    /// fields below are shown and required for cloud launches. Public so
    /// tests can pin or unpin regardless of how the test binary was built.
    pub pinned: Option<jamstream_cloud::PinnedServerArtifact>,
    pub advanced_open: bool,
    pub artifact_url: String,
    pub artifact_sha256: String,
    pub launch_error: Option<String>,
    check_job: Option<Job<Result<(), String>>>,
    regions_job: Option<Job<Result<Vec<RegionRow>, String>>>,
    launch_job: Option<Job<Result<LaunchOutcome, String>>>,
    launch_phase: Arc<Mutex<LaunchPhase>>,
    creds: Arc<dyn CredStore>,
    env: EnvReader,
    exec: Arc<Executor>,
}

// Pure state machine. Each transition validates its precondition and
// returns whether it happened, so tests can assert both directions; the
// job-spawning methods delegate to these same transitions when results
// arrive.
impl HostWizard {
    pub fn new(creds: Arc<dyn CredStore>, env: EnvReader, exec: Arc<Executor>) -> Self {
        let providers = provider_rows(creds.as_ref(), &env);
        HostWizard {
            step: WizardStep::Provider,
            providers,
            selected_provider: None,
            setup: SetupFields::default(),
            setup_open: false,
            check_result: None,
            regions: Vec::new(),
            regions_error: None,
            selected_region: None,
            hours: DEFAULT_HOURS,
            musicians: DEFAULT_MUSICIANS,
            listeners: DEFAULT_LISTENERS,
            destinations: 0,
            idle_min: DEFAULT_IDLE_MIN,
            max_hours: DEFAULT_MAX_HOURS,
            pinned: jamstream_cloud::pinned(),
            advanced_open: false,
            artifact_url: String::new(),
            artifact_sha256: String::new(),
            launch_error: None,
            check_job: None,
            regions_job: None,
            launch_job: None,
            launch_phase: Arc::new(Mutex::new(LaunchPhase::Launching)),
            creds,
            env,
            exec,
        }
    }

    pub fn selected_provider_name(&self) -> Option<&str> {
        self.selected_provider
            .and_then(|i| self.providers.get(i))
            .map(|p| p.name.as_str())
    }

    fn selected_row(&self) -> Option<&ProviderRow> {
        self.selected_provider.and_then(|i| self.providers.get(i))
    }

    pub fn is_local(&self) -> bool {
        self.selected_provider_name() == Some("local")
    }

    /// True while any background job is in flight; the app keeps repainting.
    pub fn busy(&self) -> bool {
        self.check_job.is_some() || self.regions_job.is_some() || self.launch_job.is_some()
    }

    pub fn launch_phase(&self) -> LaunchPhase {
        *self.launch_phase.lock().expect("launch phase")
    }

    /// Any provider is selectable; one that still needs credentials opens
    /// its setup pane instead of unlocking Continue.
    pub fn select_provider(&mut self, idx: usize) -> bool {
        let Some(row) = self.providers.get(idx) else {
            return false;
        };
        self.setup_open = row.status == ProviderStatus::SetupNeeded;
        if self.selected_provider != Some(idx) {
            self.check_result = None;
        }
        self.selected_provider = Some(idx);
        true
    }

    pub fn provider_ready(&self) -> bool {
        self.selected_row()
            .is_some_and(|row| row.status != ProviderStatus::SetupNeeded)
    }

    /// Recomputes one row's status from the store and environment.
    pub fn refresh_provider_status(&mut self, idx: usize) {
        let Some(row) = self.providers.get_mut(idx) else {
            return;
        };
        if row.name != "local" {
            row.status = if creds::build_provider(&row.name, self.creds.as_ref(), &self.env).is_ok()
            {
                ProviderStatus::Ready
            } else {
                ProviderStatus::SetupNeeded
            };
        }
    }

    /// A provider built from the setup pane's unsaved fields, for the
    /// credential check.
    pub fn provider_from_setup(&self) -> Result<Box<dyn Provider>, String> {
        match self.selected_provider_name() {
            Some("digitalocean") => {
                let token = self.setup.do_token.trim();
                if token.is_empty() {
                    return Err("paste the token first".to_owned());
                }
                Ok(Box::new(
                    jamstream_cloud::providers::digitalocean::DigitalOceanProvider::new(
                        token.to_owned(),
                    ),
                ))
            }
            Some("aws") => {
                let id = self.setup.aws_access_key_id.trim();
                let secret = self.setup.aws_secret_access_key.trim();
                if id.is_empty() || secret.is_empty() {
                    return Err("paste both values first".to_owned());
                }
                Ok(Box::new(jamstream_cloud::providers::aws::AwsProvider::new(
                    id.to_owned(),
                    secret.to_owned(),
                )))
            }
            Some("gcp") => {
                let json = self.setup.gcp_json.trim();
                if json.is_empty() {
                    return Err("paste the service account JSON first".to_owned());
                }
                creds::gcp_from_json(json, &self.env)
            }
            other => Err(format!("no setup pane for {other:?}")),
        }
    }

    /// Runs the credential check on the executor. Field errors surface
    /// immediately without spawning anything.
    pub fn begin_check(&mut self) -> bool {
        if self.check_job.is_some() {
            return false;
        }
        match self.provider_from_setup() {
            Ok(provider) => {
                self.check_result = None;
                self.check_job = Some(self.exec.run(check_provider(provider)));
                true
            }
            Err(err) => {
                self.check_result = Some(Err(err));
                false
            }
        }
    }

    /// A successful check saves the typed fields to the credential store
    /// and flips the row to ready; a failure is shown verbatim.
    pub fn apply_check_result(&mut self, result: Result<(), String>) {
        if result.is_ok()
            && let Some(idx) = self.selected_provider
        {
            let saves: &[((&str, &str), &str)] = match self.selected_provider_name() {
                Some("digitalocean") => &[(creds::DO_TOKEN, &self.setup.do_token)],
                Some("aws") => &[
                    (creds::AWS_ACCESS_KEY_ID, &self.setup.aws_access_key_id),
                    (
                        creds::AWS_SECRET_ACCESS_KEY,
                        &self.setup.aws_secret_access_key,
                    ),
                ],
                Some("gcp") => &[(creds::GCP_SERVICE_ACCOUNT_JSON, &self.setup.gcp_json)],
                _ => &[],
            };
            let mut save_err = None;
            for ((provider, field), value) in saves {
                if let Err(err) = self.creds.set(provider, field, value.trim()) {
                    save_err = Some(err);
                }
            }
            self.refresh_provider_status(idx);
            if let Some(err) = save_err {
                // The credentials work but did not persist; say so rather
                // than pretending they were saved.
                self.check_result = Some(Err(format!(
                    "the credentials work but saving them failed: {err}"
                )));
                return;
            }
        }
        self.check_result = Some(result);
    }

    /// Provider -> next step. Local has exactly one region (this computer,
    /// zero price) so it skips straight to the preview; clouds start the
    /// real probe job and land on the region step's progress state.
    pub fn advance_from_provider(&mut self) -> bool {
        if self.step != WizardStep::Provider || !self.provider_ready() {
            return false;
        }
        if self.is_local() {
            self.regions = vec![local_region_row()];
            self.selected_region = Some(0);
            self.step = WizardStep::Preview;
            return true;
        }
        let Some(name) = self.selected_provider_name().map(str::to_owned) else {
            return false;
        };
        match creds::build_provider(&name, self.creds.as_ref(), &self.env) {
            Ok(provider) => {
                self.regions = Vec::new();
                self.selected_region = None;
                self.regions_error = None;
                self.step = WizardStep::Region;
                self.regions_job = Some(self.exec.run(probe_regions(provider)));
                true
            }
            Err(err) => {
                // The row claimed ready but the build failed (for example a
                // keychain entry vanished); re-gate it.
                self.check_result = Some(Err(err));
                if let Some(idx) = self.selected_provider {
                    self.refresh_provider_status(idx);
                }
                self.setup_open = true;
                false
            }
        }
    }

    /// Provider -> Region with rows supplied directly; the pure transition
    /// tests and snapshot fixtures feed this, bypassing the probe job.
    pub fn continue_to_region(&mut self, rows: Vec<RegionRow>) -> bool {
        if self.step == WizardStep::Provider && self.selected_provider.is_some() {
            self.step = WizardStep::Region;
            self.set_regions(rows);
            true
        } else {
            false
        }
    }

    fn set_regions(&mut self, rows: Vec<RegionRow>) {
        self.selected_region = if rows.is_empty() { None } else { Some(0) };
        self.regions = rows;
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

    /// Launch preconditions: a region, and for clouds a verified artifact
    /// (the VM downloads and checks the server binary; local runs one that
    /// is already on this machine). Release builds carry the artifact
    /// pinned, so there is nothing to validate; only development builds
    /// gate on the advanced fields.
    pub fn can_launch(&self) -> bool {
        self.step == WizardStep::Preview
            && self.selected_region.is_some()
            && (self.is_local()
                || self.pinned.is_some()
                || (!self.artifact_url.trim().is_empty()
                    && !self.artifact_sha256.trim().is_empty()))
    }

    /// Preview -> Launching: starts the real launch on the executor.
    pub fn begin_launch(&mut self) -> bool {
        if !self.can_launch() || self.launch_job.is_some() {
            return false;
        }
        let Some(name) = self.selected_provider_name().map(str::to_owned) else {
            return false;
        };
        let Some(row) = self.selected_region.and_then(|i| self.regions.get(i)) else {
            return false;
        };
        let provider = match creds::build_provider(&name, self.creds.as_ref(), &self.env) {
            Ok(p) => p,
            Err(err) => {
                self.launch_error = Some(err);
                self.step = WizardStep::Launching;
                return true;
            }
        };
        let params = LaunchParams {
            provider_name: name,
            region: row.region.clone(),
            hourly_microusd: row.price.hourly_microusd,
            musicians: self.musicians,
            listeners: self.listeners,
            idle_min: self.idle_min,
            max_hours: self.max_hours,
            // Explicit fields (development builds) outrank the pin, same
            // precedence as the CLI's override flags.
            artifact_url: non_empty(&self.artifact_url)
                .or_else(|| self.pinned.map(|p| p.url.to_owned())),
            artifact_sha256: non_empty(&self.artifact_sha256)
                .or_else(|| self.pinned.map(|p| p.sha256.to_owned())),
            do_token: creds::lookup(
                self.creds.as_ref(),
                &self.env,
                creds::DO_TOKEN,
                "DIGITALOCEAN_TOKEN",
            ),
        };
        *self.launch_phase.lock().expect("launch phase") = LaunchPhase::Launching;
        self.launch_error = None;
        self.step = WizardStep::Launching;
        let phase = Arc::clone(&self.launch_phase);
        self.launch_job = Some(self.exec.run(launch_session(provider, params, phase)));
        true
    }

    /// One step back; Launching only goes back after a failure.
    pub fn back(&mut self) -> bool {
        match self.step {
            // While the probe job runs there is nothing to go back from;
            // once it lands (rows or an error) back is available again.
            WizardStep::Region if self.regions_job.is_none() => {
                self.regions_error = None;
                self.step = WizardStep::Provider;
                true
            }
            WizardStep::Preview => {
                if self.is_local() {
                    self.step = WizardStep::Provider;
                } else {
                    self.step = WizardStep::Region;
                }
                true
            }
            WizardStep::Launching if self.launch_error.is_some() => {
                self.launch_error = None;
                self.step = WizardStep::Preview;
                true
            }
            _ => false,
        }
    }

    /// Applies finished background jobs. Returns the launch event when the
    /// session comes up.
    pub fn poll(&mut self) -> Option<WizardEvent> {
        if let Some(job) = &mut self.check_job
            && let Some(result) = job.poll()
        {
            self.check_job = None;
            self.apply_check_result(result);
        }
        if let Some(job) = &mut self.regions_job
            && let Some(result) = job.poll()
        {
            self.regions_job = None;
            match result {
                Ok(rows) => self.set_regions(rows),
                Err(err) => self.regions_error = Some(err),
            }
        }
        if let Some(job) = &mut self.launch_job
            && let Some(result) = job.poll()
        {
            self.launch_job = None;
            match result {
                Ok(outcome) => return Some(WizardEvent::Launched(Box::new(outcome))),
                Err(err) => self.launch_error = Some(err),
            }
        }
        None
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// The local provider's single region, priced at zero, without a network
/// round trip.
fn local_region_row() -> RegionRow {
    RegionRow {
        region: Region {
            provider: ProviderKind::Local,
            id: RegionId::new("local"),
            display: "This computer".to_owned(),
            country: String::new(),
        },
        price: Price {
            hourly_microusd: 0,
            egress_microusd_per_gb: 0,
            included_egress_gb: 0,
        },
        worst_rtt_ms: 0.0,
    }
}

/// The credential check: the static catalog, one live price call, and one
/// authenticated list of jamstream-tagged instances (the same call the
/// docs' `jamstream sweep --dry-run` verification makes; price alone would
/// not exercise authentication on providers with bundled price data).
/// Errors are returned verbatim for the pane to show.
pub async fn check_provider(provider: Box<dyn Provider>) -> Result<(), String> {
    let regions = provider.regions();
    let first = regions
        .first()
        .ok_or("provider offers no regions")?
        .id
        .clone();
    provider.price(&first).await.map_err(|e| e.to_string())?;
    provider
        .list_tagged(None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Real probes: live price per region plus TCP connect timing from this
/// machine against the provider's catalog endpoints, ranked by the shared
/// solver (worst RTT in 5 ms buckets, price breaking ties).
async fn probe_regions(provider: Box<dyn Provider>) -> Result<Vec<RegionRow>, String> {
    let regions = provider.regions();
    if regions.is_empty() {
        return Err("provider offers no regions".to_owned());
    }
    let mut candidates: Vec<(Region, Price)> = Vec::with_capacity(regions.len());
    for region in &regions {
        let price = provider
            .price(&region.id)
            .await
            .map_err(|e| e.to_string())?;
        candidates.push((region.clone(), price));
    }
    let targets = jamstream_cli::host::catalog_targets(provider.kind(), &regions);
    let rtts = jamstream_cloud::probe_all(&targets).await;
    let mut matrix = ProbeMatrix::new();
    for (region, rtt_ms) in rtts {
        matrix.insert(0, region, rtt_ms);
    }
    Ok(rank(&matrix, &candidates)
        .into_iter()
        .map(|score| RegionRow {
            region: score.region,
            price: score.price,
            worst_rtt_ms: score.worst_rtt_ms,
        })
        .collect())
}

struct LaunchParams {
    provider_name: String,
    region: Region,
    hourly_microusd: u64,
    /// Musician seats including the host's own; see [`HostWizard::musicians`].
    musicians: u8,
    listeners: u8,
    idle_min: u32,
    max_hours: u32,
    artifact_url: Option<String>,
    artifact_sha256: Option<String>,
    /// For the DigitalOcean self-destruct arm; read from the credential
    /// store or environment before the job leaves the UI thread.
    do_token: Option<String>,
}

/// The launch itself, mirroring `jamstream host`: boot config, flat config
/// for local versus cloud-init for clouds, launch, wait for the address,
/// mint invites, prove the server answers a real handshake, and write the
/// same state file the CLI writes.
async fn launch_session(
    provider: Box<dyn Provider>,
    params: LaunchParams,
    phase: Arc<Mutex<LaunchPhase>>,
) -> Result<LaunchOutcome, String> {
    let set_phase = |p: LaunchPhase| *phase.lock().expect("launch phase") = p;
    let is_local = provider.kind() == ProviderKind::Local;

    if !is_local && (params.artifact_url.is_none() || params.artifact_sha256.is_none()) {
        return Err(
            "a cloud launch needs the server binary url and sha256; open the advanced \
             section of the preview step"
                .to_owned(),
        );
    }

    let session_id = SessionId::generate();
    let session_hex = hex_encode(&session_id.0);
    let issuer = Issuer::generate();
    let server_keys = generate_keypair();
    let now_unix = unix_now();
    // Invites expire with the session's hard cap, as the CLI mints them.
    let expires_unix = now_unix + u64::from(params.max_hours) * 3600;
    // Local servers share one machine with everything else on it; an
    // ephemeral port avoids colliding with another session or service.
    let port = if is_local {
        free_udp_port()?
    } else {
        SESSION_PORT
    };

    let boot = BootConfig {
        artifact_url: params
            .artifact_url
            .clone()
            .unwrap_or_else(|| PLACEHOLDER_ARTIFACT_URL.to_owned()),
        artifact_sha256: params
            .artifact_sha256
            .clone()
            .unwrap_or_else(|| PLACEHOLDER_ARTIFACT_SHA256.to_owned()),
        server_private_key_b64: base64(&server_keys.private),
        issuer_public_key_b64: base64(issuer.public_key().as_bytes()),
        session_id_hex: session_hex.clone(),
        port,
        idle_shutdown_min: params.idle_min,
        max_duration_min: params.max_hours * 60,
        self_destruct: self_destruct_for(provider.kind(), params.do_token)?,
    };
    let user_data = if is_local {
        boot.render_flat_config()
    } else {
        jamstream_cloud::cloudinit::render(&boot)
    };
    let spec = LaunchSpec {
        region: params.region.clone(),
        instance_class: InstanceClass::Standard,
        user_data,
        tags: vec![session_tag(&session_hex)],
    };

    set_phase(LaunchPhase::Launching);
    let instance = provider.launch(spec).await.map_err(|e| e.to_string())?;
    set_phase(LaunchPhase::WaitingForAddress);
    let instance = wait_for_ip(provider.as_ref(), &session_hex, instance).await?;
    let ip = instance.public_ip.ok_or("instance reported no public ip")?;
    let address = SocketAddr::new(ip, port);

    // The CLI's invite book, minted by the CLI's own function: same order,
    // same labels, same "musicians counts the host" seat math.
    let invites = jamstream_cli::host::mint_invites(
        &issuer,
        session_id,
        address,
        server_keys.public,
        expires_unix,
        params.musicians,
        params.listeners,
    );

    set_phase(LaunchPhase::CheckingReachability);
    verify_reachable(&invites[0].1, HANDSHAKE_CAP).await?;

    let state = jamstream_cli::state::SessionState {
        session_id_hex: session_hex,
        provider: params.provider_name,
        region: params.region.id.to_string(),
        instance_id: instance.id,
        address: address.to_string(),
        created_unix: now_unix,
        hourly_microusd: params.hourly_microusd,
        issuer_private_key_b64: base64(&issuer.to_bytes()),
        server_public_key_b64: base64(&server_keys.public),
        invites: invites
            .iter()
            .map(|(label, invite)| jamstream_cli::state::InviteRecord {
                role: label.clone(),
                invite: invite.encode(),
            })
            .collect(),
        status: jamstream_cli::state::SessionStatus::Running,
        ended_unix: None,
    };
    let state_path = jamstream_cli::state::save(&state).map_err(|e| e.to_string())?;
    Ok(LaunchOutcome { state, state_path })
}

/// Per-provider self-destruct, as the CLI arms it. DigitalOcean is the one
/// that needs a credential on the box, because powered-off droplets still
/// bill; the token comes from the credential store or environment.
fn self_destruct_for(kind: ProviderKind, do_token: Option<String>) -> Result<SelfDestruct, String> {
    match kind {
        ProviderKind::Aws => Ok(SelfDestruct::AwsShutdown),
        ProviderKind::Gcp => Ok(SelfDestruct::GcpMaxRunDuration),
        ProviderKind::DigitalOcean => {
            let token = do_token.filter(|t| !t.is_empty()).ok_or(
                "a DigitalOcean token is required to arm the droplet's self-destruct; \
                 refusing to launch a machine that cannot delete itself",
            )?;
            Ok(SelfDestruct::ApiToken {
                endpoint: "https://api.digitalocean.com/v2/droplets".to_owned(),
                token,
            })
        }
        // Local sessions never render cloud-init: the flat config carries
        // no self-destruct and the spawned server self-limits through its
        // own --idle-exit-min. This variant is inert here.
        ProviderKind::Local => Ok(SelfDestruct::AwsShutdown),
    }
}

/// Polls list_tagged until the instance reports a public IP; the local
/// provider returns one at launch, so this is a single pass there.
async fn wait_for_ip(
    provider: &dyn Provider,
    session_hex: &str,
    mut instance: jamstream_cloud::Instance,
) -> Result<jamstream_cloud::Instance, String> {
    let deadline = Instant::now() + IP_WAIT_CAP;
    while instance.public_ip.is_none() {
        if Instant::now() >= deadline {
            return Err(format!(
                "instance {} did not report an ip within {} s",
                instance.id,
                IP_WAIT_CAP.as_secs()
            ));
        }
        tokio::time::sleep(IP_POLL_PERIOD).await;
        let listed = provider
            .list_tagged(Some(session_hex))
            .await
            .map_err(|e| e.to_string())?;
        if let Some(found) = listed.into_iter().find(|i| i.id == instance.id) {
            instance = found;
        }
    }
    Ok(instance)
}

/// Proves the server is actually serving: a genuine ClientCore handshake
/// with the host invite over UDP, driven until Joined, exactly like the
/// CLI's reachability check.
async fn verify_reachable(invite: &Invite, cap: Duration) -> Result<(), String> {
    let bind: SocketAddr = if invite.addresses[0].is_ipv4() {
        "0.0.0.0:0".parse().expect("static addr")
    } else {
        "[::]:0".parse().expect("static addr")
    };
    let socket = tokio::net::UdpSocket::bind(bind)
        .await
        .map_err(|e| e.to_string())?;
    socket
        .connect(invite.addresses[0])
        .await
        .map_err(|e| e.to_string())?;

    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let (mut core, init) = ClientCore::connect(invite, now()).map_err(|e| e.to_string())?;
    socket.send(&init).await.map_err(|e| e.to_string())?;
    let mut buf = [0u8; 2048];
    while start.elapsed() < cap {
        for pkt in core.poll(now()) {
            socket.send(&pkt).await.map_err(|e| e.to_string())?;
        }
        if let Ok(Ok(len)) =
            tokio::time::timeout(Duration::from_millis(50), socket.recv(&mut buf)).await
        {
            for pkt in core.handle_datagram(now(), &buf[..len]) {
                socket.send(&pkt).await.map_err(|e| e.to_string())?;
            }
        }
        match core.state().clone() {
            ClientState::Joined => {
                let _ = core.leave("host reachability check");
                for pkt in core.poll(now()) {
                    socket.send(&pkt).await.map_err(|e| e.to_string())?;
                }
                return Ok(());
            }
            ClientState::Rejected { ours, theirs } => {
                return Err(format!(
                    "server rejected the handshake: this client speaks protocol {ours}, \
                     the server speaks {theirs}"
                ));
            }
            ClientState::Ejected { reason } => {
                return Err(format!("server ejected the reachability check: {reason}"));
            }
            // The core gives up after its own 10 s window; keep trying
            // fresh handshakes until our cap, the VM may still be booting.
            ClientState::TimedOut => {
                let init = core.reconnect(now()).map_err(|e| e.to_string())?;
                socket.send(&init).await.map_err(|e| e.to_string())?;
            }
            ClientState::Connecting => {}
        }
    }
    Err(format!(
        "server did not complete a handshake within {} s",
        cap.as_secs()
    ))
}

/// Bind-then-drop; racy in principle, unique enough in practice.
fn free_udp_port() -> Result<u16, String> {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("cannot pick a local port: {e}"))
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Standard base64 with padding; enough to fill the CLI state schema
/// without pulling an encoding crate into the client.
pub(crate) fn base64(bytes: &[u8]) -> String {
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

/// Inverse of [`base64`]; the invites panel reads the issuer key back out
/// of the state file with it.
pub(crate) fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    fn value(c: u8) -> Result<u32, String> {
        match c {
            b'A'..=b'Z' => Ok(u32::from(c - b'A')),
            b'a'..=b'z' => Ok(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Ok(u32::from(c - b'0') + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("invalid base64 byte 0x{c:02x}")),
        }
    }
    let stripped: Vec<u8> = text
        .bytes()
        .filter(|b| !matches!(b, b'=' | b'\n' | b'\r' | b' '))
        .collect();
    let mut out = Vec::with_capacity(stripped.len() * 3 / 4);
    for chunk in stripped.chunks(4) {
        if chunk.len() == 1 {
            return Err("truncated base64".to_owned());
        }
        let mut n = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            n |= value(b)? << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..chunk.len() - 1]);
    }
    Ok(out)
}

// Rendering. One focused card per step: the step counter and title live
// inside the card, back and continue at the bottom. Numbers are monospace
// throughout.

impl HostWizard {
    pub fn ui(&mut self, ui: &mut Ui) -> Option<WizardEvent> {
        let event = self.poll();
        // The card is taller than a short window: at 800x600, the app's
        // smallest, the setup pane and the cost preview both run past the
        // bottom edge, and Back and Continue live down there.
        egui::ScrollArea::vertical()
            .id_salt("wizard-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.card_ui(ui);
            });
        event
    }

    fn card_ui(&mut self, ui: &mut Ui) {
        theme::focused_column(ui, 620.0, |ui| {
            theme::panel(ui)
                .inner_margin(egui::Margin::same(16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let title = match self.step {
                        WizardStep::Provider => "Where should the session server run?".to_owned(),
                        WizardStep::Region => "Pick a region".to_owned(),
                        WizardStep::Preview if self.is_local() => "Before you start".to_owned(),
                        WizardStep::Preview => "Cost preview".to_owned(),
                        WizardStep::Launching if self.is_local() => {
                            "Starting on this computer".to_owned()
                        }
                        WizardStep::Launching => format!(
                            "Launching in {}",
                            self.selected_region
                                .and_then(|i| self.regions.get(i))
                                .map(|r| r.region.id.to_string())
                                .unwrap_or_default()
                        ),
                    };
                    let num = match self.step {
                        WizardStep::Provider => 1,
                        WizardStep::Region => 2,
                        WizardStep::Preview => 3,
                        WizardStep::Launching => 4,
                    };
                    ui.label(theme::muted(ui, format!("Step {num} of 4")).small());
                    ui.add_space(theme::SPACE_XS);
                    let title_font = egui::FontId::new(16.0, theme::semibold(ui));
                    ui.label(RichText::new(title).font(title_font));
                    ui.add_space(theme::SPACE_LG);
                    match self.step {
                        WizardStep::Provider => self.provider_ui(ui),
                        WizardStep::Region => self.region_ui(ui),
                        WizardStep::Preview => self.preview_ui(ui),
                        WizardStep::Launching => self.launching_ui(ui),
                    }
                });
        });
    }

    fn provider_ui(&mut self, ui: &mut Ui) {
        for i in 0..self.providers.len() {
            let row = self.providers[i].clone();
            let response = pick_row(
                ui,
                &row.name,
                self.selected_provider == Some(i),
                true,
                |ui| {
                    row_cell(ui, 110.0, |ui| {
                        ui.label(row.name.clone());
                    });
                    row_cell(ui, 130.0, |ui| {
                        ui.label(theme::muted(ui, row.status.label()));
                    });
                    ui.add(egui::Label::new(theme::muted(ui, row.hint.clone()).small()).truncate());
                },
            );
            if response.clicked() {
                self.select_provider(i);
            }
        }
        if self.setup_open
            && self
                .selected_row()
                .is_some_and(|r| r.status == ProviderStatus::SetupNeeded)
        {
            ui.add_space(theme::SPACE_MD);
            self.setup_ui(ui);
        }
        ui.add_space(theme::SPACE_LG);
        ui.horizontal(|ui| {
            let can_continue = self.provider_ready();
            if ui
                .add_enabled(can_continue, egui::Button::new("Continue"))
                .clicked()
            {
                self.advance_from_provider();
            }
            if !can_continue && self.selected_provider.is_some() {
                ui.label(theme::muted(
                    ui,
                    "Add credentials above, or pick local to host without an account.",
                ));
            }
        });
    }

    /// Inline credential setup for the selected cloud. Guidance matches
    /// the docs site's provider pages; the check runs a real API call and
    /// only a passing check writes the keychain.
    fn setup_ui(&mut self, ui: &mut Ui) {
        let Some(name) = self.selected_provider_name().map(str::to_owned) else {
            return;
        };
        theme::panel(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            match name.as_str() {
                "digitalocean" => {
                    ui.label(theme::title(ui, "Connect DigitalOcean"));
                    ui.add_space(theme::SPACE_XS);
                    guidance(
                        ui,
                        &[
                            "1. Sign up at digitalocean.com and add a payment method.",
                            "2. Generate a personal access token named jamstream.",
                            "3. Scope it to droplet and tag operations only; the docs \
                             list the exact scopes.",
                        ],
                    );
                    if ui.button("Open the token page").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(
                            "https://cloud.digitalocean.com/account/api/tokens",
                        ));
                    }
                    ui.add_space(theme::SPACE_SM);
                    let reveal = self.setup.reveal;
                    secret_field(ui, "API token", &mut self.setup.do_token, reveal);
                }
                "aws" => {
                    ui.label(theme::title(ui, "Connect AWS"));
                    ui.add_space(theme::SPACE_XS);
                    guidance(
                        ui,
                        &[
                            "1. Create an IAM user named jamstream with the minimal \
                             policy from the docs; no console access.",
                            "2. Create an access key for it (use case: CLI).",
                            "3. Paste both values; the secret is shown once by AWS.",
                        ],
                    );
                    if ui.button("Open the IAM console").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(
                            "https://console.aws.amazon.com/iam/home#/users",
                        ));
                    }
                    ui.add_space(theme::SPACE_SM);
                    let reveal = self.setup.reveal;
                    secret_field(
                        ui,
                        "Access key id",
                        &mut self.setup.aws_access_key_id,
                        reveal,
                    );
                    secret_field(
                        ui,
                        "Secret access key",
                        &mut self.setup.aws_secret_access_key,
                        reveal,
                    );
                }
                "gcp" => {
                    ui.label(theme::title(ui, "Connect Google Cloud"));
                    ui.add_space(theme::SPACE_XS);
                    guidance(
                        ui,
                        &[
                            "1. Create a project, enable the Compute Engine API.",
                            "2. Create a service account with the Compute Instance \
                             Admin (v1) role and add a JSON key to it.",
                            "3. Paste the key file's contents below, or enter its \
                             path and load it.",
                        ],
                    );
                    if ui.button("Open the service accounts page").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(
                            "https://console.cloud.google.com/iam-admin/serviceaccounts",
                        ));
                    }
                    ui.add_space(theme::SPACE_SM);
                    // There is no file-picker dependency in this build, so
                    // the key file arrives pasted or by path; both feed the
                    // same field.
                    ui.horizontal(|ui| {
                        ui.label(theme::muted(ui, "key file path"));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.setup.gcp_path)
                                .desired_width(300.0)
                                .hint_text("/path/to/jamstream-key.json"),
                        );
                        if ui.button("Load file").clicked() {
                            match std::fs::read_to_string(self.setup.gcp_path.trim()) {
                                Ok(json) => {
                                    self.setup.gcp_json = json;
                                    self.check_result = None;
                                }
                                Err(err) => {
                                    self.check_result =
                                        Some(Err(format!("cannot read the key file: {err}")));
                                }
                            }
                        }
                    });
                    ui.label(theme::muted(ui, "service account JSON"));
                    let mask = !self.setup.reveal;
                    ui.add(
                        egui::TextEdit::multiline(&mut self.setup.gcp_json)
                            .desired_width(f32::INFINITY)
                            .desired_rows(4)
                            .password(mask)
                            .hint_text("paste the downloaded key file's contents"),
                    );
                }
                _ => {}
            }
            ui.add_space(theme::SPACE_SM);
            ui.horizontal(|ui| {
                let checking = self.check_job.is_some();
                if ui
                    .add_enabled(!checking, egui::Button::new("Check credentials"))
                    .clicked()
                {
                    self.begin_check();
                }
                if ui
                    .button(if self.setup.reveal { "Hide" } else { "Show" })
                    .clicked()
                {
                    self.setup.reveal = !self.setup.reveal;
                }
                if checking {
                    ui.add(egui::Spinner::new().color(theme::palette_of(ui).text_muted));
                    ui.label(theme::muted(ui, "asking the provider"));
                }
            });
            match &self.check_result {
                Some(Ok(())) => {
                    let p = theme::palette_of(ui);
                    ui.label(RichText::new("Works. Saved to your keychain.").color(p.meter_green));
                }
                Some(Err(err)) => {
                    let p = theme::palette_of(ui);
                    ui.label(RichText::new(err.clone()).color(p.danger));
                }
                None => {}
            }
        });
    }

    fn region_ui(&mut self, ui: &mut Ui) {
        if self.regions_job.is_some() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().color(theme::palette_of(ui).text_muted));
                ui.label(theme::muted(
                    ui,
                    "Fetching prices and timing the network from this computer to each region.",
                ));
            });
            return;
        }
        if let Some(err) = self.regions_error.clone() {
            let p = theme::palette_of(ui);
            ui.label(RichText::new(err).color(p.danger));
            ui.add_space(theme::SPACE_LG);
            if ui.button("Back").clicked() {
                self.back();
            }
            return;
        }
        // The solver sorts by coverage, then worst round trip bucketed to
        // 5 ms, then hourly price, so price only decides inside a bucket.
        ui.label(theme::muted(
            ui,
            "Sorted by worst round trip in 5 ms steps, with price breaking ties.",
        ));
        ui.label(theme::muted(
            ui,
            "Latency is measured from this computer; bandmates elsewhere will differ.",
        ));
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

    fn preview_ui(&mut self, ui: &mut Ui) {
        egui::Grid::new("preview-params")
            .num_columns(2)
            .min_col_width(230.0)
            .spacing(vec2(theme::SPACE_LG, 4.0))
            .show(ui, |ui| {
                // Expected length shapes the estimate only, so it cannot
                // usefully exceed the hard cap below.
                let cap = self.max_hours as f32;
                ui.label(theme::muted(ui, "hours"));
                theme::mono_drag(
                    ui,
                    egui::DragValue::new(&mut self.hours)
                        .range(0.5..=cap)
                        .speed(0.5)
                        .suffix(" h"),
                );
                ui.end_row();
                // Seats, not guests: the count includes you, and the ceiling
                // is the capacity the server enforces.
                ui.label(theme::muted(ui, "musicians, including you"));
                theme::mono_drag(
                    ui,
                    egui::DragValue::new(&mut self.musicians).range(1..=MAX_MUSICIANS),
                );
                ui.end_row();
                ui.label(theme::muted(ui, "listeners"));
                theme::mono_drag(
                    ui,
                    egui::DragValue::new(&mut self.listeners).range(0..=MAX_LISTENERS),
                );
                ui.end_row();
                ui.label(theme::muted(ui, "stream destinations"));
                theme::mono_drag(
                    ui,
                    egui::DragValue::new(&mut self.destinations).range(0..=2),
                );
                ui.end_row();
                // The two self-limits the machine enforces on itself. Same
                // meaning and same defaults as --idle-min and --max-hours.
                ui.label(theme::muted(ui, "idle exit"));
                theme::mono_drag(
                    ui,
                    egui::DragValue::new(&mut self.idle_min)
                        .range(1..=120)
                        .suffix(" min"),
                );
                ui.end_row();
                ui.label(theme::muted(ui, "hard cap"));
                theme::mono_drag(
                    ui,
                    egui::DragValue::new(&mut self.max_hours)
                        .range(1..=24)
                        .suffix(" h"),
                );
                ui.end_row();
            });
        ui.add_space(theme::SPACE_XS);
        ui.label(theme::muted(
            ui,
            format!(
                "The server exits after {} minutes with nobody playing, and ends the session \
                 at {} hours no matter what; invites expire then too.",
                self.idle_min, self.max_hours
            ),
        ));
        ui.add_space(theme::SPACE_MD);
        if self.is_local() {
            ui.label("This session runs on this computer and costs nothing.");
            ui.label(theme::muted(
                ui,
                "Invites carry your LAN address; guests outside it need router port forwarding.",
            ));
        } else if let Some(preview) = self.preview() {
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
        if !self.is_local() {
            ui.add_space(theme::SPACE_SM);
            if self.pinned.is_some() {
                // Release builds: the server download is pinned into the
                // binary and verified by the machine at boot. One quiet
                // factual line; no URL, no hash, nothing to interact with.
                ui.label(theme::muted(
                    ui,
                    format!("Server {}, verified download.", env!("CARGO_PKG_VERSION")),
                ));
            } else {
                // Development builds only: no pinned artifact exists, so
                // the launch needs one named by hand.
                let arrow = if self.advanced_open { "v" } else { ">" };
                if ui
                    .button(format!("{arrow} Server binary (advanced)"))
                    .clicked()
                {
                    self.advanced_open = !self.advanced_open;
                }
                if self.advanced_open {
                    ui.label(theme::muted(
                        ui,
                        "This build has no pinned server binary; release builds do. Point \
                         these at a jamstreamd build you host; the machine downloads and \
                         verifies it at boot.",
                    ));
                    egui::Grid::new("artifact-grid")
                        .num_columns(2)
                        .spacing(vec2(theme::SPACE_LG, 4.0))
                        .show(ui, |ui| {
                            ui.label(theme::muted(ui, "artifact url"));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.artifact_url)
                                    .desired_width(340.0)
                                    .hint_text("https://..."),
                            );
                            ui.end_row();
                            ui.label(theme::muted(ui, "artifact sha256"));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.artifact_sha256)
                                    .desired_width(340.0)
                                    .hint_text("64 hex characters"),
                            );
                            ui.end_row();
                        });
                }
                if !self.can_launch() {
                    ui.label(theme::muted(
                        ui,
                        "A cloud launch needs the server binary fields above.",
                    ));
                }
            }
        }
        ui.add_space(theme::SPACE_SM);
        ui.horizontal(|ui| {
            if ui.button("Back").clicked() {
                self.back();
            }
            let label = if self.is_local() {
                "Start the session"
            } else {
                "Launch"
            };
            if ui
                .add_enabled(self.can_launch(), egui::Button::new(label))
                .clicked()
            {
                self.begin_launch();
            }
        });
    }

    fn launching_ui(&mut self, ui: &mut Ui) {
        if let Some(err) = self.launch_error.clone() {
            let p = theme::palette_of(ui);
            ui.label(RichText::new(err).color(p.danger));
            if !self.is_local() {
                ui.add_space(theme::SPACE_XS);
                ui.label(theme::muted(
                    ui,
                    "If a machine was launched before the failure, jamstream sweep finds \
                     and removes it.",
                ));
            }
            ui.add_space(theme::SPACE_LG);
            if ui.button("Back").clicked() {
                self.back();
            }
            return;
        }
        let phase = self.launch_phase();
        let steps: [(&str, LaunchPhase); 3] = [
            (
                if self.is_local() {
                    "starting the server process"
                } else {
                    "booting the machine"
                },
                LaunchPhase::Launching,
            ),
            ("waiting for its address", LaunchPhase::WaitingForAddress),
            (
                "checking the server answers",
                LaunchPhase::CheckingReachability,
            ),
        ];
        let active = steps.iter().position(|(_, p)| *p == phase).unwrap_or(0);
        for (i, (label, _)) in steps.iter().enumerate() {
            ui.horizontal(|ui| {
                if i == active {
                    ui.add(egui::Spinner::new().color(theme::palette_of(ui).text_muted));
                    ui.label(*label);
                } else if i < active {
                    ui.label(theme::mono_muted(ui, "done"));
                    ui.label(theme::muted(ui, *label));
                } else {
                    ui.add_space(22.0);
                    ui.label(theme::muted(ui, *label));
                }
            });
        }
        ui.add_space(theme::SPACE_SM);
        ui.label(theme::muted(
            ui,
            "You join automatically the moment the server answers.",
        ));
    }
}

fn guidance(ui: &mut Ui, lines: &[&str]) {
    for line in lines {
        ui.label(theme::muted(ui, *line));
    }
}

/// One labeled secret input; masked unless revealed, never logged.
fn secret_field(ui: &mut Ui, label: &str, value: &mut String, reveal: bool) {
    ui.horizontal(|ui| {
        row_cell(ui, 130.0, |ui| {
            ui.label(theme::muted(ui, label));
        });
        ui.add(
            egui::TextEdit::singleline(value)
                .desired_width(300.0)
                .password(!reveal),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::MemStore;
    use jamstream_cloud::MockProvider;

    fn no_env() -> EnvReader {
        Arc::new(|_| None)
    }

    fn wizard() -> HostWizard {
        HostWizard::new(Arc::new(MemStore::default()), no_env(), test_exec())
    }

    fn test_exec() -> Arc<Executor> {
        Arc::new(Executor::new())
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
        }
    }

    // The wizard opens on the session shape `jamstream host` defaults to.
    // Both surfaces read jamstream_session::limits, so this is a guard
    // against someone reintroducing a local literal.
    #[test]
    fn wizard_opens_on_the_shared_session_defaults() {
        let w = wizard();
        assert_eq!(w.hours, DEFAULT_HOURS);
        assert_eq!(w.musicians, DEFAULT_MUSICIANS);
        assert_eq!(w.listeners, DEFAULT_LISTENERS);
        assert_eq!(w.idle_min, DEFAULT_IDLE_MIN);
        assert_eq!(w.max_hours, DEFAULT_MAX_HOURS);
        assert!(usize::from(w.musicians) <= MAX_MUSICIANS);
        assert!(usize::from(w.listeners) <= MAX_LISTENERS);
    }

    // The wizard's musician count is seats including the host, so a session
    // of N seats mints N musician invites and the server admits all of them.
    #[test]
    fn wizard_seat_count_includes_the_host() {
        let issuer = Issuer::generate();
        let keys = generate_keypair();
        let invites = jamstream_cli::host::mint_invites(
            &issuer,
            SessionId::generate(),
            "203.0.113.10:43210".parse().expect("addr"),
            keys.public,
            4_000_000_000,
            MAX_MUSICIANS as u8,
            0,
        );
        assert_eq!(invites.len(), MAX_MUSICIANS);
        assert_eq!(invites[0].0, "host");
    }

    #[test]
    fn provider_rows_order_and_statuses() {
        let rows = provider_rows(&MemStore::default(), &no_env());
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["local", "digitalocean", "aws", "gcp"]);
        assert_eq!(rows[0].status, ProviderStatus::NoAccountNeeded);
        for row in &rows[1..] {
            assert_eq!(row.status, ProviderStatus::SetupNeeded, "{}", row.name);
        }
    }

    #[test]
    fn environment_credentials_make_a_provider_ready() {
        let env: EnvReader =
            Arc::new(|key| (key == "DIGITALOCEAN_TOKEN").then(|| "dop_v1_x".to_owned()));
        let rows = provider_rows(&MemStore::default(), &env);
        assert_eq!(rows[1].name, "digitalocean");
        assert_eq!(rows[1].status, ProviderStatus::Ready);
        assert_eq!(rows[2].status, ProviderStatus::SetupNeeded);
    }

    #[test]
    fn selecting_a_setup_needed_cloud_opens_the_pane_and_gates_continue() {
        let mut w = wizard();
        assert!(w.select_provider(1)); // digitalocean, no creds
        assert!(w.setup_open);
        assert!(!w.provider_ready());
        assert!(!w.advance_from_provider());
        assert_eq!(w.step, WizardStep::Provider);
        // Local closes the pane and is immediately ready.
        assert!(w.select_provider(0));
        assert!(!w.setup_open);
        assert!(w.provider_ready());
    }

    #[test]
    fn local_skips_the_region_step() {
        let mut w = wizard();
        w.select_provider(0);
        assert!(w.advance_from_provider());
        assert_eq!(w.step, WizardStep::Preview);
        assert_eq!(w.regions.len(), 1);
        assert_eq!(w.regions[0].region.id.as_str(), "local");
        assert_eq!(w.regions[0].price.hourly_microusd, 0);
        assert_eq!(w.selected_region, Some(0));
        // No artifact needed: local launches a binary already on this machine.
        assert!(w.can_launch());
    }

    #[test]
    fn cannot_continue_without_a_provider() {
        let mut w = wizard();
        assert!(!w.continue_to_region(vec![region_row("us-east-1", 16_800, 21.0)]));
        assert!(!w.advance_from_provider());
        assert_eq!(w.step, WizardStep::Provider);
    }

    #[test]
    fn cloud_happy_path_with_fed_rows() {
        let mut w = wizard();
        // Pretend DO creds are present so the row is selectable and ready.
        w.providers[1].status = ProviderStatus::Ready;
        assert!(w.select_provider(1));
        assert!(w.continue_to_region(vec![
            region_row("nyc3", 26_790, 21.0),
            region_row("sfo3", 26_790, 74.0),
        ]));
        assert_eq!(w.step, WizardStep::Region);
        // The top-ranked region is preselected.
        assert_eq!(w.selected_region, Some(0));
        assert!(w.select_region(1));
        assert!(w.continue_to_preview());
        assert_eq!(w.step, WizardStep::Preview);
        let preview = w.preview().expect("preview");
        assert!(preview.total_microusd > 0);
    }

    #[test]
    fn unpinned_cloud_launch_requires_the_artifact_fields() {
        let mut w = wizard();
        // Force the development-build state regardless of how this test
        // binary was compiled.
        w.pinned = None;
        w.providers[1].status = ProviderStatus::Ready;
        w.select_provider(1);
        w.continue_to_region(vec![region_row("nyc3", 26_790, 21.0)]);
        w.continue_to_preview();
        assert!(!w.can_launch());
        w.artifact_url = "https://example.invalid/jamstreamd".to_owned();
        assert!(!w.can_launch());
        w.artifact_sha256 = "a".repeat(64);
        assert!(w.can_launch());
    }

    #[test]
    fn pinned_cloud_launch_has_nothing_to_validate() {
        let mut w = wizard();
        w.pinned = Some(jamstream_cloud::PinnedServerArtifact {
            url: "https://github.com/sean-reid/jamstream/releases/download/v1/jamstreamd-linux-x86_64-musl",
            sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        });
        w.providers[1].status = ProviderStatus::Ready;
        w.select_provider(1);
        w.continue_to_region(vec![region_row("nyc3", 26_790, 21.0)]);
        w.continue_to_preview();
        // The artifact fields stay empty (a pinned build never shows them)
        // and the launch is ready anyway.
        assert!(w.artifact_url.is_empty() && w.artifact_sha256.is_empty());
        assert!(w.can_launch());
    }

    #[test]
    fn back_walks_and_local_back_skips_region() {
        let mut w = wizard();
        assert!(!w.back());
        w.providers[1].status = ProviderStatus::Ready;
        w.select_provider(1);
        w.continue_to_region(vec![region_row("nyc3", 26_790, 21.0)]);
        w.continue_to_preview();
        assert!(w.back());
        assert_eq!(w.step, WizardStep::Region);
        assert!(w.back());
        assert_eq!(w.step, WizardStep::Provider);
        // Selections survive going back.
        assert_eq!(w.selected_provider_name(), Some("digitalocean"));

        // Local's preview goes back to the provider step directly.
        w.select_provider(0);
        w.advance_from_provider();
        assert_eq!(w.step, WizardStep::Preview);
        assert!(w.back());
        assert_eq!(w.step, WizardStep::Provider);
    }

    #[tokio::test]
    async fn check_provider_passes_with_a_working_provider() {
        let provider = MockProvider::with_default_regions(ProviderKind::Aws);
        assert_eq!(check_provider(Box::new(provider)).await, Ok(()));
    }

    #[tokio::test]
    async fn check_provider_reports_failures_verbatim() {
        // A provider with no regions cannot even quote a price.
        let provider = MockProvider::new(ProviderKind::Aws);
        let err = check_provider(Box::new(provider))
            .await
            .expect_err("no regions must fail");
        assert!(err.contains("no regions"), "error was {err:?}");
    }

    #[test]
    fn a_passing_check_saves_credentials_and_readies_the_row() {
        let store = Arc::new(MemStore::default());
        let mut w = HostWizard::new(store.clone(), no_env(), test_exec());
        w.select_provider(1); // digitalocean
        w.setup.do_token = "dop_v1_secret".to_owned();
        w.apply_check_result(Ok(()));
        assert_eq!(
            store.get("digitalocean", "token").as_deref(),
            Some("dop_v1_secret")
        );
        assert_eq!(w.providers[1].status, ProviderStatus::Ready);
        assert!(w.provider_ready());
        assert_eq!(w.check_result, Some(Ok(())));
    }

    #[test]
    fn a_failing_check_saves_nothing() {
        let store = Arc::new(MemStore::default());
        let mut w = HostWizard::new(store.clone(), no_env(), test_exec());
        w.select_provider(1);
        w.setup.do_token = "dop_v1_wrong".to_owned();
        w.apply_check_result(Err("authentication failed: 401".to_owned()));
        assert_eq!(store.get("digitalocean", "token"), None);
        assert_eq!(w.providers[1].status, ProviderStatus::SetupNeeded);
        assert!(matches!(w.check_result, Some(Err(ref e)) if e.contains("401")));
    }

    #[test]
    fn begin_check_refuses_empty_fields_without_spawning() {
        let mut w = wizard();
        w.select_provider(1);
        assert!(!w.begin_check());
        assert!(matches!(w.check_result, Some(Err(_))));
        assert!(!w.busy());
    }

    #[test]
    fn base64_matches_reference_and_round_trips() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xfb, 0xff, 0x00]), "+/8A");
        for sample in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foobar",
            &[0xfb, 0xff, 0x00],
        ] {
            assert_eq!(base64_decode(&base64(sample)).unwrap(), sample.to_vec());
        }
        assert!(base64_decode("!!!!").is_err());
    }

    #[test]
    fn self_destruct_needs_the_do_token() {
        assert!(matches!(
            self_destruct_for(ProviderKind::Aws, None),
            Ok(SelfDestruct::AwsShutdown)
        ));
        assert!(matches!(
            self_destruct_for(ProviderKind::Gcp, None),
            Ok(SelfDestruct::GcpMaxRunDuration)
        ));
        let err = self_destruct_for(ProviderKind::DigitalOcean, None).unwrap_err();
        assert!(err.contains("self-destruct"));
        assert!(matches!(
            self_destruct_for(ProviderKind::DigitalOcean, Some("t".to_owned())),
            Ok(SelfDestruct::ApiToken { .. })
        ));
    }
}
