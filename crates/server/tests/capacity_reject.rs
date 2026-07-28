//! User story: the band is full, and the musician who arrives late is told
//! so instead of watching a progress spinner for ten seconds and then being
//! told the server timed out. A real jamstreamd socket, a real session filled
//! to `MAX_MUSICIANS`, and a real client core on the receiving end. The
//! deterministic rate-limit and free-seat coverage lives in
//! crates/session/tests/loopback.rs.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::{Initiator, generate_keypair};
use jamstream_protocol::wire::{self, Packet};
use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, Server};
use jamstream_session::MAX_MUSICIANS;
use jamstream_session::client::{ClientCore, ClientEvent, ClientState};
use tokio::net::UdpSocket;

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

/// Sends `init` from a fresh socket and returns the server's answer.
async fn exchange(server_addr: SocketAddr, init: &[u8], what: &str) -> (UdpSocket, Vec<u8>) {
    let socket = UdpSocket::bind(loopback()).await.unwrap();
    socket.connect(server_addr).await.unwrap();
    socket.send(init).await.unwrap();
    let mut buf = [0u8; 2048];
    let len = tokio::time::timeout(Duration::from_secs(3), socket.recv(&mut buf))
        .await
        .unwrap_or_else(|_| panic!("server never answered {what}"))
        .unwrap();
    (socket, buf[..len].to_vec())
}

#[tokio::test]
async fn a_full_band_tells_the_late_arrival_so() {
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

    let mint = |member: u16| -> Invite {
        issuer.mint(
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
        )
    };

    // Fill every musician seat, the host's included. The sockets are held for
    // the rest of the test: a dropped one would be a member the server can no
    // longer reach, which is not what is being measured here.
    let mut seated = Vec::new();
    for member in 0..MAX_MUSICIANS as u16 {
        let invite = mint(member);
        let (_, init) = Initiator::new(&invite).unwrap();
        let (socket, answer) = exchange(server_addr, &init, "a musician joining").await;
        assert!(
            matches!(wire::parse(&answer), Ok(Packet::HandshakeResp { .. })),
            "member {member} was not admitted: {:?}",
            wire::parse(&answer)
        );
        seated.push(socket);
    }

    // The late arrival holds a perfectly good invite and gets an answer, not
    // silence. Through a real client core, because what the issue is about is
    // what the person joining sees.
    let late = mint(MAX_MUSICIANS as u16);
    let (mut core, init) = ClientCore::connect(&late, 0).unwrap();
    let (_socket, answer) = exchange(server_addr, &init, "the over-capacity init").await;
    let Ok(Packet::CapacityReject { mac }) = wire::parse(&answer) else {
        panic!("expected a capacity reject, got {:?}", wire::parse(&answer));
    };
    assert!(answer.len() < init.len(), "a reject must never amplify");

    assert!(core.handle_datagram(1, &answer).is_empty());
    assert!(
        core.session_full(),
        "the client was not told the band is full"
    );
    assert_eq!(core.events(), vec![ClientEvent::SessionFull]);
    // Still connecting, and still trying: the server is answering, so a
    // timeout would be the one thing this client knows to be false.
    assert_eq!(*core.state(), ClientState::Connecting);
    core.poll(60_000);
    assert_eq!(*core.state(), ClientState::Connecting);

    // Only the server could have produced it. An invite carries the server's
    // public key and nothing else, so an invite holder's own handshake derives
    // a different key and their forgery is ignored.
    let (invite_holder, _) = Initiator::new(&late).unwrap();
    let forged = wire::build_capacity_reject(invite_holder.reject_key().unwrap(), &init);
    let Ok(Packet::CapacityReject { mac: forged_mac }) = wire::parse(&forged) else {
        panic!("a forged reject must still parse");
    };
    assert_ne!(forged_mac, mac);
    let (mut fresh, fresh_init) = ClientCore::connect(&late, 0).unwrap();
    let forged_for_fresh =
        wire::build_capacity_reject(invite_holder.reject_key().unwrap(), &fresh_init);
    assert!(fresh.handle_datagram(1, &forged_for_fresh).is_empty());
    assert!(!fresh.session_full());
    assert!(fresh.events().is_empty());

    let _ = stop_tx.send(());
    server_task.await.unwrap().unwrap();
}
