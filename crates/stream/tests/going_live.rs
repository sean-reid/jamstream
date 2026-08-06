//! What it takes for a destination to be called Live, against real processes.
//!
//! The fake host proves the state machine and the unit tests hold the clock
//! still, which is exactly what the bug this guards was hiding behind. A pusher
//! is two execs and an ffmpeg startup ahead of its connect, so on a machine
//! busy with an encode it can outlive any settling window and still be short of
//! the refusal it is heading for. That used to be reported Live, with nothing
//! being broadcast, for as long as it took to die (#445).
//!
//! So the two children here are shell, and the timing is the point: one takes
//! its time and then fails, the other takes the same time and then reports a
//! push. What the pipeline says about them must follow the report and not the
//! clock.
//!
//! It runs the pipeline's own argv through the real launcher, which is the half
//! no fake can check: the file each pusher writes its report to is the one the
//! pipeline put in that argv, not this test's idea of where it should be. What
//! is still not proven here is that ffmpeg writes that report at all, which is
//! `relay_chain.rs`, against a real encoder, a real relay and a real pusher.
//!
//! No encoder needed, so this runs on any unix with nothing installed.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jamstream_protocol::control::{DestinationState, StreamKey, StreamOp, StreamPlatform};
use jamstream_protocol::ids::DestinationId;
use jamstream_stream::pipeline::{Pipeline, StreamConfig};
use jamstream_stream::proc::StdProcessHost;

/// One of ffmpeg's progress blocks, shared with the pipeline's own tests.
const PROGRESS_BLOCK: &str = include_str!("../testdata/pusher.progress");

/// How long a stand-in pusher takes before it does anything at all.
///
/// Longer than the three seconds of survival that used to promote a
/// destination, because the whole question is what happens in the gap between
/// surviving that long and having pushed anything.
const SLOW_MS: u64 = 4_000;

/// How long a test watches before it gives up on its child.
const WATCH: Duration = Duration::from_secs(20);

/// Session clock granularity here. The pipeline is polled from the mix tick in
/// production, and far more often than this; nothing depends on the rate.
const POLL: Duration = Duration::from_millis(10);

/// Both children, in one script. Neither encodes anything.
///
/// The encoder's part exists to open the video FIFO the host names in argv, so
/// the host's open handshake completes, and to drain the audio pipe, so closing
/// that pipe is an end of stream it exits on rather than a signal it waits for.
/// A pusher is the one told to write a progress file, and does whatever
/// `pusher` says with it.
fn write_stand_in(dir: &Path, pusher: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join("stand-in.sh");
    let body = format!(
        r#"#!/bin/sh
progress=
fifo=
prev=
for arg do
  [ "$prev" = -progress ] && progress=$arg
  [ -p "$arg" ] && fifo=$arg
  prev=$arg
done
if [ -n "$progress" ]; then
{pusher}
  exit 0
fi
cat "$fifo" >/dev/null &
exec cat >/dev/null
"#
    );
    std::fs::write(&script, body).expect("write the stand-in");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make the stand-in executable");
    script
}

