//! Local provider: the host's own machine is the "cloud". launch() spawns
//! jamstreamd as a child process, so the whole hosting flow (wizard,
//! invites, teardown, sweeper) works unchanged at zero cost with no
//! provisioning wait.
//!
//! # user_data contract
//!
//! For this provider `LaunchSpec::user_data` is the flat key=value server
//! config (`BootConfig::render_flat_config`), written verbatim to
//! `<state_dir>/sessions/<session>/config` with mode 0600. Cloud providers
//! receive cloud-init YAML instead; the caller picks the payload per
//! provider kind.
//!
//! # Server binary resolution
//!
//! In order: the explicit `with_server_binary` override; the
//! `JAMSTREAMD_PATH` environment variable; a `jamstreamd` (`jamstreamd.exe`
//! on Windows) sitting next to the current executable; `jamstreamd` on
//! `PATH`. When none resolves, launch fails with an error naming all four.
//!
//! # Registry
//!
//! Running sessions are tracked in `<state_dir>/local.json` (mode 0600),
//! one entry per spawn: pid, session id, config path, start time. The
//! registry lives on disk, not in memory, so a fresh provider on the same
//! state dir (the sweeper story) still finds and can destroy sessions an
//! earlier process launched. Liveness is verified on every list and dead
//! entries are pruned.
//!
//! # Reachability
//!
//! `Instance::public_ip` is the primary LAN address, discovered with the
//! UDP-connect trick (connect() on a UDP socket sends no packets but makes
//! the OS pick the outbound interface). Invites built from it work for
//! musicians on the same network; cross-internet guests need router port
//! forwarding. Multi-address invites and UPnP are future work. With no
//! network at all this falls back to 127.0.0.1 with a warning.
//!
//! # Idle teardown and the session cap
//!
//! There is no systemd guard on a laptop, so the spawned server gets
//! `--idle-exit-min` from the config's `idle_shutdown_min` and exits on its
//! own once no musicians have been connected for that long. It likewise
//! gets `--max-duration-min` from the config's `max_duration_min` and
//! exits when the session has run that long, connected musicians or not.
//!
//! # Platform notes
//!
//! Unix is precise: liveness via `ps` process state (zombies count as
//! dead, so a terminated child whose parent has not reaped it does not
//! look alive), termination via SIGTERM with a SIGKILL fallback after 5 s.
//! Windows has no cross-process SIGTERM equivalent for console programs,
//! so both termination steps are a forced `taskkill /F` and liveness is a
//! `tasklist` query; the Windows path compiles but is a known, untested
//! gap.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::provider::{Provider, ProviderError, Result};
use crate::types::{Instance, LaunchSpec, Price, ProviderKind, Region, RegionId, session_tag};

const REGION_ID: &str = "local";
const REGISTRY_FILE: &str = "local.json";

#[cfg(windows)]
const BIN_NAME: &str = "jamstreamd.exe";
#[cfg(not(windows))]
const BIN_NAME: &str = "jamstreamd";

/// How long launch waits for the spawned server to come up, and destroy
/// waits for SIGTERM to take before escalating.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const TERM_TIMEOUT: Duration = Duration::from_secs(5);
const KILL_TIMEOUT: Duration = Duration::from_secs(2);
/// Minimum uptime before launch trusts the spawn: a config error makes
/// jamstreamd exit within milliseconds, well inside this window.
const READY_GRACE: Duration = Duration::from_millis(300);
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryEntry {
    pid: u32,
    session: String,
    config_path: PathBuf,
    started_unix: u64,
}

pub struct LocalProvider {
    state_dir: PathBuf,
    server_binary: Option<PathBuf>,
    /// Serializes load-modify-save cycles on the registry file within this
    /// process. Cross-process races are not locked; the CLI runs one
    /// command at a time.
    registry_gate: Mutex<()>,
    /// Child handles for processes this provider spawned. Kept so exited
    /// children get reaped (try_wait) instead of lingering as zombies that
    /// would fool the liveness probe.
    children: Mutex<HashMap<u32, Child>>,
}

impl LocalProvider {
    pub fn new(state_dir: PathBuf) -> Self {
        LocalProvider {
            state_dir,
            server_binary: None,
            registry_gate: Mutex::new(()),
            children: Mutex::new(HashMap::new()),
        }
    }

    /// Overrides binary resolution entirely (tests point this at a build
    /// artifact or a fake).
    pub fn with_server_binary(mut self, path: PathBuf) -> Self {
        self.server_binary = Some(path);
        self
    }

