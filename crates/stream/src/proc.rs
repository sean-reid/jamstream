//! Process hosting behind a trait.
//!
//! The supervisor's interesting behaviour (backoff, isolation between
//! destinations, key handling, status derivation) is all about *when* it
//! spawns and kills things, not about `std::process`. So every process
//! interaction goes through [`ProcessHost`]: [`StdProcessHost`] is the thin
//! real adapter, [`fake::FakeProcessHost`] is a scriptable double with a call
//! log that tests assert against.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

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

/// Spawn, feed, observe, kill. Blocking writes on purpose: a partial
/// rawvideo frame would corrupt the encode, so the pipeline would rather
/// wait, which is why it runs on its own thread and never on the mix tick.
pub trait ProcessHost {
    fn spawn(&mut self, spec: &ProcSpec) -> io::Result<ProcId>;
    fn write_stdin(&mut self, id: ProcId, buf: &[u8]) -> io::Result<()>;
    fn write_fifo(&mut self, id: ProcId, index: usize, buf: &[u8]) -> io::Result<()>;
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
#[derive(Debug, Default)]
pub struct StdProcessHost {
    next_id: ProcId,
    procs: BTreeMap<ProcId, Live>,
}

#[derive(Debug)]
struct Live {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    fifos: Vec<std::fs::File>,
    fifo_paths: Vec<PathBuf>,
    /// We feed this process, so closing our write ends is an end-of-stream it
    /// can act on: give it a moment to flush before the signal. A pusher has
    /// nothing to flush and no reason to wait.
    drains_on_eof: bool,
}

/// How long a fed process gets to notice EOF and exit cleanly.
const DRAIN_MS: u64 = 1_500;

impl StdProcessHost {
    pub fn new() -> Self {
        Self::default()
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
        let stdin = child.stdin.take();

        let mut fifos = Vec::with_capacity(spec.fifos.len());
        for path in &spec.fifos {
            match open_fifo_write(path) {
                Ok(file) => fifos.push(file),
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    for p in &spec.fifos {
                        let _ = std::fs::remove_file(p);
                    }
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
        use std::io::Write;
        let live = self
            .procs
            .get_mut(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such process"))?;
        let stdin = live
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "stdin is not a pipe"))?;
        stdin.write_all(buf)
    }

    fn write_fifo(&mut self, id: ProcId, index: usize, buf: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let live = self
            .procs
            .get_mut(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such process"))?;
        let fifo = live
            .fifos
            .get_mut(index)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such fifo"))?;
        fifo.write_all(buf)
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
        // Dropping our write ends first lets a fed child see end of stream,
        // flush its muxer, and exit; the kill covers everything else.
        live.stdin.take();
        live.fifos.clear();
        if live.drains_on_eof {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(DRAIN_MS);
            while std::time::Instant::now() < deadline {
                match live.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
        }
        let _ = live.child.kill();
        let _ = live.child.wait();
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
    use super::{Exit, ProcId, ProcSpec, ProcessHost, Stdin};
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
        stdin_bytes: u64,
        fifo_bytes: u64,
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
                    stdin_bytes: 0,
                    fifo_bytes: 0,
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

        fn write_fifo(&mut self, id: ProcId, index: usize, buf: &[u8]) -> io::Result<()> {
            let p = self
                .procs
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such process"))?;
            if !p.alive || p.writes_fail {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"));
            }
            p.fifo_bytes += buf.len() as u64;
            self.calls.push(Call::WriteFifo {
                id,
                index,
                len: buf.len(),
            });
            Ok(())
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
