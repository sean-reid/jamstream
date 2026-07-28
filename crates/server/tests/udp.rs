//! End-to-end over real UDP on loopback: a genuine jamstreamd runtime and a
//! genuine ClientCore, real sockets, real encryption, real time. The
//! deterministic scenario coverage lives in the harness crate; this test
//! proves the socket driver.

mod common;

use common::{Running, Session, loopback};
use std::time::{Duration, Instant};

use jamstream_protocol::ids::{MemberId, Role, TokenId};
use jamstream_session::client::{ClientCore, ClientEvent, ClientState};
use tokio::net::UdpSocket;

#[tokio::test]
async fn join_chat_and_leave_over_real_udp() {
    let session = Session::new();
    let server = Running::spawn(&session, Running::plain_options()).await;
    let server_addr = server.addr;

    let invite = session.invite(
        1,
        Role::Musician,
        TokenId::generate(),
        Some("loopback".to_owned()),
        server_addr,
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

    server.stop().await.unwrap();
}
