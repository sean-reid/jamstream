//! End to end through a real ffmpeg, with ffprobe as the judge.
//!
//! The relay is skipped: the encoder writes an FLV file instead of publishing
//! to MediaMTX, which is the same muxer and the same encode ladder, just a
//! local sink. What this proves is the part that no fake can: that the argv we
//! build produces a stream the platforms would accept, and that the
//! audio-mastered cadence keeps A/V drift far under a frame.
//!
//! Every claim is read back out of the file by a second implementation. The
//! 36 fake-host unit tests assert on the argv we passed, which says nothing
//! about what ffmpeg did with it: an option it silently ignored, a profile
//! x264 quietly downgraded, or a keyframe interval scenecut overrode all look
//! identical from the argv side. So the checks here are ffprobe on the streams
//! and `trace_headers` on the H.264 SPS, never our own opinion of ourselves.
//!
//! Skipped with a message when ffmpeg or ffprobe is missing, rather than
//! failing: the pipeline's logic is covered by the fake-host unit tests, and
//! CI runs this job on Linux where the pinned build is present.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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

/// Every H.264 header field the encoder actually wrote, as `trace_headers`
/// dumps them. This is the only honest way to check `nal-hrd=cbr`: it lives in
/// the SPS VUI, ffprobe does not surface it, and file size only ever hints at
/// it. Trace goes to stderr, so the log is the return value.
fn h264_headers(ffmpeg: &Path, file: &Path) -> String {
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-loglevel", "trace", "-i"])
        .arg(file)
        .args([
            "-c",
            "copy",
            "-bsf:v",
            "trace_headers",
            "-t",
            "0.2",
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("trace_headers runs");
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        log.contains("seq_parameter_set"),
        "trace_headers produced no SPS, so this build cannot answer the \
         question. Its output was:\n{log}"
    );
    log
}

/// One `trace_headers` field's decoded value.
fn header_field(log: &str, name: &str) -> String {
    log.lines()
        .find(|l| l.contains(name))
        .and_then(|l| l.rsplit('=').next())
        .map(|v| v.trim().to_owned())
        .unwrap_or_else(|| panic!("no {name} in the H.264 headers"))
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

    let (fps, keyframe_secs) = (cfg.fps, cfg.keyframe_secs);
    let (video_kbps, total_kbps) = (cfg.video_kbps, cfg.total_kbps());
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
    //
    // Fed at the rate a session feeds it. The pipeline is a real-time
    // component: it sheds video frames and declares a broken feed when the
    // encoder falls behind the audio clock, so a test that pushes five
    // seconds of programme as fast as the CPU allows measures the encoder's
    // throughput and nothing else. The pacing only ever waits, never hurries,
    // so a slow runner just gets a shorter wait and the same assertions.
    let total_ticks = SECONDS * TICKS_PER_SEC;
    let started = Instant::now();
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
        let due = Duration::from_micros(tick * 2_500);
        if let Some(wait) = due.checked_sub(started.elapsed()) {
            std::thread::sleep(wait);
        }
        let now_ms = tick * 5 / 2;
        pipeline.push_tick(now_ms, &audio, &levels);
        pipeline.poll(now_ms);
    }
    println!("fed {SECONDS}s of programme in {:?}", started.elapsed());
    // Nothing may be dropped at real time on any machine that can encode at
    // all: a drop here is either the renderer or the encoder failing to keep
    // up with 30 fps of 720p, which is the thing a session VM is sized for.
    assert_eq!(
        pipeline.dropped_frames(),
        0,
        "the encoder could not keep up with real time"
    );
    // Stop closes our write ends, so ffmpeg drains and finishes the file.
    pipeline
        .apply(total_ticks * 5 / 2, StreamOp::Stop)
        .expect("stop");
    drop(pipeline);

    let size = std::fs::metadata(&out_file).expect("output exists").len();
    assert!(size > 100_000, "suspiciously small output: {size} bytes");
    // CBR means the near-static stage still fills the pipe: x264 pads with
    // filler NALs, so the whole clip lands on the ladder rather than under it.
    let measured_kbps = size * 8 / (SECONDS * 1_000);
    let nominal = u64::from(total_kbps);
    println!("measured {measured_kbps} kbps against a nominal {nominal} kbps");
    assert!(
        measured_kbps * 100 > nominal * 85 && measured_kbps * 100 < nominal * 115,
        "measured {measured_kbps} kbps is not the {nominal} kbps ladder"
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

    // Every frame the cadence emitted is in the file. The drop counter above
    // says we handed them all over; this says ffmpeg encoded them all, which is
    // the difference between a five second clip and a five second clip that is
    // secretly missing a fifth of its frames.
    let frames: u64 = probe(
        &ffprobe,
        &[
            "-select_streams",
            "v:0",
            "-count_packets",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "csv=p=0",
            &out_file.to_string_lossy(),
        ],
    )
    .trim_end_matches(',')
    .parse()
    .expect("a packet count");
    // The startup frame at PTS 0 plus one per 1/fps of audio.
    let want_frames = u64::from(fps) * SECONDS + 1;
    assert_eq!(frames, want_frames, "video is missing frames");

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

    // Exactly two streams, in the order a platform expects to find them. A
    // stray third stream, or audio first, is a valid file that Twitch rejects.
    let streams = probe(
        &ffprobe,
        &[
            "-show_entries",
            "stream=index,codec_type",
            "-of",
            "csv=p=0",
            &out_file.to_string_lossy(),
        ],
    );
    assert_eq!(streams, "0,video\n1,audio", "unexpected stream layout");

    // What the bitstream itself says, rather than what we asked for.
    //
    // `nal-hrd=cbr` is the one platform requirement with no visible
    // consequence in a probe of the streams: it writes HRD parameters into the
    // SPS VUI with cbr_flag set, and Twitch reads them (platform.rs:164).
    // Nothing before this ever checked it. Profile comes from the same place
    // because x264 silently downgrades a profile it cannot honour, so `-profile
    // main` in argv is not evidence that the file is Main.
    let headers = h264_headers(&ffmpeg, &out_file);
    assert_eq!(
        header_field(&headers, "nal_hrd_parameters_present_flag"),
        "1",
        "the SPS carries no HRD parameters, so nal-hrd=cbr did not take effect"
    );
    assert_eq!(
        header_field(&headers, "cbr_flag[0]"),
        "1",
        "HRD is present but not CBR, which is what Twitch requires"
    );
    // 77 is Main, which both platforms accept and which the ladder asks for.
    assert_eq!(
        header_field(&headers, "profile_idc"),
        "77",
        "not H.264 Main profile"
    );
    assert_eq!(
        header_field(&headers, "frame_mbs_only_flag"),
        "1",
        "the stream is not progressive"
    );

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
    // An exact count, not a floor. A floor passes on a stream that is all
    // keyframes, which is a different bug with the same symptom.
    let gop = u64::from(fps * keyframe_secs);
    let want_keys = (frames - 1) / gop + 1;
    assert_eq!(
        key_times.len() as u64,
        want_keys,
        "wanted {want_keys} keyframes in {frames} frames: {key_times:?}"
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

    // CBR is a claim about every second, not about the average, and an average
    // is what the file size measures: a stream that starves for two seconds and
    // overshoots for two averages out fine and still trips a platform's
    // bitrate guard. So walk one-second windows over the video packets.
    let packets = probe(
        &ffprobe,
        &[
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,size",
            "-of",
            "csv=p=0",
            &out_file.to_string_lossy(),
        ],
    );
    let mut window_kbits = vec![0u64; SECONDS as usize];
    for line in packets.lines() {
        let mut parts = line.split(',');
        let (Some(pts), Some(size)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(pts), Ok(size)) = (pts.parse::<f64>(), size.parse::<u64>()) else {
            continue;
        };
        let second = pts as usize;
        if second < window_kbits.len() {
            window_kbits[second] += size * 8 / 1_000;
        }
    }
    println!("video kbits per second: {window_kbits:?}");
    let want = u64::from(video_kbps);
    for (second, got) in window_kbits.iter().enumerate() {
        assert!(
            got * 100 > want * 80 && got * 100 < want * 125,
            "second {second} carried {got} kbit against a constant {want}: \
             {window_kbits:?}"
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
