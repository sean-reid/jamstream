//! LiveRuntime end to end: a real jamstreamd server on loopback UDP and the
//! offline WAV backend, so the full stack (device bridge, network thread,
//! ClientCore, opus, encryption) runs with no sound card. Mirrors the
//! server's udp.rs and the CLI's headless_join.rs driving patterns.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use jamstream_audio_io::{AudioError, DeviceRung, WavBackend};
use jamstream_client::live::{AudioSettings, LiveRuntime};
use jamstream_client::runtime::{
    AudioFaultView, Command, ConnState, MemberId, RateOutcomeView, RecordState, Runtime, Snapshot,
};
use jamstream_engine::JitterBuffer;
use jamstream_protocol::control::MAX_DATAGRAM_BYTES;
use jamstream_protocol::ids::{HOST_MEMBER_ID, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, RecordingOptions, Server};

const RATE: u32 = 48_000;
/// Separates "carried no audio" from "carried a badly separated tone", which the
/// ratio below cannot: two noise floors have a ratio too. `tone_energy` is
/// unnormalised, so a second of one fixture tone reads in the thousands. Ten
/// measurements on a healthy pair: 822, 3593, 6071, 6707, 7272, 7396, 7893,
/// 7915, 8003, 8467. Two Windows runs that failed the ratio read 0.7 and 3.0.
/// This sits an order of magnitude under the worst healthy reading and two above
/// the loudest failure, so it answers only the question it is asked.
const TONE_FLOOR: f64 = 100.0;

/// Held for as long as any test in this binary has a session running, shared by
/// all of them but one, and taken exclusively by the test whose subject is the
/// log file.
///
/// That log's subscriber is process wide, and under `cargo test` the whole
/// binary is one process running tests on parallel threads, so a test asserting
/// its own log holds nothing reads what the tests beside it wrote and fails on
/// their warnings. Every session in this file starts from a `TestServer`, which
/// is what makes the shared side automatic: a new test cannot reach a runtime
/// without passing through it.
static SESSIONS: RwLock<()> = RwLock::new(());

