use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::provider::ProviderError;

/// Tag key applied to every resource JamStream creates. The value is the
/// session id; the sweeper keys off this and nothing else.
pub const SESSION_TAG_KEY: &str = "jamstream-session";

/// UDP port a session server listens on unless the host overrides it. The
/// per-session firewall opens exactly this port, so the two have to agree:
/// see `Provider::session_port`.
pub const DEFAULT_SESSION_PORT: u16 = 43210;

/// How long a launch waits for the provider to report the instance's public
/// address, and how often it asks again. This is a fact about the providers,
/// not about whichever front end is driving them, so both the CLI and the app
/// wait the same amount before telling a host the machine never came up. The
/// cap matches [`crate::WaitOpts`]'s total timeout, which bounds the other
/// half of the same launch.
pub const IP_WAIT_CAP: Duration = Duration::from_secs(180);
pub const IP_POLL_PERIOD: Duration = Duration::from_secs(2);

/// How long a launch drives a real handshake against the server it just
/// started before reporting the session unreachable. Long enough for a VM to
/// finish booting and downloading `jamstreamd` after it has an address.
pub const HANDSHAKE_CAP: Duration = Duration::from_secs(60);

/// Everywhere, in the two address families. A musician dials in from an
/// address nobody knows in advance, so the session port cannot be narrowed
/// to anything smaller than this.
pub const ANY_IPV4: &str = "0.0.0.0/0";
pub const ANY_IPV6: &str = "::/0";

/// One ingress permission a provider has open for a session, normalized
/// across three unrelated APIs so the same assertions can be made about
/// all of them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct IngressRule {
    /// Lowercase protocol name as the provider reports it: `udp`, `tcp`,
    /// `icmp`.
    pub protocol: String,
    /// Inclusive port range. Both ends are equal for a single port.
    pub from_port: u16,
    pub to_port: u16,
    /// Source ranges, sorted, as CIDR strings.
    pub cidrs: Vec<String>,
}

impl IngressRule {
    /// The only rule a session needs: its UDP port, reachable from
    /// anywhere. `cidrs` holds whichever families the provider's network
    /// actually carries, so a v4-only instance gets one entry.
    pub fn session_udp(port: u16, cidrs: Vec<String>) -> Self {
        IngressRule {
            protocol: "udp".to_owned(),
            from_port: port,
            to_port: port,
            cidrs,
        }
    }

    /// True when this is exactly one port and it is that port.
    pub fn is_only_port(&self, port: u16) -> bool {
        self.from_port == port && self.to_port == port
    }

