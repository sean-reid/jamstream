//! DigitalOcean provider: one short-lived jam-session droplet per session.
//! Droplets bill while powered off, so destroy is the only end state; the
//! droplet itself self-destructs through the API (see
//! `cloudinit::SelfDestruct::ApiToken`), and the sweeper catches leaks by
//! tag.
//!
//! # Tag mapping
//!
//! DigitalOcean tags are flat strings, not key=value pairs, and allow only
//! letters, numbers, colons, dashes, and underscores ('=' is rejected). The
//! canonical JamStream tag pair `(key, value)` therefore maps to the flat
//! DO tag `key:value` (for example `(SESSION_TAG_KEY, "abc123")` becomes
//! `jamstream-session:abc123`); a pair with an empty value maps to the bare
//! `key`. Any character outside the DO alphabet is replaced with `-` on
//! encode, so the mapping is lossy only for ids that were never DO-safe.
//! Decoding splits on the first `:` (bare tags decode to an empty value),
//! which reconstructs the canonical pair exactly, so
//! `Instance::session_id`/`session_id_from_tags` work unchanged. Every
//! droplet additionally carries the bare marker tag `jamstream` so
//! `list_tagged(None)` finds all JamStream droplets with one query.

use std::fmt;
use std::net::IpAddr;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::OnceCell;

use crate::http::{client, error_body, send_retrying};
use crate::provider::{Provider, ProviderError, Result};
use crate::types::{
    ANY_IPV4, ANY_IPV6, DEFAULT_SESSION_PORT, IngressRule, Instance, InstanceClass, LaunchSpec,
    Price, ProviderKind, Region, RegionId, SESSION_TAG_KEY,
};

const DEFAULT_BASE_URL: &str = "https://api.digitalocean.com";

/// Bare marker tag carried by every JamStream droplet.
pub const BARE_TAG: &str = "jamstream";

/// Name prefix of every firewall JamStream creates, so orphan cleanup can
/// recognize its own work without touching the account's other firewalls.
pub const FIREWALL_PREFIX: &str = "jamstream-";

/// Debian is the boot image everywhere; cloud-init does the rest.
const IMAGE: &str = "debian-12-x64";

/// Egress beyond the included pool costs $0.01/GB on DigitalOcean.
const EGRESS_MICROUSD_PER_GB: u64 = 10_000;

/// Static region catalog: (slug, display, country).
const CATALOG: &[(&str, &str, &str)] = &[
    ("nyc1", "New York 1", "US"),
    ("nyc3", "New York 3", "US"),
    ("atl1", "Atlanta 1", "US"),
    ("sfo3", "San Francisco 3", "US"),
    ("tor1", "Toronto 1", "CA"),
    ("lon1", "London 1", "GB"),
    ("ams3", "Amsterdam 3", "NL"),
    ("fra1", "Frankfurt 1", "DE"),
    ("syd1", "Sydney 1", "AU"),
    ("sgp1", "Singapore 1", "SG"),
    ("blr1", "Bangalore 1", "IN"),
];

/// Maps the abstract instance class to a concrete droplet size slug.
pub fn size_slug(class: InstanceClass) -> &'static str {
    match class {
        InstanceClass::Small => "s-1vcpu-2gb",
        InstanceClass::Standard => "s-2vcpu-2gb",
    }
}

/// Included pooled transfer per size, in GB. DigitalOcean pools the monthly
/// allowance (2 TB for s-1vcpu-2gb, 3 TB for s-2vcpu-2gb) across the account
/// and prorates it by droplet lifetime, so a short-lived session droplet
/// realistically accrues only a sliver of this. The catalog still reports
/// the full per-size figure; a jam session never gets near either number,
/// so the cost preview treats overage as effectively unreachable.
fn included_egress_gb(class: InstanceClass) -> u32 {
    match class {
        InstanceClass::Small => 2000,
        InstanceClass::Standard => 3000,
    }
}

/// True for the characters DigitalOcean accepts in a tag.
fn do_tag_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == ':' || c == '-' || c == '_'
}

fn sanitize_tag_part(part: &str) -> String {
    part.chars()
        .map(|c| if do_tag_char(c) { c } else { '-' })
        .collect()
}

/// Encodes a canonical `(key, value)` tag pair as a flat DO tag string.
/// See the module docs for the mapping rules.
pub fn to_do_tag(key: &str, value: &str) -> String {
    if value.is_empty() {
        sanitize_tag_part(key)
    } else {
        format!("{}:{}", sanitize_tag_part(key), sanitize_tag_part(value))
    }
}

