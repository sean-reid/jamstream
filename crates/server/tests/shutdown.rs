//! Two things a session server owes the people in it, both over real UDP:
//! a revocation that outlives the process, and a word before it goes.
//!
//! Neither was true. Revocation lived only in `ServerCore`, and the unit is
//! `Restart=on-failure` with `RestartSec=2`, so a revoked member who waited
//! for any exit had their invite back two seconds later. And a stop broke the
//! run loop and dropped the socket without telling anyone, so every client
//! discovered the end by ten-second timeout. SIGTERM was not handled at all,
//! which is the signal systemd stop, local teardown, and the cloud
//! self-destruct all send.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_server::config::Config;
use jamstream_server::revocations::Revocations;
use jamstream_server::runtime::{Options, Server};
use jamstream_session::client::{ClientCore, ClientEvent, ClientState};
use tokio::net::UdpSocket;

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jamstream-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Fixture {
    issuer: Issuer,
    session_id: SessionId,
    server_public: [u8; 32],
    cfg: Config,
}

impl Fixture {
    fn new() -> Fixture {
        let issuer = Issuer::generate();
        let keys = generate_keypair();
        let session_id = SessionId::generate();
        Fixture {
            cfg: Config {
                session_id,
                port: 0,
                server_private_key: keys.private.to_vec(),
                issuer_public_key: issuer.public_key().to_bytes(),
                idle_shutdown_min: 10,
                max_duration_min: 720,
            },
            issuer,
            session_id,
            server_public: keys.public,
        }
    }

    fn mint(&self, member: u16, role: Role, jti: TokenId, addr: SocketAddr) -> Invite {
        self.issuer.mint(
            self.session_id,
            vec![addr],
            self.server_public,
            Token {
                member_id: MemberId(member),
                role,
                name_hint: None,
                expires_unix: u64::MAX,
                jti,
            },
        )
    }
}

/// One client on its own socket, pumped by hand so a test can watch for a
/// specific event without a background task.
struct Client {
    core: ClientCore,
    socket: UdpSocket,
    events: Vec<ClientEvent>,
}

impl Client {
    async fn connect(invite: &Invite, addr: SocketAddr, now_ms: u64) -> Client {
        let socket = UdpSocket::bind(loopback()).await.unwrap();
        socket.connect(addr).await.unwrap();
        let (core, first) = ClientCore::connect(invite, now_ms).unwrap();
        socket.send(&first).await.unwrap();
        Client {
            core,
            socket,
            events: Vec::new(),
        }
    }

    /// One pump pass, bounded: a joined musician gets a mix frame every
    /// 2.5 ms, so draining until quiet would never return.
    async fn pump(&mut self, now_ms: u64) {
        for pkt in self.core.poll(now_ms) {
            let _ = self.socket.send(&pkt).await;
        }
        let mut buf = [0u8; 2048];
        for _ in 0..64 {
            let Ok(Ok(len)) =
                tokio::time::timeout(Duration::from_millis(2), self.socket.recv(&mut buf)).await
            else {
                break;
            };
            for pkt in self.core.handle_datagram(now_ms, &buf[..len]) {
                let _ = self.socket.send(&pkt).await;
            }
        }
        self.events.extend(self.core.events());
    }

