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
//! compares the running image to the name that was launched (see the
//! platform notes below), and after an exec those differ, so a wrapped
//! server reads as not ours and never gets destroyed.
//!
//! # Registry
//!
//! Running sessions are tracked in `<state_dir>/local.json` (mode 0600),
//! one entry per spawn: pid, session id, config path, start time, and the
//! image file name that was spawned. The registry lives on disk, not in
//! memory, so a fresh provider on the same state dir (the sweeper story)
//! still finds and can destroy sessions an earlier process launched.
//! Liveness is verified on every list and dead entries are pruned. The
//! start token, the image name, and the start time are the pid-reuse
//! guard: see [`Spawned`] and [`classify`].
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
//! [`LocalProvider::with_bind`] confines a session to one address instead:
//! the server is told to listen on it and instances report it, so the
//! invites point where the server is. Loopback is the case that matters,
//! because it is the one path the macOS Application Firewall does not
//! filter.
//!
//! # Idle teardown and the session cap
//!
//! There is no systemd guard on a laptop, so the spawned server gets
//! `--idle-exit-min` from the config's `idle_shutdown_min` and exits on its
//! own once no musicians have been connected for that long. It likewise
//! gets `--max-duration-min` from the config's `max_duration_min` and
//! exits when the session has run that long, connected musicians or not.
//!
//! Those two windows are the only thing that ever ends a local session
//! nobody tears down by hand, because `jamstream host` returns as soon as
//! it has printed the invites and the server it spawned outlives it. So
//! reading them fails closed: a key that is missing or unreadable falls
//! back to [`DEFAULT_IDLE_SHUTDOWN_MIN`] and
//! [`DEFAULT_MAX_DURATION_MIN`], never to "no limit". A key that says 0
//! still means no limit, because that is a host saying so on purpose; the
//! absent case is the one that used to say it by accident.
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
//! The provider waits for the sentinel only when jamstreamd has left a
//! `<session dir>/shutdown.supported` marker to prove it polls. Without the
//! marker, teardown falls back to SIGTERM on unix and an immediate forced
//! kill on Windows, so an older binary costs nothing. The server half is
//! shipped: `--shutdown-file` in `jamstream_server`'s `main`, the marker and
//! the poll in its `runtime`.
//!
//! # Platform notes
//!
//! Both platforms answer the same question in one query: is this pid alive
//! *and still the process we launched*. A registry entry outlives its
//! process whenever the machine reboots, sleeps, or crashes, and the pid it
//! names is then free to belong to anyone; killing it on the strength of
//! the number alone is how a sweeper murders a stranger's process.
//!
//! On unix liveness is `libc::kill(pid, 0)`, no subprocess and no output
//! to parse (EPERM still reads as alive: a pid we cannot signal exists),
//! and identity comes from the platform's own books: `/proc/<pid>/stat`
//! plus `/proc/<pid>/exe` on Linux, `proc_pidinfo` plus `proc_pidpath` on
//! macOS. Each launch records the start token the platform reports for the
//! new pid (start ticks since boot on Linux, microsecond start time on
//! macOS); a probe *corroborates* a pid by matching that token exactly,
//! which no recycled pid can. The image name and the recorded wall-clock
//! start stay on as contradiction checks for entries an older build wrote
//! without a token. Zombies count as dead (a terminated child whose parent
//! has not reaped it must not look alive). Termination is SIGTERM with a
//! SIGKILL fallback after 5 s, both `libc::kill` - and the SIGKILL is only
//! ever sent to a corroborated pid. An entry with nothing to corroborate
//! (an older registry, a unix without a cheap identity read) still gets
//! the sentinel and the SIGTERM, and destroy says exactly what it skipped.
//!
//! On Windows the forced step is `taskkill /PID <pid> /T /F` and liveness
//! is an exact-match `tasklist /FI "PID eq <pid>" /NH /FO CSV` parse
//! ([`tasklist_probe`]) cross-checked against the image name, which is as
//! much identity as the platform offers at probe cost (see the note on
//! start times there); the match is also what corroborates a pid for the
//! forced kill. The Windows path is exercised by the `cfg(windows)` tests
//! below on the CI Windows runner; it has had no soak on real Windows
//! hardware.
//!
//! # File permissions
//!
//! The per-session config carries the server's private key and the registry
//! decides what gets signalled, so both go through [`crate::private`],
//! which is also what the CLI writes its own session records with.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::artifact::ServerArch;
use crate::cloudinit::flat_config_value;
use crate::private::{create_private_dir, write_private};
use crate::provider::{Provider, ProviderError, Result};
use crate::types::{
    IngressRule, Instance, LaunchSpec, Listing, Price, ProviderKind, Region, RegionId, session_tag,
};

const REGION_ID: &str = "local";
const REGISTRY_FILE: &str = "local.json";
/// Idle window used when the session config does not say, in minutes.
///
/// It has to agree with what the host surfaces offer, which is
/// `jamstream_session::limits::DEFAULT_IDLE_MIN`. This crate cannot see
/// that one, so `jamstream_cli::cli` holds the test that pins the two
/// together: a laptop is the wrong place for two defaults to disagree
/// quietly.
pub const DEFAULT_IDLE_SHUTDOWN_MIN: u32 = 10;
/// Session cap used when the session config does not say, in minutes.
/// Pinned against `jamstream_session::limits::DEFAULT_MAX_HOURS` the same
/// way [`DEFAULT_IDLE_SHUTDOWN_MIN`] is.
pub const DEFAULT_MAX_DURATION_MIN: u32 = 12 * 60;
/// Cross-process guard on the registry: see [`FileLock`].
const LOCK_FILE: &str = "local.json.lock";
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_POLL: Duration = Duration::from_millis(20);
/// A registry cycle is a few milliseconds of file work, so anything
/// holding the lock this long is a process that died holding it.
const LOCK_STALE: Duration = Duration::from_secs(30);
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
    /// Platform-native start token of the spawned process (start ticks
    /// since boot on Linux, microsecond start time on macOS), read at spawn
    /// and matched exactly by every later probe: it is the one field a
    /// recycled pid cannot reproduce. Absent in a registry an older build
    /// wrote, or on a platform with no cheap read for it; such entries are
    /// uncorroborated and never get the forced kill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proc_start: Option<u64>,
}

impl RegistryEntry {
    fn spawned(&self) -> Spawned<'_> {
        Spawned {
            image_name: self.image_name.as_deref(),
            started_unix: self.started_unix,
            proc_start: self.proc_start,
        }
    }
}

/// What the registry knows about a spawn beyond its pid, which is the only
/// thing that lets a liveness probe tell our server from whatever process
/// inherited the number. Any field may be absent in a registry written by
/// an older build; a probe skips the checks it cannot make and reports the
/// pid uncorroborated.
#[derive(Debug, Clone, Copy)]
struct Spawned<'a> {
    image_name: Option<&'a str>,
    /// Wall-clock second the spawn was recorded, or 0 when unknown. Only
    /// unix reconciles it; [`tasklist_probe`] explains why Windows cannot.
    #[cfg_attr(windows, allow(dead_code))]
    started_unix: u64,
    /// See [`RegistryEntry::proc_start`]. Windows records none, so the
    /// field is dead weight there by design.
    #[cfg_attr(windows, allow(dead_code))]
    proc_start: Option<u64>,
}

/// One registry entry's pid, as the probe judged it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    /// No process, a zombie, or a live pid that is demonstrably not our
    /// spawn: nothing to signal, safe to prune.
    Dead,
    /// Alive and corroborated as the process the entry describes, either by
    /// the start token (image name on Windows) or by the Child handle this
    /// very provider spawned it from.
    Ours,
    /// Alive with nothing recorded, or nothing obtainable, to corroborate
    /// against: listed, asked to exit, never force-killed.
    Unverified,
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
    /// One address for the spawned server to listen on and be reached at,
    /// instead of every interface and the primary LAN address. See
    /// [`LocalProvider::with_bind`].
    bind: Option<IpAddr>,
    /// Where the spawned server writes takes, and whether stems are
    /// captured alongside the mix; None means the session cannot record.
    /// See [`LocalProvider::with_record`].
    record: Option<(PathBuf, bool)>,
}

impl LocalProvider {
    pub fn new(state_dir: PathBuf) -> Self {
        LocalProvider {
            state_dir,
            server_binary: None,
            registry_gate: Mutex::new(()),
            children: Mutex::new(HashMap::new()),
            bind: None,
            record: None,
        }
    }

    /// Overrides binary resolution entirely (tests point this at a build
    /// artifact or a fake).
    pub fn with_server_binary(mut self, path: PathBuf) -> Self {
        self.server_binary = Some(path);
        self
    }

