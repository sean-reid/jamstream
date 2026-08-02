//! GCP Compute Engine provider. Launches one short-lived jam-session VM
//! per launch call, relies on `scheduling.maxRunDuration` with
//! `instanceTerminationAction=DELETE` as the provider-enforced hard cap
//! (see [`crate::cloudinit::SelfDestruct::GcpMaxRunDuration`]), labels
//! everything it creates, lists by label, and destroys by name.
//!
//! # How a GCP session ends
//!
//! The instance carries no service account, so nothing on it can call the
//! API, which leaves three ways out and no fourth: `jamstream end`, the
//! sweeper on the next app or CLI launch, or `maxRunDuration` expiring.
//! That last one is why the cap is set from the session's own
//! `max_duration_min` rather than a constant, and why the in-guest guard
//! stops the server instead of powering the VM off: Compute Engine clears
//! a VM's pending termination timestamp when the VM stops, so a
//! powered-off instance outlives its own cap.
//!
//! # Authentication
//!
//! GCP service-account keys require an RS256-signed JWT to mint an OAuth2
//! access token. This crate signs that JWT natively with aws-lc-rs (see
//! [`ServiceAccountTokenSource`]); no gcloud subprocess is needed. Three
//! modes are supported:
//!
//! 1. [`GcpProvider::from_env`] with `GOOGLE_APPLICATION_CREDENTIALS`
//!    pointing at a service-account key file: the key is parsed, a JWT is
//!    signed, and access tokens are minted and cached natively. The
//!    project id comes from `GOOGLE_CLOUD_PROJECT` or, failing that, the
//!    key's own `project_id` field.
//! 2. [`GcpProvider::from_env`] with `GOOGLE_CLOUD_PROJECT` plus
//!    `GCP_ACCESS_TOKEN`: a pre-minted bearer token from the environment
//!    (for example `gcloud auth print-access-token`). This pair takes
//!    precedence when both modes are configured.
//! 3. [`GcpProvider::with_access_token`]: the caller supplies an OAuth2
//!    bearer token directly (tests inject a fake). The token is opaque,
//!    never logged, and redacted from `Debug` output.
//!
//! When no credentials are present at all, `from_env` fails with an Auth
//! error that explains the supported modes. Arbitrary refresh strategies
//! plug in through [`TokenSource`].
//!
//! # Zone selection
//!
//! The Compute API addresses instances by zone, not region. JamStream
//! uses one deterministic default zone per catalog region: the region id
//! plus a `-b` suffix (`us-central1` becomes `us-central1-b`). Zone `b`
//! exists in every region in the static catalog below.
//!
//! # Label mapping
//!
//! Canonical JamStream tags are free-form strings; GCP labels are limited
//! to lowercase letters, digits, and dashes, 63 characters. The mapping
//! is reversible:
//!
//! * a tag component that already matches `[a-z][a-z0-9-]*` and does not
//!   start with the reserved escape prefix `x--` passes through verbatim
//!   (session ids are lowercase hex, so the common case is untouched);
//! * anything else becomes `x--` followed by the lowercase hex encoding
//!   of its UTF-8 bytes, and is decoded back to the canonical string on
//!   read;
//! * a component whose encoding exceeds 63 characters is rejected at
//!   launch rather than truncated, since truncation would break the
//!   round trip.
//!
//! Every launch also adds the marker label `jamstream=true` so untargeted
//! list calls can filter server-side; the tag key `jamstream` is reserved
//! and the marker is stripped when labels are decoded back into tags.
//!
//! # Pricing
//!
//! Prices come from the bundled `data/gcp_prices.json` snapshot of public
//! on-demand e2-small and e2-medium rates (the e2-medium column is an
//! approximation, roughly double e2-small); refresh expectations are
//! documented in that file. Zone listing follows `nextPageToken` until
//! the token runs out.

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use data_encoding::HEXLOWER;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::task::JoinSet;

use crate::artifact::ServerArch;
use crate::http::{client, send_retrying};
use crate::provider::{Provider, ProviderError, Result};
use crate::types::{
    ANY_IPV4, DEFAULT_SESSION_PORT, IngressRule, Instance, InstanceClass, LaunchSpec, Listing,
    Price, ProviderKind, Region, RegionId, SESSION_TAG_KEY,
};

pub use super::gcp_auth::ServiceAccountTokenSource;

/// Static region catalog: (region id, display name, ISO country).
const CATALOG: &[(&str, &str, &str)] = &[
    ("us-central1", "Iowa", "US"),
    ("us-east1", "South Carolina", "US"),
    ("us-east4", "N. Virginia", "US"),
    ("us-west1", "Oregon", "US"),
    ("us-west2", "Los Angeles", "US"),
    ("northamerica-northeast1", "Montreal", "CA"),
    ("europe-west1", "Belgium", "BE"),
    ("europe-west2", "London", "GB"),
    ("europe-west3", "Frankfurt", "DE"),
];

