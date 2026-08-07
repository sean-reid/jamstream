//! A second sink for this process's own log, kept in memory for the host.
//!
//! A cloud session's machine deletes itself when the session ends, and the
//! journal it takes with it is the only place a broadcast failure is explained
//! (#438). This layer keeps the last lines of that journal in
//! [`jamstream_session::LogTail`], where [`jamstream_session::ServerCore`]
//! finds them and sends them to the host over the control link it already has.
//!
//! It is the same text the stdout log carries, formatted by the same layer, so
//! the host's copy and the journal cannot say different things. Two rules
//! narrow it:
//!
//! - Every URL is stripped, by the redactor the pusher's stderr reader already
//!   uses. A stream key lives inside an RTMP URL and an invite is a URL of its
//!   own, and neither is something a diagnostic may put on a host's disk. The
//!   diagnosis survives: `Failed to connect to rtmps://<redacted>: Connection
//!   refused` still says what went wrong.
//! - Lines are cut to the wire's cap and the ring is bounded, both in
//!   [`jamstream_session::logtail`]. This is for reading a failure afterwards,
//!   not for archiving a session.

use std::io;

use jamstream_session::LogTail;
use jamstream_stream::proc::redact;
use tracing_subscriber::fmt::MakeWriter;

/// Publishes a ring for this process's cores and returns the layer that fills
/// it. Called once, beside the layer that writes the log to stdout.
///
/// Answers `None` when a ring is already installed: the core drains the first
/// one, so a second layer would be writing where nothing reads.
pub fn layer<S>() -> Option<impl tracing_subscriber::Layer<S>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let tail = LogTail::new();
    if !jamstream_session::logtail::install(tail.clone()) {
        return None;
    }
    Some(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(Tail(tail)),
    )
}

/// The formatted log, one event per write, on its way into the ring.
#[derive(Clone)]
struct Tail(LogTail);

impl io::Write for Tail {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for line in String::from_utf8_lossy(buf).lines() {
            if !line.trim().is_empty() {
                self.0.push(redact(line, None).as_ref());
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Tail {
    type Writer = Tail;

    fn make_writer(&'a self) -> Tail {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;

    /// A key in the log must not be a key on the host's disk.
    ///
    /// The pusher's stderr reader redacts before it logs, so this is the
    /// second line of defence rather than the first, and it is the one that
    /// covers every other thing the process writes: this ring carries whatever
    /// the process logged, and a URL is where a stream key and an invite both
    /// live. The diagnosis has to survive, because a diagnosis is the only
    /// reason to keep the line.
    #[test]
    fn a_key_in_the_log_never_reaches_the_ring() {
        let tail = LogTail::new();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(Tail(tail.clone())),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                child = "twitch",
                "[flv @ 0x1] Failed to connect to rtmps://live.twitch.tv/app/live_1234_SECRET: \
                 Connection refused"
            );
        });

        let lines = tail.take(8);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert!(!line.contains("SECRET"), "{line}");
        assert!(!line.contains("live.twitch.tv"), "{line}");
        assert!(
            line.contains("Failed to connect to rtmps://<redacted>"),
            "{line}"
        );
        assert!(line.contains("Connection refused"), "{line}");
        // The level and the fields the journal shows come along, because the
        // host's copy is formatted by the same layer as the journal.
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains("twitch"), "{line}");
        // And the timestamp, which is what makes a line worth correlating
        // against the moment a host watched a broadcast drop.
        assert!(line.starts_with("20"), "{line}");
    }

    /// One ring per process, and it belongs to whoever installed it first.
    #[test]
    fn a_second_layer_is_refused_rather_than_left_writing_nowhere() {
        assert!(layer::<tracing_subscriber::Registry>().is_some());
        assert!(layer::<tracing_subscriber::Registry>().is_none());
    }
}
