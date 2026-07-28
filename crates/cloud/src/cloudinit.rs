//! Renders the cloud-init user-data for the session VM. Plain format!, no
//! template engine. The rendered YAML is snapshot-tested per self-destruct
//! variant; change the output and the snapshots must change with it.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::sync::OnceLock;

use data_encoding::BASE64;
use serde::Deserialize;

use crate::retention::Retention;
use crate::types::ProviderKind;

/// Pinned, checksummed static builds of the broadcast subprocesses. See
/// data/media_artifacts.json for the licensing and refresh notes.
const MEDIA_ARTIFACTS_JSON: &str = include_str!("../data/media_artifacts.json");

/// Unprivileged system account both long-running services run as.
const SERVICE_USER: &str = "jamstream";

/// tmpfs working directory: the activity file the guard reads, the staged
/// stream keys, and the guard's own uptime bookkeeping.
const RUN_DIR: &str = "/run/jamstream";

/// Upload-in-flight markers. The recorder drops one file here per object it
/// is uploading and the guard defers self-destruct while any remain.
pub const UPLOAD_MARKER_DIR: &str = "/run/jamstream/uploads";

/// The file jamstreamd touches while musicians are connected, and the one the
/// guard stats to decide the session is idle.
///
/// Public because jamstreamd is in another crate and the unit does not pass
/// `--activity-file`: both ends spell this path from here, or the dead man's
/// switch fails in the direction that destroys a live session. A guard that
/// stats a path nobody touches reads one mtime forever, so the idle window
/// runs from boot with musicians playing.
pub const ACTIVITY_FILE: &str = "/run/jamstream/last-active";

/// [`ACTIVITY_FILE`] without its directory, for the two scripts that build
/// the path from a directory they were handed (the guard, so its tests can
/// run against a scratch dir, and the local provider).
pub const ACTIVITY_FILE_NAME: &str = "last-active";

/// Root-owned tmpfs directory the stream pipeline stages one-shot key files
/// in. The bootstrap creates it 0700; `jamstream_stream`'s `StreamConfig`
/// names the same path from the other side of a crate boundary it cannot
/// import across, so the seam is held by a test in the server crate.
pub const STREAM_KEY_DIR: &str = "/run/jamstream/keys";

/// The flat key=value server config. Written here, read by
/// `jamstream_server::config::Config`.
pub const SERVER_CONFIG_PATH: &str = "/etc/jamstream/config";

/// Hard ceiling, in uptime seconds, on how long in-flight uploads may defer
/// self-destruct. The upload streams during the session, so what remains at
/// teardown is the tail plus the completion calls; ten minutes covers the
/// retry budget, and past it the upload is abandoned, because a VM that
/// never dies costs more than a lost recording.
pub const UPLOAD_DEFER_CEILING_SECS: u32 = 600;

/// Where cloud-init writes the recording storage config, when there is one.
/// Same flat key=value format and the same root-then-chgrp handling as
/// [`SERVER_CONFIG_PATH`].
pub const RECORDING_CONFIG_PATH: &str = "/etc/jamstream/recording";

/// The jamstreamd download and the hash it must match, each in its own
/// file so the bootstrap script takes them as data rather than as text
/// pasted into it. The hash file is in `sha256sum -c` format.
const ARTIFACT_URL_FILE: &str = "/etc/jamstream/artifact-url";
const ARTIFACT_SHA_FILE: &str = "/etc/jamstream/artifact-sha256";
/// Where the download lands, named in the hash file, so the two agree.
const ARTIFACT_DOWNLOAD: &str = "/usr/local/bin/jamstreamd.download";

/// The link-local metadata address. All three providers serve user-data
/// here over plain HTTP: AWS at /latest/user-data, DigitalOcean at
/// /metadata/v1/user-data, and GCP behind metadata.google.internal, which
/// resolves to this same address.
const METADATA_V4: &str = "169.254.169.254";

/// AWS also serves IMDS over IPv6 on dual-stack instances.
const METADATA_V6: &str = "fd00:ec2::254";

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
#[derive(Clone, PartialEq, Eq)]
pub enum SelfDestruct {
    /// AWS: instance-initiated shutdown behavior is set to terminate at
    /// launch, so plain shutdown terminates with no credentials on the box.
    AwsShutdown,
    /// DigitalOcean: powered-off droplets still bill, so the box deletes
    /// itself through the API with a droplet-scoped token from user-data.
    ApiToken { endpoint: String, token: String },
    /// GCP: maxRunDuration with instanceTerminationAction=DELETE is the
    /// provider-enforced hard cap, and it is the only thing that ends the
    /// instance, because nothing on the box holds a credential that could.
    GcpMaxRunDuration,
}

/// The DigitalOcean token is the host's account credential, so it never
/// reaches a formatter, only the rendered script that needs it.
impl fmt::Debug for SelfDestruct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelfDestruct::AwsShutdown => f.write_str("AwsShutdown"),
            SelfDestruct::GcpMaxRunDuration => f.write_str("GcpMaxRunDuration"),
            SelfDestruct::ApiToken { endpoint, .. } => f
                .debug_struct("ApiToken")
                .field("endpoint", endpoint)
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

/// The credential the VM uploads recordings with, and never the one that
/// launches machines: it is written into the machine's user data, so a key that
/// could start instances would put the whole account on a box parsing
/// unauthenticated UDP.
///
/// Scope it to one bucket under [`crate::storage::RECORDING_PREFIX`]. Not
/// strictly write-only: the launch checks write and delete a probe object under
/// the session's prefix and set the bucket's lifecycle rule, and this is the key
/// they use. It never appears on argv or in a script body, only in the
/// root-owned config file.
#[derive(Clone, PartialEq, Eq)]
pub enum StorageCredential {
    /// SigV4 access key pair. AWS S3, DigitalOcean Spaces, and Google Cloud
    /// Storage through its interop endpoint all take one; the provider on
    /// the same config decides which endpoint it signs against.
    ///
    /// One shape for all three is deliberate. GCS could also take a service
    /// account key, but its RSA signing pulled aws-lc-sys into jamstreamd,
    /// whose C does not link against musl for aarch64, and an HMAC key needs
    /// nothing the SigV4 path does not already have.
    KeyPair {
        access_key_id: String,
        secret_access_key: String,
    },
}

/// Secrets stay out of Debug; the key id alone is worth seeing in a log.
impl fmt::Debug for StorageCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageCredential::KeyPair { access_key_id, .. } => f
                .debug_struct("KeyPair")
                .field("access_key_id", access_key_id)
                .field("secret_access_key", &"<redacted>")
                .finish(),
        }
    }
}

/// The bucket a session records to, carried to the VM with the other launch
/// parameters. Absent when the host has not turned recording on; local
/// sessions record to disk and never carry one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingStorage {
    pub provider: ProviderKind,
    pub bucket: String,
    /// The bucket's region: an AWS region, a Spaces slug, or a GCS location.
    pub region: String,
    pub retention: Retention,
    pub credential: StorageCredential,
    /// Capture per-member stereo stems alongside the mix; fixed for the
    /// session at launch.
    pub stems: bool,
}

