//! Process hosting behind a trait.
//!
//! The supervisor's interesting behaviour (backoff, isolation between
//! destinations, key handling, status derivation) is all about *when* it
//! spawns and kills things, not about `std::process`. So every process
//! interaction goes through [`ProcessHost`]: [`StdProcessHost`] is the real
//! adapter, [`fake::FakeProcessHost`] is a scriptable double with a call log
//! that tests assert against.
//!
//! The real adapter is not thin, and the reason is the one thing in this
//! crate that cannot be faked: feeding two pipes to one process without
//! deadlocking. See [`StdProcessHost`].

use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Handle for one spawned process, unique per host instance.
pub type ProcId = u64;

/// Where a child's stdin comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stdin {
    /// A pipe the pipeline writes to. The encoder takes its s16le audio here.
    Pipe,
    /// A file staged with mode 0600 holding one secret line. The host opens
    /// it and unlinks the path *before* the child runs, so the secret exists
    /// only as an inherited descriptor from that moment on. This is how a
    /// stream key reaches a pusher without ever being an argument.
    SecretFile(PathBuf),
    Null,
}

/// Everything needed to start one process. Deliberately plain data: a test
/// can assert on the whole spec, which is how "no key in argv" is checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub stdin: Stdin,
    /// Named pipes the host creates (mode 0600) before spawning and opens
    /// for writing after. Index 0 is the encoder's video input.
    pub fifos: Vec<PathBuf>,
    /// Short name for logs and the fake's call log: "encoder",
    /// "pusher:twitch:1".
    pub label: String,
    /// The one URL in this child's stderr that is ours and holds no secret:
    /// the loopback relay it publishes to or reads from. Named in the reason
    /// instead of redacted, so "could not reach the local relay" and "the
    /// platform refused us" stop being the same sentence. Matched whole;
    /// anything that is not exactly this is redacted as before.
    pub relay_url: Option<String>,
}

impl ProcSpec {
    /// True if `needle` appears anywhere a local user could read it: the
    /// program path or any argument. Used by the key tests, and cheap enough
    /// to keep as a debug assertion at spawn time.
    pub fn mentions(&self, needle: &str) -> bool {
        !needle.is_empty()
            && (self.program.to_string_lossy().contains(needle)
                || self.args.iter().any(|a| a.contains(needle)))
    }
}

/// Liveness as the supervisor sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Running,
    /// Gone, with a reason fit to show a musician. Never contains a key.
    Exited {
        reason: String,
    },
}

/// What became of one video frame handed to a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    /// Accepted, and it will reach the child whole and in order.
    Queued,
    /// The child is behind and the backlog is at its cap, so the frame was
    /// discarded. The caller counts it; the alternative is a queue that grows
    /// until the VM runs out of memory.
    Dropped,
}

/// Spawn, feed, observe, kill.
///
/// Submissions never block on the child: each pipe has its own writer, and a
/// backlog past its cap is a dropped frame or a broken feed, not a wait. See
/// [`StdProcessHost`] for why that is the only shape that works.
pub trait ProcessHost {
    fn spawn(&mut self, spec: &ProcSpec) -> io::Result<ProcId>;
    /// Audio is the master clock, so it is never dropped: a child that has
    /// stopped reading it is a broken feed and an error here.
    fn write_stdin(&mut self, id: ProcId, buf: &[u8]) -> io::Result<()>;
    /// One whole frame. Written all or not at all, never torn.
    fn write_fifo(&mut self, id: ProcId, index: usize, buf: &[u8]) -> io::Result<Feed>;
    /// Non-blocking liveness check. An unknown id reads as exited.
    fn poll(&mut self, id: ProcId) -> Exit;
    /// Kills and reaps. Idempotent; the id is invalid afterwards.
    fn kill(&mut self, id: ProcId);
}

// ---------------------------------------------------------------------------
// Real implementation
// ---------------------------------------------------------------------------

/// `std::process` plus the FIFO plumbing ffmpeg needs.
///
/// ## The two-input pitfall
///
/// The encoder reads two raw streams, and a process has exactly one stdin.
/// So audio goes to stdin (`-i pipe:0`) and video to a named FIFO named in
/// argv. Ordering then matters in a way that deadlocks if you get it wrong:
/// opening the write end of a FIFO blocks until a reader opens the read end,
/// and ffmpeg opens its inputs in argv order. Therefore
///
/// 1. create the FIFO (mode 0600),
/// 2. spawn ffmpeg, which blocks opening the FIFO for reading,
/// 3. only then open the FIFO for writing, which unblocks both sides.
///
/// Opening for writing before the spawn would block forever. We open with
/// `O_NONBLOCK` and retry so a child that dies at startup surfaces as a spawn
/// error instead of a hang, then clear `O_NONBLOCK` so frame writes are
/// all-or-nothing.
///
/// ## One writer thread per pipe, and why there is no other option
///
/// ffmpeg up to and including 7.x demuxes every input on one thread and
/// interleaves them by timestamp, so it reads whichever input is behind and
/// will not touch the other until that read returns. A 720p yuv420p frame is
/// 1382400 bytes and no pipe holds anything like that, so a frame is many pipe
/// fulls and a writer is parked inside one almost all the time.
///
/// How many is deliberately not a number anything here relies on. Linux gives
/// a pipe 65536 bytes; Darwin sizes them dynamically and falls back to 16384
/// when it cannot get the large buffer, so the same frame is 21 pipe fulls on
/// one machine and 84 on the next. A design that survives one figure and not
/// the other has the bug at a different threshold, which is exactly how this
/// one lasted as long as it did.
///
/// Feeding both pipes from one thread therefore deadlocks, and did: the
/// writer sat in the video FIFO waiting for a reader, ffmpeg sat in stdin
/// waiting for audio the same thread would have sent next, and neither moved
/// again (issue #248). It is structural. No pipe size, no write ordering and
/// no ffmpeg version fixes it: 8.x only hides it by demuxing each input on
/// its own thread.
///
/// So each pipe gets a thread and a bounded queue:
///
/// - a writer blocks in `write` on its own pipe and nothing else, holding no
///   lock and owning nothing another thread needs,
/// - the pipeline thread only ever appends to a queue under a mutex held for
///   a pointer swap, so it never waits on a child,
/// - ffmpeg waits for at most one pipe at a time, and that pipe's writer is
///   waiting for exactly that read.
///
/// No thread waits on two things, so the wait-for graph has no cycle and
/// cannot deadlock, whatever order ffmpeg chooses to read its inputs in.
///
/// Falling behind is bounded rather than buffered. Video past
/// [`VIDEO_QUEUE_BYTES`] is dropped and reported as [`Feed::Dropped`] for the
/// status to count. Audio is never dropped, because a hole in the master
/// clock is worse than a restart, so its queue is allowed past
/// [`AUDIO_QUEUE_BYTES`] and is bounded instead by [`STALL`]: either queue
/// over its cap for that long is a child that has stopped consuming, and a
/// broken feed the supervisor restarts.
///
/// ## Why a child's stderr is read rather than inherited
///
/// ffmpeg's one-line errors are the most useful thing in the log when a
/// platform refuses a key, and they used to be inherited straight to ours for
/// that reason. The trouble is what a refused connect prints: ffmpeg names its
/// output URL, and for a pusher the stream key is in that URL. So the one
/// message worth keeping was also the one message that put a key into journald
/// on the session VM and into the local provider's per-session log (#204).
///
/// Each child's stderr is a pipe now, read by a thread that redacts every URL
/// and logs what is left against the child's label. See [`redact`].
///
/// The log is also the wrong place for it to stop. A session VM's journal is
/// somewhere no host can reach, so the last redacted lines are kept per child
/// and become the reason [`ProcessHost::poll`] reports when it dies: the one
/// place the failure was visible used to be the one place the explanation was
/// not (#437).
#[derive(Debug, Default)]
pub struct StdProcessHost {
    next_id: ProcId,
    procs: BTreeMap<ProcId, Live>,
}

