//! Shared plumbing for the two suites that drive real media tools.
//!
//! Both of them ask ffprobe and `trace_headers` what the bitstream says
//! rather than asserting on the argv we passed, and both have to answer the
//! same question about a missing tool. [`require`] is the second one, in one
//! place: the reason the RTMP path went untested for so long is that a test
//! returned early on every OS and reported PASS, so a missing tool is a
//! FAILURE on Linux, which is where jamstreamd runs and where CI installs the
//! pinned builds, and a skip anywhere else.
//!
//! Each integration test binary compiles this module separately and uses part
//! of it, which is what the allow is for.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use jamstream_stream::TICK_STEREO_SAMPLES;
use jamstream_stream::pipeline::{Levels, Pipeline};
use jamstream_stream::proc::StdProcessHost;

/// Session ticks per second: one every 2.5 ms.
pub const TICKS_PER_SEC: u64 = 400;

/// Session time at the start of a tick.
pub fn tick_ms(tick: u64) -> u64 {
    tick * 5 / 2
}

/// Wall-clock ceiling for a suite that drives ffmpeg.
///
/// The work is a few seconds of programme fed at real time plus a handful of
/// probes, so this is not a budget for slow hardware. It guards the way these
/// tests fail badly, which is that they do not fail, they stop. Before #248
/// the pipeline fed two blocking pipes from one thread and wedged against
/// ffmpeg with no timer on either side; it sat there for 926 seconds before a
/// human killed it, longer than the CI job is allowed to live, so the job died
/// with no idea which test hung it. ffmpeg 8 is not immunity either: the same
/// signature turned up there under coverage instrumentation, which only widens
/// the window.
const DEADLINE_SECS: u64 = 240;

/// Aborts the test binary if the deadline passes, printing `note`.
///
/// A thread rather than an elapsed-time check, because the failure it exists
/// for is a blocking syscall on the main thread, which never comes back to
/// look at a clock. `abort` rather than `exit` for the same reason: `exit`
/// runs atexit handlers on a process whose main thread is parked holding
/// whatever it holds.
///
/// Raise it with `JAMSTREAM_FFMPEG_DEADLINE_SECS` if a runner ever needs more.
pub fn arm_deadline(test: &'static str, note: &'static str) {
    let secs = std::env::var("JAMSTREAM_FFMPEG_DEADLINE_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(DEADLINE_SECS);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        eprintln!("{test} exceeded its {secs}s deadline and is being aborted. {note}");
        std::process::abort();
    });
}

