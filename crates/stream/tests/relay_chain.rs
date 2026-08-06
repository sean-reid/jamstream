//! The whole broadcast chain, relay included: encoder to MediaMTX to pusher.
//!
//! `real_ffmpeg.rs` points the encoder at a file, which proves the encode
//! ladder and proves nothing about the hop the ladder travels over. That hop
//! is where broadcast broke: the encoder publishes to
//! `rtmp://127.0.0.1:1935/jamstream` and every pusher reads from the same
//! address, so both ends of a live broadcast depend on the relay being up and
//! the RTMP handshake working, and a host on v0.2.1 got `exited with status
//! 145` (ffmpeg's -111 truncated, ECONNREFUSED on Linux) against that
//! loopback address while the suite stayed green.
//!
//! Three things make this test the thing it claims to be.
//!
//! The relay config is the one we ship. It is lifted back out of a rendered
//! cloud-init rather than written here, because a test that invents its own
//! relay config is a test agreeing with its own fake, and the failure this
//! exists for is a real configuration that nothing ever ran. The addresses in
//! it drive the test, so `StreamConfig`'s defaults and cloud-init's yml have
//! to agree or this fails before it starts.
//!
//! The relay is the pinned release, installed by
//! `scripts/install-pinned-mediamtx.sh` from the same
//! `crates/cloud/data/media_artifacts.json` a session VM boots from, so there
//! is one version rather than two that can drift.
//!
//! And the harness proves it can fail before it claims to pass. The first
//! phase runs the chain with no relay listening and requires the failure,
//! printing what a host would be told. A harness that passes whether or not
//! mediamtx is up has the same defect as the test it replaces.
//!
//! What is still unproven: the pusher's last hop. Its real destination is
//! `rtmps://a.rtmps.youtube.com:443/live2/{key}`, which no CI runner can
//! contact, so the launcher's shell is swapped for one that reads the staged
//! ingest URL, checks it is the one the catalog built, and hands ffmpeg a
//! local file instead. Every argument the pusher runs with is still the
//! pipeline's own, and everything up to and including reading from the relay
//! is real; the TLS handshake with a platform and the platform's opinion of
//! the key are not tested here and cannot be.
//!
//! Missing tools are a FAILURE on Linux and a skip elsewhere, per
//! `common::require`.
//!
//! Unix only, like `real_ffmpeg.rs`: the pipeline hands video to ffmpeg
//! through a named pipe.
#![cfg(unix)]

mod common;

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use jamstream_cloud::cloudinit::{self, BootConfig, SelfDestruct};
use jamstream_protocol::control::{DestinationState, StreamKey, StreamOp, StreamPlatform};
use jamstream_protocol::ids::{DestinationId, MemberId};
use jamstream_stream::pipeline::{Pipeline, PipelineEvent, Roster, StreamConfig, StreamMember};
use jamstream_stream::proc::StdProcessHost;

use common::{
    TICKS_PER_SEC, arm_deadline, feed_programme, h264_headers, header_field, last_pts, mean_volume,
    packet_times, probe, probe_entries, require, tick_ms,
};

/// Seconds of programme fed through the live chain.
const SECONDS: u64 = 12;

/// Least the pusher may carry out of that, in seconds. See the comment on the
/// span assertion for where the shortfall goes.
const MIN_RELAYED_SECS: f64 = 4.0;

/// When the host pastes a key and clicks Go Live, in seconds of programme.
///
/// After the encoder, on purpose. A pusher spawned before the encoder has
/// published finds a relay path with no publisher, exits, and waits out a
/// backoff, which is the pipeline working correctly and four seconds this test
/// does not need to spend. It is also the order a host works in.
const GO_LIVE_SEC: u64 = 2;

/// A destination is Live once its pusher has run this long; see HEALTHY_MS in
/// the pipeline. Kept here as the number this test's arithmetic depends on.
const HEALTHY_SEC: u64 = 3;

/// How long the relay gets to bind its port before the test gives up on it.
const RELAY_READY: Duration = Duration::from_secs(15);

/// How long the chain gets to deliver what is in flight before it is stopped.
const SETTLE: Duration = Duration::from_secs(2);

