//! User story: a client from the future (or the past) speaks the wrong
//! protocol version and gets told so, verifiably. A first flight claiming
//! version 2 hits a real server socket; the reply must parse as a
//! VersionReject whose MAC verifies under the secret that client shares with
//! the server, and a reject any invite holder could build must not verify and
//! must be ignored by a real client core. The deterministic rate-limit
//! coverage lives in crates/session/tests/loopback.rs.

mod common;

use std::time::Duration;

use common::{Running, Session, loopback};
use jamstream_protocol::PROTOCOL_VERSION;
use jamstream_protocol::transport::Initiator;
use jamstream_protocol::wire::{self, Packet};
use jamstream_session::client::{ClientCore, ClientState};
use tokio::net::UdpSocket;

#[tokio::test]
async fn wrong_version_init_gets_a_mac_verified_reject() {
    let session = Session::new();
    let server = Running::spawn(&session, Running::plain_options()).await;
    let server_addr = server.addr;

    let invite = session.musician(1, server_addr);

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

    server.stop().await.unwrap();
}
