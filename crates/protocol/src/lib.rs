//! Wire protocol for JamStream sessions: invites, handshake, media framing,
//! and the reliable control layer. Everything here is deterministic and
//! time-free; callers pass timestamps in.

pub mod control;
pub mod error;
pub mod ids;
pub mod invite;
pub mod media;
pub mod replay;
pub mod transport;
pub mod wire;

pub use error::Error;

/// Protocol version spoken by this build. A server accepts exactly this
/// version; any other draws the authenticated version reject, which is how a
/// client on the wrong build learns what to update.
///
/// Version 2 encrypted the cookie challenge and bound it to the init it
/// answers; a version 1 challenge is a bare cookie no version 2 peer emits
/// or accepts. See [`wire::build_cookie_challenge`].
pub const PROTOCOL_VERSION: u16 = 2;

/// Session audio runs at 48 kHz everywhere; other rates are resampled at the
/// edges, never on the wire.
pub const SAMPLE_RATE: u32 = 48_000;

pub(crate) fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("os rng unavailable");
    buf
}