#[derive(Debug)]
struct Live {
    child: std::process::Child,
    stdin: Option<Feeder>,
    fifos: Vec<Feeder>,
    fifo_paths: Vec<PathBuf>,
    /// The thread draining this child's stderr. It ends at end of stream,
    /// which is when the child's write end closes, so it is joined after the
    /// child is reaped and never before.
    stderr: Option<std::thread::JoinHandle<()>>,
    /// The last redacted lines that thread saw, which is what a host is told
    /// when this child dies.
    tail: Arc<StderrTail>,
    /// We feed this process, so closing our write ends is an end-of-stream it
    /// can act on: give it a moment to flush before the signal. A pusher has
    /// nothing to flush and no reason to wait.
    drains_on_eof: bool,
}

/// How long a fed process gets to drain its backlog, notice EOF, and exit.
const DRAIN_MS: u64 = 3_000;

/// Audio the writer holds before it counts as behind: two seconds at 48 kHz
/// stereo s16le.
pub const AUDIO_QUEUE_BYTES: usize = 2 * crate::SAMPLE_RATE as usize * 2 * 2;

/// Video the writer may hold: 12 MiB, about eight 720p frames or a quarter
/// second. Deep enough to ride out a scheduler hiccup, shallow enough that a
/// real stall shows up as dropped frames in the status instead of as memory.
pub const VIDEO_QUEUE_BYTES: usize = 12 << 20;

/// A queue over its cap this long means the child has stopped reading, not
/// that it is briefly behind. Reported as a broken feed, so the supervisor
/// restarts the encode instead of dropping frames into a void forever.
///
/// It is also what bounds an [`Overflow::Keep`] queue: the producer is a
/// real-time audio clock, so the most it can pile up before this fires is
/// five seconds of audio, about a megabyte.
const STALL: Duration = Duration::from_secs(5);

impl StdProcessHost {
    pub fn new() -> Self {
        Self::default()
    }
}

/// What a queue does with a submission that puts it over its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overflow {
    /// Discard it and say so. Frames only: the count is in the status, and a
    /// broadcast one frame short beats a VM out of memory.
    Discard,
    /// Take it anyway. Audio, which is the master clock and cannot have holes
    /// punched in it; [`STALL`] is what stops the queue growing for ever.
    Keep,
}

/// One child pipe, its backlog, and the thread that drains the backlog into
/// it. Dropping it does nothing; [`Feeder::close`] then [`Feeder::finish`] is
/// the shutdown, in that order.
#[derive(Debug)]
struct Feeder {
    queue: Arc<Queue>,
    join: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug)]
struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
    budget: usize,
    overflow: Overflow,
    label: String,
}

#[derive(Debug, Default)]
struct QueueState {
    items: VecDeque<Vec<u8>>,
    bytes: usize,
    /// Nothing more will be submitted; the writer flushes and closes its end.
    closed: bool,
    /// The write that failed, reported to the producer on its next call.
    error: Option<String>,
    /// Since when submissions have been refused, cleared by any acceptance.
    over_since: Option<Instant>,
    /// Buffers the writer has finished with, so a 1.4 MB frame is a memcpy
    /// into a recycled allocation rather than a fresh one thirty times a
    /// second.
    spare: Vec<Vec<u8>>,
}

impl Feeder {
    /// Starts a writer thread owning `sink`. The thread closes `sink`, and so
    /// signals end of stream, exactly when it returns.
    fn start(
        sink: Box<dyn io::Write + Send>,
        budget: usize,
        overflow: Overflow,
        label: String,
    ) -> io::Result<Feeder> {
        let queue = Arc::new(Queue {
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
            budget,
            overflow,
            label: label.clone(),
        });
        let mine = Arc::clone(&queue);
        let join = std::thread::Builder::new()
            .name(format!("jamstream-feed-{label}"))
            .spawn(move || drain(&mine, sink))?;
        Ok(Feeder {
            queue,
            join: Some(join),
        })
    }

    /// Hands over one submission. Never blocks on the child.
    fn submit(&self, buf: &[u8]) -> io::Result<Feed> {
        self.queue.submit(buf)
    }

    /// No more submissions. The writer flushes what is queued, closes its end
    /// of the pipe, and exits.
    fn close(&self) {
        let mut state = self.queue.lock();
        state.closed = true;
        drop(state);
        self.queue.ready.notify_all();
    }

    /// True once the writer has emptied its queue and closed its end of the
    /// pipe, which is the only moment at which the child has really seen end
    /// of stream.
    fn finished(&self) -> bool {
        self.join.as_ref().is_none_or(|j| j.is_finished())
    }