/// Decodes a flat DO tag string back into the canonical `(key, value)`
/// pair. Splits on the first `:`; bare tags get an empty value.
pub fn from_do_tag(tag: &str) -> (String, String) {
    match tag.split_once(':') {
        Some((k, v)) => (k.to_owned(), v.to_owned()),
        None => (tag.to_owned(), String::new()),
    }
}

/// The flat DO tag for a session id: `jamstream-session:<id>`.
pub fn session_do_tag(session_id: &str) -> String {
    to_do_tag(SESSION_TAG_KEY, session_id)
}

/// Rounds an hourly USD price (as reported by the sizes API) to integer
/// microdollars, half away from zero, i.e. round-half-up for prices.
fn microusd_from_hourly(price_hourly: f64) -> u64 {
    (price_hourly * 1_000_000.0).round() as u64
}

#[derive(Debug, Clone, Deserialize)]
struct SizeInfo {
    slug: String,
    price_hourly: f64,
    #[serde(default)]
    regions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SizesPage {
    sizes: Vec<SizeInfo>,
    #[serde(default)]
    links: Option<Links>,
}

#[derive(Debug, Deserialize)]
struct Links {
    #[serde(default)]
    pages: Option<Pages>,
}

#[derive(Debug, Deserialize)]
struct Pages {
    #[serde(default)]
    next: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DropletEnvelope {
    droplet: Droplet,
}

#[derive(Debug, Deserialize)]
struct FirewallEnvelope {
    firewall: Firewall,
}

#[derive(Debug, Deserialize)]
struct FirewallsPage {
    firewalls: Vec<Firewall>,
    #[serde(default)]
    links: Option<Links>,
}

#[derive(Debug, Deserialize)]
struct Firewall {
    id: String,
    name: String,
    #[serde(default)]
    inbound_rules: Vec<FirewallRule>,
    #[serde(default)]
    droplet_ids: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct FirewallRule {
    protocol: String,
    /// A single port, a "from-to" range, or "0" for every port.
    #[serde(default)]
    ports: String,
    #[serde(default)]
    sources: FirewallSources,
}

#[derive(Debug, Default, Deserialize)]
struct FirewallSources {
    #[serde(default)]
    addresses: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DropletsPage {
    droplets: Vec<Droplet>,
    #[serde(default)]
    links: Option<Links>,
}

#[derive(Debug, Deserialize)]
struct Droplet {
    id: u64,
    region: DropletRegion,
    #[serde(default)]
    networks: Networks,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DropletRegion {
    slug: String,
}

#[derive(Debug, Default, Deserialize)]
struct Networks {
    #[serde(default)]
    v4: Vec<NetworkV4>,
}

#[derive(Debug, Deserialize)]
struct NetworkV4 {
    ip_address: String,
    #[serde(rename = "type")]
    kind: String,
}

pub struct DigitalOceanProvider {
    token: String,
    base_url: String,
    /// The one port the per-session cloud firewall opens.
    session_port: u16,
    http: reqwest::Client,
    /// GET /v2/sizes is region-independent and immutable for our purposes;
    /// fetched once per provider instance and reused by every price() call.
    sizes: OnceCell<Vec<SizeInfo>>,
}

/// The token is a live credential; it never appears in Debug output.
impl fmt::Debug for DigitalOceanProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DigitalOceanProvider")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl DigitalOceanProvider {
    pub fn new(token: String) -> Self {
        DigitalOceanProvider {
            token,
            base_url: DEFAULT_BASE_URL.to_owned(),
            session_port: DEFAULT_SESSION_PORT,
            http: client(),
            sizes: OnceCell::new(),
        }
    }

    /// Session UDP port the cloud firewall opens; see
    /// [`Provider::session_port`].
    pub fn with_session_port(mut self, port: u16) -> Self {
        self.session_port = port;
        self
    }

    /// Reads the API token from `DIGITALOCEAN_TOKEN`.
    pub fn from_env() -> Result<Self> {
        match std::env::var("DIGITALOCEAN_TOKEN") {
            Ok(token) if !token.is_empty() => Ok(Self::new(token)),
            _ => Err(ProviderError::Auth(
                "DIGITALOCEAN_TOKEN is not set".to_owned(),
            )),
        }
    }

