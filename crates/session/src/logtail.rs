//! The last lines the server process wrote to its own log, for the host.
//!
//! A cloud session's machine deletes itself when the session ends, so its
//! journal dies with the failure it explains. This is the ring the server
//! binary's log subscriber fills and [`crate::ServerCore`] drains onto the
//! host's control link, a line at a time, while the session is still running.
//!
//! The ring is process-wide because the log is: one jamstreamd is one session,
//! and a subscriber has no session to hand its events to. Nothing is installed
//! unless a binary asks for one, so the simulation harness and every test that
//! builds a core keep the empty, deterministic behaviour they had.
//!
//! Lines arrive redacted. The subscriber that fills the ring is the one place
//! that sees the raw text, and it strips every URL before pushing, because a
//! stream key lives in one; see `jamstream_server::logtail`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use jamstream_protocol::control::fit_server_log_line;

/// The tracing target a host's client re-emits arriving lines under, and the
/// field naming the session they belong to.
///
/// The client core is sans-io and holds no path, so it reports what it
/// received the way everything else here reports: the desktop app subscribes
/// to this target and is the half that puts the lines beside the session's
/// state file. `jamstream_client::server_log` is that half.
pub const SERVER_LOG_TARGET: &str = "jamstream::server_log";
pub const SERVER_LOG_SESSION_FIELD: &str = "session";

/// Lines held for a host who has not caught up yet.
///
/// The last 128 lines at the wire's 320-byte cap is 40 KB of the server's
/// memory, and 128 lines is several times what an ffmpeg failure and its
/// fallout come to. Past that the oldest go, because the tail is the part
/// worth having: a host reads this to find out what just went wrong.
pub const LOG_TAIL_LINES: usize = 128;

/// A bounded ring of log lines, shared by the thread writing them and the
/// core sending them.
#[derive(Clone, Debug)]
pub struct LogTail {
    inner: Arc<Mutex<Ring>>,
}

#[derive(Debug, Default)]
struct Ring {
    lines: VecDeque<String>,
    dropped: u64,
}

impl Default for LogTail {
    fn default() -> Self {
        LogTail::new()
    }
}

impl LogTail {
    pub fn new() -> LogTail {
        LogTail {
            inner: Arc::new(Mutex::new(Ring::default())),
        }
    }

    /// Adds one already-redacted line, dropping the oldest when full.
    ///
    /// Cut to the wire's cap here rather than at send, so the ring's own
    /// memory is bounded by a number this module states.
    pub fn push(&self, line: &str) {
        let mut ring = self.lock();
        if ring.lines.len() >= LOG_TAIL_LINES {
            ring.lines.pop_front();
            ring.dropped += 1;
        }
        ring.lines.push_back(fit_server_log_line(line).to_owned());
    }

    /// Takes up to `max` lines, oldest first.
    pub fn take(&self, max: usize) -> Vec<String> {
        let mut ring = self.lock();
        let n = max.min(ring.lines.len());
        ring.lines.drain(..n).collect()
    }

    /// Lines dropped and not yet accounted for, so a gap in the host's copy
    /// is stated rather than left as a silence.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// Forgets the drop count, for a caller that has just reported it.
    pub fn clear_dropped(&self) {
        self.lock().dropped = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.lock().lines.is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ring> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

static INSTALLED: OnceLock<LogTail> = OnceLock::new();

/// Publishes the ring this process's cores drain. Called once, by the binary
/// that installs the log subscriber filling it.
///
/// Answers false when one is already installed, which leaves the first one
/// serving: a second ring nothing writes to would silently take the host's
/// copy away.
pub fn install(tail: LogTail) -> bool {
    INSTALLED.set(tail).is_ok()
}

/// The ring for this process, or None where no binary installed one.
pub fn installed() -> Option<LogTail> {
    INSTALLED.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamstream_protocol::control::MAX_SERVER_LOG_LINE;

    #[test]
    fn lines_come_back_oldest_first_and_only_once() {
        let tail = LogTail::new();
        tail.push("one");
        tail.push("two");
        tail.push("three");
        assert_eq!(tail.take(2), vec!["one".to_owned(), "two".to_owned()]);
        assert_eq!(tail.take(8), vec!["three".to_owned()]);
        assert!(tail.take(8).is_empty());
        assert_eq!(tail.dropped(), 0);
    }

    /// The bound is the point: a server logging faster than the link drains
    /// must cost a fixed amount of memory and say how much it lost, not grow
    /// until something else on the machine fails.
    #[test]
    fn a_full_ring_drops_the_oldest_and_counts_it() {
        let tail = LogTail::new();
        for i in 0..LOG_TAIL_LINES + 10 {
            tail.push(&format!("line {i}"));
        }
        let lines = tail.take(usize::MAX);
        assert_eq!(lines.len(), LOG_TAIL_LINES);
        assert_eq!(lines[0], format!("line {}", 10));
        assert_eq!(tail.dropped(), 10);
        // Reported once, not on every drain afterwards.
        tail.clear_dropped();
        assert_eq!(tail.dropped(), 0);
    }

    /// A line arrives cut, so nothing the ring holds can be refused by the
    /// link that carries it.
    #[test]
    fn a_long_line_is_cut_before_it_is_stored() {
        let tail = LogTail::new();
        tail.push(&"x".repeat(MAX_SERVER_LOG_LINE * 3));
        assert_eq!(tail.take(1)[0].len(), MAX_SERVER_LOG_LINE);
    }

    /// Nothing is installed by default, which is what keeps a core built by
    /// the harness or a test deterministic.
    #[test]
    fn no_ring_is_installed_until_a_binary_asks() {
        assert!(installed().is_none());
        let tail = LogTail::new();
        assert!(install(tail.clone()));
        tail.push("up");
        assert_eq!(installed().expect("installed").take(1), vec!["up"]);
        // The second install loses, so the ring the subscriber writes to is
        // the one the core drains.
        assert!(!install(LogTail::new()));
        tail.push("still here");
        assert_eq!(installed().expect("installed").take(1), vec!["still here"]);
    }
}
