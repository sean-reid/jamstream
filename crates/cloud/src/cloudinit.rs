//! Renders the cloud-init user-data for the session VM. Plain format!, no
//! template engine. The rendered YAML is snapshot-tested per self-destruct
//! variant; change the output and the snapshots must change with it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

use serde::Deserialize;

/// Pinned, checksummed static builds of the broadcast subprocesses. See
/// data/media_artifacts.json for the licensing and refresh notes.
const MEDIA_ARTIFACTS_JSON: &str = include_str!("../data/media_artifacts.json");

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MediaArtifact {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MediaTool {
    pub version: String,
    pub source: String,
    pub license: String,
    /// `tar.xz` or `tar.gz`.
    pub archive: String,
    /// Path of the binary inside the archive; may contain a leading wildcard.
    pub member: String,
    /// Keyed by `uname -m`: `x86_64`, `aarch64`.
    pub targets: BTreeMap<String, MediaArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MediaArtifacts {
    pub ffmpeg: MediaTool,
    pub mediamtx: MediaTool,
}

/// The bundled pins. The VM picks its own architecture at boot from
/// `uname -m`, so provisioning does not have to know it.
pub fn media_artifacts() -> &'static MediaArtifacts {
    static PARSED: OnceLock<MediaArtifacts> = OnceLock::new();
    PARSED.get_or_init(|| {
        serde_json::from_str(MEDIA_ARTIFACTS_JSON).expect("data/media_artifacts.json is invalid")
    })
}

/// How the VM guarantees its own death, per provider capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfDestruct {
    /// AWS: instance-initiated shutdown behavior is set to terminate at
    /// launch, so plain shutdown terminates with no credentials on the box.
    AwsShutdown,
    /// DigitalOcean: powered-off droplets still bill, so the box deletes
    /// itself through the API with a droplet-scoped token from user-data.
    ApiToken { endpoint: String, token: String },
    /// GCP: maxRunDuration with instanceTerminationAction=DELETE is the
    /// provider-enforced hard cap; the idle path self-deletes with the
    /// scoped service account token from metadata.
    GcpMaxRunDuration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootConfig {
    pub artifact_url: String,
    pub artifact_sha256: String,
    pub server_private_key_b64: String,
    pub issuer_public_key_b64: String,
    pub session_id_hex: String,
    pub port: u16,
    pub idle_shutdown_min: u32,
    pub max_duration_min: u32,
    pub self_destruct: SelfDestruct,
}

impl BootConfig {
    /// The flat key=value config jamstreamd parses at startup. This is the
    /// single home of the format: cloud-init writes it to
    /// /etc/jamstream/config, and the local provider writes it straight to
    /// disk as `LaunchSpec::user_data`.
    pub fn render_flat_config(&self) -> String {
        format!(
            "session_id_hex = {}\n\
             port = {}\n\
             server_private_key_b64 = {}\n\
             issuer_public_key_b64 = {}\n\
             idle_shutdown_min = {}\n\
             max_duration_min = {}\n",
            self.session_id_hex,
            self.port,
            self.server_private_key_b64,
            self.issuer_public_key_b64,
            self.idle_shutdown_min,
            self.max_duration_min,
        )
    }
}

/// Prefixes every nonempty line for embedding in a YAML block scalar.
fn indent(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    let mut out = String::new();
    for line in text.lines() {
        if line.is_empty() {
            out.push('\n');
        } else {
            let _ = writeln!(out, "{pad}{line}");
        }
    }
    out
}

fn self_destruct_script(cfg: &BootConfig) -> String {
    match &cfg.self_destruct {
        SelfDestruct::AwsShutdown => "#!/bin/sh
# Instance-initiated shutdown behavior is terminate, so shutdown kills the
# instance for good with no credentials on the box.
echo \"jamstream self-destruct: $1\" > /dev/console 2>/dev/null || true
shutdown -h now
"
        .to_owned(),
        SelfDestruct::ApiToken { endpoint, token } => format!(
            "#!/bin/sh
# Powered-off droplets still bill; deletion via the API is the only safe
# end state. Falls back to shutdown if the API call fails.
echo \"jamstream self-destruct: $1\" > /dev/console 2>/dev/null || true
droplet_id=$(curl -fsS http://169.254.169.254/metadata/v1/id)
curl -fsS -X DELETE -H \"Authorization: Bearer {token}\" \\
  \"{endpoint}/$droplet_id\" || shutdown -h now
"
        ),
        SelfDestruct::GcpMaxRunDuration => "#!/bin/sh
# The hard cap is provider-enforced (maxRunDuration with
# instanceTerminationAction=DELETE). This script is the idle path: delete
# the instance with the scoped service account token from metadata.
echo \"jamstream self-destruct: $1\" > /dev/console 2>/dev/null || true
md=http://metadata.google.internal/computeMetadata/v1
name=$(curl -fsS -H 'Metadata-Flavor: Google' \"$md/instance/name\")
zone=$(curl -fsS -H 'Metadata-Flavor: Google' \"$md/instance/zone\")
token=$(curl -fsS -H 'Metadata-Flavor: Google' \\
  \"$md/instance/service-accounts/default/token\" \\
  | sed -n 's/.*\"access_token\":\"\\([^\"]*\\)\".*/\\1/p')
curl -fsS -X DELETE -H \"Authorization: Bearer $token\" \\
  \"https://compute.googleapis.com/compute/v1/$zone/instances/$name\" \\
  || poweroff
"
        .to_owned(),
    }
}

fn guard_script(cfg: &BootConfig) -> String {
    format!(
        "#!/bin/sh
# Dead man's switch. jamstreamd touches /run/jamstream/last-active while
# musicians are connected; staleness past the idle window, or exceeding the
# session hard cap, triggers self-destruct.
set -eu
now=$(date +%s)
boot=$((now - $(cut -d. -f1 /proc/uptime)))
last=$(stat -c %Y /run/jamstream/last-active 2>/dev/null || echo \"$boot\")
if [ $((now - last)) -ge {idle_secs} ]; then
  exec /usr/local/sbin/jamstream-self-destruct \"idle for $((now - last))s\"
fi
if [ $((now - boot)) -ge {max_secs} ]; then
  exec /usr/local/sbin/jamstream-self-destruct \"max session duration reached\"
fi
",
        idle_secs = cfg.idle_shutdown_min as u64 * 60,
        max_secs = cfg.max_duration_min as u64 * 60,
    )
}

/// Fetches one pinned tool into /usr/local/bin, refusing on a hash mismatch
/// exactly like the jamstreamd download. The architecture is resolved on the
/// box, so provisioning stays arch-agnostic.
fn fetch_media_tool(name: &str, tool: &MediaTool) -> String {
    let mut cases = String::new();
    for (arch, artifact) in &tool.targets {
        let pattern = match arch.as_str() {
            // uname -m says aarch64 on Linux; accept arm64 too.
            "aarch64" => "aarch64|arm64",
            other => other,
        };
        let _ = writeln!(
            cases,
            "  {pattern}) url=\"{url}\"; sha=\"{sha}\" ;;",
            url = artifact.url,
            sha = artifact.sha256,
        );
    }
    let extract = if tool.archive == "tar.xz" {
        "xJf"
    } else {
        "xzf"
    };
    let strip = match tool.member.matches('/').count() {
        0 => String::new(),
        n => format!(" --strip-components={n}"),
    };
    // The GPL obligation only arises for the copyleft one, and the reason it
    // stays where it is belongs next to the download.
    let license_note = if tool.license.starts_with("GPL") {
        "\n# JamStream never links it, only spawns it, which is what keeps the\n# copyleft obligation at the process boundary."
    } else {
        ""
    };
    format!(
        "# {name} {version}, {license}. Pinned in data/media_artifacts.json.{license_note}
# Source: {source}
case \"$(uname -m)\" in
{cases}  *) echo \"jamstream: no pinned {name} for $(uname -m)\" >&2; exit 1 ;;
esac
curl -fsSL --retry 5 -o \"$tmp/{name}.archive\" \"$url\"
if ! echo \"$sha  $tmp/{name}.archive\" | sha256sum -c -; then
  echo \"jamstream: {name} sha256 mismatch, refusing to start\" >&2
  rm -rf \"$tmp\"
  exit 1
fi
tar -{extract} \"$tmp/{name}.archive\" -C \"$tmp\" --wildcards{strip} '{member}'
install -m 0755 \"$tmp/{name}\" /usr/local/bin/{name}
rm -f \"$tmp/{name}.archive\" \"$tmp/{name}\"
",
        version = tool.version,
        license = tool.license,
        source = tool.source,
        member = tool.member,
    )
}

fn bootstrap_script(cfg: &BootConfig) -> String {
    let media = media_artifacts();
    format!(
        "#!/bin/sh
set -eu
mkdir -p /run/jamstream
touch /run/jamstream/last-active
# Stream keys are staged here for the instant it takes to spawn a pusher.
# /run is tmpfs, so nothing reaches persistent disk.
mkdir -p /run/jamstream/keys
chmod 0700 /run/jamstream/keys
# A pusher's ffmpeg receives its ingest URL on stdin, but execs with it in
# argv, so hide other processes' command lines from non-root. The VM has no
# other users; this is the belt.
mount -o remount,hidepid=2 /proc 2>/dev/null || true
curl -fsSL --retry 5 -o /usr/local/bin/jamstreamd.download \"{url}\"
if ! echo \"{sha}  /usr/local/bin/jamstreamd.download\" | sha256sum -c -; then
  echo \"jamstream: artifact sha256 mismatch, refusing to start\" >&2
  rm -f /usr/local/bin/jamstreamd.download
  exit 1
fi
mv /usr/local/bin/jamstreamd.download /usr/local/bin/jamstreamd
chmod 0755 /usr/local/bin/jamstreamd
# Only the session UDP port is reachable from outside.
if command -v ufw >/dev/null 2>&1; then
  ufw default deny incoming
  ufw allow {port}/udp
  ufw --force enable
else
  iptables -A INPUT -i lo -j ACCEPT
  iptables -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
  iptables -A INPUT -p udp --dport {port} -j ACCEPT
  iptables -P INPUT DROP
fi
systemctl daemon-reload
# The session server comes up first: musicians are waiting, and the broadcast
# tooling is only needed once the host goes live.
systemctl enable --now jamstreamd.service
systemctl enable --now jamstream-guard.timer
# Broadcast subprocesses, pinned and checksummed like everything else.
tmp=$(mktemp -d)
{ffmpeg}{mediamtx}rmdir \"$tmp\" 2>/dev/null || true
systemctl enable --now mediamtx.service
",
        url = cfg.artifact_url,
        sha = cfg.artifact_sha256,
        port = cfg.port,
        ffmpeg = fetch_media_tool("ffmpeg", &media.ffmpeg),
        mediamtx = fetch_media_tool("mediamtx", &media.mediamtx),
    )
}

const JAMSTREAMD_UNIT: &str = "[Unit]
Description=JamStream session server
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/jamstreamd --config /etc/jamstream/config
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
";

/// The relay the encoder publishes to and the pushers read from. Bound to
/// loopback and stripped to one protocol and one path: nothing else has any
/// business reaching it, and the firewall already denies inbound anyway.
const MEDIAMTX_CONFIG: &str = "logLevel: warn
logDestinations: [stdout]
readTimeout: 10s
writeTimeout: 10s
# One RTMP publisher (the encoder) and N local readers (the pushers).
rtmp: true
rtmpAddress: 127.0.0.1:1935
rtmpEncryption: \"no\"
rtsp: false
rtsps: false
hls: false
webrtc: false
srt: false
api: false
metrics: false
pprof: false
playback: false
paths:
  jamstream:
    source: publisher
";

const MEDIAMTX_UNIT: &str = "[Unit]
Description=JamStream broadcast relay (MediaMTX)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/mediamtx /etc/jamstream/mediamtx.yml
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
";

const GUARD_UNIT: &str = "[Unit]
Description=JamStream dead man's switch

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/jamstream-guard
";

const GUARD_TIMER: &str = "[Unit]
Description=Run the JamStream dead man's switch every minute

[Timer]
OnBootSec=2min
OnUnitActiveSec=1min

[Install]
WantedBy=timers.target
";

pub fn render(cfg: &BootConfig) -> String {
    let files: [(&str, &str, String); 8] = [
        ("/etc/jamstream/config", "0600", cfg.render_flat_config()),
        (
            "/usr/local/sbin/jamstream-self-destruct",
            "0700",
            self_destruct_script(cfg),
        ),
        ("/usr/local/sbin/jamstream-guard", "0700", guard_script(cfg)),
        (
            "/usr/local/sbin/jamstream-bootstrap",
            "0700",
            bootstrap_script(cfg),
        ),
        (
            "/etc/systemd/system/jamstreamd.service",
            "0644",
            JAMSTREAMD_UNIT.to_owned(),
        ),
        (
            "/etc/systemd/system/jamstream-guard.service",
            "0644",
            GUARD_UNIT.to_owned(),
        ),
        (
            "/etc/jamstream/mediamtx.yml",
            "0644",
            MEDIAMTX_CONFIG.to_owned(),
        ),
        (
            "/etc/systemd/system/mediamtx.service",
            "0644",
            MEDIAMTX_UNIT.to_owned(),
        ),
    ];

    let mut out = String::from("#cloud-config\nwrite_files:\n");
    for (path, mode, content) in files {
        let _ = write!(
            out,
            "  - path: {path}\n    owner: root:root\n    permissions: \"{mode}\"\n    content: |\n{}",
            indent(&content, 6)
        );
    }
    let _ = write!(
        out,
        "  - path: /etc/systemd/system/jamstream-guard.timer\n    owner: root:root\n    permissions: \"0644\"\n    content: |\n{}",
        indent(GUARD_TIMER, 6)
    );
    out.push_str("runcmd:\n  - [/usr/local/sbin/jamstream-bootstrap]\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base_config(self_destruct: SelfDestruct) -> BootConfig {
        BootConfig {
            artifact_url: "https://github.com/sean-reid/jamstream/releases/download/v0.1.0/jamstreamd-x86_64-unknown-linux-musl".to_owned(),
            artifact_sha256: "0f2e5c1d3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d".to_owned(),
            server_private_key_b64: "c2VydmVyLXByaXZhdGUta2V5".to_owned(),
            issuer_public_key_b64: "aXNzdWVyLXB1YmxpYy1rZXk=".to_owned(),
            session_id_hex: "deadbeefcafef00d".to_owned(),
            port: 43210,
            idle_shutdown_min: 10,
            max_duration_min: 720,
            self_destruct,
        }
    }

    fn check_snapshot(name: &str, rendered: &str) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name);
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, rendered).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!("missing snapshot {path:?}; run with UPDATE_SNAPSHOTS=1 to create")
        });
        assert_eq!(rendered, expected, "snapshot mismatch for {name}");
    }

    #[test]
    fn snapshot_aws_shutdown() {
        let out = render(&base_config(SelfDestruct::AwsShutdown));
        check_snapshot("cloudinit_aws_shutdown.yaml", &out);
        assert!(out.contains("shutdown -h now"));
        assert!(!out.contains("Authorization: Bearer"));
    }

    #[test]
    fn snapshot_api_token() {
        let out = render(&base_config(SelfDestruct::ApiToken {
            endpoint: "https://api.digitalocean.com/v2/droplets".to_owned(),
            token: "dop_v1_testtoken".to_owned(),
        }));
        check_snapshot("cloudinit_api_token.yaml", &out);
        assert!(out.contains("Authorization: Bearer dop_v1_testtoken"));
        assert!(out.contains("https://api.digitalocean.com/v2/droplets/$droplet_id"));
    }

    #[test]
    fn snapshot_gcp_max_run_duration() {
        let out = render(&base_config(SelfDestruct::GcpMaxRunDuration));
        check_snapshot("cloudinit_gcp_max_run_duration.yaml", &out);
        assert!(out.contains("compute.googleapis.com"));
        assert!(out.contains("Metadata-Flavor: Google"));
    }

    #[test]
    fn rendered_invariants() {
        for sd in [
            SelfDestruct::AwsShutdown,
            SelfDestruct::ApiToken {
                endpoint: "https://api.digitalocean.com/v2/droplets".to_owned(),
                token: "t".to_owned(),
            },
            SelfDestruct::GcpMaxRunDuration,
        ] {
            let cfg = base_config(sd);
            let out = render(&cfg);
            assert!(out.starts_with("#cloud-config\n"));
            // Secrets file is root-only.
            assert!(out.contains(
                "path: /etc/jamstream/config\n    owner: root:root\n    permissions: \"0600\""
            ));
            // Refuses to start on artifact hash mismatch.
            assert!(out.contains("sha256sum -c -"));
            assert!(out.contains("refusing to start"));
            assert!(out.contains(&cfg.artifact_sha256));
            // Firewall opens only the session UDP port.
            assert!(out.contains("ufw allow 43210/udp"));
            assert!(out.contains("--dport 43210 -j ACCEPT"));
            // Guard thresholds in seconds.
            assert!(out.contains("-ge 600 ]"));
            assert!(out.contains("-ge 43200 ]"));
            assert!(out.contains("systemctl enable --now jamstreamd.service"));
            assert!(out.contains("systemctl enable --now jamstream-guard.timer"));
        }
    }

    #[test]
    fn broadcast_tooling_is_pinned_verified_and_local_only() {
        let out = render(&base_config(SelfDestruct::AwsShutdown));
        let media = media_artifacts();

        // Every pinned pair appears, and every one of them is verified with
        // the same refuse-on-mismatch discipline as jamstreamd.
        for (name, tool) in [("ffmpeg", &media.ffmpeg), ("mediamtx", &media.mediamtx)] {
            assert!(!tool.targets.is_empty(), "{name} has no targets");
            for artifact in tool.targets.values() {
                assert!(out.contains(&artifact.url), "{name} url missing");
                assert!(out.contains(&artifact.sha256), "{name} sha missing");
            }
            assert!(out.contains(&format!(
                "jamstream: {name} sha256 mismatch, refusing to start"
            )));
            assert!(out.contains(&format!("/usr/local/bin/{name}")));
        }
        // Both architectures are selected on the box, not at provision time.
        assert!(out.contains("case \"$(uname -m)\" in"));
        assert!(out.contains("x86_64)"));
        assert!(out.contains("aarch64|arm64)"));

        // The relay listens on loopback only and serves exactly one path.
        assert!(out.contains("rtmpAddress: 127.0.0.1:1935"));
        assert!(out.contains("    source: publisher"));
        assert!(out.contains("api: false"));
        assert!(out.contains("systemctl enable --now mediamtx.service"));
        // The session server is up before the broadcast tooling downloads.
        let jamstreamd = out.find("enable --now jamstreamd.service").unwrap();
        let ffmpeg = out.find("ffmpeg.archive").unwrap();
        assert!(
            jamstreamd < ffmpeg,
            "the session waits on a 100 MB download"
        );

        // Key staging is root-only tmpfs, and other processes' argv is hidden.
        assert!(out.contains("mkdir -p /run/jamstream/keys"));
        assert!(out.contains("chmod 0700 /run/jamstream/keys"));
        assert!(out.contains("hidepid=2"));

        // The GPL note travels with the copyleft artifact and only that one.
        assert!(media.ffmpeg.license.starts_with("GPL"));
        assert_eq!(media.mediamtx.license, "MIT");
        assert_eq!(
            out.matches("copyleft obligation at the process boundary")
                .count(),
            1
        );
    }

    #[test]
    fn pinned_media_urls_are_immutable_and_hashes_well_formed() {
        let media = media_artifacts();
        for tool in [&media.ffmpeg, &media.mediamtx] {
            for (arch, artifact) in &tool.targets {
                assert!(
                    artifact.sha256.len() == 64
                        && artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                    "{arch} sha256 is not 64 hex digits"
                );
                assert!(artifact.url.starts_with("https://"), "{arch} url not https");
                // A moving URL under a fixed path would turn every later boot
                // into a hash mismatch.
                for moving in ["latest/", "-release-", "/release/"] {
                    assert!(
                        !artifact.url.contains(moving),
                        "{arch} url looks mutable: {}",
                        artifact.url
                    );
                }
                assert!(
                    artifact.url.contains(&tool.version) || tool.version.contains('-'),
                    "{arch} url does not name the pinned version"
                );
            }
        }
    }
}