const DEFAULT_BASE_URL: &str = "https://compute.googleapis.com";
/// Provider-enforced hard cap default, in the units GCP's `maxRunDuration`
/// wants. Derived rather than spelled, because it was a third copy of the
/// twelve hours: the pin between the session limit and the local provider did
/// not reach here, so this could have drifted from both and only GCP hosts
/// would have noticed.
const DEFAULT_MAX_RUN_SECONDS: u64 = super::local::DEFAULT_MAX_DURATION_MIN as u64 * 60;
/// Debian 12 has cloud-init and reads the `user-data` metadata key.
const SOURCE_IMAGE: &str = "projects/debian-cloud/global/images/family/debian-12";
/// Marker label added to every launch so `list_tagged(None)` can filter
/// server-side. The tag key `jamstream` is reserved for this marker.
const MARKER_LABEL_KEY: &str = "jamstream";
/// Reserved prefix introducing a hex-escaped label component.
const ESCAPE_PREFIX: &str = "x--";
const MAX_LABEL_LEN: usize = 63;
/// The auto-mode network every project gets, given as a partial resource
/// URL, which is what the API accepts here.
const DEFAULT_NETWORK: &str = "global/networks/default";
/// Prefix on the network tag and both firewall rule names of a session.
const FIREWALL_PREFIX: &str = "jamstream-";
const ALLOW_SUFFIX: &str = "allow";
const DENY_SUFFIX: &str = "deny";

const PRICES_JSON: &str = include_str!("../../data/gcp_prices.json");

/// Pluggable OAuth2 access token supplier. Implementations must treat the
/// token as a secret: never log it and redact it from any Debug output.
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn access_token(&self) -> Result<String>;
}

/// A fixed caller-supplied bearer token.
struct StaticToken(String);

#[async_trait]
impl TokenSource for StaticToken {
    async fn access_token(&self) -> Result<String> {
        Ok(self.0.clone())
    }
}

impl fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticToken(<redacted>)")
    }
}

pub struct GcpProvider {
    project_id: String,
    token: Arc<dyn TokenSource>,
    base_url: String,
    max_run_seconds: Option<u64>,
    /// The one port the per-session firewall rule opens.
    session_port: u16,
    http: reqwest::Client,
}

impl fmt::Debug for GcpProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcpProvider")
            .field("project_id", &self.project_id)
            .field("base_url", &self.base_url)
            .field("max_run_seconds", &self.max_run_seconds)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Result of an inherent [`GcpProvider::refresh`]: the instance view plus
/// the raw GCP status string (`PROVISIONING`, `RUNNING`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshedInstance {
    pub instance: Instance,
    pub status: String,
}

impl GcpProvider {
    /// Auth mode 1: a caller-supplied OAuth2 bearer token, treated as
    /// opaque and never logged.
    pub fn with_access_token(project_id: String, token: String) -> Self {
        Self::with_token_source(project_id, Arc::new(StaticToken(token)))
    }

    /// Fully pluggable token source for callers that refresh tokens
    /// themselves (for example by shelling out to gcloud).
    pub fn with_token_source(project_id: String, token: Arc<dyn TokenSource>) -> Self {
        GcpProvider {
            project_id,
            token,
            base_url: DEFAULT_BASE_URL.to_owned(),
            max_run_seconds: None,
            session_port: DEFAULT_SESSION_PORT,
            http: client(),
        }
    }

    /// Session UDP port the firewall rule opens; see
    /// [`Provider::session_port`].
    pub fn with_session_port(mut self, port: u16) -> Self {
        self.session_port = port;
        self
    }

    /// Credentials from the environment: `GOOGLE_CLOUD_PROJECT` plus
    /// `GCP_ACCESS_TOKEN` when both are set, otherwise a service-account
    /// key file named by `GOOGLE_APPLICATION_CREDENTIALS` (signed and
    /// exchanged natively, no gcloud involved). Anything else fails with
    /// an Auth error that spells out the supported modes; see the module
    /// docs.
    pub fn from_env() -> Result<Self> {
        Self::from_env_with(&|key| std::env::var(key).ok())
    }

    fn from_env_with(get: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        if let (Some(project), Some(token)) = (get("GOOGLE_CLOUD_PROJECT"), get("GCP_ACCESS_TOKEN"))
        {
            return Ok(Self::with_access_token(project, token));
        }
        if let Some(path) = get("GOOGLE_APPLICATION_CREDENTIALS") {
            let source = ServiceAccountTokenSource::from_file(&path)?;
            let project = get("GOOGLE_CLOUD_PROJECT")
                .or_else(|| source.project_id().map(str::to_owned))
                .ok_or_else(|| {
                    ProviderError::Auth(format!(
                        "service account key {path} has no project_id field and \
                         GOOGLE_CLOUD_PROJECT is not set; set GOOGLE_CLOUD_PROJECT to the \
                         target project"
                    ))
                })?;
            return Ok(Self::with_token_source(project, Arc::new(source)));
        }
        Err(ProviderError::Auth(
            "no GCP credentials: set GOOGLE_APPLICATION_CREDENTIALS to a service account key \
             file (RS256 signing is native, no gcloud needed), or set GOOGLE_CLOUD_PROJECT and \
             GCP_ACCESS_TOKEN (for example from `gcloud auth print-access-token`), or construct \
             the provider with GcpProvider::with_access_token(project_id, token)."
                .to_owned(),
        ))
    }

