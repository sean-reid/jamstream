//! The `jamstream` CLI: session hosting (provision, invite, teardown,
//! sweep) and the headless client used by tests and power users. All logic
//! lives here in library form; `main.rs` only parses arguments and
//! dispatches, so integration tests drive the exact code the binary runs.

pub mod cli;
pub mod end;
pub mod host;
pub mod join;
pub mod providers;
pub mod state;
pub mod status;
pub mod sweep;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The user asked for something this build cannot do; fixable by them.
    #[error("{0}")]
    Usage(String),
    /// An operation started and did not finish cleanly.
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Provider(#[from] jamstream_cloud::ProviderError),
    #[error(transparent)]
    Protocol(#[from] jamstream_protocol::Error),
    #[error(transparent)]
    Session(#[from] jamstream_session::SessionError),
    #[error(transparent)]
    Wav(#[from] hound::Error),
    #[error("state file: {0}")]
    State(#[from] serde_json::Error),
}