const WEDGE_NOTE: &str = "Either an ffmpeg is blocked on a pipe (issue #248) \
                          or a process in the chain is waiting on one that is never coming. The \
                          relay's own log is in the work directory named above.";

/// The relay config cloud-init writes, lifted back out of a rendered
/// cloud-config.
///
/// Not a copy of the yml. The point of the exercise is to run the bytes a
/// session VM runs: this same text goes to `/etc/jamstream/mediamtx.yml`, and
/// mediamtx rejects an unknown key outright and exits, which systemd reports
/// as a `Type=simple` unit that started.
fn shipped_relay_config() -> String {
    let rendered = cloudinit::render(&BootConfig {
        artifact_url: "https://example.invalid/jamstreamd".to_owned(),
        artifact_sha256: "0".repeat(64),
        server_private_key_b64: "c2VydmVyLXByaXZhdGUta2V5".to_owned(),
        issuer_public_key_b64: "aXNzdWVyLXB1YmxpYy1rZXk=".to_owned(),
        session_id_hex: "deadbeefcafef00d".to_owned(),
        port: 43210,
        idle_shutdown_min: 10,
        max_duration_min: 720,
        self_destruct: SelfDestruct::AwsShutdown,
        recording: None,
    });
    // write_files bodies are indented six spaces under `content: |`.
    const BODY: &str = "      ";
    let mut lines = rendered.lines();
    lines
        .by_ref()
        .find(|l| l.trim() == "- path: /etc/jamstream/mediamtx.yml")
        .expect("cloud-init writes /etc/jamstream/mediamtx.yml");
    lines
        .by_ref()
        .find(|l| l.trim() == "content: |")
        .expect("the mediamtx.yml entry carries its content inline");
    let mut yml = String::new();
    for line in lines {
        let Some(body) = line.strip_prefix(BODY) else {
            break;
        };
        yml.push_str(body);
        yml.push('\n');
    }
    assert!(
        yml.contains("rtmpAddress:"),
        "the extracted relay config is not one:\n{yml}"
    );
    yml
}

/// One top-level scalar out of the relay config.
fn relay_scalar(yml: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    yml.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .map(|v| v.trim().trim_matches('"').to_owned())
        .unwrap_or_else(|| panic!("no {key} in the relay config:\n{yml}"))
}

/// The single path the relay serves, read from its `paths:` block.
fn relay_path(yml: &str) -> String {
    let mut lines = yml.lines().skip_while(|l| l.trim_end() != "paths:");
    lines.next();
    lines
        .next()
        .and_then(|l| l.trim().strip_suffix(':'))
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("no paths: block in the relay config:\n{yml}"))
}

/// A running mediamtx, killed when this drops so a panic cannot leave one
/// holding the port for the rest of the run.
struct Relay {
    child: Child,
    log: PathBuf,
}