    /// Overrides the API endpoint (tests point this at a mock server).
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url.trim_end_matches('/').to_owned();
        self
    }

    /// Overrides the `scheduling.maxRunDuration` hard cap for every launch,
    /// ahead of the session's own cap. Ordinarily the cap comes from the
    /// launch spec: see [`GcpProvider::launch_body`].
    pub fn with_max_run_seconds(mut self, seconds: u64) -> Self {
        self.max_run_seconds = Some(seconds);
        self
    }

    /// GET of a single instance by name: fills in the ephemeral public IP
    /// (`natIP`) once GCP has assigned it, and reports the raw status.
    pub async fn refresh(&self, region: &RegionId, name: &str) -> Result<RefreshedInstance> {
        let region = self.require_region(region)?;
        let url = self.instance_url(&region.id, name);
        let token = self.token.access_token().await?;
        let resp = send_retrying(|| self.http.get(&url).bearer_auth(&token)).await?;
        let raw: GcpInstance = resp
            .json()
            .await
            .map_err(|e| ProviderError::Other(format!("gcp instance response parse: {e}")))?;
        let status = raw.status.clone();
        Ok(RefreshedInstance {
            instance: instance_from_gcp(raw, region),
            status,
        })
    }

    fn require_region(&self, id: &RegionId) -> Result<Region> {
        self.regions()
            .into_iter()
            .find(|r| &r.id == id)
            .ok_or_else(|| ProviderError::NotFound(format!("unknown gcp region {id}")))
    }

    fn zone_url(&self, region: &RegionId) -> String {
        format!(
            "{}/compute/v1/projects/{}/zones/{}/instances",
            self.base_url,
            self.project_id,
            default_zone(region.as_str())
        )
    }

    fn instance_url(&self, region: &RegionId, name: &str) -> String {
        format!("{}/{}", self.zone_url(region), name)
    }

    /// Price for one instance class in one region, from the bundled
    /// snapshot in `data/gcp_prices.json`.
    pub fn price_for(&self, region: &RegionId, class: InstanceClass) -> Result<Price> {
        self.require_region(region)?;
        let table = price_table();
        let per_region = match class {
            InstanceClass::Small => &table.hourly_microusd,
            InstanceClass::Standard => &table.e2_medium_hourly_microusd,
        };
        let hourly = per_region
            .get(region.as_str())
            .copied()
            .ok_or_else(|| ProviderError::NotFound(format!("no gcp price for region {region}")))?;
        Ok(Price {
            hourly_microusd: hourly,
            egress_microusd_per_gb: table.egress_microusd_per_gb,
            included_egress_gb: table.included_egress_gb,
        })
    }

    /// Builds the instance insert body. Factored out so the shape is unit
    /// testable without a server.
    ///
    /// `maxRunDuration` is the session's own hard cap, read back out of the
    /// user-data that carries it. It reaches this call no other way:
    /// `LaunchSpec` has no field for the cap, and a builder on this type
    /// would only ever be set by the CLI, while the desktop app constructs
    /// its provider itself. Getting it wrong is not cosmetic: it is the one
    /// mechanism that ends a GCP session.
    fn launch_body(&self, spec: &LaunchSpec, name: &str) -> Result<Value> {
        let zone = default_zone(spec.region.id.as_str());
        let mut labels = serde_json::Map::new();
        for (key, value) in &spec.tags {
            labels.insert(label_encode(key)?, Value::String(label_encode(value)?));
        }
        labels.insert(
            MARKER_LABEL_KEY.to_owned(),
            Value::String("true".to_owned()),
        );
        let seconds = self
            .max_run_seconds
            .or_else(|| session_max_run_seconds(&spec.user_data))
            .unwrap_or(DEFAULT_MAX_RUN_SECONDS);
        let session = spec.session_id().ok_or_else(|| {
            ProviderError::Other(format!(
                "launch spec has no {SESSION_TAG_KEY} tag; refusing to create an instance the sweeper cannot find"
            ))
        })?;
        Ok(json!({
            "name": name,
            "machineType": format!("zones/{zone}/machineTypes/{}", machine_type(spec.instance_class)),
            "disks": [{
                "boot": true,
                "autoDelete": true,
                "initializeParams": { "sourceImage": SOURCE_IMAGE },
            }],
            "networkInterfaces": [{
                "network": DEFAULT_NETWORK,
                // Ephemeral public IP.
                "accessConfigs": [{ "type": "ONE_TO_ONE_NAT", "name": "External NAT" }],
            }],
            // The firewall rules for this session target this tag and
            // nothing else in the project.
            "tags": { "items": [network_tag(session)?] },
            "metadata": { "items": [
                // cloud-init on Debian images reads the "user-data" key.
                { "key": "user-data", "value": spec.user_data },
                // Project-wide SSH keys would otherwise reach a session VM,
                // and anyone holding one could read user-data back out of
                // /var/lib/cloud. Nothing needs to log in to this box.
                { "key": "block-project-ssh-keys", "value": "TRUE" },
            ] },
            "labels": labels,
            // Empty on purpose, and stated rather than left out. The raw
            // Compute API attaches no service account when the field is
            // absent, unlike gcloud and the console, which default one in
            // on the client side; anyone reading this should see the
            // decision instead of inferring it from an omission. A session
            // VM parses unauthenticated UDP from the internet and has no
            // call of its own to make, so it carries no credential.
            "serviceAccounts": [],
            "scheduling": {
                // Duration fields serialize int64 seconds as a JSON string.
                "maxRunDuration": { "seconds": seconds.to_string() },
                "instanceTerminationAction": "DELETE",
            },
        }))
    }

    fn firewalls_url(&self) -> String {
        format!(
            "{}/compute/v1/projects/{}/global/firewalls",
            self.base_url, self.project_id
        )
    }

    /// The two rules a session needs on the `default` network, which ships
    /// with nothing that would let a musician's UDP reach an instance and
    /// with tcp/22 open to the whole internet:
    ///
    /// * allow udp/{port} from anywhere at priority 900,
    /// * deny everything else at priority 1000.
    ///
    /// Both target this session's network tag only, so the project's own
    /// rules and its other instances are untouched. The deny rule is what
    /// takes `default-allow-ssh` and `default-allow-rdp` (priority 65534)
    /// out of the picture for this instance without editing them.
    fn firewall_bodies(&self, session: &str) -> Result<Vec<(String, Value)>> {
        let tag = network_tag(session)?;
        let allow = format!("{tag}-{ALLOW_SUFFIX}");
        let deny = format!("{tag}-{DENY_SUFFIX}");
        for name in [&allow, &deny] {
            if name.len() > MAX_LABEL_LEN {
                return Err(ProviderError::Other(format!(
                    "gcp firewall name {name:?} is {} characters; the cap is {MAX_LABEL_LEN}",
                    name.len()
                )));
            }
        }
        Ok(vec![
            (
                allow.clone(),
                json!({
                    "name": allow,
                    "description": "JamStream session traffic",
                    "network": DEFAULT_NETWORK,
                    "direction": "INGRESS",
                    "priority": 900,
                    "sourceRanges": [ANY_IPV4],
                    "targetTags": [tag],
                    "allowed": [{ "IPProtocol": "udp", "ports": [self.session_port.to_string()] }],
                }),
            ),
            (
                deny.clone(),
                json!({
                    "name": deny,
                    "description": "JamStream: nothing but the session port",
                    "network": DEFAULT_NETWORK,
                    "direction": "INGRESS",
                    "priority": 1000,
                    "sourceRanges": [ANY_IPV4],
                    "targetTags": [tag],
                    "denied": [{ "IPProtocol": "all" }],
                }),
            ),
        ])
    }

    /// Inserts both rules, tolerating the ones a previous attempt at the
    /// same session already created.
    async fn ensure_firewalls(&self, session: &str) -> Result<()> {
        let token = self.token.access_token().await?;
        let url = self.firewalls_url();
        for (name, body) in self.firewall_bodies(session)? {
            let resp = send_retrying(|| self.http.post(&url).bearer_auth(&token).json(&body)).await;
            match resp {
                Ok(resp) => {
                    let op: Value = resp.json().await.map_err(|e| {
                        ProviderError::Other(format!("gcp firewall response parse: {e}"))
                    })?;
                    if let Some(err) = op.get("error") {
                        return Err(ProviderError::Other(format!(
                            "gcp firewall insert failed for {name}: {err}"
                        )));
                    }
                }
                // 409 is "alreadyExists", which a relaunch of the same
                // session hits.
                Err(ProviderError::Other(msg)) if msg.contains("http 409") => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    /// Every firewall rule JamStream created, by name.
    async fn jamstream_firewalls(&self) -> Result<Vec<GcpFirewall>> {
        let token = self.token.access_token().await?;
        let url = self.firewalls_url();
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let resp = send_retrying(|| {
                let mut req = self.http.get(&url).bearer_auth(&token);
                if let Some(t) = &page_token {
                    req = req.query(&[("pageToken", t.as_str())]);
                }
                req
            })
            .await?;
            let list: GcpFirewallList = resp.json().await.map_err(|e| {
                ProviderError::Other(format!("gcp firewall list response parse: {e}"))
            })?;
            out.extend(
                list.items
                    .into_iter()
                    .filter(|f| f.name.starts_with(FIREWALL_PREFIX)),
            );
            page_token = list.next_page_token.filter(|t| !t.is_empty());
            if page_token.is_none() {
                return Ok(out);
            }
        }
    }
}

#[async_trait]
impl Provider for GcpProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Gcp
    }

    /// The e2 machine types are x86_64, as is the Debian 12 image family.
    fn server_arch(&self) -> ServerArch {
        ServerArch::X86_64
    }

    fn regions(&self) -> Vec<Region> {
        CATALOG
            .iter()
            .map(|(id, display, country)| Region {
                provider: ProviderKind::Gcp,
                id: RegionId::new(*id),
                display: (*display).to_owned(),
                country: (*country).to_owned(),
            })
            .collect()
    }

    /// Region price for the Standard session size (e2-medium); use
    /// `price_for` to price a specific class.
    async fn price(&self, region: &RegionId) -> Result<Price> {
        self.price_for(region, InstanceClass::Standard)
    }

    async fn launch(&self, spec: LaunchSpec) -> Result<Instance> {
        let region = self.require_region(&spec.region.id)?;
        let name = generate_name();
        let body = self.launch_body(&spec, &name)?;
        // The rules go in first: the instance's network tag is what they
        // target, so an instance that exists before them is an instance the
        // default network's implied deny is dropping session traffic to.
        let session = spec.session_id().expect("launch_body requires a session");
        self.ensure_firewalls(session).await?;
        let url = self.zone_url(&region.id);
        let token = self.token.access_token().await?;
        let resp = send_retrying(|| self.http.post(&url).bearer_auth(&token).json(&body)).await?;
        let op: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Other(format!("gcp operation response parse: {e}")))?;
        if let Some(err) = op.get("error") {
            return Err(ProviderError::Other(format!(
                "gcp insert operation failed: {err}"
            )));
        }
        // The insert returns an Operation; the ephemeral IP does not exist
        // yet. Callers poll `refresh` for the natIP.
        Ok(Instance {
            provider: ProviderKind::Gcp,
            region,
            id: name,
            public_ip: None,
            tags: spec.tags,
        })
    }

    async fn destroy(&self, region: &RegionId, id: &str) -> Result<()> {
        let region = self.require_region(region)?;
        let url = self.instance_url(&region.id, id);
        let token = self.token.access_token().await?;
        send_retrying(|| self.http.delete(&url).bearer_auth(&token)).await?;
        Ok(())
    }

    async fn list_tagged(&self, session_tag: Option<&str>) -> Result<Listing> {
        let filter = match session_tag {
            Some(session) => format!(
                "labels.{}={}",
                label_encode(SESSION_TAG_KEY)?,
                label_encode(session)?
            ),
            None => format!("labels.{MARKER_LABEL_KEY}=true"),
        };
        let token = self.token.access_token().await?;
        let zones = self.regions();
        let mut tasks = JoinSet::new();
        for region in zones.iter().cloned() {
            let http = self.http.clone();
            let url = self.zone_url(&region.id);
            let token = token.clone();
            let filter = filter.clone();
            let id = region.id.clone();
            tasks.spawn(async move { (id, list_zone(http, url, token, filter, region).await) });
        }
        let mut instances = Vec::new();
        // Struck off as each zone answers, so a zone whose task panicked is
        // still named rather than counted as empty.
        let mut unsearched: Vec<RegionId> = zones.iter().map(|r| r.id.clone()).collect();
        let mut first_err: Option<ProviderError> = None;
        while let Some(joined) = tasks.join_next().await {
            let (id, outcome) = match joined {
                Ok(pair) => pair,
                Err(e) => {
                    first_err.get_or_insert(ProviderError::Other(format!("zone list task: {e}")));
                    continue;
                }
            };
            match outcome {
                Ok(items) => {
                    unsearched.retain(|zone| zone != &id);
                    instances.extend(items);
                }
                // Per-zone failures are tolerated as long as at least one
                // zone answers; a jam session lives in exactly one zone.
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        if unsearched.len() == zones.len() {
            return Err(first_err
                .unwrap_or_else(|| ProviderError::Other("gcp catalog has no zones".to_owned())));
        }
        unsearched.sort();
        Ok(Listing {
            instances,
            unsearched,
        })
    }

    fn session_port(&self) -> u16 {
        self.session_port
    }

    async fn session_ingress(&self, session: &str) -> Result<Vec<IngressRule>> {
        let tag = network_tag(session)?;
        let allow = format!("{tag}-{ALLOW_SUFFIX}");
        let mut out = Vec::new();
        for firewall in self.jamstream_firewalls().await? {
            if firewall.name != allow {
                continue;
            }
            let mut cidrs = firewall.source_ranges.clone();
            cidrs.sort();
            for protocol in &firewall.allowed {
                for ports in &protocol.ports {
                    let (from, to) = match ports.split_once('-') {
                        Some((from, to)) => {
                            (from.parse().unwrap_or(0), to.parse().unwrap_or(u16::MAX))
                        }
                        None => {
                            let port = ports.parse().unwrap_or(0);
                            (port, port)
                        }
                    };
                    out.push(IngressRule {
                        protocol: protocol.protocol.clone(),
                        from_port: from,
                        to_port: to,
                        cidrs: cidrs.clone(),
                    });
                }
            }
        }
        Ok(out)
    }

    async fn destroy_orphan_firewalls(&self) -> Result<Vec<String>> {
        // A rule targets one session's network tag, and the instances still
        // carrying that session label are what makes it live.
        let live: Vec<String> = self
            .list_tagged(None)
            .await?
            .instances
            .iter()
            .filter_map(|i| i.session_id().and_then(|s| network_tag(s).ok()))
            .collect();
        let token = self.token.access_token().await?;
        let mut deleted = Vec::new();
        for firewall in self.jamstream_firewalls().await? {
            if firewall.target_tags.iter().any(|tag| live.contains(tag)) {
                continue;
            }
            let url = format!("{}/{}", self.firewalls_url(), firewall.name);
            match send_retrying(|| self.http.delete(&url).bearer_auth(&token)).await {
                Ok(_) | Err(ProviderError::NotFound(_)) => deleted.push(firewall.name),
                Err(err) => {
                    tracing::warn!(firewall = firewall.name, error = %err, "could not delete firewall rule");
                }
            }
        }
        deleted.sort();
        Ok(deleted)
    }
}

/// The session's hard cap in seconds, from the `max_duration_min` the boot
/// config carries. None when the payload has no such key or the cap is
/// zero, which leaves [`DEFAULT_MAX_RUN_SECONDS`] in place: a GCP instance
/// with no cap at all is one nothing on the box can end.
fn session_max_run_seconds(user_data: &str) -> Option<u64> {
    crate::cloudinit::flat_config_value(user_data, "max_duration_min")
        .and_then(|minutes| minutes.parse::<u64>().ok())
        .filter(|minutes| *minutes > 0)
        .map(|minutes| minutes * 60)
}

/// Default zone per catalog region: the region id plus "-b". Zone b exists
/// in every catalog region.
fn default_zone(region: &str) -> String {
    format!("{region}-b")
}

fn machine_type(class: InstanceClass) -> &'static str {
    match class {
        InstanceClass::Small => "e2-small",
        InstanceClass::Standard => "e2-medium",
    }
}

