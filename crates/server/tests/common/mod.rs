//! Shared rig for the server's integration tests: one loopback address, one
//! session fixture, one bind-and-spawn sequence, one scratch-directory policy,
//! and wall budgets that scale with the machine instead of being hardcoded.

#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, Server};
use tokio::task::JoinHandle;

/// Loopback, kernel-assigned port. Never the wildcard address: binding
/// 0.0.0.0 raises the macOS firewall prompt on every run.
pub fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

/// What the wall budgets below are worth on a developer laptop, matching the
/// harness so one variable describes the runner for the whole workspace.
const REFERENCE_LAPTOP_SECS: f64 = 30.0;

/// A wall-clock budget for a test deadline, scaled for the machine.
///
/// `JAMSTREAM_PERF_BUDGET_SECS` says what the harness's 30 s reference run is
/// allowed here, and every deadline takes the same multiplier from it. Three
/// server deadlines were fixed 5 s bounds and were reproduced failing at about
/// 5.0 s under a concurrent workspace run.
pub fn budget(laptop: Duration) -> Duration {
    let scale = std::env::var("JAMSTREAM_PERF_BUDGET_SECS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map_or(1.0, |v| v / REFERENCE_LAPTOP_SECS);
    Duration::from_secs_f64(laptop.as_secs_f64() * scale.max(1.0))
}

/// An empty scratch directory, named for the test and this process, removed
/// first so a previous run cannot be mistaken for this one's work.
pub fn scratch_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jamstream-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Polls `done` until it holds or the scaled deadline passes.
pub fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + budget(Duration::from_secs(10));
    while !done() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A port the kernel handed out, kept reserved by holding the socket until the
/// server that is to bind it starts. Bind-then-drop hands the port straight
/// back, and any concurrently running test can then be given the same one.
pub struct ReservedPort {
    socket: Option<std::net::UdpSocket>,
    pub port: u16,
}

impl ReservedPort {
    pub fn reserve() -> ReservedPort {
        let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("a free port");
        let port = socket.local_addr().expect("bound").port();
        ReservedPort {
            socket: Some(socket),
            port,
        }
    }

    /// Hands the port back to the kernel, immediately before the spawn that
    /// takes it and never earlier.
    pub fn release(&mut self) {
        self.socket.take();
    }
}

/// Kills a spawned jamstreamd unless it has already exited. A panic between
/// the spawn and the wait must not leave a server holding a port, and the drop
/// on the way out is the only thing that can clean up after a failure.
pub struct ChildGuard(pub std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            eprintln!("test left jamstreamd {} running; killing it", self.0.id());
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// The address every spawned server is confined to. Without it jamstreamd
/// binds every interface, and the macOS Application Firewall filters incoming
/// connections per binary: every rebuild is a new binary, so an unconfined
/// server raises a dialog and drops the test's datagrams until somebody
/// answers it. Loopback is the one path it does not govern.
pub const BIND: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// The jamstreamd this crate just built.
pub fn server_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_jamstreamd"))
}

/// The keys and config a test server runs with, plus the invites its members
/// join on. Every field a test needs to assert against is public.
pub struct Session {
    pub issuer: Issuer,
    pub server_public: [u8; 32],
    pub session_id: SessionId,
    pub cfg: Config,
}

impl Session {
    /// A session with the usual windows: idle after 10 minutes, capped at 12
    /// hours, so neither fires during a test.
    pub fn new() -> Session {
        Session::with_windows(10, 720)
    }

    /// A session whose idle and hard-cap windows the test chooses; zero
    /// disables either one.
    pub fn with_windows(idle_shutdown_min: u32, max_duration_min: u32) -> Session {
        let issuer = Issuer::generate();
        let keys = generate_keypair();
        let session_id = SessionId::generate();
        Session {
            cfg: Config {
                session_id,
                port: 0,
                server_private_key: keys.private.to_vec(),
                issuer_public_key: issuer.public_key().to_bytes(),
                idle_shutdown_min,
                max_duration_min,
            },
            issuer,
            server_public: keys.public,
            session_id,
        }
    }

    /// A musician's invite to `addr`, with a fresh token id and no name.
    pub fn musician(&self, member: u16, addr: SocketAddr) -> Invite {
        self.invite(member, Role::Musician, TokenId::generate(), None, addr)
    }

    /// An invite the test controls every field of: the role a member joins
    /// with, the token id a revocation has to name, and the name hint the
    /// roster shows.
    pub fn invite(
        &self,
        member: u16,
        role: Role,
        jti: TokenId,
        name_hint: Option<String>,
        addr: SocketAddr,
    ) -> Invite {
        self.issuer.mint(
            self.session_id,
            vec![addr],
            self.server_public,
            Token {
                member_id: MemberId(member),
                role,
                name_hint,
                expires_unix: u64::MAX,
                jti,
            },
        )
    }
}

impl Default for Session {
    fn default() -> Self {
        Session::new()
    }
}

/// A jamstreamd runtime running on loopback, with the handle to stop it.
/// Dropping it stops the server too, so a failed assertion never leaves the
/// task holding a socket.
pub struct Running {
    pub addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<JoinHandle<std::io::Result<()>>>,
}

impl Running {
    /// Binds `session` on loopback with `opts` and runs it on the current
    /// runtime.
    pub async fn spawn(session: &Session, opts: Options) -> Running {
        let server = Server::bind(&session.cfg, opts)
            .await
            .expect("server binds on loopback");
        Running::of(server)
    }

    /// The same, for a server the test built further with `with_revocations`
    /// or `with_stream_config`.
    pub fn of(server: Server) -> Running {
        let addr = server.local_addr().expect("bound address");
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async {
            let _ = rx.await;
        }));
        Running {
            addr,
            stop: Some(stop),
            task: Some(task),
        }
    }

    /// Options with nothing switched on: loopback, no activity file, no
    /// recording.
    pub fn plain_options() -> Options {
        Options {
            bind: loopback(),
            activity_path: None,
            recording: None,
        }
    }

    /// Signals the shutdown and waits for the run loop to return, which is
    /// what proves the goodbye and the socket close happened.
    pub async fn stop(mut self) -> std::io::Result<()> {
        let _ = self.stop.take().expect("not stopped twice").send(());
        self.task
            .take()
            .expect("not stopped twice")
            .await
            .expect("the run loop panicked")
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