    async fn pump_until_joined(&mut self, start: Instant) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            self.pump(start.elapsed().as_millis() as u64).await;
            if *self.core.state() == ClientState::Joined {
                return true;
            }
        }
        false
    }

    async fn pump_for(&mut self, start: Instant, window: Duration) {
        let until = Instant::now() + window;
        while Instant::now() < until {
            self.pump(start.elapsed().as_millis() as u64).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// The one that mattered: a revoked invite must stay revoked across the
/// restart that `Restart=on-failure` makes routine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_invite_stays_revoked_across_a_restart() {
    let f = Fixture::new();
    let dir = temp_dir("revoke-restart");
    let revoked_file = dir.join("revoked");
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;

    // First process: host joins, a guest joins, the host revokes the guest.
    let server = Server::bind(
        &f.cfg,
        Options {
            bind: loopback(),
            activity_path: None,
            recording: None,
        },
    )
    .await
    .unwrap()
    .with_revocations(Revocations::new(revoked_file.clone()));
    let addr = server.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async {
        let _ = stop_rx.await;
    }));

    let guest_jti = TokenId::generate();
    let host_invite = f.mint(0, Role::Musician, TokenId::generate(), addr);
    let guest_invite = f.mint(1, Role::Musician, guest_jti, addr);

    let mut host = Client::connect(&host_invite, addr, now()).await;
    let mut guest = Client::connect(&guest_invite, addr, now()).await;
    assert!(host.pump_until_joined(start).await, "host never joined");
    assert!(guest.pump_until_joined(start).await, "guest never joined");

    host.core.revoke(guest_jti).unwrap();
    host.pump_for(start, Duration::from_millis(400)).await;
    guest.pump_for(start, Duration::from_millis(400)).await;
    assert!(
        !matches!(guest.core.state(), ClientState::Joined),
        "revoked guest stayed joined"
    );

    let _ = stop_tx.send(());
    task.await.unwrap().unwrap();

    // The revocation is on disk, not just in the dead process's memory.
    assert_eq!(
        Revocations::new(revoked_file.clone()).load(),
        vec![guest_jti],
        "revocation never reached {}",
        revoked_file.display()
    );

    // Second process, same session keys and same revocation file: the guest's
    // invite is refused, and refused silently, which to the client looks like
    // packet loss.
    let server = Server::bind(
        &f.cfg,
        Options {
            bind: loopback(),
            activity_path: None,
            recording: None,
        },
    )
    .await
    .unwrap()
    .with_revocations(Revocations::new(revoked_file.clone()));
    let addr2 = server.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async {
        let _ = stop_rx.await;
    }));

    let guest_invite2 = f.mint(1, Role::Musician, guest_jti, addr2);
    let mut guest = Client::connect(&guest_invite2, addr2, now()).await;
    guest.pump_for(start, Duration::from_millis(600)).await;
    assert_eq!(
        *guest.core.state(),
        ClientState::Connecting,
        "a restart handed the revoked invite back"
    );

    // A member whose token was never revoked still gets in, so the check is
    // the revocation and not a broken second boot.
    let other = f.mint(2, Role::Musician, TokenId::generate(), addr2);
    let mut other = Client::connect(&other, addr2, now()).await;
    assert!(
        other.pump_until_joined(start).await,
        "the second process admitted nobody"
    );

    let _ = stop_tx.send(());
    task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stop used to break the loop and drop the socket. Members must be told.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_graceful_stop_tells_every_member_before_exiting() {
    let f = Fixture::new();
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;

    let server = Server::bind(
        &f.cfg,
        Options {
            bind: loopback(),
            activity_path: None,
            recording: None,
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async {
        let _ = stop_rx.await;
    }));

    let mut members = Vec::new();
    for (id, role) in [
        (0u16, Role::Musician),
        (1, Role::Musician),
        (5, Role::Listener),
    ] {
        let invite = f.mint(id, role, TokenId::generate(), addr);
        let mut c = Client::connect(&invite, addr, now()).await;
        assert!(c.pump_until_joined(start).await, "member {id} never joined");
        members.push(c);
    }

    let _ = stop_tx.send(());
    task.await.unwrap().unwrap();

    // The Bye is one flight with no retransmit, so it is already on the wire
    // by the time run() returns; the clients only have to read it.
    for (i, c) in members.iter_mut().enumerate() {
        c.pump_for(start, Duration::from_millis(200)).await;
        assert!(
            c.events
                .iter()
                .any(|e| matches!(e, ClientEvent::Ejected { .. })),
            "member {i} was never told the session ended: {:?}",
            c.events
        );
    }
}