/// Unique, GCP-compliant instance name: lowercase, starts with a letter,
/// well under the 63-character cap.
fn generate_name() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("jamstream-{nanos:x}-{seq:x}")
}

/// Encodes one canonical tag key or value as a GCP-compliant label
/// component. See the module docs for the mapping.
fn label_encode(raw: &str) -> Result<String> {
    let passthrough = raw.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && raw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !raw.starts_with(ESCAPE_PREFIX);
    let encoded = if passthrough {
        raw.to_owned()
    } else {
        format!("{ESCAPE_PREFIX}{}", HEXLOWER.encode(raw.as_bytes()))
    };
    if encoded.len() > MAX_LABEL_LEN {
        return Err(ProviderError::Other(format!(
            "tag component {raw:?} encodes to {} characters; gcp labels cap at {MAX_LABEL_LEN}",
            encoded.len()
        )));
    }
    Ok(encoded)
}

/// Network tag for a session. Tags take the same alphabet as labels
/// (`[a-z]([-a-z0-9]*[a-z0-9])?`, 63 characters), so the label encoder does
/// the work; the prefix is what makes a tag recognizably ours.
fn network_tag(session: &str) -> Result<String> {
    let encoded = label_encode(session)?;
    let tag = format!("{FIREWALL_PREFIX}{encoded}");
    if tag.len() > MAX_LABEL_LEN {
        return Err(ProviderError::Other(format!(
            "session id {session:?} makes a {}-character network tag; gcp caps at {MAX_LABEL_LEN}",
            tag.len()
        )));
    }
    Ok(tag)
}

