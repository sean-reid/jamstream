//! GCP Compute Engine provider. Launches one short-lived jam-session VM
//! per launch call, relies on `scheduling.maxRunDuration` with
//! `instanceTerminationAction=DELETE` as the provider-enforced hard cap
//! (see [`crate::cloudinit::SelfDestruct::GcpMaxRunDuration`]), labels
//! everything it creates, lists by label, and destroys by name.
//!
//! # Authentication
//!
//! GCP service-account keys require an RS256-signed JWT to mint an OAuth2
//! access token. This crate has no RSA implementation and adds no
//! dependencies, so native RS256 signing of service account keys is a
//! tracked follow-up. Until then the token source is pluggable and two
//! modes are supported:
//!
//! 1. [`GcpProvider::with_access_token`]: the caller supplies an OAuth2
//!    bearer token (the CLI shells out to `gcloud auth
//!    print-access-token`; tests inject a fake). The token is opaque,
//!    never logged, and redacted from `Debug` output.
//! 2. [`GcpProvider::from_env`]: reads `GOOGLE_CLOUD_PROJECT` plus
//!    `GCP_ACCESS_TOKEN`. If those are absent it fails with an Auth error
//!    that explains the supported modes; when
//!    `GOOGLE_APPLICATION_CREDENTIALS` is set and `gcloud` is on PATH the
//!    error notes that the CLI integration shells out to gcloud (the
//!    subprocess lives in the CLI, not in this library).
//!
//! Arbitrary refresh strategies plug in through [`TokenSource`].
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

use crate::http::{client, send_retrying};
use crate::provider::{Provider, ProviderError, Result};
use crate::types::{
    Instance, InstanceClass, LaunchSpec, Price, ProviderKind, Region, RegionId, SESSION_TAG_KEY,
};

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
/// Provider-enforced hard cap default: 12 hours.
const DEFAULT_MAX_RUN_SECONDS: u64 = 12 * 60 * 60;
/// Debian 12 has cloud-init and reads the `user-data` metadata key.
const SOURCE_IMAGE: &str = "projects/debian-cloud/global/images/family/debian-12";
/// Marker label added to every launch so `list_tagged(None)` can filter
/// server-side. The tag key `jamstream` is reserved for this marker.
const MARKER_LABEL_KEY: &str = "jamstream";
/// Reserved prefix introducing a hex-escaped label component.
const ESCAPE_PREFIX: &str = "x--";
const MAX_LABEL_LEN: usize = 63;

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
            http: client(),
        }
    }

    /// Auth mode 2: `GOOGLE_CLOUD_PROJECT` plus `GCP_ACCESS_TOKEN` from
    /// the environment. Anything else fails with an Auth error that spells
    /// out the supported modes; see the module docs.
    pub fn from_env() -> Result<Self> {
        Self::from_env_with(&|key| std::env::var(key).ok(), gcloud_on_path())
    }

    fn from_env_with(get: &dyn Fn(&str) -> Option<String>, gcloud_available: bool) -> Result<Self> {
        if let (Some(project), Some(token)) = (get("GOOGLE_CLOUD_PROJECT"), get("GCP_ACCESS_TOKEN"))
        {
            return Ok(Self::with_access_token(project, token));
        }
        if get("GOOGLE_APPLICATION_CREDENTIALS").is_some() && gcloud_available {
            return Err(ProviderError::Auth(
                "GOOGLE_APPLICATION_CREDENTIALS points at a service account key and gcloud is on \
                 PATH; the JamStream CLI integration shells out to `gcloud auth \
                 print-access-token` for this case (this library does not spawn subprocesses). \
                 Supported modes: GcpProvider::with_access_token(project_id, token), or set \
                 GOOGLE_CLOUD_PROJECT and GCP_ACCESS_TOKEN. Native RS256 signing of service \
                 account keys is a tracked follow-up."
                    .to_owned(),
            ));
        }
        Err(ProviderError::Auth(
            "no GCP credentials: set GOOGLE_CLOUD_PROJECT and GCP_ACCESS_TOKEN (for example from \
             `gcloud auth print-access-token`), or construct the provider with \
             GcpProvider::with_access_token(project_id, token). Native RS256 signing of service \
             account keys is a tracked follow-up."
                .to_owned(),
        ))
    }

    /// Overrides the API endpoint (tests point this at a mock server).
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url.trim_end_matches('/').to_owned();
        self
    }

    /// Overrides the `scheduling.maxRunDuration` hard cap (default 12h).
    /// GCP deletes the instance when the cap elapses
    /// (`instanceTerminationAction=DELETE`).
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
        let seconds = self.max_run_seconds.unwrap_or(DEFAULT_MAX_RUN_SECONDS);
        Ok(json!({
            "name": name,
            "machineType": format!("zones/{zone}/machineTypes/{}", machine_type(spec.instance_class)),
            "disks": [{
                "boot": true,
                "autoDelete": true,
                "initializeParams": { "sourceImage": SOURCE_IMAGE },
            }],
            "networkInterfaces": [{
                // Ephemeral public IP.
                "accessConfigs": [{ "type": "ONE_TO_ONE_NAT", "name": "External NAT" }],
            }],
            // cloud-init on Debian images reads the "user-data" metadata key.
            "metadata": { "items": [{ "key": "user-data", "value": spec.user_data }] },
            "labels": labels,
            "scheduling": {
                // Duration fields serialize int64 seconds as a JSON string.
                "maxRunDuration": { "seconds": seconds.to_string() },
                "instanceTerminationAction": "DELETE",
            },
        }))
    }
}