/// Feeds `ticks` of programme into a pipeline and polls it, once per tick.
///
/// A 440 Hz sine at -10 dBFS, so the output is provably not silence, plus
/// moving meters so the renderer does real per-frame work.
///
/// Fed at the rate a session feeds it. The pipeline is a real-time component:
/// it sheds video frames and declares a broken feed when the encoder falls
/// behind the audio clock, so a test that pushes its programme as fast as the
/// CPU allows measures the encoder's throughput and nothing else. The pacing
/// only ever waits, never hurries, so a slow runner just gets a shorter wait
/// and the same assertions.
///
/// `watch` sees each tick after the poll and stops the feed by returning
/// false. Returns the number of ticks actually fed.
pub fn feed_programme(
    pipeline: &mut Pipeline<StdProcessHost>,
    ticks: u64,
    mut watch: impl FnMut(u64, &mut Pipeline<StdProcessHost>) -> bool,
) -> u64 {
    let started = Instant::now();
    let mut sample = 0u64;
    for tick in 0..ticks {
        let mut audio = [0.0f32; TICK_STEREO_SAMPLES];
        for pair in audio.chunks_exact_mut(2) {
            let t = sample as f64 / f64::from(jamstream_stream::SAMPLE_RATE);
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
        let now_ms = tick_ms(tick);
        pipeline.push_tick(now_ms, &audio, &levels);
        pipeline.poll(now_ms);
        if !watch(tick, pipeline) {
            println!(
                "fed {} ticks in {:?}, then stopped",
                tick + 1,
                started.elapsed()
            );
            return tick + 1;
        }
    }
    println!("fed {ticks} ticks of programme in {:?}", started.elapsed());
    ticks
}

pub fn tool(name: &str) -> Option<PathBuf> {
    let out = Command::new("which").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Resolves every tool a suite needs, or explains the absence.
///
/// `None` means skip, and it is only ever returned off Linux with
/// `JAMSTREAM_REQUIRE_FFMPEG` unset, so a laptop without the tools can run the
/// rest of the workspace. On Linux a missing tool is a panic: the job that
/// exists to run these tests is then not running them, and saying so beats
/// reporting a pass. `covers` completes the sentence "nothing here checked".
pub fn require(test: &str, names: &[&str], covers: &str) -> Option<Vec<PathBuf>> {
    let found: Vec<Option<PathBuf>> = names.iter().map(|n| tool(n)).collect();
    let missing: Vec<&str> = names
        .iter()
        .zip(&found)
        .filter(|(_, path)| path.is_none())
        .map(|(name, _)| *name)
        .collect();
    if missing.is_empty() {
        return Some(found.into_iter().flatten().collect());
    }
    let missing = missing.join(", ");
    assert!(
        !cfg!(target_os = "linux") && std::env::var_os("JAMSTREAM_REQUIRE_FFMPEG").is_none(),
        "{missing} not on PATH, so nothing in {test} checked {covers}. Install \
         the pinned builds with scripts/install-pinned-ffmpeg.sh and \
         scripts/install-pinned-mediamtx.sh, or, on a machine that will never \
         have them, unset JAMSTREAM_REQUIRE_FFMPEG and run off Linux."
    );
    eprintln!("SKIP {test}: {missing} not on PATH, so {covers} went unchecked.");
    None
}

pub fn probe(ffprobe: &Path, args: &[&str]) -> String {
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

/// One `ffprobe -show_entries` query over a file, as a comma-separated row.
pub fn probe_entries(ffprobe: &Path, file: &Path, select: &str, entries: &str) -> String {
    probe(
        ffprobe,
        &[
            "-select_streams",
            select,
            "-show_entries",
            entries,
            "-of",
            "csv=p=0",
            &file.to_string_lossy(),
        ],
    )
}

/// Every H.264 header field the encoder actually wrote, as `trace_headers`
/// dumps them. This is the only honest way to check `nal-hrd=cbr`: it lives in
/// the SPS VUI, ffprobe does not surface it, and file size only ever hints at
/// it. Trace goes to stderr, so the log is the return value.
pub fn h264_headers(ffmpeg: &Path, file: &Path) -> String {
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
pub fn header_field(log: &str, name: &str) -> String {
    log.lines()
        .find(|l| l.contains(name))
        .and_then(|l| l.rsplit('=').next())
        .map(|v| v.trim().to_owned())
        .unwrap_or_else(|| panic!("no {name} in the H.264 headers"))
}

/// Every packet timestamp in one stream, in seconds, in file order.
pub fn packet_times(ffprobe: &Path, file: &Path, stream: &str) -> Vec<f64> {
    probe_entries(ffprobe, file, stream, "packet=pts_time")
        .lines()
        .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
        .collect()
}

/// Highest packet timestamp in one stream, in seconds.
pub fn last_pts(ffprobe: &Path, file: &Path, stream: &str) -> f64 {
    packet_times(ffprobe, file, stream)
        .into_iter()
        .fold(f64::MIN, f64::max)
}

/// Mean volume of a file's audio in dBFS, as ffmpeg's `volumedetect` measures
/// it. Proof that a stream carried the programme rather than silence.
pub fn mean_volume(ffmpeg: &Path, file: &Path) -> f64 {
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(file)
        .args(["-af", "volumedetect", "-f", "null", "-"])
        .output()
        .expect("volumedetect runs");
    let log = String::from_utf8_lossy(&out.stderr);
    log.lines()
        .find_map(|l| l.split("mean_volume:").nth(1))
        .and_then(|s| s.trim().split(' ').next())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or_else(|| panic!("volumedetect gave no mean_volume:\n{log}"))
}
