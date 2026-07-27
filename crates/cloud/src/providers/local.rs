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
//! It has to be the server binary, not a wrapper that execs it: liveness
//! compares the running image to the name that was launched (see
//! [`ps_probe`]), and after an exec those differ, so a wrapped server reads
//! as not ours and never gets destroyed.
//!
//! # Registry
//!
//! Running sessions are tracked in `<state_dir>/local.json` (mode 0600),
//! one entry per spawn: pid, session id, config path, start time, and the
//! image file name that was spawned. The registry lives on disk, not in
//! memory, so a fresh provider on the same state dir (the sweeper story)
//! still finds and can destroy sessions an earlier process launched.
//! Liveness is verified on every list and dead entries are pruned. The
//! image name and the start time are the pid-reuse guard: see
//! [`Spawned`] and `process::alive`.
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
//! # Shutdown: the sentinel file
//!
//! Ending a session should let the server say goodbye, not shoot it. Unix
//! has SIGTERM for that; Windows has no cross-process SIGTERM for console
//! programs, and every alternative there is worse than portable (see the
//! comment on `request_graceful_shutdown`). So the polite request is a
//! file: destroy creates `<session dir>/shutdown` and jamstreamd, which is
//! passed `--shutdown-file` at spawn, is expected to poll for it and exit
//! cleanly. One mechanism, identical everywhere, no new dependency.
//!
//! The server half of that contract is not implemented yet, so the
//! provider only *waits* for the sentinel to work when jamstreamd has left
//! a `<session dir>/shutdown.supported` marker to prove it polls. Without
//! the marker, teardown is exactly what it is today: SIGTERM on unix, an
//! immediate forced kill on Windows. Nothing regresses while the two
//! halves are out of step, and the marker is the switch that turns the
//! graceful path on.
//!
//! # Platform notes
//!
//! Both platforms answer the same question in one query: is this pid alive
//! *and still the process we launched*. A registry entry outlives its
//! process whenever the machine reboots, sleeps, or crashes, and the pid it
//! names is then free to belong to anyone; killing it on the strength of
//! the number alone is how a sweeper murders a stranger's process.
//!
//! On unix that is `ps -p <pid> -o stat=,etime=,comm=` parsed by
//! [`ps_probe`]: zombies count as dead (a terminated child whose parent has
//! not reaped it must not look alive), the command name is compared to the
//! image recorded at launch, and the elapsed time is reconciled with the
//! recorded start. Termination is SIGTERM with a SIGKILL fallback after
//! 5 s.
//!
//! On Windows the forced step is `taskkill /PID <pid> /T /F` and liveness
//! is an exact-match `tasklist /FI "PID eq <pid>" /NH /FO CSV` parse
//! ([`tasklist_probe`]) cross-checked against the image name. The Windows
//! path is exercised by the `cfg(windows)` tests below on the CI Windows
//! runner; it has had no soak on real Windows hardware.
//!
//! # File permissions
//!
//! The per-session config carries the server's private key, so unix writes
//! it (and the registry) 0600. Windows has no mode bits: a new file gets
//! the inheritable ACEs of the directory it lands in. Under the default
//! state dir (`%LOCALAPPDATA%\jamstream`) those come from the user profile
//! and are the user, SYSTEM, and Administrators, so no other account can
//! read the key - but that is the default, not a guarantee.
//! `JAMSTREAM_STATE_DIR` can point anywhere, and a directory created under
//! `C:\` inherits `Authenticated Users: Modify` from the volume root,
//! which would leave the key readable by every account on the machine.
//! Every directory this provider creates is therefore tightened once, at
//! creation, with `icacls` (`harden_new_dir`); pre-existing directories
//! are left alone.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::provider::{Provider, ProviderError, Result};
use crate::types::{
    IngressRule, Instance, LaunchSpec, Price, ProviderKind, Region, RegionId, session_tag,
};

const REGION_ID: &str = "local";
const REGISTRY_FILE: &str = "local.json";
/// The graceful-shutdown request and the marker that proves the spawned
/// server acts on it, both inside the per-session directory.
const SHUTDOWN_FILE: &str = "shutdown";
const SHUTDOWN_SUPPORTED_FILE: &str = "shutdown.supported";

#[cfg(windows)]
const BIN_NAME: &str = "jamstreamd.exe";
#[cfg(not(windows))]
const BIN_NAME: &str = "jamstreamd";

/// How long launch waits for the spawned server to come up. The window
/// destroy allows a polite exit is per-platform: [`process::term_grace`].
const READY_TIMEOUT: Duration = Duration::from_secs(5);
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
    /// File name of the binary that was spawned (`jamstreamd.exe`), half of
    /// the pid-reuse guard. Defaulted so a registry written by an older
    /// build still loads; None simply skips the check, which is the
    /// pre-existing behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image_name: Option<String>,
}

impl RegistryEntry {
    fn spawned(&self) -> Spawned<'_> {
        Spawned {
            image_name: self.image_name.as_deref(),
            started_unix: self.started_unix,
        }
    }
}