    /// See the module docs for the resolution order.
    fn resolve_server_binary(&self) -> Result<PathBuf> {
        if let Some(explicit) = &self.server_binary {
            return Ok(explicit.clone());
        }
        if let Some(from_env) = std::env::var_os("JAMSTREAMD_PATH")
            && !from_env.is_empty()
        {
            return Ok(PathBuf::from(from_env));
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            let sibling = dir.join(BIN_NAME);
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
        if let Some(on_path) = find_on_path(BIN_NAME) {
            return Ok(on_path);
        }
        Err(ProviderError::Other(format!(
            "cannot find the {BIN_NAME} server binary: no explicit override was set, \
             JAMSTREAMD_PATH is not set, there is no {BIN_NAME} next to the current \
             executable, and {BIN_NAME} is not on PATH"
        )))
    }

    fn local_region() -> Region {
        Region {
            provider: ProviderKind::Local,
            id: RegionId::new(REGION_ID),
            display: "This computer".to_owned(),
            country: String::new(),
        }
    }

    fn registry_path(&self) -> PathBuf {
        self.state_dir.join(REGISTRY_FILE)
    }

    fn session_dir(&self, session: &str) -> PathBuf {
        self.state_dir.join("sessions").join(fs_safe(session))
    }

    /// One locked load-modify-save cycle on the registry file.
    fn with_registry<T>(&self, f: impl FnOnce(&mut Vec<RegistryEntry>) -> T) -> Result<T> {
        let _gate = self.registry_gate.lock().unwrap();
        let path = self.registry_path();
        let mut entries: Vec<RegistryEntry> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                ProviderError::Other(format!("registry {} is corrupt: {e}", path.display()))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(ProviderError::Other(format!(
                    "cannot read registry {}: {e}",
                    path.display()
                )));
            }
        };
        let out = f(&mut entries);
        std::fs::create_dir_all(&self.state_dir).map_err(|e| {
            ProviderError::Other(format!(
                "cannot create state dir {}: {e}",
                self.state_dir.display()
            ))
        })?;
        let json = serde_json::to_vec_pretty(&entries)
            .map_err(|e| ProviderError::Other(format!("registry encode: {e}")))?;
        write_private(&path, &json)
            .map_err(|e| ProviderError::Other(format!("cannot write registry: {e}")))?;
        Ok(out)
    }

    /// True while the process runs. Reaps children this provider spawned so
    /// their exit is visible immediately; foreign pids go through the
    /// platform probe.
    fn pid_alive(&self, pid: u32) -> bool {
        let mut children = self.children.lock().unwrap();
        if let Some(child) = children.get_mut(&pid) {
            match child.try_wait() {
                Ok(Some(_)) => {
                    children.remove(&pid);
                    return false;
                }
                Ok(None) => return true,
                Err(_) => {}
            }
        }
        drop(children);
        process::alive(pid)
    }

    fn instance_for(entry: &RegistryEntry, ip: IpAddr) -> Instance {
        Instance {
            provider: ProviderKind::Local,
            region: Self::local_region(),
            id: entry.pid.to_string(),
            public_ip: Some(ip),
            tags: vec![session_tag(&entry.session)],
        }
    }

    async fn wait_dead(&self, pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.pid_alive(pid) {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
        true
    }
}

