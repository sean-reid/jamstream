//! LiveRuntime end to end: a real jamstreamd server on loopback UDP and the
//! offline WAV backend, so the full stack (device bridge, network thread,
//! ClientCore, opus, encryption) runs with no sound card. Mirrors the
//! server's udp.rs and the CLI's headless_join.rs driving patterns.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jamstream_audio_io::WavBackend;
use jamstream_client::live::{AudioSettings, LiveRuntime};
use jamstream_client::runtime::{Command, ConnState, MemberId, RecordState, Runtime, Snapshot};
use jamstream_protocol::ids::{HOST_MEMBER_ID, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, RecordingOptions, Server};

const RATE: u32 = 48_000;

/// A real server on loopback, owned by a private tokio runtime so the tests
/// themselves stay synchronous like the app.
struct TestServer {
    rt: tokio::runtime::Runtime,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    addr: SocketAddr,
    issuer: Issuer,
    server_pk: [u8; 32],
    session_id: SessionId,
}

impl TestServer {
    /// A session that was never armed to record, which is every session the
    /// app's own launch wizard makes.
    fn start() -> Self {
        TestServer::with_recording(None)
    }

    /// A session armed to record to a directory, as `jamstream host
    /// --record` arms the server it spawns.
    fn recording_to(dir: &Path) -> Self {
        std::fs::create_dir_all(dir).expect("record dir");
        TestServer::with_recording(Some(RecordingOptions::Disk {
            dir: dir.to_path_buf(),
            stems: false,
        }))
    }

    fn with_recording(recording: Option<RecordingOptions>) -> Self {
        let issuer = Issuer::generate();
        let keys = generate_keypair();
        let session_id = SessionId::generate();
        let cfg = Config {
            session_id,
            port: 0,
            server_private_key: keys.private.to_vec(),
            issuer_public_key: issuer.public_key().to_bytes(),
            idle_shutdown_min: 10,
            max_duration_min: 720,
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (server, addr) = rt.block_on(async {
            let server = Server::bind(
                &cfg,
                Options {
                    bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                    activity_path: None,
                    recording,
                },
            )
            .await
            .expect("bind server");
            let addr = server.local_addr().expect("server addr");
            (server, addr)
        });
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = rt.spawn(server.run(async {
            let _ = stop_rx.await;
        }));
        TestServer {
            rt,
            stop: Some(stop_tx),
            task: Some(task),
            addr,
            issuer,
            server_pk: keys.public,
            session_id,
        }
    }

    fn invite(&self, member: u16, name: &str) -> Invite {
        self.issuer.mint(
            self.session_id,
            vec![self.addr],
            self.server_pk,
            Token {
                member_id: MemberId(member),
                role: Role::Musician,
                name_hint: Some(name.to_owned()),
                expires_unix: u64::MAX,
                jti: TokenId::generate(),
            },
        )
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = self.rt.block_on(task);
        }
    }
}

fn settings() -> AudioSettings {
    AudioSettings {
        capture_id: None,
        playback_id: None,
        buffer_frames: 120,
    }
}

fn temp_path(test: &str, name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jamstream-live-{}-{test}-{name}",
        std::process::id()
    ))
}

/// 30 s mono 16-bit sine at half scale: long enough to outlast any test
/// even when a loaded machine stretches the joins and waits.
fn sine_fixture(test: &str, hz: f32) -> PathBuf {
    let path = temp_path(test, &format!("sine-{hz}.wav"));
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("fixture wav");
    for i in 0..(30 * RATE) {
        let t = i as f32 / RATE as f32;
        let s = (t * hz * std::f32::consts::TAU).sin() * 0.5;
        writer
            .write_sample((s * f32::from(i16::MAX)) as i16)
            .expect("fixture sample");
    }
    writer.finalize().expect("finalize fixture");
    path
}

/// Samples of a 32-bit float WAV, as the offline backend writes them.
fn wav_samples(path: &Path) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("open capture wav");
    reader
        .samples::<f32>()
        .map(|s| s.expect("wav sample"))
        .collect()
}

