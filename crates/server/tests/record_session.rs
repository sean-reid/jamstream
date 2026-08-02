//! User story: the host presses record, everyone in the room is told,
//! someone who walks in mid-take is told before their first note, the host
//! presses stop, and a take exists on disk. All of it through the real
//! runtime over real UDP: the wire message drives the recorder that writes
//! the file, not a test double on either side.

mod common;

use std::time::{Duration, Instant};

use common::{Running, Session, budget, loopback, scratch_dir};
use jamstream_protocol::control::{RecordOp, RecordingState};
use jamstream_protocol::ids::HOST_MEMBER_ID;
use jamstream_protocol::invite::Invite;
use jamstream_server::runtime::{Options, RecordingOptions, Server};
use jamstream_session::client::{ClientCore, ClientEvent, ClientState};
use jamstream_session::testing::pump;
use tokio::net::UdpSocket;

struct Client {
    core: ClientCore,
    socket: UdpSocket,
    events: Vec<ClientEvent>,
}

impl Client {
    async fn join(invite: &Invite, now_ms: u64) -> Client {
        let socket = UdpSocket::bind(loopback()).await.unwrap();
        socket.connect(invite.addresses[0]).await.unwrap();
        let (core, first) = ClientCore::connect(invite, now_ms).unwrap();
        socket.send(&first).await.unwrap();
        Client {
            core,
            socket,
            events: Vec::new(),
        }
    }

    /// One pump pass, keeping the events: this test is about what the
    /// members were told.
    async fn pump(&mut self, now_ms: u64) {
        self.events
            .extend(pump(&self.socket, &mut self.core, now_ms).await);
    }

    fn last_record_state(&self) -> Option<&RecordingState> {
        self.events.iter().rev().find_map(|e| match e {
            ClientEvent::RecordStatus { state, .. } => Some(state),
            _ => None,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_take_driven_over_the_wire_lands_on_disk_and_everyone_is_told() {
    let session = Session::new();
    let dir = scratch_dir("record-session");
    let server = Running::of(
        Server::bind(
            &session.cfg,
            Options {
                recording: Some(RecordingOptions::Disk {
                    dir: dir.clone(),
                    stems: false,
                }),
                ..Running::plain_options()
            },
        )
        .await
        .unwrap(),
    );
    let server_addr = server.addr;

    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;

    let mut host = Client::join(&session.musician(HOST_MEMBER_ID.0, server_addr), now()).await;
    let mut guest = Client::join(&session.musician(1, server_addr), now()).await;

    let deadline = Instant::now() + budget(Duration::from_secs(5));
    while *host.core.state() != ClientState::Joined || *guest.core.state() != ClientState::Joined {
        assert!(Instant::now() < deadline, "clients never joined");
        host.pump(now()).await;
        guest.pump(now()).await;
    }

    // The host presses record; the room is told, the host included, and
    // only by the server: no optimistic echo exists to fake this.
    host.core.record_ctl(RecordOp::Start).unwrap();
    let deadline = Instant::now() + budget(Duration::from_secs(5));
    loop {
        host.pump(now()).await;
        guest.pump(now()).await;
        if host.last_record_state() == Some(&RecordingState::Recording)
            && guest.last_record_state() == Some(&RecordingState::Recording)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the room was never told it is being recorded; host saw {:?}, guest {:?}",
            host.last_record_state(),
            guest.last_record_state()
        );
    }

    // A mid-take joiner is told on arrival, not on the next transition.
    let mut late = Client::join(&session.musician(2, server_addr), now()).await;
    let deadline = Instant::now() + budget(Duration::from_secs(5));
    while late.last_record_state() != Some(&RecordingState::Recording) {
        assert!(
            Instant::now() < deadline,
            "the mid-take joiner was never told; saw {:?}",
            late.last_record_state()
        );
        host.pump(now()).await;
        guest.pump(now()).await;
        late.pump(now()).await;
    }

    // Let the recorder see some ticks, then stop; everyone hears Idle.
    tokio::time::sleep(Duration::from_millis(300)).await;
    host.core.record_ctl(RecordOp::Stop).unwrap();
    let deadline = Instant::now() + budget(Duration::from_secs(5));
    while host.last_record_state() != Some(&RecordingState::Idle)
        || late.last_record_state() != Some(&RecordingState::Idle)
    {
        assert!(
            Instant::now() < deadline,
            "the room was never told the take ended; host saw {:?}, late {:?}",
            host.last_record_state(),
            late.last_record_state()
        );
        host.pump(now()).await;
        guest.pump(now()).await;
        late.pump(now()).await;
    }

    let _ = server.stop().await;

    // One mix file, finished (no .part), and it decodes with an independent
    // decoder to a non-empty take.
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let flacs: Vec<_> = files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "flac"))
        .collect();
    assert_eq!(flacs.len(), 1, "expected exactly one take: {files:?}");
    assert!(
        flacs[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("mix"),
        "the one file is the mix: {files:?}"
    );
    let mut reader = claxon::FlacReader::open(flacs[0]).unwrap();
    let samples: Vec<i32> = reader.samples().map(Result::unwrap).collect();
    assert!(
        !samples.is_empty(),
        "the take decoded to zero samples; the recorder never saw a tick"
    );

    std::fs::remove_dir_all(&dir).ok();
}