impl RecordingStorage {
    /// The flat key=value file cloud-init writes to
    /// [`RECORDING_CONFIG_PATH`]. Secrets travel base64, matching the key
    /// convention in the server config.
    pub fn render_flat_config(&self) -> String {
        let mut out = format!(
            "provider = {}\nbucket = {}\nregion = {}\nretention = {}\nstems = {}\n",
            self.provider.as_str(),
            self.bucket,
            self.region,
            self.retention,
            self.stems,
        );
        let StorageCredential::KeyPair {
            access_key_id,
            secret_access_key,
        } = &self.credential;
        let _ = writeln!(out, "access_key_id = {access_key_id}");
        let _ = writeln!(
            out,
            "secret_access_key_b64 = {}",
            BASE64.encode(secret_access_key.as_bytes())
        );
        out
    }

    /// Parses what [`RecordingStorage::render_flat_config`] wrote, on the
    /// machine that received it. The two live in one impl so they cannot
    /// drift apart unnoticed; the round-trip test holds them together.
    pub fn parse_flat_config(text: &str) -> Result<RecordingStorage, String> {
        let want = |key: &str| {
            flat_config_value(text, key)
                .map(str::to_owned)
                .ok_or_else(|| format!("recording config is missing {key}"))
        };
        let provider: ProviderKind = want("provider")?
            .parse()
            .map_err(|e| format!("recording config: {e}"))?;
        if !provider.has_object_storage() {
            return Err(format!(
                "recording config names provider {provider}, which has no bucket to record to"
            ));
        }
        let retention: Retention = want("retention")?
            .parse()
            .map_err(|e| format!("recording config retention: {e}"))?;
        let stems = match flat_config_value(text, "stems") {
            None => false,
            Some("true") => true,
            Some("false") => false,
            Some(other) => return Err(format!("recording config stems is {other:?}")),
        };
        let decode = |key: &str, b64: &str| {
            BASE64
                .decode(b64.as_bytes())
                .map_err(|_| format!("recording config {key} is not base64"))
                .and_then(|bytes| {
                    String::from_utf8(bytes).map_err(|_| format!("{key} is not utf-8"))
                })
        };
        let credential = StorageCredential::KeyPair {
            access_key_id: want("access_key_id")?,
            secret_access_key: decode("secret_access_key_b64", &want("secret_access_key_b64")?)?,
        };
        Ok(RecordingStorage {
            provider,
            bucket: want("bucket")?,
            region: want("region")?,
            retention,
            credential,
            stems,
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
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
    /// Storage the VM records to; None means recording is off.
    pub recording: Option<RecordingStorage>,
}

/// Redacts the server's private key. Everything else here is either
/// public or a number, and the fields are worth seeing in a log.
impl fmt::Debug for BootConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootConfig")
            .field("artifact_url", &self.artifact_url)
            .field("artifact_sha256", &self.artifact_sha256)
            .field("server_private_key_b64", &"<redacted>")
            .field("issuer_public_key_b64", &self.issuer_public_key_b64)
            .field("session_id_hex", &self.session_id_hex)
            .field("port", &self.port)
            .field("idle_shutdown_min", &self.idle_shutdown_min)
            .field("max_duration_min", &self.max_duration_min)
            .field("self_destruct", &self.self_destruct)
            .field("recording", &self.recording)
            .finish()
    }
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

/// Reads one value back out of the flat key=value config. Lines are
/// trimmed first, so this also reads the config as it appears indented
/// inside the rendered cloud-init, which is how a provider recovers the
/// session's own shape from `LaunchSpec::user_data`. Best effort:
/// a payload in some other format simply yields None.
pub fn flat_config_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (k, v) = line.split_once('=')?;
        (k.trim() == key).then(|| v.trim())
    })
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
# No credential is attached to this instance, so it cannot delete itself.
# What ends it is maxRunDuration with instanceTerminationAction=DELETE,
# set at launch to the session's own hard cap, plus the host's sweeper,
# which runs on every app and CLI launch and gets there sooner.
#
# So this stops serving and nothing more. It must not power the VM off:
# Compute Engine clears the pending termination timestamp whenever a VM
# stops, so a powered-off instance outlives the cap that was supposed to
# collect it and keeps billing its disk until a human notices.
echo \"jamstream self-destruct: $1\" > /dev/console 2>/dev/null || true
systemctl stop jamstreamd.service mediamtx.service 2>/dev/null || true
"
        .to_owned(),
    }
}

fn guard_script(cfg: &BootConfig) -> String {
    guard_script_at(
        cfg,
        RUN_DIR,
        UPLOAD_MARKER_DIR,
        "/proc/uptime",
        "/usr/local/sbin/jamstream-self-destruct",
    )
}

