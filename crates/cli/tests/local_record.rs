//! The local recording story, end to end through the real CLI code: a
//! `jamstream host --provider local --record` spawns a jamstreamd that can
//! record, the host output says where takes land, the host starts and
//! stops a take over the wire through the printed invite, and a finished
//! FLAC sits in exactly the directory that was printed, still there after
//! `jamstream end` has torn the session down.
//!
//! One test function: the state directory override is process-global env.

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{ServerGuard, budget, free_udp_port, jamstreamd_binary};
use jamstream_cli::cli::{EndArgs, HostArgs};
use jamstream_cli::{end, host, providers, state};
use jamstream_protocol::control::{RecordOp, RecordingState};
use jamstream_protocol::invite::Invite;
use jamstream_session::client::{ClientCore, ClientEvent, ClientState};
use jamstream_session::testing::pump;
use tokio::net::UdpSocket;

/// A real client on a real socket, kept only as far as this story needs:
/// join, press record, press stop, leave.
struct Wire {
    core: ClientCore,
    socket: UdpSocket,
    last_record_state: Option<RecordingState>,
}

impl Wire {
    async fn connect(invite: &Invite, now_ms: u64) -> Wire {
        let server: SocketAddr = invite.addresses[0];
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect(server).await.unwrap();
        let (core, first) = ClientCore::connect(invite, now_ms).unwrap();
        socket.send(&first).await.unwrap();
        Wire {
            core,
            socket,
            last_record_state: None,
        }
    }

    /// One pump pass, remembering the latest recording state the server
    /// reported.
    async fn pump(&mut self, now_ms: u64) {
        for event in pump(&self.socket, &mut self.core, now_ms).await {
            if let ClientEvent::RecordStatus { state, .. } = event {
                self.last_record_state = Some(state);
            }
        }
    }
}