    /// Overrides the API base URL (tests point this at a mock server).
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url.trim_end_matches('/').to_owned();
        self
    }

    fn region_by_slug(&self, slug: &str) -> Region {
        CATALOG
            .iter()
            .find(|(s, _, _)| *s == slug)
            .map(|(s, display, country)| Region {
                provider: ProviderKind::DigitalOcean,
                id: RegionId::new(*s),
                display: (*display).to_owned(),
                country: (*country).to_owned(),
            })
            .unwrap_or_else(|| Region {
                provider: ProviderKind::DigitalOcean,
                id: RegionId::new(slug),
                display: slug.to_owned(),
                country: String::new(),
            })
    }

    fn known_region(&self, id: &RegionId) -> Result<()> {
        if CATALOG.iter().any(|(s, _, _)| *s == id.as_str()) {
            Ok(())
        } else {
            Err(ProviderError::NotFound(format!("digitalocean region {id}")))
        }
    }

    fn instance_from(&self, d: &Droplet) -> Instance {
        let public_ip = d
            .networks
            .v4
            .iter()
            .find(|n| n.kind == "public")
            .and_then(|n| n.ip_address.parse::<IpAddr>().ok());
        Instance {
            provider: ProviderKind::DigitalOcean,
            region: self.region_by_slug(&d.region.slug),
            id: d.id.to_string(),
            public_ip,
            tags: d.tags.iter().map(|t| from_do_tag(t)).collect(),
        }
    }

    async fn parse_json<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
        resp.json::<T>()
            .await
            .map_err(|e| ProviderError::Other(format!("digitalocean response decode: {e}")))
    }

    /// Fetches `first_url` and every `links.pages.next` after it, decoding
    /// each page with `page`.
    async fn get_paginated<T, P>(&self, first_url: String, page: P) -> Result<Vec<T>>
    where
        P: Fn(serde_json::Value) -> Result<(Vec<T>, Option<String>)>,
    {
        let mut out = Vec::new();
        let mut next = Some(first_url);
        while let Some(url) = next {
            let resp = send_retrying(|| self.http.get(&url).bearer_auth(&self.token)).await?;
            let value: serde_json::Value = Self::parse_json(resp).await?;
            let (items, link) = page(value)?;
            out.extend(items);
            next = link;
        }
        Ok(out)
    }

    async fn sizes(&self) -> Result<&[SizeInfo]> {
        self.sizes
            .get_or_try_init(|| async {
                self.get_paginated(
                    format!("{}/v2/sizes?per_page=200", self.base_url),
                    |value| {
                        let page: SizesPage = serde_json::from_value(value).map_err(|e| {
                            ProviderError::Other(format!("digitalocean sizes decode: {e}"))
                        })?;
                        let next = page.links.and_then(|l| l.pages).and_then(|p| p.next);
                        Ok((page.sizes, next))
                    },
                )
                .await
            })
            .await
            .map(Vec::as_slice)
    }

    /// Live price for one instance class in one region, from the cached
    /// sizes catalog.
    pub async fn price_for(&self, region: &RegionId, class: InstanceClass) -> Result<Price> {
        self.known_region(region)?;
        let slug = size_slug(class);
        let sizes = self.sizes().await?;
        let size = sizes.iter().find(|s| s.slug == slug).ok_or_else(|| {
            ProviderError::NotFound(format!("digitalocean size {slug} not in catalog"))
        })?;
        if !size.regions.iter().any(|r| r == region.as_str()) {
            return Err(ProviderError::NotFound(format!(
                "digitalocean size {slug} unavailable in region {region}"
            )));
        }
        Ok(Price {
            hourly_microusd: microusd_from_hourly(size.price_hourly),
            egress_microusd_per_gb: EGRESS_MICROUSD_PER_GB,
            included_egress_gb: included_egress_gb(class),
        })
    }

    /// Refreshes a droplet: DO assigns the public IPv4 asynchronously after
    /// create, so poll this until `public_ip` is `Some`. The region argument
    /// is advisory; droplet ids are account-global on DigitalOcean.
    pub async fn refresh(&self, _region: &RegionId, id: &str) -> Result<Instance> {
        let url = format!("{}/v2/droplets/{id}", self.base_url);
        let resp = send_retrying(|| self.http.get(&url).bearer_auth(&self.token)).await?;
        let envelope: DropletEnvelope = Self::parse_json(resp).await?;
        Ok(self.instance_from(&envelope.droplet))
    }