    /// Joins the writer. Only call once the child is gone: a writer parked in
    /// `write` returns when the read end closes and not before.
    fn finish(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Queue {
    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        // A writer thread that panicked mid-frame has already recorded what
        // matters; a poisoned queue is no reason to take the session down.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn submit(&self, buf: &[u8]) -> io::Result<Feed> {
        let mut item = {
            let mut state = self.lock();
            if let Some(err) = &state.error {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, err.clone()));
            }
            // An empty queue always accepts, whatever the size: the cap bounds
            // the backlog, not one frame.
            if state.items.is_empty() || state.bytes + buf.len() <= self.budget {
                state.over_since = None;
            } else {
                let since = *state.over_since.get_or_insert_with(Instant::now);
                let waited = since.elapsed();
                if waited >= STALL {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "{}: {} bytes queued and unread for {:.1}s",
                            self.label,
                            state.bytes,
                            waited.as_secs_f32()
                        ),
                    ));
                }
                if self.overflow == Overflow::Discard {
                    return Ok(Feed::Dropped);
                }
            }
            state.spare.pop().unwrap_or_default()
        };
        // The copy happens outside the lock: a frame is 1.4 MB and the writer
        // wants the lock back between frames.
        item.clear();
        item.extend_from_slice(buf);
        let mut state = self.lock();
        state.bytes += item.len();
        state.items.push_back(item);
        drop(state);
        self.ready.notify_one();
        Ok(Feed::Queued)
    }
}

