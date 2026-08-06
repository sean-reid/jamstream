//! The encoder through a real ffmpeg, with ffprobe as the judge.
//!
//! The relay is skipped: the encoder writes an FLV file instead of publishing
//! to MediaMTX, which is the same muxer and the same encode ladder, just a
//! local sink. What this proves is the part that no fake can: that the argv we
//! build produces a stream the platforms would accept, and that the
//! audio-mastered cadence keeps A/V drift far under a frame. The hop it skips
//! is `relay_chain.rs`, which stands a real mediamtx up on the shipped config
//! and runs the encoder and a pusher through it.
//!
//! Every claim is read back out of the file by a second implementation. The
//! 36 fake-host unit tests assert on the argv we passed, which says nothing
//! about what ffmpeg did with it: an option it silently ignored, a profile
//! x264 quietly downgraded, or a keyframe interval scenecut overrode all look
//! identical from the argv side. So the checks here are ffprobe on the streams
//! and `trace_headers` on the H.264 SPS, never our own opinion of ourselves.
//!
//! Missing tools are a FAILURE on Linux, which is where jamstreamd runs and
//! where CI installs ffmpeg. This used to return early instead, with no
//! `#[ignore]` and no runner carrying ffmpeg, so the whole RTMP path reported
//! PASS on every OS in the matrix and nothing ever checked H.264, AAC-LC,
//! `nal-hrd=cbr`, the keyframe cadence or A/V drift. See `common::require`,
//! which is where that rule now lives for both suites.
//!
//! The whole file is unix-only: the pipeline hands video to ffmpeg through a
//! named pipe. On Windows this target compiles to nothing rather than to a test
//! that passes without testing.
#![cfg(unix)]

mod common;

use jamstream_protocol::control::StreamOp;
use jamstream_protocol::ids::MemberId;
use jamstream_stream::pipeline::{Pipeline, Roster, StreamConfig, StreamMember};
use jamstream_stream::proc::StdProcessHost;

use common::{
    TICKS_PER_SEC, arm_deadline, feed_programme, h264_headers, header_field, last_pts, mean_volume,
    probe, probe_entries, require, tick_ms,
};

/// Seconds of program material to encode.
const SECONDS: u64 = 5;

const WEDGE_NOTE: &str = "This is a deadlock, not slow encoding: something is \
                          feeding both of the encoder's pipes in a way that lets ffmpeg block one \
                          producer by refusing the other (issue #248). Confirm with `sample` or \
                          `gdb` on both processes; a `write` on the video fifo under push_tick, \
                          against an ffmpeg in `read`, is that bug. Anything ffmpeg printed \
                          follows.";

#[test]
fn real_ffmpeg_produces_a_stream_the_platforms_would_accept() {
    arm_deadline("real_ffmpeg", WEDGE_NOTE);
    let Some(tools) = require(
        "real_ffmpeg_produces_a_stream_the_platforms_would_accept",
        &["ffmpeg", "ffprobe"],
        "the encode ladder the platforms accept",
    ) else {
        return;
    };
    let (ffmpeg, ffprobe) = (tools[0].clone(), tools[1].clone());

    let root = std::env::temp_dir().join(format!("jamstream-realffmpeg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("work dir");
    let out_file = root.join("broadcast.flv");

    let cfg = StreamConfig {
        ffmpeg: ffmpeg.clone(),
        work_dir: root.clone(),
        key_dir: root.join("keys"),
        encoder_output: out_file.to_string_lossy().into_owned(),
        ..StreamConfig::default()
    };

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

    let total_ticks = SECONDS * TICKS_PER_SEC;
    feed_programme(&mut pipeline, total_ticks, |_, _| true);
    // Nothing may be dropped or repeated at real time on any machine that can
    // encode at all: either count here is the renderer or the encoder failing
    // to keep up with 30 fps of 720p, which is the thing a session VM is sized
    // for.
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
    // Stop closes our write ends, so ffmpeg drains and finishes the file.
    pipeline
        .apply(tick_ms(total_ticks), StreamOp::Stop)
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
    let v = probe_entries(
        &ffprobe,
        &out_file,
        "v:0",
        "stream=codec_name,width,height,pix_fmt,r_frame_rate",
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

    let a = probe_entries(
        &ffprobe,
        &out_file,
        "a:0",
        "stream=codec_name,sample_rate,channels,profile",
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
    let packets = probe_entries(&ffprobe, &out_file, "v:0", "packet=pts_time,size");
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
    let mean = mean_volume(&ffmpeg, &out_file);
    assert!(
        (-20.0..-3.0).contains(&mean),
        "mean volume {mean} dBFS is not the -10 dBFS sine we fed"
    );

    let _ = std::fs::remove_dir_all(&root);
}