    /// Firewall name for a session. DigitalOcean accepts only
    /// `^[a-zA-Z0-9][a-zA-Z0-9.-]+$` here, which is narrower than the tag
    /// alphabet: no colons and no underscores.
    fn firewall_name(session_id: &str) -> String {
        let tail: String = session_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        format!("{FIREWALL_PREFIX}{tail}")
    }

    /// Creates the tag a firewall can be attached to. A firewall may only
    /// reference tags that already exist, and a droplet create is what
    /// would otherwise create this one, which is too late: the firewall has
    /// to be in place before the droplet boots.
    async fn ensure_tag(&self, tag: &str) -> Result<()> {
        let url = format!("{}/v2/tags", self.base_url);
        let body = json!({ "name": tag });
        match send_retrying(|| self.http.post(&url).bearer_auth(&self.token).json(&body)).await {
            Ok(_) => Ok(()),
            // 422 is "already exists", which is the relaunch case.
            Err(ProviderError::Other(msg)) if msg.contains("http 422") => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Creates the session's cloud firewall: inbound only the session UDP
    /// port, outbound everything, attached to the session tag so the
    /// droplet is covered from the moment it exists.
    ///
    /// Without this a droplet is reachable on every port for its whole life,
    /// which is what made DigitalOcean the one provider a session could
    /// reach and also the one with nothing in front of sshd.
    async fn ensure_firewall(&self, session_id: &str) -> Result<String> {
        let tag = session_do_tag(session_id);
        self.ensure_tag(&tag).await?;
        let anywhere = json!([ANY_IPV4, ANY_IPV6]);
        // A firewall with no outbound rules blocks all egress, and the VM
        // has to fetch jamstreamd and delete itself through the API.
        let outbound: Vec<serde_json::Value> = ["tcp", "udp", "icmp"]
            .iter()
            .map(|protocol| {
                json!({
                    "protocol": protocol,
                    "ports": "0",
                    "destinations": { "addresses": anywhere },
                })
            })
            .collect();
        let body = json!({
            "name": Self::firewall_name(session_id),
            "inbound_rules": [{
                "protocol": "udp",
                "ports": self.session_port.to_string(),
                "sources": { "addresses": anywhere },
            }],
            "outbound_rules": outbound,
            "tags": [tag],
        });
        let url = format!("{}/v2/firewalls", self.base_url);
        match send_retrying(|| self.http.post(&url).bearer_auth(&self.token).json(&body)).await {
            Ok(resp) => {
                let envelope: FirewallEnvelope = Self::parse_json(resp).await?;
                Ok(envelope.firewall.id)
            }
            // A duplicate name means a previous attempt at this same session
            // already created it.
            Err(ProviderError::Other(msg)) if msg.contains("http 422") => {
                let name = Self::firewall_name(session_id);
                self.firewalls()
                    .await?
                    .into_iter()
                    .find(|f| f.name == name)
                    .map(|f| f.id)
                    .ok_or(ProviderError::Other(msg))
            }
            Err(err) => Err(err),
        }
    }

    /// Every firewall on the account, following pagination.
    async fn firewalls(&self) -> Result<Vec<Firewall>> {
        self.get_paginated(
            format!("{}/v2/firewalls?per_page=200", self.base_url),
            |value| {
                let page: FirewallsPage = serde_json::from_value(value).map_err(|e| {
                    ProviderError::Other(format!("digitalocean firewalls decode: {e}"))
                })?;
                let next = page.links.and_then(|l| l.pages).and_then(|p| p.next);
                Ok((page.firewalls, next))
            },
        )
        .await
    }

    /// Bulk destroy by tag: `Some(session_id)` destroys that session's
    /// droplets, `None` destroys every JamStream-tagged droplet. DO returns
    /// 204 whether or not the tag matched anything, so this is idempotent.
    pub async fn destroy_by_tag(&self, session_id: Option<&str>) -> Result<()> {
        let tag = match session_id {
            Some(id) => session_do_tag(id),
            None => BARE_TAG.to_owned(),
        };
        let url = format!("{}/v2/droplets", self.base_url);
        send_retrying(|| {
            self.http
                .delete(&url)
                .query(&[("tag_name", tag.as_str())])
                .bearer_auth(&self.token)
        })
        .await?;
        Ok(())
    }
}

#[async_trait]
impl Provider for DigitalOceanProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::DigitalOcean
    }