fn rms(samples: &[f32]) -> f64 {
    assert!(!samples.is_empty(), "no samples to measure");
    (samples
        .iter()
        .map(|&s| f64::from(s) * f64::from(s))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt()
}

/// RMS of the final `secs` seconds of a stereo capture file.
fn tail_rms(path: &Path, secs: f64) -> f64 {
    let samples = wav_samples(path);
    let take = ((secs * f64::from(RATE)) as usize * 2).min(samples.len());
    assert!(take > 0, "window is empty for {path:?}");
    rms(&samples[samples.len() - take..])
}

/// Polls snapshots until `pred` holds; panics with the last state on timeout.
fn wait_for(
    rt: &LiveRuntime,
    what: &str,
    timeout: Duration,
    mut pred: impl FnMut(&Snapshot) -> bool,
) -> Snapshot {
    let deadline = Instant::now() + timeout;
    loop {
        let snap = rt.snapshot();
        if pred(&snap) {
            return snap;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; state {:?}, {} members, {} chat lines",
            snap.stats.state,
            snap.members.len(),
            snap.chat.len()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn joined(snap: &Snapshot) -> bool {
    snap.stats.state == ConnState::Joined
}

fn join_silent(server: &TestServer, member: u16, name: &str) -> LiveRuntime {
    let invite = server.invite(member, name);
    LiveRuntime::join_offline(&invite, settings(), WavBackend::new(None, None))
        .expect("join offline")
}

#[test]
fn join_reaches_joined_and_stats_populate() {
    let server = TestServer::start();
    let invite = server.invite(1, "solo");
    let rt = LiveRuntime::join_offline(&invite, settings(), WavBackend::new(None, None))
        .expect("join offline");

    let snap = wait_for(&rt, "joined", Duration::from_secs(10), joined);
    assert_eq!(snap.server_addr, server.addr.to_string());
    let expected_short: String = invite.session_id.0[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(snap.session_short, expected_short);
    assert!(!snap.is_host, "member 1 is not the host");

    let snap = wait_for(&rt, "roster with self", Duration::from_secs(3), |s| {
        s.members
            .iter()
            .any(|m| m.name == "solo" && m.is_you && m.connected)
    });
    assert_eq!(snap.members.len(), 1);

    // Pings run once a second; rtt must land well inside three.
    let snap = wait_for(&rt, "rtt sample", Duration::from_secs(3), |s| {
        s.stats.rtt_ms.is_some()
    });
    assert!(snap.stats.mouth_to_ear_ms.is_some());
}

#[test]
fn two_runtimes_hear_each_other() {
    let server = TestServer::start();
    let sine = sine_fixture("hear", 440.0);
    let out_b = temp_path("hear", "out-b.wav");

    let a = LiveRuntime::join_offline(
        &server.invite(1, "a"),
        settings(),
        WavBackend::new(Some(sine.clone()), None),
    )
    .expect("join a");
    let b = LiveRuntime::join_offline(
        &server.invite(2, "b"),
        settings(),
        WavBackend::new(None, Some(out_b.clone())),
    )
    .expect("join b");

    wait_for(&a, "a joined", Duration::from_secs(10), joined);
    wait_for(&b, "b sees both members", Duration::from_secs(10), |s| {
        joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
    });

    // Let audio flow through the whole stack, then close B so its capture
    // file finalizes.
    std::thread::sleep(Duration::from_millis(2_500));
    b.send(Command::Leave);
    wait_for(&b, "b idle", Duration::from_secs(3), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(b);
    drop(a);

    // The personal mix excludes self, so energy in B's playout proves A's
    // sine crossed the server.
    let energy = tail_rms(&out_b, 1.0);
    assert!(
        energy > 0.02,
        "b heard near-silence (rms {energy}); a's audio never arrived"
    );

    for p in [&sine, &out_b] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn chat_crosses_between_runtimes() {
    let server = TestServer::start();
    let a = join_silent(&server, 1, "a");
    let b = join_silent(&server, 2, "b");

    wait_for(&a, "a sees both members", Duration::from_secs(10), |s| {
        joined(s) && s.members.len() == 2
    });
    wait_for(&b, "b sees both members", Duration::from_secs(10), |s| {
        joined(s) && s.members.len() == 2
    });

    a.send(Command::SendChat("hello from a".to_owned()));
    let snap = wait_for(&b, "chat arrival", Duration::from_secs(3), |s| {
        s.chat.iter().any(|l| l.text == "hello from a")
    });
    let line = snap
        .chat
        .iter()
        .find(|l| l.text == "hello from a")
        .expect("chat line");
    assert_eq!(line.from_id, MemberId(1));
    assert_eq!(line.from_name, "a");
}

#[test]
fn fader_mute_silences_the_member() {
    let server = TestServer::start();
    let sine = sine_fixture("mute", 440.0);
    let out_a = temp_path("mute", "out-a.wav");

    let a = LiveRuntime::join_offline(
        &server.invite(1, "a"),
        settings(),
        WavBackend::new(None, Some(out_a.clone())),
    )
    .expect("join a");
    let b = LiveRuntime::join_offline(
        &server.invite(2, "b"),
        settings(),
        WavBackend::new(Some(sine.clone()), None),
    )
    .expect("join b");

    wait_for(&b, "b joined", Duration::from_secs(10), joined);
    wait_for(&a, "a sees both members", Duration::from_secs(10), |s| {
        joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
    });

    // Two seconds of B audible, then mute B in A's monitor mix and give
    // the server a moment to apply it plus two seconds of silence.
    std::thread::sleep(Duration::from_millis(2_000));
    a.send(Command::SetFader {
        member: MemberId(2),
        gain_db: 0.0,
        pan: 0.0,
        muted: true,
    });
    std::thread::sleep(Duration::from_millis(2_000));
    a.send(Command::Leave);
    wait_for(&a, "a idle", Duration::from_secs(3), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(a);
    drop(b);

    let samples = wav_samples(&out_a);
    // The pre-mute window: one second ending 2.5 s before the file's end,
    // safely inside the audible span.
    let end = samples.len().saturating_sub((RATE as usize * 2) * 5 / 2);
    let start = end.saturating_sub(RATE as usize * 2);
    let audible = rms(&samples[start..end]);
    let muted = tail_rms(&out_a, 1.0);
    assert!(
        audible > 0.02,
        "a never heard b before the mute (rms {audible})"
    );
    assert!(
        muted < audible / 10.0,
        "mute did not silence b: before {audible}, after {muted}"
    );

    for p in [&sine, &out_a] {
        let _ = std::fs::remove_file(p);
    }
}

/// An avatar the whole way: raw file bytes into one runtime's
/// `SetOwnAvatar`, hashed and announced by the session core, requested and
/// chunked by the server, decoded once on the far side, and attached to the
/// matching member in the snapshot the UI paints. Also the other half of the
/// contract: a decode failure never reaches a snapshot.
#[test]
fn an_avatar_crosses_between_runtimes_and_decodes_once() {
    let server = TestServer::start();
    let a = join_silent(&server, 1, "a");
    let b = join_silent(&server, 2, "b");

    for (rt, who) in [(&a, "a"), (&b, "b")] {
        wait_for(rt, who, Duration::from_secs(10), |s| {
            joined(s) && s.members.len() == 2
        });
    }
    // Nobody has a picture yet: the roster carries no hash, so the UI gets
    // None and draws initials.
    assert!(b.snapshot().members.iter().all(|m| m.avatar.is_none()));

    // Two chunks' worth, so the train is more than one message.
    let png = wide_png(120, 60);
    assert!(
        png.len() > 8 * 1024,
        "one chunk would not exercise the train"
    );
    a.send(Command::SetOwnAvatar(Some(png)));

    let snap = wait_for(&b, "a's avatar", Duration::from_secs(10), |s| {
        s.members
            .iter()
            .any(|m| m.id == MemberId(1) && m.avatar.is_some())
    });
    let member = snap
        .members
        .iter()
        .find(|m| m.id == MemberId(1))
        .expect("a on b's roster");
    let handle = member.avatar.as_ref().expect("decoded avatar");
    assert_eq!((handle.width, handle.height), (120, 60));
    assert_eq!(handle.rgba.len(), 120 * 60 * 4);
    // Content-addressed: the hash is the Blake2s hex the transfer used.
    assert_eq!(handle.hash.len(), 64);
    assert!(handle.hash.chars().all(|c| c.is_ascii_hexdigit()));
    // Nobody else grew a picture.
    assert!(
        snap.members
            .iter()
            .filter(|m| m.id != MemberId(1))
            .all(|m| m.avatar.is_none())
    );
    // Two frames of the same snapshot share the buffer: decoded once, and
    // the UI's texture cache keys off that one hash.
    let again = b.snapshot();
    let second = again
        .members
        .iter()
        .find(|m| m.id == MemberId(1))
        .and_then(|m| m.avatar.clone())
        .expect("still there");
    assert!(
        std::sync::Arc::ptr_eq(&handle.rgba, &second.rgba),
        "each snapshot must hand out the same decode, not a new one"
    );

    // Bytes that are not an image travel exactly the same way and are
    // dropped at the decoder: the member keeps their initials disc.
    let garbage = vec![0x42u8; 9_000];
    b.send(Command::SetOwnAvatar(Some(garbage)));
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        assert!(
            a.snapshot()
                .members
                .iter()
                .filter(|m| m.id == MemberId(2))
                .all(|m| m.avatar.is_none()),
            "undecodable bytes must never reach a snapshot"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A landscape PNG large enough to need several 8 KB chunks: noise, so the
/// encoder cannot compress it down to one.
fn wide_png(w: u32, h: u32) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let img = image::RgbaImage::from_fn(w, h, |x, y| {
        let n = x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503);
        image::Rgba([(n >> 3) as u8, (n >> 11) as u8, (n >> 19) as u8, 255])
    });
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .expect("encode png");
    buf.into_inner()
}

#[test]
fn leave_tears_down_and_shrinks_the_roster() {
    let server = TestServer::start();
    let a = join_silent(&server, 1, "a");
    let b = join_silent(&server, 2, "b");

    wait_for(&b, "b sees both members", Duration::from_secs(10), |s| {
        joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
    });

    a.send(Command::Leave);
    wait_for(&a, "a idle after leave", Duration::from_secs(3), |s| {
        s.stats.state == ConnState::Idle
    });

    // The network thread owns the socket and stream; its exit is the
    // teardown proof.
    let deadline = Instant::now() + Duration::from_secs(3);
    while !a.finished() {
        assert!(Instant::now() < deadline, "network thread never exited");
        std::thread::sleep(Duration::from_millis(25));
    }

    // The server saw the Bye: B's roster drops to one connected member.
    wait_for(&b, "roster shrink", Duration::from_secs(5), |s| {
        s.members.iter().filter(|m| m.connected).count() == 1
    });
}

#[test]
fn reconfigure_audio_swaps_the_stream_mid_session() {
    let server = TestServer::start();
    let sine = sine_fixture("reconf", 440.0);
    let out_b = temp_path("reconf", "out-b.wav");

    let a = LiveRuntime::join_offline(
        &server.invite(1, "a"),
        settings(),
        WavBackend::new(Some(sine.clone()), None),
    )
    .expect("join a");
    let b = LiveRuntime::join_offline(
        &server.invite(2, "b"),
        settings(),
        WavBackend::new(None, Some(out_b.clone())),
    )
    .expect("join b");

    wait_for(&a, "a joined", Duration::from_secs(10), joined);
    wait_for(&b, "b sees both members", Duration::from_secs(10), |s| {
        joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
    });
    std::thread::sleep(Duration::from_millis(1_500));

    // Swap to a bigger buffer mid-session. The offline backend recreates
    // its capture file on reopen, so everything in it afterwards is
    // post-swap audio.
    b.reconfigure_audio(AudioSettings {
        capture_id: None,
        playback_id: None,
        buffer_frames: 240,
    });
    std::thread::sleep(Duration::from_millis(2_000));

    let snap = b.snapshot();
    assert_eq!(
        snap.stats.state,
        ConnState::Joined,
        "reconfigure dropped the session"
    );

    b.send(Command::Leave);
    wait_for(&b, "b idle", Duration::from_secs(3), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(b);
    drop(a);

    let samples = wav_samples(&out_b);
    assert!(
        samples.len() > RATE as usize * 2,
        "post-swap capture too short: {} samples",
        samples.len()
    );
    let energy = tail_rms(&out_b, 1.0);
    assert!(
        energy > 0.02,
        "audio did not resume after the swap (rms {energy})"
    );

    for p in [&sine, &out_b] {
        let _ = std::fs::remove_file(p);
    }
}

/// Finished takes under `dir`: `.flac` files, never `.part` leftovers.
fn finished_takes(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "flac"))
        .collect()
}

/// User story: the host presses Record in the app, the lamp goes red for
/// everyone, and the room's music is in a file afterwards.
///
/// This is the only test that crosses from the command the record sheet's
/// button sends to a take on disk. `interactions.rs` proves the button
/// emits `StartRecord`, but into a fake runtime that does nothing except
/// collect commands, and the server's own suite proves `record_ctl`
/// records. Between those two halves sat the seam of #164: recording no
/// surface could reach, while both halves stayed green. `LiveRuntime`'s
/// dispatch has an arm that ignores `StartRecord` a screen above the arm
/// that routes it, so dropping the routing is a plausible edit that
/// nothing else in the suite would notice.
#[test]
fn the_host_pressing_record_lands_a_take_and_lights_the_lamp() {
    let takes = temp_path("record", "takes");
    let _ = std::fs::remove_dir_all(&takes);
    let server = TestServer::recording_to(&takes);
    let sine = sine_fixture("record", 440.0);

    // Member 0 is the host seat, and the server refuses record control
    // from anybody else, so it is the only seat this could pass from.
    let host = LiveRuntime::join_offline(
        &server.invite(HOST_MEMBER_ID.0, "host"),
        settings(),
        WavBackend::new(Some(sine.clone()), None),
    )
    .expect("join host");
    let snap = wait_for(&host, "the host joined", Duration::from_secs(10), joined);
    assert!(snap.is_host, "member 0 holds the host seat");
    assert_eq!(snap.record.state, RecordState::Idle, "nothing recorded yet");
    assert!(
        finished_takes(&takes).is_empty(),
        "no take before the press"
    );

    host.send(Command::StartRecord);
    // The lamp the record sheet paints, which only turns red on the
    // server's own status report; there is no optimistic echo to mistake
    // for the recorder having started.
    wait_for(&host, "the record lamp", Duration::from_secs(10), |s| {
        s.record.state == RecordState::Recording
    });

    // Half a second of the sine through the mix tick, then end the take.
    std::thread::sleep(Duration::from_millis(500));
    host.send(Command::StopRecord);
    wait_for(&host, "the take to end", Duration::from_secs(10), |s| {
        s.record.state == RecordState::Idle
    });
    host.send(Command::Leave);
    wait_for(&host, "the host idle", Duration::from_secs(3), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(host);

    // The rename off `.part` happens on the recorder's own thread, so it
    // is waited for rather than assumed.
    let deadline = Instant::now() + Duration::from_secs(10);
    let take = loop {
        let found = finished_takes(&takes);
        if let [take] = found.as_slice() {
            break take.clone();
        }
        assert!(
            Instant::now() < deadline,
            "the take never landed in {}: {found:?}",
            takes.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let name = take.file_name().expect("take name").to_string_lossy();
    assert!(name.ends_with("-mix.flac"), "named as a mix: {name}");
    let bytes = std::fs::read(&take).expect("read the take");
    assert_eq!(&bytes[..4], b"fLaC", "the take is a flac stream");
    // The host's sine, not an empty container and not silence: FLAC codes
    // half a second of silence as a handful of constant blocks, a couple of
    // hundred bytes all in, while half a second of 440 Hz stereo runs to
    // tens of kilobytes. The floor sits an order of magnitude above the
    // first and an order below the second.
    assert!(
        bytes.len() > 2_048,
        "the take holds no audio: {} bytes",
        bytes.len()
    );

    let _ = std::fs::remove_file(&sine);
    let _ = std::fs::remove_dir_all(&takes);
}

/// The other half of the same contract: a session nobody armed to record
/// must say so in the lamp rather than swallow the press. That is what a
/// host who launched from the app sees today, because the app's own launch
/// wizard hardcodes `recording: None` (#164), so it is the path most
/// presses of that button currently take.
#[test]
fn record_on_an_unarmed_session_fails_visibly_in_the_lamp() {
    let server = TestServer::start();
    let host = join_silent(&server, HOST_MEMBER_ID.0, "host");
    wait_for(&host, "the host joined", Duration::from_secs(10), joined);

    host.send(Command::StartRecord);
    let snap = wait_for(&host, "the refusal", Duration::from_secs(10), |s| {
        matches!(s.record.state, RecordState::Failed { .. })
    });
    let RecordState::Failed { reason } = snap.record.state else {
        unreachable!("the predicate above matched Failed")
    };
    assert!(
        reason.contains("not configured"),
        "the lamp must say why, verbatim from the server: {reason:?}"
    );

    host.send(Command::Leave);
    drop(host);
}
