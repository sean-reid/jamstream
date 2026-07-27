//! Sans-io session state machines for JamStream: `ServerCore` admits, mixes,
//! and relays on the session VM; `ClientCore` drives a musician or listener
//! endpoint. Callers own sockets and clocks and pass `now_ms` plus datagrams
//! in, getting datagrams and events out, so both cores run identically under
//! the server binary, the desktop client, and the simulation harness.

mod avatar;
pub mod client;
pub mod server;

pub use client::{ClientCore, ClientEvent, ClientState, ClientStats};
pub use server::{MemberStats, ServerConfig, ServerCore, ServerEvent};

/// Errors surfaced by client-side calls; the cores otherwise swallow bad
/// input from the network by design.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("not joined to a session")]
    NotJoined,
    #[error("invalid parameter: {0}")]
    InvalidParam(&'static str),
    #[error(transparent)]
    Protocol(#[from] jamstream_protocol::Error),
    #[error(transparent)]
    Codec(#[from] jamstream_engine::CodecError),
}
