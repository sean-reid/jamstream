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
/// 1382400 bytes and a pipe holds 65536, so a frame is twenty-odd pipe fulls
/// and a writer is parked inside it almost all the time.
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
        // stderr is inherited: ffmpeg's one-line errors are the most useful
        // thing in the journal when a platform refuses a key.
        cmd.stderr(Stdio::inherit());
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
        let abandon = |child: &mut std::process::Child, feeders: Vec<Feeder>| {
            let _ = child.kill();
            let _ = child.wait();
            for feeder in feeders {
                feeder.close();
                feeder.finish();
            }
            for path in &spec.fifos {
                let _ = std::fs::remove_file(path);
            }
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
                    abandon(&mut child, Vec::new());
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
                    abandon(&mut child, fifos);
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
            Ok(Some(status)) => Exit::Exited {
                reason: describe(&status),
            },
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
        for path in &live.fifo_paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn describe(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(0) => "exited cleanly".to_owned(),
        Some(code) => format!("exited with status {code}"),
        None => "killed by signal".to_owned(),
    }
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

/// A scriptable [`ProcessHost`] with a call log. Compiled unconditionally so
/// integration tests in `tests/` can use it; it spawns nothing.
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
        };
        assert!(!spec.mentions("live_123_secret"));
        assert!(spec.mentions("JS_INGEST"));
        // An empty needle never matches, so a keyless spec cannot false-positive.
        assert!(!spec.mentions(""));
    }
}