/// The sentinel door, which is how the local provider stops a server on
/// Windows. The marker beside it is the provider's signal that the door
/// exists at all; without it the provider skips the graceful wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_shutdown_sentinel_exits_cleanly_and_advertises_itself() {
    let f = Fixture::new();
    let dir = temp_dir("shutdown-sentinel");
    let sentinel = dir.join("shutdown");

    let server = Server::bind(
        &f.cfg,
        Options {
            bind: loopback(),
            activity_path: None,
            recording: None,
        },
    )
    .await
    .unwrap()
    .with_shutdown_file(sentinel.clone());
    let marker = dir.join("shutdown.supported");
    assert!(
        marker.is_file(),
        "the provider looks for {}",
        marker.display()
    );

    // Never resolves: the sentinel is the only way out of this run.
    let task = tokio::spawn(server.run(std::future::pending::<()>()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!task.is_finished(), "server exited before being asked");
    std::fs::write(&sentinel, b"requested_unix=0\n").unwrap();

    // The sentinel is read on the one-second heartbeat.
    let exited = tokio::time::timeout(Duration::from_secs(4), task).await;
    assert!(
        exited.is_ok(),
        "server ignored the shutdown sentinel at {}",
        sentinel.display()
    );
    exited.unwrap().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The real binary, the real signal. SIGTERM had its default disposition, so
/// the kernel killed the process and the shutdown future never resolved: the
/// exit code was not zero and nobody was told anything.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sigtermed_process_says_goodbye_and_exits_zero() {
    use std::process::{Command, Stdio};

    let f = Fixture::new();
    let dir = temp_dir("sigterm");
    let config_path = dir.join("config");
    // A fixed port: the child owns the socket, so the test cannot ask it
    // which one it got. 43307 is outside the ephemeral range on every
    // platform we run on.
    let port = 43_307u16;
    std::fs::write(
        &config_path,
        format!(
            "session_id_hex = {}\nport = {port}\nserver_private_key_b64 = {}\nissuer_public_key_b64 = {}\nidle_shutdown_min = 0\nmax_duration_min = 0\n",
            f.cfg.session_id.0.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            data_encoding::BASE64.encode(&f.cfg.server_private_key),
            data_encoding::BASE64.encode(&f.cfg.issuer_public_key),
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_jamstreamd"))
        .arg("--config")
        .arg(&config_path)
        .arg("--activity-file")
        .arg(dir.join("last-active"))
        .arg("--revoked-file")
        .arg(dir.join("revoked"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let invite = f.mint(0, Role::Musician, TokenId::generate(), addr);
    let mut client = Client::connect(&invite, addr, now()).await;
    if !client.pump_until_joined(start).await {
        let _ = child.kill();
        panic!("client never joined the spawned jamstreamd on port {port}");
    }

    send_sigterm(child.id());

    // Read the Bye and then confirm a clean exit. The Bye is sent before the
    // process returns from run(), so it is already in the socket buffer.
    client.pump_for(start, Duration::from_millis(500)).await;
    assert!(
        client
            .events
            .iter()
            .any(|e| matches!(e, ClientEvent::Ejected { .. })),
        "SIGTERM killed the session without telling the member: {:?}",
        client.events
    );

    let status = wait_with_deadline(&mut child, Duration::from_secs(5));
    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM must be a clean exit, got {status:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
fn send_sigterm(pid: u32) {
    // One libc call, and adding libc to the server crate's dev-dependencies
    // for it is not worth it: `kill` is in POSIX and on every runner.
    let ok = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "cannot signal pid {pid}");
}

#[cfg(unix)]
fn wait_with_deadline(
    child: &mut std::process::Child,
    window: Duration,
) -> std::process::ExitStatus {
    let deadline = Instant::now() + window;
    loop {
        match child.try_wait().unwrap() {
            Some(status) => return status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("jamstreamd did not exit within {window:?} of SIGTERM");
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}
