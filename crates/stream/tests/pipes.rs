//! The two encoder pipes must make progress independently.
//!
//! This is the regression gate for issue #248, and it needs no ffmpeg. The
//! encoder takes audio on stdin and whole 720p frames on a FIFO, both of them
//! twenty-odd times a pipe buffer. Feeding them from one thread with blocking
//! writes wedges the moment the child wants one while a write to the other is
//! in flight, and that is not a race: it happened on every run, against every
//! ffmpeg from 6.1 to 7.1.
//!
//! So the child here is not ffmpeg, it is an adversary in two lines of shell
//! that reads one pipe and refuses the other for ever. That is the worst case
//! ffmpeg's demuxer can present, held indefinitely instead of for a few hundred
//! milliseconds, and it reproduces on any unix with no encoder installed.
//!
//! What each test asserts is progress: the submissions come back, the pipe
//! being read receives every byte, and the pipe that is not costs bounded
//! memory and a counter rather than the session.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jamstream_stream::proc::{Feed, ProcSpec, ProcessHost, StdProcessHost, Stdin};

/// A 720p yuv420p frame, which is what makes this hard: 1382400 bytes against
/// a pipe that holds 65536.
const FRAME: usize = 1280 * 720 * 3 / 2;
/// One 2.5 ms tick of s16le stereo at 48 kHz.
const TICK: usize = 480;
/// Ticks between video frames, near enough 30 fps for these tests. The exact
/// cadence is [`jamstream_stream::VideoCadence`]'s job and is tested there.
const TICKS_PER_FRAME: u32 = 13;

/// Nothing here should take more than a few seconds, so anything past this is
/// the deadlock rather than a slow machine.
const DEADLINE: Duration = Duration::from_secs(90);

/// Aborts the test binary if `name` is still running at the deadline.
///
/// A thread and `abort`, not an elapsed-time check, because the failure this
/// guards is a blocking `write` on the calling thread: it never returns to look
/// at a clock, and `exit` would run atexit handlers on a parked thread. Without
/// this the regression is a job that hangs until the runner kills it, which is
/// how #248 cost 926 seconds and told nobody which test did it.
fn deadline(name: &'static str) {
    std::thread::spawn(move || {
        std::thread::sleep(DEADLINE);
        eprintln!(
            "{name} passed its {}s deadline, so a submission to one of the \
             encoder's pipes never came back. That is issue #248: the two \
             pipes are being fed in a way that lets the child block one \
             producer by refusing the other. Each pipe needs its own writer.",
            DEADLINE.as_secs()
        );
        std::process::abort();
    });
}

/// Sleeps until `tick` is due on a 2.5 ms session clock.
///
/// The pipeline is a real-time component and sheds frames when the child falls
/// behind the audio clock, so a test that pushes as fast as the CPU allows is
/// measuring the child's throughput and would drop frames on any machine. This
/// only ever waits, never hurries.
fn pace(started: Instant, tick: u32) {
    let due = Duration::from_micros(u64::from(tick) * 2_500);
    if let Some(wait) = due.checked_sub(started.elapsed()) {
        std::thread::sleep(wait);
    }
}