/// The writer thread: one submission at a time, whole, until the queue is
/// closed and empty or a write fails.
fn drain(queue: &Queue, mut sink: Box<dyn io::Write + Send>) {
    loop {
        let item = {
            let mut state = queue.lock();
            loop {
                if let Some(item) = state.items.pop_front() {
                    // Accounted as consumed here rather than after the write,
                    // so the producer can refill while this frame is in
                    // flight. That is the whole point of the thread.
                    state.bytes -= item.len();
                    break Some(item);
                }
                if state.closed {
                    break None;
                }
                state = queue.ready.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(item) = item else { break };
        let wrote = sink.write_all(&item);
        let mut state = queue.lock();
        state.spare.push(item);
        if let Err(err) = wrote {
            state.error = Some(format!("{}: {err}", queue.label));
            break;
        }
    }
    let _ = sink.flush();
    // Dropping the sink closes this end of the pipe, which is the end of
    // stream the child needs to finish its file.
}

/// What replaces the tail of a URL in a child's stderr.
const REDACTED: &str = "<redacted>";

/// What replaces a URL that is [`ProcSpec::relay_url`]: our own loopback
/// relay, which holds no secret and is the difference between a fault on our
/// side of the machine and a platform saying no.
const RELAY: &str = "<local relay>";

/// Longest stderr line kept, in bytes. ffmpeg at `-loglevel error` writes
/// short lines; anything longer is a child misbehaving, and the tail of it is
/// dropped rather than buffered without a bound.
const STDERR_LINE_CAP: usize = 2_048;

/// Redacted stderr lines kept per child, to quote when it dies.
///
/// Two, oldest first. ffmpeg reports the fault it hit and then what that
/// broke, so a refused connect is followed by "Could not write header:
/// Broken pipe": the first line is the diagnosis and the second is only
/// evidence the first one mattered.
const STDERR_TAIL_LINES: usize = 2;

/// The last few redacted lines a child wrote, shared with the thread reading
/// its stderr.
#[derive(Debug, Default)]
struct StderrTail {
    lines: Mutex<VecDeque<String>>,
}

impl StderrTail {
    fn push(&self, line: &str) {
        let mut lines = self.lock();
        if lines.len() == STDERR_TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line.to_owned());
    }

    /// What the child said, oldest first, or None if it said nothing.
    fn quote(&self) -> Option<String> {
        let lines = self.lock();
        if lines.is_empty() {
            return None;
        }
        Some(lines.iter().cloned().collect::<Vec<_>>().join("; "))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<String>> {
        self.lines.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Strips everything after `scheme://` in one line of a child's stderr.
///
/// The line is worth keeping and the URL in it is not: `Failed to connect to
/// rtmps://host/app/KEY: Connection refused` is the diagnosis, and `KEY` is a
/// stream key. Redacting the URL keeps the diagnosis.
///
/// Nothing after `://` survives, not even the host. A host is not a secret,
/// but `user:pass@host` and `?key=...` are, and a redactor that has to decide
/// which part of an authority is safe is a redactor with a bug waiting in it.
/// Nothing is lost either way: which destination this is comes from the label
/// the line is logged against, not from the URL.
///
/// One exception, and it is not a decision this function makes: a URL that
/// matches `relay` exactly is replaced by [`RELAY`]. That string is our own
/// loopback relay out of the pipeline's configuration, it never held a key,
/// and telling the two refusals apart is the whole point (#437). A URL that
/// is not character-for-character that string is redacted, so the exception
/// can never print anything the caller did not already know.
fn redact<'a>(line: &'a str, relay: Option<&str>) -> Cow<'a, str> {
    if !line.contains("://") {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("://") {
        let scheme_end = at + 3;
        // The URL runs to the next whitespace. Punctuation ffmpeg puts after
        // it goes too, since a port makes `:` no delimiter at all.
        let url_end = rest[scheme_end..]
            .find(char::is_whitespace)
            .map_or(rest.len(), |end| scheme_end + end);
        let scheme_start = scheme_start(&rest[..at]);
        let url =
            rest[scheme_start..url_end].trim_end_matches([':', ',', '.', ';', ')', '"', '\'']);
        if relay.is_some_and(|r| r == url) {
            out.push_str(&rest[..scheme_start]);
            out.push_str(RELAY);
        } else {
            // Up to and including the separator, so the scheme stays readable.
            out.push_str(&rest[..scheme_end]);
            out.push_str(REDACTED);
        }
        if url_end == rest.len() {
            return Cow::Owned(out);
        }
        rest = &rest[url_end..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Where the scheme of the URL ending at `head` begins, so the whole URL can
/// be compared against the relay's. A scheme is letters, digits, `+`, `-`
/// and `.`, so the first character outside that set ends the search.
fn scheme_start(head: &str) -> usize {
    head.rfind(|c: char| !c.is_ascii_alphanumeric() && c != '+' && c != '-' && c != '.')
        .map_or(0, |at| {
            at + head[at..].chars().next().map_or(1, char::len_utf8)
        })
}

/// Reads one line of at most [`STDERR_LINE_CAP`] bytes into `line`, without
/// the newline. `Ok(false)` is end of stream.
///
/// Bytes past the cap are discarded along with the rest of that line rather
/// than emitted as a line of their own. A cut in the middle of a URL would
/// otherwise hand the tail of it, key included, to [`redact`] with no scheme
/// left in it to recognise.
fn read_capped_line(reader: &mut impl io::BufRead, line: &mut Vec<u8>) -> io::Result<bool> {
    line.clear();
    let mut any = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(any);
        }
        any = true;
        let (chunk, consumed, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(at) => (&buf[..at], at + 1, true),
            None => (buf, buf.len(), false),
        };
        let room = STDERR_LINE_CAP.saturating_sub(line.len());
        line.extend_from_slice(&chunk[..room.min(chunk.len())]);
        reader.consume(consumed);
        if done {
            return Ok(true);
        }
    }
}

/// Drains a child's stderr, one redacted line at a time, until end of stream.
///
/// `emit` is where a line goes; the caller supplies it so a test can prove
/// what does and does not arrive there. `relay` is [`ProcSpec::relay_url`].
fn relay_stderr(mut reader: impl io::BufRead, relay: Option<&str>, mut emit: impl FnMut(&str)) {
    let mut line = Vec::with_capacity(256);
    loop {
        match read_capped_line(&mut reader, &mut line) {
            Ok(true) => {}
            Ok(false) => return,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            // A child whose stderr cannot be read is the supervisor's problem
            // to notice through its exit, not this thread's to report.
            Err(_) => return,
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        if text.trim().is_empty() {
            continue;
        }
        emit(redact(text, relay).as_ref());
    }
}

impl Drop for StdProcessHost {
    fn drop(&mut self) {
        let ids: Vec<ProcId> = self.procs.keys().copied().collect();
        for id in ids {
            self.kill(id);
        }
    }
}

impl ProcessHost for StdProcessHost {
    fn spawn(&mut self, spec: &ProcSpec) -> io::Result<ProcId> {
        use std::process::{Command, Stdio};

        for path in &spec.fifos {
            let _ = std::fs::remove_file(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            mkfifo_0600(path)?;
        }

        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args);
        cmd.stdout(Stdio::null());
        // Piped, not inherited: a refused connect names the output URL, and a
        // pusher's URL holds its stream key. See the note on [`StdProcessHost`].
        cmd.stderr(Stdio::piped());
        match &spec.stdin {
            Stdin::Pipe => {
                cmd.stdin(Stdio::piped());
            }
            Stdin::Null => {
                cmd.stdin(Stdio::null());
            }
            Stdin::SecretFile(path) => {
                let file = std::fs::File::open(path)?;
                // Unlink before the child can run: from here the secret is
                // an open descriptor and nothing else.
                let _ = std::fs::remove_file(path);
                cmd.stdin(Stdio::from(file));
            }
        }

        let mut child = cmd.spawn().inspect_err(|_| {
            for path in &spec.fifos {
                let _ = std::fs::remove_file(path);
            }
        })?;

        // Every pipe gets its own writer from here on. Anything that fails
        // while wiring them up takes the child and the FIFOs with it, so a
        // half-connected encoder is never handed back as running.
        let abandon = |child: &mut std::process::Child,
                       feeders: Vec<Feeder>,
                       stderr: Option<std::thread::JoinHandle<()>>| {
            let _ = child.kill();
            let _ = child.wait();
            // The child held the only write end, so the reader is at end of
            // stream by now and this join returns.
            if let Some(join) = stderr {
                let _ = join.join();
            }
            for feeder in feeders {
                feeder.close();
                feeder.finish();
            }
            for path in &spec.fifos {
                let _ = std::fs::remove_file(path);
            }
        };

        // Before any feeder, because a child with nobody reading its stderr
        // blocks in `write` once the pipe fills and stops encoding.
        let tail = Arc::new(StderrTail::default());
        let stderr = match child.stderr.take() {
            Some(pipe) => {
                let label = spec.label.clone();
                let relay = spec.relay_url.clone();
                let mine = Arc::clone(&tail);
                let started = std::thread::Builder::new()
                    .name(format!("jamstream-stderr-{}", spec.label))
                    .spawn(move || {
                        relay_stderr(io::BufReader::new(pipe), relay.as_deref(), |line| {
                            tracing::warn!(child = %label, "{line}");
                            mine.push(line);
                        });
                    });
                match started {
                    Ok(join) => Some(join),
                    Err(err) => {
                        abandon(&mut child, Vec::new(), None);
                        return Err(err);
                    }
                }
            }
            None => None,
        };

        let stdin = match child.stdin.take() {
            Some(pipe) => match Feeder::start(
                Box::new(pipe),
                AUDIO_QUEUE_BYTES,
                Overflow::Keep,
                format!("{}:audio", spec.label),
            ) {
                Ok(feeder) => Some(feeder),
                Err(err) => {
                    abandon(&mut child, Vec::new(), stderr);
                    return Err(err);
                }
            },
            None => None,
        };

        let mut fifos: Vec<Feeder> = Vec::with_capacity(spec.fifos.len());
        for (index, path) in spec.fifos.iter().enumerate() {
            let started = open_fifo_write(path).and_then(|file| {
                Feeder::start(
                    Box::new(file),
                    VIDEO_QUEUE_BYTES,
                    Overflow::Discard,
                    format!("{}:fifo{index}", spec.label),
                )
            });
            match started {
                Ok(feeder) => fifos.push(feeder),
                Err(err) => {
                    fifos.extend(stdin);
                    abandon(&mut child, fifos, stderr);
                    return Err(err);
                }
            }
        }

        let id = self.next_id;
        self.next_id += 1;
        let drains_on_eof = spec.stdin == Stdin::Pipe || !spec.fifos.is_empty();
        self.procs.insert(
            id,
            Live {
                child,
                stdin,
                fifos,
                fifo_paths: spec.fifos.clone(),
                stderr,
                tail,
                drains_on_eof,
            },
        );
        Ok(id)
    }

    fn write_stdin(&mut self, id: ProcId, buf: &[u8]) -> io::Result<()> {
        let live = self
            .procs
            .get_mut(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such process"))?;
        let stdin = live
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "stdin is not a pipe"))?;
        // Audio queues with Overflow::Keep, so this is Queued or an error.
        stdin.submit(buf).map(|_| ())
    }

    fn write_fifo(&mut self, id: ProcId, index: usize, buf: &[u8]) -> io::Result<Feed> {
        let live = self
            .procs
            .get_mut(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such process"))?;
        let fifo = live
            .fifos
            .get_mut(index)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such fifo"))?;
        fifo.submit(buf)
    }

    fn poll(&mut self, id: ProcId) -> Exit {
        let Some(live) = self.procs.get_mut(&id) else {
            return Exit::Exited {
                reason: "process handle is gone".to_owned(),
            };
        };
        match live.child.try_wait() {
            Ok(None) => Exit::Running,
            Ok(Some(status)) => {
                // The child is gone, so it held the last write end of its
                // stderr and the reader is at end of stream: this join is
                // bounded, and it is what makes the last line the child
                // wrote available here rather than a moment later. Without
                // it the reason is a race with a thread that has already
                // been handed the answer.
                if let Some(join) = live.stderr.take() {
                    let _ = join.join();
                }
                Exit::Exited {
                    // What the child said, if it said anything. An exit code
                    // is what is left when it did not: on its own it is a
                    // number whose meaning changes with the host OS, which
                    // is no use to the musician reading it (#437).
                    reason: live.tail.quote().unwrap_or_else(|| describe(&status)),
                }
            }
            Err(err) => Exit::Exited {
                reason: format!("wait failed: {err}"),
            },
        }
    }

    fn kill(&mut self, id: ProcId) {
        let Some(mut live) = self.procs.remove(&id) else {
            return;
        };
        // Closing the queues lets each writer finish its backlog and then drop
        // its end of the pipe, which is the end of stream a fed child needs to
        // flush its muxer and exit cleanly.
        let mut feeders: Vec<Feeder> = live.fifos.drain(..).collect();
        feeders.extend(live.stdin.take());
        for feeder in &feeders {
            feeder.close();
        }
        if live.drains_on_eof {
            let deadline = Instant::now() + Duration::from_millis(DRAIN_MS);
            // Our own backlog first. Closing a queue is not an end of stream
            // until the writer has sent what we already accepted, so waiting
            // on the child before that waits for the wrong thing and can cut a
            // frame we had promised to deliver in half.
            while Instant::now() < deadline && !feeders.iter().all(Feeder::finished) {
                std::thread::sleep(Duration::from_millis(5));
            }
            // Then the child's own flush, on what is left of the same budget,
            // so a wedged encoder costs one DRAIN_MS and not two.
            while Instant::now() < deadline {
                match live.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => break,
                }
            }
        }
        let _ = live.child.kill();
        let _ = live.child.wait();
        // The read ends are gone with the child, so a writer still parked in
        // `write` gets EPIPE now and returns. Joining is bounded from here,
        // and it is what guarantees no thread outlives the process it feeds.
        for feeder in feeders {
            feeder.finish();
        }
        // Same reasoning in the other direction: the child held the only write
        // end of its stderr, so the reader is at end of stream and returns.
        if let Some(join) = live.stderr.take() {
            let _ = join.join();
        }
        for path in &live.fifo_paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn describe(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(0) => "exited cleanly".to_owned(),
        Some(code) => match errno_behind(code) {
            Some(err) => format!("exited with status {code}: {err}"),
            None => format!("exited with status {code}"),
        },
        None => "killed by signal".to_owned(),
    }
}

/// The errno an ffmpeg exit code is hiding, when it is hiding one.
///
/// ffmpeg exits with an AVERROR, which for a system failure is a negative
/// errno, and an exit status keeps only the low eight bits: a connection
/// refused on Linux is -111 and arrives as 145. The number does not travel,
/// because the same refusal on macOS is 195, ECONNREFUSED being 61 there. So
/// the code is put back through the OS rather than printed as an integer that
/// means a different thing on the machine the reader is sitting at.
///
/// Two guards against dressing a plain exit code up as an error the OS never
/// meant. 255 is excluded: it is both -1 and the conventional "it went
/// wrong", and calling it EPERM would be a confident wrong answer. And the
/// candidate has to land on an [`io::ErrorKind`] worth naming, which is the
/// portable form of "the OS recognised it".
fn errno_behind(code: i32) -> Option<io::Error> {
    if !(129..=254).contains(&code) {
        return None;
    }
    let err = io::Error::from_raw_os_error(256 - code);
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::NetworkDown
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::AddrInUse
            | io::ErrorKind::TimedOut
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::NotFound
    )
    .then_some(err)
}

#[cfg(unix)]
fn mkfifo_0600(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "fifo path has a nul byte"))?;
    // SAFETY: c_path is a valid nul-terminated path for the duration.
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn mkfifo_0600(_path: &std::path::Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the broadcast pipeline needs named pipes; it runs on the Linux session VM",
    ))
}

#[cfg(unix)]
fn open_fifo_write(path: &std::path::Path) -> io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};

    /// The reader is ffmpeg starting up; a second is already generous.
    const DEADLINE: Duration = Duration::from_secs(5);

    let start = Instant::now();
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => {
                let fd = file.as_raw_fd();
                // SAFETY: fd is owned by `file` and open for the calls.
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                if flags < 0 {
                    return Err(io::Error::last_os_error());
                }
                if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                return Ok(file);
            }
            // ENXIO: no reader yet. The child is still starting up.
            Err(err) if err.raw_os_error() == Some(libc::ENXIO) => {
                if start.elapsed() >= DEADLINE {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "encoder never opened the video pipe",
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(not(unix))]
fn open_fifo_write(_path: &std::path::Path) -> io::Result<std::fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the broadcast pipeline needs named pipes; it runs on the Linux session VM",
    ))
}

