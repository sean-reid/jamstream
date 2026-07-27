//! End to end through a real ffmpeg, with ffprobe as the judge.
//!
//! The relay is skipped: the encoder writes an FLV file instead of publishing
//! to MediaMTX, which is the same muxer and the same encode ladder, just a
//! local sink. What this proves is the part that no fake can: that the argv we
//! build produces a stream the platforms would accept (H.264, AAC-LC at
//! 48 kHz, keyframes exactly 2 s apart), and that the audio-mastered cadence
//! keeps A/V drift far under a frame.
//!
//! Skipped with a message when ffmpeg or ffprobe is missing, rather than
//! failing: the pipeline's logic is covered by the fake-host unit tests, and
//! CI runs this job on Linux where the pinned build is present.

use std::path::{Path, PathBuf};
use std::process::Command;

use jamstream_protocol::control::StreamOp;
use jamstream_protocol::ids::MemberId;
use jamstream_stream::pipeline::{Levels, Pipeline, Roster, StreamConfig, StreamMember};
use jamstream_stream::proc::StdProcessHost;

/// Seconds of program material to encode.
const SECONDS: u64 = 5;
/// Session ticks per second.
const TICKS_PER_SEC: u64 = 400;
const TICK_STEREO: usize = 240;

fn tool(name: &str) -> Option<PathBuf> {
    let out = Command::new("which").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn probe(ffprobe: &Path, args: &[&str]) -> String {
    let out = Command::new(ffprobe)
        .args(["-v", "error"])
        .args(args)
        .output()
        .expect("ffprobe runs");
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Highest packet timestamp in one stream, in seconds.
fn last_pts(ffprobe: &Path, file: &Path, stream: &str) -> f64 {
    let raw = probe(
        ffprobe,
        &[
            "-select_streams",
            stream,
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
            &file.to_string_lossy(),
        ],
    );
    raw.lines()
        .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
        .fold(f64::MIN, f64::max)
}

#[test]
fn real_ffmpeg_produces_a_stream_the_platforms_would_accept() {
    if !cfg!(unix) {
        eprintln!(
            "SKIP real_ffmpeg_produces_a_stream_the_platforms_would_accept: \
             the pipeline feeds video through a named pipe, which is a unix \
             thing. jamstreamd runs on Linux."
        );
        return;
    }
    let (Some(ffmpeg), Some(ffprobe)) = (tool("ffmpeg"), tool("ffprobe")) else {
        eprintln!(
            "SKIP real_ffmpeg_produces_a_stream_the_platforms_would_accept: \
             ffmpeg and ffprobe are not on PATH. The pipeline's supervision, \
             cadence, and key handling are covered by the fake-host unit \
             tests; this test needs the real encoder."
        );
        return;
    };

    let root = std::env::temp_dir().join(format!("jamstream-realffmpeg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("work dir");
    let out_file = root.join("broadcast.flv");

    let mut cfg = StreamConfig::new("Real Encoder Test");
    cfg.ffmpeg = ffmpeg.clone();
    cfg.work_dir = root.clone();
    cfg.key_dir = root.join("keys");
    cfg.encoder_output = out_file.to_string_lossy().into_owned();

    let (fps, keyframe_secs, total_kbps) = (cfg.fps, cfg.keyframe_secs, cfg.total_kbps());
    let mut pipeline = Pipeline::new(cfg, StdProcessHost::new());
    pipeline
        .apply(0, StreamOp::Start)
        .expect("start the encoder");
    pipeline.set_roster(Roster {
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
    });

    // A 440 Hz sine at -10 dBFS, so the output is provably not silence, plus
    // moving meters so the renderer does real per-frame work.
    let total_ticks = SECONDS * TICKS_PER_SEC;
    let mut sample = 0u64;
    for tick in 0..total_ticks {
        let mut audio = [0.0f32; TICK_STEREO];
        for pair in audio.chunks_exact_mut(2) {
            let t = sample as f64 / 48_000.0;
            let v = (t * 440.0 * std::f64::consts::TAU).sin() as f32 * 0.316;
            pair[0] = v;
            pair[1] = v;
            sample += 1;
        }
        let mut levels = Levels::default();
        let phase = (tick % 200) as f32 / 200.0;
        levels.push(0.2 + 0.5 * phase, 0.1 + 0.3 * phase);
        levels.push(0.6 - 0.4 * phase, 0.3 - 0.2 * phase);
        let now_ms = tick * 5 / 2;
        pipeline.push_tick(now_ms, &audio, &levels);
        pipeline.poll(now_ms);
    }
    // Stop closes our write ends, so ffmpeg drains and finishes the file.
    pipeline
        .apply(total_ticks * 5 / 2, StreamOp::Stop)
        .expect("stop");
    drop(pipeline);

    let size = std::fs::metadata(&out_file).expect("output exists").len();
    assert!(size > 100_000, "suspiciously small output: {size} bytes");
    // CBR means the near-static stage still fills the pipe: x264 pads with
    // filler NALs. Measured rate should land near nominal, which is the only
    // black-box evidence that nal-hrd=cbr took effect.
    let measured_kbps = size * 8 / (SECONDS * 1_000);
    let nominal = u64::from(total_kbps);
    println!("measured {measured_kbps} kbps against a nominal {nominal} kbps");
    assert!(
        measured_kbps * 10 > nominal * 7 && measured_kbps < nominal * 2,
        "measured {measured_kbps} kbps is nowhere near the {nominal} kbps ladder"
    );

    // Codecs and geometry.
    let v = probe(
        &ffprobe,
        &[
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,width,height,pix_fmt,r_frame_rate",
            "-of",
            "csv=p=0",
            &out_file.to_string_lossy(),
        ],
    );
    assert!(v.contains("h264"), "video is not H.264: {v}");
    assert!(v.contains("1280,720"), "not 720p landscape: {v}");
    assert!(v.contains("yuv420p"), "not yuv420p: {v}");
    assert!(v.contains(&format!("{fps}/1")), "not {fps} fps: {v}");

    let a = probe(
        &ffprobe,
        &[
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name,sample_rate,channels,profile",
            "-of",
            "csv=p=0",
            &out_file.to_string_lossy(),
        ],
    );
    assert!(a.contains("aac"), "audio is not AAC: {a}");
    assert!(a.contains("48000"), "audio is not 48 kHz: {a}");
    assert!(a.contains(",2"), "audio is not stereo: {a}");
    assert!(a.to_uppercase().contains("LC"), "not AAC-LC: {a}");

    // Keyframes exactly every 2 s. Both platforms require it, and x264 only
    // obeys with keyint, min-keyint, and scenecut pinned together.
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
            &out_file.to_string_lossy(),
        ],
    );
    let key_times: Vec<f64> = keys
        .lines()
        .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
        .collect();
    assert!(
        key_times.len() >= SECONDS as usize / keyframe_secs as usize,
        "too few keyframes: {key_times:?}"
    );
    // The first keyframe sits a few tens of milliseconds in: FLV timestamps
    // start at the first packet, and the AAC encoder's priming shifts the
    // whole timeline by one audio frame. What matters is the spacing.
    assert!(
        key_times[0] < 0.1,
        "stream does not open on a keyframe: {key_times:?}"
    );
    for pair in key_times.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            (gap - f64::from(keyframe_secs)).abs() < 0.05,
            "keyframe gap {gap:.3}s, wanted {keyframe_secs}s: {key_times:?}"
        );
    }

    // A/V alignment: the two streams must end together. This is the property
    // the audio-mastered cadence exists to guarantee.
    let v_end = last_pts(&ffprobe, &out_file, "v:0");
    let a_end = last_pts(&ffprobe, &out_file, "a:0");
    let drift_ms = (v_end - a_end).abs() * 1_000.0;
    println!("video ends {v_end:.4}s, audio ends {a_end:.4}s, drift {drift_ms:.1} ms");
    assert!(drift_ms < 40.0, "A/V drift {drift_ms:.1} ms exceeds 40 ms");
    assert!(
        (v_end - SECONDS as f64).abs() < 0.2,
        "clip is {v_end:.3}s, wanted {SECONDS}s"
    );

    // The audio really carried our sine, rather than a stream of silence.
    let vol = Command::new(&ffmpeg)
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(&out_file)
        .args(["-af", "volumedetect", "-f", "null", "-"])
        .output()
        .expect("volumedetect runs");
    let log = String::from_utf8_lossy(&vol.stderr);
    let mean = log
        .lines()
        .find_map(|l| l.split("mean_volume:").nth(1))
        .and_then(|s| s.trim().split(' ').next())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or_else(|| panic!("volumedetect gave no mean_volume:\n{log}"));
    assert!(
        (-20.0..-3.0).contains(&mean),
        "mean volume {mean} dBFS is not the -10 dBFS sine we fed"
    );

    let _ = std::fs::remove_dir_all(&root);
}
