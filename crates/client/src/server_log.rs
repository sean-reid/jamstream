//! Where a session server's log lands on the host's machine.
//!
//! A cloud session's machine deletes itself when the session ends, so the
//! journal explaining why a broadcast or a take failed goes with it. The
//! server sends the host each line as it writes it, the session core reports
//! what arrives, and this is the half that puts it on disk: one file per
//! session, `logs/<session>.log` beside the session's state file, at 0600
//! because it names members and addresses.
//!
//! Appended rather than replaced, unlike the app's own log. A host who quits
//! and rejoins gets a second run of lines; the server's ring holds only what
//! it has not sent yet, so replacing the file would lose the first run's
//! evidence for good.
//!
//! Diagnostics must never cost the feature they describe, so every failure
//! here is silent: a host with no writable state directory keeps their
//! session and loses only the file.

use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;

use jamstream_cloud::private::{append_private, create_private_dir, write_private};
use jamstream_session::logtail::{SERVER_LOG_SESSION_FIELD, SERVER_LOG_TARGET};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::Context;

/// Bytes of one session's log kept on this machine.
///
/// This is for reading a failure after the fact, not for archiving a session.
/// 64 KiB is a few hundred of the lines the server actually sends, which is
/// many times over what an ffmpeg refusal and its fallout come to, and it is
/// small enough to attach to a bug report whole. Past that the oldest lines go:
/// what a host needs is the end of the log, and the file is trimmed on a line
/// boundary so what survives is still readable.
pub const SERVER_LOG_TAIL_BYTES: u64 = 64 * 1024;

/// Collects the lines a session server sent this host into that session's file.
pub fn layer<S>() -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    // The target filter and nothing else, so the file is written whatever
    // `RUST_LOG` says about the app's own log: a host debugging a failed
    // broadcast must not have to have set a variable first.
    ServerLog::default()
        .with_filter(Targets::new().with_target(SERVER_LOG_TARGET, LevelFilter::INFO))
}

#[derive(Default)]
struct ServerLog {
    open: Mutex<Option<Open>>,
}

impl<S: tracing::Subscriber> Layer<S> for ServerLog {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut line = Reported::default();
        event.record(&mut line);
        let (Some(session), Some(text)) = (line.session, line.text) else {
            return;
        };
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if open.as_ref().is_none_or(|o| o.session != session) {
            *open = Open::create(&session);
        }
        if open.as_mut().is_some_and(|o| !o.append(&text)) {
            // The handle is no longer good for anything; the next line opens a
            // fresh one rather than writing into a file nobody can find.
            *open = None;
        }
    }
}

/// The two fields a reported line carries: which session, and the line.
#[derive(Default)]
struct Reported {
    session: Option<String>,
    text: Option<String>,
}

impl Reported {
    fn set(&mut self, field: &Field, value: String) {
        match field.name() {
            SERVER_LOG_SESSION_FIELD => self.session = Some(value),
            "message" => self.text = Some(value),
            _ => {}
        }
    }
}

impl Visit for Reported {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.set(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.set(field, format!("{value:?}"));
    }
}

struct Open {
    session: String,
    path: PathBuf,
    file: File,
    len: u64,
}

impl Open {
    fn create(session: &str) -> Option<Open> {
        Open::at(
            jamstream_cli::state::server_log_path_for(session).ok()?,
            session,
        )
    }

    /// The half that touches disk, split from resolving the path so a test
    /// says where the file goes instead of steering the environment.
    fn at(path: PathBuf, session: &str) -> Option<Open> {
        create_private_dir(path.parent()?).ok()?;
        let file = append_private(&path).ok()?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        Some(Open {
            session: session.to_owned(),
            path,
            file,
            len,
        })
    }

    /// Appends one line, answering whether this handle is still usable.
    fn append(&mut self, line: &str) -> bool {
        if writeln!(self.file, "{line}").is_err() {
            return false;
        }
        self.len += line.len() as u64 + 1;
        // Trimmed at twice the budget, so the rewrite happens once per budget
        // written rather than once per line.
        self.len <= SERVER_LOG_TAIL_BYTES * 2 || self.trim()
    }

    /// Rewrites the file with its last [`SERVER_LOG_TAIL_BYTES`], from the
    /// first line boundary inside them.
    fn trim(&mut self) -> bool {
        let Ok(bytes) = std::fs::read(&self.path) else {
            return false;
        };
        let from = bytes.len().saturating_sub(SERVER_LOG_TAIL_BYTES as usize);
        let cut = bytes[from..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |at| from + at + 1);
        // Replaces the file, which leaves our handle on the inode it replaced.
        if write_private(&self.path, &bytes[cut..]).is_err() {
            return false;
        }
        match append_private(&self.path) {
            Ok(file) => {
                self.file = file;
                self.len = (bytes.len() - cut) as u64;
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A log directory of this test's own, gone before it starts, so a mode
    /// assertion is about what this code did.
    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jamstream-server-log-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn write_lines(path: &Path, lines: impl IntoIterator<Item = String>) {
        let mut open = Open::at(path.to_owned(), "session").expect("open the session log");
        for line in lines {
            assert!(open.append(&line), "the handle stopped being usable");
        }
    }

    /// One file per session, private, and a second run of the app adds to what
    /// the first one recorded instead of taking it away: the server's ring
    /// holds only what it has not sent, so a replaced file loses the evidence.
    #[test]
    fn lines_append_across_runs_and_the_file_stays_private() {
        let dir = scratch("append");
        let path = dir.join("logs").join("session.log");
        write_lines(&path, ["first".to_owned()]);
        write_lines(&path, ["second".to_owned()]);

        assert_eq!(
            std::fs::read_to_string(&path).expect("read the log"),
            "first\nsecond\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the server log must be 0600");
            let dir_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bound is enforced, the end of the log is what survives, and what
    /// survives starts at a line boundary rather than mid-sentence.
    #[test]
    fn a_long_session_keeps_the_end_of_its_log() {
        let dir = scratch("trim");
        let path = dir.join("logs").join("session.log");
        let line = |n: usize| format!("{n:06} {}", "x".repeat(120));
        write_lines(&path, (0..2_000).map(line));

        let text = std::fs::read_to_string(&path).expect("read the log");
        assert!(
            (text.len() as u64) <= SERVER_LOG_TAIL_BYTES * 2,
            "the log grew to {} bytes",
            text.len()
        );
        assert!(text.ends_with(&format!("{}\n", line(1_999))), "{text:?}");
        // A whole first line, not the tail of one that was cut in half.
        let first = text.lines().next().expect("a line");
        assert_eq!(first.len(), line(0).len(), "{first:?}");
        assert!(!text.contains(&line(0)), "the oldest lines should be gone");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