/// Finished takes under `dir`: `.flac` files, never `.part` leftovers.
fn finished_takes(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "flac"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recorded_local_host_puts_the_take_where_it_said_it_would() {
    let state_dir = std::env::temp_dir().join(format!(
        "jamstream-cli-local-record-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let server_binary = jamstreamd_binary();
    // Safety: this test binary is single-test and sets every variable
    // before any state or provider access. JAMSTREAM_BIND confines the
    // spawned server to loopback, the one path the macOS Application
    // Firewall does not filter; without it a freshly built jamstreamd
    // raises a dialog and this test hangs until somebody answers it.
    unsafe {
        std::env::set_var(state::STATE_DIR_ENV, &state_dir);
        std::env::set_var("JAMSTREAMD_PATH", &server_binary);
        std::env::set_var("JAMSTREAM_BIND", "127.0.0.1");
    }

    let guard = ServerGuard::new();

    let args = HostArgs {
        provider: "local".to_owned(),
        region: None,
        // The host alone: pressing record needs nobody else in the room.
        musicians: 1,
        listeners: 0,
        hours: 1.0,
        destinations: 0,
        port: free_udp_port(),
        idle_min: 10,
        max_hours: 12,
        record: true,
        record_stems: false,
        bucket: None,
        retention: jamstream_cloud::Retention::Days30,
        artifact_url: None,
        artifact_sha256: None,
        yes: true,
        json: true,
    };
    // Through the real resolution seam, not a hand-built provider: this is
    // what proves `jamstream host --record` arms the server it spawns.
    let provider = providers::resolve_for_host(&args).unwrap();
    let mut out = Vec::new();
    let hosted = host::run(&args, provider.as_ref(), &mut out).await;
    let text = String::from_utf8(out).unwrap();
    // The pid is in the output whether or not host::run succeeded past it,
    // so the guard learns about the server before anything below can panic.
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(pid) = json["instance_id"].as_str()
    {
        guard.watch(pid);
    }
    hosted.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("host --json must emit exactly one JSON object ({e}): {text}"));

    // The output names the directory takes land in, and it is the
    // recordings directory beside the session state, not somewhere inside
    // the per-session server directory that `jamstream end` removes.
    let record_dir = PathBuf::from(json["record_dir"].as_str().unwrap_or_else(|| {
        panic!("host --record --json must print record_dir: {json}");
    }));
    assert_eq!(record_dir, state_dir.join("recordings"));
    assert!(
        record_dir.is_dir(),
        "the printed directory must exist the moment it is printed"
    );
    assert!(finished_takes(&record_dir).is_empty(), "no take yet");

    // The host joins through the printed invite and presses record; only
    // the server's own status report says it is on, no optimistic echo.
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let invite = Invite::decode(json["invites"][0]["invite"].as_str().unwrap()).unwrap();
    let mut wire = Wire::connect(&invite, now()).await;
    let deadline = Instant::now() + budget(Duration::from_secs(10));
    while *wire.core.state() != ClientState::Joined {
        assert!(Instant::now() < deadline, "the host never joined");
        wire.pump(now()).await;
    }
    wire.core.record_ctl(RecordOp::Start).unwrap();
    let deadline = Instant::now() + budget(Duration::from_secs(10));
    while wire.last_record_state != Some(RecordingState::Recording) {
        assert!(
            Instant::now() < deadline,
            "the server never reported the take started; saw {:?}",
            wire.last_record_state
        );
        wire.pump(now()).await;
    }

    // Let the recorder see some ticks, then stop the take.
    let until = Instant::now() + budget(Duration::from_millis(300));
    while Instant::now() < until {
        wire.pump(now()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    wire.core.record_ctl(RecordOp::Stop).unwrap();
    let deadline = Instant::now() + budget(Duration::from_secs(10));
    while wire.last_record_state != Some(RecordingState::Idle) {
        assert!(
            Instant::now() < deadline,
            "the server never reported the take ended; saw {:?}",
            wire.last_record_state
        );
        wire.pump(now()).await;
    }
    let _ = wire.core.leave("recording story done");
    wire.pump(now()).await;

    // The finished take is a .flac (never a .part) in the printed
    // directory, named as a mix. The rename happens on the recorder's own
    // task, so it is waited for rather than assumed.
    let deadline = Instant::now() + budget(Duration::from_secs(10));
    let takes = loop {
        let takes = finished_takes(&record_dir);
        if !takes.is_empty() {
            break takes;
        }
        assert!(
            Instant::now() < deadline,
            "no finished take appeared in {}",
            record_dir.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(takes.len(), 1, "one take was recorded: {takes:?}");
    let take = &takes[0];
    let name = take.file_name().unwrap().to_string_lossy();
    assert!(
        name.contains("mix"),
        "without --record-stems the one file is the mix: {name}"
    );

    // Ending the session removes the server and its directory, and the
    // take survives it: a recording outlives the session that made it.
    let session_id = json["session_id"].as_str().unwrap();
    let (path, session) = end::select(&EndArgs {
        session: Some(session_id[..8].to_owned()),
        last: false,
    })
    .unwrap();
    let end_provider = end::resolve_provider(&session).unwrap();
    let mut out = Vec::new();
    end::run(&path, session, end_provider.as_ref(), &mut out)
        .await
        .unwrap();
    assert!(String::from_utf8(out).unwrap().contains("ended"));
    guard.disarm();
    assert!(
        take.is_file(),
        "ending the session must never take the recording with it"
    );

    std::fs::remove_dir_all(&state_dir).unwrap();
}

/// The runner is described once, by the variable the harness already reads and
/// `crates/server/tests/common/mod.rs` already scales against, and a deadline
/// can only ever get longer from it. The two copies of this scaling are pinned
/// to the same numbers by this test and its twin in the server suite, so a
/// drift in either reference fails somewhere.
#[test]
fn a_deadline_scales_with_the_runner_and_never_shrinks() {
    assert_eq!(
        common::budget_scale(None),
        1.0,
        "unset is the laptop budget"
    );
    // What CI sets: 120 s against the harness's 30 s reference run.
    assert_eq!(common::budget_scale(Some("120")), 4.0);
    assert_eq!(common::budget_scale(Some("45")), 1.5);
    for nonsense in ["0", "-30", "", "soon", "NaN", "inf"] {
        assert_eq!(
            common::budget_scale(Some(nonsense)),
            1.0,
            "{nonsense:?} must not shorten a deadline"
        );
    }
    assert!(budget(Duration::from_secs(5)) >= Duration::from_secs(5));
}