#[async_trait]
impl Provider for LocalProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Local
    }

    fn regions(&self) -> Vec<Region> {
        vec![Self::local_region()]
    }

    async fn price(&self, region: &RegionId) -> Result<Price> {
        if region.as_str() != REGION_ID {
            return Err(ProviderError::NotFound(format!("local region {region}")));
        }
        Ok(Price {
            hourly_microusd: 0,
            egress_microusd_per_gb: 0,
            included_egress_gb: 0,
        })
    }

    async fn launch(&self, spec: LaunchSpec) -> Result<Instance> {
        if spec.region.id.as_str() != REGION_ID {
            return Err(ProviderError::NotFound(format!(
                "local region {}",
                spec.region.id
            )));
        }
        let session = spec
            .session_id()
            .ok_or_else(|| {
                ProviderError::Other("launch spec carries no jamstream-session tag".to_owned())
            })?
            .to_owned();
        let binary = self.resolve_server_binary()?;

        let dir = self.session_dir(&session);
        std::fs::create_dir_all(&dir).map_err(|e| {
            ProviderError::Other(format!("cannot create session dir {}: {e}", dir.display()))
        })?;
        let config_path = dir.join("config");
        write_private(&config_path, spec.user_data.as_bytes()).map_err(|e| {
            ProviderError::Other(format!("cannot write {}: {e}", config_path.display()))
        })?;

        // The flat config is the source of truth; the port feeds the
        // reachability probe, and idle_shutdown_min / max_duration_min
        // become the spawned server's own dead man's switch and session
        // cap (no external guard on a laptop).
        let port = flat_config_value(&spec.user_data, "port").and_then(|v| v.parse::<u16>().ok());
        let idle_min = flat_config_value(&spec.user_data, "idle_shutdown_min")
            .unwrap_or("0")
            .to_owned();
        let max_duration_min = flat_config_value(&spec.user_data, "max_duration_min")
            .unwrap_or("0")
            .to_owned();

        let log = std::fs::File::create(dir.join("server.log"))
            .map_err(|e| ProviderError::Other(format!("cannot create server log: {e}")))?;
        let child = Command::new(&binary)
            .arg("--config")
            .arg(&config_path)
            .arg("--activity-file")
            .arg(dir.join("last-active"))
            .arg("--idle-exit-min")
            .arg(&idle_min)
            .arg("--max-duration-min")
            .arg(&max_duration_min)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().map_err(|e| {
                ProviderError::Other(format!("cannot clone server log handle: {e}"))
            })?))
            .stderr(Stdio::from(log))
            .spawn()
            .map_err(|e| ProviderError::Other(format!("cannot spawn {}: {e}", binary.display())))?;
        let pid = child.id();
        self.children.lock().unwrap().insert(pid, child);

        // Recorded before the readiness wait so even a botched startup is
        // visible to list_tagged and gets swept, never leaked.
        let started_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.with_registry(|entries| {
            entries.push(RegistryEntry {
                pid,
                session: session.clone(),
                config_path: config_path.clone(),
                started_unix,
            })
        })?;

        // Readiness: the process must survive the grace window (a bad
        // config kills jamstreamd immediately), plus a best-effort UDP send
        // to the configured port. If the probe never confirms but the
        // process lives, proceed; the client join surfaces real trouble.
        let started = Instant::now();
        loop {
            if !self.pid_alive(pid) {
                let _ = self.with_registry(|entries| entries.retain(|e| e.pid != pid));
                return Err(ProviderError::Other(format!(
                    "jamstreamd exited during startup; see {}",
                    dir.join("server.log").display()
                )));
            }
            if started.elapsed() >= READY_GRACE && udp_probe(port) {
                break;
            }
            if started.elapsed() >= READY_TIMEOUT {
                tracing::warn!(
                    pid,
                    "local server alive but readiness probe never confirmed"
                );
                break;
            }
            tokio::time::sleep(POLL).await;
        }

        Ok(Instance {
            provider: ProviderKind::Local,
            region: Self::local_region(),
            id: pid.to_string(),
            public_ip: Some(primary_lan_ip()),
            tags: spec.tags,
        })
    }

    async fn destroy(&self, _region: &RegionId, id: &str) -> Result<()> {
        let pid: u32 = id
            .parse()
            .map_err(|_| ProviderError::NotFound(format!("local instance {id}")))?;
        let entry = self.with_registry(|entries| entries.iter().find(|e| e.pid == pid).cloned())?;
        let Some(entry) = entry else {
            return Err(ProviderError::NotFound(format!("local instance {id}")));
        };
        if !self.pid_alive(pid) {
            self.with_registry(|entries| entries.retain(|e| e.pid != pid))?;
            let _ = std::fs::remove_dir_all(self.session_dir(&entry.session));
            return Err(ProviderError::NotFound(format!(
                "local instance {id} already dead"
            )));
        }

        process::terminate(pid);
        if !self.wait_dead(pid, TERM_TIMEOUT).await {
            tracing::warn!(pid, "graceful termination timed out, killing");
            process::kill(pid);
            if !self.wait_dead(pid, KILL_TIMEOUT).await {
                return Err(ProviderError::Other(format!(
                    "local instance {id} survived SIGKILL"
                )));
            }
        }
        // Reap our own child if we spawned it in this process.
        if let Some(mut child) = self.children.lock().unwrap().remove(&pid) {
            let _ = child.wait();
        }
        self.with_registry(|entries| entries.retain(|e| e.pid != pid))?;
        let _ = std::fs::remove_dir_all(self.session_dir(&entry.session));
        Ok(())
    }

    async fn list_tagged(&self, session_tag: Option<&str>) -> Result<Vec<Instance>> {
        // Prune dead pids while listing; the registry only ever holds
        // sessions that were actually running at last look.
        let live = self.with_registry(|entries| {
            entries.retain(|e| self.pid_alive(e.pid));
            entries.clone()
        })?;
        let ip = primary_lan_ip();
        Ok(live
            .iter()
            .filter(|e| session_tag.is_none_or(|want| e.session == want))
            .map(|e| Self::instance_for(e, ip))
            .collect())
    }
}

/// Keeps [A-Za-z0-9._-]; everything else becomes '-'. Session ids are hex
/// in practice, this is belt and braces for the filesystem path.
fn fs_safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Reads one value out of the flat key=value config format
/// (`BootConfig::render_flat_config`). Best effort: non-flat payloads
/// simply yield None.
fn flat_config_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (k, v) = line.split_once('=')?;
        (k.trim() == key).then(|| v.trim())
    })
}