    /// Confines the session to one address: the spawned server is told to
    /// bind it, and it is the address the instance reports, so the invites
    /// minted from it point at the same place the server is listening.
    ///
    /// Without this the server binds every interface and invites carry the
    /// primary LAN address, which is what a band on one network needs and
    /// what this must keep defaulting to.
    ///
    /// With `127.0.0.1` the whole session stays on loopback, which is the
    /// one path the macOS Application Firewall does not filter. It filters
    /// incoming connections per binary, so every rebuilt jamstreamd raises
    /// a dialog, and on a managed Mac that dialog cannot be pre-answered
    /// from the command line. A test that spawns a real server binds
    /// loopback and never meets it.
    pub fn with_bind(mut self, ip: IpAddr) -> Self {
        self.bind = Some(ip);
        self
    }

    /// Arms recording for the sessions this provider launches: the spawned
    /// server gets `--record-dir dir`, plus `--record-stems` when `stems`
    /// is set, and every take the host then starts lands in `dir` as FLAC.
    pub fn with_record(mut self, dir: PathBuf, stems: bool) -> Self {
        self.record = Some((dir, stems));
        self
    }

    /// The address instances report: whatever [`with_bind`] was given, else
    /// the primary LAN address.
    ///
    /// [`with_bind`]: LocalProvider::with_bind
    fn reachable_ip(&self) -> IpAddr {
        self.bind.unwrap_or_else(primary_lan_ip)
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
            && let Some(sibling) = resolve_beside(dir)
        {
            return Ok(sibling);
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

    /// One load-modify-save cycle on the registry file, locked against
    /// every other one on this machine.
    fn with_registry<T>(&self, f: impl FnOnce(&mut Vec<RegistryEntry>) -> T) -> Result<T> {
        let _gate = self.registry_gate.lock().unwrap();
        create_private_dir(&self.state_dir).map_err(|e| {
            ProviderError::Other(format!(
                "cannot create state dir {}: {e}",
                self.state_dir.display()
            ))
        })?;
        let _lock = FileLock::acquire(&self.state_dir.join(LOCK_FILE));
        let path = self.registry_path();
        let mut entries: Vec<RegistryEntry> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                // Refusing forever would leave the sweeper unable to find a
                // single local session for the life of the file, and the
                // registry is recoverable state: pids and session ids, no
                // key material. Set it aside, say so, and carry on. What is
                // lost is the ability to destroy whatever it named, and
                // those servers still hold their own idle and duration
                // limits.
                let aside = path.with_extension("corrupt");
                tracing::warn!(
                    error = %err,
                    registry = %path.display(),
                    moved_to = %aside.display(),
                    "registry does not parse; starting a new one"
                );
                let _ = std::fs::rename(&path, &aside);
                Vec::new()
            }),
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

    /// What one pid is to us right now. Reaps children this provider
    /// spawned so their exit is visible immediately (a still-running child
    /// we hold the handle of is ours beyond doubt); foreign pids go through
    /// the platform probe, which takes what the registry recorded so a
    /// recycled pid cannot pass for ours.
    fn pid_liveness(&self, pid: u32, spawned: Spawned<'_>) -> Liveness {
        let mut children = self.children.lock().unwrap();
        if let Some(child) = children.get_mut(&pid) {
            match child.try_wait() {
                Ok(Some(_)) => {
                    children.remove(&pid);
                    return Liveness::Dead;
                }
                Ok(None) => return Liveness::Ours,
                Err(_) => {}
            }
        }
        drop(children);
        match process::probe(pid, spawned) {
            PidProbe::Dead => Liveness::Dead,
            PidProbe::Alive { corroborated: true } => Liveness::Ours,
            PidProbe::Alive {
                corroborated: false,
            } => Liveness::Unverified,
            // Loud, because the honest answer ("not ours") means we will
            // never signal this pid, so a real leak of our own server would
            // otherwise be invisible.
            PidProbe::Mismatch { running } => {
                tracing::warn!(
                    pid,
                    running = %running,
                    expected = spawned.image_name.unwrap_or(""),
                    "pid is alive but is not the process we launched; treating it as \
                     dead rather than signalling an unrelated process"
                );
                Liveness::Dead
            }
        }
    }

    /// True while the process runs, whether or not it could be corroborated.
    fn pid_alive(&self, pid: u32, spawned: Spawned<'_>) -> bool {
        self.pid_liveness(pid, spawned) != Liveness::Dead
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
    ///
    /// `#[must_use]` because this is the only thing that distinguishes a
    /// teardown that worked from one that reported success over a server still
    /// holding the port and the audio device. It waits, so a bare call reads
    /// like a deliberate pause rather than a dropped verdict.
    #[must_use]
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

    /// This machine's own architecture; local sessions download nothing,
    /// so the value is never used to pick an artifact.
    fn server_arch(&self) -> ServerArch {
        if cfg!(target_arch = "aarch64") {
            ServerArch::Aarch64
        } else {
            ServerArch::X86_64
        }
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
        let idle_min = self_limit(
            &spec.user_data,
            "idle_shutdown_min",
            DEFAULT_IDLE_SHUTDOWN_MIN,
        );
        let max_duration_min = self_limit(
            &spec.user_data,
            "max_duration_min",
            DEFAULT_MAX_DURATION_MIN,
        );

        // 0600 like everything else in here: a server log is a session's
        // roster, addresses, and whatever a future line decides to print.
        let log = create_log_file(&dir.join("server.log"))
            .map_err(|e| ProviderError::Other(format!("cannot create server log: {e}")))?;
        // --shutdown-file is the graceful-exit request path. jamstreamd's
        // argument scan ignores flags it does not know, so passing it to a
        // build that predates the server half costs nothing.
        let mut command = Command::new(&binary);
        command
            .arg("--config")
            .arg(&config_path)
            .arg("--activity-file")
            .arg(dir.join(crate::cloudinit::ACTIVITY_FILE_NAME))
            .arg("--shutdown-file")
            .arg(shutdown_path(&dir))
            .arg("--idle-exit-min")
            .arg(&idle_min)
            .arg("--max-duration-min")
            .arg(&max_duration_min);
        // Only when confined: without the flag jamstreamd binds every
        // interface, which is what a band on one network needs.
        if let Some(ip) = self.bind {
            command.arg("--bind").arg(ip.to_string());
        }
        // Recording is armed here and started by the host in session. The
        // directory is created now, so a session never launches whose first
        // take would fail on a directory that cannot exist, and it lives
        // outside the per-session directory because destroy removes that
        // and a take has to outlive its session.
        if let Some((record_dir, stems)) = &self.record {
            std::fs::create_dir_all(record_dir).map_err(|e| {
                ProviderError::Other(format!(
                    "cannot create record dir {}: {e}",
                    record_dir.display()
                ))
            })?;
            command.arg("--record-dir").arg(record_dir);
            if *stems {
                command.arg("--record-stems");
            }
        }
        let child = quiet(&mut command)
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
        // The start token the platform reports for the fresh pid, the exact
        // identity every later probe corroborates against. Read while the
        // process is certainly there: it is our unreaped child, so even an
        // instant exit leaves the token readable.
        let proc_start = process::start_token(pid);

        // Recorded before the readiness wait so even a botched startup is
        // visible to list_tagged and gets swept, never leaked.
        let started_unix = now_unix();
        let spawned = Spawned {
            image_name: image_name.as_deref(),
            started_unix,
            proc_start,
        };
        self.with_registry(|entries| {
            entries.push(RegistryEntry {
                pid,
                session: session.clone(),
                config_path: config_path.clone(),
                started_unix,
                image_name: image_name.clone(),
                proc_start,
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
            public_ip: Some(self.reachable_ip()),
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
        let liveness = self.pid_liveness(pid, spawned);
        if liveness == Liveness::Dead {
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
            // Insisting is only for a pid the registry can prove is still
            // our spawn. An entry with nothing to corroborate (older build,
            // platform without a cheap identity read) got the sentinel and
            // the polite request; a forced kill on the pid's say-so alone
            // is how a sweeper murders a stranger's process.
            if liveness == Liveness::Unverified {
                tracing::warn!(
                    pid,
                    "identity uncorroborated; skipping the forced {}",
                    process::FORCED_KILL
                );
                return Err(ProviderError::Other(format!(
                    "local instance {id} is still running after the shutdown request, \
                     and the registry entry cannot corroborate that pid {pid} is still \
                     the server it launched (entry from an older build?); skipped the \
                     forced {} - end the process by hand if it is yours",
                    process::FORCED_KILL
                )));
            }
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

    async fn list_tagged(&self, session_tag: Option<&str>) -> Result<Listing> {
        // Prune dead pids while listing; the registry only ever holds
        // sessions that were actually running at last look.
        let live = self.with_registry(|entries| {
            entries.retain(|e| self.pid_alive(e.pid, e.spawned()));
            entries.clone()
        })?;
        let ip = self.reachable_ip();
        // One registry, one region: either it was read or this returned an
        // error, so there is never a partial answer here.
        Ok(Listing::complete(
            live.iter()
                .filter(|e| session_tag.is_none_or(|want| e.session == want))
                .map(|e| Self::instance_for(e, ip))
                .collect(),
        ))
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

/// A best-effort mutual exclusion between processes, held for the length
/// of one registry cycle.
///
/// The desktop app sweeps on launch while the CLI is mid-`host`, and both
/// do load-modify-save on the same file; the in-process mutex says nothing
/// about that, and the loser's entry disappears, leaving a server nothing
/// will ever destroy. `create_new` is the primitive every platform has
/// here, without a dependency and without flock's differences between
/// Windows, Linux, and macOS.
///
/// Best effort in two specific ways, both deliberate:
///
/// * a holder that died leaves the file behind, so a lock older than
///   [`LOCK_STALE`] is taken from it. A registry cycle is a few
///   milliseconds of file work, so nothing legitimate holds one that long;
/// * failing to take the lock does not fail the operation. The cycle it
///   guards is what the CLI is for, and refusing to run a command because
///   another process is holding a file would be a worse failure than the
///   rare lost update it prevents.
struct FileLock {
    path: PathBuf,
    held: bool,
}

impl FileLock {
    fn acquire(path: &Path) -> Self {
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => {
                    return FileLock {
                        path: path.to_owned(),
                        held: true,
                    };
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(path) {
                        tracing::warn!(lock = %path.display(), "breaking a stale registry lock");
                        let _ = std::fs::remove_file(path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        tracing::warn!(
                            lock = %path.display(),
                            "registry lock is held elsewhere; proceeding unlocked"
                        );
                        return FileLock {
                            path: path.to_owned(),
                            held: false,
                        };
                    }
                    std::thread::sleep(LOCK_POLL);
                }
                Err(err) => {
                    tracing::warn!(error = %err, lock = %path.display(), "cannot take the registry lock");
                    return FileLock {
                        path: path.to_owned(),
                        held: false,
                    };
                }
            }
        }
    }
}

fn lock_is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|m| m.elapsed().unwrap_or_default() > LOCK_STALE)
        // A lock file we cannot stat is one we cannot wait on either.
        .unwrap_or(true)
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The spawned server's stdout and stderr. Truncating rather than
/// appending, as `File::create` was: a relaunched session starts a new log.
fn create_log_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// On Windows every child spawn carries CREATE_NO_WINDOW: the desktop app
/// is built for the GUI subsystem (crates/client/src/main.rs), and a GUI
/// parent has no console to lend, so without the flag each spawned console
/// binary (jamstreamd, tasklist, taskkill) pops its own window.
#[cfg(windows)]
fn quiet(command: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(not(windows))]
fn quiet(command: &mut Command) -> &mut Command {
    command
}

/// One path component for a session id: the object-key rule from
/// [`crate::storage::sanitize_component`], with the dots taken out as well.
///
/// Session ids are lowercase hex, so in practice nothing is touched. The dot is
/// the one difference from the key rule, and it is what makes this worth
/// having: destroy calls `remove_dir_all` on the directory this names, and a
/// session of ".." would have named the parent, taking every other session's
/// directory with it. An empty id would have named the parent too, by naming
/// nothing at all, which is why the shared rule yields `unnamed`.
fn fs_safe(s: &str) -> String {
    crate::storage::sanitize_component(s).replace('.', "-")
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
///
/// `#[must_use]` because the write is the visible part and the answer is not:
/// a caller that asks and drops the reply goes on to wait out the grace period
/// for a request that never landed, then force-kills a server mid-upload. The
/// grace is only owed to a server that was actually asked.
#[must_use]
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

/// One self-exit window for the spawned server, as the string its flag
/// takes. jamstreamd reads fractional minutes, so the value is passed
/// through verbatim once it parses rather than rounded to whole minutes.
///
/// The fallback is the point of this function. A local server has no
/// external guard and no parent left to notice it: whatever these two
/// windows say is the entire lifetime policy. Reading "I could not find
/// the key" as "run forever" put six of them on a laptop for an afternoon,
/// so an absent or unreadable value takes the documented default and says
/// so in the log. Zero is left alone: it means no limit, and a host who
/// typed it meant it.
fn self_limit(user_data: &str, key: &str, fallback: u32) -> String {
    let Some(raw) = flat_config_value(user_data, key) else {
        tracing::warn!(
            key,
            fallback,
            "session config carries no window; using the default rather than none"
        );
        return fallback.to_string();
    };
    match raw.parse::<f64>() {
        Ok(minutes) if minutes.is_finite() && minutes >= 0.0 => raw.to_owned(),
        _ => {
            tracing::warn!(
                key,
                value = raw,
                fallback,
                "session config window is not a number of minutes; using the default"
            );
            fallback.to_string()
        }
    }
}

/// The app-adjacent step of the resolution order: the `jamstreamd` release
/// artifacts place in `dir`, which production always passes as the current
/// executable's own directory. Split out so the rule is testable against a
/// directory the test owns: an exe-named fixture in the live target/ dir
/// loses sharing-violation races on Windows.
fn resolve_beside(dir: &Path) -> Option<PathBuf> {
    let sibling = dir.join(BIN_NAME);
    sibling.is_file().then_some(sibling)
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
///
/// Public because minting needs the same answer: an invite naming this
/// address is an invite to a server on this machine, which is what lets
/// the host offer loopback alongside it.
pub fn primary_lan_ip() -> IpAddr {
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
    /// A live process that contradicts nothing the registry recorded.
    /// `corroborated` is the stronger claim: the platform positively
    /// matched the identity recorded at spawn (the start token on unix, the
    /// image name on Windows), which is what earns the forced kill.
    Alive { corroborated: bool },
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
/// Start times are the stronger check and unix reads them (see
/// [`classify`]), but Windows has no cheap column for them: `wmic process
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
            // The image match is the identity check Windows has, so it is
            // also what corroborates the pid; with nothing recorded the pid
            // is alive on its own say-so and never gets the forced kill.
            Some(_) => PidProbe::Alive { corroborated: true },
            None => PidProbe::Alive {
                corroborated: false,
            },
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

/// How far a process's observed start time may sit after the moment the
/// registry recorded the spawn before the entry is judged stale.
///
/// The window is wide on purpose. The platform's start time and
/// `started_unix` were read from clocks that can step apart (an NTP
/// correction after a laptop resume shifts one and not the other), and
/// being wrong in that direction means refusing to destroy a session that
/// is genuinely ours. Five minutes absorbs any such correction while still
/// catching what this check is for, an entry that outlived a reboot.
#[cfg(any(unix, test))]
const START_SLACK_SECS: u64 = 300;

/// Longest command name Linux keeps (`TASK_COMM_LEN` minus the terminator),
/// so anything at exactly this length may have been truncated.
#[cfg(any(unix, test))]
const COMM_MAX: usize = 15;

/// What the platform could see about the process currently holding a pid,
/// filled by the per-OS `identity` readers in [`process`]. Every field the
/// platform cannot supply stays empty and [`classify`] skips its check.
#[cfg(any(unix, test))]
#[derive(Debug, Default)]
struct Observed {
    /// A terminated child whose parent has not reaped it: not alive.
    zombie: bool,
    /// Platform-native start token, comparable only to a
    /// [`RegistryEntry::proc_start`] recorded on the same machine.
    start_token: Option<u64>,
    /// Wall-clock second the process started, when derivable.
    start_unix: Option<u64>,
    /// Every name the platform gives the running image (the kernel's comm,
    /// the executable path); matching any one of them clears the check,
    /// because they legitimately disagree after an exec through a symlink.
    images: Vec<String>,
}

/// Judges what the platform observed against what the registry recorded,
/// one platform-independent rulebook for both:
///
/// * the start token is the identity: an exact match is the one thing a
///   recycled pid cannot fake, so it corroborates the pid outright, and a
///   token that differs is a recycled pid however right the name looks;
/// * the image name and the wall-clock start are contradiction checks for
///   entries with no token (a registry an older build wrote): a process
///   running some other image, or one younger than its entry by more than
///   [`START_SLACK_SECS`], is not the process the entry describes.
///
/// A mismatch reads as dead and the caller never signals it: refusing to
/// kill a stranger's process is the only acceptable way to be wrong here.
/// Contradicting nothing while matching no token reads alive but
/// uncorroborated, which destroy honors by never escalating past SIGTERM.
#[cfg(any(unix, test))]
fn classify(observed: &Observed, spawned: Spawned<'_>) -> PidProbe {
    if observed.zombie {
        return PidProbe::Dead;
    }
    if let (Some(seen), Some(recorded)) = (observed.start_token, spawned.proc_start) {
        if seen == recorded {
            return PidProbe::Alive { corroborated: true };
        }
        return PidProbe::Mismatch {
            running: format!(
                "{}, start token {seen} against recorded {recorded}",
                observed.images.first().map_or("unknown image", |s| s)
            ),
        };
    }
    if let Some(want) = spawned.image_name
        && !observed.images.is_empty()
        && !observed
            .images
            .iter()
            .any(|image| same_command(image, want))
    {
        return PidProbe::Mismatch {
            running: observed.images.join(", "),
        };
    }
    if spawned.started_unix > 0
        && let Some(started) = observed.start_unix
        && started > spawned.started_unix + START_SLACK_SECS
    {
        return PidProbe::Mismatch {
            running: format!(
                "{}, started {}s after the registry entry",
                observed.images.first().map_or("unknown image", |s| s),
                started - spawned.started_unix
            ),
        };
    }
    PidProbe::Alive {
        corroborated: false,
    }
}

/// File-name comparison for what the platform reported against the image
/// recorded at launch, tolerating Linux's truncation of long names.
#[cfg(any(unix, test))]
fn same_command(observed: &str, expected: &str) -> bool {
    fn file_name(s: &str) -> &str {
        s.rsplit('/').next().unwrap_or(s)
    }
    let observed = file_name(observed);
    let expected = file_name(expected);
    observed == expected || (observed.len() >= COMM_MAX && expected.starts_with(observed))
}

/// A helper the platform provides, resolved to an absolute path. Windows
/// only, now that unix probes and signals through libc instead of spawning
/// anything.
///
/// Every one of these used to go through `PATH`, so a writable directory
/// early in it meant code execution on every liveness probe, in a process
/// that is about to signal something. The candidates below are where the
/// platform actually keeps these; the bare name is the last resort, for an
/// unusual layout, and it is the only case that reads `PATH` at all.
#[cfg(windows)]
fn system_tool(name: &str, candidates: &[&str]) -> PathBuf {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

#[cfg(unix)]
mod process {
    use std::time::Duration;

    use super::{PidProbe, Spawned};

    /// Named in the error when a process outlives the forced step.
    pub const FORCED_KILL: &str = "SIGKILL";

    /// One pid judged against one registry entry, no subprocess anywhere:
    /// `kill(pid, 0)` answers existence and `identity` reads the rest from
    /// the platform's own books. [`super::classify`] holds the rules.
    pub fn probe(pid: u32, spawned: Spawned<'_>) -> PidProbe {
        if !exists(pid) {
            return PidProbe::Dead;
        }
        match identity::observe(pid) {
            Some(observed) => super::classify(&observed, spawned),
            // Alive a moment ago but unobservable now: losing the race with
            // an exit is the common way here, so ask existence again before
            // settling for "alive with nothing to check".
            None if !exists(pid) => PidProbe::Dead,
            None => PidProbe::Alive {
                corroborated: false,
            },
        }
    }

    /// Signal 0 delivers nothing but still runs the kernel's existence and
    /// permission checks: 0 and EPERM are a live pid (EPERM is somebody
    /// else's, which the identity checks then rule out), ESRCH is none.
    fn exists(pid: u32) -> bool {
        if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// The start token to record for a fresh spawn; see
    /// [`super::RegistryEntry::proc_start`]. None where the platform has no
    /// cheap read for it, which leaves the entry uncorroborated.
    pub fn start_token(pid: u32) -> Option<u64> {
        identity::observe(pid).and_then(|observed| observed.start_token)
    }

    /// SIGTERM is a real request that the kernel delivers whether or not
    /// anything on disk says the server understands it, so unix always
    /// spends the full window before escalating - unchanged behavior.
    pub fn term_grace(_sentinel_honored: bool) -> Duration {
        Duration::from_secs(5)
    }

    /// The polite step. An error (already gone, not ours to signal) is
    /// handled by the wait-and-escalate above this call.
    pub fn terminate(pid: u32) {
        let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }

    /// The forced step; only ever sent to a corroborated pid.
    pub fn kill(pid: u32) {
        let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }

    /// What Linux keeps in `/proc`: one read of `/proc/<pid>/stat` gives
    /// the state, the start ticks, and the comm; `/proc/<pid>/exe` adds the
    /// untruncated executable path when the process is ours to inspect.
    #[cfg(target_os = "linux")]
    mod identity {
        use std::sync::OnceLock;

        use super::super::Observed;

        pub fn observe(pid: u32) -> Option<Observed> {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let (comm, state, start_ticks) = parse_stat(&stat)?;
            let mut images = Vec::new();
            if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
                let exe = exe.to_string_lossy();
                // A rebuilt server runs on from a replaced binary, and the
                // kernel marks the link rather than lying about it; the
                // image is still the one the registry recorded.
                images.push(exe.strip_suffix(" (deleted)").unwrap_or(&exe).to_owned());
            }
            if !comm.is_empty() {
                images.push(comm);
            }
            Some(Observed {
                zombie: state == 'Z',
                start_token: Some(start_ticks),
                start_unix: start_unix(start_ticks),
                images,
            })
        }

        /// `pid (comm) state ...` with the start ticks in field 22. The
        /// comm may contain spaces and parentheses, so the parse anchors on
        /// the last ')' rather than splitting the whole line.
        fn parse_stat(stat: &str) -> Option<(String, char, u64)> {
            let open = stat.find('(')?;
            let close = stat.rfind(')')?;
            let comm = stat.get(open + 1..close)?.to_owned();
            let rest: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
            let state = rest.first()?.chars().next()?;
            // Field 22 of the row; the state at index 0 here is field 3.
            let start_ticks = rest.get(19)?.parse().ok()?;
            Some((comm, state, start_ticks))
        }

        /// Wall-clock start reconstructed from boot time plus the ticks,
        /// for reconciling entries that recorded only `started_unix`.
        fn start_unix(start_ticks: u64) -> Option<u64> {
            static BTIME: OnceLock<Option<u64>> = OnceLock::new();
            let btime = (*BTIME.get_or_init(|| {
                let stat = std::fs::read_to_string("/proc/stat").ok()?;
                stat.lines()
                    .find_map(|line| line.strip_prefix("btime "))?
                    .trim()
                    .parse()
                    .ok()
            }))?;
            let hz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })
                .ok()
                .filter(|&hz| hz > 0)?;
            Some(btime + start_ticks / hz)
        }
    }

    /// What macOS answers through libproc: `proc_pidinfo` with the BSD-info
    /// flavor, and `proc_pidpath` for the untruncated executable path of a
    /// process ours to inspect.
    #[cfg(target_os = "macos")]
    mod identity {
        use super::super::Observed;

        pub fn observe(pid: u32) -> Option<Observed> {
            let pid = i32::try_from(pid).ok()?;
            let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
            let got = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            if got != size {
                // The kernel answers ESRCH for a zombie even while
                // `kill(pid, 0)` still says the pid exists (measured on
                // macOS 15; the zombie fallback in XNU's proc_info does not
                // reach this flavor). Every caller has just proven the pid
                // exists, so ESRCH here is an exited process, reaped or
                // not: dead either way.
                if got == 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                    return Some(Observed {
                        zombie: true,
                        ..Observed::default()
                    });
                }
                return None;
            }
            let info = unsafe { info.assume_init() };
            let mut images = Vec::new();
            if let Some(path) = exe_path(pid) {
                images.push(path);
            }
            // pbi_name is the longer of the kernel's two name fields and
            // falls back to the 16-byte comm; either is truncated, which
            // same_command tolerates.
            if let Some(name) = cstr(&info.pbi_name).or_else(|| cstr(&info.pbi_comm)) {
                images.push(name);
            }
            Some(Observed {
                zombie: info.pbi_status == libc::SZOMB,
                start_token: Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec),
                start_unix: Some(info.pbi_start_tvsec),
                images,
            })
        }

        fn exe_path(pid: i32) -> Option<String> {
            let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
            let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
            let len = usize::try_from(len).ok().filter(|&len| len > 0)?;
            buf.truncate(len);
            String::from_utf8(buf).ok()
        }

        /// The readable prefix of a fixed-size, NUL-terminated name field.
        fn cstr(field: &[libc::c_char]) -> Option<String> {
            let bytes: Vec<u8> = field
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            if bytes.is_empty() {
                return None;
            }
            String::from_utf8(bytes).ok()
        }
    }

    /// Every other unix: `kill(pid, 0)` still answers liveness, but there
    /// is no cheap identity read, so pids are never corroborated and the
    /// forced kill is never sent. destroy says so when it matters.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    mod identity {
        use super::super::Observed;

        pub fn observe(_pid: u32) -> Option<Observed> {
            None
        }
    }
}

#[cfg(windows)]
mod process {
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::OnceLock;
    use std::time::Duration;

    use super::{PidProbe, Spawned, system_tool};

    /// `%SystemRoot%\System32` is where both of these live; the constant
    /// path is the fallback for an environment with no SystemRoot set.
    fn system32(name: &str) -> PathBuf {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_owned());
        let configured = format!("{root}\\System32\\{name}");
        let default = format!("C:\\Windows\\System32\\{name}");
        system_tool(name, &[configured.as_str(), default.as_str()])
    }

    fn tasklist() -> &'static PathBuf {
        static TASKLIST: OnceLock<PathBuf> = OnceLock::new();
        TASKLIST.get_or_init(|| system32("tasklist.exe"))
    }

    fn taskkill() -> &'static PathBuf {
        static TASKKILL: OnceLock<PathBuf> = OnceLock::new();
        TASKKILL.get_or_init(|| system32("taskkill.exe"))
    }

    pub const FORCED_KILL: &str = "taskkill /F";

    /// Two ticks of the server's 1 s activity heartbeat, which is where the
    /// sentinel poll belongs. Deliberately far short of unix's 5 s: destroy
    /// plus the 2 s forced-kill wait has to finish inside the 5 s budget
    /// crates/server/tests/local_provider.rs asserts, and on Windows the
    /// wait is followed by a kill that always works, so a longer window
    /// buys nothing but teardown latency.
    const SENTINEL_GRACE: Duration = Duration::from_secs(2);

    /// See [`super::tasklist_probe`] for the parse and the pid-reuse rule.
    pub fn probe(pid: u32, spawned: Spawned<'_>) -> PidProbe {
        match super::quiet(Command::new(tasklist()).args([
            "/FI",
            &format!("PID eq {pid}"),
            "/NH",
            "/FO",
            "CSV",
        ]))
        .output()
        {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                super::tasklist_probe(&text, pid, spawned.image_name)
            }
            Err(err) => {
                tracing::warn!(pid, error = %err, "cannot run tasklist; treating the pid as dead");
                PidProbe::Dead
            }
        }
    }

    /// Windows records no start token ([`super::tasklist_probe`] explains
    /// why start times are off the table); the image name carries identity.
    pub fn start_token(_pid: u32) -> Option<u64> {
        None
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
        let _ =
            super::quiet(Command::new(taskkill()).args(["/PID", &pid.to_string(), "/T"])).output();
    }

    /// The SIGKILL equivalent: kill the process and its children outright.
    /// `output()` rather than `status()` so taskkill's "SUCCESS: the
    /// process ... has been terminated" chatter does not land in the CLI's
    /// own stdout.
    pub fn kill(pid: u32) {
        let _ = super::quiet(Command::new(taskkill()).args(["/PID", &pid.to_string(), "/T", "/F"]))
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::assert_provider_contract;
    use crate::types::InstanceClass;

    /// Private (0700 on unix), because the system temp dir's parent is
    /// world-writable and some of what lands here gets executed.
    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jamstream-local-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        create_private_dir(&dir).unwrap();
        dir
    }

    /// Body of a Windows stand-in server: `prelude` lines run first, then it
    /// waits out the test on a pinned ping, because `timeout` refuses the
    /// redirected stdin every spawn here has. CRLF throughout: cmd's label
    /// scanner is only dependable with it.
    #[cfg(windows)]
    fn cmd_body(prelude: &str) -> String {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_owned());
        format!("@echo off\r\n{prelude}\"{root}\\System32\\ping.exe\" -n 601 127.0.0.1 > nul\r\n")
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

    /// The Windows stand-in is a `.cmd`, which std runs through cmd.exe, so
    /// the pid the provider records belongs to cmd.exe while the recorded
    /// image is `fake-jamstreamd.cmd`: the tasklist probe would read that as
    /// a recycled pid. That confines this fake to tests where liveness is
    /// answered by the launching provider's own child handle, or where the
    /// probe is meant to find nothing to corroborate; the identity-sensitive
    /// sweeper tests stay unix.
    #[cfg(windows)]
    fn fake_server(dir: &Path) -> PathBuf {
        let path = dir.join("fake-jamstreamd.cmd");
        std::fs::write(&path, cmd_body("")).unwrap();
        path
    }

    /// [`fake_server`] with one extra line: it writes the arguments it was
    /// spawned with, one per line, before exec'ing. Everything else about
    /// it, the symlinked image and why, is the same.
    #[cfg(unix)]
    fn recording_server(dir: &Path, args_file: &Path) -> PathBuf {
        let script = fake_server(dir);
        let body = std::fs::read_to_string(&script).unwrap();
        let exec = body.lines().last().unwrap().to_owned();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n{exec}\n",
                args_file.display()
            ),
        )
        .unwrap();
        script
    }

    /// [`fake_server`] that first writes its argv, one per line. `%~1`
    /// strips the quotes cmd wrapped around path-shaped arguments; the dump
    /// is appended a line per open, so it goes to a temporary and lands with
    /// one move, or [`read_when_written`] could return a half-written file.
    #[cfg(windows)]
    fn recording_server(dir: &Path, args_file: &Path) -> PathBuf {
        let path = dir.join("fake-jamstreamd.cmd");
        let tmp = args_file.with_extension("tmp");
        let prelude = format!(
            ":args\r\n\
             if \"%~1\"==\"\" goto run\r\n\
             >> \"{tmp}\" echo %~1\r\n\
             shift\r\n\
             goto args\r\n\
             :run\r\n\
             move /y \"{tmp}\" \"{args}\" > nul\r\n",
            tmp = tmp.display(),
            args = args_file.display()
        );
        std::fs::write(&path, cmd_body(&prelude)).unwrap();
        path
    }

    /// Reads a file a spawned process is expected to write, waiting for it
    /// rather than assuming it is already there.
    async fn read_when_written(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && !text.is_empty()
            {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "{} was never written",
                path.display()
            );
            tokio::time::sleep(POLL).await;
        }
    }

