//! Sans-io session state machines for JamStream: `ServerCore` admits, mixes,
//! and relays on the session VM; `ClientCore` drives a musician or listener
//! endpoint. Callers own sockets and clocks and pass `now_ms` plus datagrams
//! in, getting datagrams and events out, so both cores run identically under
//! the server binary, the desktop client, and the simulation harness.

mod avatar;
pub mod client;
pub mod limits;
pub mod server;

/// Reassembly internals for the fuzz workspace only; production traffic
/// reaches them through the cores' control-message handling.
#[cfg(feature = "fuzzing")]
pub use avatar::{AvatarCache, AvatarHash, AvatarRx, RxStep, avatar_hash, chunk_total};

pub use client::{ClientCore, ClientEvent, ClientState, ClientStats, ServerCandidates};
/// Session capacity and the host-surface defaults, defined once in
/// [`limits`] and re-exported here because every crate that offers seats
/// needs them: `jamstream_session::MAX_MUSICIANS` is the number.
pub use limits::{
    DEFAULT_HOURS, DEFAULT_IDLE_MIN, DEFAULT_LISTENERS, DEFAULT_MAX_HOURS,
    DEFAULT_MEMBER_TIMEOUT_MS, DEFAULT_MUSICIANS, MAX_LISTENERS, MAX_MUSICIANS,
    MEMBER_QUIET_AFTER_MS, VIOLATION_BURST,
};
pub use server::{
    BroadcastMember, BroadcastTick, MemberStats, ServerConfig, ServerCore, ServerEvent, Stem,
    TICK_SAMPLES,
};

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