/// Inverse of [`label_encode`]. Labels not written by JamStream (no valid
/// escape) pass through unchanged.
fn label_decode(label: &str) -> String {
    if let Some(hex) = label.strip_prefix(ESCAPE_PREFIX)
        && let Ok(bytes) = HEXLOWER.decode(hex.as_bytes())
        && let Ok(decoded) = String::from_utf8(bytes)
    {
        return decoded;
    }
    label.to_owned()
}

#[derive(Deserialize)]
struct PriceTable {
    egress_microusd_per_gb: u64,
    included_egress_gb: u32,
    /// e2-small per-region hourly rates.
    hourly_microusd: BTreeMap<String, u64>,
    /// e2-medium per-region hourly rates (approximate, about 2x e2-small).
    e2_medium_hourly_microusd: BTreeMap<String, u64>,
}

fn price_table() -> &'static PriceTable {
    static TABLE: OnceLock<PriceTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(PRICES_JSON).expect("bundled data/gcp_prices.json must parse")
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpInstance {
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    network_interfaces: Vec<GcpNetworkInterface>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpNetworkInterface {
    #[serde(default)]
    access_configs: Vec<GcpAccessConfig>,
}

#[derive(Deserialize)]
struct GcpAccessConfig {
    #[serde(default, rename = "natIP")]
    nat_ip: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpFirewall {
    name: String,
    #[serde(default)]
    allowed: Vec<GcpFirewallProtocol>,
    #[serde(default)]
    source_ranges: Vec<String>,
    #[serde(default)]
    target_tags: Vec<String>,
}

#[derive(Deserialize)]
struct GcpFirewallProtocol {
    #[serde(rename = "IPProtocol")]
    protocol: String,
    #[serde(default)]
    ports: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpFirewallList {
    #[serde(default)]
    items: Vec<GcpFirewall>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpInstanceList {
    #[serde(default)]
    items: Vec<GcpInstance>,
    #[serde(default)]
    next_page_token: Option<String>,
}

fn instance_from_gcp(raw: GcpInstance, region: Region) -> Instance {
    let public_ip = raw
        .network_interfaces
        .iter()
        .flat_map(|nic| nic.access_configs.iter())
        .find_map(|ac| {
            ac.nat_ip
                .as_deref()
                .and_then(|ip| ip.parse::<IpAddr>().ok())
        });
    let tags = raw
        .labels
        .iter()
        .filter(|(key, _)| key.as_str() != MARKER_LABEL_KEY)
        .map(|(key, value)| (label_decode(key), label_decode(value)))
        .collect();
    Instance {
        provider: ProviderKind::Gcp,
        region,
        id: raw.name,
        public_ip,
        tags,
    }
}

/// Lists one zone, following `nextPageToken` until the listing is
/// complete.
async fn list_zone(
    http: reqwest::Client,
    url: String,
    token: String,
    filter: String,
    region: Region,
) -> Result<Vec<Instance>> {
    let mut instances = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let resp = send_retrying(|| {
            let mut req = http
                .get(&url)
                .bearer_auth(&token)
                .query(&[("filter", filter.as_str())]);
            if let Some(t) = &page_token {
                req = req.query(&[("pageToken", t.as_str())]);
            }
            req
        })
        .await?;
        let list: GcpInstanceList = resp
            .json()
            .await
            .map_err(|e| ProviderError::Other(format!("gcp list response parse: {e}")))?;
        instances.extend(
            list.items
                .into_iter()
                .map(|item| instance_from_gcp(item, region.clone())),
        );
        page_token = list.next_page_token.filter(|t| !t.is_empty());
        if page_token.is_none() {
            return Ok(instances);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::session_tag;

    fn provider() -> GcpProvider {
        GcpProvider::with_access_token("test-project".to_owned(), "super-secret".to_owned())
    }

    /// The e2 machine types are x86_64, so this provider must select the
    /// x86_64 artifact.
    #[test]
    fn launches_x86_64_machines_and_says_so() {
        assert_eq!(provider().server_arch(), ServerArch::X86_64);
    }

    #[test]
    fn label_round_trip_passthrough() {
        for raw in ["jamstream-session", "deadbeefcafef00d", "a", "abc-123"] {
            let encoded = label_encode(raw).unwrap();
            assert_eq!(encoded, raw, "compliant input must pass through");
            assert_eq!(label_decode(&encoded), raw);
        }
    }

    #[test]
    fn label_round_trip_transformed() {
        // Uppercase, apostrophe, space, underscore, leading digit, empty,
        // and a literal collision with the escape prefix: all must escape
        // and decode back to the canonical string.
        for raw in ["Sean's Jam", "my_key", "9lives", "", "x--already", "Owner"] {
            let encoded = label_encode(raw).unwrap();
            assert_ne!(encoded, raw, "{raw:?} must be escaped");
            assert!(encoded.starts_with(ESCAPE_PREFIX));
            assert!(
                encoded
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_lowercase())
                    && encoded
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "encoded label {encoded:?} must be gcp-compliant"
            );
            assert_eq!(label_decode(&encoded), raw);
        }
    }

    #[test]
    fn label_encode_rejects_oversize() {
        let long = "A".repeat(40); // escapes to 3 + 80 characters
        let err = label_encode(&long).unwrap_err();
        assert!(matches!(err, ProviderError::Other(_)));
    }

    #[test]
    fn label_decode_tolerates_foreign_labels() {
        // Not produced by our encoder: invalid hex after the prefix.
        assert_eq!(label_decode("x--zz"), "x--zz");
        assert_eq!(label_decode("plain"), "plain");
    }

    #[test]
    fn price_table_covers_catalog_for_both_classes() {
        let table = price_table();
        for (id, _, _) in CATALOG {
            let small = table
                .hourly_microusd
                .get(*id)
                .copied()
                .unwrap_or_else(|| panic!("missing e2-small price for {id}"));
            let medium = table
                .e2_medium_hourly_microusd
                .get(*id)
                .copied()
                .unwrap_or_else(|| panic!("missing e2-medium price for {id}"));
            assert!(small > 0, "zero e2-small price for {id}");
            assert!(
                medium > small,
                "e2-medium must cost more than e2-small in {id}"
            );
        }
        assert_eq!(table.egress_microusd_per_gb, 120_000);
        assert_eq!(table.included_egress_gb, 0);
    }

    #[test]
    fn launch_body_defaults_and_labels() {
        let p = provider();
        let region = p.regions().into_iter().next().unwrap();
        let spec = LaunchSpec {
            region,
            instance_class: InstanceClass::Small,
            user_data: "#cloud-config\n".to_owned(),
            tags: vec![
                session_tag("deadbeef"),
                ("Owner".to_owned(), "Sean Reid".to_owned()),
            ],
        };
        let body = p.launch_body(&spec, "jamstream-test").unwrap();
        assert_eq!(
            body["machineType"],
            "zones/us-central1-b/machineTypes/e2-small"
        );
        // No cap in this payload, so the 12 h default stands.
        assert_eq!(body["scheduling"]["maxRunDuration"]["seconds"], "43200");
        assert_eq!(body["scheduling"]["instanceTerminationAction"], "DELETE");
        assert_eq!(body["labels"]["jamstream-session"], "deadbeef");
        assert_eq!(body["labels"]["jamstream"], "true");
        // "Owner" -> hex(4f 77 6e 65 72); "Sean Reid" escapes too.
        assert_eq!(body["labels"]["x--4f776e6572"], "x--5365616e2052656964");
        assert_eq!(body["metadata"]["items"][0]["key"], "user-data");
        assert_eq!(body["metadata"]["items"][0]["value"], "#cloud-config\n");
    }

    /// #51: `--max-hours` reached the launch body nowhere, so a session
    /// asked to live one hour was capped at twelve, and one asked for
    /// twenty-four was deleted mid-jam.
    #[test]
    fn the_run_cap_is_the_session_cap() {
        let p = provider();
        let region = p.regions().into_iter().next().unwrap();
        let boot = |max_hours: u32| crate::cloudinit::BootConfig {
            artifact_url: "https://example.invalid/jamstreamd".to_owned(),
            artifact_sha256: "0".repeat(64),
            server_private_key_b64: "c2s=".to_owned(),
            issuer_public_key_b64: "aXA=".to_owned(),
            session_id_hex: "deadbeef".to_owned(),
            port: 43210,
            idle_shutdown_min: 10,
            max_duration_min: max_hours * 60,
            self_destruct: crate::cloudinit::SelfDestruct::GcpMaxRunDuration,
            recording: None,
        };
        let spec = |user_data: String| LaunchSpec {
            region: region.clone(),
            instance_class: InstanceClass::Small,
            user_data,
            tags: vec![session_tag("deadbeef")],
        };
        let seconds = |p: &GcpProvider, user_data: String| {
            p.launch_body(&spec(user_data), "jamstream-test").unwrap()["scheduling"]
                ["maxRunDuration"]["seconds"]
                .as_str()
                .expect("maxRunDuration is a string of seconds")
                .to_owned()
        };

        // The cap the host asked for, read out of the rendered cloud-init
        // the VM boots from, which is the only place it travels.
        assert_eq!(
            seconds(&p, crate::cloudinit::render(&boot(1))),
            (3600).to_string()
        );
        assert_eq!(
            seconds(&p, crate::cloudinit::render(&boot(24))),
            (24 * 3600).to_string()
        );
        // And out of the flat config, which is the same key undecorated.
        assert_eq!(
            seconds(&p, boot(6).render_flat_config()),
            (6 * 3600).to_string()
        );
        // A payload with no cap, or a nonsense one, keeps the default
        // rather than launching an instance nothing can end.
        assert_eq!(seconds(&p, "#cloud-config\n".to_owned()), "43200");
        assert_eq!(seconds(&p, "max_duration_min = 0\n".to_owned()), "43200");
        assert_eq!(seconds(&p, "max_duration_min = soon\n".to_owned()), "43200");
        // An explicit override still outranks everything.
        let pinned = GcpProvider::with_access_token("p".to_owned(), "t".to_owned())
            .with_max_run_seconds(7_200);
        assert_eq!(
            seconds(&pinned, crate::cloudinit::render(&boot(24))),
            "7200"
        );
    }

    /// A session VM parses unauthenticated UDP and has no API call of its
    /// own to make, so it gets no service account. The API attaches nothing
    /// when the field is absent, but absence is not a decision anyone can
    /// read.
    #[test]
    fn no_service_account_is_attached() {
        let p = provider();
        let region = p.regions().into_iter().next().unwrap();
        let spec = LaunchSpec {
            region,
            instance_class: InstanceClass::Small,
            user_data: "#cloud-config\n".to_owned(),
            tags: vec![session_tag("deadbeef")],
        };
        let body = p.launch_body(&spec, "jamstream-test").unwrap();
        assert_eq!(
            body["serviceAccounts"],
            serde_json::json!([]),
            "a session VM must carry no credential"
        );
    }

    /// Path of the committed throwaway service-account key fixture.
    fn fixture_path() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/gcp_test_key.json").to_owned()
    }

    #[test]
    fn from_env_with_token_pair() {
        let get = |key: &str| match key {
            "GOOGLE_CLOUD_PROJECT" => Some("proj".to_owned()),
            "GCP_ACCESS_TOKEN" => Some("tok".to_owned()),
            _ => None,
        };
        let p = GcpProvider::from_env_with(&get).unwrap();
        assert_eq!(p.project_id, "proj");
    }

    #[test]
    fn from_env_service_account_key_builds_native_source() {
        let get = |key: &str| (key == "GOOGLE_APPLICATION_CREDENTIALS").then(fixture_path);
        let p = GcpProvider::from_env_with(&get).unwrap();
        // Project id falls back to the key's own project_id field.
        assert_eq!(p.project_id, "jamstream-test-project");
    }

    #[test]
    fn from_env_project_env_overrides_key_project_id() {
        let get = |key: &str| match key {
            "GOOGLE_APPLICATION_CREDENTIALS" => Some(fixture_path()),
            "GOOGLE_CLOUD_PROJECT" => Some("explicit-project".to_owned()),
            _ => None,
        };
        let p = GcpProvider::from_env_with(&get).unwrap();
        assert_eq!(p.project_id, "explicit-project");
    }

    #[test]
    fn from_env_token_pair_takes_precedence_over_key_file() {
        let get = |key: &str| match key {
            "GOOGLE_APPLICATION_CREDENTIALS" => Some(fixture_path()),
            "GOOGLE_CLOUD_PROJECT" => Some("proj".to_owned()),
            "GCP_ACCESS_TOKEN" => Some("tok".to_owned()),
            _ => None,
        };
        let p = GcpProvider::from_env_with(&get).unwrap();
        assert_eq!(p.project_id, "proj");
    }

    #[test]
    fn from_env_unreadable_key_file_is_auth_error() {
        let get = |key: &str| {
            (key == "GOOGLE_APPLICATION_CREDENTIALS").then(|| "/nonexistent/key.json".to_owned())
        };
        let err = GcpProvider::from_env_with(&get).unwrap_err();
        match err {
            ProviderError::Auth(msg) => assert!(msg.contains("/nonexistent/key.json")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn from_env_empty_explains_modes() {
        let err = GcpProvider::from_env_with(&|_| None).unwrap_err();
        match err {
            ProviderError::Auth(msg) => {
                assert!(msg.contains("GOOGLE_APPLICATION_CREDENTIALS"));
                assert!(msg.contains("GCP_ACCESS_TOKEN"));
                assert!(msg.contains("with_access_token"));
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn debug_never_reveals_token() {
        let p = provider();
        let rendered = format!("{p:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn default_zone_appends_b() {
        assert_eq!(default_zone("us-central1"), "us-central1-b");
    }

    #[test]
    fn generated_names_are_unique_and_compliant() {
        let a = generate_name();
        let b = generate_name();
        assert_ne!(a, b);
        for name in [&a, &b] {
            assert!(name.starts_with("jamstream-"));
            assert!(name.len() <= 63);
            assert!(
                name.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            );
        }
    }
}