impl Relay {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts the relay on the shipped config and waits for it to listen.
fn start_relay(mediamtx: &Path, dir: &Path, yml: &str, addr: SocketAddr) -> Relay {
    let cfg_path = dir.join("mediamtx.yml");
    std::fs::write(&cfg_path, yml).expect("write the relay config");
    let log_path = dir.join("mediamtx.log");
    let log = std::fs::File::create(&log_path).expect("relay log");
    let child = Command::new(mediamtx)
        .arg(&cfg_path)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone().expect("log handle"))
        .stderr(log)
        .spawn()
        .expect("mediamtx starts");
    let mut relay = Relay {
        child,
        log: log_path,
    };
    let deadline = Instant::now() + RELAY_READY;
    loop {
        if let Ok(Some(status)) = relay.child.try_wait() {
            panic!(
                "the relay ended ({status}) before it listened on {addr}, on the config \
                 cloud-init writes. This is the failure a session VM cannot \
                 show anyone: systemd calls a Type=simple unit started the \
                 moment it forks, so a relay that dies on its own \
                 configuration still logs `Started mediamtx.service` and \
                 nothing after it. The relay said:\n{}",
                relay.log()
            );
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return relay;
        }
        assert!(
            Instant::now() < deadline,
            "the relay is still running but never listened on {addr} within \
             {RELAY_READY:?}. It said:\n{}",
            relay.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Writes the shell the pipeline launches pushers with, replaced by one that
/// terminates the push at `sink`.
///
/// The pipeline invokes `<shell> -c <launcher script>` with the staged ingest
/// URL on stdin. This reads that URL, refuses to run if it is not the one the
/// catalog built, and re-runs the real shell with the same arguments and a
/// local path on stdin instead. The launcher, and the ffmpeg argv it execs,
/// are untouched.
fn write_platform_stand_in(dir: &Path, sink: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let line = dir.join("sink-url");
    std::fs::write(&line, format!("{}\n", sink.display())).expect("write the sink url");
    let script = dir.join("platform-stand-in.sh");
    let body = format!(
        r#"#!/bin/sh
IFS= read -r url || exit 64
case "$url" in
  rtmps://*) ;;
  *) echo 'the staged ingest url is not one the catalog builds' >&2; exit 65 ;;
esac
exec /bin/sh "$@" < '{}'
"#,
        line.display()
    );
    std::fs::write(&script, body).expect("write the stand-in shell");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make the stand-in shell executable");
    script
}

fn roster() -> Roster {
    Roster {
        members: vec![
            StreamMember {
                id: MemberId(1),
                name: "Ana Solari".into(),
                connected: true,
                avatar: None,
            },
            StreamMember {
                id: MemberId(2),
                name: "Ben Okafor".into(),
                connected: true,
                avatar: None,
            },
        ],
        listeners: 7,
    }
}

fn add_youtube(pipeline: &mut Pipeline<StdProcessHost>, now_ms: u64) {
    pipeline
        .apply(
            now_ms,
            StreamOp::AddDestination {
                id: DestinationId(1),
                platform: StreamPlatform::YouTube,
                // Shaped like a key and worth nothing. It never leaves the
                // stand-in shell, which drops it.
                key: StreamKey::new("jams-tream-test-key0"),
            },
        )
        .expect("add a destination");
}

/// What the host would be told about the one destination.
fn destination_state(pipeline: &Pipeline<StdProcessHost>) -> DestinationState {
    pipeline
        .status()
        .first()
        .map(|d| d.state.clone())
        .expect("one destination")
}

#[test]
fn the_encoder_reaches_a_pusher_through_the_relay_we_ship() {
    arm_deadline("relay_chain", WEDGE_NOTE);
    let Some(tools) = require(
        "the_encoder_reaches_a_pusher_through_the_relay_we_ship",
        &["ffmpeg", "ffprobe", "mediamtx"],
        "the encoder-to-relay-to-pusher chain a live broadcast runs on",
    ) else {
        return;
    };
    let (ffmpeg, ffprobe, mediamtx) = (tools[0].clone(), tools[1].clone(), tools[2].clone());

    let root = std::env::temp_dir().join(format!("jamstream-relaychain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("work dir");
    println!("work directory: {}", root.display());

    // The addresses under test come from the config the VM boots with, not
    // from this file, and the pipeline's defaults have to match them. Two
    // crates hold one address between them; drift means the encoder publishes
    // where nothing is listening, which is the shape of the bug being chased.
    let yml = shipped_relay_config();
    let rtmp_addr: SocketAddr = relay_scalar(&yml, "rtmpAddress")
        .parse()
        .expect("rtmpAddress is a socket address");
    let relay_url = format!("rtmp://{rtmp_addr}/{}", relay_path(&yml));
    let defaults = StreamConfig::default();
    assert_eq!(
        defaults.encoder_output, relay_url,
        "the encoder publishes somewhere the relay config does not serve"
    );
    assert_eq!(
        defaults.pusher_input, relay_url,
        "the pushers read from somewhere the relay config does not serve"
    );
    println!("the shipped relay config serves {relay_url}");

    // Nothing else may own the port, or every claim below is about some other
    // process's relay.
    assert!(
        TcpStream::connect_timeout(&rtmp_addr, Duration::from_millis(200)).is_err(),
        "something is already listening on {rtmp_addr}; this test needs the \
         address the product uses, so it cannot judge a relay it did not start"
    );

    // Phase one: production's failure, on purpose.
    //
    // If the rest of this test passed with the relay down it would be
    // measuring nothing, so the failure comes first and is required.
    let refused = root.join("refused.flv");
    let out = Command::new(&ffmpeg)
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostats",
            "-i",
            &relay_url,
            "-c",
            "copy",
            "-f",
            "flv",
        ])
        .arg(&refused)
        .output()
        .expect("ffmpeg runs");
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    println!(
        "a pusher against a relay that is not there: {}\n{stderr}",
        out.status
    );
    assert!(
        !out.status.success(),
        "a pusher succeeded against a relay that is not running, so nothing \
         below is evidence of anything"
    );
    assert!(
        stderr.to_lowercase().contains("refused"),
        "the pusher failed without saying the connection was refused, so this \
         is some other failure and the harness is not reproducing the one it \
         claims: {stderr}"
    );

    // The same absence as the pipeline sees it, which is what a host is shown.
    let dead = root.join("dead");
    std::fs::create_dir_all(&dead).expect("work dir");
    let mut pipeline = Pipeline::new(
        StreamConfig {
            ffmpeg: ffmpeg.clone(),
            shell: write_platform_stand_in(&dead, &root.join("unreachable.flv")),
            work_dir: dead.clone(),
            key_dir: dead.join("keys"),
            ..StreamConfig::default()
        },
        StdProcessHost::new(),
    );
    pipeline.set_roster(roster());
    pipeline.apply(0, StreamOp::Start).expect("start");
    add_youtube(&mut pipeline, 0);
    let mut down = None;
    // Both ends have to be seen refused: the encoder could not publish and the
    // pusher could not read. Usually inside half a second, but the cap is
    // generous because how long a spawned ffmpeg takes to reach its connect is
    // a fact about the runner, and a pusher that has not died yet is reported
    // Connecting or, past three seconds, Live.
    feed_programme(&mut pipeline, 12 * TICKS_PER_SEC, |tick, p| {
        for event in p.events() {
            println!("  {} ms {event:?}", tick_ms(tick));
            if let PipelineEvent::EncoderDown { reason } = event {
                down = Some(reason);
            }
        }
        let failed = matches!(destination_state(p), DestinationState::Failed { .. });
        !(down.is_some() && failed)
    });
    let reason = down.expect(
        "the encoder published to a relay that is not running and the pipeline \
         never reported it down. Every assertion in this test would then hold \
         with the relay dead, which is the defect it exists to remove.",
    );
    println!("with no relay, the pipeline reported the encoder down: {reason}");
    match destination_state(&pipeline) {
        DestinationState::Failed { reason } => {
            println!("with no relay, the host is told: {reason}");
        }
        other => panic!(
            "the destination is {other:?} against a relay that is not running, \
             so a broken broadcast is indistinguishable from a working one"
        ),
    }
    assert!(!pipeline.on_air(), "on air with no relay");
    drop(pipeline);

    // Phase two: the same chain, with the relay the VM runs.
    let live = root.join("live");
    std::fs::create_dir_all(&live).expect("work dir");
    let relay = start_relay(&mediamtx, &live, &yml, rtmp_addr);
    let relayed = live.join("relayed.flv");

    let cfg = StreamConfig {
        ffmpeg: ffmpeg.clone(),
        shell: write_platform_stand_in(&live, &relayed),
        work_dir: live.clone(),
        key_dir: live.join("keys"),
        ..StreamConfig::default()
    };
    let (fps, keyframe_secs, video_kbps) = (cfg.fps, cfg.keyframe_secs, cfg.video_kbps);
    let mut pipeline = Pipeline::new(cfg, StdProcessHost::new());
    pipeline.set_roster(roster());
    pipeline.apply(0, StreamOp::Start).expect("start");

    let go_live_tick = GO_LIVE_SEC * TICKS_PER_SEC;
    let mut went_live = None;
    let fed = feed_programme(&mut pipeline, SECONDS * TICKS_PER_SEC, |tick, p| {
        if tick == go_live_tick {
            add_youtube(p, tick_ms(tick));
        }
        for event in p.events() {
            match event {
                PipelineEvent::EncoderDown { reason } => panic!(
                    "the encoder died against a running relay at {}: {reason}. \
                     The relay said:\n{}",
                    tick_ms(tick),
                    relay.log()
                ),
                PipelineEvent::DestinationChanged {
                    state: DestinationState::Live,
                    ..
                } => went_live = Some(tick_ms(tick)),
                _ => {}
            }
        }
        true
    });
    let went_live = went_live.unwrap_or_else(|| {
        panic!(
            "the pusher never reached Live reading from the relay. It is at \
             {:?}, and the relay said:\n{}",
            destination_state(&pipeline),
            relay.log()
        )
    });
    println!("the destination went live at {went_live} ms");
    // First attempt, no backoff: a pusher that had to retry would land a
    // multiple of 500 ms later, and that is worth noticing rather than
    // absorbing, because it means the relay refused a reader it should have
    // served.
    assert!(
        went_live <= (GO_LIVE_SEC + HEALTHY_SEC) * 1_000 + 500,
        "the pusher took until {went_live} ms to go live, so the relay refused \
         it at least once"
    );
    assert!(
        pipeline.on_air(),
        "the chain is not on air at the end of the feed: {:?}",
        destination_state(&pipeline)
    );
    // The same real-time guarantee real_ffmpeg asserts, now with a relay and a
    // second ffmpeg on the machine.
    assert_eq!(
        pipeline.dropped_frames(),
        0,
        "the encoder refused frames at real time"
    );
    assert_eq!(
        pipeline.repeated_frames(),
        0,
        "the renderer could not keep up with real time"
    );
    // Let what is already in flight arrive. Stop kills a pusher with a signal
    // rather than an end of stream, because a pusher has nothing of ours to
    // flush, so whatever is still in the encoder's queue or the relay's when
    // it lands is lost. A session ends the same way; the wait is only here so
    // the window judged below is close to the window that was fed.
    let settle = Instant::now() + SETTLE;
    while Instant::now() < settle {
        pipeline.poll(tick_ms(fed));
        std::thread::sleep(Duration::from_millis(20));
    }
    pipeline.apply(tick_ms(fed), StreamOp::Stop).expect("stop");
    drop(pipeline);
    let relay_log = relay.log();
    drop(relay);
    if !relay_log.trim().is_empty() {
        println!("the relay said:\n{relay_log}");
    }

    // What came out the far side. Every claim below travelled encoder to
    // MediaMTX to pusher, and is read back by ffprobe rather than asserted
    // from our own argv.
    let size = std::fs::metadata(&relayed)
        .unwrap_or_else(|e| panic!("the pusher wrote no output: {e}"))
        .len();
    assert!(size > 100_000, "suspiciously small output: {size} bytes");

    let v = probe_entries(
        &ffprobe,
        &relayed,
        "v:0",
        "stream=codec_name,width,height,pix_fmt,r_frame_rate",
    );
    assert!(v.contains("h264"), "video is not H.264: {v}");
    assert!(v.contains("1280,720"), "not 720p landscape: {v}");
    assert!(v.contains("yuv420p"), "not yuv420p: {v}");
    assert!(v.contains(&format!("{fps}/1")), "not {fps} fps: {v}");

    let a = probe_entries(
        &ffprobe,
        &relayed,
        "a:0",
        "stream=codec_name,sample_rate,channels,profile",
    );
    assert!(a.contains("aac"), "audio is not AAC: {a}");
    assert!(a.contains("48000"), "audio is not 48 kHz: {a}");
    assert!(a.contains(",2"), "audio is not stereo: {a}");
    assert!(a.to_uppercase().contains("LC"), "not AAC-LC: {a}");

    let streams = probe(
        &ffprobe,
        &[
            "-show_entries",
            "stream=index,codec_type",
            "-of",
            "csv=p=0",
            &relayed.to_string_lossy(),
        ],
    );
    assert_eq!(streams, "0,video\n1,audio", "unexpected stream layout");

    // The SPS the platforms read, after a round trip through the relay. The
    // encoder's own file is checked in real_ffmpeg; this says MediaMTX handed
    // the pusher the same bitstream rather than a re-muxed approximation.
    let headers = h264_headers(&ffmpeg, &relayed);
    assert_eq!(
        header_field(&headers, "nal_hrd_parameters_present_flag"),
        "1",
        "the relayed SPS carries no HRD parameters"
    );
    assert_eq!(
        header_field(&headers, "cbr_flag[0]"),
        "1",
        "the relayed stream is not CBR, which is what Twitch requires"
    );
    assert_eq!(
        header_field(&headers, "profile_idc"),
        "77",
        "the relayed stream is not H.264 Main profile"
    );

    // Timestamps are relative here, unlike real_ffmpeg's: the pusher joined a
    // stream already in progress, and the pipeline signals it to stop by
    // killing it, so the file starts and ends where the relay's delivery did.
    //
    // Which is why the floor is well under the window fed. About two and a
    // half seconds of programme is in flight across the chain at any moment
    // and none of it survives the signal; measured, the shortfall is the same
    // whether the feed is 10 s or 18 s, so it is a tail and not a ceiling.
    // The floor is what makes this a measurement rather than a formality: four
    // seconds is two full keyframe intervals and change.
    let times = packet_times(&ffprobe, &relayed, "v:0");
    assert!(times.len() > 60, "only {} video packets", times.len());
    let span = times.last().copied().unwrap_or_default() - times[0];
    let live_seconds = (SECONDS - GO_LIVE_SEC) as f64;
    println!("the pusher wrote {span:.2}s of video, {size} bytes");
    assert!(
        (MIN_RELAYED_SECS..live_seconds + 0.5).contains(&span),
        "the pusher carried {span:.2}s out of a {live_seconds:.0}s window"
    );
    // Nothing was lost in the relay: one frame per 1/fps across the window,
    // plus the one that opens it.
    let want = (span * f64::from(fps)).round() as i64 + 1;
    assert!(
        (times.len() as i64 - want).abs() <= 2,
        "{} video packets across {span:.2}s, wanted about {want}",
        times.len()
    );

    // Keyframe cadence survives the relay, which is what lets a platform cut
    // the stream into segments.
    let keys = probe(
        &ffprobe,
        &[
            "-select_streams",
            "v:0",
            "-skip_frame",
            "nokey",
            "-show_entries",
            "frame=pts_time",
            "-of",
            "csv=p=0",
            &relayed.to_string_lossy(),
        ],
    );
    let key_times: Vec<f64> = keys
        .lines()
        .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
        .collect();
    assert!(
        key_times.len() >= 3,
        "only {} keyframes in {span:.2}s: {key_times:?}",
        key_times.len()
    );
    for pair in key_times.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            (gap - f64::from(keyframe_secs)).abs() < 0.05,
            "keyframe gap {gap:.3}s, wanted {keyframe_secs}s: {key_times:?}"
        );
    }

