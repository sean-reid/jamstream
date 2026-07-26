//! User story: every musician disconnects and the dead man's switch tears
//! the server down. The teardown itself is a systemd path unit on the VM;
//! what the runtime owns, and what this test proves over real UDP, is the
//! guard's input signal: the activity file's mtime advances while musicians
//! are connected and stops advancing once they all leave.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, Server};
use jamstream_session::client::{ClientCore, ClientState};
use tokio::net::UdpSocket;

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

struct Client {
    core: ClientCore,
    socket: UdpSocket,
}

impl Client {
    /// One pump pass: flush queued traffic, then drain a bounded batch of
    /// datagrams. Bounded because a joined musician receives a mix frame
    /// every 2.5 ms; draining until quiet would never return.
    async fn pump(&mut self, now_ms: u64) {
        for pkt in self.core.poll(now_ms) {
            self.socket.send(&pkt).await.unwrap();
        }
        let mut buf = [0u8; 2048];
        for _ in 0..64 {
            let Ok(Ok(len)) =
                tokio::time::timeout(Duration::from_millis(2), self.socket.recv(&mut buf)).await
            else {
                break;
            };
            for pkt in self.core.handle_datagram(now_ms, &buf[..len]) {
                self.socket.send(&pkt).await.unwrap();
            }
        }
        // Events are not asserted here; drain so the queue stays bounded.
        let _ = self.core.events();
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().map(|m| m.modified().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_file_advances_with_musicians_and_stops_after_they_leave() {
    let issuer = Issuer::generate();
    let server_keys = generate_keypair();
    let session_id = SessionId::generate();

    let dir = std::env::temp_dir().join(format!("jamstream-deadman-{}", std::process::id()));
    let activity = dir.join("activity");

    let cfg = Config {
        session_id,
        port: 0,
        server_private_key: server_keys.private.to_vec(),
        issuer_public_key: issuer.public_key().to_bytes(),
        idle_shutdown_min: 10,
        max_duration_min: 720,
    };
    let server = Server::bind(
        &cfg,
        Options {
            bind: loopback(),
            activity_path: Some(activity.clone()),
        },
    )
    .await
    .unwrap();
    let server_addr = server.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(server.run(async {
        let _ = stop_rx.await;
    }));

    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;

    // Two musicians join over real UDP.
    let mut clients = Vec::new();
    for member in [1u16, 2] {
        let invite = issuer.mint(
            session_id,
            vec![server_addr],
            server_keys.public,
            Token {
                member_id: MemberId(member),
                role: Role::Musician,
                name_hint: None,
                expires_unix: u64::MAX,
                jti: TokenId::generate(),
            },
        );
        let socket = UdpSocket::bind(loopback()).await.unwrap();
        socket.connect(server_addr).await.unwrap();
        let (core, first) = ClientCore::connect(&invite, now()).unwrap();
        socket.send(&first).await.unwrap();
        clients.push(Client { core, socket });
    }
    let join_deadline = Instant::now() + Duration::from_secs(5);
    while clients
        .iter()
        .any(|c| *c.core.state() != ClientState::Joined)
    {
        assert!(
            Instant::now() < join_deadline,
            "clients never joined over real udp"
        );
        for c in &mut clients {
            c.pump(now()).await;
        }
    }

    // While both are connected the heartbeat touches the file about once a
    // second; over ~3.5 s of polling the mtime must advance repeatedly.
    let mut observed: Vec<SystemTime> = Vec::new();
    let watch_until = Instant::now() + Duration::from_millis(3_500);
    while Instant::now() < watch_until {
        for c in &mut clients {
            c.pump(now()).await;
        }
        if let Some(m) = mtime(&activity)
            && observed.last() != Some(&m)
        {
            observed.push(m);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        observed.len() >= 3,
        "activity mtime advanced only {} time(s) in 3.5 s with musicians connected",
        observed.len()
    );

    // Both leave cleanly; pump long enough for the Byes to be delivered and
    // acked (the control link retransmits at 100 ms).
    for c in &mut clients {
        c.core.leave("deadman test done").unwrap();
    }
    let flush_until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < flush_until {
        for c in &mut clients {
            c.pump(now()).await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A heartbeat already in flight may land just after the leaves; skip
    // past that window, then compare across a window longer than two
    // heartbeat periods. Identical mtimes mean the signal stopped.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let settled = mtime(&activity).expect("activity file must exist by now");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let later = mtime(&activity).expect("activity file must persist");
    assert_eq!(
        settled, later,
        "activity mtime kept advancing after every musician left"
    );

    let _ = stop_tx.send(());
    server_task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
