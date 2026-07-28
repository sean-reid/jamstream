//! Socket, tick, and lifecycle driver for `ServerCore`. All session logic
//! lives in jamstream-session; this file owns exactly the things the core
//! must not: time, UDP, the activity file the dead man's switch reads, and
//! the broadcast pipeline's processes.
//!
//! Why the pipeline is driven here rather than from `ServerCore`: the core is
//! sans-io and deterministic, which is what lets the harness replay a session
//! packet for packet. Spawning ffmpeg is neither. So the core routes an
//! accepted `StreamCtl` out as an event, this file hands it to the stream
//! worker, and the worker's per-destination status comes back in through
//! `set_stream_status` for fanout to every member.

use std::io;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ed25519_dalek::VerifyingKey;
use jamstream_protocol::control::MAX_DATAGRAM_BYTES;
use jamstream_protocol::control::StreamOp;
use jamstream_protocol::transport::derive_public;
use jamstream_session::server::{ServerConfig, ServerCore, ServerEvent};
use jamstream_stream::pipeline::{Roster, StreamConfig, StreamMember};
use jamstream_stream::worker::{StreamWorker, TickPayload};
use tokio::net::UdpSocket;
use tokio::time::MissedTickBehavior;

use crate::config::Config;
use crate::revocations::Revocations;

const TICK: Duration = Duration::from_micros(2_500);
const ACTIVITY_PERIOD: Duration = Duration::from_secs(1);
/// Card title when provisioning supplied no session name.
const DEFAULT_SESSION_NAME: &str = "JamStream session";

#[derive(Debug, Clone)]
pub struct Options {
    pub bind: SocketAddr,
    /// Touched once a second while musicians are connected; the systemd
    /// guard treats staleness as idleness. None disables (tests, dev).
    pub activity_path: Option<PathBuf>,
}

/// Elapsed session time for the two self-exit windows, which is a different
/// question from elapsed time for the mix clock.
///
/// `Instant` is monotonic, and on macOS it reads `CLOCK_UPTIME_RAW`, which
/// stops while the machine is asleep; Linux's `CLOCK_MONOTONIC` does the same
/// across suspend. A laptop is exactly where the local dead man's switch has
/// to work, and a lid closed between two jams holds the countdown open for as
/// long as the nap: a ten minute idle window on a machine that sleeps
/// overnight is ten minutes of *awake* time. That is how a local server nobody
/// wanted survives an afternoon.
///
/// So the switch reads both clocks and believes whichever has seen more time.
/// A wall clock stepped backwards leaves the monotonic reading in charge.
/// Stepped forwards it can only end a session sooner, and sooner is the
/// direction a dead man's switch is allowed to be wrong in.
///
/// The mix clock keeps using `Instant` alone: media timestamps must not jump
/// because ntpd corrected the hour.
fn session_elapsed(monotonic: Duration, wall: Option<Duration>) -> Duration {
    match wall {
        Some(wall) => monotonic.max(wall),
        None => monotonic,
    }
}

/// The idle-exit countdown, pure so it is testable without time: feed it
/// (elapsed-since-start, musician count) once per heartbeat, it answers
/// whether the server should exit.
#[derive(Debug)]
pub struct IdleExit {
    window: Duration,
    idle_since: Option<Duration>,
}

impl IdleExit {
    /// The countdown starts armed at construction: a server nobody ever
    /// joins still dies after one window.
    pub fn new(window: Duration) -> Self {
        IdleExit {
            window,
            idle_since: Some(Duration::ZERO),
        }
    }

    /// True means exit now. A zero window never fires.
    pub fn observe(&mut self, now: Duration, musicians: usize) -> bool {
        if self.window.is_zero() {
            return false;
        }
        if musicians > 0 {
            self.idle_since = None;
            return false;
        }
        let since = *self.idle_since.get_or_insert(now);
        now.saturating_sub(since) >= self.window
    }
}

/// The whole-session cap, pure like [`IdleExit`]: feed it elapsed-since-start
/// once per heartbeat, it answers whether the server should exit. Occupancy
/// is deliberately not an input: the cap ends the session even mid-jam.
#[derive(Debug)]
pub struct MaxDuration {
    window: Duration,
}

