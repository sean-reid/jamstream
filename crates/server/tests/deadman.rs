//! User story: every musician disconnects and the dead man's switch tears
//! the server down. The teardown itself is a systemd path unit on the VM;
//! what the runtime owns, and what this test proves over real UDP, is the
//! guard's input signal: the activity file's mtime advances while musicians
//! are connected and stops advancing once they all leave.

mod common;

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use common::{Running, Session, budget, loopback, scratch_dir};
use jamstream_server::runtime::{Options, Server};
use jamstream_session::client::{ClientCore, ClientState};
use jamstream_session::testing::pump;
use tokio::net::UdpSocket;

struct Client {
    core: ClientCore,
    socket: UdpSocket,
}

impl Client {
    /// One pump pass. Events are not asserted here; the pass drains them so
    /// the queue stays bounded.
    async fn pump(&mut self, now_ms: u64) {
        let _ = pump(&self.socket, &mut self.core, now_ms).await;
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().map(|m| m.modified().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_file_advances_with_musicians_and_stops_after_they_leave() {
    let session = Session::new();
    let dir = scratch_dir("deadman");
    let activity = dir.join("activity");
    let server = Running::of(
        Server::bind(
            &session.cfg,
            Options {
                activity_path: Some(activity.clone()),
                ..Running::plain_options()
            },
        )
        .await
        .unwrap(),
    );
    let server_addr = server.addr;

    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;

    // Two musicians join over real UDP.
    let mut clients = Vec::new();
    for member in [1u16, 2] {
        let invite = session.musician(member, server_addr);
        let socket = UdpSocket::bind(loopback()).await.unwrap();
        socket.connect(server_addr).await.unwrap();
        let (core, first) = ClientCore::connect(&invite, now()).unwrap();
        socket.send(&first).await.unwrap();
        clients.push(Client { core, socket });
    }
    let join_deadline = Instant::now() + budget(Duration::from_secs(5));
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

    server.stop().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