// ---------------------------------------------------------------------------
// Fake implementation
// ---------------------------------------------------------------------------

/// A scriptable [`ProcessHost`] with a call log; it spawns nothing.
///
/// cfg(test), because the only callers are this crate's own unit tests. It
/// used to be compiled unconditionally for integration tests in `tests/`, and
/// the two that exist there drive real processes, so all that reached was
/// jamstreamd.
#[cfg(test)]
pub mod fake {
    use super::{Exit, Feed, ProcId, ProcSpec, ProcessHost, Stdin};
    use std::collections::BTreeMap;
    use std::io;

    /// One observable interaction, in order. Polls are not logged: they are
    /// noise, and liveness is observable through [`FakeProcessHost::live`].
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        Spawn {
            id: ProcId,
            label: String,
        },
        SpawnFailed {
            label: String,
        },
        WriteStdin {
            id: ProcId,
            len: usize,
        },
        WriteFifo {
            id: ProcId,
            index: usize,
            len: usize,
        },
        Kill {
            id: ProcId,
            label: String,
        },
    }

    #[derive(Debug)]
    struct FakeProc {
        spec: ProcSpec,
        alive: bool,
        exit_reason: Option<String>,
        /// The secret read out of the staged stdin file at spawn, if any.
        secret: Option<String>,
        writes_fail: bool,
        fifo_full: bool,
        stdin_bytes: u64,
        fifo_bytes: u64,
        fifo_dropped: u64,
    }

    #[derive(Debug, Default)]
    pub struct FakeProcessHost {
        next_id: ProcId,
        procs: BTreeMap<ProcId, FakeProc>,
        calls: Vec<Call>,
        /// Labels whose next spawn fails, with the io error message.
        spawn_failures: BTreeMap<String, String>,
    }

    impl FakeProcessHost {
        pub fn new() -> Self {
            Self::default()
        }

        /// The next spawn of `label` fails. Consumed on use, so a supervisor
        /// under test can recover on its next attempt.
        pub fn fail_next_spawn(&mut self, label: &str, reason: &str) {
            self.spawn_failures
                .insert(label.to_owned(), reason.to_owned());
        }

        /// Marks a live process as exited, as if it crashed.
        pub fn exit(&mut self, id: ProcId, reason: &str) {
            if let Some(p) = self.procs.get_mut(&id) {
                p.alive = false;
                p.exit_reason = Some(reason.to_owned());
            }
        }

        /// Every subsequent write to `id` returns a broken pipe.
        pub fn fail_writes(&mut self, id: ProcId) {
            if let Some(p) = self.procs.get_mut(&id) {
                p.writes_fail = true;
            }
        }

        /// `id`'s video queue is at its cap, so every frame from now on comes
        /// back [`Feed::Dropped`]. Audio keeps flowing, which is the whole
        /// point: the two pipes are independent.
        pub fn fill_fifo(&mut self, id: ProcId, full: bool) {
            if let Some(p) = self.procs.get_mut(&id) {
                p.fifo_full = full;
            }
        }

        pub fn calls(&self) -> &[Call] {
            &self.calls
        }

        pub fn clear_calls(&mut self) {
            self.calls.clear();
        }

        /// Ids still running, in spawn order.
        pub fn live(&self) -> Vec<ProcId> {
            self.procs
                .iter()
                .filter(|(_, p)| p.alive)
                .map(|(&id, _)| id)
                .collect()
        }

        pub fn spec(&self, id: ProcId) -> Option<&ProcSpec> {
            self.procs.get(&id).map(|p| &p.spec)
        }

        pub fn label(&self, id: ProcId) -> Option<&str> {
            self.procs.get(&id).map(|p| p.spec.label.as_str())
        }

        /// What arrived through the staged stdin secret file.
        pub fn secret(&self, id: ProcId) -> Option<&str> {
            self.procs.get(&id).and_then(|p| p.secret.as_deref())
        }

        pub fn stdin_bytes(&self, id: ProcId) -> u64 {
            self.procs.get(&id).map_or(0, |p| p.stdin_bytes)
        }

        pub fn fifo_bytes(&self, id: ProcId) -> u64 {
            self.procs.get(&id).map_or(0, |p| p.fifo_bytes)
        }

        /// Frames refused because the video queue was at its cap.
        pub fn fifo_dropped(&self, id: ProcId) -> u64 {
            self.procs.get(&id).map_or(0, |p| p.fifo_dropped)
        }

        /// The most recent live process whose label contains `needle`.
        pub fn find_live(&self, needle: &str) -> Option<ProcId> {
            self.procs
                .iter()
                .rev()
                .find(|(_, p)| p.alive && p.spec.label.contains(needle))
                .map(|(&id, _)| id)
        }

        /// Every spawn ever recorded, with its spec: the audit trail the key
        /// tests scan.
        pub fn specs(&self) -> impl Iterator<Item = &ProcSpec> {
            self.procs.values().map(|p| &p.spec)
        }
    }

    impl ProcessHost for FakeProcessHost {
        fn spawn(&mut self, spec: &ProcSpec) -> io::Result<ProcId> {
            if let Some(reason) = self.spawn_failures.remove(&spec.label) {
                self.calls.push(Call::SpawnFailed {
                    label: spec.label.clone(),
                });
                return Err(io::Error::other(reason));
            }
            // Consume a staged secret exactly like the real host: read the
            // descriptor, unlink the path.
            let secret = match &spec.stdin {
                Stdin::SecretFile(path) => {
                    let contents = std::fs::read_to_string(path)?;
                    std::fs::remove_file(path)?;
                    Some(contents.trim_end().to_owned())
                }
                _ => None,
            };
            let id = self.next_id;
            self.next_id += 1;
            self.procs.insert(
                id,
                FakeProc {
                    spec: spec.clone(),
                    alive: true,
                    exit_reason: None,
                    secret,
                    writes_fail: false,
                    fifo_full: false,
                    stdin_bytes: 0,
                    fifo_bytes: 0,
                    fifo_dropped: 0,
                },
            );
            self.calls.push(Call::Spawn {
                id,
                label: spec.label.clone(),
            });
            Ok(id)
        }

        fn write_stdin(&mut self, id: ProcId, buf: &[u8]) -> io::Result<()> {
            let p = self
                .procs
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such process"))?;
            if !p.alive || p.writes_fail {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"));
            }
            p.stdin_bytes += buf.len() as u64;
            self.calls.push(Call::WriteStdin { id, len: buf.len() });
            Ok(())
        }

        fn write_fifo(&mut self, id: ProcId, index: usize, buf: &[u8]) -> io::Result<Feed> {
            let p = self
                .procs
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such process"))?;
            if !p.alive || p.writes_fail {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"));
            }
            if p.fifo_full {
                p.fifo_dropped += 1;
                return Ok(Feed::Dropped);
            }
            p.fifo_bytes += buf.len() as u64;
            self.calls.push(Call::WriteFifo {
                id,
                index,
                len: buf.len(),
            });
            Ok(Feed::Queued)
        }

        fn poll(&mut self, id: ProcId) -> Exit {
            match self.procs.get(&id) {
                Some(p) if p.alive => Exit::Running,
                Some(p) => Exit::Exited {
                    reason: p.exit_reason.clone().unwrap_or_else(|| "exited".to_owned()),
                },
                None => Exit::Exited {
                    reason: "process handle is gone".to_owned(),
                },
            }
        }

        fn kill(&mut self, id: ProcId) {
            if let Some(p) = self.procs.get_mut(&id) {
                let label = p.spec.label.clone();
                p.alive = false;
                p.exit_reason = Some("killed".to_owned());
                self.calls.push(Call::Kill { id, label });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_mentions_finds_a_secret_in_argv() {
        let spec = ProcSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "exec ffmpeg -f flv \"$JS_INGEST\"".into()],
            stdin: Stdin::SecretFile(PathBuf::from("/run/jamstream/keys/1")),
            fifos: Vec::new(),
            label: "pusher:twitch:1".into(),
            relay_url: None,
        };
        assert!(!spec.mentions("live_123_secret"));
        assert!(spec.mentions("JS_INGEST"));
        // An empty needle never matches, so a keyless spec cannot false-positive.
        assert!(!spec.mentions(""));
    }

    /// Everything the relay is for: the line ffmpeg writes when a platform
    /// refuses a key is the line that contains the key.
    #[test]
    fn a_key_in_a_refused_connect_does_not_reach_the_sink() {
        const KEY: &str = "live_918273645_TZq0cVnB4kLsX";
        let stderr = format!(
            "[flv @ 0x55d1c0a2f480] Failed to connect to \
             rtmps://ingest.twitch.tv/app/{KEY}: Connection refused\n\
             [out#0/flv @ 0x55d1c0a31200] Could not write header: Broken pipe\n"
        );

        let mut got: Vec<String> = Vec::new();
        relay_stderr(io::Cursor::new(stderr), None, |line| {
            got.push(line.to_owned())
        });

        let all = got.join("\n");
        assert!(!all.contains(KEY), "the key reached the sink: {all}");
        assert!(!all.contains("ingest.twitch.tv"), "{all}");
        // And the diagnosis survived, which is the only reason to read stderr
        // at all rather than send it to /dev/null.
        assert_eq!(
            got,
            vec![
                "[flv @ 0x55d1c0a2f480] Failed to connect to rtmps://<redacted> \
                 Connection refused"
                    .to_owned(),
                "[out#0/flv @ 0x55d1c0a31200] Could not write header: Broken pipe".to_owned(),
            ]
        );
    }

    /// A line with no URL in it is passed through untouched, so redaction
    /// costs nothing in the ordinary case.
    #[test]
    fn a_line_without_a_url_arrives_verbatim() {
        let stderr = "[libx264 @ 0x1] VBV buffer size 0 too small, using 2500\n\
                      Conversion failed!\n";
        let mut got: Vec<String> = Vec::new();
        relay_stderr(io::Cursor::new(stderr), None, |line| {
            got.push(line.to_owned())
        });
        assert_eq!(
            got,
            vec![
                "[libx264 @ 0x1] VBV buffer size 0 too small, using 2500".to_owned(),
                "Conversion failed!".to_owned(),
            ]
        );
    }

    /// Redaction runs to the end of the URL and no further, several times a
    /// line if it has to, and a line that is only a URL still logs its scheme.
    #[test]
    fn redaction_stops_at_the_end_of_each_url() {
        assert_eq!(
            redact("rtmp://relay/in to rtmps://out/app/k failed", None),
            "rtmp://<redacted> to rtmps://<redacted> failed"
        );
        assert_eq!(
            redact("rtmps://host/app/secret", None),
            "rtmps://<redacted>"
        );
        assert_eq!(redact("no url here", None), "no url here");
        assert_eq!(redact("", None), "");
    }

    /// The distinction #437 turned on: a refusal from the loopback relay and
    /// a refusal from the platform are the same errno and completely
    /// different problems. Only the relay's own URL is named, and only when
    /// it matches character for character.
    #[test]
    fn only_the_configured_relay_is_named_and_everything_else_is_redacted() {
        const RELAY_URL: &str = "rtmp://127.0.0.1:1935/jamstream";
        let relay = Some(RELAY_URL);

        // Ours, with the colon ffmpeg puts after it.
        assert_eq!(
            redact(
                "[flv @ 0x1] Failed to connect to rtmp://127.0.0.1:1935/jamstream: \
                 Connection refused",
                relay
            ),
            "[flv @ 0x1] Failed to connect to <local relay> Connection refused"
        );

        // The platform's, in the same line and with the same wording. This is
        // the one that carries a key, and it is redacted whole.
        const KEY: &str = "live_918273645_TZq0cVnB4kLsX";
        let platform = format!(
            "[flv @ 0x1] Failed to connect to rtmps://a.rtmps.youtube.com:443/live2/{KEY}: \
             Connection refused"
        );
        let got = redact(&platform, relay);
        assert!(!got.contains(KEY), "{got}");
        assert!(!got.contains("youtube.com"), "{got}");
        assert_eq!(
            got,
            "[flv @ 0x1] Failed to connect to rtmps://<redacted> Connection refused"
        );

        // A near miss is not a match: a different port, a different path, or
        // a different host all stay redacted, so the exception can never
        // print something the pipeline did not hand it.
        for near in [
            "rtmp://127.0.0.1:1936/jamstream",
            "rtmp://127.0.0.1:1935/jamstream/evil",
            "rtmp://127.0.0.2:1935/jamstream",
            "rtmps://127.0.0.1:1935/jamstream",
        ] {
            let line = format!("connect to {near}: refused");
            let got = redact(&line, relay);
            assert!(!got.contains(RELAY), "{near} was treated as the relay");
            assert!(got.contains(REDACTED), "{near} -> {got}");
        }
    }

    /// A line longer than the cap loses its tail rather than arriving as a
    /// second line. Split in the middle of a URL, that second line would have
    /// carried the key with no scheme left in it to redact.
    #[test]
    fn an_overlong_line_is_truncated_rather_than_split() {
        const KEY: &str = "live_5551212_dontLogMe";
        let padding = "x".repeat(STDERR_LINE_CAP);
        let stderr = format!("{padding} rtmps://ingest.example/app/{KEY}\nshort line\n");

        let mut got: Vec<String> = Vec::new();
        relay_stderr(io::Cursor::new(stderr), None, |line| {
            got.push(line.to_owned())
        });

        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].len(), STDERR_LINE_CAP);
        assert!(!got[0].contains(KEY));
        // The next line is still read: the tail was discarded, not the stream.
        assert_eq!(got[1], "short line");
    }

    /// Invalid UTF-8 and a missing final newline are both a child's business,
    /// not a reason to lose the line or hang waiting for one.
    #[test]
    fn a_partial_line_of_invalid_utf8_still_arrives() {
        let stderr: Vec<u8> = b"caf\xff done".to_vec();
        let mut got: Vec<String> = Vec::new();
        relay_stderr(io::Cursor::new(stderr), None, |line| {
            got.push(line.to_owned())
        });
        assert_eq!(got, vec!["caf\u{fffd} done".to_owned()]);
    }

    /// The tail keeps the last lines, oldest first, and nothing before them.
    #[test]
    fn the_tail_keeps_the_last_lines_oldest_first() {
        let tail = StderrTail::default();
        assert_eq!(tail.quote(), None);
        for n in 1..=STDERR_TAIL_LINES + 2 {
            tail.push(&format!("line {n}"));
        }
        assert_eq!(tail.quote().as_deref(), Some("line 3; line 4"));
    }

    /// An exit code that is a truncated negative errno says so, and the
    /// translation is the OS's rather than a table here: the same refusal is
    /// 145 on Linux and 195 on macOS, and neither number means anything to
    /// the musician who is shown it.
    #[test]
    fn a_truncated_errno_exit_code_is_translated_by_the_os() {
        let refused = 256 - libc::ECONNREFUSED;
        // The numbers from the issue, pinned so the claim stays checkable.
        #[cfg(target_os = "linux")]
        assert_eq!(refused, 145);
        #[cfg(target_os = "macos")]
        assert_eq!(refused, 195);

        let err = errno_behind(refused).expect("a refused connect must decode");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);

        // 255 is both -1 and the conventional "it went wrong", so it is left
        // as a number rather than reported as EPERM.
        assert!(errno_behind(255).is_none());
        // And an ordinary small exit code is not an errno at all.
        assert!(errno_behind(1).is_none());
        assert!(errno_behind(0).is_none());
    }

    #[cfg(unix)]
    mod real {
        use super::*;
        use std::process::ExitStatus;

        /// Runs a shell script to completion under the real host and returns
        /// the reason the supervisor would report.
        fn reason_after_death(script: &str, relay: Option<&str>) -> String {
            let mut host = StdProcessHost::new();
            let spec = ProcSpec {
                program: PathBuf::from("/bin/sh"),
                args: vec!["-c".to_owned(), script.to_owned()],
                stdin: Stdin::Null,
                fifos: Vec::new(),
                label: "pusher:youtube:1".to_owned(),
                relay_url: relay.map(str::to_owned),
            };
            let id = host.spawn(&spec).expect("sh must spawn");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match host.poll(id) {
                    Exit::Exited { reason } => return reason,
                    Exit::Running => {
                        assert!(Instant::now() < deadline, "the child never exited");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        }

        /// The whole of #437, against a real process: a host who pasted a
        /// good key into a session that cannot reach the platform used to be
        /// shown an integer. Now the row carries the sentence ffmpeg wrote,
        /// and the key that sentence contained is still not in it.
        #[test]
        fn a_dead_pusher_reports_what_it_printed_and_not_the_key_in_it() {
            const KEY: &str = "live_918273645_TZq0cVnB4kLsX";
            let script = format!(
                "printf '%s\\n' \
                 '[flv @ 0x55d1c0a2f480] Failed to connect to \
                 rtmps://a.rtmps.youtube.com:443/live2/{KEY}: Connection refused' \
                 '[out#0/flv @ 0x55d1c0a31200] Could not write header: Broken pipe' >&2\n\
                 exit 145\n"
            );
            let reason = reason_after_death(&script, Some("rtmp://127.0.0.1:1935/jamstream"));

            assert!(
                !reason.contains(KEY),
                "the key reached the reason: {reason}"
            );
            assert!(!reason.contains("youtube.com"), "{reason}");
            assert!(
                reason.contains("Failed to connect to rtmps://<redacted> Connection refused"),
                "{reason}"
            );
            // The diagnosis replaces the exit code rather than joining it.
            assert!(!reason.contains("exited with status"), "{reason}");
        }

        /// The distinction that cost an evening. Same errno, same wording
        /// from ffmpeg, different problem: this one is a relay that is not
        /// listening on the session's own loopback, not a platform saying no.
        #[test]
        fn a_refusal_from_the_loopback_relay_names_the_relay() {
            const RELAY_URL: &str = "rtmp://127.0.0.1:1935/jamstream";
            let script = format!(
                "printf '%s\\n' \
                 '[flv @ 0x1] Failed to connect to {RELAY_URL}: Connection refused' >&2\n\
                 exit 145\n"
            );
            let reason = reason_after_death(&script, Some(RELAY_URL));
            assert!(reason.contains("<local relay>"), "{reason}");
            assert!(!reason.contains("127.0.0.1"), "{reason}");
        }

        /// Nothing on stderr is the only case left to the exit code, and even
        /// then it is translated rather than shown as a bare integer.
        #[test]
        fn a_silent_child_falls_back_to_a_translated_exit_code() {
            let refused = 256 - libc::ECONNREFUSED;
            let reason = reason_after_death(&format!("exit {refused}\n"), None);
            assert!(reason.starts_with(&format!("exited with status {refused}")));
            assert!(reason.contains("Connection refused"), "{reason}");
        }

        /// A child with nothing to say and nothing to decode still reports
        /// the plain thing it always did.
        #[test]
        fn an_ordinary_failure_still_reports_its_code() {
            assert_eq!(reason_after_death("exit 1\n", None), "exited with status 1");
        }

        /// `describe` is what the fallback runs, so it is worth checking on
        /// its own where an exit status can be built rather than caused.
        #[test]
        fn describe_translates_only_what_the_os_recognises() {
            use std::os::unix::process::ExitStatusExt;
            let status = |code: i32| ExitStatus::from_raw(code << 8);
            assert_eq!(describe(&status(0)), "exited cleanly");
            assert_eq!(describe(&status(1)), "exited with status 1");
            let refused = 256 - libc::ECONNREFUSED;
            assert!(describe(&status(refused)).contains("Connection refused"));
        }
    }
}