impl MaxDuration {
    pub fn new(window: Duration) -> Self {
        MaxDuration { window }
    }

    /// True means exit now. A zero window never fires.
    pub fn observe(&self, now: Duration) -> bool {
        !self.window.is_zero() && now >= self.window
    }
}

pub struct Server {
    core: ServerCore,
    socket: UdpSocket,
    activity_path: Option<PathBuf>,
    idle_exit: Duration,
    max_duration: Duration,
    /// Broadcast pipeline, started on the host's first stream request. Most
    /// sessions never stream, and the renderer's buffers plus a thread are not
    /// worth paying for until one does.
    stream: Option<StreamWorker>,
    stream_cfg: StreamConfig,
    /// Roster generation last handed to the pipeline.
    stream_roster_epoch: u64,
    /// Durable revocation list. None keeps revocations in memory only, which
    /// is fine for a test and wrong for a deployment.
    revocations: Option<Revocations>,
    /// Sentinel path whose appearance requests a clean exit.
    shutdown_path: Option<PathBuf>,
    /// Panics caught in the datagram path since startup.
    panics: u64,
}

impl Server {
    pub async fn bind(cfg: &Config, opts: Options) -> io::Result<Server> {
        let private: [u8; 32] = cfg.server_private_key.as_slice().try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "server key must be 32 bytes")
        })?;
        let server_public = derive_public(&private)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "server key is not valid"))?;
        let issuer_pk = VerifyingKey::from_bytes(&cfg.issuer_public_key)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "issuer key is not valid"))?;

        // Capacity and member timeout come from jamstream_session's shared
        // limits, the same numbers the CLI flags and the desktop wizard
        // offer seats against.
        let core = ServerCore::new(ServerConfig::new(
            cfg.session_id,
            private.to_vec(),
            server_public,
            issuer_pk,
        ));
        let socket = UdpSocket::bind(opts.bind).await?;
        // The card title. The wire protocol carries no session name, so
        // jamstreamd takes it as a flag (see with_stream_config); this is the
        // fallback when nobody supplies one.
        let stream_cfg = StreamConfig::new(DEFAULT_SESSION_NAME);
        Ok(Server {
            core,
            socket,
            activity_path: opts.activity_path,
            idle_exit: Duration::ZERO,
            max_duration: Duration::ZERO,
            stream: None,
            stream_cfg,
            stream_roster_epoch: 0,
            revocations: None,
            shutdown_path: None,
            panics: 0,
        })
    }

    /// Points the server at a durable revocation list: the file is read now,
    /// so revocations from before this process survive, and appended to on
    /// every new revocation. Without it revocation lasts exactly as long as
    /// the process, which `Restart=on-failure` makes a very short time.
    /// Builder-style so `Options` stays stable for existing constructors.
    pub fn with_revocations(mut self, store: Revocations) -> Self {
        let known = store.load();
        if !known.is_empty() {
            tracing::info!(
                revoked = known.len(),
                path = %store.path().display(),
                "restored revoked invites"
            );
            self.core.restore_revoked(known);
        }
        self.revocations = Some(store);
        self
    }

    /// Watches a sentinel path: once it exists, the server shuts down
    /// cleanly. This is how the local provider asks on Windows, which has no
    /// cross-process SIGTERM for a console process; on unix the signal gets
    /// there first and this is a second door to the same exit. Creates the
    /// marker beside it that tells the provider the door is there at all.
    pub fn with_shutdown_file(mut self, path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let marker = shutdown_supported_path(&path);
        if let Err(err) = std::fs::write(&marker, b"") {
            tracing::warn!(error = %err, path = %marker.display(), "cannot write the shutdown marker");
        }
        self.shutdown_path = Some(path);
        self
    }

    /// Arms the idle self-exit: the server exits cleanly once no musicians
    /// have been connected for `window`, measured from startup or from the
    /// last musician leaving. Zero (the default) disables. This is the
    /// local-mode dead man's switch; cloud deployments keep the external
    /// guard and leave it off. Builder-style so `Options` stays stable for
    /// existing constructors.
    pub fn with_idle_exit(mut self, window: Duration) -> Self {
        self.idle_exit = window;
        self
    }

    /// Arms the whole-session cap: the server exits cleanly once `window`
    /// has elapsed since startup, whether or not musicians are connected.
    /// Zero (the default) disables. Local mode passes this so the cap the
    /// host chose ends the session for real; cloud deployments keep the
    /// external guard and leave it off.
    pub fn with_max_duration(mut self, window: Duration) -> Self {
        self.max_duration = window;
        self
    }

    /// Overrides the broadcast pipeline's configuration (ffmpeg path, relay
    /// URL, working directories). Tests and local runs use it; the default is
    /// the layout cloud-init creates on the session VM.
    pub fn with_stream_config(mut self, cfg: StreamConfig) -> Self {
        self.stream_cfg = cfg;
        self
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Hands one accepted host request to the pipeline, starting the worker on
    /// the first one.
    fn route_stream_ctl(&mut self, now_ms: u64, op: StreamOp) {
        if self.stream.is_none() {
            match StreamWorker::spawn(self.stream_cfg.clone()) {
                Ok(worker) => self.stream = Some(worker),
                Err(err) => {
                    tracing::error!(error = %err, "cannot start the broadcast pipeline");
                    return;
                }
            }
        }
        if let Some(worker) = self.stream.as_ref() {
            worker.apply(now_ms, op);
        }
    }

    /// Feeds the pipeline this tick's broadcast audio and card state. Copies a
    /// fixed-size payload and hands it off; it never waits on ffmpeg.
    fn feed_stream(&mut self, now_ms: u64) {
        let Some(worker) = self.stream.as_ref() else {
            return;
        };
        let wants = worker.wants_audio();
        if wants != self.core.broadcast_tap() {
            self.core.set_broadcast_tap(wants);
        }
        if !wants {
            return;
        }
        let tick = self.core.broadcast_tick();
        let mut payload = TickPayload::default();
        let n = tick.audio.len().min(payload.audio.len());
        payload.audio[..n].copy_from_slice(&tick.audio[..n]);
        for m in &tick.members {
            payload.levels.push(m.level_peak, m.level_rms);
        }
        let roster = (tick.roster_epoch != self.stream_roster_epoch).then(|| Roster {
            members: tick
                .members
                .iter()
                .map(|m| StreamMember {
                    id: m.id,
                    name: m.name.to_owned(),
                    connected: m.connected,
                    avatar: m.avatar.map(|(hash, bytes)| (*hash, bytes.to_vec())),
                })
                .collect(),
            listeners: tick.listeners,
        });
        let epoch = tick.roster_epoch;
        if let Some(roster) = roster {
            worker.submit_roster(roster);
            self.stream_roster_epoch = epoch;
        }
        worker.submit_tick(now_ms, payload);
    }

    /// Once a second: keep the supervisor's clock moving and publish its
    /// per-destination status to every member.
    fn beat_stream(&mut self, now_ms: u64) {
        let Some(worker) = self.stream.as_ref() else {
            return;
        };
        worker.beat(now_ms);
        let gaps = worker.gap_ticks();
        if gaps > 0 {
            tracing::warn!(gaps, "broadcast pipeline fell behind the mix tick");
        }
        let status = worker.status();
        self.core.set_stream_status(now_ms, status);
    }

    /// Logs the core's events and routes the two kinds that need an actuator.
    fn drain_events(&mut self, now_ms: u64) {
        for event in self.core.events() {
            log_event(&event);
            match event {
                ServerEvent::StreamCtl(op) => self.route_stream_ctl(now_ms, op),
                // Written through before this call returns, so the revocation
                // survives whatever exit comes next. Nothing is buffered:
                // a revocation the host has been told about must already be
                // on disk, not waiting for a flush that a crash skips.
                ServerEvent::TokenRevoked { jti } => {
                    if let Some(store) = self.revocations.as_ref()
                        && let Err(err) = store.append(jti)
                    {
                        tracing::error!(
                            error = %err,
                            path = %store.path().display(),
                            "cannot persist a revocation: it will not survive a restart"
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Tells every connected member the session is ending, one flight each,
    /// then returns. No retransmit: the process is going away. Before this
    /// existed, every client discovered a stop by ten-second timeout.
    async fn say_goodbye(&mut self, now_ms: u64, reason: &str) {
        let farewells = self.core.shutdown(now_ms, reason);
        let members = farewells.len();
        for (addr, pkt) in farewells {
            let _ = self.socket.send_to(&pkt, addr).await;
        }
        self.drain_events(now_ms);
        if members > 0 {
            tracing::info!(members, reason, "told members the session is ending");
        }
    }

    /// Runs until `shutdown` resolves. Datagram handling, the 2.5 ms mix
    /// tick, and the activity heartbeat share one task; the core is not
    /// thread-safe and does not need to be.
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let start = Instant::now();
        // Second start stamp, on the clock that keeps counting while the
        // machine is asleep. See session_elapsed.
        let start_wall = SystemTime::now();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Burst);
        let mut heartbeat = tokio::time::interval(ACTIVITY_PERIOD);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut idle_exit = IdleExit::new(self.idle_exit);
        let max_duration = MaxDuration::new(self.max_duration);
        // Sized for the largest datagram a client can send: an avatar
        // chunk. A short buffer would truncate the upload instead of
        // failing, and the avatar would never reach the cache.
        let mut buf = [0u8; MAX_DATAGRAM_BYTES];
        tokio::pin!(shutdown);
        let reason = loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break "session ended",
                _ = tick.tick() => {
                    let now_ms = start.elapsed().as_millis() as u64;
                    for (addr, pkt) in self.core.tick(now_ms) {
                        let _ = self.socket.send_to(&pkt, addr).await;
                    }
                    // Straight after the tick: the broadcast slice the tap
                    // exposes is the slot that tick just wrote.
                    self.feed_stream(now_ms);
                    self.drain_events(now_ms);
                }
                _ = heartbeat.tick() => {
                    let now_ms = start.elapsed().as_millis() as u64;
                    self.beat_stream(now_ms);
                    let musicians = self.core.musicians_connected();
                    if musicians > 0 {
                        touch(self.activity_path.as_deref());
                    }
                    if self.shutdown_requested() {
                        tracing::info!("shutdown requested on the sentinel file, exiting");
                        break "session ended";
                    }
                    let elapsed = session_elapsed(start.elapsed(), start_wall.elapsed().ok());
                    if idle_exit.observe(elapsed, musicians) {
                        tracing::info!(
                            idle_secs = self.idle_exit.as_secs_f64(),
                            "no musicians for the idle window, exiting"
                        );
                        break "session idle";
                    }
                    if max_duration.observe(elapsed) {
                        tracing::info!(
                            max_duration_secs = self.max_duration.as_secs_f64(),
                            musicians,
                            "session reached its maximum duration, exiting"
                        );
                        break "session time limit reached";
                    }
                }
                received = self.socket.recv_from(&mut buf) => {
                    let (len, src) = match received {
                        Ok(ok) => ok,
                        // Transient per-datagram errors (ICMP unreachable
                        // surfacing on some platforms) must not kill the
                        // session.
                        Err(err) => {
                            tracing::debug!(error = %err, "recv_from failed");
                            continue;
                        }
                    };
                    let now_ms = start.elapsed().as_millis() as u64;
                    let now_unix = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    // One peer's datagram must not be able to take the session
                    // with it. The core is one struct in one task, so an
                    // unwind from here used to leave run(), block_on, and
                    // main, dropping every member.
                    let core = &mut self.core;
                    let replies = guard(|| core.handle_datagram(now_ms, now_unix, src, &buf[..len]));
                    let Some(replies) = replies else {
                        self.after_panic(now_ms, src);
                        continue;
                    };
                    for (addr, pkt) in replies {
                        let _ = self.socket.send_to(&pkt, addr).await;
                    }
                    // A StreamCtl arrives on this path, not the tick, and the
                    // host should not wait 2.5 ms for it.
                    self.drain_events(now_ms);
                }
            }
        };
        let now_ms = start.elapsed().as_millis() as u64;
        self.say_goodbye(now_ms, reason).await;
        tracing::info!(reason, "shutting down");
        Ok(())
    }

    /// A panic in the core is a bug, not an input to keep feeding: the member
    /// whose datagram provoked it is dropped, because their half-updated state
    /// is exactly what we stopped trusting, and everyone else plays on. Their
    /// token stays valid, so a client on the receiving end of someone else's
    /// bug reconnects with a fresh handshake.
    fn after_panic(&mut self, now_ms: u64, src: SocketAddr) {
        self.panics += 1;
        // A peer that can provoke this can provoke it at line rate, and the
        // journal shares a small VM's disk with the recording.
        if self.panics <= 10 || self.panics % 100 == 0 {
            tracing::error!(
                panics = self.panics,
                peer = %src,
                "panic while handling a datagram; dropping the peer and continuing"
            );
        }
        if let Some(id) = self.core.drop_peer(src) {
            tracing::warn!(member = id.0, "dropped after a panic on its datagram");
        }
        self.drain_events(now_ms);
    }

    /// Panics caught since startup, for a test or an operator asking whether
    /// the session has been limping.
    pub fn panics(&self) -> u64 {
        self.panics
    }

    /// True once the sentinel path exists. Checked on the one-second
    /// heartbeat, not per tick: 400 stats a second for a file that appears
    /// once is not a trade worth making on the audio path.
    fn shutdown_requested(&self) -> bool {
        self.shutdown_path
            .as_deref()
            .is_some_and(std::path::Path::exists)
    }
}

/// Runs one call to the core, answering None if it panicked. `AssertUnwindSafe`
/// is a claim, not a proof: the caller has to drop whatever state the unwind
/// may have left half written, which for a datagram means the peer that sent
/// it. The panic hook has already logged the payload and location by the time
/// this returns.
fn guard<T>(f: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(AssertUnwindSafe(f)).ok()
}

/// The marker jamstreamd leaves beside its shutdown sentinel, telling whoever
/// spawned it that the sentinel has a reader. The local provider skips the
/// graceful wait when it is absent, which is what an older build gets.
fn shutdown_supported_path(shutdown: &std::path::Path) -> PathBuf {
    let mut name = shutdown.file_name().unwrap_or_default().to_owned();
    name.push(".supported");
    shutdown.with_file_name(name)
}

fn log_event(event: &ServerEvent) {
    match event {
        ServerEvent::MusicianCountChanged(n) => tracing::info!(musicians = n, "occupancy"),
        ServerEvent::MemberJoined { id, name } => {
            tracing::info!(member = id.0, name = %name, "joined");
        }
        ServerEvent::MemberDisconnected { id } => tracing::info!(member = id.0, "disconnected"),
        ServerEvent::MemberRevoked { id } => tracing::info!(member = id.0, "revoked"),
        ServerEvent::ProtocolViolation { id, what } => {
            tracing::warn!(member = id.0, what, "protocol violation");
        }
        ServerEvent::MemberEjected { id, violations } => {
            tracing::warn!(member = id.0, violations, "ejected for protocol violations");
        }
        ServerEvent::TokenRevoked { jti } => tracing::info!(jti = ?jti, "token revoked"),
        // The op's Debug redacts the stream key by construction.
        ServerEvent::StreamCtl(op) => tracing::info!(op = ?op, "stream control"),
    }
}

fn touch(path: Option<&std::path::Path>) {
    let Some(path) = path else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(SystemTime::now()))
    {
        tracing::warn!(error = %err, "cannot touch activity file");
    }
}

#[cfg(test)]
mod tests {
    use super::{IdleExit, MaxDuration, guard, session_elapsed, shutdown_supported_path};
    use std::path::Path;
    use std::time::Duration;

    /// The whole point of #47: an unwind out of the core stops here instead of
    /// leaving run(), block_on, and main, which took every member with it.
    #[test]
    fn guard_turns_an_unwind_into_a_value_the_loop_can_handle() {
        assert_eq!(guard(|| 7), Some(7));
        // The default hook would print this; the test only cares that the
        // unwind is contained.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let caught: Option<()> = guard(|| panic!("indexing bug in a 2,400-line network path"));
        let caught_string: Option<()> = guard(|| panic!("{}", String::from("owned payload")));
        std::panic::set_hook(previous);
        assert!(caught.is_none());
        assert!(caught_string.is_none());
        // And the loop keeps working afterwards.
        assert_eq!(guard(|| 8), Some(8));
    }

    /// The local provider looks for this exact name beside the sentinel; a
    /// mismatch means it silently skips the graceful wait forever.
    #[test]
    fn the_shutdown_marker_sits_beside_the_sentinel() {
        assert_eq!(
            shutdown_supported_path(Path::new("/tmp/session-abc/shutdown")),
            Path::new("/tmp/session-abc/shutdown.supported")
        );
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    /// The laptop case: three hours of wall time passed, of which the
    /// monotonic clock saw four minutes because the lid was shut. A ten
    /// minute idle window has to fire on the wake, not four minutes after
    /// the next one.
    #[test]
    fn a_sleeping_machine_does_not_hold_the_countdown_open() {
        let elapsed = session_elapsed(secs(240), Some(secs(3 * 3600)));
        assert_eq!(elapsed, secs(3 * 3600));
        let mut ie = IdleExit::new(secs(600));
        assert!(ie.observe(elapsed, 0));
        assert!(
            MaxDuration::new(secs(43_200))
                .observe(session_elapsed(secs(240), Some(secs(13 * 3600))))
        );
    }

    /// A wall clock that stepped backwards, or one the platform refused to
    /// difference, leaves the monotonic reading in charge rather than
    /// resetting the countdown.
    #[test]
    fn a_useless_wall_clock_falls_back_to_the_monotonic_one() {
        assert_eq!(session_elapsed(secs(700), None), secs(700));
        // Wall clock behind the monotonic one: the larger still wins, so a
        // backwards step cannot buy a session extra life.
        assert_eq!(session_elapsed(secs(700), Some(secs(5))), secs(700));
        assert!(IdleExit::new(secs(600)).observe(session_elapsed(secs(700), Some(secs(5))), 0));
    }

    /// Awake the whole time, the two clocks agree and nothing changes.
    #[test]
    fn an_awake_machine_reads_the_same_either_way() {
        for t in [0u64, 1, 59, 600, 43_200] {
            assert_eq!(session_elapsed(secs(t), Some(secs(t))), secs(t));
        }
    }

    #[test]
    fn zero_window_never_fires() {
        let mut ie = IdleExit::new(Duration::ZERO);
        for t in 0..10_000 {
            assert!(!ie.observe(secs(t), 0));
        }
    }

    #[test]
    fn fires_after_window_from_startup_when_nobody_joins() {
        let mut ie = IdleExit::new(secs(60));
        assert!(!ie.observe(secs(1), 0));
        assert!(!ie.observe(secs(59), 0));
        assert!(ie.observe(secs(60), 0));
    }

    #[test]
    fn musicians_hold_the_countdown_and_leaving_rearms_it() {
        let mut ie = IdleExit::new(secs(60));
        assert!(!ie.observe(secs(10), 1));
        // Idle long past the window while occupied: never fires.
        assert!(!ie.observe(secs(500), 2));
        // Last musician leaves at t=500; the window restarts there.
        assert!(!ie.observe(secs(501), 0));
        assert!(!ie.observe(secs(559), 0));
        assert!(ie.observe(secs(561), 0));
    }

    #[test]
    fn zero_cap_never_fires() {
        let md = MaxDuration::new(Duration::ZERO);
        for t in 0..10_000 {
            assert!(!md.observe(secs(t)));
        }
    }

    #[test]
    fn cap_fires_at_the_window_and_stays_fired() {
        let md = MaxDuration::new(secs(60));
        assert!(!md.observe(secs(0)));
        assert!(!md.observe(secs(59)));
        assert!(md.observe(secs(60)));
        assert!(md.observe(secs(61)));
        assert!(md.observe(secs(10_000)));
    }

    #[test]
    fn cap_ignores_going_backwards_in_its_own_history() {
        // Pure function of elapsed time: earlier observations after later
        // ones (heartbeat reordering cannot happen, but the type must not
        // care) still answer purely from the input.
        let md = MaxDuration::new(secs(60));
        assert!(md.observe(secs(60)));
        assert!(!md.observe(secs(59)));
    }

    #[test]
    fn rejoin_within_the_window_resets_cleanly() {
        let mut ie = IdleExit::new(secs(60));
        assert!(!ie.observe(secs(59), 0));
        assert!(!ie.observe(secs(60), 1));
        assert!(!ie.observe(secs(61), 0));
        assert!(!ie.observe(secs(120), 0));
        assert!(ie.observe(secs(121), 0));
    }
}
