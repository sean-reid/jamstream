//! The shape of a session, defined once.
//!
//! Capacity is what [`crate::ServerCore`] enforces at admission, so every
//! surface that offers seats or mints invites has to agree with it or offer
//! seats the server will refuse. These constants are that agreement: the
//! `jamstream host` flags, the desktop host wizard, the invites panel, and
//! the server's own config all read them from here.
//!
//! Musician capacity counts the host. The host holds member 0 and joins as
//! a musician like everyone else, so a session of [`MAX_MUSICIANS`] is the
//! host plus `MAX_MUSICIANS - 1` guests.

/// Musicians admitted at once, the host's own seat included.
pub const MAX_MUSICIANS: usize = 10;

/// Listeners admitted at once. Listeners receive the broadcast mix and send
/// nothing, so they are cheaper than musicians and the cap is higher.
pub const MAX_LISTENERS: usize = 20;

/// Musician seats a host surface offers before the host changes it, the
/// host's own seat included: a quartet.
pub const DEFAULT_MUSICIANS: u8 = 4;

/// Listener seats offered by default. None: listener invites are opt-in,
/// and an unused invite is a credential nobody asked for.
pub const DEFAULT_LISTENERS: u8 = 0;

/// Expected session length in hours, for the cost preview. Shapes the
/// estimate only; the real bill is metered.
pub const DEFAULT_HOURS: f32 = 3.0;

/// Minutes with no musicians connected before the server exits and the
/// machine is destroyed.
pub const DEFAULT_IDLE_MIN: u32 = 10;

/// Hard cap on session length in hours. The machine destroys itself at the
/// cap and invites expire with it.
pub const DEFAULT_MAX_HOURS: u32 = 12;

/// A member silent this long is dropped from the roster.
pub const DEFAULT_MEMBER_TIMEOUT_MS: u64 = 10_000;
