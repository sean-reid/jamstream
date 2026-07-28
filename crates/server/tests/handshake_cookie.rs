//! User story: somebody floods a session's port with handshake inits from
//! addresses that do not exist, and a musician with a real invite still gets
//! in. A real jamstreamd socket, a real flood over loopback, and a real client
//! core doing the cookie round trip. The deterministic budget coverage lives
//! in crates/session/src/server.rs and crates/session/tests/loopback.rs.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use jamstream_protocol::PROTOCOL_VERSION;
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_protocol::wire::{self, Packet};
use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, Server};
use jamstream_session::client::{ClientCore, ClientState};
use tokio::net::UdpSocket;

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

#[tokio::test]
async fn a_musician_joins_a_flooded_session() {
    let issuer = Issuer::generate();
    let server_keys = generate_keypair();
    let session_id = SessionId::generate();

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
            activity_path: None,
            recording: None,
        },
    )
    .await
    .unwrap();
    let server_addr = server.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(server.run(async {
        let _ = stop_rx.await;
    }));

    // The flood: enough inits to drain the cookie trigger, from a socket that
    // reads nothing back. Loopback cannot spoof a source, so this stands in
    // for a flood by rate alone, which is all the trigger measures.
    let flooder = UdpSocket::bind(loopback()).await.unwrap();
    flooder.connect(server_addr).await.unwrap();
    let garbage = wire::build_handshake_init(PROTOCOL_VERSION, &[0xAA; 96]);
    for _ in 0..200 {
        flooder.send(&garbage).await.unwrap();
    }

    let invite = issuer.mint(
        session_id,
        vec![server_addr],
        server_keys.public,
        Token {
            member_id: MemberId(1),
            role: Role::Musician,
            name_hint: None,
            expires_unix: u64::MAX,
            jti: TokenId::generate(),
        },
    );

    // A real client core driving a real socket, with nothing in the loop that
    // knows what the answer is supposed to be.
    let socket = UdpSocket::bind(loopback()).await.unwrap();
    socket.connect(server_addr).await.unwrap();
    let (mut core, init) = ClientCore::connect(&invite, 0).unwrap();
    socket.send(&init).await.unwrap();

    let mut buf = [0u8; 2048];
    let mut challenged = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut now_ms = 0u64;
    while *core.state() == ClientState::Connecting && tokio::time::Instant::now() < deadline {
        now_ms += 100;
        if let Ok(Ok(len)) =
            tokio::time::timeout(Duration::from_millis(100), socket.recv(&mut buf)).await
        {
            if matches!(wire::parse(&buf[..len]), Ok(Packet::CookieChallenge { .. })) {
                challenged = true;
            }
            for pkt in core.handle_datagram(now_ms, &buf[..len]) {
                socket.send(&pkt).await.unwrap();
            }
        }
        for pkt in core.poll(now_ms) {
            socket.send(&pkt).await.unwrap();
        }
    }

    assert!(
        challenged,
        "the server never asked for a cookie, so this proved nothing"
    );
    assert_eq!(
        *core.state(),
        ClientState::Joined,
        "the musician did not get in through the flood"
    );

    let _ = stop_tx.send(());
    server_task.await.unwrap().unwrap();
}