    /// True when every source is an everywhere range. Narrowing the session
    /// port would lock out musicians, so the check is that the sources are
    /// these and not that they are small.
    pub fn is_open_to_the_internet(&self) -> bool {
        !self.cidrs.is_empty()
            && self
                .cidrs
                .iter()
                .all(|c| c == ANY_IPV4 || c == ANY_IPV6 || c == "::0/0")
    }
}

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
    /// Every provider, in the order the CLI and the app offer them. A fifth
    /// one is a compile error here rather than a name some list forgot.
    pub const ALL: [ProviderKind; 4] = [
        ProviderKind::Local,
        ProviderKind::DigitalOcean,
        ProviderKind::Aws,
        ProviderKind::Gcp,
    ];

    /// The one spelling of this provider's name. Config files, session
    /// records, keychain entries, and error messages all use it, and
    /// [`ProviderKind::from_str`] is its only inverse.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Aws => "aws",
            ProviderKind::DigitalOcean => "digitalocean",
            ProviderKind::Gcp => "gcp",
            ProviderKind::Local => "local",
        }
    }

    /// True when this provider has an S3-compatible bucket a recording can go
    /// to. A local session records to the host's own disk instead.
    pub fn has_object_storage(&self) -> bool {
        match self {
            ProviderKind::Aws | ProviderKind::DigitalOcean | ProviderKind::Gcp => true,
            ProviderKind::Local => false,
        }
    }

    /// Comma-separated names, for an error that has to say what it would have
    /// accepted.
    pub fn name_list(kinds: impl IntoIterator<Item = ProviderKind>) -> String {
        kinds
            .into_iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The inverse of [`ProviderKind::as_str`], accepting that spelling and
/// nothing else beyond surrounding space and case.
impl FromStr for ProviderKind {
    type Err = ProviderError;

    fn from_str(s: &str) -> Result<Self, ProviderError> {
        let normalized = s.trim().to_ascii_lowercase();
        ProviderKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == normalized)
            .ok_or_else(|| {
                ProviderError::Other(format!(
                    "unknown provider {s:?}; known providers are {}",
                    ProviderKind::name_list(ProviderKind::ALL)
                ))
            })
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSpec {
    pub region: Region,
    pub instance_class: InstanceClass,
    /// Boot payload, interpreted per provider: cloud providers receive
    /// cloud-init YAML (`cloudinit::render`); the local provider receives
    /// the flat key=value server config (`BootConfig::render_flat_config`).
    ///
    /// Either way it carries the session server's private key, and on
    /// DigitalOcean an account API token, so it stays out of `Debug`.
    pub user_data: String,
    pub tags: Vec<(String, String)>,
}

/// Hand-written, like every other type in this crate that holds a secret:
/// one `tracing::debug!(?spec)` would otherwise print a private key into
/// the host's terminal and whatever collects it.
impl fmt::Debug for LaunchSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchSpec")
            .field("region", &self.region)
            .field("instance_class", &self.instance_class)
            .field(
                "user_data",
                &format_args!("<{} bytes>", self.user_data.len()),
            )
            .field("tags", &self.tags)
            .finish()
    }
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

    /// user_data is the server's private key on every provider and the
    /// host's API token on DigitalOcean. It must not be printable by
    /// accident.
    #[test]
    fn debug_never_reveals_the_boot_payload() {
        let spec = LaunchSpec {
            region: Region {
                provider: ProviderKind::Aws,
                id: RegionId::new("us-east-1"),
                display: "N. Virginia".to_owned(),
                country: "US".to_owned(),
            },
            instance_class: InstanceClass::Standard,
            user_data: "server_private_key_b64 = c3VwZXJzZWNyZXQ=\n".to_owned(),
            tags: vec![session_tag("deadbeef")],
        };
        let rendered = format!("{spec:?}");
        assert!(!rendered.contains("c3VwZXJzZWNyZXQ="));
        assert!(!rendered.contains("server_private_key"));
        assert!(rendered.contains("<42 bytes>"), "was: {rendered}");
        // The rest is what makes the line worth logging at all.
        assert!(rendered.contains("us-east-1"));
        assert!(rendered.contains("deadbeef"));
    }

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
    fn session_rule_is_one_port_open_to_everyone() {
        let rule = IngressRule::session_udp(
            DEFAULT_SESSION_PORT,
            vec![ANY_IPV4.to_owned(), ANY_IPV6.to_owned()],
        );
        assert_eq!(rule.protocol, "udp");
        assert!(rule.is_only_port(43210));
        assert!(!rule.is_only_port(43211));
        assert!(rule.is_open_to_the_internet());
    }

    #[test]
    fn a_port_range_is_not_one_port() {
        let range = IngressRule {
            protocol: "udp".to_owned(),
            from_port: 0,
            to_port: u16::MAX,
            cidrs: vec![ANY_IPV4.to_owned()],
        };
        assert!(!range.is_only_port(43210));
    }

    #[test]
    fn a_narrowed_source_is_not_open_to_the_internet() {
        // A session narrowed to one address locks out every musician whose
        // address nobody knew at launch.
        let narrowed = IngressRule {
            protocol: "udp".to_owned(),
            from_port: 43210,
            to_port: 43210,
            cidrs: vec!["198.51.100.7/32".to_owned()],
        };
        assert!(!narrowed.is_open_to_the_internet());
        let empty = IngressRule {
            cidrs: Vec::new(),
            ..narrowed
        };
        assert!(!empty.is_open_to_the_internet());
    }

    #[test]
    fn provider_kind_strings() {
        assert_eq!(ProviderKind::Aws.as_str(), "aws");
        assert_eq!(ProviderKind::DigitalOcean.as_str(), "digitalocean");
        assert_eq!(ProviderKind::Gcp.as_str(), "gcp");
        assert_eq!(ProviderKind::Local.as_str(), "local");
    }

    #[test]
    fn every_provider_name_parses_back_to_the_kind_it_names() {
        for kind in ProviderKind::ALL {
            assert_eq!(kind.as_str().parse::<ProviderKind>().unwrap(), kind);
            // Config files and flags arrive with whatever spacing and case a
            // host typed.
            let typed = format!("  {}  ", kind.as_str().to_ascii_uppercase());
            assert_eq!(typed.parse::<ProviderKind>().unwrap(), kind);
        }
        // ALL is a set, not a list that grew a duplicate.
        let mut names: Vec<&str> = ProviderKind::ALL.iter().map(|k| k.as_str()).collect();
        names.sort_unstable();
        let unique = names.len();
        names.dedup();
        assert_eq!(names.len(), unique);
    }

    #[test]
    fn an_unknown_provider_name_lists_the_ones_that_exist() {
        let err = "azure".parse::<ProviderKind>().unwrap_err().to_string();
        assert!(err.contains("azure"), "{err}");
        for kind in ProviderKind::ALL {
            assert!(err.contains(kind.as_str()), "{err} omits {kind}");
        }
        // Not a name, not a kind: an empty provider must not resolve to one.
        assert!("".parse::<ProviderKind>().is_err());
        assert!("digital ocean".parse::<ProviderKind>().is_err());
    }

    #[test]
    fn only_the_local_provider_has_no_bucket() {
        assert!(!ProviderKind::Local.has_object_storage());
        for kind in ProviderKind::ALL {
            assert_eq!(
                kind.has_object_storage(),
                kind != ProviderKind::Local,
                "{kind}"
            );
        }
        assert_eq!(
            ProviderKind::name_list(
                ProviderKind::ALL
                    .into_iter()
                    .filter(|k| k.has_object_storage())
            ),
            "digitalocean, aws, gcp"
        );
    }
}
