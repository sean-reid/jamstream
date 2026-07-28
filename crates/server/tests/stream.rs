//! The broadcast pipeline as jamstreamd actually drives it: real UDP, real
//! encryption, a real host and a real listener, and a real supervisor whose
//! ffmpeg is a stand-in script.
//!
//! What this covers that the unit tests cannot: that a host's StreamCtl leaves
//! the client, survives the control plane, reaches the pipeline, and that the
//! resulting per-destination status comes back to a member who is *not* the
//! host, with no stream key anywhere in it.

mod common;

use std::net::SocketAddr;

use common::{Running, Session, loopback};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use jamstream_protocol::control::{
    DestinationState, DestinationStatus, StreamKey, StreamOp, StreamPlatform,
};
use jamstream_protocol::ids::{DestinationId, Role, TokenId};
use jamstream_protocol::invite::Invite;
use jamstream_server::runtime::{Options, Server};
use jamstream_session::client::{ClientCore, ClientEvent};
use jamstream_stream::pipeline::StreamConfig;
use tokio::net::UdpSocket;

const KEY: &str = "live_777_never_relay_me";

/// A stand-in ffmpeg: drains every FIFO named in argv and then its stdin, so
/// the encoder's spawn and pipe handshake happen for real without needing a
/// codec. Pushers exec it too and exit at once, which is what puts a
/// destination into Failed with a reason.
fn fake_ffmpeg(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake-ffmpeg");
    std::fs::write(
        &path,
        "#!/bin/sh\nfor a in \"$@\"; do\n  if [ -p \"$a\" ]; then cat \"$a\" > /dev/null & fi\ndone\ncat > /dev/null\n",
    )
    .expect("write fake ffmpeg");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    path
}

struct Peer {
    socket: UdpSocket,
    core: ClientCore,
    statuses: Vec<Vec<DestinationStatus>>,
}

impl Peer {
    async fn connect(invite: &Invite, server_addr: SocketAddr, now_ms: u64) -> Peer {
        let socket = UdpSocket::bind(loopback()).await.unwrap();
        socket.connect(server_addr).await.unwrap();
        let (core, first) = ClientCore::connect(invite, now_ms).unwrap();
        socket.send(&first).await.unwrap();
        Peer {
            socket,
            core,
            statuses: Vec::new(),
        }
    }

    async fn pump(&mut self, now_ms: u64) {
        for pkt in self.core.poll(now_ms) {
            let _ = self.socket.send(&pkt).await;
        }
        // Bounded: a musician's downlink is 400 packets a second, so an
        // unbounded drain never returns and the caller never gets to look at
        // its events.
        let mut buf = [0u8; 2048];
        for _ in 0..64 {
            let Ok(Ok(len)) =
                tokio::time::timeout(Duration::from_millis(5), self.socket.recv(&mut buf)).await
            else {
                break;
            };
            for pkt in self.core.handle_datagram(now_ms, &buf[..len]) {
                let _ = self.socket.send(&pkt).await;
            }
        }
        for event in self.core.events() {
            if let ClientEvent::StreamStatus(d) = event {
                self.statuses.push(d);
            }
        }
    }

    fn last_status(&self) -> Option<&Vec<DestinationStatus>> {
        self.statuses.last()
    }
}

// Multi-threaded on purpose: the 2.5 ms tick loop and the test's own pumping
// must not share one executor thread, or the server starves the client side
// exactly as it never would in deployment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosts_stream_request_reaches_the_pipeline_and_status_reaches_everyone() {
    if !cfg!(unix) {
        eprintln!(
            "SKIP a_hosts_stream_request_reaches_the_pipeline_and_status_reaches_everyone: \
             the pipeline needs named pipes and a POSIX shell. jamstreamd runs on Linux."
        );
        return;
    }
    let root = std::env::temp_dir().join(format!("jamstream-serverstream-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let session = Session::new();

    let mut stream_cfg = StreamConfig::new("Integration Jam");
    stream_cfg.ffmpeg = fake_ffmpeg(&root);
    stream_cfg.work_dir = root.clone();
    stream_cfg.key_dir = root.join("keys");
    // No relay in this test: the stand-in ignores its output argument.
    stream_cfg.encoder_output = root.join("out.flv").to_string_lossy().into_owned();
    // Small frames: this test is about wiring, not pixels.
    stream_cfg.width = 320;
    stream_cfg.height = 180;

    let server = Server::bind(
        &session.cfg,
        Options {
            bind: loopback(),
            activity_path: None,
            recording: None,
        },
    )
    .await
    .unwrap()
    .with_stream_config(stream_cfg);
    let server = Running::of(server);
    let server_addr = server.addr;

    let mint = |member: u16, role: Role| {
        session.invite(
            member,
            role,
            TokenId::generate(),
            Some(format!("member{member}")),
            server_addr,
        )
    };

    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    // Member 0 is the host by definition; member 5 is an ordinary listener.
    let mut host = Peer::connect(&mint(0, Role::Musician), server_addr, now()).await;
    let mut listener = Peer::connect(&mint(5, Role::Listener), server_addr, now()).await;

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut asked = false;
    while Instant::now() < deadline {
        host.pump(now()).await;
        listener.pump(now()).await;
        if !asked && matches!(host.core.state(), jamstream_session::ClientState::Joined) {
            host.core
                .stream_ctl(StreamOp::AddDestination {
                    id: DestinationId(1),
                    platform: StreamPlatform::Twitch,
                    key: StreamKey::new(KEY),
                })
                .unwrap();
            host.core.stream_ctl(StreamOp::Start).unwrap();
            asked = true;
        }
        // Both members must see the destination, and the pusher must have run
        // and failed (the stand-in exits at once), which is the reason path.
        let done = |p: &Peer| {
            p.last_status()
                .is_some_and(|s| s.len() == 1 && s[0].state != DestinationState::Idle)
        };
        if asked && done(&host) && done(&listener) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(asked, "host never joined");
    for (who, peer) in [("host", &host), ("listener", &listener)] {
        let status = peer
            .last_status()
            .unwrap_or_else(|| panic!("{who} never saw a stream status"));
        assert_eq!(status.len(), 1, "{who}: {status:?}");
        assert_eq!(status[0].id, DestinationId(1));
        assert_eq!(status[0].platform, StreamPlatform::Twitch);
        assert_eq!(status[0].bitrate_kbps, 2_628);
        // The stand-in pusher exits immediately, so this is the failure path
        // with a reason, which is exactly what a bad key would look like.
        match &status[0].state {
            DestinationState::Connecting => {}
            DestinationState::Failed { reason } => assert!(!reason.is_empty()),
            other => panic!("{who}: unexpected state {other:?}"),
        }
        // Nothing that reached a member mentions the key.
        let bytes = postcard::to_allocvec(status).unwrap();
        let needle = KEY.as_bytes();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle),
            "{who} received the stream key"
        );
        assert!(!format!("{status:?}").contains(KEY));
    }

    // The staged key file did not outlive the spawn.
    if let Ok(mut entries) = std::fs::read_dir(root.join("keys")) {
        assert!(entries.next().is_none(), "a key file is still on disk");
    }

    server.stop().await.unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
