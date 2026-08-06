//! LiveRuntime end to end: a real jamstreamd server on loopback UDP and the
//! offline WAV backend, so the full stack (device bridge, network thread,
//! ClientCore, opus, encryption) runs with no sound card. Mirrors the
//! server's udp.rs and the CLI's headless_join.rs driving patterns.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jamstream_audio_io::{AudioError, DeviceRung, WavBackend};
use jamstream_client::live::{AudioSettings, LiveRuntime};
use jamstream_client::runtime::{
    Command, ConnState, MemberId, RateOutcomeView, RecordState, Runtime, Snapshot,
};
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
        self.invite_hinted(member, Some(name.to_owned()))
    }

    fn invite_hinted(&self, member: u16, name_hint: Option<String>) -> Invite {
        self.issuer.mint(
            self.session_id,
            vec![self.addr],
            self.server_pk,
            Token {
                member_id: MemberId(member),
                role: Role::Musician,
                name_hint,
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
        ..AudioSettings::default()
    }
}

fn temp_path(test: &str, name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jamstream-live-{}-{test}-{name}",
        std::process::id()
    ))
}

/// 30 s mono 16-bit sine at half scale, on the given device clock: long
/// enough to outlast any test even when a loaded machine stretches the
/// joins and waits.
fn sine_fixture(test: &str, hz: f32, rate: u32) -> PathBuf {
    let path = temp_path(test, &format!("sine-{hz}-{rate}.wav"));
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("fixture wav");
    for i in 0..(30 * rate) {
        let t = i as f32 / rate as f32;
        let s = (t * hz * std::f32::consts::TAU).sin() * 0.5;
        writer
            .write_sample((s * f32::from(i16::MAX)) as i16)
            .expect("fixture sample");
    }
    writer.finalize().expect("finalize fixture");
    path
}

/// A 32-bit float WAV with its own clock, as the offline backend writes
/// them: the capture file of a 44.1 kHz device runs at 44.1, so every
/// measurement reads the rate from the file instead of assuming RATE.
fn rate_and_samples(path: &Path) -> (u32, Vec<f32>) {
    let mut reader = hound::WavReader::open(path).expect("open capture wav");
    let rate = reader.spec().sample_rate;
    let samples = reader
        .samples::<f32>()
        .map(|s| s.expect("wav sample"))
        .collect();
    (rate, samples)
}