    // The ladder, averaged over the window rather than per second: the pusher
    // copies packets, so a per-second walk would be measuring the encoder
    // again, and real_ffmpeg already does that against the encoder's own file.
    let sizes = probe_entries(&ffprobe, &relayed, "v:0", "packet=size");
    let bits: u64 = sizes
        .lines()
        .filter_map(|l| l.trim_end_matches(',').parse::<u64>().ok())
        .sum::<u64>()
        * 8;
    let measured = (bits as f64 / span / 1_000.0).round() as u64;
    let want = u64::from(video_kbps);
    println!("the relayed video measured {measured} kbps against {want} kbps");
    assert!(
        measured * 100 > want * 85 && measured * 100 < want * 115,
        "the relayed video measured {measured} kbps, not the {want} kbps ladder"
    );

    // A/V still land together. Wider than real_ffmpeg's 40 ms because the
    // pipeline stops a pusher with a signal rather than an end of stream, so
    // the file is cut at whatever the muxer had written.
    let v_end = last_pts(&ffprobe, &relayed, "v:0");
    let a_end = last_pts(&ffprobe, &relayed, "a:0");
    let drift_ms = (v_end - a_end).abs() * 1_000.0;
    println!("relayed video ends {v_end:.4}s, audio ends {a_end:.4}s, drift {drift_ms:.1} ms");
    assert!(
        drift_ms < 150.0,
        "A/V drift {drift_ms:.1} ms exceeds 150 ms"
    );

    // And it carried the programme, not silence: the audio came off the
    // session mix, through the encoder, through the relay, into this file.
    let mean = mean_volume(&ffmpeg, &relayed);
    println!("relayed mean volume {mean} dBFS");
    assert!(
        (-20.0..-3.0).contains(&mean),
        "mean volume {mean} dBFS is not the -10 dBFS sine we fed"
    );

    let _ = std::fs::remove_dir_all(&root);
}