#[async_trait]
impl Provider for GcpProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Gcp
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

    async fn list_tagged(&self, session_tag: Option<&str>) -> Result<Vec<Instance>> {
        let filter = match session_tag {
            Some(session) => format!(
                "labels.{}={}",
                label_encode(SESSION_TAG_KEY)?,
                label_encode(session)?
            ),
            None => format!("labels.{MARKER_LABEL_KEY}=true"),
        };
        let token = self.token.access_token().await?;
        let mut tasks = JoinSet::new();
        for region in self.regions() {
            let http = self.http.clone();
            let url = self.zone_url(&region.id);
            let token = token.clone();
            let filter = filter.clone();
            tasks.spawn(async move { list_zone(http, url, token, filter, region).await });
        }
        let mut instances = Vec::new();
        let mut any_zone_succeeded = false;
        let mut first_err: Option<ProviderError> = None;
        while let Some(joined) = tasks.join_next().await {
            let outcome = joined
                .unwrap_or_else(|e| Err(ProviderError::Other(format!("zone list task: {e}"))));
            match outcome {
                Ok(items) => {
                    any_zone_succeeded = true;
                    instances.extend(items);
                }
                // Per-zone failures are tolerated as long as at least one
                // zone answers; a jam session lives in exactly one zone.
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if any_zone_succeeded {
            Ok(instances)
        } else {
            Err(first_err
                .unwrap_or_else(|| ProviderError::Other("gcp catalog has no zones".to_owned())))
        }
    }
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

fn gcloud_on_path() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join("gcloud").is_file()))
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
        assert_eq!(body["scheduling"]["maxRunDuration"]["seconds"], "43200");
        assert_eq!(body["scheduling"]["instanceTerminationAction"], "DELETE");
        assert_eq!(body["labels"]["jamstream-session"], "deadbeef");
        assert_eq!(body["labels"]["jamstream"], "true");
        // "Owner" -> hex(4f 77 6e 65 72); "Sean Reid" escapes too.
        assert_eq!(body["labels"]["x--4f776e6572"], "x--5365616e2052656964");
        assert_eq!(body["metadata"]["items"][0]["key"], "user-data");
        assert_eq!(body["metadata"]["items"][0]["value"], "#cloud-config\n");
    }

    #[test]
    fn from_env_with_token_pair() {
        let get = |key: &str| match key {
            "GOOGLE_CLOUD_PROJECT" => Some("proj".to_owned()),
            "GCP_ACCESS_TOKEN" => Some("tok".to_owned()),
            _ => None,
        };
        let p = GcpProvider::from_env_with(&get, false).unwrap();
        assert_eq!(p.project_id, "proj");
    }

    #[test]
    fn from_env_service_account_key_mentions_gcloud() {
        let get = |key: &str| {
            (key == "GOOGLE_APPLICATION_CREDENTIALS").then(|| "/tmp/key.json".to_owned())
        };
        let err = GcpProvider::from_env_with(&get, true).unwrap_err();
        match err {
            ProviderError::Auth(msg) => {
                assert!(msg.contains("gcloud auth print-access-token"));
                assert!(msg.contains("with_access_token"));
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn from_env_empty_explains_modes() {
        let err = GcpProvider::from_env_with(&|_| None, false).unwrap_err();
        match err {
            ProviderError::Auth(msg) => {
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
