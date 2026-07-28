//! End-to-end over real UDP on loopback: a genuine jamstreamd runtime and a
//! genuine ClientCore, real sockets, real encryption, real time. The
//! deterministic scenario coverage lives in the harness crate; this test
//! proves the socket driver.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, Server};
use jamstream_session::client::{ClientCore, ClientEvent, ClientState};
use tokio::net::UdpSocket;

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

#[tokio::test]
async fn join_chat_and_leave_over_real_udp() {
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
            name_hint: Some("loopback".into()),
            expires_unix: u64::MAX,
            jti: TokenId::generate(),
        },
    );

    let start = Instant::now();
    let now = |start: Instant| start.elapsed().as_millis() as u64;

    let socket = UdpSocket::bind(loopback()).await.unwrap();
    socket.connect(server_addr).await.unwrap();

    let (mut client, first) = ClientCore::connect(&invite, now(start)).unwrap();
    socket.send(&first).await.unwrap();

    let mut joined = false;
    let mut chat_echoed = false;
    let mut buf = [0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(5);

    while Instant::now() < deadline && !(joined && chat_echoed) {
        // Pump the client's own timers and queued control traffic.
        for pkt in client.poll(now(start)) {
            socket.send(&pkt).await.unwrap();
        }
        if let Ok(Ok(len)) =
            tokio::time::timeout(Duration::from_millis(20), socket.recv(&mut buf)).await
        {
            for pkt in client.handle_datagram(now(start), &buf[..len]) {
                socket.send(&pkt).await.unwrap();
            }
        }
        for event in client.events() {
            match event {
                ClientEvent::Joined => {
                    joined = true;
                    client.send_chat("hello from the integration test").unwrap();
                }
                ClientEvent::Chat { from, text } => {
                    assert_eq!(from, MemberId(1));
                    assert_eq!(text, "hello from the integration test");
                    chat_echoed = true;
                }
                _ => {}
            }
        }
    }

    assert!(joined, "client never joined over real udp");
    assert!(chat_echoed, "chat never echoed back");
    assert!(matches!(client.state(), ClientState::Joined));

    client.leave("test done").unwrap();
    for pkt in client.poll(now(start)) {
        socket.send(&pkt).await.unwrap();
    }

    let _ = stop_tx.send(());
    server_task.await.unwrap().unwrap();
}
