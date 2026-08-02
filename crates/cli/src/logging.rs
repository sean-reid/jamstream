//! The `RUST_LOG` filter shared by the CLI and the desktop app.
//!
//! `filter::Targets` rather than `EnvFilter`: both parse the target and
//! level directives operators actually write, but EnvFilter's span-field
//! grammar needs a regex engine that cost every binary about 280 KiB of
//! .text and nothing in this repository uses (#298).

use tracing_subscriber::filter::{LevelFilter, Targets};

/// The stderr filter built from `RUST_LOG`.
pub fn from_env() -> Targets {
    filter(std::env::var("RUST_LOG").ok().as_deref())
}

/// The parse behind [`from_env`], split from the environment read so tests
/// can pin the behavior without racing the process environment.
///
/// Warnings by default: with `RUST_LOG` unset or unparseable, every `warn!`
/// still reaches the operator, including the security-relevant Windows ACL
/// degradations. A set `RUST_LOG` wins outright.
pub fn filter(rust_log: Option<&str>) -> Targets {
    rust_log
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| Targets::new().with_default(LevelFilter::WARN))
}

#[cfg(test)]
mod tests {
    use super::filter;
    use tracing::Level;
    use tracing_subscriber::filter::LevelFilter;

    /// The contract from #345: warnings show with `RUST_LOG` unset, so the
    /// security-relevant degradations are never silently dropped.
    #[test]
    fn defaults_to_warn_when_rust_log_is_unset() {
        let unset = filter(None);
        assert_eq!(unset.default_level(), Some(LevelFilter::WARN));
        assert!(unset.would_enable("jamstream_cli", &Level::WARN));
        assert!(!unset.would_enable("jamstream_cli", &Level::INFO));
    }

    #[test]
    fn honors_rust_log_target_directives() {
        let targets = filter(Some("jamstream_cli=debug,hyper=error"));
        assert!(targets.would_enable("jamstream_cli", &Level::DEBUG));
        assert!(!targets.would_enable("hyper", &Level::WARN));
    }

    /// A value Targets cannot parse falls back to the default rather than
    /// panicking before the subscriber exists, matching what
    /// `EnvFilter::try_from_default_env`'s error arm did.
    #[test]
    fn falls_back_to_warn_on_an_unparseable_value() {
        assert_eq!(
            filter(Some("jamstream_cli=notalevel")).default_level(),
            Some(LevelFilter::WARN)
        );
    }
}