/// A pipeline whose children are the stand-in, streaming to one destination.
fn rig(name: &str, pusher: &str) -> (PathBuf, Pipeline<StdProcessHost>) {
    let root = std::env::temp_dir().join(format!("jamstream-golive-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("work dir");
    let ffmpeg = write_stand_in(&root, pusher);
    let mut pipeline = Pipeline::new(
        StreamConfig {
            ffmpeg,
            work_dir: root.clone(),
            key_dir: root.join("keys"),
            ..StreamConfig::default()
        },
        StdProcessHost::new(),
    );
    pipeline.apply(0, StreamOp::Start).expect("start");
    pipeline
        .apply(
            0,
            StreamOp::AddDestination {
                id: DestinationId(1),
                platform: StreamPlatform::Twitch,
                // Shaped like a key and worth nothing. It reaches the stand-in
                // on stdin, through the real launcher, and goes no further.
                key: StreamKey::new("live_000000_notakey00"),
            },
        )
        .expect("add a destination");
    // Both children have to be up, or nothing below is about promotion at all.
    // The host gives an encoder five seconds to open the video pipe, and a
    // runner loaded enough that a shell takes longer than that to start fails
    // the spawn: the destination then carries the encoder's reason, at no
    // interesting moment, and every assertion after this would be describing
    // that instead.
    match state(&pipeline) {
        DestinationState::Connecting => {}
        other => panic!(
            "the stand-in children never came up on this machine, so nothing here was \
             watched: {other:?}"
        ),
    }
    (root, pipeline)
}

fn state(pipeline: &Pipeline<StdProcessHost>) -> DestinationState {
    pipeline
        .status()
        .first()
        .map(|d| d.state.clone())
        .expect("one destination")
}

/// The whole of #445, over fork and exec: a pusher that outlives the window
/// survival used to buy and only then reaches its refused connect must never
/// have been called Live.
#[test]
fn a_pusher_that_pushed_nothing_is_never_live_however_long_it_lives() {
    let (root, mut pipeline) = rig(
        "silent",
        &format!(
            "  sleep {slow}\n  \
             echo '[flv @ 0x1] Failed to connect to \
             rtmps://ingest.example/app/live_000000_notakey00: Connection refused' >&2\n  \
             exit 195",
            slow = SLOW_MS / 1_000
        ),
    );

    let started = Instant::now();
    let mut failed = None;
    while started.elapsed() < WATCH && failed.is_none() {
        let now_ms = started.elapsed().as_millis() as u64;
        pipeline.poll(now_ms);
        match state(&pipeline) {
            DestinationState::Connecting => {}
            DestinationState::Failed { reason } => failed = Some((now_ms, reason)),
            other => panic!(
                "the destination is {other:?} at {now_ms} ms with nothing pushed, so a \
                 broadcast nobody is receiving is indistinguishable from a working one"
            ),
        }
        assert!(
            !pipeline.on_air(),
            "on air at {now_ms} ms with nothing pushed"
        );
        std::thread::sleep(POLL);
    }

    let (at_ms, reason) = failed.expect("the stand-in pusher never failed, so nothing was watched");
    println!("the pusher failed at {at_ms} ms and the host is told: {reason}");
    // The harness has to reproduce the case to be evidence of anything: a
    // pusher that died inside the old window says nothing about one that did
    // not.
    assert!(
        at_ms >= SLOW_MS,
        "the stand-in failed after {at_ms} ms, inside the window this test exists to \
         outlive, so it proves nothing"
    );
    assert!(reason.starts_with("push failed: "), "{reason}");
    assert!(reason.contains("Failed to connect"), "{reason}");
    // And what it printed still travels without the key that was in it.
    assert!(!reason.contains("notakey"), "{reason}");
    drop(pipeline);
    let _ = std::fs::remove_dir_all(&root);
}

/// The other direction, so the state follows the report rather than the clock:
/// the same slow pusher, reporting a push at the end of the same wait, is Live
/// promptly afterwards and not before.
#[test]
fn a_pusher_is_live_once_it_reports_a_push_and_not_before() {
    let (root, mut pipeline) = rig(
        "reports",
        // The block a real ffmpeg writes, from the same fixture the pipeline's
        // own tests use, written the way ffmpeg writes it: to the path the
        // pipeline named in this child's argv. Then it stays up, as a push
        // does, and `exec` so that killing it kills the wait rather than
        // leaving an orphan holding this child's stderr open.
        &format!(
            "  sleep {slow}\n  cat > \"$progress\" <<'REPORT'\n{PROGRESS_BLOCK}REPORT\n  \
             exec sleep 60",
            slow = SLOW_MS / 1_000
        ),
    );

    let started = Instant::now();
    let mut live_at = None;
    while started.elapsed() < WATCH && live_at.is_none() {
        let now_ms = started.elapsed().as_millis() as u64;
        pipeline.poll(now_ms);
        match state(&pipeline) {
            DestinationState::Connecting => {}
            DestinationState::Live => live_at = Some(now_ms),
            other => panic!("the destination is {other:?} at {now_ms} ms"),
        }
        std::thread::sleep(POLL);
    }

    let live_at = live_at.expect("a pusher that reported a push was never called Live");
    println!("the destination went live at {live_at} ms");
    assert!(
        live_at >= SLOW_MS,
        "the destination went live at {live_at} ms, before its pusher had reported \
         anything, so something other than the report promoted it"
    );
    // Promptly, though: a report is read on a throttle of a fifth of a second,
    // not on the slow cadence a live destination's file is emptied on. Measured
    // at about a tenth of a second, and the ceiling is loose because how soon a
    // loaded runner gets round to it is not what this measures.
    assert!(
        live_at < SLOW_MS + 5_000,
        "the destination took until {live_at} ms to notice a report written at {SLOW_MS} ms"
    );
    assert!(pipeline.on_air());
    // And the status the room is sent carries no key.
    let status = pipeline.status();
    assert!(!format!("{status:?}").contains("notakey"), "{status:?}");
    drop(pipeline);
    let _ = std::fs::remove_dir_all(&root);
}