/// [`guard_script`] with its paths as parameters, so the tests below can run
/// the script for real against a scratch directory on any OS.
fn guard_script_at(
    cfg: &BootConfig,
    state: &str,
    uploads: &str,
    uptime: &str,
    self_destruct: &str,
) -> String {
    format!(
        "#!/bin/sh
# Dead man's switch. jamstreamd touches {state}/{activity} while
# musicians are connected; staleness past the idle window, or exceeding the
# session hard cap, triggers self-destruct.
#
# Both windows are measured in uptime seconds, never wall clock. A cloud VM
# routinely boots with a wrong hardware clock and takes a large NTP step a
# minute later; a wall-clock idle window reads that step as a dead session
# and destroys a VM with musicians playing on it.
set -eu
up=$(cut -d. -f1 {uptime})
stamp=$(stat -c %Y {state}/{activity} 2>/dev/null || echo none)
# The mtime is only ever compared for equality, so no clock step can fake
# activity or hide it. What the idle window measures is the uptime at which
# the mtime last changed.
if [ \"$stamp\" != \"$(cat {state}/guard-stamp 2>/dev/null || echo none)\" ]; then
  printf '%s\\n' \"$stamp\" > {state}/guard-stamp
  printf '%s\\n' \"$up\" > {state}/guard-active-up
fi
active_up=$(cat {state}/guard-active-up 2>/dev/null || echo 0)
idle=$((up - active_up))
reason=\"\"
if [ \"$idle\" -ge {idle_secs} ]; then
  reason=\"idle for ${{idle}}s\"
fi
if [ \"$up\" -ge {max_secs} ]; then
  reason=\"max session duration reached\"
fi
if [ -z \"$reason\" ]; then
  rm -f {state}/guard-defer-up
  exit 0
fi
# A recording upload in flight (a marker file under {uploads}) defers
# destruction, but never past the ceiling: a stuck upload keeping a VM
# alive forever is a worse failure than a lost recording.
if [ -n \"$(ls -A {uploads} 2>/dev/null || true)\" ]; then
  if [ ! -f {state}/guard-defer-up ]; then
    printf '%s\\n' \"$up\" > {state}/guard-defer-up
  fi
  deferred=$((up - $(cat {state}/guard-defer-up)))
  if [ \"$deferred\" -lt {ceiling_secs} ]; then
    exit 0
  fi
  exec {self_destruct} \"$reason; upload still in flight after ${{deferred}}s, abandoning it to the lifecycle cleanup rule\"
fi
exec {self_destruct} \"$reason\"
",
        activity = ACTIVITY_FILE_NAME,
        idle_secs = cfg.idle_shutdown_min as u64 * 60,
        max_secs = cfg.max_duration_min as u64 * 60,
        ceiling_secs = UPLOAD_DEFER_CEILING_SECS,
    )
}

/// Packet filter for the session VM: only the session UDP port is
/// reachable, and only root may talk to the cloud metadata service.
///
/// This lives in its own script rather than inline in the bootstrap
/// because a reboot loses the rules while jamstreamd.service stays
/// enabled, so `jamstream-firewall.service` runs it again before the
/// server comes back up.
fn firewall_script(cfg: &BootConfig) -> String {
    format!(
        "#!/bin/sh
set -eu
# Flushing first makes this idempotent: bootstrap runs it once and the
# oneshot unit runs it again after every reboot.
for ipt in iptables ip6tables; do
  command -v \"$ipt\" >/dev/null 2>&1 || {{
    echo \"jamstream: no $ipt on this image, no in-guest packet filter\" >&2
    continue
  }}
  \"$ipt\" -F INPUT
  \"$ipt\" -F OUTPUT
  \"$ipt\" -A INPUT -i lo -j ACCEPT
  \"$ipt\" -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
  \"$ipt\" -A INPUT -p udp --dport {port} -j ACCEPT
  # ICMPv6 carries path MTU discovery and neighbour discovery; dropping it
  # breaks IPv6 rather than hardening it.
  if [ \"$ipt\" = ip6tables ]; then
    \"$ipt\" -A INPUT -p ipv6-icmp -j ACCEPT
  fi
  \"$ipt\" -P INPUT DROP
done
# The server private key and, on DigitalOcean, the API token arrive as
# user-data, which the metadata service hands to anything that asks over
# plain HTTP with no credential. jamstreamd runs as the unprivileged
# jamstream user, so restricting the metadata address to uid 0 puts those
# secrets out of reach of the process that parses untrusted UDP. cloud-init
# and the self-destruct script are root and keep their access.
for md in {metadata_v4} {metadata_v6}; do
  case \"$md\" in
    *:*) ipt=ip6tables ;;
    *) ipt=iptables ;;
  esac
  command -v \"$ipt\" >/dev/null 2>&1 || continue
  # The owner match needs xt_owner. Where it is missing the rule is
  # refused, and saying so beats silently leaving the door open.
  if \"$ipt\" -A OUTPUT -d \"$md\" -m owner --uid-owner 0 -j ACCEPT 2>/dev/null; then
    \"$ipt\" -A OUTPUT -d \"$md\" -j REJECT
  else
    echo \"jamstream: no owner match for $md, metadata stays world-readable\" >&2
  fi
done
",
        port = cfg.port,
        metadata_v4 = METADATA_V4,
        metadata_v6 = METADATA_V6,
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

fn bootstrap_script(_cfg: &BootConfig) -> String {
    format!(
        "#!/bin/sh
set -eu
# Everything below can fail: a GitHub 503 past --retry 5 is enough. A VM
# that cannot finish bootstrapping has no server and no way to be told to
# stop, so it bills until a human notices. This trap and the guard timer
# below are what make every other failure in this script survivable.
trap 'rc=$?; [ \"$rc\" -eq 0 ] || /usr/local/sbin/jamstream-self-destruct \"bootstrap failed with status $rc\"' EXIT

# jamstreamd parses unauthenticated UDP and hands the bytes to libopus, so
# it does not run as root. The config holds the server private key and stays
# unreadable to everything but root and this account.
id {user} >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin {user}
chgrp {user} {server_cfg}
chmod 0640 {server_cfg}
# The recording storage key, present only when the host turned recording
# on. Same handling as the config: root writes it, the service account
# reads it, nobody else sees it.
if [ -f {recording_cfg} ]; then
  chgrp {user} {recording_cfg}
  chmod 0640 {recording_cfg}
fi
install -d -o {user} -g {user} -m 0750 {run}
# jamstreamd updates this file's mtime while musicians are connected and the
# guard reads it, so the server has to own it.
install -o {user} -g {user} -m 0644 /dev/null {activity}
# Stream keys are staged here for the instant it takes to spawn a pusher.
# /run is tmpfs, so nothing reaches persistent disk.
install -d -o {user} -g {user} -m 0700 {keys}
# The recorder drops one marker file per in-flight upload; the guard defers
# self-destruct while any remain.
install -d -o {user} -g {user} -m 0700 {uploads}
# A pusher's ffmpeg receives its ingest URL on stdin, but execs with it in
# argv, so hide other processes' command lines from non-root. The VM has no
# other users; this is the belt.
mount -o remount,hidepid=2 /proc 2>/dev/null || true

systemctl daemon-reload
# The metadata lockdown in the firewall script rides on iptables, and a
# newer Debian cloud image may not ship it. Install it if missing, before
# the firewall unit runs: without this the script degrades to a warning
# and the API token in the metadata service stays readable by the process
# that parses untrusted UDP. Failure here still only warns, because the
# provider's own cloud firewall stands in front either way and a session
# beats a perfect one that never starts; the warning names what was lost.
if ! command -v iptables >/dev/null 2>&1; then
  if timeout 120 apt-get update -qq && \\
     timeout 120 apt-get install -y -qq --no-install-recommends iptables; then
    echo \"jamstream: installed iptables for the in-guest filter\"
  else
    echo \"jamstream: cannot install iptables; in-guest filter and metadata lockdown are OFF\" >&2
  fi
fi
# The firewall and the dead man's switch go in before anything that can
# fail. In the other order a failed download leaves a VM with provider
# default networking, no idle window, and no hard cap.
systemctl enable --now jamstream-firewall.service
systemctl enable --now jamstream-guard.timer

# The url and the expected hash are read from files rather than pasted
# into this script, so neither can be anything but an argument.
curl -fsSL --retry 5 -o {download} \"$(cat {artifact_url_file})\"
if ! sha256sum -c {artifact_sha_file}; then
  echo \"jamstream: artifact sha256 mismatch, refusing to start\" >&2
  rm -f {download}
  exit 1
fi
mv {download} /usr/local/bin/jamstreamd
chmod 0755 /usr/local/bin/jamstreamd
# A simple-type unit's start job succeeds at fork, so a binary that dies at
# exec (say, the wrong architecture) would let this script exit 0 and disarm
# the trap. Running it once here makes that death the script's own failure.
/usr/local/bin/jamstreamd --version >/dev/null
# The session server comes up first: musicians are waiting, and the broadcast
# tooling is only needed once the host goes live.
systemctl enable --now jamstreamd.service

# A session runs fine with no broadcast tooling, so a failed fetch warns
# instead of taking a working VM down with it.
if /usr/local/sbin/jamstream-media; then
  systemctl enable --now mediamtx.service
else
  echo \"jamstream: broadcast tooling unavailable, session continues without it\" >&2
fi
",
        artifact_url_file = ARTIFACT_URL_FILE,
        artifact_sha_file = ARTIFACT_SHA_FILE,
        download = ARTIFACT_DOWNLOAD,
        user = SERVICE_USER,
        run = RUN_DIR,
        activity = ACTIVITY_FILE,
        keys = STREAM_KEY_DIR,
        uploads = UPLOAD_MARKER_DIR,
        server_cfg = SERVER_CONFIG_PATH,
        recording_cfg = RECORDING_CONFIG_PATH,
    )
}

/// Fetches the pinned broadcast subprocesses. Separate from the bootstrap
/// because the bootstrap runs it as an `if` condition, and a shell ignores
/// `set -e` inside a condition: as its own process it still aborts on the
/// first failed download.
fn media_script() -> String {
    let media = media_artifacts();
    format!(
        "#!/bin/sh
set -eu
tmp=$(mktemp -d)
trap 'rm -rf \"$tmp\"' EXIT
{ffmpeg}{mediamtx}",
        ffmpeg = fetch_media_tool("ffmpeg", &media.ffmpeg),
        mediamtx = fetch_media_tool("mediamtx", &media.mediamtx),
    )
}

/// Hardening common to both long-running services. jamstreamd is the one
/// that matters (it is the process reachable from the internet), and
/// MediaMTX gets the same treatment because leaving it as root while
/// hardening its sibling protects nothing.
const SERVICE_HARDENING: &str = "NoNewPrivileges=yes
CapabilityBoundingSet=
AmbientCapabilities=
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
RestrictNamespaces=yes
LockPersonality=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service
LimitCORE=0
";

fn jamstreamd_unit(cfg: &BootConfig) -> String {
    format!(
        "[Unit]
Description=JamStream session server
After=network-online.target jamstream-firewall.service
Wants=network-online.target
Requires=jamstream-firewall.service

[Service]
User={user}
Group={user}
# The guard is the cap that actually destroys the VM. These two make the
# server stop serving on its own if the guard ever stops running.
ExecStart=/usr/local/bin/jamstreamd --config {server_cfg} \
--idle-exit-min {idle_min} --max-duration-min {max_min}
Restart=on-failure
RestartSec=2
ReadWritePaths={run}
# The encoder and one RTMP pusher per destination are children in this
# cgroup, on a 2 GB instance shared with MediaMTX and the OS. A memory
# exhaustion bug in the packet path costs a restart instead of the session.
MemoryMax=1500M
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
{hardening}
[Install]
WantedBy=multi-user.target
",
        user = SERVICE_USER,
        run = RUN_DIR,
        server_cfg = SERVER_CONFIG_PATH,
        idle_min = cfg.idle_shutdown_min,
        max_min = cfg.max_duration_min,
        hardening = SERVICE_HARDENING,
    )
}

/// The relay the encoder publishes to and the pushers read from. Bound to
/// loopback and stripped to one protocol and one path. The loopback bind is
/// what keeps it off the internet: the packet filter is a second gate that
/// is not up during early boot, and it is not there to make a listener on
/// 0.0.0.0 safe.
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

fn mediamtx_unit() -> String {
    format!(
        "[Unit]
Description=JamStream broadcast relay (MediaMTX)
After=network-online.target jamstream-firewall.service
Wants=network-online.target
Requires=jamstream-firewall.service

[Service]
User={user}
Group={user}
ExecStart=/usr/local/bin/mediamtx /etc/jamstream/mediamtx.yml
Restart=on-failure
RestartSec=2
MemoryMax=256M
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
{hardening}
[Install]
WantedBy=multi-user.target
",
        user = SERVICE_USER,
        hardening = SERVICE_HARDENING,
    )
}

const FIREWALL_UNIT: &str = "[Unit]
Description=JamStream host firewall
# A reboot loses the rules while jamstreamd.service stays enabled, so they
# go back in before the network and before the server.
DefaultDependencies=no
After=local-fs.target
Before=network-pre.target jamstreamd.service mediamtx.service shutdown.target
Wants=network-pre.target
Conflicts=shutdown.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/jamstream-firewall

[Install]
WantedBy=multi-user.target
";

const GUARD_UNIT: &str = "[Unit]
Description=JamStream dead man's switch
# A guard that cannot run is a VM with no cap on it at all, so a failed run
# destroys the VM rather than logging and shrugging.
OnFailure=jamstream-self-destruct.service

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

const SELF_DESTRUCT_UNIT: &str = "[Unit]
Description=JamStream self-destruct

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/jamstream-self-destruct \"dead man's switch failed\"
";

pub fn render(cfg: &BootConfig) -> String {
    let mut files: Vec<(&str, &str, String)> = vec![
        // Written root-only; the bootstrap hands the group to the service
        // account once that account exists.
        (SERVER_CONFIG_PATH, "0600", cfg.render_flat_config()),
        (ARTIFACT_URL_FILE, "0644", format!("{}\n", cfg.artifact_url)),
        (
            ARTIFACT_SHA_FILE,
            "0644",
            format!("{}  {ARTIFACT_DOWNLOAD}\n", cfg.artifact_sha256),
        ),
        (
            "/usr/local/sbin/jamstream-self-destruct",
            "0700",
            self_destruct_script(cfg),
        ),
        ("/usr/local/sbin/jamstream-guard", "0700", guard_script(cfg)),
        (
            "/usr/local/sbin/jamstream-firewall",
            "0700",
            firewall_script(cfg),
        ),
        ("/usr/local/sbin/jamstream-media", "0700", media_script()),
        (
            "/usr/local/sbin/jamstream-bootstrap",
            "0700",
            bootstrap_script(cfg),
        ),
        (
            "/etc/systemd/system/jamstreamd.service",
            "0644",
            jamstreamd_unit(cfg),
        ),
        (
            "/etc/systemd/system/jamstream-firewall.service",
            "0644",
            FIREWALL_UNIT.to_owned(),
        ),
        (
            "/etc/systemd/system/jamstream-guard.service",
            "0644",
            GUARD_UNIT.to_owned(),
        ),
        (
            "/etc/systemd/system/jamstream-guard.timer",
            "0644",
            GUARD_TIMER.to_owned(),
        ),
        (
            "/etc/systemd/system/jamstream-self-destruct.service",
            "0644",
            SELF_DESTRUCT_UNIT.to_owned(),
        ),
        (
            "/etc/jamstream/mediamtx.yml",
            "0644",
            MEDIAMTX_CONFIG.to_owned(),
        ),
        (
            "/etc/systemd/system/mediamtx.service",
            "0644",
            mediamtx_unit(),
        ),
    ];
    if let Some(recording) = &cfg.recording {
        // Root-only like the config; the bootstrap chgrps it.
        files.push((
            RECORDING_CONFIG_PATH,
            "0600",
            recording.render_flat_config(),
        ));
    }

    let mut out = String::from("#cloud-config\nwrite_files:\n");
    for (path, mode, content) in files {
        let _ = write!(
            out,
            "  - path: {path}\n    owner: root:root\n    permissions: \"{mode}\"\n    content: |\n{}",
            indent(&content, 6)
        );
    }
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
            recording: None,
        }
    }

    fn recording_storage() -> RecordingStorage {
        RecordingStorage {
            provider: ProviderKind::Aws,
            bucket: "my-jams".to_owned(),
            region: "us-east-1".to_owned(),
            retention: Retention::default(),
            credential: StorageCredential::KeyPair {
                access_key_id: "AKIDRECORD".to_owned(),
                secret_access_key: "record-secret".to_owned(),
            },
            stems: true,
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

    /// #51: the idle path used to fetch a service account token from
    /// metadata, and no service account was ever attached, so the fetch
    /// failed and the script fell through to `poweroff`. Compute Engine
    /// clears a stopped VM's termination timestamp, so that left an
    /// instance nothing would ever collect.
    #[test]
    fn snapshot_gcp_max_run_duration() {
        let out = render(&base_config(SelfDestruct::GcpMaxRunDuration));
        check_snapshot("cloudinit_gcp_max_run_duration.yaml", &out);
        assert!(!out.contains("poweroff"), "a stopped VM outlives its cap");
        assert!(
            !out.contains("service-accounts"),
            "nothing on the instance can authenticate, so nothing should try"
        );
        assert!(!out.contains("Authorization: Bearer"));
        assert!(out.contains("systemctl stop jamstreamd.service"));
    }

    /// Every self-destruct variant has to actually end the instance, or
    /// stand aside for the thing that does. Neither shutdown nor poweroff
    /// qualifies on GCP.
    #[test]
    fn no_variant_leaves_an_instance_that_bills_forever() {
        for sd in all_variants() {
            let script = self_destruct_script(&base_config(sd.clone()));
            match sd {
                // Instance-initiated shutdown behavior is terminate.
                SelfDestruct::AwsShutdown => assert!(script.contains("shutdown -h now")),
                // Powered-off droplets bill, so deletion is the end state.
                SelfDestruct::ApiToken { .. } => {
                    assert!(script.contains("-X DELETE"));
                }
                SelfDestruct::GcpMaxRunDuration => {
                    assert!(!script.contains("poweroff") && !script.contains("shutdown"));
                }
            }
        }
    }

    /// The flat config is how the session's own shape reaches a provider
    /// that otherwise sees nothing but `LaunchSpec::user_data`, so it has
    /// to read back the same from either rendering: bare, or indented
    /// inside the YAML.
    #[test]
    fn flat_config_reads_back_from_both_renderings() {
        let cfg = base_config(SelfDestruct::GcpMaxRunDuration);
        for text in [cfg.render_flat_config(), render(&cfg)] {
            assert_eq!(flat_config_value(&text, "max_duration_min"), Some("720"));
            assert_eq!(flat_config_value(&text, "idle_shutdown_min"), Some("10"));
            assert_eq!(flat_config_value(&text, "port"), Some("43210"));
            assert_eq!(flat_config_value(&text, "missing"), None);
        }
        assert_eq!(flat_config_value("# port = 1\n", "port"), None);
        assert_eq!(flat_config_value("#cloud-config\n", "port"), None);
    }

    /// Rust cannot concatenate two `&'static str` constants in a const, so
    /// the tmpfs paths spell out a directory that is also its own constant.
    /// Two spellings of one path is the defect these constants exist to
    /// remove, so the relationship is asserted rather than assumed.
    #[test]
    fn every_run_dir_path_is_under_the_run_dir() {
        assert_eq!(ACTIVITY_FILE, format!("{RUN_DIR}/{ACTIVITY_FILE_NAME}"));
        assert_eq!(STREAM_KEY_DIR, format!("{RUN_DIR}/keys"));
        assert_eq!(UPLOAD_MARKER_DIR, format!("{RUN_DIR}/uploads"));
        // The unit grants write access to RUN_DIR alone, so anything outside
        // it is a path the hardened service cannot create.
        for path in [ACTIVITY_FILE, STREAM_KEY_DIR, UPLOAD_MARKER_DIR] {
            assert!(path.starts_with(RUN_DIR), "{path} escapes {RUN_DIR}");
        }
        assert!(RECORDING_CONFIG_PATH.starts_with("/etc/jamstream/"));
        assert!(SERVER_CONFIG_PATH.starts_with("/etc/jamstream/"));
    }

    /// The bootstrap script runs as root, and the artifact pair is the one
    /// part of it a caller supplies. Neither value is pasted into the
    /// script now: both are written to files and read back, so the worst a
    /// hostile pair can be is a url that does not resolve.
    #[test]
    fn the_artifact_pair_is_data_not_script() {
        let mut cfg = base_config(SelfDestruct::AwsShutdown);
        cfg.artifact_url = "https://example.invalid/a\";touch /tmp/pwned;\"".to_owned();
        cfg.artifact_sha256 = "$(id > /tmp/pwned)".to_owned();
        let script = bootstrap_script(&cfg);
        assert!(
            !script.contains("pwned"),
            "the pair reached the script body: {script}"
        );
        assert!(script.contains(
            "-o /usr/local/bin/jamstreamd.download \"$(cat /etc/jamstream/artifact-url)\""
        ));
        assert!(script.contains("sha256sum -c /etc/jamstream/artifact-sha256"));

        // They travel as files instead, the hash in the format
        // `sha256sum -c` reads, naming the path the download lands at.
        let out = render(&base_config(SelfDestruct::AwsShutdown));
        assert!(out.contains("path: /etc/jamstream/artifact-url"));
        assert!(out.contains("path: /etc/jamstream/artifact-sha256"));
        assert!(out.contains(
            "0f2e5c1d3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d  \
             /usr/local/bin/jamstreamd.download"
        ));
    }

    #[test]
    fn snapshot_aws_with_recording() {
        let mut cfg = base_config(SelfDestruct::AwsShutdown);
        cfg.recording = Some(recording_storage());
        let out = render(&cfg);
        check_snapshot("cloudinit_aws_recording.yaml", &out);
        assert!(out.contains("path: /etc/jamstream/recording"));
    }

    /// The provider caps this document can never cross. EC2 refuses user
    /// data past 16384 bytes, which rejected every AWS launch the day the
    /// rendered cloud-init reached about 17 KB; the AWS provider now gzips
    /// it, so what must fit is the compressed form, with headroom because
    /// the failure mode is a launch error in a musician's face. Raw size is
    /// held under DigitalOcean's 64 KB for the same reason.
    #[test]
    fn the_rendered_cloudinit_fits_every_providers_cap() {
        use std::io::Write;
        for sd in all_variants() {
            let mut cfg = base_config(sd);
            cfg.recording = Some(recording_storage());
            let out = render(&cfg);
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            enc.write_all(out.as_bytes()).unwrap();
            let gz = enc.finish().unwrap().len();
            assert!(
                gz < 12_288,
                "gzipped cloud-init is {gz} bytes; EC2 refuses past 16384 and \
                 this floor keeps a quarter of it in reserve"
            );
            assert!(
                out.len() < 49_152,
                "raw cloud-init is {} bytes; DigitalOcean refuses past 65536",
                out.len()
            );
        }
    }

    /// The storage key is a secret: it travels as a root-owned file the
    /// bootstrap chgrps to the service account, and it must never appear on
    /// argv, in a script body, or anywhere twice.
    #[test]
    fn recording_key_is_a_root_file_never_argv_or_script() {
        let mut cfg = base_config(SelfDestruct::AwsShutdown);
        cfg.recording = Some(recording_storage());
        let out = render(&cfg);

        assert!(out.contains(
            "path: /etc/jamstream/recording\n    owner: root:root\n    permissions: \"0600\""
        ));
        // The plaintext secret is nowhere; its base64 form appears exactly
        // once, inside the config file content.
        let secret_b64 = BASE64.encode(b"record-secret");
        assert!(!out.contains("record-secret"));
        assert_eq!(out.matches(secret_b64.as_str()).count(), 1);
        assert_eq!(out.matches("AKIDRECORD").count(), 1);
        // No script interpolates it: every script body is checked, not just
        // the ones that exist today.
        for script in [
            self_destruct_script(&cfg),
            guard_script(&cfg),
            firewall_script(&cfg),
            media_script(),
            bootstrap_script(&cfg),
        ] {
            assert!(!script.contains("AKIDRECORD") && !script.contains(&secret_b64));
        }
        // The bootstrap locks the file down exactly like the server config.
        let script = bootstrap_script(&cfg);
        assert!(script.contains("if [ -f /etc/jamstream/recording ]; then"));
        assert!(script.contains("chgrp jamstream /etc/jamstream/recording"));
        assert!(script.contains("chmod 0640 /etc/jamstream/recording"));

        // No recording, no file.
        let out = render(&base_config(SelfDestruct::AwsShutdown));
        assert!(!out.contains("path: /etc/jamstream/recording"));
    }

    /// The VM reads the recording config back with [`flat_config_value`],
    /// so both credential shapes have to round trip through the flat format.
    #[test]
    fn recording_flat_config_round_trips() {
        let text = recording_storage().render_flat_config();
        // The structured parse is what the VM runs; it must reproduce the
        // exact value that was rendered, both credential shapes.
        assert_eq!(
            RecordingStorage::parse_flat_config(&text).unwrap(),
            recording_storage()
        );
        // A config written before the stems key reads as stems off, not as
        // an error: the file on a machine outlives the code that wrote it.
        let legacy: String = text
            .lines()
            .filter(|l| !l.starts_with("stems"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(!RecordingStorage::parse_flat_config(&legacy).unwrap().stems);
        assert_eq!(flat_config_value(&text, "provider"), Some("aws"));
        assert_eq!(flat_config_value(&text, "bucket"), Some("my-jams"));
        assert_eq!(flat_config_value(&text, "region"), Some("us-east-1"));
        assert_eq!(flat_config_value(&text, "retention"), Some("30d"));
        assert_eq!(
            flat_config_value(&text, "access_key_id"),
            Some("AKIDRECORD")
        );
        let secret = flat_config_value(&text, "secret_access_key_b64").unwrap();
        assert_eq!(BASE64.decode(secret.as_bytes()).unwrap(), b"record-secret");

        // GCS takes the same HMAC pair through its interop endpoint, so the
        // file has one credential shape for all three providers.
        let gcp = RecordingStorage {
            provider: ProviderKind::Gcp,
            ..recording_storage()
        };
        let text = gcp.render_flat_config();
        assert_eq!(flat_config_value(&text, "provider"), Some("gcp"));
        assert_eq!(RecordingStorage::parse_flat_config(&text).unwrap(), gcp);
    }

    #[test]
    fn a_recording_config_naming_a_bucketless_provider_is_refused() {
        let swap = |name: &str| {
            recording_storage()
                .render_flat_config()
                .replace("provider = aws", &format!("provider = {name}"))
        };
        // Local records to the host's own disk, so a config that names it
        // never came from a launch this code path can serve.
        let err = RecordingStorage::parse_flat_config(&swap("local")).unwrap_err();
        assert!(err.contains("local"), "{err}");
        assert!(err.contains("no bucket"), "{err}");
        // An unknown name says what the known ones are, once, from
        // ProviderKind.
        let err = RecordingStorage::parse_flat_config(&swap("azure")).unwrap_err();
        assert!(err.contains("azure"), "{err}");
        assert!(err.contains("digitalocean"), "{err}");
    }

    #[test]
    fn recording_debug_redacts_the_secret() {
        let mut cfg = base_config(SelfDestruct::AwsShutdown);
        cfg.recording = Some(recording_storage());
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("record-secret"));
        assert!(rendered.contains("AKIDRECORD"), "the key id is loggable");
        assert!(rendered.contains("my-jams"));
    }

    /// The three providers all redact their credentials from Debug and
    /// have a test saying so; the boot config and the self-destruct spec
    /// hold the same class of secret and were missed.
    #[test]
    fn debug_never_reveals_a_key_or_a_token() {
        let cfg = BootConfig {
            self_destruct: SelfDestruct::ApiToken {
                endpoint: "https://api.digitalocean.com/v2/droplets".to_owned(),
                token: "dop_v1_supersecret".to_owned(),
            },
            ..base_config(SelfDestruct::AwsShutdown)
        };
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("dop_v1_supersecret"));
        assert!(!rendered.contains(&cfg.server_private_key_b64));
        assert_eq!(rendered.matches("<redacted>").count(), 2);
        // What is left is the part worth having in a log.
        assert!(rendered.contains("api.digitalocean.com"));
        assert!(rendered.contains("deadbeefcafef00d"));
        assert!(rendered.contains("43210"));
        // The variants with nothing to hide print plainly.
        assert_eq!(format!("{:?}", SelfDestruct::AwsShutdown), "AwsShutdown");
        assert_eq!(
            format!("{:?}", SelfDestruct::GcpMaxRunDuration),
            "GcpMaxRunDuration"
        );
    }

    /// The three self-destruct variants, for tests that must hold for all
    /// of them.
    fn all_variants() -> [SelfDestruct; 3] {
        [
            SelfDestruct::AwsShutdown,
            SelfDestruct::ApiToken {
                endpoint: "https://api.digitalocean.com/v2/droplets".to_owned(),
                token: "t".to_owned(),
            },
            SelfDestruct::GcpMaxRunDuration,
        ]
    }

    /// Byte offset of `needle`, or a panic naming what was missing.
    fn at(out: &str, needle: &str) -> usize {
        out.find(needle)
            .unwrap_or_else(|| panic!("rendered user-data has no {needle:?}"))
    }

    #[test]
    fn rendered_invariants() {
        for sd in all_variants() {
            let cfg = base_config(sd);
            let out = render(&cfg);
            assert!(out.starts_with("#cloud-config\n"));
            // Secrets file is written root-only, then handed to the service
            // account's group and nobody else.
            assert!(out.contains(
                "path: /etc/jamstream/config\n    owner: root:root\n    permissions: \"0600\""
            ));
            assert!(out.contains("chgrp jamstream /etc/jamstream/config"));
            assert!(out.contains("chmod 0640 /etc/jamstream/config"));
            // Refuses to start on artifact hash mismatch.
            assert!(out.contains("sha256sum -c /etc/jamstream/artifact-sha256"));
            assert!(out.contains("refusing to start"));
            assert!(out.contains(&cfg.artifact_sha256));
            // Only the session UDP port is reachable, over both families.
            assert!(out.contains("-A INPUT -p udp --dport 43210 -j ACCEPT"));
            assert!(out.contains("for ipt in iptables ip6tables; do"));
            assert!(out.contains("\"$ipt\" -P INPUT DROP"));
            // Guard thresholds in seconds, plus the upload deferral ceiling.
            assert!(out.contains("-ge 600 ]"));
            assert!(out.contains("-ge 43200 ]"));
            assert!(out.contains("-lt 600 ]"));
            assert!(out.contains("systemctl enable --now jamstreamd.service"));
            assert!(out.contains("systemctl enable --now jamstream-guard.timer"));
            // The marker directory the recorder writes and the guard reads.
            assert!(
                out.contains("install -d -o jamstream -g jamstream -m 0700 /run/jamstream/uploads")
            );
            assert!(out.contains("ls -A /run/jamstream/uploads"));
        }
    }

    /// The ordering defect this test exists for: the firewall and the dead
    /// man's switch used to be the last two steps of the bootstrap, so any
    /// earlier failure (a checksum mismatch, a GitHub 503 past --retry 5)
    /// left a VM with provider default networking and no cap on its life.
    #[test]
    fn guard_and_firewall_are_armed_before_anything_can_fail() {
        for sd in all_variants() {
            let cfg = base_config(sd);
            // Order is a property of the bootstrap script, not of the order
            // cloud-init happens to write the files in.
            let script = bootstrap_script(&cfg);
            let trap = at(&script, "trap 'rc=$?;");
            let firewall = at(&script, "systemctl enable --now jamstream-firewall.service");
            let guard = at(&script, "systemctl enable --now jamstream-guard.timer");
            let download = at(&script, "-o /usr/local/bin/jamstreamd.download");
            let media = at(&script, "/usr/local/sbin/jamstream-media");

            assert!(trap < firewall, "the trap must cover the firewall step too");
            assert!(
                firewall < download && guard < download,
                "firewall and guard must be installed before the download"
            );
            assert!(download < media, "jamstreamd downloads before the encoder");
            // A failed bootstrap ends in a destroyed VM, not an idle bill.
            let out = render(&cfg);
            assert!(out.contains("jamstream-self-destruct \"bootstrap failed with status $rc\""));
            // The broadcast tools are the one part a session can live
            // without, so their failure must not trip the trap.
            assert!(out.contains("session continues without it"));
        }
    }

    /// The metadata lockdown rides on iptables and a newer Debian cloud
    /// image may not ship it. The bootstrap installs it when missing,
    /// before the firewall unit runs, and an install failure warns rather
    /// than destroying the machine: the provider's cloud firewall stands
    /// in front either way.
    #[test]
    fn a_missing_iptables_is_installed_before_the_firewall_runs() {
        for sd in all_variants() {
            let script = bootstrap_script(&base_config(sd));
            let check = at(&script, "if ! command -v iptables");
            let install = at(
                &script,
                "apt-get install -y -qq --no-install-recommends iptables",
            );
            let firewall = at(&script, "systemctl enable --now jamstream-firewall.service");
            assert!(
                check < install && install < firewall,
                "install iptables, if needed, before the unit that uses it"
            );
            // Bounded: a hung mirror must not stall the bootstrap forever
            // with the trap armed behind it.
            assert!(script.contains("timeout 120 apt-get"));
            // And survivable: the whole attempt sits in a condition, so a
            // failed install warns instead of tripping the fail-closed trap.
            assert!(script.contains("in-guest filter and metadata lockdown are OFF"));
        }
    }

    /// #139: `systemctl enable --now` on a simple-type unit reports success
    /// at fork, so a downloaded binary that dies at exec (an x86_64 build on
    /// a Graviton machine) used to leave the bootstrap exiting 0, the trap
    /// disarmed, and the VM billing with no server. The script must run the
    /// binary itself, after the hash check and before the unit is enabled,
    /// so an exec failure trips the trap and destroys the machine.
    #[test]
    fn the_binary_must_execute_before_the_unit_is_enabled() {
        for sd in all_variants() {
            let script = bootstrap_script(&base_config(sd));
            let sha = at(&script, "sha256sum -c /etc/jamstream/artifact-sha256");
            let exec = at(&script, "/usr/local/bin/jamstreamd --version >/dev/null");
            let enable = at(&script, "systemctl enable --now jamstreamd.service");
            assert!(
                sha < exec,
                "the binary must not run before its hash is proven"
            );
            assert!(
                exec < enable,
                "the exec check must come before the unit hides the failure"
            );
        }
    }

    /// #42: the process that parses unauthenticated UDP does not run as
    /// root, cannot gain privileges, and cannot eat the whole instance.
    #[test]
    fn jamstreamd_runs_unprivileged_and_confined() {
        let out = render(&base_config(SelfDestruct::AwsShutdown));
        assert!(out.contains("useradd --system --no-create-home"));
        for setting in [
            "User=jamstream",
            "NoNewPrivileges=yes",
            "CapabilityBoundingSet=\n",
            "ProtectSystem=strict",
            "PrivateTmp=yes",
            "SystemCallFilter=@system-service",
            "MemoryMax=1500M",
            "LimitCORE=0",
            "ReadWritePaths=/run/jamstream",
        ] {
            assert!(out.contains(setting), "jamstreamd unit lacks {setting}");
        }
        // The in-process windows match the guard's, so a dead guard still
        // ends with the server refusing to serve.
        assert!(out.contains("--idle-exit-min 10 --max-duration-min 720"));
        // ProtectSystem=strict would make the tmpfs work directory
        // read-only without this, and the guard reads the file the server
        // has to be able to touch.
        assert!(out.contains(
            "install -o jamstream -g jamstream -m 0644 /dev/null /run/jamstream/last-active"
        ));
    }

    /// #52: neither window may be derived from the wall clock. A VM whose
    /// hardware clock is wrong at boot takes an NTP step minutes later, and
    /// a wall-clock idle window reads that step as an empty session.
    #[test]
    fn guard_windows_are_uptime_derived() {
        let out = render(&base_config(SelfDestruct::GcpMaxRunDuration));
        assert!(!out.contains("date +%s"), "guard is back on the wall clock");
        assert!(out.contains("up=$(cut -d. -f1 /proc/uptime)"));
        assert!(out.contains("idle=$((up - active_up))"));
        // The mtime is compared for equality only, never subtracted.
        assert!(out.contains("stamp=$(stat -c %Y /run/jamstream/last-active"));
        assert!(!out.contains("now - last"));
        // A guard that cannot run destroys the VM instead of logging.
        assert!(out.contains("OnFailure=jamstream-self-destruct.service"));
        assert!(out.contains("path: /etc/systemd/system/jamstream-self-destruct.service"));
    }

    /// Runs the rendered guard script for real against a scratch directory:
    /// a fake uptime file stands in for /proc/uptime and a stub self-destruct
    /// records the reason it was invoked with. What the snapshots cannot
    /// prove, this does.
    #[cfg(unix)]
    mod guard_behavior {
        use super::*;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        use std::process::Command;

        struct Guard {
            dir: PathBuf,
        }

        impl Guard {
            fn new(name: &str) -> Guard {
                let dir = std::env::temp_dir()
                    .join(format!("jamstream-guard-{}-{name}", std::process::id()));
                let _ = fs::remove_dir_all(&dir);
                let state = dir.join("state");
                let uploads = dir.join("uploads");
                fs::create_dir_all(&state).unwrap();
                fs::create_dir_all(&uploads).unwrap();
                let stub = dir.join("self-destruct");
                fs::write(
                    &stub,
                    format!(
                        "#!/bin/sh\nprintf '%s\\n' \"$1\" > {}/destroyed\n",
                        dir.display()
                    ),
                )
                .unwrap();
                fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
                let script = guard_script_at(
                    &base_config(SelfDestruct::AwsShutdown),
                    &state.display().to_string(),
                    &uploads.display().to_string(),
                    &dir.join("uptime").display().to_string(),
                    &stub.display().to_string(),
                );
                fs::write(dir.join("guard"), script).unwrap();
                Guard { dir }
            }

            fn state(&self) -> PathBuf {
                self.dir.join("state")
            }

            fn uploads(&self) -> PathBuf {
                self.dir.join("uploads")
            }

            fn seed(&self, file: &str, content: &str) {
                fs::write(self.state().join(file), content).unwrap();
            }

            fn marker(&self) {
                fs::write(self.uploads().join("mix.flac"), "").unwrap();
            }

            /// One guard tick at `up` uptime seconds: (clean exit, the
            /// self-destruct reason if it fired).
            fn run(&self, up: u64) -> (bool, Option<String>) {
                let _ = fs::remove_file(self.dir.join("destroyed"));
                // /proc/uptime shape: seconds.hundredths, twice.
                fs::write(self.dir.join("uptime"), format!("{up}.55 {up}.00\n")).unwrap();
                let status = Command::new("sh")
                    .arg(self.dir.join("guard"))
                    .status()
                    .expect("run guard under sh");
                let destroyed = fs::read_to_string(self.dir.join("destroyed"))
                    .ok()
                    .map(|s| s.trim().to_owned());
                (status.success(), destroyed)
            }
        }

        #[test]
        fn an_active_session_lives() {
            let g = Guard::new("active");
            assert_eq!(g.run(100), (true, None));
            assert_eq!(g.run(599), (true, None));
        }

        #[test]
        fn an_idle_session_without_uploads_is_destroyed() {
            let g = Guard::new("idle");
            let (ok, destroyed) = g.run(700);
            assert!(ok);
            let reason = destroyed.expect("idle past the window must destroy");
            assert_eq!(reason, "idle for 700s");
        }

        #[test]
        fn an_upload_in_flight_defers_destruction_up_to_the_ceiling() {
            let g = Guard::new("defer");
            g.marker();
            // Overdue, but an upload is in flight: a clean exit, and the
            // deferral start is recorded in uptime seconds.
            assert_eq!(g.run(700), (true, None));
            assert_eq!(
                fs::read_to_string(g.state().join("guard-defer-up")).unwrap(),
                "700\n"
            );
            // Still inside the ceiling.
            assert_eq!(g.run(1250), (true, None));
            // Past it: destroyed, and the reason says what was abandoned.
            let (ok, destroyed) = g.run(1301);
            assert!(ok);
            let reason = destroyed.expect("the ceiling must end the deferral");
            assert!(reason.contains("idle for 1301s"), "{reason}");
            assert!(
                reason.contains("upload still in flight after 601s"),
                "{reason}"
            );
            assert!(reason.contains("abandoning"), "{reason}");
        }

        #[test]
        fn a_finished_upload_ends_the_deferral() {
            let g = Guard::new("finished");
            g.marker();
            assert_eq!(g.run(700), (true, None));
            fs::remove_file(g.uploads().join("mix.flac")).unwrap();
            let (_, destroyed) = g.run(1250);
            let reason = destroyed.expect("no marker, no mercy");
            assert_eq!(reason, "idle for 1250s");
        }

        #[test]
        fn resumed_activity_clears_the_deferral_bookkeeping() {
            let g = Guard::new("resumed");
            g.marker();
            assert_eq!(g.run(700), (true, None));
            // A changed activity stamp resets the idle window; the stale
            // deferral must not survive to shorten a later one.
            g.seed("guard-stamp", "stale\n");
            assert_eq!(g.run(1200), (true, None));
            assert!(!g.state().join("guard-defer-up").exists());
        }

        #[test]
        fn the_hard_cap_is_deferred_but_bounded_too() {
            let g = Guard::new("cap");
            // Recent activity, but past the session hard cap.
            g.seed("guard-active-up", "43100\n");
            g.marker();
            assert_eq!(g.run(43200), (true, None));
            let (_, destroyed) = g.run(43801);
            let reason = destroyed.expect("the cap must win in the end");
            assert!(reason.contains("max session duration reached"), "{reason}");
            assert!(reason.contains("upload still in flight"), "{reason}");
        }

        #[test]
        fn an_empty_or_missing_marker_dir_does_not_defer() {
            let g = Guard::new("empty-dir");
            // Empty directory: no deferral.
            let (_, destroyed) = g.run(700);
            assert!(destroyed.is_some());
            // Missing directory reads as absent, not as an error.
            fs::remove_dir_all(g.uploads()).unwrap();
            let (ok, destroyed) = g.run(700);
            assert!(ok);
            assert!(destroyed.is_some());
        }

        /// Corrupt bookkeeping must never keep the VM alive: a defer stamp
        /// that is not a number makes the tick fail, and a failed tick is
        /// destruction by the OnFailure= on the guard unit.
        #[test]
        fn corrupt_deferral_bookkeeping_fails_closed() {
            let g = Guard::new("corrupt");
            g.marker();
            g.seed("guard-defer-up", "12abc\n");
            let (ok, destroyed) = g.run(700);
            assert!(!ok, "a broken tick must not exit 0");
            assert!(destroyed.is_none());
        }

        /// Same fail-closed shape when the state dir cannot be written.
        #[test]
        fn an_unwritable_state_dir_fails_closed() {
            let g = Guard::new("readonly");
            if fs::metadata(&g.dir)
                .map(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0)
                .unwrap_or(false)
            {
                // root ignores directory modes; nothing to prove here.
                return;
            }
            // Force the write path, then take the permission away.
            g.seed("guard-stamp", "different\n");
            fs::set_permissions(g.state(), fs::Permissions::from_mode(0o500)).unwrap();
            let (ok, destroyed) = g.run(100);
            fs::set_permissions(g.state(), fs::Permissions::from_mode(0o755)).unwrap();
            assert!(!ok, "a tick that cannot keep its books must not exit 0");
            assert!(destroyed.is_none());
        }
    }

    /// #41, the half of it that lives in the guest: user-data holds the
    /// server private key and, on DigitalOcean, an account API token, and
    /// the metadata service hands user-data to any process that asks.
    #[test]
    fn metadata_service_is_root_only() {
        for sd in all_variants() {
            let out = render(&base_config(sd));
            assert!(out.contains("-d \"$md\" -m owner --uid-owner 0 -j ACCEPT"));
            assert!(out.contains("-d \"$md\" -j REJECT"));
            assert!(out.contains("for md in 169.254.169.254 fd00:ec2::254; do"));
        }
    }

    /// Nothing else here catches a shell syntax error, and a broken
    /// bootstrap script on the VM shows up as a session that never answers.
    /// `sh -n` parses without executing; the runners that have a shell run
    /// this, which is every OS in the matrix except Windows.
    #[cfg(unix)]
    #[test]
    fn rendered_scripts_parse_as_posix_shell() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        for sd in all_variants() {
            let cfg = base_config(sd);
            let scripts = [
                ("self-destruct", self_destruct_script(&cfg)),
                ("guard", guard_script(&cfg)),
                ("firewall", firewall_script(&cfg)),
                ("media", media_script()),
                ("bootstrap", bootstrap_script(&cfg)),
            ];
            for (name, script) in scripts {
                let mut child = Command::new("sh")
                    .arg("-n")
                    .stdin(Stdio::piped())
                    .spawn()
                    .expect("spawn sh");
                child
                    .stdin
                    .take()
                    .expect("sh stdin")
                    .write_all(script.as_bytes())
                    .expect("write script");
                let status = child.wait().expect("wait for sh");
                assert!(status.success(), "{name} script is not valid shell");
            }
        }
    }

    /// The rules go back in after a reboot, which the old inline iptables
    /// block never did: nothing persisted them and jamstreamd.service came
    /// straight back up.
    #[test]
    fn firewall_survives_a_reboot() {
        let out = render(&base_config(SelfDestruct::AwsShutdown));
        assert!(out.contains("path: /usr/local/sbin/jamstream-firewall"));
        assert!(out.contains("path: /etc/systemd/system/jamstream-firewall.service"));
        assert!(out.contains("Before=network-pre.target jamstreamd.service"));
        assert!(out.contains("Requires=jamstream-firewall.service"));
        // Rerunning the script must not stack duplicate rules.
        assert!(out.contains("\"$ipt\" -F INPUT"));
        assert!(out.contains("\"$ipt\" -F OUTPUT"));
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
        let script = bootstrap_script(&base_config(SelfDestruct::AwsShutdown));
        let jamstreamd = at(&script, "enable --now jamstreamd.service");
        let ffmpeg = at(&script, "/usr/local/sbin/jamstream-media");
        assert!(
            jamstreamd < ffmpeg,
            "the session waits on a 100 MB download"
        );

        // Key staging is tmpfs readable only by the service account, and
        // other processes' argv is hidden.
        assert!(out.contains("install -d -o jamstream -g jamstream -m 0700 /run/jamstream/keys"));
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
