//! User story: the host presses record, everyone in the room is told,
//! someone who walks in mid-take is told before their first note, the host
//! presses stop, and a take exists on disk. All of it through the real
//! runtime over real UDP: the wire message drives the recorder that writes
//! the file, not a test double on either side.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use jamstream_protocol::control::{RecordOp, RecordingState};
use jamstream_protocol::ids::{HOST_MEMBER_ID, MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, RecordingOptions, Server};
use jamstream_session::client::{ClientCore, ClientEvent, ClientState};
use tokio::net::UdpSocket;

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

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
        self.events.extend(self.core.events());
    }

    fn last_record_state(&self) -> Option<&RecordingState> {
        self.events.iter().rev().find_map(|e| match e {
            ClientEvent::RecordStatus { state, .. } => Some(state),
            _ => None,
        })
    }
}

fn mint(issuer: &Issuer, session: SessionId, addr: SocketAddr, pk: [u8; 32], id: u16) -> Invite {
    issuer.mint(
        session,
        vec![addr],
        pk,
        Token {
            member_id: MemberId(id),
            role: Role::Musician,
            name_hint: None,
            expires_unix: u64::MAX,
            jti: TokenId::generate(),
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_take_driven_over_the_wire_lands_on_disk_and_everyone_is_told() {
    let issuer = Issuer::generate();
    let server_keys = generate_keypair();
    let session_id = SessionId::generate();
    let dir = std::env::temp_dir().join(format!(
        "jamstream-record-session-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

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
            recording: Some(RecordingOptions::Disk {
                dir: dir.clone(),
                stems: false,
            }),
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

    let pk = server_keys.public;
    let mut host = Client::join(
        &mint(&issuer, session_id, server_addr, pk, HOST_MEMBER_ID.0),
        now(),
    )
    .await;
    let mut guest = Client::join(&mint(&issuer, session_id, server_addr, pk, 1), now()).await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while *host.core.state() != ClientState::Joined || *guest.core.state() != ClientState::Joined {
        assert!(Instant::now() < deadline, "clients never joined");
        host.pump(now()).await;
        guest.pump(now()).await;
    }

    // The host presses record; the room is told, the host included, and
    // only by the server: no optimistic echo exists to fake this.
    host.core.record_ctl(RecordOp::Start).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
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
    let mut late = Client::join(&mint(&issuer, session_id, server_addr, pk, 2), now()).await;
    let deadline = Instant::now() + Duration::from_secs(5);
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
    let deadline = Instant::now() + Duration::from_secs(5);
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

    let _ = stop_tx.send(());
    let _ = server_task.await;

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
