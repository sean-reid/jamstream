//! User story: a client from the future (or the past) speaks the wrong
//! protocol version and gets told so, verifiably. A first flight claiming
//! version 2 hits a real server socket; the reply must parse as a
//! VersionReject whose MAC verifies under the secret that client shares with
//! the server, and a reject any invite holder could build must not verify and
//! must be ignored by a real client core. The deterministic rate-limit
//! coverage lives in crates/session/tests/loopback.rs.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use jamstream_protocol::PROTOCOL_VERSION;
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Issuer, Token};
use jamstream_protocol::transport::{Initiator, generate_keypair};
use jamstream_protocol::wire::{self, Packet};
use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, Server};
use jamstream_session::client::{ClientCore, ClientState};
use tokio::net::UdpSocket;

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

#[tokio::test]
async fn wrong_version_init_gets_a_mac_verified_reject() {
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

    // A handshake init claiming version 2 against a version-1 server. It has
    // to be a first flight the server can read: the reject is authenticated
    // with a secret recovered from the init, so garbage draws silence.
    let wrong_version = PROTOCOL_VERSION + 1;
    let (from_the_future, init) = Initiator::new_claiming_version(&invite, wrong_version).unwrap();
    let socket = UdpSocket::bind(loopback()).await.unwrap();
    socket.connect(server_addr).await.unwrap();
    socket.send(&init).await.unwrap();

    let mut buf = [0u8; 2048];
    let len = tokio::time::timeout(Duration::from_secs(3), socket.recv(&mut buf))
        .await
        .expect("server never answered the wrong-version init")
        .unwrap();

    // The reply parses as a reject naming both versions.
    let Packet::VersionReject { ours, theirs, mac } = wire::parse(&buf[..len]).unwrap() else {
        panic!(
            "expected a version reject, got {:?}",
            wire::parse(&buf[..len])
        );
    };
    assert_eq!(ours, PROTOCOL_VERSION);
    assert_eq!(theirs, wrong_version);

    // It MAC-verifies under the key that init established with the server,
    // bound to the exact init it answers.
    let key = from_the_future
        .reject_key()
        .expect("client derives the key");
    assert!(
        wire::verify_version_reject(key, ours, theirs, &mac, &init),
        "reject must verify under the key the init established"
    );
    // Bound to this init: the same reject does not vouch for another one.
    let (_, other_init) = Initiator::new_claiming_version(&invite, wrong_version).unwrap();
    assert!(!wire::verify_version_reject(
        key,
        ours,
        theirs,
        &mac,
        &other_init
    ));

    // A forgery by someone holding the same invite fails verification. An
    // invite carries the server's public key, which is no longer enough.
    let (invite_holder, _) = Initiator::new(&invite).unwrap();
    let forged =
        wire::build_version_reject(invite_holder.reject_key().unwrap(), ours, theirs, &init);
    let Packet::VersionReject {
        mac: forged_mac, ..
    } = wire::parse(&forged).unwrap()
    else {
        panic!("forged reject must still parse");
    };
    assert!(!wire::verify_version_reject(
        key,
        ours,
        theirs,
        &forged_mac,
        &init
    ));

    // ...and a real client mid-handshake ignores it outright: no state
    // change, no event, no reply.
    let (mut core, sent_init) = ClientCore::connect(&invite, 0).unwrap();
    let forged_for_client = wire::build_version_reject(
        invite_holder.reject_key().unwrap(),
        PROTOCOL_VERSION,
        PROTOCOL_VERSION,
        &sent_init,
    );
    assert!(core.handle_datagram(1, &forged_for_client).is_empty());
    assert_eq!(*core.state(), ClientState::Connecting);
    assert!(core.events().is_empty());

    let _ = stop_tx.send(());
    server_task.await.unwrap().unwrap();
}