/// What the registry knows about a spawn beyond its pid, which is the only
/// thing that lets a liveness probe tell our server from whatever process
/// inherited the number. Either field may be absent in a registry written
/// by an older build; a probe skips the checks it cannot make.
#[derive(Debug, Clone, Copy)]
struct Spawned<'a> {
    image_name: Option<&'a str>,
    /// Wall-clock second the spawn was recorded, or 0 when unknown.
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
        create_private_dir(&self.state_dir).map_err(|e| {
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
    /// platform probe, which takes what the registry recorded so a recycled
    /// pid cannot pass for ours.
    fn pid_alive(&self, pid: u32, spawned: Spawned<'_>) -> bool {
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
        process::alive(pid, spawned)
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

    /// A zero timeout means "check once": already-dead reads true, still
    /// alive reads false with no waiting at all.
    async fn wait_dead(&self, pid: u32, spawned: Spawned<'_>, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.pid_alive(pid, spawned) {
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
        create_private_dir(&dir).map_err(|e| {
            ProviderError::Other(format!("cannot create session dir {}: {e}", dir.display()))
        })?;
        // A shutdown request left behind by a previous session that reused
        // this id would make the new server exit at its first heartbeat,
        // and a stale support marker would make destroy wait on a server
        // that never polls. Both start every launch absent.
        clear_shutdown_files(&dir);
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
        // --shutdown-file is the graceful-exit request path. jamstreamd's
        // argument scan ignores flags it does not know, so passing it to a
        // build that predates the server half costs nothing.
        let child = Command::new(&binary)
            .arg("--config")
            .arg(&config_path)
            .arg("--activity-file")
            .arg(dir.join("last-active"))
            .arg("--shutdown-file")
            .arg(shutdown_path(&dir))
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
        let image_name = binary.file_name().map(|n| n.to_string_lossy().into_owned());

        // Recorded before the readiness wait so even a botched startup is
        // visible to list_tagged and gets swept, never leaked.
        let started_unix = now_unix();
        let spawned = Spawned {
            image_name: image_name.as_deref(),
            started_unix,
        };
        self.with_registry(|entries| {
            entries.push(RegistryEntry {
                pid,
                session: session.clone(),
                config_path: config_path.clone(),
                started_unix,
                image_name: image_name.clone(),
            })
        })?;

        // Readiness: the process must survive the grace window (a bad
        // config kills jamstreamd immediately), plus a best-effort UDP send
        // to the configured port. If the probe never confirms but the
        // process lives, proceed; the client join surfaces real trouble.
        let started = Instant::now();
        loop {
            if !self.pid_alive(pid, spawned) {
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
        let spawned = entry.spawned();
        if !self.pid_alive(pid, spawned) {
            self.with_registry(|entries| entries.retain(|e| e.pid != pid))?;
            let _ = std::fs::remove_dir_all(self.session_dir(&entry.session));
            return Err(ProviderError::NotFound(format!(
                "local instance {id} already dead"
            )));
        }

        // Ask first, then insist. The request is the sentinel file on every
        // platform plus, on unix, the SIGTERM that has always ended these
        // processes. How long the ask is given depends on whether anything
        // is listening for it: see process::term_grace.
        let dir = self.session_dir(&entry.session);
        let asked = request_graceful_shutdown(&dir);
        process::terminate(pid);
        let grace = process::term_grace(asked && graceful_shutdown_supported(&dir));
        if !self.wait_dead(pid, spawned, grace).await {
            if !grace.is_zero() {
                tracing::warn!(pid, "graceful termination timed out, killing");
            }
            process::kill(pid);
            if !self.wait_dead(pid, spawned, KILL_TIMEOUT).await {
                return Err(ProviderError::Other(format!(
                    "local instance {id} survived {}",
                    process::FORCED_KILL
                )));
            }
        }
        // Reap our own child if we spawned it in this process.
        if let Some(mut child) = self.children.lock().unwrap().remove(&pid) {
            let _ = child.wait();
        }
        self.with_registry(|entries| entries.retain(|e| e.pid != pid))?;
        // Takes the shutdown request and the support marker with it.
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    async fn list_tagged(&self, session_tag: Option<&str>) -> Result<Vec<Instance>> {
        // Prune dead pids while listing; the registry only ever holds
        // sessions that were actually running at last look.
        let live = self.with_registry(|entries| {
            entries.retain(|e| self.pid_alive(e.pid, e.spawned()));
            entries.clone()
        })?;
        let ip = primary_lan_ip();
        Ok(live
            .iter()
            .filter(|e| session_tag.is_none_or(|want| e.session == want))
            .map(|e| Self::instance_for(e, ip))
            .collect())
    }

    /// There is no cloud network here. The host's own firewall and router are
    /// the host's business, which is why local invites carry a LAN address
    /// and guests outside it need port forwarding.
    async fn session_ingress(&self, _session: &str) -> Result<Vec<IngressRule>> {
        Ok(Vec::new())
    }

    async fn destroy_orphan_firewalls(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// Where the graceful-shutdown request lives for a session, and the path
/// jamstreamd is told to poll via `--shutdown-file`.
fn shutdown_path(session_dir: &Path) -> PathBuf {
    session_dir.join(SHUTDOWN_FILE)
}

/// Asks the session server to exit cleanly by creating the sentinel file it
/// polls. Returns whether the request actually reached the disk.
///
/// Why a file, when the obvious move is an OS signal:
///
/// * `taskkill /PID <pid> /T` without `/F` posts WM_CLOSE, which only a
///   process with a message loop and windows can act on. jamstreamd is a
///   console binary, so it answers "this process can only be terminated
///   forcefully" and nothing happens.
/// * `GenerateConsoleCtrlEvent` can deliver a real CTRL_BREAK, but only
///   into a process group the child was spawned into with
///   CREATE_NEW_PROCESS_GROUP, only from a process that owns a console
///   (the desktop app is a windowed binary and usually has none), and only
///   with a windows-sys/winapi dependency here. It would also need the
///   server to install a CTRL_BREAK handler, since the default disposition
///   for it is an abrupt kill - so even that route cannot avoid changing
///   the server crate.
/// * A sentinel file needs no dependency, no console, and no process
///   group; it works from a different process than the one that spawned
///   the server (which is exactly the sweeper's situation), and it is one
///   code path on every platform.
fn request_graceful_shutdown(session_dir: &Path) -> bool {
    let path = shutdown_path(session_dir);
    // The content is for a human reading a stuck session dir; the server
    // only cares that the path exists.
    let stamp = now_unix();
    match write_private(&path, format!("requested_unix={stamp}\n").as_bytes()) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(error = %err, path = %path.display(), "cannot write the shutdown request");
            false
        }
    }
}

/// True when the spawned server left the marker that says it polls the
/// sentinel. Absent marker means an older jamstreamd: skip the wait and go
/// straight to the forced kill, which is what that build has always got.
fn graceful_shutdown_supported(session_dir: &Path) -> bool {
    session_dir.join(SHUTDOWN_SUPPORTED_FILE).exists()
}

/// Clears both shutdown files, for a session directory about to be reused.
fn clear_shutdown_files(session_dir: &Path) {
    for name in [SHUTDOWN_FILE, SHUTDOWN_SUPPORTED_FILE] {
        let path = session_dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(error = %err, path = %path.display(), "cannot clear stale shutdown file");
            }
        }
    }
}

/// Creates or truncates `path` with owner-only permissions: 0600 on unix.
/// Windows has no mode bits, so the file takes the inheritable ACEs of its
/// directory instead, which is why the directories this provider creates
/// are hardened at creation (see [`harden_new_dir`]).
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

/// `create_dir_all` that hands every directory it actually creates to
/// [`harden_new_dir`]. Directories that already exist are untouched: the
/// state dir may be a path the user chose and shares on purpose, and
/// silently rewriting an existing ACL is not ours to do.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        create_private_dir(parent)?;
    }
    match std::fs::create_dir(dir) {
        Ok(()) => {
            harden_new_dir(dir);
            Ok(())
        }
        // Lost a race with another process; its directory is fine.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(not(windows))]
fn harden_new_dir(_dir: &Path) {}

/// Drops the multi-account groups from a freshly created directory, so the
/// per-session config (which carries the server's private key) is not
/// readable by every account on the machine just because the state dir was
/// pointed somewhere permissive. Under the default `%LOCALAPPDATA%` path
/// there is nothing to remove and this is a no-op in effect; it earns its
/// keep for a `JAMSTREAM_STATE_DIR` under `C:\` or another shared root.
///
/// Two `icacls` passes, because `/remove:g` cannot touch an inherited ACE:
///
/// 1. `/inheritance:d` copies the inherited ACEs into the directory's own
///    ACL, and `/grant:r` pins this account's full control so pass 2 can
///    never delete the last entry that lets us read our own state.
/// 2. `/remove:g` drops the multi-account groups a directory outside the
///    user profile can inherit (Everyone, Authenticated Users, Users,
///    Anonymous), addressed by well-known SID because the display names
///    are localized.
///
/// Limits, deliberately:
///
/// * SYSTEM and Administrators keep their access, which is not a boundary
///   anyone can enforce anyway (an administrator can take ownership);
/// * an individual *other* account explicitly granted access on the parent
///   keeps it - only the four groups above are removed;
/// * the `(OI)(CI)` grant covers files created here afterwards, which is
///   all of them, since the directory is new;
/// * a volume without ACLs (FAT/exFAT) has nothing to tighten, `icacls`
///   fails there, and we only log it;
/// * the registry (`local.json`) sits in the state dir, so it is only
///   covered when the state dir is one we created - it holds pids and
///   session ids, no key material;
/// * and this is an external command rather than a DACL passed to
///   CreateFile, which is the correct fix and needs a windows-sys
///   dependency we are not taking on for this.
#[cfg(windows)]
fn harden_new_dir(dir: &Path) {
    let Some(user) = std::env::var_os("USERNAME").filter(|u| !u.is_empty()) else {
        tracing::warn!(
            dir = %dir.display(),
            "USERNAME is unset, leaving the directory ACL inherited: a state dir outside \
             the user profile may be readable by other accounts"
        );
        return;
    };
    let grant = format!("{}:(OI)(CI)F", user.to_string_lossy());
    let icacls = |args: &[&std::ffi::OsStr]| -> bool {
        match Command::new("icacls").args(args).output() {
            Ok(out) if out.status.success() => true,
            Ok(out) => {
                tracing::warn!(
                    dir = %dir.display(),
                    status = ?out.status.code(),
                    output = %String::from_utf8_lossy(&out.stderr).trim(),
                    "icacls did not tighten the directory ACL"
                );
                false
            }
            Err(err) => {
                tracing::warn!(error = %err, "cannot run icacls; directory ACL left inherited");
                false
            }
        }
    };
    let o = std::ffi::OsStr::new;
    let target = dir.as_os_str();
    if !icacls(&[target, o("/inheritance:d"), o("/grant:r"), o(&grant)]) {
        return;
    }
    icacls(&[
        target,
        o("/remove:g"),
        o("*S-1-1-0"), // Everyone
        o("/remove:g"),
        o("*S-1-5-11"), // Authenticated Users
        o("/remove:g"),
        o("*S-1-5-32-545"), // Users
        o("/remove:g"),
        o("*S-1-5-7"), // Anonymous Logon
    ]);
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

/// What the platform said about one pid.
#[derive(Debug, PartialEq, Eq)]
enum PidProbe {
    /// No process with that pid, or one that has already exited.
    Dead,
    /// A process with that pid matching everything the registry recorded
    /// about our spawn (or nothing was recorded to check against).
    Alive,
    /// A live pid that is not the process we launched: it was recycled
    /// while our registry entry went stale. `running` describes what is
    /// there instead, for the log.
    Mismatch { running: String },
}

/// Exact-match parse of `tasklist /FI "PID eq <pid>" /NH /FO CSV`.
///
/// `/FO CSV` quotes every field - `"image","pid","session","session#","mem"` -
/// so a row counts only when the line starts with a quote, its image field is
/// non-empty, and its pid field equals `pid` exactly. That is what makes this
/// robust where a substring search was not:
///
/// * the memory column is comma-grouped and quoted (`"12,345 K"`), so a pid
///   whose digits appear there cannot pass for a match, and neither can a
///   longer pid that merely starts with ours;
/// * the "no tasks are running which match" line is localized, but it is
///   not a quoted CSV row, so it fails the shape test without us having to
///   know its translation. Empty output fails it too.
///
/// `expect_image` is the pid-reuse guard. Windows recycles pids briskly and
/// the registry can outlive a process (a laptop that slept, a crash before
/// the sweep), so a live pid alone does not prove it is our server. When
/// the recorded image name does not match what is running, this reports
/// [`PidProbe::Mismatch`] and the caller treats it as dead: refusing to
/// kill a stranger's process is the only acceptable way to be wrong here.
/// The comparison is case-insensitive (Windows file names are) and
/// tolerates a missing or extra `.exe`, because CreateProcess appends the
/// extension the registry may not have recorded.
///
/// The registry's `started_unix` is the stronger check and unix reconciles
/// it (see [`ps_probe`]), but Windows has no column for it: `wmic process
/// where processid=N get creationdate` is deprecated and gone from Windows
/// 11 24H2, and `powershell -Command "Get-Process -Id N | Select-Object
/// StartTime"` costs a PowerShell startup (a few hundred ms) on every
/// liveness check - and `list_tagged` probes every entry, on every list, in
/// a sweeper that runs often. The image name buys nearly all of the safety
/// out of output we already have, for free, so here start time stays an
/// audit field. A pid recycled by *another jamstreamd* still slips through
/// on Windows; that window is small, and the cost of being wrong there is
/// destroying a session that was going to be destroyed anyway.
#[cfg(any(windows, test))]
fn tasklist_probe(stdout: &str, pid: u32, expect_image: Option<&str>) -> PidProbe {
    let wanted = pid.to_string();
    for line in stdout.lines() {
        let fields = csv_quoted_fields(line);
        let (Some(image), Some(found)) = (fields.first(), fields.get(1)) else {
            continue;
        };
        if image.is_empty() || *found != wanted.as_str() {
            continue;
        }
        return match expect_image {
            Some(want) if !same_image(image, want) => PidProbe::Mismatch {
                running: (*image).to_owned(),
            },
            _ => PidProbe::Alive,
        };
    }
    PidProbe::Dead
}

/// Case-insensitive image-name comparison that ignores a trailing `.exe` on
/// either side.
#[cfg(any(windows, test))]
fn same_image(a: &str, b: &str) -> bool {
    /// `get` rather than slicing: a name whose last bytes are mid-character
    /// must fall through, not panic.
    fn stem(s: &str) -> &str {
        match s.len().checked_sub(4).map(|cut| (cut, s.get(cut..))) {
            Some((cut, Some(tail))) if tail.eq_ignore_ascii_case(".exe") => &s[..cut],
            _ => s,
        }
    }
    stem(a).eq_ignore_ascii_case(stem(b))
}

/// Splits one `/FO CSV` row into its quoted fields. tasklist quotes every
/// field, so a line that does not start with a quote is not a task row and
/// yields nothing at all. Image names cannot contain a quote, so there is
/// no escape syntax to handle.
#[cfg(any(windows, test))]
fn csv_quoted_fields(line: &str) -> Vec<&str> {
    let mut rest = match line.trim().strip_prefix('"') {
        Some(rest) => rest,
        None => return Vec::new(),
    };
    let mut fields = Vec::new();
    while let Some(end) = rest.find('"') {
        fields.push(&rest[..end]);
        // Fields are separated by exactly `","`; anything else ends the row.
        match rest[end + 1..]
            .strip_prefix(',')
            .and_then(|r| r.strip_prefix('"'))
        {
            Some(next) => rest = next,
            None => break,
        }
    }
    fields
}

/// How far a process's reconstructed start time may sit after the moment
/// the registry recorded the spawn before the entry is judged stale.
///
/// The window is wide on purpose. `etime` is elapsed time, so the start it
/// implies is derived from the current wall clock, while `started_unix` was
/// read from the wall clock as it stood at launch: an NTP step between the
/// two shifts one and not the other, and being wrong in that direction
/// means refusing to destroy a session that is genuinely ours. Five minutes
/// absorbs any correction a laptop makes after resume while still catching
/// what this check is for, an entry that outlived a reboot.
#[cfg(any(unix, test))]
const START_SLACK_SECS: u64 = 300;

/// Longest command name Linux keeps (`TASK_COMM_LEN` minus the terminator),
/// so anything at exactly this length may have been truncated.
#[cfg(any(unix, test))]
const COMM_MAX: usize = 15;

/// Parse of `ps -p <pid> -o stat=,etime=,comm=`, one line, three fields:
/// process state, elapsed time, command.
///
/// Both identity checks come out of that single call, which is what makes
/// them affordable in a sweeper that probes every entry on every list:
///
/// * the command name against the image recorded at launch. Unix reports it
///   differently per platform (Linux gives the kernel's `comm`, a bare name
///   truncated to [`COMM_MAX`]; macOS gives argv[0], which is the path the
///   process was launched from), so the comparison is on file names with a
///   truncation allowance and nothing else;
/// * the elapsed time against `started_unix`. A pid that came back around
///   started after we recorded ours, so a process younger than the entry by
///   more than [`START_SLACK_SECS`] is not the process the entry describes.
///   This is the half that catches a pid recycled by another `jamstreamd`,
///   which the name check alone cannot see.
///
/// Either way a mismatch reads as dead and the caller never signals it:
/// refusing to kill a stranger's process is the only acceptable way to be
/// wrong here.
#[cfg(any(unix, test))]
fn ps_probe(stdout: &str, spawned: Spawned<'_>, now_unix: u64) -> PidProbe {
    let Some((stat, etime, comm)) = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .and_then(split_ps_fields)
    else {
        return PidProbe::Dead;
    };
    // A terminated child whose parent has not reaped it is not alive.
    if stat.starts_with('Z') {
        return PidProbe::Dead;
    }
    if let Some(want) = spawned.image_name
        && !comm.is_empty()
        && !same_command(comm, want)
    {
        return PidProbe::Mismatch {
            running: comm.to_owned(),
        };
    }
    if spawned.started_unix > 0
        && let Some(elapsed) = parse_etime(etime)
    {
        let started = now_unix.saturating_sub(elapsed);
        if started > spawned.started_unix + START_SLACK_SECS {
            return PidProbe::Mismatch {
                running: format!(
                    "{comm}, started {}s after the registry entry",
                    started - spawned.started_unix
                ),
            };
        }
    }
    PidProbe::Alive
}

/// Splits one `ps` row into state, elapsed time, and command. The command
/// is the whole remainder because a macOS argv[0] can contain spaces; a row
/// with fewer than three fields is not one we can read.
#[cfg(any(unix, test))]
fn split_ps_fields(line: &str) -> Option<(&str, &str, &str)> {
    let (stat, rest) = line.split_once(char::is_whitespace)?;
    let (etime, comm) = rest.trim_start().split_once(char::is_whitespace)?;
    Some((stat, etime, comm.trim()))
}

/// File-name comparison for what `ps` reported against the image recorded
/// at launch, tolerating Linux's truncation of long names.
#[cfg(any(unix, test))]
fn same_command(observed: &str, expected: &str) -> bool {
    fn file_name(s: &str) -> &str {
        s.rsplit('/').next().unwrap_or(s)
    }
    let observed = file_name(observed);
    let expected = file_name(expected);
    observed == expected || (observed.len() >= COMM_MAX && expected.starts_with(observed))
}

/// Seconds from POSIX `etime`, `[[dd-]hh:]mm:ss`.
#[cfg(any(unix, test))]
fn parse_etime(field: &str) -> Option<u64> {
    let (days, hms) = match field.split_once('-') {
        Some((days, rest)) => (days.parse::<u64>().ok()?, rest),
        None => (0, field),
    };
    let mut parts = hms.rsplit(':');
    let secs: u64 = parts.next()?.parse().ok()?;
    let mins: u64 = parts.next()?.parse().ok()?;
    let hours: u64 = match parts.next() {
        Some(hours) => hours.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() || secs > 59 || mins > 59 {
        return None;
    }
    Some(days * 86_400 + hours * 3_600 + mins * 60 + secs)
}

#[cfg(unix)]
mod process {
    use std::process::Command;
    use std::time::Duration;

    use super::{PidProbe, Spawned};

    /// Named in the error when a process outlives the forced step.
    pub const FORCED_KILL: &str = "SIGKILL";

    /// See [`super::ps_probe`] for the parse and the pid-reuse rules.
    pub fn alive(pid: u32, spawned: Spawned<'_>) -> bool {
        match Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat=,etime=,comm="])
            .output()
        {
            // ps exits nonzero when no process matches, and prints nothing.
            Ok(out) if !out.status.success() => false,
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                match super::ps_probe(&text, spawned, super::now_unix()) {
                    PidProbe::Alive => true,
                    PidProbe::Dead => false,
                    // Loud, because the honest answer ("not ours") means we
                    // will never kill this pid, so a real leak of our own
                    // server would otherwise be invisible.
                    PidProbe::Mismatch { running } => {
                        tracing::warn!(
                            pid,
                            running = %running,
                            expected = spawned.image_name.unwrap_or(""),
                            "pid is alive but is not the process we launched; treating it as \
                             dead rather than killing an unrelated process"
                        );
                        false
                    }
                }
            }
            Err(err) => {
                tracing::warn!(pid, error = %err, "cannot run ps; treating the pid as dead");
                false
            }
        }
    }

    /// SIGTERM is a real request that the kernel delivers whether or not
    /// anything on disk says the server understands it, so unix always
    /// spends the full window before escalating - unchanged behavior.
    pub fn term_grace(_sentinel_honored: bool) -> Duration {
        Duration::from_secs(5)
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
    use std::time::Duration;

    use super::{PidProbe, Spawned};

    pub const FORCED_KILL: &str = "taskkill /F";

    /// Two ticks of the server's 1 s activity heartbeat, which is where the
    /// sentinel poll belongs. Deliberately far short of unix's 5 s: destroy
    /// plus the 2 s forced-kill wait has to finish inside the 5 s budget
    /// crates/server/tests/local_provider.rs asserts, and on Windows the
    /// wait is followed by a kill that always works, so a longer window
    /// buys nothing but teardown latency.
    const SENTINEL_GRACE: Duration = Duration::from_secs(2);

    /// See [`super::tasklist_probe`] for the parse and the pid-reuse rule.
    pub fn alive(pid: u32, spawned: Spawned<'_>) -> bool {
        match Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
        {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                match super::tasklist_probe(&text, pid, spawned.image_name) {
                    PidProbe::Alive => true,
                    PidProbe::Dead => false,
                    // Loud, because the honest answer ("not ours") means we
                    // will never kill this pid, so a real leak of our own
                    // server would otherwise be invisible.
                    PidProbe::Mismatch { running } => {
                        tracing::warn!(
                            pid,
                            running = %running,
                            expected = spawned.image_name.unwrap_or(""),
                            "pid is alive but running another image; treating it as dead \
                             rather than killing an unrelated process"
                        );
                        false
                    }
                }
            }
            Err(err) => {
                tracing::warn!(pid, error = %err, "cannot run tasklist; treating the pid as dead");
                false
            }
        }
    }

    /// Windows has nothing to ask with except the sentinel file, and only a
    /// jamstreamd that polls it can answer. Without the marker proving it
    /// does, waiting would only delay the forced kill that has to follow.
    pub fn term_grace(sentinel_honored: bool) -> Duration {
        if sentinel_honored {
            SENTINEL_GRACE
        } else {
            Duration::ZERO
        }
    }

    /// The polite step. The sentinel file is already on disk by the time
    /// this runs (destroy writes it first); this adds the one signal
    /// Windows itself offers, `taskkill` without `/F`, which posts WM_CLOSE
    /// to the process's windows. A console binary has none, so this
    /// normally fails with "can only be terminated forcefully" - it is free
    /// insurance for the day jamstreamd runs windowed, not the mechanism we
    /// rely on, so its status is ignored.
    pub fn terminate(pid: u32) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .output();
    }

    /// The SIGKILL equivalent: kill the process and its children outright.
    /// `output()` rather than `status()` so taskkill's "SUCCESS: the
    /// process ... has been terminated" chatter does not land in the CLI's
    /// own stdout.
    pub fn kill(pid: u32) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
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
    ///
    /// It execs `sleep` through a symlink of its own name because the
    /// liveness probe compares what is running to the image recorded at
    /// launch, and what runs after an exec is the exec'd image, not the
    /// script. A real jamstreamd is a binary of that name; so is this. A
    /// symlink rather than a copy: macOS kills a copy of a system binary
    /// for failing its code signature.
    #[cfg(unix)]
    fn fake_server(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        const NAME: &str = "fake-jamstreamd";
        let sleep = ["/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .or_else(|| find_on_path("sleep"))
            .expect("no sleep binary to stand in for jamstreamd");
        let image_dir = dir.join("image");
        std::fs::create_dir_all(&image_dir).unwrap();
        let image = image_dir.join(NAME);
        let _ = std::fs::remove_file(&image);
        std::os::unix::fs::symlink(&sleep, &image).unwrap();

        // Quoted: the temp path carries a thread id in parentheses.
        let path = dir.join(NAME);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nexec \"{}\" 600\n", image.display()),
        )
        .unwrap();
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

    /// The app-bundling story: release artifacts place jamstreamd beside
    /// the app/CLI binary, and resolution must find it there with no
    /// override, no env var, and no PATH entry. The test binary stands in
    /// for the app executable.
    #[cfg(unix)]
    #[test]
    fn resolves_the_binary_beside_the_current_executable() {
        if std::env::var_os("JAMSTREAMD_PATH").is_some_and(|v| !v.is_empty()) {
            // The env var outranks the sibling by design; this test is
            // about the sibling step, so a preconfigured env skips it.
            eprintln!("skipping: JAMSTREAMD_PATH is set in this environment");
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let sibling = exe.parent().unwrap().join(BIN_NAME);
        std::fs::write(&sibling, b"#!/bin/sh\nexit 0\n").unwrap();
        let provider = LocalProvider::new(temp_dir("adjacent").join("state"));
        let resolved = provider.resolve_server_binary().unwrap();
        assert_eq!(resolved, sibling);
        let _ = std::fs::remove_file(&sibling);
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

    /// One real row, from a US-English Windows 11 `tasklist /FI "PID eq
    /// 4242" /NH /FO CSV`, kept verbatim as the shape the parser must
    /// accept.
    const ROW: &str = "\"jamstreamd.exe\",\"4242\",\"Console\",\"1\",\"12,345 K\"\r\n";

    #[test]
    fn tasklist_exact_pid_match_is_alive() {
        assert_eq!(
            tasklist_probe(ROW, 4242, Some("jamstreamd.exe")),
            PidProbe::Alive
        );
        // No expectation recorded (a registry from an older build) still
        // answers on the pid alone.
        assert_eq!(tasklist_probe(ROW, 4242, None), PidProbe::Alive);
        // CreateProcess appends the extension the registry may not have,
        // and Windows names are case-insensitive either way.
        for expected in ["jamstreamd", "JAMSTREAMD.EXE", "JamStreamd.Exe"] {
            assert_eq!(
                tasklist_probe(ROW, 4242, Some(expected)),
                PidProbe::Alive,
                "expected image {expected} should match the row"
            );
        }
        // A name too short to carry an extension must not panic or match.
        assert!(!same_image("a", "b.exe"));
        assert!(same_image("é.exe", "é"));
        assert_eq!(
            tasklist_probe(
                "\"jamstreamd\",\"4242\",\"Console\",\"1\",\"9 K\"",
                4242,
                Some("jamstreamd.exe")
            ),
            PidProbe::Alive
        );
    }

    /// The substring bug the old `contains("\"<pid>\"")` check had: digits
    /// of the wanted pid appear in another field, or in a longer pid.
    #[test]
    fn tasklist_never_matches_a_pid_substring() {
        // The memory column is comma-grouped and quoted, so "4,242 K"
        // contains the digits of pid 4242 with a quote on each side.
        let other = "\"notepad.exe\",\"9001\",\"Console\",\"1\",\"4,242 K\"";
        assert_eq!(tasklist_probe(other, 4242, None), PidProbe::Dead);
        // A longer pid that merely starts with ours.
        let longer = "\"jamstreamd.exe\",\"42421\",\"Console\",\"1\",\"9 K\"";
        assert_eq!(tasklist_probe(longer, 4242, None), PidProbe::Dead);
        // ... and one that ends with ours.
        let suffix = "\"jamstreamd.exe\",\"14242\",\"Console\",\"1\",\"9 K\"";
        assert_eq!(tasklist_probe(suffix, 4242, None), PidProbe::Dead);
        // The image field must be a real name, not an empty cell.
        let nameless = "\"\",\"4242\",\"Console\",\"1\",\"9 K\"";
        assert_eq!(tasklist_probe(nameless, 4242, None), PidProbe::Dead);
    }

    /// No match is reported as a localized INFO line, so the parser rejects
    /// it on shape (not a quoted CSV row) rather than on wording.
    #[test]
    fn tasklist_no_task_messages_are_dead_in_any_locale() {
        for stdout in [
            "",
            "\r\n",
            "INFO: No tasks are running which match the specified criteria.\r\n",
            // de-DE and fr-FR, including a comma, to prove we do not
            // depend on the text at all.
            "INFO: Es werden keine Aufgaben ausgeführt, die den angegebenen Kriterien entsprechen.\r\n",
            "INFOS: Aucune tâche en cours d'exécution ne correspond aux critères spécifiés.\r\n",
            // Even a localized line that happens to contain the pid.
            "INFO: no task 4242 here\r\n",
        ] {
            assert_eq!(
                tasklist_probe(stdout, 4242, None),
                PidProbe::Dead,
                "should be dead: {stdout:?}"
            );
        }
    }

    /// The pid-reuse guard: something is running under our pid, but it is
    /// not our program, so it must read as dead and never be killed.
    #[test]
    fn recycled_pid_running_another_image_is_not_ours() {
        let stolen = "\"chrome.exe\",\"4242\",\"Console\",\"1\",\"400,000 K\"";
        assert_eq!(
            tasklist_probe(stolen, 4242, Some("jamstreamd.exe")),
            PidProbe::Mismatch {
                running: "chrome.exe".to_owned()
            }
        );
    }

    /// One real row from `ps -p N -o stat=,etime=,comm=`, Linux shape
    /// (bare command name), kept verbatim.
    const PS_ROW: &str = "Ssl  02:17:43 jamstreamd\n";

    fn spawned(image: Option<&str>, started_unix: u64) -> Spawned<'_> {
        Spawned {
            image_name: image,
            started_unix,
        }
    }

    /// The reference point for the start-time half: 2h17m43s of elapsed
    /// time in PS_ROW means the process began at NOW minus 8263 s.
    const NOW: u64 = 1_800_000_000;
    const PS_ROW_STARTED: u64 = NOW - 8_263;

    #[test]
    fn ps_row_matching_the_entry_is_alive() {
        assert_eq!(
            ps_probe(PS_ROW, spawned(Some("jamstreamd"), PS_ROW_STARTED), NOW),
            PidProbe::Alive
        );
        // A registry from an older build records neither; the pid alone
        // still answers, which is the pre-existing behavior.
        assert_eq!(ps_probe(PS_ROW, spawned(None, 0), NOW), PidProbe::Alive);
        // The launch path records an absolute path, macOS reports argv[0],
        // and Linux truncates its own field at 15 characters. All three
        // have to compare equal to the same spawn.
        for (row, image) in [
            (
                "S 00:04 /usr/local/bin/jamstreamd",
                "/opt/jamstream/jamstreamd",
            ),
            ("S 00:04 jamstreamd", "jamstreamd"),
            ("S 00:04 jamstreamd-head", "jamstreamd-headless"),
        ] {
            assert_eq!(
                ps_probe(row, spawned(Some(image), 0), NOW),
                PidProbe::Alive,
                "{row:?} should match {image:?}"
            );
        }
    }

    #[test]
    fn ps_no_process_and_zombies_are_dead() {
        for stdout in ["", "\n", "   \n"] {
            assert_eq!(
                ps_probe(stdout, spawned(Some("jamstreamd"), 0), NOW),
                PidProbe::Dead,
                "should be dead: {stdout:?}"
            );
        }
        // A terminated child whose parent has not reaped it.
        assert_eq!(
            ps_probe(
                "Z+   00:01 jamstreamd",
                spawned(Some("jamstreamd"), NOW - 1),
                NOW
            ),
            PidProbe::Dead
        );
    }

    /// The defect this whole probe exists for: a stale entry names a pid
    /// that now belongs to somebody else, and `ps -p N` alone says yes.
    #[test]
    fn ps_recycled_pid_running_another_image_is_not_ours() {
        assert_eq!(
            ps_probe(
                "S    00:12 /usr/bin/ssh-agent",
                spawned(Some("jamstreamd"), NOW - 12),
                NOW
            ),
            PidProbe::Mismatch {
                running: "/usr/bin/ssh-agent".to_owned()
            }
        );
    }

    /// The half the image name cannot see: the pid came back around to
    /// another jamstreamd, so only the start time gives it away.
    #[test]
    fn ps_recycled_pid_running_the_same_image_is_caught_by_the_start_time() {
        // Our entry is a day old; what holds the pid started two hours ago.
        let entry_started = NOW - 86_400;
        let probe = ps_probe(PS_ROW, spawned(Some("jamstreamd"), entry_started), NOW);
        match probe {
            PidProbe::Mismatch { running } => assert!(running.contains("after the registry entry")),
            other => panic!("a day-old entry on a two-hour-old process must not match: {other:?}"),
        }
        // Inside the slack it is the same process seen through a stepped
        // clock, and destroying our own session must stay possible.
        assert_eq!(
            ps_probe(
                PS_ROW,
                spawned(Some("jamstreamd"), PS_ROW_STARTED - START_SLACK_SECS),
                NOW
            ),
            PidProbe::Alive
        );
        // A clock that went backwards leaves the process looking older than
        // its entry, which no recycled pid can be, so it is not a mismatch.
        assert_eq!(
            ps_probe(PS_ROW, spawned(Some("jamstreamd"), NOW), NOW),
            PidProbe::Alive
        );
    }

    #[test]
    fn etime_parses_every_posix_shape() {
        assert_eq!(parse_etime("00:06"), Some(6));
        assert_eq!(parse_etime("02:17:43"), Some(8_263));
        assert_eq!(parse_etime("46-15:21:05"), Some(4_029_665));
        assert_eq!(parse_etime("1-00:00:00"), Some(86_400));
        // Anything else must not be read as a start time at all.
        for junk in ["", "-", "12", "a:b", "1:2:3:4", "01:99"] {
            assert_eq!(parse_etime(junk), None, "{junk:?} is not an etime");
        }
        // An unreadable elapsed time leaves the image name as the only
        // check, rather than failing either way.
        assert_eq!(
            ps_probe("S ? jamstreamd", spawned(Some("jamstreamd"), 1), NOW),
            PidProbe::Alive
        );
        assert_eq!(
            ps_probe("S ? ssh-agent", spawned(Some("jamstreamd"), 1), NOW),
            PidProbe::Mismatch {
                running: "ssh-agent".to_owned()
            }
        );
    }

    #[test]
    fn tasklist_finds_our_row_among_several() {
        let out = format!("\"notepad.exe\",\"7\",\"Console\",\"1\",\"1 K\"\r\n{ROW}");
        assert_eq!(
            tasklist_probe(&out, 4242, Some("jamstreamd.exe")),
            PidProbe::Alive
        );
    }

    #[test]
    fn csv_rows_split_on_quoted_fields_only() {
        assert_eq!(csv_quoted_fields("\"a\",\"b\",\"c\""), vec!["a", "b", "c"]);
        // Unquoted lines are not rows.
        assert!(csv_quoted_fields("a,b,c").is_empty());
        assert!(csv_quoted_fields("").is_empty());
        // A field may contain commas and spaces.
        assert_eq!(csv_quoted_fields("\"x\",\"1,234 K\""), vec!["x", "1,234 K"]);
    }

    /// The provider half of the graceful-shutdown contract: the request is a
    /// file, and the wait for it only happens when the server proved it
    /// polls.
    #[test]
    fn shutdown_request_is_a_file_and_needs_the_support_marker() {
        let dir = temp_dir("sentinel");
        assert!(
            !graceful_shutdown_supported(&dir),
            "no marker means no graceful wait"
        );
        assert!(request_graceful_shutdown(&dir));
        assert!(shutdown_path(&dir).is_file(), "request must be on disk");

        // With the marker (which the server writes at startup) the wait is
        // worth spending; without it the platform goes straight to force.
        std::fs::write(dir.join(SHUTDOWN_SUPPORTED_FILE), b"").unwrap();
        assert!(graceful_shutdown_supported(&dir));

        // A reused session directory starts clean, or the next server would
        // exit at its first heartbeat.
        clear_shutdown_files(&dir);
        assert!(!shutdown_path(&dir).exists());
        assert!(!graceful_shutdown_supported(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unix must be unchanged: SIGTERM is delivered whether or not anything
    /// on disk claims support, so the full window is always spent. Windows
    /// only waits when the sentinel has a reader.
    #[test]
    fn term_grace_reflects_what_the_platform_can_actually_ask() {
        if cfg!(unix) {
            assert_eq!(process::term_grace(false), Duration::from_secs(5));
            assert_eq!(process::term_grace(true), Duration::from_secs(5));
        } else {
            assert_eq!(process::term_grace(false), Duration::ZERO);
            assert!(process::term_grace(true) > Duration::ZERO);
            assert!(
                process::term_grace(true) < Duration::from_secs(5),
                "destroy has a 5 s end-to-end budget and still has to force-kill after this"
            );
        }
    }

    #[test]
    fn private_dirs_are_created_recursively_and_idempotently() {
        let root = temp_dir("privdir");
        let nested = root.join("a").join("b").join("c");
        create_private_dir(&nested).unwrap();
        assert!(nested.is_dir());
        // Second call is a no-op, not an error.
        create_private_dir(&nested).unwrap();
        // And the point of it all: a file written inside is ours to read.
        let f = nested.join("config");
        write_private(&f, b"server_private_key=...").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"server_private_key=...");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Unix liveness against the one process we know everything about:
    /// this test binary.
    #[cfg(unix)]
    #[test]
    fn unix_liveness_matches_our_own_process() {
        let exe = std::env::current_exe().unwrap();
        let image = exe.file_name().unwrap().to_string_lossy().into_owned();
        let me = std::process::id();
        assert!(
            process::alive(me, spawned(Some(&image), 0)),
            "ps did not see this test process ({me}, {image})"
        );
        assert!(
            process::alive(me, spawned(None, 0)),
            "pid-only probe must see us too"
        );
        assert!(
            !process::alive(me, spawned(Some("definitely-not-jamstreamd"), 0)),
            "an image mismatch must read as dead so we never kill a stranger"
        );
        assert!(
            !process::alive(me, spawned(None, 1)),
            "a process that started long after its entry is not that entry's"
        );
    }

    /// The end-to-end shape of the same guard, and the reason it is not
    /// enough to test the parser: a stale registry entry must not get an
    /// unrelated process killed by `jamstream sweep`. By then the sweeper is
    /// a different process, so the in-process child handle that usually
    /// answers the liveness question is gone and only the registry and the
    /// platform probe are left.
    #[cfg(unix)]
    async fn stale_entry_leaves_the_process_alone(
        label: &str,
        tamper: impl Fn(&mut serde_json::Value),
    ) {
        let dir = temp_dir(label);
        let state = dir.join("state");
        let launcher = LocalProvider::new(state.clone()).with_server_binary(fake_server(&dir));
        let instance = launcher
            .launch(LaunchSpec {
                region: LocalProvider::local_region(),
                instance_class: InstanceClass::Small,
                user_data: "#cloud-config\n".to_owned(),
                tags: vec![session_tag("stale")],
            })
            .await
            .unwrap();
        let pid: u32 = instance.id.parse().unwrap();

        // The stand-in reaches its final image one exec after it starts,
        // which a real server binary does not, so wait that out before
        // asking the probe anything. Asked with `ps` directly, not through
        // the code under test.
        wait_for_image(pid, "fake-jamstreamd");

        let sweeper = LocalProvider::new(state.clone());
        assert_eq!(
            sweeper.list_tagged(None).await.unwrap().len(),
            1,
            "the probe must still recognize a server that really is ours"
        );

        let registry = state.join(REGISTRY_FILE);
        let mut entries: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
        tamper(&mut entries[0]);
        let tampered = serde_json::to_vec(&entries).unwrap();
        std::fs::write(&registry, &tampered).unwrap();

        let err = sweeper
            .destroy(&RegionId::new(REGION_ID), &instance.id)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::NotFound(_)),
            "a stale entry must read as already dead, got {err:?}"
        );
        assert!(
            pid_is_running(pid),
            "{label}: sweeping a stale entry killed the process that holds its pid"
        );

        // destroy dropped the entry; put it back to check the listing path,
        // which is what runs on every CLI launch.
        std::fs::write(&registry, &tampered).unwrap();
        assert!(
            sweeper.list_tagged(None).await.unwrap().is_empty(),
            "a stale entry must be pruned, not reported as a running session"
        );

        process::kill(pid);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Asks the OS directly rather than through the probe under test.
    #[cfg(unix)]
    fn pid_is_running(pid: u32) -> bool {
        Command::new("ps")
            .args(["-p", &pid.to_string()])
            .output()
            .is_ok_and(|out| out.status.success())
    }

    #[cfg(unix)]
    fn wait_for_image(pid: u32, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let out = Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "comm="])
                .output()
                .expect("ps");
            let running = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if running.rsplit('/').next() == Some(want) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the stand-in server never became {want}, ps says {running:?}"
            );
            std::thread::sleep(POLL);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_recycled_pid_running_another_image_survives_the_sweeper() {
        stale_entry_leaves_the_process_alone("stale-image", |entry| {
            entry["image_name"] = serde_json::json!("someone-elses-daemon");
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_pid_recycled_since_the_entry_was_written_survives_the_sweeper() {
        stale_entry_leaves_the_process_alone("stale-clock", |entry| {
            // Same image name, so only the start time separates our server
            // from a pid that came back around to another one.
            let day_old = now_unix() - 86_400;
            entry["started_unix"] = serde_json::json!(day_old);
        })
        .await;
    }

    /// Windows liveness against the one process we know everything about:
    /// this test binary. Runs on the CI Windows leg.
    #[cfg(windows)]
    #[test]
    fn windows_liveness_matches_our_own_process() {
        let exe = std::env::current_exe().unwrap();
        let image = exe.file_name().unwrap().to_string_lossy().into_owned();
        let me = std::process::id();
        assert!(
            process::alive(me, Some(&image)),
            "tasklist did not see this test process ({me}, {image})"
        );
        assert!(process::alive(me, None), "pid-only probe must see us too");
        assert!(
            !process::alive(me, Some("definitely-not-jamstreamd.exe")),
            "an image mismatch must read as dead so we never kill a stranger"
        );
    }

    /// A pid that has exited reads dead. The image name doubles as the
    /// anti-flake guard: if Windows recycled the pid in the microseconds
    /// after the wait, the recycled process is not cmd.exe.
    #[cfg(windows)]
    #[test]
    fn windows_liveness_sees_an_exited_process_as_dead() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!process::alive(pid, Some("cmd.exe")));
    }

    /// The ACL tightening must leave the directory usable by us: the worst
    /// outcome of getting icacls wrong is locking the host out of its own
    /// state. Also asserts inheritance is really broken, which is
    /// locale-independent: icacls marks inherited entries `(I)`.
    #[cfg(windows)]
    #[test]
    fn windows_new_dirs_lose_inherited_aces_and_stay_writable() {
        let root = temp_dir("acl");
        let dir = root.join("state").join("sessions").join("abc");
        create_private_dir(&dir).unwrap();

        let config = dir.join("config");
        write_private(&config, b"server_private_key=secret").unwrap();
        assert_eq!(
            std::fs::read(&config).unwrap(),
            b"server_private_key=secret"
        );

        let out = Command::new("icacls").arg(&dir).output().unwrap();
        assert!(out.status.success(), "icacls query failed");
        let acl = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            !acl.contains("(I)"),
            "inherited ACEs survived on a directory we created: {acl}"
        );
        let _ = std::fs::remove_dir_all(&root);
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