/// Which side of [`SESSIONS`] a server holds. Never read, only dropped: how
/// long it lives is the whole of it.
enum Bystanders {
    Beside {
        _shared: RwLockReadGuard<'static, ()>,
    },
    None {
        _exclusive: RwLockWriteGuard<'static, ()>,
    },
}

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
    _bystanders: Bystanders,
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

    /// Waits until no other test in this process has a session running, and
    /// keeps it that way until the server is dropped. For a test that reads a
    /// process-wide surface and has to know that only its own session wrote to
    /// it. Take it before touching that surface, not after.
    ///
    /// A poisoned lock is taken anyway: one test failing must not turn into
    /// every later test panicking here instead of reporting what it found.
    fn alone_in_the_process() -> Self {
        TestServer::build(
            None,
            Bystanders::None {
                _exclusive: SESSIONS.write().unwrap_or_else(PoisonError::into_inner),
            },
        )
    }

    fn with_recording(recording: Option<RecordingOptions>) -> Self {
        TestServer::build(
            recording,
            Bystanders::Beside {
                _shared: SESSIONS.read().unwrap_or_else(PoisonError::into_inner),
            },
        )
    }

    fn build(recording: Option<RecordingOptions>, bystanders: Bystanders) -> Self {
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
            _bystanders: bystanders,
        }
    }

    fn invite(&self, member: u16, name: &str) -> Invite {
        self.invite_hinted(member, Some(name.to_owned()))
    }

    fn invite_hinted(&self, member: u16, name_hint: Option<String>) -> Invite {
        self.invite_to(self.addr, member, name_hint)
    }

    /// An invite that dials somewhere other than the server's own port, for a
    /// test that puts something in the path.
    fn invite_via(&self, addr: SocketAddr, member: u16, name: &str) -> Invite {
        self.invite_to(addr, member, Some(name.to_owned()))
    }

    fn invite_to(&self, addr: SocketAddr, member: u16, name_hint: Option<String>) -> Invite {
        self.issuer.mint(
            self.session_id,
            vec![addr],
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

/// What the device buffers cost mouth to ear for a stream whose negotiated
/// callback is `frames` session-rate frames: the capture buffer, plus the
/// playout cushion the top-up loop holds at two callbacks. Everything else in
/// the figure is the link, priced by [`link_ms`] from the same snapshot.
fn device_buffers_ms(frames: f32) -> f32 {
    frames / 48.0 + 2.0 * frames / 48.0
}

/// What the link costs mouth to ear, from the figures the snapshot reports
/// beside it: the round trip charged for both network legs, both jitter buffers
/// at the depth each is holding, and one media frame of encode latency.
fn link_ms(snap: &Snapshot) -> f32 {
    let rtt = snap.stats.rtt_ms.expect("joined sessions measure rtt");
    let buffered = snap.stats.jitter_depth + snap.stats.uplink_jitter_depth.unwrap_or(0);
    rtt + buffered as f32 * 2.5 + 2.5
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

/// Channel 0 of an interleaved stereo buffer.
fn left(stereo: &[f32]) -> Vec<f32> {
    stereo.iter().copied().step_by(2).collect()
}

/// The final `secs` seconds of a stereo capture file, with the file's rate.
///
/// Only for a span that is meant to be silent, where the padding [`loudest`]
/// describes reads the same as the thing under test. Everything asserting that
/// audio was there wants [`loudest`] instead.
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

/// The loudest `secs` window anywhere in a stereo capture file, with the
/// file's rate: where a measurement of the session goes.
///
/// A capture file spans from before the join to after the leave, so both ends
/// hold silence the backend wrote while nothing played, and on a slow host that
/// padding is longer than the window: measured 1.75 s at the front and 0.75 s
/// at the back on a loaded Windows runner. Candidates step by a quarter window
/// so one can land clear of the padding rather than straddling it, which a
/// zero-run reading would not survive.
fn loudest(path: &Path, secs: f64) -> (u32, Vec<f32>) {
    let (rate, samples) = rate_and_samples(path);
    assert!(samples.len() >= 2, "no samples to measure in {path:?}");
    (rate, loudest_of(&samples, rate, secs))
}

/// The loudest `secs` window of samples a caller already holds, for a
/// measurement that has to start somewhere other than the top of the file.
fn loudest_of(samples: &[f32], rate: u32, secs: f64) -> Vec<f32> {
    let frames = samples.len() / 2;
    let win = ((secs * f64::from(rate)) as usize).max(1);
    if frames <= win {
        return samples.to_vec();
    }
    let start = (0..=frames - win)
        .step_by((win / 4).max(1))
        .map(|f| (f, rms(&samples[f * 2..(f + win) * 2])))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .expect("at least one window")
        .0;
    samples[start * 2..(start + win) * 2].to_vec()
}

/// Samples from the first one that is not silent, and how many were skipped.
///
/// A device that has just opened writes its first period before any pull has
/// reached it, so a capture begins with one period of zeros that is the stream
/// starting rather than a gap in it. Callers bound what they skip, because a
/// long opening silence is a fault and must not be trimmed away.
fn after_opening_silence(samples: &[f32]) -> (&[f32], usize) {
    let skipped = samples.iter().take_while(|v| **v == 0.0).count();
    (&samples[skipped..], skipped)
}

/// RMS of the loudest `secs` window anywhere in a capture file.
fn loudest_rms(path: &Path, secs: f64) -> f64 {
    rms(&loudest(path, secs).1)
}

/// Energy at one frequency, by Goertzel. Cheaper than a transform when the
/// question is about a handful of candidates rather than a whole spectrum.
fn tone_energy(samples: &[f32], rate: u32, hz: f64) -> f64 {
    // Summed over blocks, not measured in one pass. A Goertzel is coherent, so
    // a phase discontinuity inside the window cancels a tone that is plainly
    // audible: one inversion halfway through a second takes 12000 to 0, and
    // concealed or repeated frames put such steps in real playout. Block
    // magnitudes add instead, which cost a quarter of a block per step. A
    // quarter second resolves 4 Hz, finer than the 20 Hz the pitch callers
    // allow.
    let block = (rate as usize / 4).max(1);
    samples
        .chunks(block)
        .map(|c| coherent_energy(c, rate, hz))
        .sum()
}

/// The whole file's `tone_energy` blocks at `hz`, with its duration: what a
/// tail reading of nothing looks like across the run it came from.
///
/// A tail is one window of a paced file, and two very different runs read the
/// same in it. A session that never carried audio reads nothing everywhere; a
/// session the machine took the cpu away from reads healthy and then stops,
/// because the offline pump replays the debt faster than media can arrive and
/// the pulls it cannot fill are written to the capture file as zeros. Only the
/// profile separates them, so the tail assertions print it when they fire.
fn tone_profile(path: &Path, hz: f64) -> String {
    let (rate, samples) = rate_and_samples(path);
    let mono = left(&samples);
    let profile: Vec<u64> = mono
        .chunks((rate as usize / 4).max(1))
        .map(|c| coherent_energy(c, rate, hz) as u64)
        .collect();
    format!(
        "{:.2} s at {rate} Hz reads {profile:?} per 250 ms at {hz} Hz",
        mono.len() as f64 / f64::from(rate)
    )
}

/// One Goertzel pass. Only meaningful over a span the tone holds phase across.
fn coherent_energy(samples: &[f32], rate: u32, hz: f64) -> f64 {
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

/// Pitch of channel 0 of one stereo window, measured on the file's own clock,
/// as the strongest tone between 300 and 700 Hz. Give it a window that carries
/// audio: an argmax over silence returns a plausible frequency rather than
/// failing, so a window of padding reads as a wrong answer.
///
/// Not zero crossings. That counted every crossing the signal made, so a
/// dropout or a bit of ringing added cycles that were never in the tone,
/// and the estimate rose with load rather than with pitch: the nightly of
/// 2026-08-02 read a 440 Hz sine as 484 on a loaded runner while the same
/// commit passed unloaded. Energy at a frequency does not care how ragged
/// the waveform is around it, so a glitchy 440 still reads as 440, and the
/// dropouts it used to disguise are left to `longest_zero_run` and `rms`,
/// which is where a test can say what it actually found.
fn pitch_hz(stereo: &[f32], rate: u32) -> f64 {
    let mono = left(stereo);
    // 2 Hz steps: an order finer than the +-20 Hz the callers allow, and
    // far finer than the 8.8% a rate mismatch would move the tone.
    let mut best = (0.0f64, 0.0f64);
    let mut hz = 300.0;
    while hz <= 700.0 {
        let energy = tone_energy(&mono, rate, hz);
        if energy > best.1 {
            best = (hz, energy);
        }
        hz += 2.0;
    }
    best.0
}

/// A capture file shaped like the ones the sessions leave: two seconds of a
/// 440 Hz tone with `front` and `back` seconds of the silence the backend
/// writes while nothing is playing.
fn padded_fixture(label: &str, front: f64, back: f64) -> PathBuf {
    let path = temp_path("padding", &format!("{label}.wav"));
    let mut writer = hound::WavWriter::create(
        &path,
        hound::WavSpec {
            channels: 2,
            sample_rate: RATE,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .expect("padded wav");
    let quiet = ((front * f64::from(RATE)) as usize, RATE as usize * 2);
    for i in 0..quiet.0 + quiet.1 + (back * f64::from(RATE)) as usize {
        let s = match i.checked_sub(quiet.0) {
            Some(t) if t < quiet.1 => {
                (t as f32 / RATE as f32 * 440.0 * std::f32::consts::TAU).sin() * 0.5
            }
            _ => 0.0,
        };
        for _ in 0..2 {
            writer.write_sample(s).expect("padded sample");
        }
    }
    writer.finalize().expect("finalize padded wav");
    path
}

/// What makes a loudest-window reading evidence rather than a substitution that
/// happens to pass: the three readings the session tests take, on a file with
/// the padding a loaded Windows runner profiled around its audio, 1.75 s at the
/// front and 0.75 s at the back.
///
/// The tail of the same file is what the padding costs. At the padding as
/// profiled it holds three quarters of a second of exact zeros, which is the
/// silence-run assertion failing on a session that was healthy; one step slower
/// than that and the window is padding outright, with no level in it and a
/// pitch of nothing. That is the shape of the run in #464, which read 6 in its
/// last second against 8290 in its loudest.
/// The shape a loaded runner produced: nearly two seconds of silence while the
/// reopen got media flowing, then unbroken music. The reading has to be of the
/// music, and how long the machine took to start is not the test's business.
#[test]
fn a_slow_start_still_reads_the_music_that_followed() {
    let path = padded_fixture("slow-start", 1.96, 0.0);
    let (rate, all) = rate_and_samples(&path);
    let (music, opening) = after_opening_silence(&all);
    let second = f64::from(rate) as usize * 2;

    assert!(opening > second, "the fixture opens with under a second");
    assert!(
        music.len() >= second,
        "a second of tone follows, and it is what gets measured"
    );
    let window = loudest_of(music, rate, 1.0);
    assert!(rms(&window) > 0.02, "the window reads {}", rms(&window));
    assert_eq!(longest_zero_run(&window), 0, "the window holds the silence");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_measurement_starts_after_the_period_a_device_open_costs() {
    // One 240-frame period of zeros, then a second of tone: a capture from a
    // device that has just opened, which is what a reopen leaves behind.
    let period = 240 * 2;
    let path = padded_fixture("device-open", 240.0 / f64::from(RATE), 0.0);
    let (rate, all) = rate_and_samples(&path);
    assert_eq!(rate, RATE);

    let (music, opening) = after_opening_silence(&all);
    // A sine starts at zero, so the first sample of the tone is silent too and
    // the count runs a sample or two past the period rather than landing on it.
    assert!(
        (period..period + 4).contains(&opening),
        "the fixture opens with one period, and the count read {opening}"
    );
    assert_eq!(
        longest_zero_run(&loudest_of(music, rate, 1.0)),
        0,
        "the measured window still holds the silence the open wrote"
    );

    // The window taken from the top of the file is what used to be measured,
    // and it carries the whole opening period, so the bound the swap test
    // applies really does depend on starting after it.
    assert_eq!(
        longest_zero_run(&all[..period]),
        period,
        "the opening period is not silent, so this proves nothing"
    );

    // A stream that took much longer than a period to make a sound is a fault
    // rather than an open, and the count says so instead of being trimmed away.
    let slow = padded_fixture("device-open-slow", 0.5, 0.0);
    let (_, all) = rate_and_samples(&slow);
    let (_, opening) = after_opening_silence(&all);
    assert!(
        opening > period,
        "a half second of silence read as one period: {opening}"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&slow);
}

#[test]
fn a_measurement_reads_the_audio_and_not_the_padding_around_it() {
    for (label, back) in [("as-profiled", 0.75), ("a-slower-leave", 1.25)] {
        let path = padded_fixture(label, 1.75, back);

        let (rate, window) = loudest(&path, 1.0);
        assert_eq!(rate, RATE);
        let level = rms(&window);
        assert!(level > 0.02, "the loudest second reads {level}");
        let run = longest_zero_run(&window);
        assert!(run < 240, "the loudest second straddles the padding: {run}");
        let hz = pitch_hz(&window, rate);
        assert!(
            (hz - 440.0).abs() < 20.0,
            "the loudest second reads {hz} Hz"
        );

        let (_, last) = tail(&path, 1.0);
        let run = longest_zero_run(&last);
        assert!(
            run > (back * f64::from(RATE)) as usize,
            "the tail holds a {run}-sample silence run, so this proves nothing"
        );
        if back > 1.0 {
            let level = rms(&last);
            assert!(level < 0.001, "a tail of pure padding reads {level}");
            let hz = pitch_hz(&last, rate);
            assert!(
                (hz - 440.0).abs() > 20.0,
                "a tail of pure padding reads {hz} Hz, which would pass the bound"
            );
        }

        let _ = std::fs::remove_file(&path);
    }
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
    assert!(snap.stats.mouth_to_ear_ms().is_some());
}

/// The playout low water mark, end to end: the render callback measures the
/// fill, the worker samples it once a window, and it lands on the snapshot in
/// frames. Nothing between them is faked, which is the only way this reads a
/// real ring rather than a number the test wrote itself.
///
/// The figure is the claim: the cushion is two device buffers of frames, and the
/// offline driver tops the ring up before every pump, so it reads exactly that.
/// A reading in samples would be twice it, because the ring is interleaved
/// stereo.
#[test]
fn the_playout_water_mark_lands_on_the_snapshot_in_frames() {
    let server = TestServer::start();
    let rt = join_silent(&server, 1, "solo");
    wait_for(&rt, "joined", Duration::from_secs(10), joined);

    // The window is a second wide, so the first reading waits for one to close.
    let snap = wait_for(&rt, "a water mark", Duration::from_secs(10), |s| {
        s.stats.playout_low_frames.is_some()
    });
    let cushion = 2 * settings().buffer_frames as usize;
    assert_eq!(
        snap.stats.playout_low_frames,
        Some(cushion),
        "a worker that never missed a pump should read the whole {cushion}-frame \
         cushion; {} would be the interleaved samples and not the frames",
        cushion * 2
    );
}

/// The playout cushion inside the headline figure, at the two buffer sizes
/// furthest apart on the settings screen. The cushion is two device callbacks,
/// so it costs twice what the capture buffer does and it moves with the pick:
/// 5 ms at 120 frames and 20 ms at 480. Strip the link terms the same snapshot
/// reports and what is left is three callbacks, and the hover's own two figures
/// are the ones the sum was built from, as is the depth the buffer control
/// reports under the choices.
///
/// Both sizes, because a term the code got wrong by a constant would satisfy
/// this at one of them.
#[test]
fn the_figure_and_its_hover_carry_the_playout_cushion() {
    let server = TestServer::start();
    for (member, frames) in [(1u16, 120u32), (2, 480)] {
        let rt = LiveRuntime::join_offline(
            &server.invite(member, "solo"),
            AudioSettings {
                buffer_frames: frames,
                ..settings()
            },
            WavBackend::new(None, None),
        )
        .expect("join offline");
        let snap = wait_for(&rt, "joined with stats", Duration::from_secs(10), |s| {
            joined(s) && s.stats.mouth_to_ear_ms().is_some()
        });

        let m2e = snap.stats.mouth_to_ear_ms().expect("the predicate held");
        let link = link_ms(&snap);
        let device_ms = device_buffers_ms(frames as f32);
        assert!(
            (m2e - link - device_ms).abs() < 0.01,
            "at {frames}-frame callbacks mouth to ear {m2e} ms must carry \
             {device_ms} ms of device buffers over {link} ms of link"
        );

        let device = snap
            .stats
            .device_buffers
            .expect("a running stream prices both of its buffers");
        assert_eq!(device.capture_ms, frames as f32 / 48.0);
        assert_eq!(
            device.playout_ms,
            2.0 * frames as f32 / 48.0,
            "the cushion is two callbacks, not the ring it sits in"
        );
        assert_eq!(
            device.lines(),
            [
                format!("capture buffer {:.1} ms", device.capture_ms),
                format!("playout cushion {:.1} ms", device.playout_ms),
            ],
            "each direction is named, or the two read as one another"
        );

        // What the buffer control says about that same term, off the same
        // controller: the depth it reports and the depth the figure was priced
        // from are one number, and this ring is never starved, so nothing is
        // deepening it and nothing is out of room.
        let cushion = snap
            .stats
            .cushion
            .expect("a running stream is holding a depth");
        assert_eq!(cushion.held_ms(), device.playout_ms);
        assert_eq!(cushion.held_frames, 2 * frames as usize);
        assert_eq!(cushion.base_frames, cushion.held_frames);
        assert_eq!(cushion.callback_frames, frames as usize);
        assert!(
            !cushion.deepened(),
            "an offline ring is topped up before every pump"
        );
        assert!(!cushion.out_of_room);

        rt.send(Command::Leave);
        wait_for(&rt, "idle", Duration::from_secs(5), |s| {
            s.stats.state == ConnState::Idle
        });
    }
}

/// The server's own jitter buffer inside the headline figure. It holds this
/// client's uplink ahead of the mix, so it delays what the band hears the same
/// way the local buffer delays what this machine plays, and only the server can
/// see it: the depth arrives in its Stats report once a second.
///
/// Nothing here is faked: a real `ServerCore` buffers a real uplink and reports
/// the depth it settled on. What that report is worth is the wiring, since a
/// loopback link buffers nothing and reads 0; the term's price is pinned in the
/// unit tests, where the depth is ours to choose. The reports start a second
/// after the join, so the figure picks the term up while the session runs rather
/// than at the join.
#[test]
fn the_figure_charges_the_buffer_the_server_reports() {
    let server = TestServer::start();
    let rt = join_silent(&server, 1, "solo");
    // The figure is absent until a round trip has been measured, which joining
    // does not guarantee, so it is part of what this waits for rather than
    // something to read the instant the buffer report lands.
    let snap = wait_for(
        &rt,
        "the server's buffer report and a measured round trip",
        Duration::from_secs(10),
        |s| {
            joined(s)
                && s.stats.uplink_jitter_depth.is_some()
                && s.stats.mouth_to_ear_ms().is_some()
        },
    );

    let depth = snap.stats.uplink_jitter_depth.expect("the predicate held");
    let m2e = snap.stats.mouth_to_ear_ms().expect("the predicate held");
    let device_ms = device_buffers_ms(settings().buffer_frames as f32);
    assert!(
        (m2e - link_ms(&snap) - device_ms).abs() < 0.01,
        "mouth to ear {m2e} ms must be the terms beside it, the server's \
         {depth}-frame buffer included"
    );
    assert_eq!(
        snap.stats.jitter_lines()[1],
        format!("server buffer {depth} frames, yours standing in for the band's"),
        "the hover names the term the figure charges, or the number cannot be \
         broken down into what built it"
    );
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
    let energy = loudest_rms(&out_b, 1.0);
    assert!(
        energy > 0.02,
        "b's loudest second is near-silence (rms {energy}); a's audio never arrived"
    );

    for p in [&sine, &out_b] {
        let _ = std::fs::remove_file(p);
    }
}

/// Both sides play a tone of their own, at different frequencies so "own" and
/// "other" cannot be confused. A asks to hear itself; B never does, and it is
/// B's own exclusion still holding that makes A's inclusion mean something
/// rather than a personal mix that always included everyone.
#[test]
fn hear_self_puts_your_own_tone_in_your_own_playout() {
    let server = TestServer::start();
    let sine_a = sine_fixture("hear-self", 440.0, RATE);
    let sine_b = sine_fixture("hear-self", 660.0, RATE);
    let out_a = temp_path("hear-self", "out-a.wav");
    let out_b = temp_path("hear-self", "out-b.wav");

    let a = LiveRuntime::join_offline(
        &server.invite(1, "a"),
        settings(),
        WavBackend::new(Some(sine_a.clone()), Some(out_a.clone())),
    )
    .expect("join a");
    let b = LiveRuntime::join_offline(
        &server.invite(2, "b"),
        settings(),
        WavBackend::new(Some(sine_b.clone()), Some(out_b.clone())),
    )
    .expect("join b");

    for (rt, who) in [(&a, "a"), (&b, "b")] {
        wait_for(rt, who, Duration::from_secs(10), |s| {
            joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
        });
    }

    // The personal mix excludes self by default; give that a real stretch to
    // run before asking otherwise.
    std::thread::sleep(Duration::from_millis(2_000));
    a.send(Command::SetHearSelf(true));
    // Once A's own tone joins B's in A's mix, this stretch outweighs the
    // excluded-self one before it, so the whole file's loudest second lands
    // here rather than needing a fixed offset to find it.
    std::thread::sleep(Duration::from_millis(2_500));

    for rt in [&a, &b] {
        rt.send(Command::Leave);
    }
    for (rt, who) in [(&a, "a"), (&b, "b")] {
        wait_for(rt, who, Duration::from_secs(3), |s| {
            s.stats.state == ConnState::Idle
        });
    }
    drop(a);
    drop(b);

    // The back half, then the loudest second inside it. `loudest` over the
    // whole file picks by total level, which the other runtime's tone
    // dominates from the first second, so it can land before the command took
    // effect and read A's own tone as absent.
    let (rate_a, all_a) = rate_and_samples(&out_a);
    let back_a = &all_a[(all_a.len() / 4) * 2..];
    let window_a = loudest_of(back_a, rate_a, 1.0);
    let mono_a = left(&window_a);
    let own_in_a = tone_energy(&mono_a, rate_a, 440.0);
    let other_in_a = tone_energy(&mono_a, rate_a, 660.0);
    assert!(
        own_in_a > TONE_FLOOR,
        "a asked to hear itself and its own 440 Hz tone still reads {own_in_a} \
         in its loudest second, under the {TONE_FLOOR} floor. Whole file: {}",
        tone_profile(&out_a, 440.0)
    );
    assert!(
        other_in_a > TONE_FLOOR,
        "a should still hear b's 660 Hz tone alongside its own, reads {other_in_a}"
    );

    let (rate_b, all_b) = rate_and_samples(&out_b);
    let back_b = &all_b[(all_b.len() / 4) * 2..];
    let window_b = loudest_of(back_b, rate_b, 1.0);
    let mono_b = left(&window_b);
    let own_in_b = tone_energy(&mono_b, rate_b, 660.0);
    let other_in_b = tone_energy(&mono_b, rate_b, 440.0);
    assert!(
        other_in_b > TONE_FLOOR,
        "b's mix should be unchanged and still carry a's 440 Hz tone, reads {other_in_b}"
    );
    assert!(
        other_in_b > own_in_b * 4.0,
        "b never asked to hear itself, so its mix must keep excluding its own \
         660 Hz tone: heard {other_in_b} at 440 Hz against {own_in_b} at its own \
         660 Hz. Whole file: {}",
        tone_profile(&out_b, 660.0)
    );

    for p in [&sine_a, &sine_b, &out_a, &out_b] {
        let _ = std::fs::remove_file(p);
    }
}

/// A UDP relay that holds every datagram for a fixed delay in both directions,
/// so one client sits a real distance from the server while the rest of the
/// room is on loopback. Transparent to the session: every packet is encrypted
/// and authenticated end to end, and the server sees the relay as the peer.
///
/// One client per relay. The route back is the source address of whatever
/// arrived that was not the server, and two clients through one relay would be
/// one address with two members behind it.
struct DelayRelay {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DelayRelay {
    fn start(server: SocketAddr, one_way: Duration) -> Self {
        let socket = std::net::UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .expect("relay socket");
        let addr = socket.local_addr().expect("relay addr");
        // Short enough that the queue is flushed on time whether or not
        // anything is arriving, and the wait is the only thing this blocks on.
        socket
            .set_read_timeout(Some(Duration::from_millis(1)))
            .expect("relay read timeout");
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("jamstream-test-relay".into())
            .spawn(move || {
                let mut queue: std::collections::VecDeque<(Instant, Vec<u8>, SocketAddr)> =
                    std::collections::VecDeque::new();
                let mut client: Option<SocketAddr> = None;
                let mut buf = [0u8; MAX_DATAGRAM_BYTES];
                while !flag.load(Ordering::Relaxed) {
                    if let Ok((len, from)) = socket.recv_from(&mut buf) {
                        let to = if from == server {
                            client
                        } else {
                            client = Some(from);
                            Some(server)
                        };
                        if let Some(to) = to {
                            queue.push_back((Instant::now() + one_way, buf[..len].to_vec(), to));
                        }
                    }
                    let now = Instant::now();
                    while queue.front().is_some_and(|(due, _, _)| *due <= now) {
                        let (_, packet, to) = queue.pop_front().expect("the front is there");
                        let _ = socket.send_to(&packet, to);
                    }
                }
            })
            .expect("relay thread");
        DelayRelay {
            addr,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for DelayRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The offer to hear yourself, against a real session over a link with a real
/// delay in it. 40 ms each way puts mouth to ear where a cross-country band
/// reads, well past the threshold, and the loopback member in the same room
/// reads a tenth of it.
///
/// Three things this proves that the episode's own tests cannot: that the
/// figure the offer is taken from is the one the worker publishes, that a
/// healthy session reaches the same code and is offered nothing, and that a
/// musician who has already asked to hear themselves is never asked about it.
/// Every wait stops on its own condition; none of them is a measurement
/// window.
#[test]
fn a_far_apart_session_is_offered_hearing_itself() {
    const ONE_WAY: Duration = Duration::from_millis(40);
    // The window the offer waits out is ten seconds of the figure holding, so
    // this is that plus room for a join over a slow link on a loaded runner.
    const OFFER_BUDGET: Duration = Duration::from_secs(40);

    let server = TestServer::start();
    let far_relay = DelayRelay::start(server.addr, ONE_WAY);
    let settled_relay = DelayRelay::start(server.addr, ONE_WAY);

    let far = LiveRuntime::join_offline(
        &server.invite_via(far_relay.addr, 1, "far"),
        settings(),
        WavBackend::new(None, None),
    )
    .expect("join far");
    // Same distance, and this one asks to hear itself before the window can
    // ever close: the offer must never go out to somebody who has decided.
    let settled = LiveRuntime::join_offline(
        &server.invite_via(settled_relay.addr, 2, "settled"),
        settings(),
        WavBackend::new(None, None),
    )
    .expect("join settled");
    // Nobody is out of time with themselves, so the room needs a band in it.
    let near = join_silent(&server, 3, "near");

    for (rt, who) in [(&far, "far"), (&settled, "settled"), (&near, "near")] {
        wait_for(rt, who, Duration::from_secs(20), |s| {
            joined(s) && s.members.iter().filter(|m| m.connected).count() == 3
        });
    }
    settled.send(Command::SetHearSelf(true));

    let snap = wait_for(&far, "the offer", OFFER_BUDGET, |s| s.offer_hear_self);
    let far_ms = snap
        .stats
        .mouth_to_ear_ms()
        .expect("a joined session measures the figure the offer is taken from");
    assert!(
        far_ms > 30.0,
        "the offer must only stand over a figure past the threshold, and this \
         one reads {far_ms} ms"
    );
    assert!(
        snap.chat.is_empty(),
        "the band's column is for the band: {:?}",
        snap.chat
    );

    // Nothing here is asserted about the other members' figures, and the delay
    // is not what this proves. What the code promises is that a figure past the
    // threshold earns the offer, not that the network is why the figure is past
    // it, and on a contended runner it is not: one read 121 ms for the loopback
    // member against 108.5 for the one carrying 40 ms each way, so the machine
    // moves this number further than the leg does and the two are not
    // comparable. That a figure under the threshold offers nothing is asserted
    // in the unit tests, where the reading is ours to choose. What a real
    // session adds is the wiring: a real server, a real delay, and the offer
    // arriving on the figure the musician is shown.
    let settled_snap = settled.snapshot();
    assert!(
        settled_snap.hear_self,
        "the settled member asked to hear itself"
    );
    assert!(
        !settled_snap.offer_hear_self,
        "a musician already hearing themselves must stay unasked, and this one \
         reads {:?} ms",
        settled_snap.stats.mouth_to_ear_ms()
    );

    // And the offer that is standing goes when it is acted on.
    far.send(Command::SetHearSelf(true));
    wait_for(&far, "the offer to go", Duration::from_secs(5), |s| {
        !s.offer_hear_self
    });

    for rt in [&far, &settled, &near] {
        rt.send(Command::Leave);
    }
    for (rt, who) in [(&far, "far"), (&settled, "settled"), (&near, "near")] {
        wait_for(rt, who, Duration::from_secs(5), |s| {
            s.stats.state == ConnState::Idle
        });
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

    // The loudest second is the pre-mute one, since the mute is what the rest
    // of the file is: a fixed offset from the end walks off the audio entirely
    // once a slow leave stretches the padding behind it. The tail stays the
    // tail, because here the tail is the muted period and padding is silence
    // too.
    let audible = loudest_rms(&out_a, 1.0);
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

/// Changing your own buffer size must not cost you your uplink. The far side is
/// the judge, because the swapper hears themselves either way and the person who
/// stops being heard is the last to know.
#[test]
fn a_buffer_swap_keeps_the_swapper_audible_to_everybody_else() {
    let server = TestServer::start();
    let sine_b = sine_fixture("reconf-up", 660.0, RATE);
    let out_a = temp_path("reconf-up", "out-a.wav");

    // B is the one who plays and the one who changes settings; A only
    // listens, so this asks whether a swap costs B their uplink.
    let a = LiveRuntime::join_offline(
        &server.invite(1, "a"),
        settings(),
        WavBackend::new(None, Some(out_a.clone())),
    )
    .expect("join a");
    let b = LiveRuntime::join_offline(
        &server.invite(2, "b"),
        settings(),
        WavBackend::new(Some(sine_b.clone()), None),
    )
    .expect("join b");

    wait_for(&b, "b joined", Duration::from_secs(10), joined);
    wait_for(&a, "a sees both members", Duration::from_secs(10), |s| {
        joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
    });
    std::thread::sleep(Duration::from_millis(1_000));

    b.reconfigure_audio(AudioSettings {
        capture_id: None,
        playback_id: None,
        buffer_frames: 240,
        ..AudioSettings::default()
    });
    // Much longer after than before, so the back of this capture is post-swap
    // audio on any machine rather than at one particular speed.
    std::thread::sleep(Duration::from_millis(3_500));

    // What A recorded is the whole assertion, and it is the right one: the
    // question is whether the far side still hears B, and A's own file answers
    // it without going through a counter on this side, which can only report
    // B's uplink as the server last summarised it. The sharp detector for the
    // fault itself is `a_capture_gap_on_a_jittery_stream` in the session
    // crate, which fails every run in under half a second.

    a.send(Command::Leave);
    wait_for(&a, "a idle", Duration::from_secs(3), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(a);
    drop(b);

    let (rate, all) = rate_and_samples(&out_a);
    let frames = all.len() / 2;
    // The back half only. The loudest second of the whole file would be the
    // second before the swap, so a lost uplink would read as a pass.
    let back = &all[(frames / 2) * 2..];
    let window = loudest_of(back, rate, 1.0);
    let heard = tone_energy(left(&window).as_slice(), rate, 660.0);
    assert!(
        heard > TONE_FLOOR,
        "a stopped hearing b after b changed its buffer: {heard} at 660 Hz, \
         floor {TONE_FLOOR}. {}",
        tone_profile(&out_a, 660.0)
    );
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
    let energy = loudest_rms(&out_b, 1.0);
    assert!(
        energy > 0.02,
        "audio did not resume after the swap (loudest second rms {energy})"
    );

    for p in [&sine, &out_b] {
        let _ = std::fs::remove_file(p);
    }
}

/// A reopen somebody asked for is not a device failing, however many of them
/// they ask for: the counter behind the cutting-out state moves only for a
/// stream that stopped on its own. Four buffer changes is past the floor, so a
/// counter that took them would have this device reading as broken on the
/// screen of somebody who was only trying sizes out.
#[test]
fn settings_changes_never_read_as_a_device_cutting_out() {
    let server = TestServer::start();
    let sine = sine_fixture("settings-stops", 440.0, RATE);
    let backend = WavBackend::new(Some(sine.clone()), None);
    let device = backend.clone();
    let rt = LiveRuntime::join_offline(&server.invite(1, "solo"), settings(), backend)
        .expect("join offline");
    wait_for(&rt, "joined", Duration::from_secs(10), joined);

    // The join's own open is the first, so each pick waits for the next one.
    for (opens, buffer_frames) in [(2, 240u32), (3, 480), (4, 120), (5, 240)] {
        rt.reconfigure_audio(AudioSettings {
            buffer_frames,
            ..settings()
        });
        wait_for(&rt, "the reopen", Duration::from_secs(10), |_| {
            device.opens() >= opens
        });
    }

    let snap = rt.snapshot();
    assert_eq!(
        snap.stats.cutting_out,
        None,
        "{} picks the musician made read as a device cutting out",
        device.opens() - 1
    );
    assert_eq!(snap.audio_fault, None, "and none of them is a fault");

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(rt);
    let _ = std::fs::remove_file(&sine);
}

/// The cushion answer reaches the depth controller without the device being
/// touched. That is the whole reason it is not part of a reconfigure: a reopen
/// costs the band a few hundred milliseconds of capture, and this is a depth the
/// worker's own loop fills to. Measured against the backend's open count, so
/// nothing between the click and the sound card is stood in for.
#[test]
fn pinning_the_cushion_reaches_the_controller_without_reopening_the_device() {
    let server = TestServer::start();
    let sine = sine_fixture("pin-cushion", 440.0, RATE);
    let backend = WavBackend::new(Some(sine.clone()), None);
    let device = backend.clone();
    let rt = LiveRuntime::join_offline(&server.invite(1, "solo"), settings(), backend)
        .expect("join offline");
    wait_for(&rt, "joined", Duration::from_secs(10), joined);
    wait_for(&rt, "a cushion", Duration::from_secs(10), |s| {
        s.stats.cushion.is_some()
    });
    let opens = device.opens();
    let opening = rt.snapshot().stats.cushion.expect("a cushion");
    assert!(opening.auto, "a stream opens on the app's own default");

    rt.set_auto_cushion(false);
    wait_for(&rt, "the pin", Duration::from_secs(10), |s| {
        s.stats.cushion.is_some_and(|c| !c.auto)
    });
    let pinned = rt.snapshot().stats.cushion.expect("a cushion");
    assert_eq!(
        pinned.held_frames, pinned.base_frames,
        "a pinned depth is what the buffer size asks for and nothing more"
    );
    assert_eq!(
        device.opens(),
        opens,
        "pinning the cushion cost the band a device reopen"
    );

    // And back the other way, on the same message.
    rt.set_auto_cushion(true);
    wait_for(&rt, "the box ticked again", Duration::from_secs(10), |s| {
        s.stats.cushion.is_some_and(|c| c.auto)
    });
    assert_eq!(
        device.opens(),
        opens,
        "letting the cushion move again cost a device reopen"
    );

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(rt);
    let _ = std::fs::remove_file(&sine);
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

    let (_, window) = loudest(&out_b, 1.0);
    // A's sine arrived at all (capture side made it through the ring)...
    let energy = rms(&window);
    assert!(
        energy > 0.02,
        "b's loudest second is near-silence (rms {energy}); a's audio never arrived"
    );
    // ...and B's render never went hungry: an undersized ring pads ~480
    // zeros per callback, so anything close to that run length is padding,
    // not music.
    let run = longest_zero_run(&window);
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

    let (rate, window) = loudest(&out_b, 1.0);
    assert_eq!(rate, 44_100, "b's device writes on its own clock");
    let energy = rms(&window);
    assert!(
        energy > 0.02,
        "b's loudest second is near-silence (rms {energy}); a's audio never arrived"
    );
    let run = longest_zero_run(&window);
    assert!(
        run < 240,
        "steady-state playout contains a {run}-sample silence run"
    );
    let hz = pitch_hz(&window, rate);
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
        let (rate, window) = loudest(path, 1.0);
        assert_eq!(rate, device_rate, "{who} writes on its own device clock");
        let energy = rms(&window);
        assert!(
            energy > 0.02,
            "{who}'s loudest second is near-silence (rms {energy}); the other \
             member's audio never arrived"
        );
        let mono = left(&window);
        let wanted = tone_energy(&mono, rate, theirs);
        let leaked = tone_energy(&mono, rate, mine);
        assert!(
            wanted > leaked * 4.0,
            "{who} played {mine} Hz at {leaked:.1} against {theirs} Hz at {wanted:.1}: \
             that is its own signal, not the other member's"
        );
        let run = longest_zero_run(&window);
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
/// own added milliseconds, one line per converted direction for the status
/// bar and the Audio tab to render for as long as the stream runs, and a
/// mouth-to-ear figure that grew by exactly the disclosed amount.
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
        joined(s) && s.stats.mouth_to_ear_ms().is_some()
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

    // One disclosure per direction, in the state the Audio tab renders under
    // the pickers and the latency hover repeats. Standing facts about the
    // device, so they hold for as long as the stream does.
    let lines = rate.lines();
    for side in ["capture", "playback"] {
        let want = format!("converting {side} 44.1 kHz to 48 kHz");
        assert_eq!(
            lines.iter().filter(|l| l.contains(&want)).count(),
            1,
            "{side} disclosure: {lines:?}"
        );
    }
    assert!(
        snap.chat.is_empty(),
        "the band's column is for the band: {:?}",
        snap.chat
    );

    // Mouth to ear grew by the disclosed amount: strip the link terms this
    // same snapshot reports and the two device buffers, and what is left is
    // the converter's own figure. Both buffers are sized from the negotiated
    // callback in session-rate frames: the 120-frame request on a 44.1 kHz
    // device is ceil(120 * 160/147) = 131.
    let m2e = snap.stats.mouth_to_ear_ms().expect("the predicate held");
    let link = link_ms(&snap) + device_buffers_ms(131.0);
    let disclosed = capture_ms + playback_ms;
    assert!(
        (m2e - link - disclosed).abs() < 0.01,
        "mouth to ear {m2e} ms must carry the disclosed {disclosed} ms over {link} ms of link"
    );

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
}

/// A device lost mid-session: the stream is closed and reopened with the same
/// settings, on the first attempt of the episode, without losing the session
/// and without leaving a mark on any surface once it is back.
///
/// This is the test the offline backend existed for and could not run.
/// `Driver::errored` answered a flat `false` for the offline arm, so
/// `WavStream::errored` was never read and `Worker::check_stream`'s device-gone
/// branch was dead code under test: the only backend a test can drive could not
/// report a lost device, and the only one that could needs a hand on a cable.
/// Both halves of the fix are in the same change, which is the point:
/// `with_device_loss_after` on the backend, and reading it here.
///
/// One open for the join and one for the reopen is the promptness the backoff
/// must not cost: a device that was fine until it was pulled comes back on the
/// attempt whose wait is zero, not after the wait a flapping device earns.
#[test]
fn a_device_lost_mid_session_is_reopened_without_dropping_the_session() {
    let server = TestServer::start();
    let sine = sine_fixture("device-loss", 440.0, RATE);
    // Two hundred frames is half a second of pumping at 2.5 ms, so the loss
    // lands well after the join and well inside the test's own patience.
    let backend = WavBackend::new(Some(sine.clone()), None).with_device_loss_after(200);
    let device = backend.clone();
    let rt = LiveRuntime::join_offline(&server.invite(1, "solo"), settings(), backend)
        .expect("join offline");
    wait_for(&rt, "joined", Duration::from_secs(10), joined);

    let snap = wait_for(&rt, "the reopen", Duration::from_secs(10), |_| {
        device.opens() >= 2
    });
    assert_eq!(
        device.opens(),
        2,
        "the loss must be answered by one open, on the attempt that waits none"
    );
    assert_eq!(
        snap.stats.state,
        ConnState::Joined,
        "losing a device must not drop the session"
    );

    // Nothing is left saying so, because there is nothing left to say: the
    // stream a musician is playing through is the one that came back.
    std::thread::sleep(Duration::from_millis(200));
    let snap = rt.snapshot();
    assert_eq!(snap.audio_fault, None, "the fault is over");
    assert_eq!(
        snap.stats.cutting_out, None,
        "one device that came back is a blip, not a device cutting out"
    );
    assert_eq!(snap.device_error, None, "nothing refused");
    assert!(
        snap.chat.is_empty(),
        "a device is not something the app says in the room: {:?}",
        snap.chat
    );

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
/// its own interval: a close and an open every tick, and `Worker::step`
/// running far past the tick, so the rings were not serviced for the whole
/// episode. A WASAPI exclusive endpoint another process takes, a half-present
/// USB interface, and a PipeWire graph refusing the rate all arrive in this
/// shape.
///
/// What must hold instead: a handful of attempts over a widening backoff, a
/// state that says the loop has stopped and agrees with how many times it
/// really tried, and a session that is still joined at the end of it, which
/// is the proof the worker kept servicing the loop rather than drowning in
/// device opens.
///
/// This is also the harshest case for the column the band talks in: no
/// episode produces more device events than one that opens and dies six
/// times. It stays empty through all of it.
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

    // And it stops, in a state that stays on the Audio tab with the pick that
    // ends it, rather than a line that has scrolled by the time anybody looks.
    let snap = wait_for(&rt, "the loop to give up", Duration::from_secs(30), |s| {
        matches!(s.audio_fault, Some(AudioFaultView::GaveUp { .. }))
    });
    let Some(AudioFaultView::GaveUp { tries }) = snap.audio_fault else {
        unreachable!("the predicate above matched GaveUp")
    };
    // The count the state claims and the number of opens the fake counted are
    // the same story: one open for the join, then the tries it says it made.
    assert_eq!(
        device.opens(),
        tries + 1,
        "the state claims {tries} tries after {} opens",
        device.opens()
    );
    // Every one of those opens latched, so every one of them was a stream that
    // stopped on its own: the stops the cutting-out state counts are the opens
    // the fake counted, and no test fixture stands between the two.
    assert_eq!(
        snap.stats.cutting_out,
        Some(u64::from(device.opens())),
        "a device that dies on every open is a device cutting out"
    );

    // Nothing more is tried, and the session is still up: the network side
    // kept its tick the whole time the device was failing.
    std::thread::sleep(Duration::from_millis(1_500));
    assert_eq!(device.opens(), tries + 1, "the loop restarted itself");
    let snap = rt.snapshot();
    assert_eq!(snap.stats.state, ConnState::Joined);
    assert!(snap.stats.rtt_ms.is_some(), "pings kept flowing");
    assert_eq!(
        snap.audio_fault,
        Some(AudioFaultView::GaveUp { tries }),
        "the state holds while the stream is down"
    );
    assert!(
        snap.chat.is_empty(),
        "the band's column carries what people type and nothing else: {:?}",
        snap.chat
    );

    rt.send(Command::Leave);
    wait_for(&rt, "idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
}

/// User story: a musician swaps to an interface the stream it replaces has not
/// let go of yet, so the session spends a few seconds with no audio stream at
/// all, and when the device finally opens the band is still there.
///
/// The buffer left behind by those few seconds is #447. Playout is advanced only
/// from the device-paced top-up, so a worker with no stream stopped pulling
/// while media kept arriving and being pushed: the depth pinned at the cap the
/// buffer holds, every later arrival was refused for it, and the frames the
/// reopened stream would have continued from were the ones thrown away. On the
/// session that reported it the buffer sat at the cap on both machines with
/// `late` past a hundred thousand frames and not one re-anchor.
#[test]
fn a_stream_away_for_a_few_seconds_keeps_the_jitter_buffer_moving() {
    const REFUSAL: &str = "playback device is in use by another stream";
    let server = TestServer::start();
    let sine = sine_fixture("stream-away", 440.0, RATE);
    let out_b = temp_path("stream-away", "out-b.wav");

    let a = LiveRuntime::join_offline(
        &server.invite(1, "a"),
        settings(),
        WavBackend::new(Some(sine.clone()), None),
    )
    .expect("join a");
    // Three refused reopens on the 500/1000/2000 ms cadence: seconds with no
    // stream, inside the episode's own budget, and then a device again.
    let backend = WavBackend::new(None, Some(out_b.clone()))
        .refusing_reopens(3, AudioError::Unsupported(REFUSAL.to_owned()));
    let device = backend.clone();
    let b = LiveRuntime::join_offline(&server.invite(2, "b"), settings(), backend).expect("join b");

    wait_for(&a, "a joined", Duration::from_secs(10), joined);
    wait_for(&b, "b sees both members", Duration::from_secs(10), |s| {
        joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
    });
    // A's media has to be arriving before the stream goes away, or there is
    // nothing for the buffer to fill with.
    wait_for(&b, "b's buffer to anchor", Duration::from_secs(10), |s| {
        s.stats.jitter_depth > 0
    });

    let mut deepest = 0usize;
    b.reconfigure_audio(AudioSettings {
        playback_id: Some("the other interface".to_owned()),
        ..settings()
    });
    wait_for(&b, "the refusal", Duration::from_secs(10), |s| {
        deepest = deepest.max(s.stats.jitter_depth);
        s.device_error.is_some()
    });
    wait_for(&b, "the reopen", Duration::from_secs(20), |s| {
        deepest = deepest.max(s.stats.jitter_depth);
        s.device_error.is_none()
    });
    assert!(
        device.opens() >= 4,
        "{} opens: the refusals never happened",
        device.opens()
    );
    // Reaching the cap is what costs the playout position, and a buffer nobody
    // drains pins there: that is the fault this covers. How far under the cap a
    // drained buffer sits depends on how promptly the worker gets to run, so the
    // cap is the assertion and the target is only context.
    assert!(
        deepest < JitterBuffer::MAX_DEPTH_FRAMES,
        "b's buffer reached the {}-frame cap with no stream to pull it, so the \
         drain never ran; its target never exceeds {}",
        JitterBuffer::MAX_DEPTH_FRAMES,
        JitterBuffer::MAX_TARGET_FRAMES
    );

    // And the half that matters: audio comes back. The capture file is
    // rewritten by the open that succeeded, so everything in it played after
    // the stream returned.
    std::thread::sleep(Duration::from_millis(2_500));
    b.send(Command::Leave);
    wait_for(&b, "b idle", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(b);
    drop(a);
    let rms = loudest_rms(&out_b, 1.0);
    assert!(
        rms > 0.02,
        "b heard near-silence (rms {rms}) after its stream came back"
    );

    for p in [&sine, &out_b] {
        let _ = std::fs::remove_file(p);
    }
}

/// User story: a musician swaps interfaces mid-song, the new one runs at
/// 44.1 kHz, and the music keeps playing through the boundary converter
/// (#347 rung 3) where it used to be refused. What arrives on the swapped-in
/// interface must still be the room's audio: at level, at pitch, and free of
/// underrun padding; and the swap is disclosed in the snapshot's rate outcome
/// for as long as the converter runs.
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
        s.stats.rate.is_some_and(|r| {
            matches!(
                r.playback,
                RateOutcomeView::Resampled { device: 44_100, .. }
            )
        })
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
    // The swap is disclosed: the snapshot carries the converting outcome for
    // as long as the converter runs, which is what both surfaces read.
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
        rate.lines()
            .iter()
            .filter(|l| l.contains("converting playback 44.1 kHz to 48 kHz"))
            .count(),
        1,
        "one disclosure for the swapped-in direction: {:?}",
        rate.lines()
    );
    assert_eq!(snap.audio_fault, None, "the stream is back");
    b.send(Command::Leave);
    wait_for(&b, "b idle", Duration::from_secs(3), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(b);
    drop(a);

    let (rate, all) = rate_and_samples(&out_b);
    assert_eq!(
        rate, 44_100,
        "the swapped-in device writes on its own clock"
    );
    // A capture opens with silence: the device writes its first period before
    // any pull reaches it, and a loaded machine takes longer still to get
    // media flowing after a reopen. How long that took is the machine's
    // business, so what is asserted is the music that followed rather than
    // how soon it arrived. There has to be a second of it to read.
    let (music, opening) = after_opening_silence(&all);
    let second = f64::from(rate) as usize * 2;
    assert!(
        music.len() >= second,
        "only {:.3} s of audio followed {:.3} s of opening silence, too little \
         to measure. {}",
        music.len() as f64 / 2.0 / f64::from(rate),
        opening as f64 / 2.0 / f64::from(rate),
        tone_profile(&out_b, 440.0)
    );
    let window = loudest_of(music, rate, 1.0);
    let energy = rms(&window);
    assert!(
        energy > 0.02,
        "b's loudest second after the swap is near-silence (rms {energy})"
    );
    let run = longest_zero_run(&window);
    assert!(
        run < 240,
        "post-swap playout contains a {run}-sample silence run"
    );
    let hz = pitch_hz(&window, rate);
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
    // claimed: both were, before #327. The device's own words are the whole
    // of it, and nothing about it reaches the band's column.
    assert!(
        !reason.contains("disconnected") && !reason.contains("system default"),
        "a refusal must not read as an unplug or a fallback: {reason}"
    );
    assert!(
        snap.chat.is_empty(),
        "a refusal is not something the app says in the room: {:?}",
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
        s.device_error.is_some()
    });
    assert_eq!(
        snap.device_error.as_deref(),
        Some("audio device is gone or was never present"),
        "a device that is gone is not a device that refused"
    );
    // The same words however many times the cadence retries it, because the
    // state is the last failed open's own reason rather than a running tally.
    std::thread::sleep(Duration::from_millis(1_500));
    let snap = rt.snapshot();
    assert_eq!(
        snap.device_error.as_deref(),
        Some("audio device is gone or was never present")
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
        joined(s) && s.stats.mouth_to_ear_ms().is_some()
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

    // One disclosure, for the direction that earned it.
    let lines = rate.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("converting capture 44.1 kHz to 48 kHz"));

    // And one direction's milliseconds. Both device buffers are sized from the
    // negotiated callback in session-rate frames: the 120-frame request against
    // a 44.1 kHz capture endpoint is ceil(120 * 160/147) = 131.
    let m2e = snap.stats.mouth_to_ear_ms().expect("the predicate held");
    let link = link_ms(&snap) + device_buffers_ms(131.0);
    assert!(
        (m2e - link - capture_ms).abs() < 0.01,
        "mouth to ear {m2e} ms must carry the converted direction's \
         {capture_ms} ms over {link} ms of link"
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
/// way from the backend to the Audio tab.
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
        joined(s) && s.stats.mouth_to_ear_ms().is_some()
    });

    let rate = snap.stats.rate.expect("a running stream reports its rungs");
    assert_eq!(rate.capture, RateOutcomeView::ClockSet { from: 44_100 });
    assert_eq!(
        rate.playback,
        RateOutcomeView::OsConverted { device: 44_100 }
    );
    assert_eq!(rate.added_ms(), 0.0, "neither rung runs a converter");

    // Both rungs are disclosed under the pickers in their own words: the moved
    // clock has a consequence outside this app, since every other program on
    // that device is now hearing 48 kHz, and the OS converter is the device's
    // own rate winning.
    let lines = rate.lines();
    assert_eq!(
        lines,
        vec![
            "moved the capture device to 48 kHz (was 44.1)".to_owned(),
            "the OS is converting playback to this device's 44.1 kHz".to_owned(),
        ],
        "the copy per rung is not interchangeable"
    );

    // No converter, so mouth to ear carries no converter term, and both device
    // buffers are sized from the plain 120-frame request.
    let m2e = snap.stats.mouth_to_ear_ms().expect("the predicate held");
    let link = link_ms(&snap) + device_buffers_ms(120.0);
    assert!((m2e - link).abs() < 0.01, "mouth to ear {m2e} ms");

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
/// refusal stands under the pickers in the device's own words for as long as it
/// holds, and nothing claims the fallback the old code silently made. Before
/// this, `applied_audio` and the pickers said the new device while the system
/// default ran, for the rest of the session.
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
    assert!(
        !reason.contains("disconnected") && !reason.contains("system default"),
        "a refused pick must not be reported as an unplug or a fallback: {reason}"
    );

    // The cadence keeps retrying the same refused device, and the reason on
    // screen is the last failed open's own rather than one line per attempt.
    std::thread::sleep(Duration::from_millis(1_500));
    let snap = rt.snapshot();
    assert_eq!(
        snap.device_error.as_deref(),
        Some(format!("unsupported audio configuration: {REFUSAL}").as_str())
    );
    assert!(
        snap.chat.is_empty(),
        "the retry cadence must leave the band's column alone: {:?}",
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
/// file. The subscriber and the file are both process wide, so the session below
/// has to be the only one in the process for its whole length, install included:
/// that is `alone_in_the_process`, and it is what makes the answer the same
/// under `cargo test`, where the binary runs every test on threads of one
/// process, as under nextest, where each test gets a process of its own.
/// A settings change is the one moment this client cannot see into: the device
/// is shut for as long as the platform takes and nothing is captured then. The
/// reopen carries diagnostics for that gap, and they speak only when the gap
/// costs something, because a buffer size somebody chose is not a fault and this
/// file's first line promises a healthy run leaves it empty.
#[test]
fn a_settings_change_that_worked_stays_out_of_the_log() {
    if std::env::var_os("RUST_LOG").is_some() {
        eprintln!("skipping: RUST_LOG replaces the default filter this is about");
        return;
    }
    let server = TestServer::alone_in_the_process();

    let dir = temp_path("reopen-diag", "logs");
    let _ = std::fs::remove_dir_all(&dir);
    let log = dir.join("app.log");
    // Needs a process to itself: the subscriber is global, installing truncates
    // the file, and the quiet-log test asserts its own log holds only a banner.
    // Under `cargo test` the two share a process and whichever installs second
    // erases the other's lines. nextest gives every test its own, and CI runs
    // nextest.
    if std::env::var_os("NEXTEST").is_none() {
        eprintln!("skipping: this needs a process to itself; run it under nextest");
        return;
    }
    let installed = jamstream_client::logging::init_at(log.clone()).expect("install the log");
    assert_eq!(installed, log);

    let sine = sine_fixture("reopen-diag", 440.0, RATE);
    let out_b = temp_path("reopen-diag", "out-b.wav");
    let a = LiveRuntime::join_offline(
        &server.invite(1, "a"),
        settings(),
        WavBackend::new(Some(sine.clone()), None),
    )
    .expect("join a");
    let b = LiveRuntime::join_offline(
        &server.invite(2, "b"),
        settings(),
        WavBackend::new(Some(sine.clone()), Some(out_b.clone())),
    )
    .expect("join b");
    wait_for(&b, "b sees both members", Duration::from_secs(10), |s| {
        joined(s) && s.members.iter().filter(|m| m.connected).count() == 2
    });

    b.reconfigure_audio(AudioSettings {
        capture_id: None,
        playback_id: None,
        buffer_frames: 240,
        ..AudioSettings::default()
    });
    // Two seconds of capture have to move before the second line reports, so
    // that the server's opinion of the uplink has arrived and is not None.
    std::thread::sleep(Duration::from_millis(3_000));

    b.send(Command::Leave);
    wait_for(&b, "b idle", Duration::from_secs(3), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(b);
    drop(a);

    let text = std::fs::read_to_string(&log).expect("the log is readable");
    let noise: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("reopen") || line.contains("uplink"))
        .collect();
    assert!(
        noise.is_empty(),
        "a settings change that worked wrote {} line(s) to a file whose first \
         line promises to stay empty on a healthy run:\n{}",
        noise.len(),
        noise.join("\n")
    );

    let _ = std::fs::remove_file(&sine);
    let _ = std::fs::remove_file(&out_b);
}

#[test]
fn a_healthy_session_leaves_the_log_holding_only_its_banner() {
    if std::env::var_os("RUST_LOG").is_some() {
        eprintln!("skipping: RUST_LOG replaces the default filter this is about");
        return;
    }
    let server = TestServer::alone_in_the_process();

    // Its own directory: the log goes through the private-file machinery, which
    // refuses to write key material next to a world-writable temp root.
    let dir = temp_path("quiet", "logs");
    let _ = std::fs::remove_dir_all(&dir);
    let log = dir.join("app.log");
    let installed = jamstream_client::logging::init_at(log.clone()).expect("install the log");
    assert_eq!(installed, log);

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
        let (rate, window) = loudest(out, 1.0);
        let mono = left(&window);
        assert!(
            mono.len() >= rate as usize / 2,
            "{out:?} holds {} frames at {rate} Hz, under half a second, so there \
             is nothing to measure",
            mono.len()
        );
        let heard = tone_energy(&mono, rate, theirs);
        let own = tone_energy(&mono, rate, mine);

        // The floor comes first: a ratio between two noise floors decides
        // nothing, and a silent run reads single digits where a tone reads
        // thousands.
        assert!(
            heard > TONE_FLOOR,
            "{out:?} heard {heard} at {theirs} Hz in its loudest second, under \
             the {TONE_FLOOR} floor, so no second of it carried audio; its own \
             {mine} Hz read {own} there. The whole file: {}. The log holds: {}",
            tone_profile(out, theirs),
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        assert!(
            heard > own * 4.0,
            "{out:?} heard {heard} at {theirs} Hz against {own} at its own {mine} Hz. \
             The whole file: {}",
            tone_profile(out, theirs)
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

    // Every warning the client writes about its own configuration and health is
    // decided by the fixtures and has to be absent. The dropout line is the one
    // exception, because it is the one measured against the wall clock: this
    // session's threads share a runner with the rest of the suite, and a runner
    // that takes the process away for a quarter second leaves a client pulling
    // at the frame clock with nothing arriving. That is a gap a listener would
    // have heard, so the line is right to be there, and the offline driver makes
    // it worse than a device clock can by replaying a stalled tick's debt at cpu
    // speed. What this no longer proves is that the process was never starved.
    let (gaps, rest): (Vec<&str>, Vec<&str>) = lines[1..]
        .iter()
        .partition(|l| l.contains("playout is concealing a gap"));
    assert!(rest.is_empty(), "a healthy session wrote {rest:#?}");

    // Tolerated is not unexamined. `concealed` counts frames the buffer had
    // nothing to play and `gap_ms` is wall clock; they are measured apart from
    // each other, so frames that do not cover the milliseconds mean the observer
    // stalled rather than the stream, which is the ring's story and not this
    // line's. And the gaps together stay a small part of the playing window, or
    // this is a transport that does not work rather than a runner under load.
    fn field(line: &str, key: &str) -> u64 {
        let at = line
            .find(key)
            .unwrap_or_else(|| panic!("no {key} in {line}"));
        line[at + key.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or_else(|_| panic!("no number after {key} in {line}"))
    }
    let mut total = Duration::ZERO;
    for line in &gaps {
        let claimed = Duration::from_millis(field(line, "gap_ms="));
        let covered = Duration::from_micros(field(line, "concealed=") * 2_500);
        assert!(
            covered * 2 >= claimed,
            "{line} claims {claimed:?} of gap that only {covered:?} of concealed \
             frames accounts for, so playout was not pulling across it"
        );
        total += claimed;
    }
    assert!(
        total <= Duration::from_secs(1),
        "playout dropped out for {total:?} of a four second session, which is a \
         transport that does not work rather than a loaded runner: {gaps:#?}"
    );

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