    fn regions(&self) -> Vec<Region> {
        CATALOG
            .iter()
            .map(|(slug, _, _)| self.region_by_slug(slug))
            .collect()
    }

    /// Region price for the Standard session size; use `price_for` to price
    /// a specific class.
    async fn price(&self, region: &RegionId) -> Result<Price> {
        self.price_for(region, InstanceClass::Standard).await
    }

    async fn launch(&self, spec: LaunchSpec) -> Result<Instance> {
        // DO answers an unknown region or size with a bare 422; validating
        // against the static catalog up front gives the caller a proper
        // NotFound instead.
        self.known_region(&spec.region.id)?;
        let Some(session) = spec.session_id().map(str::to_owned) else {
            return Err(ProviderError::Other(format!(
                "launch spec has no {SESSION_TAG_KEY} tag; refusing to create a droplet the sweeper cannot find"
            )));
        };
        // The firewall is attached to the session tag and created first, so
        // the droplet is behind it from the moment DigitalOcean creates it.
        self.ensure_firewall(&session).await?;
        let suffix = session.clone();
        let name: String = format!("jamstream-{suffix}")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let mut tags = vec![BARE_TAG.to_owned()];
        tags.extend(spec.tags.iter().map(|(k, v)| to_do_tag(k, v)));
        let body = json!({
            "name": name,
            "region": spec.region.id.as_str(),
            "size": size_slug(spec.instance_class),
            "image": IMAGE,
            "user_data": spec.user_data,
            "tags": tags,
        });
        let url = format!("{}/v2/droplets", self.base_url);
        let resp = send_retrying(|| self.http.post(&url).bearer_auth(&self.token).json(&body))
            .await
            .map_err(|e| match &e {
                // DO explains a rejected create in the 422 body's JSON
                // "message" field, which the shared http layer now carries
                // on the error; surface it verbatim.
                ProviderError::Other(msg) if msg.contains("http 422") => {
                    let detail = error_body(&e)
                        .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok())
                        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned));
                    ProviderError::Other(match detail {
                        Some(message) => {
                            format!("digitalocean rejected droplet create: {message}")
                        }
                        None => format!(
                            "digitalocean rejected droplet create (invalid region, size, or image): {msg}"
                        ),
                    })
                }
                _ => e,
            })?;
        let envelope: DropletEnvelope = Self::parse_json(resp).await?;
        // The create response carries no public IP; DO assigns it
        // asynchronously. Callers poll refresh() until it shows up.
        Ok(self.instance_from(&envelope.droplet))
    }

    async fn destroy(&self, _region: &RegionId, id: &str) -> Result<()> {
        // A missing droplet 404s and the shared classifier maps that to
        // NotFound, which double-destroy relies on.
        let url = format!("{}/v2/droplets/{id}", self.base_url);
        send_retrying(|| self.http.delete(&url).bearer_auth(&self.token)).await?;
        Ok(())
    }

    async fn list_tagged(&self, session_tag: Option<&str>) -> Result<Vec<Instance>> {
        let tag = match session_tag {
            Some(id) => session_do_tag(id),
            None => BARE_TAG.to_owned(),
        };
        let first = format!(
            "{}/v2/droplets?tag_name={}&per_page=200",
            self.base_url, tag
        );
        let droplets = self
            .get_paginated(first, |value| {
                let page: DropletsPage = serde_json::from_value(value).map_err(|e| {
                    ProviderError::Other(format!("digitalocean droplets decode: {e}"))
                })?;
                let next = page.links.and_then(|l| l.pages).and_then(|p| p.next);
                Ok((page.droplets, next))
            })
            .await?;
        Ok(droplets.iter().map(|d| self.instance_from(d)).collect())
    }

    fn session_port(&self) -> u16 {
        self.session_port
    }

    async fn session_ingress(&self, session: &str) -> Result<Vec<IngressRule>> {
        let name = Self::firewall_name(session);
        let Some(firewall) = self.firewalls().await?.into_iter().find(|f| f.name == name) else {
            return Ok(Vec::new());
        };
        Ok(firewall
            .inbound_rules
            .iter()
            .map(|rule| {
                let (from, to) = parse_do_ports(&rule.ports);
                let mut cidrs = rule.sources.addresses.clone();
                cidrs.sort();
                IngressRule {
                    protocol: rule.protocol.clone(),
                    from_port: from,
                    to_port: to,
                    cidrs,
                }
            })
            .collect())
    }

    async fn destroy_orphan_firewalls(&self) -> Result<Vec<String>> {
        let mut deleted = Vec::new();
        for firewall in self.firewalls().await? {
            // A firewall with droplets behind it belongs to a live session.
            if !firewall.name.starts_with(FIREWALL_PREFIX) || !firewall.droplet_ids.is_empty() {
                continue;
            }
            let url = format!("{}/v2/firewalls/{}", self.base_url, firewall.id);
            match send_retrying(|| self.http.delete(&url).bearer_auth(&self.token)).await {
                Ok(_) | Err(ProviderError::NotFound(_)) => deleted.push(firewall.name),
                Err(err) => {
                    tracing::warn!(firewall = firewall.name, error = %err, "could not delete firewall");
                }
            }
        }
        deleted.sort();
        Ok(deleted)
    }
}

