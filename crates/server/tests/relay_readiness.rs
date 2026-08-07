//! Whether a session can broadcast, as a member actually finds out: a real
//! jamstreamd over real UDP, a real relay socket that goes away, and a real
//! client core on the other end.
//!
//! The interesting case is not a healthy relay but one that is there and then
//! is not. Nothing else tells that apart from a working one: systemd calls a
//! `Type=simple` unit started the moment it forks, and everything after boot
//! goes to a journal no host can read. Paired with the reason cloud-init leaves
//! behind when the broadcast tooling never downloaded at all, which is the
//! sentence the host is shown instead of a generic absence.

mod common;

use std::net::TcpListener;
use std::time::{Duration, Instant};

use common::{Running, Session, budget, loopback, scratch_dir};
use jamstream_protocol::control::BroadcastReadiness;
use jamstream_protocol::ids::{Role, TokenId};
use jamstream_protocol::invite::Invite;
use jamstream_server::runtime::Server;
use jamstream_session::client::{ClientCore, ClientEvent};
use jamstream_session::testing::pump;
use jamstream_stream::pipeline::StreamConfig;
use tokio::net::UdpSocket;

/// A member, and every readiness answer it has been told.
struct Peer {
    socket: UdpSocket,
    core: ClientCore,
    readiness: Vec<BroadcastReadiness>,
}

impl Peer {
    async fn connect(invite: &Invite, server: std::net::SocketAddr, now_ms: u64) -> Peer {
        let socket = UdpSocket::bind(loopback()).await.unwrap();
        socket.connect(server).await.unwrap();
        let (core, first) = ClientCore::connect(invite, now_ms).unwrap();
        socket.send(&first).await.unwrap();
        Peer {
            socket,
            core,
            readiness: Vec::new(),
        }
    }

    async fn pump(&mut self, now_ms: u64) {
        for event in pump(&self.socket, &mut self.core, now_ms).await {
            if let ClientEvent::BroadcastReadiness(state) = event {
                self.readiness.push(state);
            }
        }
    }

    fn last(&self) -> Option<&BroadcastReadiness> {
        self.readiness.last()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_that_dies_is_reported_to_every_member_with_a_reason() {
    let dir = scratch_dir("relayready");
    let note = dir.join("broadcast-unavailable");

    // The stand-in relay. Bound before the server starts, exactly like a
    // session VM where the relay is up before anyone joins.
    let relay = TcpListener::bind("127.0.0.1:0").expect("bind a stand-in relay");
    let relay_addr = relay.local_addr().unwrap();
    let relay_url = format!("rtmp://{relay_addr}/jamstream");

    let session = Session::new();
    let server = Server::bind(&session.cfg, Running::plain_options())
        .await
        .unwrap()
        .with_stream_config(StreamConfig {
            encoder_output: relay_url.clone(),
            pusher_input: relay_url,
            work_dir: dir.clone(),
            key_dir: dir.join("keys"),
            ..StreamConfig::default()
        })
        .with_broadcast_note(note.clone());
    let server = Running::of(server);
    let addr = server.addr;

    let mint = |member: u16, role: Role| {
        session.invite(
            member,
            role,
            TokenId::generate(),
            Some(format!("member{member}")),
            addr,
        )
    };
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    // The host, and a listener who is not the host: everyone in the room is
    // being broadcast, so everyone is told whether the room can be.
    let mut host = Peer::connect(&mint(0, Role::Musician), addr, now()).await;
    let mut listener = Peer::connect(&mint(5, Role::Listener), addr, now()).await;

    // Phase one: the relay is listening, so the session can stream.
    let deadline = Instant::now() + budget(Duration::from_secs(15));
    while Instant::now() < deadline {
        host.pump(now()).await;
        listener.pump(now()).await;
        if host.last() == Some(&BroadcastReadiness::Ready)
            && listener.last() == Some(&BroadcastReadiness::Ready)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        (host.last(), listener.last()),
        (
            Some(&BroadcastReadiness::Ready),
            Some(&BroadcastReadiness::Ready)
        ),
        "a listening relay was never reported ready"
    );

    // Phase two: the relay dies, which is the failure that could not be seen.
    // The note is what cloud-init leaves when the tooling never arrived; it is
    // written here to prove the sentence in that file is the one that reaches
    // the host rather than a reason this process invented.
    std::fs::write(&note, "the broadcast tooling could not be downloaded\n").unwrap();
    drop(relay);

    let deadline = Instant::now() + budget(Duration::from_secs(30));
    while Instant::now() < deadline {
        host.pump(now()).await;
        listener.pump(now()).await;
        if matches!(host.last(), Some(BroadcastReadiness::Unavailable { .. })) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let want = BroadcastReadiness::Unavailable {
        reason: "the broadcast tooling could not be downloaded".to_owned(),
    };
    for (who, peer) in [("host", &host), ("listener", &listener)] {
        assert_eq!(
            peer.last(),
            Some(&want),
            "{who} was not told the session cannot broadcast"
        );
        // Two answers, not one a second: this changes at most twice a session.
        assert_eq!(peer.readiness.len(), 2, "{who} got {:?}", peer.readiness);
    }

    // And a member who joins after the relay died is told at join, not on the
    // next change, because there is not going to be one.
    let mut late = Peer::connect(&mint(6, Role::Listener), addr, now()).await;
    let deadline = Instant::now() + budget(Duration::from_secs(10));
    while Instant::now() < deadline && late.last().is_none() {
        late.pump(now()).await;
        host.pump(now()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(late.last(), Some(&want), "a late joiner was told nothing");

    server.stop().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