/// Creates or truncates `path` with owner-only permissions (0600 on unix;
/// Windows inherits the directory ACL, a documented gap).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)
}

fn find_on_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

/// Best-effort "is anything listening" signal. A UDP send to a closed
/// local port often succeeds anyway (the ICMP error arrives later), so
/// this can only delay readiness, never veto it; process liveness through
/// the grace window is the real check.
fn udp_probe(port: Option<u16>) -> bool {
    let Some(port) = port else { return true };
    std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|s| s.send_to(&[0], (Ipv4Addr::LOCALHOST, port)))
        .is_ok()
}

/// Primary LAN address via the UDP-connect trick: connect() sends no
/// packets but makes the OS pick the outbound interface. 203.0.113.1 is
/// TEST-NET-3, guaranteed not to be a neighbor.
fn primary_lan_ip() -> IpAddr {
    let discover = || -> std::io::Result<IpAddr> {
        let s = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        s.connect(("203.0.113.1", 9))?;
        Ok(s.local_addr()?.ip())
    };
    match discover() {
        Ok(ip) if !ip.is_unspecified() && !ip.is_loopback() => ip,
        _ => {
            tracing::warn!(
                "no LAN address found; invites will carry 127.0.0.1 and only work on this machine"
            );
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        }
    }
}

#[cfg(unix)]
mod process {
    use std::process::Command;

    /// Zombies count as dead: a terminated child whose parent has not
    /// reaped it yet must not look alive to the registry.
    pub fn alive(pid: u32) -> bool {
        match Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
        {
            Ok(out) => {
                out.status.success()
                    && !String::from_utf8_lossy(&out.stdout)
                        .trim_start()
                        .starts_with('Z')
            }
            Err(_) => false,
        }
    }

    pub fn terminate(pid: u32) {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }

    pub fn kill(pid: u32) {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
}

#[cfg(windows)]
mod process {
    use std::process::Command;

    pub fn alive(pid: u32) -> bool {
        // tasklist prints a CSV row per match and an INFO line otherwise.
        match Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
        {
            Ok(out) => String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\"")),
            Err(_) => false,
        }
    }

    /// Platform gap: no cross-process SIGTERM equivalent for console
    /// programs, so termination is forced from the start.
    pub fn terminate(pid: u32) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }

    pub fn kill(pid: u32) {
        terminate(pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The contract test spawns a shell-script fake server, so it and its
    // import are unix-gated.
    #[cfg(unix)]
    use crate::contract::assert_provider_contract;
    use crate::types::InstanceClass;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jamstream-local-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stand-in server: exec keeps the script's pid, SIGTERM kills it.
    #[cfg(unix)]
    fn fake_server(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("fake-jamstreamd");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 600\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_provider_passes_contract() {
        let dir = temp_dir("contract");
        let provider = LocalProvider::new(dir.join("state")).with_server_binary(fake_server(&dir));
        assert_provider_contract(&provider).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_binary_fails_launch_cleanly() {
        let dir = temp_dir("missing-bin");
        let provider =
            LocalProvider::new(dir.join("state")).with_server_binary(dir.join("does-not-exist"));
        let spec = LaunchSpec {
            region: LocalProvider::local_region(),
            instance_class: InstanceClass::Small,
            user_data: String::new(),
            tags: vec![session_tag("s1")],
        };
        let err = provider.launch(spec).await.unwrap_err();
        assert!(matches!(err, ProviderError::Other(_)));
        assert!(err.to_string().contains("does-not-exist"));
        assert!(
            provider.list_tagged(None).await.unwrap().is_empty(),
            "failed launch must not leave a registry entry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flat_config_values_parse() {
        let text = "# comment\nport = 43210\nidle_shutdown_min = 10\nmax_duration_min = 720\n";
        assert_eq!(flat_config_value(text, "port"), Some("43210"));
        assert_eq!(flat_config_value(text, "idle_shutdown_min"), Some("10"));
        assert_eq!(flat_config_value(text, "max_duration_min"), Some("720"));
        assert_eq!(flat_config_value(text, "missing"), None);
        assert_eq!(flat_config_value("#cloud-config\n", "port"), None);
    }

    #[test]
    fn region_catalog_is_this_computer() {
        let p = LocalProvider::new(PathBuf::from("/tmp/unused"));
        let regions = p.regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].provider, ProviderKind::Local);
        assert_eq!(regions[0].id.as_str(), "local");
        assert_eq!(regions[0].display, "This computer");
        assert_eq!(regions[0].country, "");
    }
}
