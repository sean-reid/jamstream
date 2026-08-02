//! Where the desktop app's diagnostics go.
//!
//! Without this the app installed no subscriber, so every `warn!` in the
//! client vanished, and a failure before the window opened left nothing
//! behind at all. Events now go to stderr, for the shells that have one, and
//! to a log file for everything else; a panic lands in the same file before
//! the default hook runs. The file is truncated at startup: one run's log,
//! bounded, with no rotation machinery to go wrong.
//!
//! Diagnostics must never cost the feature they describe, so a log directory
//! that cannot be made falls back to stderr alone, silently.

use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use tracing_subscriber::fmt::writer::MakeWriterExt as _;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Where the app writes its log: `logs/app.log` under the same per-user
/// jamstream data directory the CLI keeps session state in, so one folder
/// holds everything this machine knows. None when the platform offers no
/// data directory at all.
pub fn log_path() -> Option<PathBuf> {
    jamstream_cli::state::data_dir()
        .ok()
        .map(|dir| dir.join("logs").join("app.log"))
}

/// Installs the subscriber and the panic hook. Returns the log file's path
/// when one is in use.
///
/// Warnings show by default and `RUST_LOG` still overrides, through the
/// same filter the CLI uses. Called once, first thing in main: everything
/// after it can fail visibly.
pub fn init() -> Option<PathBuf> {
    let filter = jamstream_cli::logging::from_env();
    match open_log_file() {
        Some((path, file)) => {
            let file = Arc::new(file);
            // ANSI off because one format layer feeds both writers, and a
            // log file full of color codes is unreadable where it matters.
            let _ = tracing_subscriber::registry()
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::io::stderr.and(Arc::clone(&file))),
                )
                .with(filter)
                .try_init();
            install_panic_hook(file);
            Some(path)
        }
        None => {
            let _ = tracing_subscriber::registry()
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                .with(filter)
                .try_init();
            None
        }
    }
}

fn open_log_file() -> Option<(PathBuf, File)> {
    open_log_file_at(log_path()?)
}

fn open_log_file_at(path: PathBuf) -> Option<(PathBuf, File)> {
    std::fs::create_dir_all(path.parent()?).ok()?;
    let mut file = File::create(&path).ok()?;
    // Written directly, past the filter: only warnings and panics log, so
    // a healthy run's file would otherwise be empty, and an empty file is
    // indistinguishable from a sink that never worked. The Windows console
    // fix makes this the app's only diagnostic surface, so it has to prove
    // itself on every run.
    let _ = writeln!(
        file,
        "jamstream-app {} log; warnings and panics land here; empty after \
         this line is a healthy run",
        env!("CARGO_PKG_VERSION")
    );
    Some((path, file))
}

/// Writes the panic and its backtrace to the log file, then runs the default
/// hook so stderr keeps working where there is one. The file is the point: a
/// crash before the window opens is otherwise invisible on Windows, and it
/// is what a bug report can attach.
fn install_panic_hook(file: Arc<File>) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let mut out = &*file;
        let _ = writeln!(out, "{info}\n{backtrace}");
        let _ = file.sync_all();
        default(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The app takes the CLI's filter wholesale: warnings by default so
    /// degradations show, and a `RUST_LOG` target directive still wins.
    /// Pinned from this binary's side so the shared module cannot drift
    /// out from under it (#298 moved it from EnvFilter to Targets).
    #[test]
    fn the_filter_defaults_to_warn_and_honors_rust_log() {
        use tracing::Level;
        let unset = jamstream_cli::logging::filter(None);
        assert!(unset.would_enable("jamstream_client", &Level::WARN));
        assert!(!unset.would_enable("jamstream_client", &Level::INFO));

        let set = jamstream_cli::logging::filter(Some("jamstream_client=trace"));
        assert!(set.would_enable("jamstream_client", &Level::TRACE));
    }

    /// The log sits under the same jamstream data directory as the CLI's
    /// session state. None is only for a machine with no platform data
    /// directory, where init falls back to stderr alone.
    #[test]
    fn the_log_lives_under_the_jamstream_data_directory() {
        let Some(path) = log_path() else {
            eprintln!("skipping: no platform data directory here");
            return;
        };
        assert!(path.is_absolute(), "{}", path.display());
        assert!(
            path.ends_with(Path::new("jamstream/logs/app.log")),
            "{}",
            path.display()
        );
        assert_eq!(
            path.parent().and_then(|p| p.parent()),
            jamstream_cli::state::data_dir().ok().as_deref()
        );
    }

    /// A fresh log opens with the banner, and reopening truncates back to
    /// exactly one: the file always proves the sink ran, and an otherwise
    /// empty file means a healthy run rather than a broken sink.
    #[test]
    fn the_log_opens_with_a_banner_and_truncates_on_reopen() {
        let dir = std::env::temp_dir().join(format!("jamstream-log-banner-{}", std::process::id()));
        let path = dir.join("app.log");
        for _ in 0..2 {
            let (p, file) = open_log_file_at(path.clone()).expect("open log");
            drop(file);
            let text = std::fs::read_to_string(&p).expect("read log");
            assert!(
                text.starts_with(&format!("jamstream-app {}", env!("CARGO_PKG_VERSION"))),
                "{text:?}"
            );
            assert_eq!(text.lines().count(), 1, "{text:?}");
            assert!(text.contains("empty after this line is a healthy run"));
        }
    }
}