    #[tokio::test]
    async fn local_provider_passes_contract() {
        let dir = temp_dir("contract");
        let provider = LocalProvider::new(dir.join("state")).with_server_binary(fake_server(&dir));
        assert_provider_contract(&provider).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fallback is the whole point: nothing else ends a local session
    /// once `jamstream host` has returned, so "I could not read the window"
    /// must not mean "no window". An explicit 0 is still a host saying no
    /// limit, and fractional minutes still reach jamstreamd unrounded.
    #[test]
    fn an_unreadable_window_falls_back_to_the_default_not_to_forever() {
        let present = "idle_shutdown_min = 3\nmax_duration_min = 90\n";
        assert_eq!(self_limit(present, "idle_shutdown_min", 10), "3");
        assert_eq!(self_limit(present, "max_duration_min", 720), "90");

        // Absent, empty, and unparseable all take the default.
        for text in [
            "",
            "port = 43210\n",
            "idle_shutdown_min =\n",
            "idle_shutdown_min = ten\n",
            "idle_shutdown_min = -1\n",
            "idle_shutdown_min = NaN\n",
            "idle_shutdown_min = inf\n",
        ] {
            assert_eq!(
                self_limit(text, "idle_shutdown_min", 10),
                "10",
                "{text:?} must not disable the idle exit"
            );
        }

        // Zero survives: it is the one way to ask for no limit on purpose.
        assert_eq!(
            self_limit("idle_shutdown_min = 0\n", "idle_shutdown_min", 10),
            "0"
        );
        // jamstreamd takes fractional minutes; passing the text through
        // rather than a parsed integer is what keeps 0.05 meaning 3 s.
        assert_eq!(
            self_limit("idle_shutdown_min = 0.05\n", "idle_shutdown_min", 10),
            "0.05"
        );
    }

    /// The other half of the same contract: the resolved window has to
    /// reach the process. A fake server that records its own argv proves
    /// the flag is spelled the way jamstreamd's argument scan reads it.
    #[tokio::test]
    async fn a_config_with_no_windows_still_spawns_a_server_that_will_exit() {
        let dir = temp_dir("windows");
        let args_file = dir.join("argv");
        let provider = LocalProvider::new(dir.join("state"))
            .with_server_binary(recording_server(&dir, &args_file));
        let spec = LaunchSpec {
            region: LocalProvider::local_region(),
            instance_class: InstanceClass::Small,
            // Deliberately silent about both windows.
            user_data: "port = 43210\n".to_owned(),
            tags: vec![session_tag("nowindows")],
        };
        let instance = provider.launch(spec).await.unwrap();
        // Readiness says the process is alive, not that the shell inside it
        // has reached its first line; a machine busy running the rest of
        // this suite can take a moment over that.
        let argv = read_when_written(&args_file).await;
        let args: Vec<&str> = argv.lines().collect();
        let value_of = |flag: &str| {
            args.iter()
                .position(|a| *a == flag)
                .and_then(|i| args.get(i + 1))
                .copied()
        };
        assert_eq!(
            value_of("--idle-exit-min"),
            Some(DEFAULT_IDLE_SHUTDOWN_MIN.to_string().as_str())
        );
        assert_eq!(
            value_of("--max-duration-min"),
            Some(DEFAULT_MAX_DURATION_MIN.to_string().as_str())
        );
        // Recording stays opt-in: an unconfigured provider arms nothing.
        assert!(!args.contains(&"--record-dir"));
        assert!(!args.contains(&"--record-stems"));
        provider
            .destroy(&RegionId::new(REGION_ID), &instance.id)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The recording half of the same spawn contract: `with_record` reaches
    /// the process as the flags jamstreamd's argument scan reads, and the
    /// directory exists before the first take could need it.
    #[tokio::test]
    async fn with_record_reaches_the_spawned_server_as_flags() {
        let dir = temp_dir("record");
        let args_file = dir.join("argv");
        let record_dir = dir.join("takes");
        let provider = LocalProvider::new(dir.join("state"))
            .with_server_binary(recording_server(&dir, &args_file))
            .with_record(record_dir.clone(), true);
        let spec = LaunchSpec {
            region: LocalProvider::local_region(),
            instance_class: InstanceClass::Small,
            user_data: "port = 43210\n".to_owned(),
            tags: vec![session_tag("recorded")],
        };
        let instance = provider.launch(spec).await.unwrap();
        let argv = read_when_written(&args_file).await;
        // Torn down before anything is asserted, so a failure here never
        // leaves a process behind.
        provider
            .destroy(&RegionId::new(REGION_ID), &instance.id)
            .await
            .unwrap();

        let args: Vec<&str> = argv.lines().collect();
        let value_of = |flag: &str| {
            args.iter()
                .position(|a| *a == flag)
                .and_then(|i| args.get(i + 1))
                .copied()
        };
        assert_eq!(value_of("--record-dir"), record_dir.to_str());
        assert!(args.contains(&"--record-stems"), "stems were asked for");
        assert!(
            record_dir.is_dir(),
            "the record dir is created at launch, not at the first take"
        );
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
            provider
                .list_tagged(None)
                .await
                .unwrap()
                .instances
                .is_empty(),
            "failed launch must not leave a registry entry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A confined session has to be *offered* where it listens. Reporting
    /// the LAN address for a server bound to loopback would mint invites
    /// pointing at a port nothing answers on, which is the same silence a
    /// firewall produces and just as hard to read.
    #[tokio::test]
    async fn a_bound_session_is_reported_at_the_address_it_listens_on() {
        let dir = temp_dir("bind");
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let provider = LocalProvider::new(dir.join("state"))
            .with_server_binary(fake_server(&dir))
            .with_bind(loopback);
        let spec = LaunchSpec {
            region: LocalProvider::local_region(),
            instance_class: InstanceClass::Small,
            user_data: "port = 43210\n".to_owned(),
            tags: vec![session_tag("bound")],
        };
        let instance = provider.launch(spec).await.unwrap();
        let listed = provider.list_tagged(Some("bound")).await.unwrap();
        // Torn down before anything is asserted: a failing assertion must
        // not be the reason a server outlives this process.
        provider
            .destroy(&RegionId::new(REGION_ID), &instance.id)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(instance.public_ip, Some(loopback));
        assert_eq!(listed.instances.len(), 1);
        assert_eq!(listed.instances[0].public_ip, Some(loopback));
    }

    /// The default is the whole point of the flag being a flag: an
    /// unconfined provider still binds every interface and still offers the
    /// LAN address, so a band on one network is unaffected.
    #[test]
    fn an_unconfined_provider_still_offers_the_lan_address() {
        let provider = LocalProvider::new(PathBuf::from("/state"));
        assert_eq!(provider.reachable_ip(), primary_lan_ip());
        assert!(provider.bind.is_none());
    }

    /// The app-bundling story: release artifacts place jamstreamd beside
    /// the app/CLI binary, and resolution must find it there with no
    /// override, no env var, and no PATH entry. Asserted against a private
    /// directory standing in for the install dir: this test used to drop
    /// its fixture beside the real test executable, and on Windows a new
    /// exe-named file in the shared target/ dir loses sharing-violation
    /// races with the scanners watching it.
    #[test]
    fn resolves_the_binary_beside_the_current_executable() {
        let dir = temp_dir("adjacent");
        assert_eq!(resolve_beside(&dir), None, "an empty dir offers nothing");
        let sibling = dir.join(BIN_NAME);
        // Resolution only asks whether the file exists; nothing runs it.
        std::fs::write(&sibling, b"stand-in for a bundled jamstreamd\n").unwrap();
        assert_eq!(resolve_beside(&dir), Some(sibling));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `destroy` removes the session directory, so the id it comes from has
    /// to stay inside `sessions/`. Session ids are hex and none of this is
    /// reachable today, which is the point: it is reachable the moment
    /// something upstream stops checking.
    #[test]
    fn a_session_id_cannot_name_a_directory_outside_its_own() {
        let provider = LocalProvider::new(PathBuf::from("/state"));
        let sessions = PathBuf::from("/state").join("sessions");
        for id in ["..", ".", "../..", "a/../..", "..\\..", ""] {
            let dir = provider.session_dir(id);
            assert_eq!(
                dir.parent(),
                Some(sessions.as_path()),
                "session {id:?} escaped to {dir:?}"
            );
            assert_ne!(dir, sessions, "session {id:?} named the sessions directory");
        }
        // The real thing is untouched, and so is anything readable.
        assert_eq!(fs_safe("deadbeefcafef00d"), "deadbeefcafef00d");
        assert_eq!(fs_safe("my_session-1"), "my_session-1");
    }

    /// A one-byte flip in the registry used to fail every local operation
    /// for the life of the file, and `sweep` logged that and carried on, so
    /// local sweeping was off with nothing to show for it.
    #[tokio::test]
    async fn a_corrupt_registry_is_set_aside_rather_than_fatal() {
        let dir = temp_dir("corrupt");
        let state = dir.join("state");
        create_private_dir(&state).unwrap();
        let registry = state.join(REGISTRY_FILE);
        std::fs::write(&registry, b"{not json at all").unwrap();

        let provider = LocalProvider::new(state.clone());
        assert!(
            provider
                .list_tagged(None)
                .await
                .unwrap()
                .instances
                .is_empty(),
            "a corrupt registry reads as no sessions, not as an error forever"
        );
        // The unparseable bytes are kept, in case a human wants them, and
        // the registry itself is usable again.
        let aside = registry.with_extension("corrupt");
        assert_eq!(std::fs::read(&aside).unwrap(), b"{not json at all");
        assert!(provider.list_tagged(None).await.is_ok());
        assert!(registry.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The app sweeping on launch while the CLI is mid-host is two
    /// processes in the same load-modify-save cycle, and the loser's entry
    /// used to vanish, leaving a server nothing would ever destroy.
    #[test]
    fn the_registry_lock_excludes_another_holder_and_breaks_a_stale_one() {
        let dir = temp_dir("lock");
        let path = dir.join("local.json.lock");

        let held = FileLock::acquire(&path);
        assert!(path.is_file(), "the lock is a file another process can see");

        // A second holder waits, gives up inside its window, and says so
        // rather than failing the command it was guarding.
        let started = Instant::now();
        let contended = FileLock::acquire(&path);
        assert!(started.elapsed() >= LOCK_WAIT);
        drop(contended);
        assert!(
            path.is_file(),
            "giving up must not release someone else's lock"
        );

        drop(held);
        assert!(!path.exists(), "the holder releases on drop");

        // A process that died holding it must not lock the machine out
        // forever, so an old enough lock is taken from it.
        std::fs::write(&path, b"").unwrap();
        let stale = std::time::SystemTime::now() - LOCK_STALE - Duration::from_secs(1);
        // Opened for writing because Windows needs write access to set a
        // file's times at all.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stale))
            .unwrap();
        assert!(lock_is_stale(&path));
        let stolen = FileLock::acquire(&path);
        assert!(path.is_file());
        drop(stolen);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One real row, from a US-English Windows 11 `tasklist /FI "PID eq
    /// 4242" /NH /FO CSV`, kept verbatim as the shape the parser must
    /// accept.
    const ROW: &str = "\"jamstreamd.exe\",\"4242\",\"Console\",\"1\",\"12,345 K\"\r\n";

    #[test]
    fn tasklist_exact_pid_match_is_alive() {
        // The image match is Windows's identity check, so it corroborates.
        assert_eq!(
            tasklist_probe(ROW, 4242, Some("jamstreamd.exe")),
            PidProbe::Alive { corroborated: true }
        );
        // No expectation recorded (a registry from an older build) still
        // answers on the pid alone, but nothing vouches for it.
        assert_eq!(
            tasklist_probe(ROW, 4242, None),
            PidProbe::Alive {
                corroborated: false
            }
        );
        // CreateProcess appends the extension the registry may not have,
        // and Windows names are case-insensitive either way.
        for expected in ["jamstreamd", "JAMSTREAMD.EXE", "JamStreamd.Exe"] {
            assert_eq!(
                tasklist_probe(ROW, 4242, Some(expected)),
                PidProbe::Alive { corroborated: true },
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
            PidProbe::Alive { corroborated: true }
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

    fn spawned(image: Option<&str>, started_unix: u64, proc_start: Option<u64>) -> Spawned<'_> {
        Spawned {
            image_name: image,
            started_unix,
            proc_start,
        }
    }

    fn observed(images: &[&str], start_token: Option<u64>, start_unix: Option<u64>) -> Observed {
        Observed {
            zombie: false,
            start_token,
            start_unix,
            images: images.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    const NOW: u64 = 1_800_000_000;

    /// The start token is the identity: only the recorded process
    /// incarnation carries it, so a match corroborates the pid outright and
    /// anything else under the same number is a recycled pid, however right
    /// its name looks.
    #[test]
    fn a_start_token_settles_the_pid_either_way() {
        let obs = observed(&["/usr/local/bin/jamstreamd"], Some(77_000), Some(NOW - 60));
        assert_eq!(
            classify(&obs, spawned(Some("jamstreamd"), NOW - 60, Some(77_000))),
            PidProbe::Alive { corroborated: true }
        );
        match classify(&obs, spawned(Some("jamstreamd"), NOW - 60, Some(76_999))) {
            PidProbe::Mismatch { running } => {
                assert!(running.contains("start token"), "said: {running}");
            }
            other => panic!("a different token is a recycled pid, got {other:?}"),
        }
    }

    /// A terminated child whose parent has not reaped it must not look
    /// alive, whatever else matches.
    #[test]
    fn a_zombie_is_dead_even_with_a_matching_token() {
        let obs = Observed {
            zombie: true,
            ..observed(&["jamstreamd"], Some(77_000), Some(NOW))
        };
        assert_eq!(
            classify(&obs, spawned(Some("jamstreamd"), NOW, Some(77_000))),
            PidProbe::Dead
        );
    }

    /// A registry an older build wrote has no token, so the image name and
    /// the wall-clock start carry the pid-reuse guard alone; matching them
    /// keeps the entry alive but never corroborates it.
    #[test]
    fn an_entry_with_no_token_falls_back_to_the_contradiction_checks() {
        // Nothing recorded at all: the pid answers alone, as it always has,
        // but nothing vouches for it.
        assert_eq!(
            classify(
                &observed(&["jamstreamd"], Some(1), Some(NOW)),
                spawned(None, 0, None)
            ),
            PidProbe::Alive {
                corroborated: false
            }
        );
        // The defect the probe exists for: the pid now belongs to somebody
        // else entirely.
        match classify(
            &observed(&["/usr/bin/ssh-agent"], Some(1), Some(NOW)),
            spawned(Some("jamstreamd"), 0, None),
        ) {
            PidProbe::Mismatch { running } => assert!(running.contains("ssh-agent")),
            other => panic!("another image under our pid must mismatch, got {other:?}"),
        }
        // The platform reports several names for one image (the kernel's
        // comm and the resolved executable path disagree after an exec
        // through a symlink); matching any one of them clears the check.
        for (images, recorded) in [
            (
                &["/usr/bin/sleep", "fake-jamstreamd"][..],
                "fake-jamstreamd",
            ),
            // Linux truncates its own comm field at 15 characters.
            (&["jamstreamd-head"][..], "jamstreamd-headless"),
            // Recorded absolute, observed from another prefix.
            (
                &["/usr/local/bin/jamstreamd"][..],
                "/opt/jamstream/jamstreamd",
            ),
        ] {
            assert_eq!(
                classify(
                    &observed(images, None, None),
                    spawned(Some(recorded), 0, None)
                ),
                PidProbe::Alive {
                    corroborated: false
                },
                "{images:?} should match {recorded:?}"
            );
        }
    }

    /// The half the image name cannot see: the pid came back around to
    /// another jamstreamd, so only the start time gives it away.
    #[test]
    fn a_tokenless_pid_younger_than_its_entry_is_a_recycled_pid() {
        // Our entry is a day old; what holds the pid started two hours ago.
        let obs = observed(&["jamstreamd"], None, Some(NOW - 7_200));
        match classify(&obs, spawned(Some("jamstreamd"), NOW - 86_400, None)) {
            PidProbe::Mismatch { running } => {
                assert!(
                    running.contains("after the registry entry"),
                    "said: {running}"
                );
            }
            other => panic!("a day-old entry on a two-hour-old process must not match: {other:?}"),
        }
        // Inside the slack it is the same process seen through a stepped
        // clock, and destroying our own session must stay possible.
        assert_eq!(
            classify(
                &obs,
                spawned(Some("jamstreamd"), NOW - 7_200 - START_SLACK_SECS, None)
            ),
            PidProbe::Alive {
                corroborated: false
            }
        );
        // A clock that went backwards leaves the process looking older than
        // its entry, which no recycled pid can be, so it is not a mismatch.
        assert_eq!(
            classify(&obs, spawned(Some("jamstreamd"), NOW, None)),
            PidProbe::Alive {
                corroborated: false
            }
        );
    }

    #[test]
    fn tasklist_finds_our_row_among_several() {
        let out = format!("\"notepad.exe\",\"7\",\"Console\",\"1\",\"1 K\"\r\n{ROW}");
        assert_eq!(
            tasklist_probe(&out, 4242, Some("jamstreamd.exe")),
            PidProbe::Alive { corroborated: true }
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

    /// Unix liveness against the one process we know everything about:
    /// this test binary.
    #[cfg(unix)]
    #[test]
    fn unix_liveness_matches_our_own_process() {
        let exe = std::env::current_exe().unwrap();
        let image = exe.file_name().unwrap().to_string_lossy().into_owned();
        let me = std::process::id();
        assert!(
            matches!(
                process::probe(me, spawned(Some(&image), 0, None)),
                PidProbe::Alive { .. }
            ),
            "the probe did not see this test process ({me}, {image})"
        );
        assert!(
            matches!(
                process::probe(me, spawned(None, 0, None)),
                PidProbe::Alive { .. }
            ),
            "pid-only probe must see us too"
        );
        assert!(
            matches!(
                process::probe(me, spawned(Some("definitely-not-jamstreamd"), 0, None)),
                PidProbe::Mismatch { .. }
            ),
            "an image mismatch must read as not ours so we never kill a stranger"
        );
        // The platforms with an identity read must produce a token, settle
        // on it exactly, and see through a stale wall-clock start.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let token = process::start_token(me).expect("this platform reads start tokens");
            assert_eq!(
                process::probe(me, spawned(Some(&image), 0, Some(token))),
                PidProbe::Alive { corroborated: true }
            );
            assert!(matches!(
                process::probe(me, spawned(Some(&image), 0, Some(token + 1))),
                PidProbe::Mismatch { .. }
            ));
            assert!(
                matches!(
                    process::probe(me, spawned(None, 1, None)),
                    PidProbe::Mismatch { .. }
                ),
                "a process that started long after its entry is not that entry's"
            );
        }
    }

    /// A sleep binary to stand in for a child process.
    #[cfg(unix)]
    fn sleep_bin() -> PathBuf {
        ["/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .or_else(|| find_on_path("sleep"))
            .expect("no sleep binary on this machine")
    }

    /// The probe against a real process rather than a parsed fixture: a
    /// spawned child reads alive under its own token, and dead once waited.
    #[cfg(unix)]
    #[test]
    fn liveness_tracks_a_real_child_from_spawn_to_reaped() {
        let mut child = Command::new(sleep_bin())
            .arg("600")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let token = process::start_token(pid);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(token.is_some(), "this platform reads start tokens");
        let entry = spawned(Some("sleep"), now_unix(), token);
        match process::probe(pid, entry) {
            PidProbe::Alive { corroborated } => assert_eq!(
                corroborated,
                token.is_some(),
                "the recorded token is exactly what corroborates"
            ),
            other => panic!("a running child must read alive, got {other:?}"),
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let after = process::probe(pid, entry);
        assert!(
            !matches!(after, PidProbe::Alive { .. }),
            "a waited child must read dead, got {after:?}"
        );
    }

    /// An exited child nobody has waited on yet: the platform must call the
    /// zombie dead, because destroy's wait loop is exactly this observer
    /// whenever the launching provider still holds the child handle.
    #[cfg(unix)]
    #[test]
    fn an_unreaped_child_reads_as_dead_not_alive() {
        let mut child = Command::new(sleep_bin())
            .arg("0")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let entry = spawned(None, 0, None);
        // sleep 0 exits on its own; poll until the zombie shows through.
        let deadline = Instant::now() + Duration::from_secs(10);
        while process::probe(pid, entry) != PidProbe::Dead {
            assert!(
                Instant::now() < deadline,
                "an unreaped child never read as dead"
            );
            std::thread::sleep(POLL);
        }
        child.wait().unwrap();
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
            sweeper.list_tagged(None).await.unwrap().instances.len(),
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
            sweeper
                .list_tagged(None)
                .await
                .unwrap()
                .instances
                .is_empty(),
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

    /// Asks the OS directly rather than through the probe under test: a raw
    /// tasklist run, judged only on whether a quoted row carries the exact
    /// pid field.
    #[cfg(windows)]
    fn pid_is_running(pid: u32) -> bool {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_owned());
        let out = Command::new(format!("{root}\\System32\\tasklist.exe"))
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .expect("tasklist");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line.starts_with('"') && line.contains(&format!("\",\"{pid}\",\"")))
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
            // An entry an older build wrote: no token, and the image the
            // pid now runs is not the one recorded.
            entry.as_object_mut().unwrap().remove("proc_start");
            entry["image_name"] = serde_json::json!("someone-elses-daemon");
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_pid_recycled_since_the_entry_was_written_survives_the_sweeper() {
        stale_entry_leaves_the_process_alone("stale-clock", |entry| {
            // An older build's entry again, and this time only the start
            // time separates our server from a pid that came back around.
            entry.as_object_mut().unwrap().remove("proc_start");
            let day_old = now_unix() - 86_400;
            entry["started_unix"] = serde_json::json!(day_old);
        })
        .await;
    }

    /// The strongest half of the guard: right image, believable clock, but
    /// the platform's start token says the pid was reborn. This is the
    /// check that catches a pid recycled by another jamstreamd.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn a_pid_reborn_under_a_different_start_token_survives_the_sweeper() {
        stale_entry_leaves_the_process_alone("stale-token", |entry| {
            let recorded = entry["proc_start"]
                .as_u64()
                .expect("launch records a start token on this platform");
            entry["proc_start"] = serde_json::json!(recorded + 1);
        })
        .await;
    }

    /// A stand-in for a server that will not go politely: it ignores
    /// SIGTERM and loops. What destroy may do next depends on whether the
    /// registry can still vouch for the pid. The `.ready` marker appears
    /// only after the trap is armed, because a TERM delivered before that
    /// line runs would end the process the default way and prove nothing.
    #[cfg(unix)]
    fn stubborn_server(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("fake-jamstreamd");
        std::fs::write(
            &path,
            "#!/bin/sh\ntrap '' TERM\n: > \"$0.ready\"\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// On Windows every windowless console stand-in is already stubborn:
    /// the polite step posts WM_CLOSE, which nothing here can see. The
    /// `.ready` marker keeps the same spawn-completed shape as unix.
    #[cfg(windows)]
    fn stubborn_server(dir: &Path) -> PathBuf {
        let path = dir.join("fake-jamstreamd.cmd");
        std::fs::write(&path, cmd_body("break > \"%~f0.ready\"\r\n")).unwrap();
        path
    }

    /// The migration policy under real signals: an entry with nothing to
    /// corroborate gets the sentinel and the polite step but never the
    /// forced kill, and destroy says what it skipped. The stand-in shrugs
    /// the polite step off, so only the skip keeps it alive.
    #[tokio::test]
    async fn destroy_never_force_kills_a_pid_it_cannot_corroborate() {
        let dir = temp_dir("unverified");
        let state = dir.join("state");
        let server = stubborn_server(&dir);
        let ready = PathBuf::from(format!("{}.ready", server.display()));
        let launcher = LocalProvider::new(state.clone()).with_server_binary(server);
        let instance = launcher
            .launch(LaunchSpec {
                region: LocalProvider::local_region(),
                instance_class: InstanceClass::Small,
                user_data: "#cloud-config\n".to_owned(),
                tags: vec![session_tag("unverified")],
            })
            .await
            .unwrap();
        let pid: u32 = instance.id.parse().unwrap();

        // Only once the marker exists (on unix: once the trap is armed) is
        // the polite step below guaranteed to be ignored rather than fatal.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "the stand-in never armed its trap"
            );
            std::thread::sleep(POLL);
        }

        // Rewrite the entry as an older build would have left it: nothing
        // recorded beyond the pid and a believable start.
        let registry = state.join(REGISTRY_FILE);
        let mut entries: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
        let entry = entries[0].as_object_mut().unwrap();
        entry.remove("proc_start");
        entry.remove("image_name");
        std::fs::write(&registry, serde_json::to_vec(&entries).unwrap()).unwrap();

        // The sweeper is a fresh process in spirit: no child handle, only
        // the registry's word for whose pid this is.
        let sweeper = LocalProvider::new(state.clone());
        let err = sweeper
            .destroy(&RegionId::new(REGION_ID), &instance.id)
            .await
            .unwrap_err();
        let skipped = format!("skipped the forced {}", process::FORCED_KILL);
        assert!(
            err.to_string().contains(&skipped),
            "destroy must say what it skipped, said: {err}"
        );
        assert!(
            pid_is_running(pid),
            "an uncorroborated pid was force-killed anyway"
        );

        process::kill(pid);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows liveness against the one process we know everything about:
    /// this test binary. Runs on the CI Windows leg.
    #[cfg(windows)]
    #[test]
    fn windows_liveness_matches_our_own_process() {
        let exe = std::env::current_exe().unwrap();
        let image = exe.file_name().unwrap().to_string_lossy().into_owned();
        let me = std::process::id();
        assert_eq!(
            process::probe(me, spawned(Some(&image), 0, None)),
            PidProbe::Alive { corroborated: true },
            "tasklist did not see this test process ({me}, {image})"
        );
        assert_eq!(
            process::probe(me, spawned(None, 0, None)),
            PidProbe::Alive {
                corroborated: false
            },
            "pid-only probe must see us too, with nothing vouching for it"
        );
        assert!(
            matches!(
                process::probe(me, spawned(Some("definitely-not-jamstreamd.exe"), 0, None)),
                PidProbe::Mismatch { .. }
            ),
            "an image mismatch must read as not ours so we never kill a stranger"
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
        let after = process::probe(pid, spawned(Some("cmd.exe"), 0, None));
        assert!(
            !matches!(after, PidProbe::Alive { .. }),
            "a waited child must read dead, got {after:?}"
        );
    }

    /// std offers no way to read creation flags back off a Command, so
    /// this asserts what it can: a quieted child still spawns, runs, and
    /// pipes its output, meaning CREATE_NO_WINDOW is a value CreateProcess
    /// accepts alongside piped stdio. That no window appears is only
    /// checkable on hardware and sits on the release checklist.
    #[cfg(windows)]
    #[test]
    fn a_quieted_child_still_runs_and_pipes_output() {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "echo quiet"]);
        let out = quiet(&mut command).output().unwrap();
        assert!(out.status.success(), "quieted cmd /C echo failed: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "quiet");
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