/// DigitalOcean reports a rule's ports as a single port, a "from-to" range,
/// or "0" meaning every port.
fn parse_do_ports(ports: &str) -> (u16, u16) {
    match ports.split_once('-') {
        Some((from, to)) => (from.parse().unwrap_or(0), to.parse().unwrap_or(u16::MAX)),
        None => match ports.parse::<u16>() {
            Ok(0) | Err(_) => (0, u16::MAX),
            Ok(port) => (port, port),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{session_id_from_tags, session_tag};
    use proptest::prelude::*;

    #[test]
    fn size_slugs() {
        assert_eq!(size_slug(InstanceClass::Small), "s-1vcpu-2gb");
        assert_eq!(size_slug(InstanceClass::Standard), "s-2vcpu-2gb");
    }

    #[test]
    fn microusd_conversion_rounds_half_up() {
        assert_eq!(microusd_from_hourly(0.02679), 26_790);
        assert_eq!(microusd_from_hourly(0.01786), 17_860);
        assert_eq!(microusd_from_hourly(0.0000015), 2);
        assert_eq!(microusd_from_hourly(0.0), 0);
    }

    #[test]
    fn tag_encoding_canonical_session() {
        let (k, v) = session_tag("abc123");
        assert_eq!(to_do_tag(&k, &v), "jamstream-session:abc123");
        assert_eq!(
            from_do_tag("jamstream-session:abc123"),
            ("jamstream-session".to_owned(), "abc123".to_owned())
        );
        // Bare tags round trip through an empty value.
        assert_eq!(from_do_tag(BARE_TAG), (BARE_TAG.to_owned(), String::new()));
        assert_eq!(to_do_tag(BARE_TAG, ""), BARE_TAG);
        // Disallowed characters are replaced, never sent.
        assert_eq!(to_do_tag("k=ey", "v al"), "k-ey:v-al");
        // Values keep their own colons; the split is on the first colon.
        assert_eq!(
            from_do_tag("jamstream-session:a:b"),
            ("jamstream-session".to_owned(), "a:b".to_owned())
        );
    }

    #[test]
    fn regions_are_static_and_do_flavored() {
        let p = DigitalOceanProvider::new("t".into());
        let regions = p.regions();
        assert_eq!(regions.len(), 11);
        assert!(
            regions
                .iter()
                .all(|r| r.provider == ProviderKind::DigitalOcean)
        );
        let fra = regions.iter().find(|r| r.id.as_str() == "fra1").unwrap();
        assert_eq!(fra.country, "DE");
        assert_eq!(fra.display, "Frankfurt 1");
    }

    #[test]
    fn debug_redacts_token() {
        let p = DigitalOceanProvider::new("dop_v1_supersecret".into());
        let debugged = format!("{p:?}");
        assert!(!debugged.contains("supersecret"));
        assert!(debugged.contains("<redacted>"));
    }

    proptest! {
        /// Canonical pair -> DO tag string -> canonical pair, for any
        /// DO-safe session id; session_id_from_tags stays correct.
        #[test]
        fn session_tag_round_trips(id in "[a-zA-Z0-9_][a-zA-Z0-9_-]{0,30}") {
            let (k, v) = session_tag(&id);
            let do_tag = to_do_tag(&k, &v);
            prop_assert!(do_tag.chars().all(do_tag_char));
            prop_assert_eq!(from_do_tag(&do_tag), (k, v));
            let tags = vec![from_do_tag(BARE_TAG), from_do_tag(&do_tag)];
            prop_assert_eq!(session_id_from_tags(&tags), Some(id.as_str()));
        }
    }
}
