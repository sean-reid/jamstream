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

use data_encoding::{BASE64, HEXLOWER};
use egui::{RichText, Ui, vec2};
use jamstream_cli::launch::{self, ArtifactOverride};
use jamstream_cli::reason::{self, Attempt};
use jamstream_cloud::{
    BootConfig, CostPreview, HANDSHAKE_CAP, InstanceClass, LaunchSpec, PinnedServerArtifacts,
    Price, ProbeMatrix, Provider, ProviderKind, Region, RegionId, RetentionEnforcement, ServerArch,
    rank, session_tag,
};
use jamstream_protocol::ids::SessionId;
use jamstream_protocol::invite::Issuer;
use jamstream_protocol::transport::generate_keypair;
// The session shape, defined once for the CLI and this wizard alike; see
// jamstream_session::limits.
use jamstream_session::{
    DEFAULT_HOURS, DEFAULT_IDLE_MIN, DEFAULT_LISTENERS, DEFAULT_MAX_HOURS, DEFAULT_MUSICIANS,
    MAX_LISTENERS, MAX_MUSICIANS,
};

use crate::creds::{self, CredStore, EnvReader};
use crate::exec::{Executor, Job};
use crate::screens::recording::{RecordingChoice, RecordingSetup};
use crate::theme;
use crate::widgets::{PICK_INDENT, pick_row, row_cell};

