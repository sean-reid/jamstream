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

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::{Running, Session, budget, loopback, scratch_dir};
// The signal half of this file is unix only, and so is everything it needs.
#[cfg(unix)]
use common::{BIND, ChildGuard, ReservedPort, server_binary};
use jamstream_protocol::ids::{Role, TokenId};
use jamstream_protocol::invite::Invite;
use jamstream_server::revocations::Revocations;
use jamstream_server::runtime::Server;
use jamstream_session::client::{ClientCore, ClientEvent, ClientState};
use jamstream_session::testing::pump;
#[cfg(unix)]
use std::net::{IpAddr, Ipv4Addr};
use tokio::net::UdpSocket;

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

    /// One pump pass, keeping the events this test watches for.
    async fn pump(&mut self, now_ms: u64) {
        self.events
            .extend(pump(&self.socket, &mut self.core, now_ms).await);
    }

    async fn pump_until_joined(&mut self, start: Instant) -> bool {
        let deadline = Instant::now() + budget(Duration::from_secs(5));
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

/// The runner is described once, by the variable the harness already reads,
/// and a deadline can only ever get longer from it. A missing or nonsense
/// value has to leave the laptop budget alone rather than collapse to zero.
#[test]
fn a_deadline_scales_with_the_runner_and_never_shrinks() {
    assert_eq!(
        common::budget_scale(None),
        1.0,
        "unset is the laptop budget"
    );
    // What CI sets: 120 s against the harness's 30 s reference run.
    assert_eq!(common::budget_scale(Some("120")), 4.0);
    assert_eq!(common::budget_scale(Some("45")), 1.5);
    for nonsense in ["0", "-30", "", "soon", "NaN", "inf"] {
        assert_eq!(
            common::budget_scale(Some(nonsense)),
            1.0,
            "{nonsense:?} must not shorten a deadline"
        );
    }
    // Whatever the runner sets, a deadline is at least what it says.
    assert!(budget(Duration::from_secs(5)) >= Duration::from_secs(5));
}

/// The one that mattered: a revoked invite must stay revoked across the
/// restart that `Restart=on-failure` makes routine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_invite_stays_revoked_across_a_restart() {
    let f = Session::new();
    let dir = scratch_dir("revoke-restart");
    let revoked_file = dir.join("revoked");
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;

    // First process: host joins, a guest joins, the host revokes the guest.
    let server = Running::of(
        Server::bind(&f.cfg, Running::plain_options())
            .await
            .unwrap()
            .with_revocations(Revocations::new(revoked_file.clone())),
    );
    let addr = server.addr;

    let guest_jti = TokenId::generate();
    let host_invite = f.invite(0, Role::Musician, TokenId::generate(), None, addr);
    let guest_invite = f.invite(1, Role::Musician, guest_jti, None, addr);

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

    server.stop().await.unwrap();

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
    let server = Running::of(
        Server::bind(&f.cfg, Running::plain_options())
            .await
            .unwrap()
            .with_revocations(Revocations::new(revoked_file.clone())),
    );
    let addr2 = server.addr;

    let guest_invite2 = f.invite(1, Role::Musician, guest_jti, None, addr2);
    let mut guest = Client::connect(&guest_invite2, addr2, now()).await;
    guest.pump_for(start, Duration::from_millis(600)).await;
    assert_eq!(
        *guest.core.state(),
        ClientState::Connecting,
        "a restart handed the revoked invite back"
    );

    // A member whose token was never revoked still gets in, so the check is
    // the revocation and not a broken second boot.
    let other = f.musician(2, addr2);
    let mut other = Client::connect(&other, addr2, now()).await;
    assert!(
        other.pump_until_joined(start).await,
        "the second process admitted nobody"
    );

    server.stop().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stop tells every member before it drops the socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_graceful_stop_tells_every_member_before_exiting() {
    let f = Session::new();
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;

    let server = Running::spawn(&f, Running::plain_options()).await;
    let addr = server.addr;

    let mut members = Vec::new();
    for (id, role) in [
        (0u16, Role::Musician),
        (1, Role::Musician),
        (5, Role::Listener),
    ] {
        let invite = f.invite(id, role, TokenId::generate(), None, addr);
        let mut c = Client::connect(&invite, addr, now()).await;
        assert!(c.pump_until_joined(start).await, "member {id} never joined");
        members.push(c);
    }

    server.stop().await.unwrap();

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
    let f = Session::new();
    let dir = scratch_dir("shutdown-sentinel");
    let sentinel = dir.join("shutdown");

    let server = Server::bind(&f.cfg, Running::plain_options())
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
    let exited = tokio::time::timeout(budget(Duration::from_secs(4)), task).await;
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

    let f = Session::new();
    let dir = scratch_dir("sigterm");
    let config_path = dir.join("config");
    // The child owns the socket, so the test cannot ask it which port it got:
    // one is reserved here and held until the instant of the spawn, which is
    // narrower than a fixed number two concurrent runs can both pick.
    let mut reserved = ReservedPort::reserve();
    let port = reserved.port;
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

    reserved.release();
    let mut child = ChildGuard(
        Command::new(server_binary())
            .arg("--config")
            .arg(&config_path)
            .arg("--bind")
            .arg(BIND.to_string())
            .arg("--activity-file")
            .arg(dir.join("last-active"))
            .arg("--revoked-file")
            .arg(dir.join("revoked"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let invite = f.invite(0, Role::Musician, TokenId::generate(), None, addr);
    let mut client = Client::connect(&invite, addr, now()).await;
    assert!(
        client.pump_until_joined(start).await,
        "client never joined the spawned jamstreamd on port {port}"
    );

    send_sigterm(child.0.id());

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

    let status = wait_with_deadline(&mut child.0, budget(Duration::from_secs(5)));
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