struct Rig {
    root: PathBuf,
    host: StdProcessHost,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let root =
            std::env::temp_dir().join(format!("jamstream-pipes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("work dir");
        Rig {
            root,
            host: StdProcessHost::new(),
        }
    }

    fn fifo(&self) -> PathBuf {
        self.root.join("video.raw")
    }

    /// Where a `wc -c` in the child leaves its count.
    fn tally(&self, which: &str) -> PathBuf {
        self.root.join(which)
    }

    /// Starts `/bin/sh -c script` with a FIFO for video and a pipe for audio,
    /// exactly as the encoder is started.
    fn start(&mut self, script: String) -> u64 {
        let spec = ProcSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_owned(), script],
            stdin: Stdin::Pipe,
            fifos: vec![self.fifo()],
            label: "adversary".to_owned(),
        };
        self.host.spawn(&spec).expect("the adversary starts")
    }

    /// Bytes the child counted on one pipe. Only valid after the kill, which
    /// is what closes our ends and lets `wc` print.
    fn counted(&self, which: &str) -> usize {
        let raw = std::fs::read_to_string(self.tally(which))
            .unwrap_or_else(|err| panic!("the child left no {which} count: {err}"));
        raw.trim()
            .parse()
            .unwrap_or_else(|_| panic!("{which} count is not a number: {raw:?}"))
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn frame(fill: u8) -> Vec<u8> {
    vec![fill; FRAME]
}

/// A child that drains audio and never touches video.
///
/// This is ffmpeg's demuxer having decided it wants audio, frozen there. Under
/// a single writer thread this hung on the second frame: the write blocked with
/// twenty pipe fulls to go, so the audio that would have released it was never
/// sent. The submissions are interleaved here in the order `push_tick` makes
/// them, which is the interleaving that used to hang.
///
/// Unpaced on purpose. There is nothing to pace against when the child reads no
/// video, and running flat out is what fills the queue quickly enough to prove
/// the cap is real.
#[test]
fn a_child_that_never_reads_video_still_gets_every_byte_of_audio() {
    deadline("a_child_that_never_reads_video_still_gets_every_byte_of_audio");
    let mut rig = Rig::new("videostall");
    // fd 3 holds the FIFO open, so our end opens and nothing ever reads it.
    let script = format!(
        "cat 3< {fifo} | wc -c > {audio}",
        fifo = quote(&rig.fifo()),
        audio = quote(&rig.tally("audio")),
    );
    let id = rig.start(script);

    // Two seconds of session: 800 ticks of audio and the frames due with them.
    let started = Instant::now();
    let mut audio_bytes = 0usize;
    let (mut queued, mut dropped) = (0u32, 0u32);
    for tick in 0..800u32 {
        let pcm = vec![(tick % 251) as u8; TICK];
        rig.host
            .write_stdin(id, &pcm)
            .expect("audio keeps flowing while video is stuck");
        audio_bytes += pcm.len();
        if tick % TICKS_PER_FRAME == 0 {
            match rig.host.write_fifo(id, 0, &frame(tick as u8)) {
                Ok(Feed::Queued) => queued += 1,
                Ok(Feed::Dropped) => dropped += 1,
                Err(err) => panic!("video submission failed: {err}"),
            }
        }
    }
    let elapsed = started.elapsed();
    println!("{queued} frames queued, {dropped} dropped, in {elapsed:?}");

    // The point of the test: it came back at all, and quickly.
    assert!(
        elapsed < Duration::from_secs(5),
        "feeding two seconds of session took {elapsed:?}, so a submission blocked"
    );
    // Backpressure is honest in both directions. Some frames got as far as the
    // queue and the pipe; the rest were refused rather than buffered, which is
    // what the drop counter in the status exists to report.
    assert!(queued > 0, "nothing reached the video pipe at all");
    assert!(
        dropped > 0,
        "the video queue took {queued} frames from a child that reads none of \
         them, so it has no cap"
    );
    assert_eq!(queued + dropped, 62, "every due frame was accounted for");

    // And the audio really arrived, all of it, rather than being shed to keep
    // things moving. Closing our end is what lets `wc` print.
    rig.host.kill(id);
    assert_eq!(
        rig.counted("audio"),
        audio_bytes,
        "the child was short of audio"
    );
}

/// The mirror: a child that drains video and never touches audio.
///
/// Video must keep flowing, and the audio backlog must be bounded. Audio is the
/// master clock so it is never dropped, which leaves one honest answer to a
/// child that has stopped reading it: report a broken feed and let the
/// supervisor restart the encode. That is what this asserts, including that it
/// happens on a timer rather than never.
#[test]
fn a_child_that_never_reads_audio_still_gets_every_frame() {
    deadline("a_child_that_never_reads_audio_still_gets_every_frame");
    let mut rig = Rig::new("audiostall");
    // fd 3 holds our audio pipe open and unread; stdin becomes the FIFO.
    let script = format!(
        "cat 3<&0 < {fifo} | wc -c > {video}",
        fifo = quote(&rig.fifo()),
        video = quote(&rig.tally("video")),
    );
    let id = rig.start(script);

    let started = Instant::now();
    let mut frames = 0usize;
    let mut audio_error = None;
    for tick in 0..8_000u32 {
        pace(started, tick);
        let pcm = vec![(tick % 251) as u8; TICK];
        if let Err(err) = rig.host.write_stdin(id, &pcm) {
            audio_error = Some(err);
            break;
        }
        if tick % TICKS_PER_FRAME == 0 {
            match rig.host.write_fifo(id, 0, &frame(tick as u8)) {
                Ok(Feed::Queued) => frames += 1,
                Ok(Feed::Dropped) => panic!("a frame was dropped by a child that reads them"),
                Err(err) => panic!("video submission failed: {err}"),
            }
        }
    }
    let elapsed = started.elapsed();
    let err = audio_error.expect(
        "a child that never read a byte of audio was fed twenty seconds of it \
         without complaint, so the audio queue has no bound",
    );
    println!("audio gave up after {elapsed:?}: {err}");

    // On a timer, and a generous one, rather than on the first submission that
    // does not fit: a runner that pauses must not take the encode down.
    assert!(
        elapsed > Duration::from_secs(4),
        "audio reported a broken feed after only {elapsed:?}, which a hiccup \
         would trip"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "audio took {elapsed:?} to notice a child that reads none of it"
    );
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");

    // Video was untouched by any of that: every frame submitted, none dropped,
    // all of them delivered.
    assert!(frames > 100, "only {frames} frames got through");
    rig.host.kill(id);
    assert_eq!(
        rig.counted("video"),
        frames * FRAME,
        "the child was short of video"
    );
}

/// A child reading both pipes at its own pace loses nothing.
///
/// The queues exist to absorb a child that is briefly behind, and this is the
/// case where dropping anything would be wrong: two `cat`s read on their own
/// schedule, but they do read, so every frame and every sample must arrive.
#[test]
fn a_child_that_reads_both_pipes_loses_nothing() {
    deadline("a_child_that_reads_both_pipes_loses_nothing");
    let mut rig = Rig::new("both");
    // One reader per pipe, neither in step with us. The audio reader needs an
    // explicit redirection: a background job in a non-interactive shell gets
    // /dev/null for stdin otherwise, and would count zero for ever.
    let script = format!(
        "exec 3<&0\n\
         cat < {fifo} | wc -c > {video} &\n\
         cat <&3 | wc -c > {audio} &\n\
         wait\n",
        fifo = quote(&rig.fifo()),
        video = quote(&rig.tally("video")),
        audio = quote(&rig.tally("audio")),
    );
    let id = rig.start(script);

    let started = Instant::now();
    let mut audio_bytes = 0usize;
    let mut frames = 0usize;
    for tick in 0..800u32 {
        pace(started, tick);
        let pcm = vec![(tick % 251) as u8; TICK];
        rig.host.write_stdin(id, &pcm).expect("audio flows");
        audio_bytes += pcm.len();
        if tick % TICKS_PER_FRAME == 0 {
            let fed = rig
                .host
                .write_fifo(id, 0, &frame(tick as u8))
                .expect("video flows");
            assert_eq!(
                fed,
                Feed::Queued,
                "a frame was dropped by a child that keeps up"
            );
            frames += 1;
        }
    }
    rig.host.kill(id);

    assert_eq!(rig.counted("video"), frames * FRAME, "video was truncated");
    assert_eq!(rig.counted("audio"), audio_bytes, "audio was truncated");
}

/// The FIFO open handshake still fails loudly when the child dies at startup.
///
/// Opening the write end of a FIFO blocks until a reader appears, so a child
/// that exits immediately would hang the spawn for ever. It has to surface as a
/// spawn error, which is what the supervisor turns into a backoff and a reason
/// the host can read.
#[test]
fn a_child_that_dies_before_opening_the_fifo_is_a_spawn_error() {
    deadline("a_child_that_dies_before_opening_the_fifo_is_a_spawn_error");
    let mut rig = Rig::new("deadchild");
    let started = Instant::now();
    let spec = ProcSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_owned(), "exit 3".to_owned()],
        stdin: Stdin::Pipe,
        fifos: vec![rig.fifo()],
        label: "adversary".to_owned(),
    };
    let err = rig
        .host
        .spawn(&spec)
        .expect_err("a dead child cannot be fed");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the spawn hung"
    );
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
    // Nothing is left behind for the next attempt to trip over: no FIFO, and
    // no writer thread still holding one.
    assert!(!rig.fifo().exists(), "the FIFO outlived a failed spawn");
}