/// The UDP port a cloud session listens on. It is the provider's own default
/// rather than a second copy of the number, because it is also the only port
/// the per-session firewall opens: the two have to be the same value or the
/// machine comes up behind a firewall for a port nothing is listening on.
const SESSION_PORT: u16 = jamstream_cloud::DEFAULT_SESSION_PORT;

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
///
/// The wizard offers [`ProviderKind::ALL`], in that order: local first (no
/// account), then DigitalOcean as the recommended cloud, then the rest. The
/// mock provider is deliberately absent, and it is absent from the enum too,
/// so this cannot forget a fifth provider or invent one.
pub fn provider_rows(creds: &dyn CredStore, env: &EnvReader) -> Vec<ProviderRow> {
    ProviderKind::ALL
        .into_iter()
        .map(|kind| {
            let name = kind.as_str();
            let status = match kind {
                ProviderKind::Local => ProviderStatus::NoAccountNeeded,
                _ if creds::build_provider(name, creds, env).is_ok() => ProviderStatus::Ready,
                _ => ProviderStatus::SetupNeeded,
            };
            let hint = match kind {
                ProviderKind::Local => "this computer; free, LAN or port-forwarded guests",
                ProviderKind::DigitalOcean => "recommended cloud: one token, transfer included",
                ProviderKind::Aws => "more setup; fine if you already use AWS",
                ProviderKind::Gcp => "most setup steps; egress billed on top",
            };
            ProviderRow {
                name: name.to_owned(),
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegionRow {
    pub region: Region,
    pub price: Price,
    /// Probed from this computer; infinite when the region never answered.
    pub worst_rtt_ms: f32,
}

impl RegionRow {
    /// True when a probe actually came back for this region. Unknown is not
    /// a slow measurement and it is certainly not a fast one, so nothing may
    /// print it as a number or treat it as one.
    pub fn measured(&self) -> bool {
        self.worst_rtt_ms.is_finite()
    }
}

/// What the region step got back: the rows to show, and the regions that
/// never made it into them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RegionSurvey {
    pub rows: Vec<RegionRow>,
    /// Region ids the provider offers but cannot run this session's instance
    /// size in. Named in the interface so a table that is shorter than the
    /// provider's own region list says why.
    pub unavailable: Vec<String>,
}

impl From<Vec<RegionRow>> for RegionSurvey {
    fn from(rows: Vec<RegionRow>) -> Self {
        RegionSurvey {
            rows,
            unavailable: Vec::new(),
        }
    }
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
    /// What the retention call actually did, for a session armed to record to
    /// a bucket, and `None` for one that records nowhere or to local disk.
    ///
    /// Carried rather than logged because the answer can be "nothing is
    /// enforcing your choice": a key that can write a lifecycle rule but not
    /// read one back comes out of here as
    /// [`RetentionEnforcement::Manual`](jamstream_cloud::RetentionEnforcement),
    /// and the host has to be told before they record something they think
    /// will be deleted for them.
    pub retention: Option<RetentionEnforcement>,
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
    /// Region ids left out of the table because this session's instance size
    /// is not offered there.
    pub regions_unavailable: Vec<String>,
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
    /// The per-architecture server artifacts pinned into this build, read
    /// once at construction; the launch picks the one matching the
    /// provider's machines. When any pin is present (every release build)
    /// the wizard shows no artifact fields at all: cloud launches silently
    /// use the pinned pair, and the preview step carries one quiet line
    /// saying the server download is verified. When empty (development
    /// builds) the advanced fields below are shown and required for cloud
    /// launches. Public so tests can pin or unpin regardless of how the
    /// test binary was built.
    pub pinned: PinnedServerArtifacts,
    pub advanced_open: bool,
    pub artifact_url: String,
    pub artifact_sha256: String,
    pub launch_error: Option<String>,
    /// Set once the host has stopped waiting on a launch, so the preview step
    /// says what may be running out there. Cleared by the next launch.
    pub launch_abandoned: bool,
    /// What this session records. Off unless the host says otherwise, every
    /// time: nothing is captured by surprise and an unused recorder costs
    /// nothing.
    pub recording: RecordingChoice,
    /// What this computer can record to, refreshed by the app from the
    /// Recording tab each frame. Default is nothing configured, which is what a
    /// wizard nobody has handed one to must assume.
    pub recording_setup: RecordingSetup,
    check_job: Option<Job<Result<(), String>>>,
    regions_job: Option<Job<Result<RegionSurvey, String>>>,
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
            regions_unavailable: Vec::new(),
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
            launch_abandoned: false,
            recording: RecordingChoice::Off,
            recording_setup: RecordingSetup::default(),
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
        self.selected_provider_kind() == Some(ProviderKind::Local)
    }

    /// The selected provider as the cloud crate names it, for the storage
    /// lookups that are keyed by provider. The row's name came from
    /// [`ProviderKind::as_str`], so this is that function's own inverse
    /// rather than a second table of the same four spellings.
    pub fn selected_provider_kind(&self) -> Option<ProviderKind> {
        self.selected_provider_name()?.parse().ok()
    }

    /// Why this session cannot record, if it cannot. A local session records to
    /// this computer's disk and needs no credential, so it never refuses; a
    /// cloud session needs a bucket and a key, and the reason names which is
    /// missing and where to fix it.
    pub fn recording_refusal(&self) -> Option<String> {
        if self.is_local() {
            return None;
        }
        self.recording_setup.refusal()
    }

    pub fn can_record(&self) -> bool {
        self.recording_refusal().is_none()
    }

    /// Turns recording on or off for this launch. Refused while the session
    /// cannot record at all, so the control and the state cannot disagree.
    pub fn set_recording(&mut self, choice: RecordingChoice) -> bool {
        if choice.is_on() && !self.can_record() {
            return false;
        }
        self.recording = choice;
        true
    }

    /// Takes what this computer can record to, from the Recording tab.
    ///
    /// A take that was armed and then lost its bucket or its key goes back to
    /// off here, rather than leaving a lit control over a refusal: a host who
    /// deletes the key they armed with must see recording off, not find out at
    /// the launch.
    pub fn set_recording_setup(&mut self, setup: RecordingSetup) {
        self.recording_setup = setup;
        if self.recording.is_on() && !self.can_record() {
            self.recording = RecordingChoice::Off;
        }
    }

    /// The recording plan the cost preview prices: one stereo stem per musician
    /// when stems are on, and the retention this computer defaults to.
    fn recording_plan(&self) -> Option<jamstream_cloud::RecordingPlan> {
        if !self.recording.is_on() {
            return None;
        }
        let stems = if self.recording.stems() {
            self.musicians
        } else {
            0
        };
        Some(
            jamstream_cloud::RecordingPlan {
                stems,
                ..jamstream_cloud::RecordingPlan::default()
            }
            .retention(self.recording_setup.retention),
        )
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
        if row.name != ProviderKind::Local.as_str() {
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
        let Some(kind) = self.selected_provider_kind() else {
            return Err("pick a provider first".to_owned());
        };
        match kind {
            ProviderKind::DigitalOcean => {
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
            ProviderKind::Aws => {
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
            ProviderKind::Gcp => {
                let json = self.setup.gcp_json.trim();
                if json.is_empty() {
                    return Err("paste the service account JSON first".to_owned());
                }
                creds::gcp_from_json(json, &self.env)
            }
            ProviderKind::Local => Err("this computer needs no credentials".to_owned()),
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
            let saves: &[((&str, &str), &str)] = match self.selected_provider_kind() {
                Some(ProviderKind::DigitalOcean) => &[(creds::DO_TOKEN, &self.setup.do_token)],
                Some(ProviderKind::Aws) => &[
                    (creds::AWS_ACCESS_KEY_ID, &self.setup.aws_access_key_id),
                    (
                        creds::AWS_SECRET_ACCESS_KEY,
                        &self.setup.aws_secret_access_key,
                    ),
                ],
                Some(ProviderKind::Gcp) => {
                    &[(creds::GCP_SERVICE_ACCOUNT_JSON, &self.setup.gcp_json)]
                }
                // Local holds nothing, and nothing selected saves nothing.
                Some(ProviderKind::Local) | None => &[],
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
                self.regions_unavailable = Vec::new();
                self.selected_region = None;
                self.regions_error = None;
                self.step = WizardStep::Region;
                self.regions_job = Some(self.exec.run(survey_regions(provider)));
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
    pub fn continue_to_region(&mut self, survey: impl Into<RegionSurvey>) -> bool {
        if self.step == WizardStep::Provider && self.selected_provider.is_some() {
            self.step = WizardStep::Region;
            self.set_regions(survey.into());
            true
        } else {
            false
        }
    }

    fn set_regions(&mut self, survey: RegionSurvey) {
        // The top row is preselected only when it was actually measured.
        // With nothing measured the order is price alone, which is not a
        // recommendation, and preselecting anyway is how a region nobody
        // timed became the default choice.
        self.selected_region = survey.rows.first().filter(|r| r.measured()).map(|_| 0);
        self.regions = survey.rows;
        self.regions_unavailable = survey.unavailable;
    }

    /// True when the table is up and not one region answered a probe. The
    /// rows are still usable, but their order is price and nothing else.
    pub fn nothing_measured(&self) -> bool {
        !self.regions.is_empty() && self.regions.iter().all(|r| !r.measured())
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

    /// The session's cost, with the recording folded in when the host has turned
    /// it on. Turning stems on is roughly five times the bytes, and this is
    /// where that shows up: the preview is recomputed from the choice every
    /// frame, so the figure moves as the choice does.
    pub fn preview(&self) -> Option<CostPreview> {
        let row = self.selected_region.and_then(|i| self.regions.get(i))?;
        let preview = CostPreview::compute(
            &row.price,
            self.hours,
            self.musicians,
            self.destinations,
            self.listeners,
        );
        Some(match self.recording_estimate() {
            Some(recording) => preview.with_recording(&recording),
            None => preview,
        })
    }

    /// What the take itself costs: storage for the retention period, and the one
    /// download. Priced in the bucket's own region, which is the region the key
    /// was checked against.
    pub fn recording_estimate(&self) -> Option<jamstream_cloud::RecordingEstimate> {
        let plan = self.recording_plan()?;
        let bucket = self.recording_setup.bucket.as_ref()?;
        jamstream_cloud::RecordingEstimate::compute(
            self.selected_provider_kind()?,
            &RegionId::new(&bucket.region),
            &plan,
            self.hours,
        )
        .ok()
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
                || self.pinned.any()
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
        let provider = match self.launch_provider(&name) {
            Ok(p) => p,
            Err(err) => {
                self.launch_error = Some(err);
                self.step = WizardStep::Launching;
                return true;
            }
        };
        // A cloud take goes to the bucket this computer has a checked key for.
        // Built before anything is launched, so a key that vanished from the
        // keychain since the preview costs no machine.
        let recording = match self.storage_for_launch() {
            Ok(storage) => storage,
            Err(err) => {
                self.launch_error = Some(err);
                self.step = WizardStep::Launching;
                return true;
            }
        };
        // Resolved against the architecture this provider launches, and
        // refused here if that architecture has no pin: the machine would
        // download a binary it cannot run.
        let (artifact_url, artifact_sha256) = match resolve_artifact(
            provider.kind() == ProviderKind::Local,
            provider.server_arch(),
            &self.artifact_url,
            &self.artifact_sha256,
            self.pinned,
        ) {
            Ok(pair) => pair,
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
            artifact_url,
            artifact_sha256,
            do_token: creds::lookup(
                self.creds.as_ref(),
                &self.env,
                creds::DO_TOKEN,
                "DIGITALOCEAN_TOKEN",
            ),
            recording,
        };
        *self.launch_phase.lock().expect("launch phase") = LaunchPhase::Launching;
        self.launch_error = None;
        self.launch_abandoned = false;
        self.step = WizardStep::Launching;
        let phase = Arc::clone(&self.launch_phase);
        self.launch_job = Some(self.exec.run(launch_session(provider, params, phase)));
        true
    }

    /// Stops waiting on a launch and goes back to the preview. This gives the
    /// Launching step a way out even when the reachability check never
    /// passes; without it, quitting the app is the only way out.
    ///
    /// The work itself cannot be interrupted: it is a future on the executor
    /// and it may still bring a machine up. So this drops the result rather
    /// than pretending to cancel, and the preview step says a machine may be
    /// running and how to remove it.
    pub fn abandon_launch(&mut self) -> bool {
        if self.step != WizardStep::Launching || self.launch_error.is_some() {
            return false;
        }
        self.launch_job = None;
        self.launch_abandoned = true;
        self.step = WizardStep::Preview;
        true
    }

    /// The provider this launch runs on. A local session that records gets one
    /// armed with the record directory, because a local take goes through the
    /// spawned server's own flags rather than the boot config, exactly as
    /// `jamstream host --record --provider local` arms it.
    ///
    /// A cloud provider is built with the session port, which is the one port
    /// it opens in the firewall it creates for the session.
    fn launch_provider(&self, name: &str) -> Result<Box<dyn Provider>, String> {
        if name == ProviderKind::Local.as_str() && self.recording.is_on() {
            return jamstream_cli::providers::resolve_local_recording(self.recording.stems())
                .map_err(|e| e.to_string());
        }
        creds::build_provider_for_port(name, SESSION_PORT, self.creds.as_ref(), &self.env)
    }

    /// The bucket config a cloud launch carries, or None when this session
    /// records nothing or records to this computer's disk.
    fn storage_for_launch(&self) -> Result<Option<jamstream_cloud::RecordingStorage>, String> {
        if !self.recording.is_on() || self.is_local() {
            return Ok(None);
        }
        let kind = self
            .selected_provider_kind()
            .ok_or("this provider has no recording storage")?;
        let bucket = self
            .recording_setup
            .bucket
            .as_ref()
            .ok_or_else(|| self.recording_setup.refusal().unwrap_or_default())?;
        jamstream_cli::storage::storage_for_launch(
            kind,
            &bucket.name,
            &RegionId::new(&bucket.region),
            self.recording_setup.retention,
            || {
                creds::storage_credential(self.creds.as_ref(), &self.env, kind)
                    .map_err(jamstream_cli::CliError::Usage)
            },
            self.recording.stems(),
        )
        .map(Some)
        .map_err(|e| e.to_string())
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
                Ok(survey) => self.set_regions(survey),
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

/// The artifact a launch hands the boot config, resolved by the same
/// function `jamstream host` resolves it with: the advanced-field override
/// first, then the pin for the architecture this provider launches, and a
/// refusal when neither is there. A local launch downloads nothing and gets
/// the inert placeholders.
///
/// The typed pair is validated here rather than on the VM, where a bad one
/// costs a launch, a boot and a self-destruct before anyone hears about it.
fn resolve_artifact(
    is_local: bool,
    arch: ServerArch,
    url_field: &str,
    sha_field: &str,
    pinned: PinnedServerArtifacts,
) -> Result<(String, String), String> {
    let url = non_empty(url_field);
    let sha = non_empty(sha_field);
    launch::resolve_artifact(
        !is_local,
        arch,
        url.as_deref(),
        sha.as_deref(),
        pinned,
        ArtifactOverride::AdvancedFields,
    )
    .map_err(|e| e.to_string())
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

/// The credential check: the static catalog, one live price call, one
/// authenticated list of jamstream-tagged instances (the same call the
/// docs' `jamstream sweep --dry-run` verification makes; price alone would
/// not exercise authentication on providers with bundled price data), and
/// the provider's launch preflight, so a token that can price sessions but
/// cannot launch them fails here rather than at step 4 of 4. A refusal comes
/// back as a sentence naming the scope or the policy to fix; see
/// `machine_failure`.
pub async fn check_provider(provider: Box<dyn Provider>) -> Result<(), String> {
    let kind = provider.kind();
    let regions = provider.regions();
    let first = regions
        .first()
        .ok_or("provider offers no regions")?
        .id
        .clone();
    provider
        .price(&first)
        .await
        .map_err(|e| machine_failure("pricing a region", kind, e))?;
    provider
        .list_tagged(None)
        .await
        .map_err(|e| machine_failure("listing tagged instances", kind, e))?;
    provider
        .preflight()
        .await
        .map_err(|e| machine_failure("the launch preflight", kind, e))?;
    Ok(())
}

/// The wizard's binding of the shared mapping: every refusal it draws is the
/// provider's machine API saying no, so the remedy is about the scopes and
/// the policy that launch machines, never a bucket. The provider's own
/// response goes to the log, because an EC2 403 names the account number and
/// the IAM ARN of the key that was refused.
fn machine_failure(
    doing: &str,
    provider: ProviderKind,
    err: jamstream_cloud::ProviderError,
) -> String {
    reason::error_sentence(
        doing,
        Attempt::Machines,
        Some(provider),
        &jamstream_cli::CliError::Provider(err),
    )
}

/// The real region step: live price per region, TCP connect timing from
/// this machine against the provider's catalog endpoints, ranked by the
/// shared solver (worst RTT in 5 ms buckets, price breaking ties, unknown
/// last).
///
/// Both facts can be absent and neither absence is fatal here.
/// `priced_regions` drops a region whose instance size the account cannot
/// buy and keeps the rest; a region no probe answered keeps its row and an
/// infinite worst RTT, which the table renders as `no probe`.
async fn survey_regions(provider: Box<dyn Provider>) -> Result<RegionSurvey, String> {
    let table = jamstream_cloud::priced_regions(provider.as_ref())
        .await
        .map_err(|e| machine_failure("pricing the regions", provider.kind(), e))?;
    let regions: Vec<Region> = table.candidates.iter().map(|(r, _)| r.clone()).collect();
    let targets = jamstream_cli::host::catalog_targets(provider.kind(), &regions);
    let rtts = jamstream_cloud::probe_all(&targets).await;
    if rtts.is_empty() && !targets.is_empty() {
        tracing::warn!(
            targets = targets.len(),
            provider = provider.kind().as_str(),
            "no probe target answered; the region table has prices but no latencies"
        );
    }
    let mut matrix = ProbeMatrix::new();
    for (region, rtt_ms) in rtts {
        matrix.insert(0, region, rtt_ms);
    }
    Ok(RegionSurvey {
        rows: rank(&matrix, &table.candidates)
            .into_iter()
            .map(|score| RegionRow {
                region: score.region,
                price: score.price,
                worst_rtt_ms: score.worst_rtt_ms,
            })
            .collect(),
        unavailable: table
            .unavailable
            .into_iter()
            .map(|r| r.id.as_str().to_owned())
            .collect(),
    })
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
    artifact_url: String,
    artifact_sha256: String,
    /// For the DigitalOcean self-destruct arm; read from the credential
    /// store or environment before the job leaves the UI thread.
    do_token: Option<String>,
    /// The bucket a cloud take goes to, absent when the host left recording off
    /// and when the session runs on this computer.
    recording: Option<jamstream_cloud::RecordingStorage>,
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
        artifact_url: params.artifact_url.clone(),
        artifact_sha256: params.artifact_sha256.clone(),
        server_private_key_b64: base64(&server_keys.private),
        issuer_public_key_b64: base64(issuer.public_key().as_bytes()),
        session_id_hex: session_hex.clone(),
        port,
        idle_shutdown_min: params.idle_min,
        max_duration_min: params.max_hours * 60,
        self_destruct: launch::self_destruct_for(provider.kind(), params.do_token)
            .map_err(|e| e.to_string())?,
        // A local session records through the provider's own spawn flags, so
        // this carries a bucket and nothing else, exactly as the CLI's does.
        recording: params.recording.clone(),
    };
    // Proved before the machine exists, through the same call `jamstream host
    // --bucket` makes: the key writes and deletes a probe object under this
    // session's prefix, and the retention rule is applied to it.
    //
    // Applied, not necessarily enforced. The call answers with what the bucket
    // actually agreed to, and a key that cannot read a lifecycle rule back
    // leaves the choice unenforced with a note saying so. That answer rides
    // out in the outcome, so a host sees an unenforced retention choice
    // before arming the recording, not after the bill arrives.
    let retention = match &params.recording {
        Some(storage) => Some(
            jamstream_cli::host::verify_bucket(storage, &session_hex)
                .await
                .map_err(|e| {
                    reason::error_sentence(
                        "arming the bucket",
                        Attempt::Probe,
                        Some(storage.provider),
                        &e,
                    )
                })?,
        ),
        None => None,
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
    let kind = provider.kind();
    let instance = provider
        .launch(spec)
        .await
        .map_err(|e| machine_failure("launching the machine", kind, e))?;
    set_phase(LaunchPhase::WaitingForAddress);
    let instance = launch::wait_for_ip(provider.as_ref(), &session_hex, instance)
        .await
        .map_err(|e| {
            reason::error_sentence(
                "waiting for the machine's address",
                Attempt::Machines,
                Some(kind),
                &e,
            )
        })?;
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
    launch::verify_reachable(&invites[0].1, HANDSHAKE_CAP)
        .await
        .map_err(|e| e.to_string())?;

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
    // Written beside the session record, because `jamstream recordings` and the
    // Takes screen have no other way to find the bucket once the VM that wrote
    // to it is gone. What the bucket did with the retention rule goes with it,
    // for the same reason: it is answered once, here, and read for as long as
    // the takes exist.
    if let Some((storage, applied)) = params.recording.as_ref().zip(retention.as_ref()) {
        jamstream_cli::state::save_recording(
            &state.session_id_hex,
            &jamstream_cli::host::recording_record(storage, applied),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(LaunchOutcome {
        state,
        state_path,
        retention,
    })
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
    HEXLOWER.encode(bytes)
}

/// Standard base64 with padding, the spelling the CLI state schema uses:
/// `jamstream_cli` writes these fields with `data_encoding::BASE64` and this
/// crate reads them back, so both sides go through the same codec.
pub(crate) fn base64(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

/// Inverse of [`base64`]; the invites panel reads the issuer key back out
/// of the state file with it. Whitespace is stripped first, because a state
/// file that has been through a text editor is still a state file.
pub(crate) fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let stripped: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    BASE64
        .decode(stripped.as_bytes())
        .map_err(|err| format!("invalid base64: {err}"))
}

// Rendering. One focused card per step: the step counter and title at the top,
// the step's own body between them and the actions, and the actions on the
// card's bottom edge. Numbers are monospace throughout.

/// The card's width; every step's content is laid out against it.
const CARD_W: f32 = 620.0;

/// What the actions row and the gap above it need. The body is bounded by
/// what is left, so the row keeps the card's bottom edge rather than
/// scrolling off it.
const ACTIONS_H: f32 = 34.0;

/// The body never shrinks past this, whatever the window does; below that it
/// is the card that scrolls.
const MIN_BODY_H: f32 = 120.0;

/// What the card keeps below its body: the panel's own bottom margin and its
/// hairline.
const CARD_BOTTOM_H: f32 = 17.0;

impl HostWizard {
    pub fn ui(&mut self, ui: &mut Ui) -> Option<WizardEvent> {
        let event = self.poll();
        // Measured here and handed down, because inside the scroll area below
        // it a Ui can no longer say how much window there is.
        let room = ui.available_height();
        // A card taller than the window scrolls inside itself, so this outer
        // one only ever moves in a window too short for even the floor the
        // body keeps.
        egui::ScrollArea::vertical()
            .id_salt("wizard-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.card_ui(ui, room);
            });
        event
    }

    fn card_ui(&mut self, ui: &mut Ui, room: f32) {
        theme::focused_column(ui, CARD_W, room, |ui, room| {
            let card_top = ui.cursor().top();
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
                    // The body scrolls and the actions do not: at 800x600 the
                    // preview step is taller than the window, and Back and
                    // Launch have to stay on screen. Each step keeps its own
                    // offset, so a step opens at its own top.
                    let header_h = ui.cursor().top() - card_top;
                    let room_for_body =
                        (room - header_h - ACTIONS_H - CARD_BOTTOM_H).max(MIN_BODY_H);
                    // What the step's own content wants, from the last frame.
                    // A scroll area inside this card cannot be left to size
                    // itself: everything here is inside the outer scroll area,
                    // where a Ui's available height is zero and anything that
                    // sizes to it collapses. So the box is allocated exactly,
                    // and the content's own height is what decides whether a
                    // short step still gets a short card.
                    let natural_key = ui.id().with(("wizard-body-natural", num));
                    let natural: f32 = ui
                        .ctx()
                        .data(|d| d.get_temp(natural_key))
                        .unwrap_or(f32::INFINITY);
                    let body_h = room_for_body.min(natural.max(MIN_BODY_H));
                    let body = ui.allocate_ui(vec2(ui.available_width(), body_h), |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt(("wizard-body", num))
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                match self.step {
                                    WizardStep::Provider => self.provider_ui(ui),
                                    WizardStep::Region => self.region_ui(ui),
                                    WizardStep::Preview => self.preview_ui(ui),
                                    WizardStep::Launching => self.launching_ui(ui),
                                }
                            })
                            .content_size
                            .y
                    });
                    ui.ctx()
                        .data_mut(|d| d.insert_temp(natural_key, body.inner));
                    ui.add_space(theme::SPACE_SM);
                    self.actions_ui(ui);
                });
        });
    }

    /// The step's actions, on the card's bottom edge whatever the body above
    /// them is doing. Every step has a way on and, past the first, a way back;
    /// the launch has a way to stop waiting.
    fn actions_ui(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| match self.step {
            WizardStep::Provider => {
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
            }
            WizardStep::Region => {
                // Nothing to go back from while the probe job runs; once it
                // lands, rows or an error, Back is live again.
                if ui
                    .add_enabled(self.regions_job.is_none(), egui::Button::new("Back"))
                    .clicked()
                {
                    self.back();
                }
                let can_continue = self.selected_region.is_some();
                if ui
                    .add_enabled(can_continue, egui::Button::new("Continue"))
                    .clicked()
                {
                    self.continue_to_preview();
                }
            }
            WizardStep::Preview => {
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
            }
            WizardStep::Launching => {
                if self.launch_error.is_some() {
                    if ui.button("Back").clicked() {
                        self.back();
                    }
                } else if ui
                    .button("Stop waiting")
                    .on_hover_text(if self.is_local() {
                        "goes back to the preview; the server process may already be up"
                    } else {
                        "goes back to the preview; the machine may already be up"
                    })
                    .clicked()
                {
                    self.abandon_launch();
                }
            }
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
    }

    /// Inline credential setup for the selected cloud. Guidance matches
    /// the docs site's provider pages; the check runs a real API call and
    /// only a passing check writes the keychain.
    fn setup_ui(&mut self, ui: &mut Ui) {
        let Some(kind) = self.selected_provider_kind() else {
            return;
        };
        // Indented under the provider row it belongs to, not framed: a panel
        // inside the step card is a card in a card, and the destinations sheet
        // states that rule for the same job and follows it.
        ui.indent("provider-setup", |ui| {
            ui.set_width(ui.available_width());
            match kind {
                ProviderKind::DigitalOcean => {
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
                    secret_field(ui, "API token", &mut self.setup.do_token);
                }
                ProviderKind::Aws => {
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
                    secret_field(ui, "Access key id", &mut self.setup.aws_access_key_id);
                    secret_field(
                        ui,
                        "Secret access key",
                        &mut self.setup.aws_secret_access_key,
                    );
                }
                ProviderKind::Gcp => {
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
                    ui.add(
                        egui::TextEdit::multiline(&mut self.setup.gcp_json)
                            .desired_width(f32::INFINITY)
                            .desired_rows(4)
                            .password(true)
                            .hint_text("paste the downloaded key file's contents"),
                    );
                    character_count(ui, &self.setup.gcp_json);
                }
                // Local runs on this computer and holds no credential, so its
                // row never reaches setup needed and this pane never opens
                // for it.
                ProviderKind::Local => {}
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
                if checking {
                    ui.add(egui::Spinner::new().color(theme::palette_of(ui).text_muted));
                    ui.label(theme::muted(ui, "asking the provider"));
                }
            });
            match &self.check_result {
                Some(Ok(())) => {
                    let p = theme::palette_of(ui);
                    // "On this computer", not "keychain": a key the keychain
                    // refuses as too long (a GCP key on Windows) is kept as a
                    // private file instead, and this line covers both.
                    ui.label(RichText::new("Works. Saved on this computer.").color(p.meter_green));
                }
                Some(Err(err)) => {
                    theme::reason(ui, err.clone());
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
            theme::reason(ui, err);
            return;
        }
        // The solver sorts by coverage, then worst round trip bucketed to
        // 5 ms, then hourly price, so price only decides inside a bucket.
        // A region with no probe has its own bucket at the end.
        if self.nothing_measured() {
            // Zeros here would be a lie the table tells confidently; say
            // instead that the measurement is missing and what is left.
            //
            // Amber, not danger: nothing failed and nothing was lost, the table
            // is still complete, and a region is still pickable on price. The
            // session-full message on the mixer argues the same case and picks
            // the same colour.
            let p = theme::palette_of(ui);
            ui.label(
                RichText::new("No region answered a probe, so none of them are timed.")
                    .color(theme::readable(p.meter_amber, p.surface1, p)),
            );
            ui.label(theme::muted(
                ui,
                "Check this computer's connection and go back to try again, or pick a region \
                 below on price alone.",
            ));
        } else {
            ui.label(theme::muted(
                ui,
                "Sorted by worst round trip in 5 ms steps, with price breaking ties.",
            ));
            ui.label(theme::muted(
                ui,
                "Latency is measured from this computer; bandmates elsewhere will differ.",
            ));
            // Only when there is one on screen: a line about a state that is
            // not happening is a line to read every time for nothing.
            if self.regions.iter().any(|r| !r.measured()) {
                ui.label(theme::muted(
                    ui,
                    "A region that did not answer a probe reads no probe and sorts last.",
                ));
            }
        }
        if !self.regions_unavailable.is_empty() {
            ui.label(theme::muted(
                ui,
                format!(
                    "Not listed: {}. Your account cannot run this session's machine size there.",
                    self.regions_unavailable.join(", ")
                ),
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
            let rtt = if row.measured() {
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
        self.recording_ui(ui);
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
            if self.pinned.any() {
                // Release builds: the server download is pinned into the
                // binary and verified by the machine at boot. One quiet
                // factual line; no URL, no hash, nothing to interact with,
                // and no version number, which would only put this build's
                // number into a published screenshot.
                ui.label(theme::muted(
                    ui,
                    "Pinned server binary, verified at download.",
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
        if self.launch_abandoned {
            ui.add_space(theme::SPACE_SM);
            ui.add(egui::Label::new(theme::muted(
                ui,
                if self.is_local() {
                    "You stopped waiting for the last start. If the server process came up, \
                     Stop strays on the home screen finds and removes it."
                } else {
                    "You stopped waiting for the last launch. If a machine came up, \
                     Stop strays on the home screen finds and removes it."
                },
            )));
        }
    }

    /// The recording choice for this launch: off, the mix, or the mix and
    /// stems. Fixed for the session once it starts, which is why it is here and
    /// not in the session's own Record sheet.
    ///
    /// Off is the top row and the state this arrives in. A session that cannot
    /// record shows the other two disabled with the reason under them, rather
    /// than a control that looks live and does nothing.
    fn recording_ui(&mut self, ui: &mut Ui) {
        ui.label(theme::title(ui, "Recording"));
        let refusal = self.recording_refusal();
        let enabled = refusal.is_none();
        let mut pick = None;
        for choice in RecordingChoice::ALL {
            let size = match choice {
                RecordingChoice::Off => String::new(),
                RecordingChoice::MixOnly => size_hint(self.hours, 0),
                RecordingChoice::MixAndStems => size_hint(self.hours, self.musicians),
            };
            let response = pick_row(
                ui,
                choice.label(),
                self.recording == choice,
                enabled || choice == RecordingChoice::Off,
                |ui| {
                    row_cell(ui, 150.0, |ui| {
                        ui.label(choice.label());
                    });
                    row_cell(ui, 120.0, |ui| {
                        ui.label(theme::mono_muted(ui, size.clone()));
                    });
                },
            );
            if response.clicked() {
                pick = Some(choice);
            }
        }
        if let Some(choice) = pick {
            self.set_recording(choice);
        }
        match (&refusal, self.recording.is_on()) {
            (Some(reason), _) => {
                ui.label(theme::muted(ui, reason.clone()));
            }
            (None, true) if self.is_local() => {
                let dir = jamstream_cli::state::recordings_dir()
                    .map(|d| d.display().to_string())
                    .unwrap_or_else(|_| "this computer's recordings folder".to_owned());
                ui.add(egui::Label::new(theme::muted(ui, format!("Takes land in {dir}."))).wrap());
            }
            (None, true) => {
                if let Some(bucket) = &self.recording_setup.bucket {
                    ui.add(
                        egui::Label::new(theme::muted(
                            ui,
                            format!(
                                "Takes go to {} in {}, {}.",
                                bucket.name,
                                bucket.region,
                                self.recording_setup.retention.label().to_lowercase()
                            ),
                        ))
                        .wrap(),
                    );
                }
            }
            (None, false) => {
                ui.label(theme::muted(
                    ui,
                    "Nothing is captured unless you turn this on.",
                ));
            }
        }
    }

    fn launching_ui(&mut self, ui: &mut Ui) {
        if let Some(err) = self.launch_error.clone() {
            theme::reason(ui, err);
            if !self.is_local() {
                ui.add_space(theme::SPACE_XS);
                ui.label(theme::muted(
                    ui,
                    "If a machine was launched before the failure, Stop strays on the \
                     home screen finds and removes it.",
                ));
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

/// How big a take of `hours` is with `stems` stems, from the same arithmetic
/// that prices it, so the size beside a row and the money below it cannot
/// disagree. Stems are stereo like the mix, which is why turning them on for a
/// four piece is about five times the bytes.
fn size_hint(hours: f32, stems: u8) -> String {
    let plan = jamstream_cloud::RecordingPlan {
        stems,
        ..jamstream_cloud::RecordingPlan::default()
    };
    let bytes = plan.total_bytes((hours.max(0.0) * 3600.0).round() as u64);
    format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
}

fn guidance(ui: &mut Ui, lines: &[&str]) {
    for line in lines {
        ui.label(theme::muted(ui, *line));
    }
}

/// One labeled secret input: masked, with no reveal, and a character count
/// under it. See [`crate::creds`] for why there is no reveal anywhere.
fn secret_field(ui: &mut Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        row_cell(ui, 130.0, |ui| {
            ui.label(theme::muted(ui, label));
        });
        ui.vertical(|ui| {
            ui.add(
                egui::TextEdit::singleline(value)
                    .desired_width(300.0)
                    .password(true),
            );
            character_count(ui, value);
        });
    });
}

/// What stands in for reading a masked field back: a masked field cannot be
/// proofread, and a count catches the paste that took half a token.
fn character_count(ui: &mut Ui, value: &str) {
    ui.label(theme::mono_muted(
        ui,
        format!("{} characters", value.trim().chars().count()),
    ));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::creds::MemStore;
    use jamstream_cloud::{MockProvider, ProviderError};

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

    /// The rows are the cloud crate's providers, in its order, spelled its
    /// way, and every one of them comes back out through
    /// [`HostWizard::selected_provider_kind`]. A fifth provider added to
    /// `ProviderKind::ALL` appears here without an edit; a row this wizard
    /// invented, or a spelling the cloud crate does not answer to, fails.
    #[test]
    fn every_row_is_a_provider_kind_and_maps_back_to_it() {
        let rows = provider_rows(&MemStore::default(), &no_env());
        let names: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();
        let kinds: Vec<String> = ProviderKind::ALL
            .iter()
            .map(|k| k.as_str().to_owned())
            .collect();
        assert_eq!(names, kinds);
        let mut w = wizard();
        for (index, kind) in ProviderKind::ALL.into_iter().enumerate() {
            assert!(w.select_provider(index), "row {index} must be selectable");
            assert_eq!(w.selected_provider_kind(), Some(kind));
            assert_eq!(w.is_local(), kind == ProviderKind::Local);
            assert!(
                !rows[index].hint.is_empty(),
                "{kind} has no hint beside its row"
            );
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

    /// The real job, not the fixture path: a provider whose probe targets
    /// are all unreachable. Every row must come back unmeasured, nothing may
    /// be preselected, and the wizard must be able to say so. This is the
    /// path the snapshot fixtures skip, which is why the defect shipped.
    #[tokio::test]
    async fn every_probe_failing_leaves_the_table_unmeasured_and_unselected() {
        let survey = survey_regions(Box::new(unreachable_provider()))
            .await
            .expect("prices still work when probes do not");
        assert_eq!(survey.rows.len(), 2);
        for row in &survey.rows {
            assert!(
                !row.measured(),
                "{} claims {} ms with no probe behind it",
                row.region.id,
                row.worst_rtt_ms
            );
        }
        // Price is the only signal left, so it decides the order.
        assert_eq!(survey.rows[0].region.id.as_str(), "mock-east");

        let mut w = wizard();
        w.set_regions(survey);
        assert!(w.nothing_measured());
        assert_eq!(
            w.selected_region, None,
            "a region nobody timed must not be the default choice"
        );
        assert!(
            !w.continue_to_preview(),
            "continue stays gated until the host picks one"
        );
    }

    /// One region missing from the probe catalog, the rest fine: the
    /// unmeasured one keeps its row, reads as unknown, and sorts last, while
    /// the measured ones keep the table usable.
    #[test]
    fn an_unmeasured_region_sorts_last_and_is_not_preselected_over_a_measured_one() {
        let mut w = wizard();
        w.providers[1].status = ProviderStatus::Ready;
        assert!(w.select_provider(1));
        assert!(w.continue_to_region(vec![
            region_row("nyc3", 26_790, 21.0),
            region_row("atl1", 9_000, f32::INFINITY),
        ]));
        assert_eq!(w.selected_region, Some(0));
        assert!(w.regions[0].measured());
        assert!(!w.regions[1].measured());
        assert!(!w.nothing_measured());
    }

    /// A region the account cannot buy the machine size in is dropped from
    /// the table, and the table says which, rather than being quietly one
    /// row shorter than the provider's region list.
    #[tokio::test]
    async fn a_region_without_our_machine_size_is_named_not_fatal() {
        let provider = MockProvider::with_default_regions(ProviderKind::DigitalOcean)
            .with_unpriced_region(Region {
                provider: ProviderKind::DigitalOcean,
                id: RegionId::new("mock-atl"),
                display: "Mock Atlanta".to_owned(),
                country: "US".to_owned(),
            });
        let survey = survey_regions(Box::new(provider)).await.expect("a table");
        assert_eq!(survey.rows.len(), 2);
        assert_eq!(survey.unavailable, vec!["mock-atl".to_owned()]);

        let mut w = wizard();
        w.set_regions(survey);
        assert_eq!(w.regions_unavailable, vec!["mock-atl".to_owned()]);
    }

    /// The other half of the same distinction: a provider that cannot be
    /// priced at all is a failure with a message, not a short table.
    #[tokio::test]
    async fn a_credential_failure_on_price_is_still_an_error() {
        let provider = MockProvider::with_default_regions(ProviderKind::DigitalOcean);
        provider.fail_next_prices(1, ProviderError::Auth("token rejected".to_owned()));
        let err = survey_regions(Box::new(provider))
            .await
            .expect_err("an auth failure must not read as a shorter table");
        assert!(err.contains("token rejected"), "{err}");
    }

    /// A provider none of whose regions are in the probe catalog, so the
    /// probe matrix comes back empty. That is the same state as every probe
    /// timing out, which is what the host actually hit, and it gets there
    /// without the test depending on the network being down while it runs.
    /// Real timeouts are covered in the cloud crate by
    /// `probe_all_mixes_reachable_and_unreachable`.
    fn unreachable_provider() -> MockProvider {
        MockProvider::with_default_regions(ProviderKind::DigitalOcean)
    }

    #[test]
    fn unpinned_cloud_launch_requires_the_artifact_fields() {
        let mut w = wizard();
        // Force the development-build state regardless of how this test
        // binary was compiled.
        w.pinned = PinnedServerArtifacts::default();
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
        w.pinned = both_pins();
        w.providers[1].status = ProviderStatus::Ready;
        w.select_provider(1);
        w.continue_to_region(vec![region_row("nyc3", 26_790, 21.0)]);
        w.continue_to_preview();
        // The artifact fields stay empty (a pinned build never shows them)
        // and the launch is ready anyway.
        assert!(w.artifact_url.is_empty() && w.artifact_sha256.is_empty());
        assert!(w.can_launch());
    }

    /// A release-shaped pin set with two distinct downloads, so a test
    /// that selects the wrong one cannot pass by coincidence.
    fn both_pins() -> PinnedServerArtifacts {
        PinnedServerArtifacts {
            x86_64: Some(jamstream_cloud::PinnedServerArtifact {
                url: "https://github.com/sean-reid/jamstream/releases/download/v1/jamstreamd-linux-x86_64-musl",
                sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            }),
            aarch64: Some(jamstream_cloud::PinnedServerArtifact {
                url: "https://github.com/sean-reid/jamstream/releases/download/v1/jamstreamd-linux-aarch64-musl",
                sha256: "4444444444444444444444444444444444444444444444444444444444444444",
            }),
        }
    }

    /// The pin follows the machine, not the build: the launch must select
    /// by the provider's architecture, and the real providers disagree (AWS
    /// launches arm64, DigitalOcean x86_64), so a single-pin resolution
    /// cannot be right for both.
    #[test]
    fn the_launch_selects_the_pin_for_the_providers_architecture() {
        let pins = both_pins();
        let (url, _) =
            resolve_artifact(false, ServerArch::Aarch64, "", "", pins).expect("arm64 pin");
        assert!(url.ends_with("jamstreamd-linux-aarch64-musl"));
        let (url, _) =
            resolve_artifact(false, ServerArch::X86_64, "", "", pins).expect("x86_64 pin");
        assert!(url.ends_with("jamstreamd-linux-x86_64-musl"));
        // The advanced-field override applies to the architecture being
        // launched, whatever the pins say.
        let (url, sha) = resolve_artifact(
            false,
            ServerArch::Aarch64,
            "https://own.example/jamstreamd-arm64",
            &"3".repeat(64),
            pins,
        )
        .expect("override");
        assert_eq!(url, "https://own.example/jamstreamd-arm64");
        assert_eq!(sha, "3".repeat(64));
        // A local launch downloads nothing, so it carries the inert
        // placeholder pair rather than a real build.
        let (url, _) =
            resolve_artifact(true, ServerArch::X86_64, "", "", pins).expect("a local launch");
        assert!(url.contains("invalid"), "{url}");
    }

    /// A cloud launch whose architecture has no pin must refuse before a
    /// machine is paid for, and the error must name the architecture:
    /// launching anyway would produce a machine that cannot run the binary
    /// it downloads.
    #[test]
    fn a_missing_arch_pin_refuses_to_launch_naming_the_architecture() {
        let x86_only = PinnedServerArtifacts {
            aarch64: None,
            ..both_pins()
        };
        let err = resolve_artifact(false, ServerArch::Aarch64, "", "", x86_only)
            .expect_err("an arm64 launch with only an x86_64 pin must refuse");
        assert!(err.contains("aarch64"), "error was: {err}");
        // An unpinned development build gets the advanced-fields message
        // instead.
        let err = resolve_artifact(
            false,
            ServerArch::Aarch64,
            "",
            "",
            PinnedServerArtifacts::default(),
        )
        .expect_err("dev builds need the fields");
        assert!(err.contains("advanced"), "error was: {err}");
    }

    /// The wizard and `jamstream host` take the same operator-typed artifact
    /// pair and both validate it through one resolver, so a typo in the app
    /// is caught before it buys a launch, a boot and a self-destruct: every
    /// refusal the CLI gives is the refusal the wizard gives, word for word.
    #[test]
    fn the_wizard_refuses_a_bad_artifact_pair_exactly_as_the_cli_does() {
        let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        for (url, sha) in [
            ("http://own.example/jamstreamd", sha),
            ("https://own.example/a\";id;\"", sha),
            ("https://own.example/jamstreamd", "abcd"),
        ] {
            let from_the_cli = jamstream_cli::launch::resolve_artifact(
                true,
                ServerArch::X86_64,
                Some(url),
                Some(sha),
                PinnedServerArtifacts::default(),
                ArtifactOverride::AdvancedFields,
            )
            .expect_err("the CLI refuses this pair")
            .to_string();
            let from_the_wizard =
                resolve_artifact(false, ServerArch::X86_64, url, sha, both_pins())
                    .expect_err("so the wizard must refuse it too");
            assert_eq!(from_the_wizard, from_the_cli);
        }
        // And the whole way through the wizard: a launch with a typo in the
        // advanced fields never reaches a provider.
        let env: EnvReader =
            Arc::new(|key| (key == "DIGITALOCEAN_TOKEN").then(|| "dop_v1_x".to_owned()));
        let mut w = HostWizard::new(Arc::new(MemStore::default()), env, test_exec());
        w.select_provider(1);
        w.continue_to_region(vec![region_row("nyc3", 26_790, 21.0)]);
        w.continue_to_preview();
        w.pinned = PinnedServerArtifacts::default();
        w.artifact_url = "http://own.example/jamstreamd".to_owned();
        w.artifact_sha256 = sha.to_owned();
        assert!(w.begin_launch(), "the launch step reports the refusal");
        assert!(w.launch_job.is_none(), "nothing was launched");
        let err = w.launch_error.expect("a refusal");
        assert!(err.contains("https"), "{err}");
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

    /// The launching step needs a way out even when a reachability check
    /// never passes and produces no error for 60 seconds. Stopping the wait
    /// goes back to the preview and says what may be running, and the job
    /// that cannot be interrupted is dropped rather than left to drag the
    /// wizard back into the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_a_wait_returns_to_the_preview_and_says_what_may_be_running() {
        let mut w = wizard();
        w.select_provider(0); // local, which needs no artifact
        assert!(w.advance_from_provider());
        assert_eq!(w.step, WizardStep::Preview);
        // A launch that never lands, which is the state the issue is about.
        let outcome: Job<Result<LaunchOutcome, String>> = w.exec.run(async {
            tokio::time::sleep(Duration::from_secs(600)).await;
            Err("never".to_owned())
        });
        w.launch_job = Some(outcome);
        w.step = WizardStep::Launching;
        assert!(!w.back(), "no error yet, so Back is not the way out");

        assert!(w.abandon_launch());
        assert_eq!(w.step, WizardStep::Preview);
        assert!(
            w.launch_abandoned,
            "the preview has to say a machine may be up"
        );
        assert!(!w.busy(), "the abandoned job is no longer waited on");
        assert!(
            w.poll().is_none(),
            "and cannot pull the wizard into a session"
        );
        // Launching again clears the note and is not refused by the old job.
        assert!(w.begin_launch());
        assert!(!w.launch_abandoned);
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

    /// The setup pane is picked by [`ProviderKind`], so every cloud reaches its
    /// own fields and asks for them in its own words. Local is the one provider
    /// with nothing to set up and says so rather than borrowing a cloud's pane.
    #[test]
    fn each_provider_reaches_its_own_setup_pane() {
        let mut w = wizard();
        let mut refusals: Vec<String> = Vec::new();
        for (index, kind) in ProviderKind::ALL.into_iter().enumerate() {
            assert!(w.select_provider(index), "row {index} must be selectable");
            let err = w
                .provider_from_setup()
                .err()
                .unwrap_or_else(|| panic!("{kind} built a provider from empty fields"));
            if kind == ProviderKind::Local {
                assert!(err.contains("no credentials"), "{kind} said {err:?}");
            } else {
                assert!(
                    !refusals.contains(&err),
                    "{kind} shares a pane with another provider: {err:?}"
                );
                refusals.push(err);
            }
        }
        assert_eq!(refusals.len(), ProviderKind::ALL.len() - 1);
    }

    /// A cloud bucket this computer has a key for, as the app hands one over.
    fn armed_setup() -> RecordingSetup {
        RecordingSetup {
            bucket: Some(crate::prefs::Bucket {
                name: "my-jams".to_owned(),
                region: "nyc3".to_owned(),
            }),
            has_key: true,
            retention: jamstream_cloud::Retention::Days30,
        }
    }

    /// A wizard on the preview step for a DigitalOcean session.
    fn cloud_preview() -> HostWizard {
        let mut w = wizard();
        w.providers[1].status = ProviderStatus::Ready;
        w.select_provider(1);
        w.continue_to_region(vec![region_row("nyc3", 26_790, 21.0)]);
        w.continue_to_preview();
        w
    }

    /// Recording is off in a fresh wizard and stays off unless a host says
    /// otherwise. Nothing is captured by surprise, and an unused recorder costs
    /// nothing, so the default is the whole feature's safety property.
    #[test]
    fn recording_is_off_by_default_and_a_session_with_no_key_cannot_turn_it_on() {
        let mut w = cloud_preview();
        assert_eq!(w.recording, RecordingChoice::Off);
        assert!(!w.can_record());
        let refusal = w.recording_refusal().expect("no bucket, no key");
        assert!(refusal.contains("Recording tab"), "{refusal}");
        assert!(!w.set_recording(RecordingChoice::MixOnly));
        assert_eq!(w.recording, RecordingChoice::Off);
        // And nothing is armed, so the launch carries no bucket.
        assert_eq!(w.storage_for_launch(), Ok(None));

        w.recording_setup = armed_setup();
        assert!(w.can_record());
        assert!(w.set_recording(RecordingChoice::MixAndStems));
        assert_eq!(w.recording, RecordingChoice::MixAndStems);
    }

    /// A local session records to this computer's disk, so it needs no bucket
    /// and no key: the credential that a cloud take requires does not exist in
    /// this path at all.
    #[test]
    fn a_local_session_can_record_with_no_credential_and_carries_no_bucket() {
        let mut w = wizard();
        w.select_provider(0);
        w.advance_from_provider();
        assert_eq!(w.recording_refusal(), None);
        assert!(w.set_recording(RecordingChoice::MixAndStems));
        // Local recording goes through the spawned server's flags, not the boot
        // config, so no storage config is built for it.
        assert_eq!(w.storage_for_launch(), Ok(None));
        // And the provider it launches is the one with the record directory
        // armed, which is what makes the take happen at all.
        assert!(w.launch_provider("local").is_ok());
    }

    /// The launch config a cloud take carries: the bucket from the Recording
    /// tab, the key from the keychain, and the stems choice from this screen.
    #[test]
    fn arming_a_cloud_launch_carries_the_bucket_the_key_and_the_stems_choice() {
        let store = Arc::new(MemStore::default());
        let mut w = HostWizard::new(store.clone(), no_env(), test_exec());
        w.providers[1].status = ProviderStatus::Ready;
        w.select_provider(1);
        w.recording_setup = armed_setup();
        w.recording = RecordingChoice::MixAndStems;
        // A bucket with no key in the keychain must not launch: the VM would
        // come up with nothing to sign uploads with.
        let err = w.storage_for_launch().expect_err("no key is saved");
        assert!(err.contains("SPACES_ACCESS_KEY_ID"), "{err}");

        creds::save_storage_credential(
            store.as_ref(),
            ProviderKind::DigitalOcean,
            "DO00ID",
            "0000-fake-secret",
        )
        .expect("save");
        let storage = w
            .storage_for_launch()
            .expect("a checked key arms the launch")
            .expect("a bucket");
        assert_eq!(storage.bucket, "my-jams");
        assert_eq!(storage.region, "nyc3");
        assert_eq!(storage.provider, ProviderKind::DigitalOcean);
        assert!(storage.stems);
        assert_eq!(storage.retention, jamstream_cloud::Retention::Days30);
        // The key is in the config and out of its Debug, which is where a
        // launch failure would print one.
        let debug = format!("{storage:?}");
        assert!(!debug.contains("0000-fake-secret"), "{debug}");
    }

    /// The 5x that has to be visible at the moment of the decision: stems for a
    /// four piece are about five times the mix, and the preview moves when the
    /// choice does rather than at launch.
    #[test]
    fn the_cost_preview_moves_when_the_recording_choice_changes() {
        let mut w = cloud_preview();
        w.recording_setup = armed_setup();
        w.musicians = 4;
        let session_only = w.preview().expect("a preview").total_microusd;
        let rows_without = w.preview().expect("a preview").line_items.len();

        assert!(w.set_recording(RecordingChoice::MixOnly));
        let mix = w.preview().expect("a preview");
        assert!(
            mix.total_microusd > session_only,
            "recording must show up in the total"
        );
        assert_eq!(
            mix.line_items.len(),
            rows_without + 2,
            "the storage line and the download line both belong on screen"
        );
        let mix_bytes = w.recording_estimate().expect("an estimate").total_bytes;

        assert!(w.set_recording(RecordingChoice::MixAndStems));
        let stems = w.preview().expect("a preview");
        assert!(
            stems.total_microusd > mix.total_microusd,
            "stems cost more than the mix alone: {} vs {}",
            stems.total_microusd,
            mix.total_microusd
        );
        let stems_bytes = w.recording_estimate().expect("an estimate").total_bytes;
        // Four stereo stems beside a stereo mix: five times the bytes, which is
        // the figure the design says has to be visible here.
        assert_eq!(stems_bytes, mix_bytes * 5);
        // And the size beside the row says the same thing as the estimate.
        assert_eq!(
            size_hint(w.hours, w.musicians),
            format!("{:.1} GB", stems_bytes as f64 / 1_000_000_000.0)
        );

        // Off again and the recording lines go with it.
        assert!(w.set_recording(RecordingChoice::Off));
        assert_eq!(w.preview().expect("a preview").total_microusd, session_only);
        assert!(w.recording_estimate().is_none());
    }

    /// A bucket in a region the provider has no bucket service in is refused
    /// before a machine is paid for, with the region named.
    #[test]
    fn a_bucket_in_a_region_with_no_storage_service_refuses_the_launch() {
        let store = Arc::new(MemStore::default());
        let mut w = HostWizard::new(store.clone(), no_env(), test_exec());
        w.providers[1].status = ProviderStatus::Ready;
        w.select_provider(1);
        w.recording_setup = RecordingSetup {
            bucket: Some(crate::prefs::Bucket {
                name: "my-jams".to_owned(),
                region: "atl1".to_owned(),
            }),
            has_key: true,
            retention: jamstream_cloud::Retention::Days30,
        };
        w.recording = RecordingChoice::MixOnly;
        creds::save_storage_credential(
            store.as_ref(),
            ProviderKind::DigitalOcean,
            "DO00ID",
            "0000-fake-secret",
        )
        .expect("save");
        let err = w
            .storage_for_launch()
            .expect_err("Spaces is not in every region");
        assert!(err.contains("atl1"), "{err}");
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
}
