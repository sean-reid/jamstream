use std::fmt;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// Tag key applied to every resource JamStream creates. The value is the
/// session id; the sweeper keys off this and nothing else.
pub const SESSION_TAG_KEY: &str = "jamstream-session";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Aws,
    DigitalOcean,
    Gcp,
    /// The host's own machine: jamstreamd runs as a local child process.
    Local,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Aws => "aws",
            ProviderKind::DigitalOcean => "digitalocean",
            ProviderKind::Gcp => "gcp",
            ProviderKind::Local => "local",
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RegionId(pub String);

impl RegionId {
    pub fn new(id: impl Into<String>) -> Self {
        RegionId(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RegionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for RegionId {
    fn from(s: &str) -> Self {
        RegionId(s.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub provider: ProviderKind,
    pub id: RegionId,
    pub display: String,
    pub country: String,
}

/// All monetary values are integer microdollars (1_000_000 per USD) so cost
/// math never touches floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    pub hourly_microusd: u64,
    pub egress_microusd_per_gb: u64,
    pub included_egress_gb: u32,
}

impl Price {
    pub fn hourly_display(&self) -> String {
        format!("{}/hr", format_microusd(self.hourly_microusd))
    }

    pub fn egress_display(&self) -> String {
        format!("{}/GB", format_microusd(self.egress_microusd_per_gb))
    }
}

/// "$0.0168" style: trims trailing zeros but always keeps at least two
/// decimal places.
pub fn format_microusd(microusd: u64) -> String {
    let dollars = microusd / 1_000_000;
    let frac = microusd % 1_000_000;
    let mut frac_str = format!("{frac:06}");
    while frac_str.len() > 2 && frac_str.ends_with('0') {
        frac_str.pop();
    }
    format!("${dollars}.{frac_str}")
}

/// Size hint only; the provider implementation maps this to a concrete
/// instance type per region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstanceClass {
    Small,
    Standard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSpec {
    pub region: Region,
    pub instance_class: InstanceClass,
    /// Boot payload, interpreted per provider: cloud providers receive
    /// cloud-init YAML (`cloudinit::render`); the local provider receives
    /// the flat key=value server config (`BootConfig::render_flat_config`).
    pub user_data: String,
    pub tags: Vec<(String, String)>,
}

impl LaunchSpec {
    pub fn session_id(&self) -> Option<&str> {
        session_id_from_tags(&self.tags)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub provider: ProviderKind,
    pub region: Region,
    pub id: String,
    pub public_ip: Option<IpAddr>,
    pub tags: Vec<(String, String)>,
}

impl Instance {
    pub fn session_id(&self) -> Option<&str> {
        session_id_from_tags(&self.tags)
    }
}

/// Builds the canonical session tag pair for a launch.
pub fn session_tag(session_id: &str) -> (String, String) {
    (SESSION_TAG_KEY.to_owned(), session_id.to_owned())
}

/// Extracts the session id from a tag list, if present.
pub fn session_id_from_tags(tags: &[(String, String)]) -> Option<&str> {
    tags.iter()
        .find(|(k, _)| k == SESSION_TAG_KEY)
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_microusd_examples() {
        assert_eq!(format_microusd(16_800), "$0.0168");
        assert_eq!(format_microusd(0), "$0.00");
        assert_eq!(format_microusd(5_000_000), "$5.00");
        assert_eq!(format_microusd(1_234_567), "$1.234567");
        assert_eq!(format_microusd(10_000), "$0.01");
        assert_eq!(format_microusd(100_000), "$0.10");
    }

    #[test]
    fn price_display() {
        let p = Price {
            hourly_microusd: 16_800,
            egress_microusd_per_gb: 90_000,
            included_egress_gb: 0,
        };
        assert_eq!(p.hourly_display(), "$0.0168/hr");
        assert_eq!(p.egress_display(), "$0.09/GB");
    }

    #[test]
    fn zero_price_displays_sensibly() {
        // The local provider is free; its price card must still read well.
        let p = Price {
            hourly_microusd: 0,
            egress_microusd_per_gb: 0,
            included_egress_gb: 0,
        };
        assert_eq!(p.hourly_display(), "$0.00/hr");
        assert_eq!(p.egress_display(), "$0.00/GB");
    }

    #[test]
    fn session_tag_round_trip() {
        let tag = session_tag("abc123");
        let tags = vec![("name".to_owned(), "x".to_owned()), tag];
        assert_eq!(session_id_from_tags(&tags), Some("abc123"));
        assert_eq!(session_id_from_tags(&[]), None);
    }

    #[test]
    fn provider_kind_strings() {
        assert_eq!(ProviderKind::Aws.as_str(), "aws");
        assert_eq!(ProviderKind::DigitalOcean.as_str(), "digitalocean");
        assert_eq!(ProviderKind::Gcp.as_str(), "gcp");
        assert_eq!(ProviderKind::Local.as_str(), "local");
    }
}