fn wav_samples(path: &Path) -> Vec<f32> {
    rate_and_samples(path).1
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

/// The final `secs` seconds of a stereo capture file, with the file's rate.
fn tail(path: &Path, secs: f64) -> (u32, Vec<f32>) {
    let (rate, samples) = rate_and_samples(path);
    let take = ((secs * f64::from(rate)) as usize * 2).min(samples.len());
    assert!(take > 0, "window is empty for {path:?}");
    (rate, samples[samples.len() - take..].to_vec())
}

/// RMS of the final `secs` seconds of a stereo capture file.
fn tail_rms(path: &Path, secs: f64) -> f64 {
    rms(&tail(path, secs).1)
}

/// Energy at one frequency, by Goertzel. Cheaper than a transform when the
/// question is about a handful of candidates rather than a whole spectrum.
fn tone_energy(samples: &[f32], rate: u32, hz: f64) -> f64 {
    let k = 2.0 * std::f64::consts::PI * hz / f64::from(rate);
    let coeff = 2.0 * k.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s0 = f64::from(x) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt()
}

/// Pitch of channel 0 over the final `secs` seconds of a stereo capture
/// file, measured on the file's own clock, as the strongest tone between
/// 300 and 700 Hz.
///
/// Not zero crossings. That counted every crossing the signal made, so a
/// dropout or a bit of ringing added cycles that were never in the tone,
/// and the estimate rose with load rather than with pitch: the nightly of
/// 2026-08-02 read a 440 Hz sine as 484 on a loaded runner while the same
/// commit passed unloaded. Energy at a frequency does not care how ragged
/// the waveform is around it, so a glitchy 440 still reads as 440, and the
/// dropouts it used to disguise are left to `longest_zero_run` and `rms`,
/// which is where a test can say what it actually found.
fn tail_pitch_hz(path: &Path, secs: f64) -> f64 {
    let (rate, samples) = tail(path, secs);
    let left: Vec<f32> = samples.iter().copied().step_by(2).collect();
    // 2 Hz steps: an order finer than the +-20 Hz the callers allow, and
    // far finer than the 8.8% a rate mismatch would move the tone.
    let mut best = (0.0f64, 0.0f64);
    let mut hz = 300.0;
    while hz <= 700.0 {
        let energy = tone_energy(&left, rate, hz);
        if energy > best.1 {
            best = (hz, energy);
        }
        hz += 2.0;
    }
    best.0
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

/// The frame loop asks for the connection state alone rather than pulling a
/// snapshot to read one field off it (#382), and it leaves the session on
/// what that answer says, so the two must never disagree: before the join
/// lands, once it has, and after a leave.
#[test]
fn the_state_accessor_agrees_with_the_snapshot() {
    let server = TestServer::start();
    let rt = LiveRuntime::join_offline(
        &server.invite(1, "solo"),
        settings(),
        WavBackend::new(None, None),
    )
    .expect("join offline");

    assert_eq!(rt.conn_state(), rt.snapshot().stats.state);
    wait_for(&rt, "joined", Duration::from_secs(10), joined);
    assert_eq!(rt.conn_state(), ConnState::Joined);
    assert_eq!(rt.conn_state(), rt.snapshot().stats.state);

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(3), |s| {
        s.stats.state == ConnState::Idle
    });
    assert_eq!(rt.conn_state(), ConnState::Idle);
    assert_eq!(rt.conn_state(), rt.snapshot().stats.state);
}

#[test]
fn two_runtimes_hear_each_other() {
    let server = TestServer::start();
    let sine = sine_fixture("hear", 440.0, RATE);
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
    let sine = sine_fixture("mute", 440.0, RATE);
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
    let sine = sine_fixture("reconf", 440.0, RATE);
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
        ..AudioSettings::default()
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
    let sine = sine_fixture("record", 440.0, RATE);

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

/// Longest run of consecutive exact-zero samples. Underrun padding writes
/// literal 0.0 into the device buffer; decoded audio of a running sine does
/// not produce long runs of exact zeros.
fn longest_zero_run(samples: &[f32]) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for &s in samples {
        run = if s == 0.0 { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    longest
}

/// WASAPI shared mode, end to end: the settings ask for 120-frame buffers,
/// both devices ignore that and call back at a 480-frame period, and the
/// audio still crosses intact. Before #323 the ring was sized from the
/// request alone, so every render callback padded roughly half its period
/// with silence and every capture callback dropped its tail; the padding
/// would show here as long runs of exact zeros in the steady-state tail of
/// B's capture file.
#[test]
fn a_device_period_larger_than_the_request_still_carries_audio() {
    let server = TestServer::start();
    let sine = sine_fixture("period", 440.0, RATE);
    let out_b = temp_path("period", "out-b.wav");

    let a = LiveRuntime::join_offline(
        &server.invite(1, "a"),
        settings(),
        WavBackend::new(Some(sine.clone()), None).with_device_period(480),
    )
    .expect("join a");
    let b = LiveRuntime::join_offline(
        &server.invite(2, "b"),
        settings(),
        WavBackend::new(None, Some(out_b.clone())).with_device_period(480),
    )
    .expect("join b");

    wait_for(&a, "a joined", Duration::from_secs(10), joined);
    wait_for(&b, "b sees both members", Duration::from_secs(10), |s| {
        joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
    });

    std::thread::sleep(Duration::from_millis(2_500));
    b.send(Command::Leave);
    wait_for(&b, "b idle", Duration::from_secs(3), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(b);
    drop(a);

    let samples = wav_samples(&out_b);
    let take = ((1.0 * f64::from(RATE)) as usize * 2).min(samples.len());
    let tail = &samples[samples.len() - take..];
    // A's sine arrived at all (capture side made it through the ring)...
    let energy = rms(tail);
    assert!(
        energy > 0.02,
        "b heard near-silence (rms {energy}); a's audio never arrived"
    );
    // ...and B's render never went hungry: an undersized ring pads ~480
    // zeros per callback, so anything close to that run length is padding,
    // not music.
    let run = longest_zero_run(tail);
    assert!(
        run < 240,
        "steady-state playout contains a {run}-sample silence run; the ring underran"
    );

    for p in [&sine, &out_b] {
        let _ = std::fs::remove_file(p);
    }
}

/// The #347 ladder's rung 3, end to end: both members sit on 44.1 kHz
/// interfaces while the wire and engine stay at 48 kHz. A captures a 440 Hz
/// sine from a 44.1 fixture, B renders to a 44.1 capture file, and the audio
/// crosses two boundary conversions plus the server, arriving at level, at
/// pitch, and without underrun padding.
///
/// This is also the test that holds the offline pump to the device's own
/// clock: paced at 48 000 frames per second, a 44.1 kHz uplink runs 8.8%
/// fast, which is far past the compensators' authority and shows up here as
/// concealment instead of music.
#[test]
fn a_44_1_interface_carries_the_session_both_ways() {
    let server = TestServer::start();
    let sine = sine_fixture("rung3", 440.0, 44_100);
    let out_b = temp_path("rung3", "out-b.wav");

    let a = LiveRuntime::join_offline(
        &server.invite(1, "a"),
        settings(),
        WavBackend::new(Some(sine.clone()), None).with_device_rate(44_100),
    )
    .expect("join a");
    let b = LiveRuntime::join_offline(
        &server.invite(2, "b"),
        settings(),
        WavBackend::new(None, Some(out_b.clone())).with_device_rate(44_100),
    )
    .expect("join b");

    wait_for(&a, "a joined", Duration::from_secs(10), joined);
    wait_for(&b, "b sees both members", Duration::from_secs(10), |s| {
        joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
    });

    std::thread::sleep(Duration::from_millis(2_500));
    b.send(Command::Leave);
    wait_for(&b, "b idle", Duration::from_secs(3), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(b);
    drop(a);

    let (rate, samples) = tail(&out_b, 1.0);
    assert_eq!(rate, 44_100, "b's device writes on its own clock");
    let energy = rms(&samples);
    assert!(
        energy > 0.02,
        "b heard near-silence (rms {energy}); a's audio never arrived"
    );
    let run = longest_zero_run(&samples);
    assert!(
        run < 240,
        "steady-state playout contains a {run}-sample silence run"
    );
    let hz = tail_pitch_hz(&out_b, 1.0);
    assert!(
        (hz - 440.0).abs() < 20.0,
        "the sine crossed two conversions off pitch: {hz:.1} Hz"
    );

    for p in [&sine, &out_b] {
        let _ = std::fs::remove_file(p);
    }
}

/// Two members on two different device clocks, which is what a real session
/// looks like the moment the two machines are not the same: the host runs a
/// 44.1 kHz interface and converts both directions, the joiner runs at the
/// session rate and converts nothing. Audio has to cross in both directions.
///
/// Every other rate test puts both members on the same clock, so a fault that
/// needed one converting peer beside one native peer had nowhere to show. This
/// is the shape #447 was reported in.
#[test]
fn a_44_1_host_and_a_native_joiner_hear_each_other() {
    const HOST_HZ: f64 = 440.0;
    const JOINER_HZ: f64 = 660.0;
    let server = TestServer::start();
    let host_sine = sine_fixture("mixed", HOST_HZ as f32, 44_100);
    let joiner_sine = sine_fixture("mixed", JOINER_HZ as f32, RATE);
    let out_host = temp_path("mixed", "out-host.wav");
    let out_joiner = temp_path("mixed", "out-joiner.wav");

    let host = LiveRuntime::join_offline(
        &server.invite(HOST_MEMBER_ID.0, "host"),
        settings(),
        WavBackend::new(Some(host_sine.clone()), Some(out_host.clone())).with_device_rate(44_100),
    )
    .expect("join host");
    let joiner = LiveRuntime::join_offline(
        &server.invite(1, "joiner"),
        settings(),
        WavBackend::new(Some(joiner_sine.clone()), Some(out_joiner.clone())),
    )
    .expect("join joiner");

    for (rt, who) in [(&host, "host"), (&joiner, "joiner")] {
        wait_for(rt, who, Duration::from_secs(10), |s| {
            joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
        });
    }

    // The output meter is the mixed playout as it leaves the session core,
    // before the device ever sees it, so this separates "the mix is silent"
    // from "the device path lost it". Both members must see theirs move.
    for (rt, who) in [(&host, "host"), (&joiner, "joiner")] {
        wait_for(
            rt,
            &format!("{who}'s mixed playout to carry audio"),
            Duration::from_secs(5),
            |s| s.levels.output_peak > 0.01,
        );
    }

    std::thread::sleep(Duration::from_millis(2_500));
    // Neither side may be reported quiet: a quiet member is the server saying
    // it has heard nothing from them, which is a different fault from a mix
    // that drops what did arrive.
    for (rt, who) in [(&host, "host"), (&joiner, "joiner")] {
        let snap = rt.snapshot();
        assert!(
            snap.members.iter().all(|m| !m.quiet),
            "{who} sees a quiet member: {:?}",
            snap.members
        );
    }

    for (rt, who) in [(&joiner, "joiner"), (&host, "host")] {
        rt.send(Command::Leave);
        wait_for(rt, who, Duration::from_secs(3), |s| {
            s.stats.state == ConnState::Idle
        });
    }
    drop(joiner);
    drop(host);

    // Each side's playout must carry the other's tone and not its own: the
    // personal mix excludes self, so the wrong tone would mean a mix fault
    // rather than a transport one.
    for (path, who, mine, theirs, device_rate) in [
        (&out_host, "host", HOST_HZ, JOINER_HZ, 44_100),
        (&out_joiner, "joiner", JOINER_HZ, HOST_HZ, RATE),
    ] {
        let (rate, samples) = tail(path, 1.0);
        assert_eq!(rate, device_rate, "{who} writes on its own device clock");
        let energy = rms(&samples);
        assert!(
            energy > 0.02,
            "{who} heard near-silence (rms {energy}); the other member's audio never arrived"
        );
        let left: Vec<f32> = samples.iter().copied().step_by(2).collect();
        let wanted = tone_energy(&left, rate, theirs);
        let leaked = tone_energy(&left, rate, mine);
        assert!(
            wanted > leaked * 4.0,
            "{who} played {mine} Hz at {leaked:.1} against {theirs} Hz at {wanted:.1}: \
             that is its own signal, not the other member's"
        );
        let run = longest_zero_run(&samples);
        assert!(
            run < 240,
            "{who}'s steady-state playout contains a {run}-sample silence run"
        );
    }

    for p in [&host_sine, &joiner_sine, &out_host, &out_joiner] {
        let _ = std::fs::remove_file(p);
    }
}

/// The disclosure that rides with rung 3 (#347): a member on a 44.1 kHz
/// interface sees the Resampled outcome in the snapshot with the converter's
/// own added milliseconds, one chat line per converted direction at join,
/// and a mouth-to-ear figure that grew by exactly the disclosed amount.
#[test]
fn a_converting_stream_discloses_itself_and_prices_the_latency() {
    let server = TestServer::start();
    let rt = LiveRuntime::join_offline(
        &server.invite(1, "solo"),
        settings(),
        WavBackend::new(None, None).with_device_rate(44_100),
    )
    .expect("join offline");
    let snap = wait_for(&rt, "joined with stats", Duration::from_secs(10), |s| {
        joined(s) && s.stats.mouth_to_ear_ms.is_some()
    });

    let rate = snap.stats.rate.expect("a running stream reports its rungs");
    let (capture_ms, playback_ms) = match (rate.capture, rate.playback) {
        (
            RateOutcomeView::Resampled {
                device: 44_100,
                added_ms: capture_ms,
            },
            RateOutcomeView::Resampled {
                device: 44_100,
                added_ms: playback_ms,
            },
        ) => (capture_ms, playback_ms),
        other => panic!("both directions must convert at 44.1: {other:?}"),
    };
    assert!(capture_ms > 0.0 && playback_ms > 0.0);

    // Said once per direction, at join, in the state chat carries.
    for side in ["capture", "playback"] {
        let line = format!("converting {side} 44.1 kHz to 48 kHz");
        assert_eq!(
            snap.chat.iter().filter(|l| l.text.contains(&line)).count(),
            1,
            "{side} notice: {:?}",
            snap.chat
        );
    }

    // Mouth to ear grew by the disclosed amount: strip the link terms this
    // same snapshot reports and the capture-buffer term, and what is left is
    // the converter's own figure. The buffer term is the negotiated callback
    // in session-rate frames: the 120-frame request on a 44.1 kHz device is
    // ceil(120 * 160/147) = 131.
    let m2e = snap.stats.mouth_to_ear_ms.expect("the predicate held");
    let rtt = snap.stats.rtt_ms.expect("joined sessions measure rtt");
    let link_ms = rtt / 2.0 + snap.stats.jitter_depth as f32 * 2.5 + 2.5 + 131.0 / 48.0;
    let disclosed = capture_ms + playback_ms;
    assert!(
        (m2e - link_ms - disclosed).abs() < 0.01,
        "mouth to ear {m2e} ms must carry the disclosed {disclosed} ms over {link_ms} ms of link"
    );

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
}

/// A device lost mid-session: the stream is closed, the room is told the
/// stream stopped, and the runtime reopens with the same settings without
/// losing the session.
///
/// This is the test the offline backend existed for and could not run.
/// `Driver::errored` answered a flat `false` for the offline arm, so
/// `WavStream::errored` was never read and `Worker::check_stream`'s device-gone
/// branch was dead code under test: the only backend a test can drive could not
/// report a lost device, and the only one that could needs a hand on a cable.
/// Both halves of the fix are in the same change, which is the point:
/// `with_device_loss_after` on the backend, and reading it here.
///
/// The wording is part of the contract. A latched stream error carries no
/// class, and the exclusive Windows path latches on any read or write hiccup,
/// so this line may not claim a disconnection it cannot know about (#327).
#[test]
fn a_device_lost_mid_session_is_announced_and_reopened() {
    let server = TestServer::start();
    let sine = sine_fixture("device-loss", 440.0, RATE);
    // Two hundred frames is half a second of pumping at 2.5 ms, so the loss
    // lands well after the join and well inside the test's own patience.
    let backend = WavBackend::new(Some(sine.clone()), None).with_device_loss_after(200);
    let rt = LiveRuntime::join_offline(&server.invite(1, "solo"), settings(), backend)
        .expect("join offline");
    wait_for(&rt, "joined", Duration::from_secs(10), joined);

    // The notice goes to everyone on this client's own chat, which is where the
    // app puts a device problem: nothing else on screen would say why the
    // meters went quiet.
    let snap = wait_for(&rt, "the device notice", Duration::from_secs(10), |s| {
        s.chat
            .iter()
            .any(|l| l.text.contains("the audio stream stopped; retrying"))
    });
    assert!(
        !snap.chat.iter().any(|l| l.text.contains("disconnected")),
        "a classless stream error must not be reported as an unplug: {:?}",
        snap.chat
    );

    // And it comes back. The modelled unplug is spent, so the replacement
    // stream keeps running and the session was never dropped.
    let snap = wait_for(&rt, "the reopen", Duration::from_secs(10), |s| {
        s.chat
            .iter()
            .any(|l| l.text.contains("audio device reopened"))
    });
    assert_eq!(
        snap.stats.state,
        ConnState::Joined,
        "losing a device must not drop the session"
    );
    // Promptly, which is the property the backoff must not cost: a device
    // that was fine until it was pulled is retried on the next tick, not
    // after the wait a flapping device earns.
    let at = |text: &str| {
        snap.chat
            .iter()
            .find(|l| l.text.contains(text))
            .unwrap_or_else(|| panic!("{text} is in chat: {:?}", snap.chat))
            .at_ms
    };
    let gap = at("audio device reopened") - at("the audio stream stopped; retrying");
    assert!(gap < 100, "the reopen waited {gap} ms after the loss");

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(rt);
    let _ = std::fs::remove_file(&sine);
}

/// The device that will not stay open, which is where the reopen loop used
/// to come apart. It opens every time and latches before the next 2.5 ms
/// tick, so the loop got a fresh success on every attempt and never consulted
/// its own interval: a close and an open every tick, two chat lines a tick
/// (neither deduped, because they alternate), the 500-line scrollback emptied
/// of the band's conversation in about a second, and `Worker::step` running
/// far past the tick, so the rings were not serviced for the whole episode.
/// A WASAPI exclusive endpoint another process takes, a half-present USB
/// interface, and a PipeWire graph refusing the rate all arrive in this
/// shape.
///
/// What must hold instead: a handful of attempts over a widening backoff, a
/// bounded number of chat lines, a plain sentence when the loop gives up that
/// agrees with how many times it really tried, and a session that is still
/// joined at the end of it, which is the proof the worker kept servicing the
/// loop rather than drowning in device opens.
#[test]
fn a_device_that_will_not_stay_open_is_retried_a_few_times_and_then_left_alone() {
    let server = TestServer::start();
    // Dead on arrival on every open, reopens included: the client polls
    // errored before it ever pumps, so no frame count would model this.
    let backend = WavBackend::new(None, None).losing_device_every(0);
    let device = backend.clone();
    let rt = LiveRuntime::join_offline(&server.invite(1, "solo"), settings(), backend)
        .expect("join offline");
    wait_for(&rt, "joined", Duration::from_secs(10), joined);

    // Two and a half seconds in: single figures, against the ~1000 opens a
    // 2.5 ms tick would have made by now.
    std::thread::sleep(Duration::from_millis(2_500));
    let early = device.opens();
    assert!(
        early <= 6,
        "{early} device opens in 2.5 s; the backoff is not holding"
    );
    let snap = rt.snapshot();
    assert!(
        snap.chat.len() <= 3,
        "the retry loop is flooding chat: {:?}",
        snap.chat
    );

    // And it stops, saying so in a sentence a musician can act on.
    let snap = wait_for(&rt, "the loop to give up", Duration::from_secs(30), |s| {
        s.chat.iter().any(|l| l.text.contains("did not stay open"))
    });
    let given_up = snap
        .chat
        .iter()
        .find(|l| l.text.contains("did not stay open"))
        .expect("the predicate above matched");
    assert!(
        given_up.text.contains("pick a device"),
        "the give-up line must say what to do: {:?}",
        given_up.text
    );
    // The claim in that line and the number of opens the fake counted are the
    // same story: one open for the join, then the tries it says it made.
    let tries: u32 = given_up
        .text
        .split_whitespace()
        .find_map(|w| w.parse().ok())
        .expect("the line names how many tries it made");
    assert_eq!(
        device.opens(),
        tries + 1,
        "the line claims {tries} tries: {:?}",
        given_up.text
    );

    // Three lines for the whole episode: stopped, reopened, gave up.
    assert_eq!(snap.chat.len(), 3, "{:?}", snap.chat);
    for text in [
        "the audio stream stopped; retrying",
        "audio device reopened",
        "did not stay open",
    ] {
        assert_eq!(
            snap.chat.iter().filter(|l| l.text.contains(text)).count(),
            1,
            "{text}: {:?}",
            snap.chat
        );
    }

    // Nothing more is tried, and the session is still up: the network side
    // kept its tick the whole time the device was failing.
    std::thread::sleep(Duration::from_millis(1_500));
    assert_eq!(device.opens(), tries + 1, "the loop restarted itself");
    let snap = rt.snapshot();
    assert_eq!(snap.stats.state, ConnState::Joined);
    assert!(snap.stats.rtt_ms.is_some(), "pings kept flowing");
    assert_eq!(snap.chat.len(), 3, "{:?}", snap.chat);

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
}

/// User story: a musician swaps interfaces mid-song, the new one runs at
/// 44.1 kHz, and the music keeps playing through the boundary converter
/// (#347 rung 3) where it used to be refused. What arrives on the swapped-in
/// interface must still be the room's audio: at level, at pitch, and free of
/// underrun padding; and the swap is disclosed, in chat once and in the
/// snapshot's rate outcome for as long as the converter runs.
#[test]
fn a_44_1_interface_swapped_in_mid_song_keeps_the_music_playing() {
    let server = TestServer::start();
    let sine = sine_fixture("swap-44-1", 440.0, RATE);
    let out_b = temp_path("swap-44-1", "out-b.wav");

    let a = LiveRuntime::join_offline(
        &server.invite(1, "a"),
        settings(),
        WavBackend::new(Some(sine.clone()), None),
    )
    .expect("join a");
    // B's first device is unplugged moments after the join; the interface
    // that answers the reopen is clocked at 44.1 kHz.
    let b = LiveRuntime::join_offline(
        &server.invite(2, "b"),
        settings(),
        WavBackend::new(None, Some(out_b.clone()))
            .with_device_loss_after(200)
            .reopening_at(44_100),
    )
    .expect("join b");

    wait_for(&a, "a joined", Duration::from_secs(10), joined);
    wait_for(&b, "b sees both members", Duration::from_secs(10), |s| {
        joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
    });
    wait_for(&b, "the reopen", Duration::from_secs(10), |s| {
        s.chat
            .iter()
            .any(|l| l.text.contains("audio device reopened"))
    });

    // Two seconds of the room through the swapped-in interface. The reopen
    // recreated the capture file, so everything in it is post-swap audio.
    std::thread::sleep(Duration::from_millis(2_000));
    let snap = b.snapshot();
    assert_eq!(
        snap.stats.state,
        ConnState::Joined,
        "the swap must not drop the session"
    );
    assert!(
        snap.device_error.is_none(),
        "nothing was refused: {:?}",
        snap.device_error
    );
    // The swap is disclosed: the snapshot carries the converting outcome and
    // chat was told once, not once per reopen-cadence tick.
    let rate = snap
        .stats
        .rate
        .expect("the swapped-in stream reports rungs");
    assert!(
        matches!(
            rate.playback,
            RateOutcomeView::Resampled { device: 44_100, .. }
        ),
        "the swapped-in interface converts: {rate:?}"
    );
    assert_eq!(
        snap.chat
            .iter()
            .filter(|l| l.text.contains("converting playback 44.1 kHz to 48 kHz"))
            .count(),
        1,
        "one disclosure line for the swap: {:?}",
        snap.chat
    );
    b.send(Command::Leave);
    wait_for(&b, "b idle", Duration::from_secs(3), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(b);
    drop(a);

    let (rate, samples) = tail(&out_b, 1.0);
    assert_eq!(
        rate, 44_100,
        "the swapped-in device writes on its own clock"
    );
    let energy = rms(&samples);
    assert!(
        energy > 0.02,
        "b heard near-silence after the swap (rms {energy})"
    );
    let run = longest_zero_run(&samples);
    assert!(
        run < 240,
        "post-swap playout contains a {run}-sample silence run"
    );
    let hz = tail_pitch_hz(&out_b, 1.0);
    assert!(
        (hz - 440.0).abs() < 20.0,
        "a's sine arrived off pitch: {hz:.1} Hz"
    );

    for p in [&sine, &out_b] {
        let _ = std::fs::remove_file(p);
    }
}

/// The refusal that outlives rung 3, and the surface that carries it: the
/// interface a musician swaps to will not open at all (the #242 final
/// fallback). The reason must land in the snapshot for the UI, not only in a
/// log, and it must arrive in the device's own words, because those words are
/// the only thing that tells them what to do about it.
///
/// The refusal is modelled as a refusal. This test used to reach one by
/// handing the fake a 48 kHz fixture and reopening at 44.1, so what it
/// actually asserted was WavBackend's input-file check, a string no real
/// backend can produce, and after #367 a 44.1 kHz interface is exactly the
/// case that succeeds now.
#[test]
fn a_device_the_reopen_cannot_open_says_so_in_the_snapshot() {
    const REFUSAL: &str = "capture device runs at 16000 Hz and will not open at 48000 Hz; \
                           that is a Bluetooth or headset microphone with no 48000 Hz mode, \
                           so use another capture device";
    let server = TestServer::start();
    let backend = WavBackend::new(None, None)
        .with_device_loss_after(200)
        .refusing_reopen(AudioError::Unsupported(REFUSAL.to_owned()));
    let rt = LiveRuntime::join_offline(&server.invite(1, "solo"), settings(), backend)
        .expect("join offline");
    wait_for(&rt, "joined", Duration::from_secs(10), joined);

    let snap = wait_for(&rt, "the refusal", Duration::from_secs(10), |s| {
        s.device_error.is_some()
    });
    let reason = snap.device_error.expect("the predicate above matched");
    assert_eq!(
        reason,
        format!("unsupported audio configuration: {REFUSAL}"),
        "the device's own sentence must reach the UI whole"
    );
    assert_eq!(
        snap.stats.state,
        ConnState::Joined,
        "a refused device must not drop the session"
    );
    // A refusal is not an unplug and no fallback happened, so neither may be
    // claimed: both were, before #327.
    assert!(
        !snap
            .chat
            .iter()
            .any(|l| l.text.contains("disconnected") || l.text.contains("system default")),
        "a refusal must not read as an unplug or a fallback: {:?}",
        snap.chat
    );
    let snap = wait_for(&rt, "the chat notice", Duration::from_secs(10), |s| {
        s.chat
            .iter()
            .any(|l| l.text.contains("audio device refused"))
    });
    assert_eq!(
        snap.chat
            .iter()
            .filter(|l| l.text == format!("audio device refused: {REFUSAL}"))
            .count(),
        1,
        "said once, in the device's words: {:?}",
        snap.chat
    );

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
}

/// The other class, and the only one this client may call a disconnection: a
/// reopen that finds no device at all. Every other latched error goes out as
/// a refusal in the device's words, because the exclusive Windows path
/// latches on any read or write hiccup and an unplug is not knowable from
/// that (#327). Nothing covered this arm before, so the two could have been
/// swapped and only the negative half of the refusal tests would have
/// noticed.
#[test]
fn a_reopen_that_finds_nothing_is_the_one_case_called_a_disconnection() {
    let server = TestServer::start();
    let backend = WavBackend::new(None, None)
        .with_device_loss_after(200)
        .refusing_reopen(AudioError::DeviceGone);
    let rt = LiveRuntime::join_offline(&server.invite(1, "solo"), settings(), backend)
        .expect("join offline");
    wait_for(&rt, "joined", Duration::from_secs(10), joined);

    let snap = wait_for(&rt, "the disconnection", Duration::from_secs(10), |s| {
        s.chat
            .iter()
            .any(|l| l.text == "audio device disconnected; retrying")
    });
    assert_eq!(
        snap.device_error.as_deref(),
        Some("audio device is gone or was never present")
    );
    assert!(
        !snap.chat.iter().any(|l| l.text.contains("refused")),
        "a device that is gone is not a device that refused: {:?}",
        snap.chat
    );
    // Said once however many times the cadence retries it.
    std::thread::sleep(Duration::from_millis(1_500));
    let snap = rt.snapshot();
    assert_eq!(
        snap.chat
            .iter()
            .filter(|l| l.text.contains("disconnected"))
            .count(),
        1,
        "{:?}",
        snap.chat
    );

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
}

/// The asymmetric pair, end to end: a 44.1 kHz microphone beside 48 kHz
/// monitors, which is the ordinary case the moment the two endpoints are
/// different hardware. Capture converts and playback does not, so exactly one
/// direction is disclosed and exactly that direction's milliseconds reach
/// mouth to ear. The fake had one rate for the whole stream until now, so
/// every disclosure test ran on a pair that matched and this shape was
/// unreachable from any surface.
#[test]
fn a_stream_that_converts_one_direction_discloses_only_that_direction() {
    let server = TestServer::start();
    let backend = WavBackend::new(None, None)
        .with_direction_rungs(DeviceRung::Converted { device: 44_100 }, DeviceRung::Native);
    let rt = LiveRuntime::join_offline(&server.invite(1, "solo"), settings(), backend)
        .expect("join offline");
    let snap = wait_for(&rt, "joined with stats", Duration::from_secs(10), |s| {
        joined(s) && s.stats.mouth_to_ear_ms.is_some()
    });

    let rate = snap.stats.rate.expect("a running stream reports its rungs");
    let capture_ms = match rate.capture {
        RateOutcomeView::Resampled {
            device: 44_100,
            added_ms,
        } => added_ms,
        other => panic!("capture converts at 44.1: {other:?}"),
    };
    assert_eq!(
        rate.playback,
        RateOutcomeView::Native,
        "the monitors are already at the session rate"
    );

    // One line, for the direction that earned it.
    let notices: Vec<&str> = snap
        .chat
        .iter()
        .filter(|l| l.text.contains("converting"))
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].starts_with("converting capture 44.1 kHz to 48 kHz"));

    // And one direction's milliseconds. The buffer term is the negotiated
    // callback in session-rate frames: the 120-frame request against a
    // 44.1 kHz capture endpoint is ceil(120 * 160/147) = 131.
    let m2e = snap.stats.mouth_to_ear_ms.expect("the predicate held");
    let rtt = snap.stats.rtt_ms.expect("joined sessions measure rtt");
    let link_ms = rtt / 2.0 + snap.stats.jitter_depth as f32 * 2.5 + 2.5 + 131.0 / 48.0;
    assert!(
        (m2e - link_ms - capture_ms).abs() < 0.01,
        "mouth to ear {m2e} ms must carry the converted direction's \
         {capture_ms} ms over {link_ms} ms of link"
    );

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
}

/// The two rungs no test could reach, because the fake could only be native
/// or converting: a host that moved the capture device's clock to the session
/// rate, beside one that is carrying playback over its own. Neither costs the
/// converter's latency, and the copy is not interchangeable, so what a
/// musician reads about their machine depends on this pair surviving the whole
/// way from the backend to chat.
#[test]
fn a_moved_clock_and_an_os_converter_read_as_themselves() {
    let server = TestServer::start();
    let backend = WavBackend::new(None, None).with_direction_rungs(
        DeviceRung::ClockSet { from: 44_100 },
        DeviceRung::OsConverted { device: 44_100 },
    );
    let rt = LiveRuntime::join_offline(&server.invite(1, "solo"), settings(), backend)
        .expect("join offline");
    let snap = wait_for(&rt, "joined with stats", Duration::from_secs(10), |s| {
        joined(s) && s.stats.mouth_to_ear_ms.is_some()
    });

    let rate = snap.stats.rate.expect("a running stream reports its rungs");
    assert_eq!(rate.capture, RateOutcomeView::ClockSet { from: 44_100 });
    assert_eq!(
        rate.playback,
        RateOutcomeView::OsConverted { device: 44_100 }
    );
    assert_eq!(rate.added_ms(), 0.0, "neither rung runs a converter");

    // The moved clock is announced, once: it is the one rung with a
    // consequence outside this app, since every other program on that device
    // is now hearing 48 kHz. The OS converter is hover-only.
    assert_eq!(
        snap.chat
            .iter()
            .filter(|l| l.text == "moved the capture device to 48 kHz (was 44.1)")
            .count(),
        1,
        "{:?}",
        snap.chat
    );
    assert!(
        !snap.chat.iter().any(|l| l.text.contains("the OS is")),
        "the OS converter belongs on the hover, not in the room: {:?}",
        snap.chat
    );

    // No converter, so mouth to ear carries no converter term, and the
    // buffer term is the plain 120-frame request.
    let m2e = snap.stats.mouth_to_ear_ms.expect("the predicate held");
    let rtt = snap.stats.rtt_ms.expect("joined sessions measure rtt");
    let link_ms = rtt / 2.0 + snap.stats.jitter_depth as f32 * 2.5 + 2.5 + 120.0 / 48.0;
    assert!((m2e - link_ms).abs() < 0.01, "mouth to ear {m2e} ms");

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
}

/// The client half of #357 against the real server: a member whose invite
/// carried no name joins as the member-N fallback, says their own name
/// through [`Command::SetOwnName`], and every roster in the session carries
/// it, their bandmate's included. No fake in the middle: the name rides the
/// real control link into the real jamstreamd roster and back out.
#[test]
fn a_name_set_after_join_reaches_every_roster() {
    let server = TestServer::start();
    let unnamed = LiveRuntime::join_offline(
        &server.invite_hinted(1, None),
        settings(),
        WavBackend::new(None, None),
    )
    .expect("join unnamed");
    let witness = join_silent(&server, 2, "cass");
    wait_for(&unnamed, "joined", Duration::from_secs(10), joined);
    let snap = wait_for(&witness, "both on roster", Duration::from_secs(10), |s| {
        joined(s) && s.members.len() == 2
    });
    assert!(
        snap.members.iter().any(|m| m.name == "member 1"),
        "an unnamed invite starts as the fallback: {:?}",
        snap.members
    );

    unnamed.send(Command::SetOwnName("Ana".to_owned()));
    let snap = wait_for(&witness, "the rename", Duration::from_secs(10), |s| {
        s.members.iter().any(|m| m.name == "Ana")
    });
    assert!(
        !snap.members.iter().any(|m| m.name == "member 1"),
        "the fallback must be gone from the bandmate's roster: {:?}",
        snap.members
    );
    // And from your own: the strip under your fader is you by name.
    wait_for(&unnamed, "own roster", Duration::from_secs(10), |s| {
        s.members.iter().any(|m| m.is_you && m.name == "Ana")
    });

    for rt in [&unnamed, &witness] {
        rt.send(Command::Leave);
    }
    drop(unnamed);
    drop(witness);
}

/// The repro on #327, made honest: a mid-session device pick is refused. The
/// runtime keeps the selection it was handed and retries exactly it, the
/// refusal lands in chat once in the device's own words, and nothing claims
/// the fallback the old code silently made. Before this, `applied_audio` and
/// the pickers said the new device while the system default ran, for the rest
/// of the session.
#[test]
fn a_refused_reconfigure_keeps_the_selection_and_says_why() {
    const REFUSAL: &str = "playback device runs at 44100 Hz and will not open at 48000 Hz \
                           (ASBD not supported); check Audio MIDI Setup, Format, for a \
                           48000 Hz entry on that device";
    let server = TestServer::start();
    // The join succeeds on the device they started on; the pick they make
    // mid-session is what refuses.
    let backend =
        WavBackend::new(None, None).refusing_reopen(AudioError::Unsupported(REFUSAL.to_owned()));
    let rt = LiveRuntime::join_offline(&server.invite(1, "solo"), settings(), backend)
        .expect("join offline");
    wait_for(&rt, "joined", Duration::from_secs(10), joined);

    rt.reconfigure_audio(AudioSettings {
        capture_id: Some("BlackHole 2ch".to_owned()),
        playback_id: None,
        buffer_frames: 120,
        ..AudioSettings::default()
    });

    let snap = wait_for(&rt, "the refusal", Duration::from_secs(10), |s| {
        s.device_error.is_some()
    });
    let reason = snap.device_error.expect("the predicate above matched");
    assert_eq!(
        reason,
        format!("unsupported audio configuration: {REFUSAL}"),
        "the refusal must carry the device's own words"
    );
    let snap = wait_for(&rt, "the chat notice", Duration::from_secs(10), |s| {
        s.chat
            .iter()
            .any(|l| l.text == format!("audio device refused: {REFUSAL}"))
    });
    assert!(
        !snap
            .chat
            .iter()
            .any(|l| l.text.contains("disconnected") || l.text.contains("system default")),
        "a refused pick must not be reported as an unplug or a fallback: {:?}",
        snap.chat
    );

    // The cadence keeps retrying the same refused device; each distinct
    // reason is said once, not once per attempt.
    std::thread::sleep(Duration::from_millis(1_500));
    let snap = rt.snapshot();
    assert_eq!(
        snap.chat
            .iter()
            .filter(|l| l.text.contains("audio device refused"))
            .count(),
        1,
        "the retry cadence must not flood chat: {:?}",
        snap.chat
    );
    assert_eq!(
        snap.stats.state,
        ConnState::Joined,
        "a refused reconfigure must not drop the session"
    );

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
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

/// The log file's first line promises that an empty file is a healthy run, and
/// #451 is that the promise was false in the case that mattered: a member heard
/// nobody for a whole session and the file said nothing. The playout watch that
/// fixes it can break the promise the other way, by writing warnings on
/// ordinary sessions until nobody reads the file, so this holds a real session
/// to the promise: two members, audio crossing both ways, through the app's own
/// subscriber and a real log file.
///
/// The file rather than a captured formatter, because the promise is about the
/// file. One subscriber per process, which is what nextest gives every test;
/// under `cargo test` the whole binary shares one, so the install would land in
/// another test's file and this would read an empty one.
#[test]
fn a_healthy_session_leaves_the_log_holding_only_its_banner() {
    if std::env::var_os("RUST_LOG").is_some() {
        eprintln!("skipping: RUST_LOG replaces the default filter this is about");
        return;
    }
    // Its own directory: the log goes through the private-file machinery, which
    // refuses to write key material next to a world-writable temp root.
    let dir = temp_path("quiet", "logs");
    let _ = std::fs::remove_dir_all(&dir);
    let log = dir.join("app.log");
    let installed = jamstream_client::logging::init_at(log.clone()).expect("install the log");
    assert_eq!(installed, log);

    let server = TestServer::start();
    let a_sine = sine_fixture("quiet", 440.0, RATE);
    let b_sine = sine_fixture("quiet", 660.0, RATE);
    let out_a = temp_path("quiet", "out-a.wav");
    let out_b = temp_path("quiet", "out-b.wav");
    let a = LiveRuntime::join_offline(
        &server.invite(1, "a"),
        settings(),
        WavBackend::new(Some(a_sine.clone()), Some(out_a.clone())),
    )
    .expect("join a");
    let b = LiveRuntime::join_offline(
        &server.invite(2, "b"),
        settings(),
        WavBackend::new(Some(b_sine.clone()), Some(out_b.clone())),
    )
    .expect("join b");
    for (rt, who) in [(&a, "a"), (&b, "b")] {
        wait_for(rt, who, Duration::from_secs(10), |s| {
            joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
        });
    }

    // Several seconds of playing, which is many times the second of silence the
    // watch waits out and the second its refusal window measures.
    std::thread::sleep(Duration::from_millis(4_000));
    for (rt, who) in [(&a, "a"), (&b, "b")] {
        rt.send(Command::Leave);
        wait_for(rt, who, Duration::from_secs(5), |s| {
            s.stats.state == ConnState::Idle
        });
    }
    drop(b);
    drop(a);

    // A session that carried no audio would be quiet in the log for the wrong
    // reason, so each side has to have heard the other's tone. The personal mix
    // excludes self, so the tone measured is the one that crossed the wire.
    for (out, theirs, mine) in [(&out_a, 660.0, 440.0), (&out_b, 440.0, 660.0)] {
        let (rate, samples) = tail(out, 1.0);
        let left: Vec<f32> = samples.iter().copied().step_by(2).collect();
        let heard = tone_energy(&left, rate, theirs);
        let own = tone_energy(&left, rate, mine);
        assert!(
            heard > own * 4.0,
            "{out:?} heard {heard} at {theirs} Hz against {own} at its own {mine} Hz"
        );
    }

    let text = std::fs::read_to_string(&log).expect("read the log");
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines
            .first()
            .is_some_and(|l| l.contains("empty after this line is a healthy run")),
        "the banner is missing, so this proves nothing: {text:?}"
    );
    assert_eq!(lines.len(), 1, "a healthy session wrote {:#?}", &lines[1..]);

    // And the file has to prove it could have carried one, or a subscriber that
    // never installed reads as a healthy run and the assertion above is empty.
    tracing::warn!("a line this test wrote itself");
    let text = std::fs::read_to_string(&log).expect("reread the log");
    assert!(
        text.contains("a line this test wrote itself"),
        "warnings from this process never reach the file: {text:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    for p in [&a_sine, &b_sine, &out_a, &out_b] {
        let _ = std::fs::remove_file(p);
    }
}
