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

/// Messages one member may send that the server relays to every other member,
/// which is a chat line or a change of avatar. One of these costs roughly
/// [`MAX_MUSICIANS`] + [`MAX_LISTENERS`] times its own size in egress, so the
/// rate is set by what a person does, not by what a link can carry.
pub const FANOUT_BURST: u32 = 12;

/// How fast that allowance comes back. Two per second is above a fast typist
/// in a heated moment and far below the rate at which relaying to everyone
/// costs the host real money.
pub const FANOUT_REFILL_PER_SEC: u32 = 2;

/// Illegal packets a member may send before the server drops them. An honest
/// client sends none: every violation the server counts is something no
/// shipped client does. 32 is room for a version skew nobody anticipated.
pub const VIOLATION_BURST: u32 = 32;

/// How fast that allowance comes back. One per second means a peer who keeps
/// rejoining after an ejection gets one illegal packet per second rather than
/// a flood, and a client with a systematic bug trickles instead of being
/// locked out of the session for good.
pub const VIOLATION_REFILL_PER_SEC: u32 = 1;

/// A token bucket over integer milliseconds. Time-free like the rest of the
/// cores: the caller passes `now_ms`, so the harness replays a rate limit
/// exactly. Refill is accounted in thousandths of a token, which makes the
/// rate exact at millisecond resolution instead of drifting a percent or so
/// per refill.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: u32,
    per_sec: u32,
    tokens: u32,
    millitokens: u64,
    last_ms: u64,
}

impl TokenBucket {
    /// Starts full: the first burst of `capacity` is always allowed.
    pub fn new(capacity: u32, per_sec: u32) -> TokenBucket {
        TokenBucket {
            capacity,
            per_sec,
            tokens: capacity,
            millitokens: 0,
            last_ms: 0,
        }
    }

    /// Spends one token, answering whether there was one. Time going
    /// backwards only forfeits refill; it never grants any.
    pub fn take(&mut self, now_ms: u64) -> bool {
        self.refill(now_ms);
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }

    /// Whether a token is there, without spending it.
    pub fn available(&mut self, now_ms: u64) -> bool {
        self.refill(now_ms);
        self.tokens > 0
    }

    fn refill(&mut self, now_ms: u64) {
        let elapsed = now_ms.saturating_sub(self.last_ms);
        self.last_ms = self.last_ms.max(now_ms);
        if self.tokens >= self.capacity {
            self.millitokens = 0;
            return;
        }
        self.millitokens += elapsed.saturating_mul(u64::from(self.per_sec));
        let gained = u32::try_from(self.millitokens / 1_000).unwrap_or(u32::MAX);
        self.millitokens %= 1_000;
        self.tokens = self.capacity.min(self.tokens.saturating_add(gained));
        if self.tokens >= self.capacity {
            self.millitokens = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TokenBucket;

    #[test]
    fn burst_then_refill_at_the_configured_rate() {
        let mut b = TokenBucket::new(4, 2);
        for _ in 0..4 {
            assert!(b.take(0));
        }
        assert!(!b.take(0));
        // Two per second: the first token is back at 500 ms, not before.
        assert!(!b.take(499));
        assert!(b.take(500));
        assert!(!b.take(500));
        // Idle long enough to refill past capacity: it clamps.
        for _ in 0..4 {
            assert!(b.take(60_000));
        }
        assert!(!b.take(60_000));
    }

    #[test]
    fn refill_does_not_drift_over_many_small_steps() {
        // 16 per second sampled every millisecond: exactly 16 tokens per
        // second, which a naive integer division would overshoot.
        let mut b = TokenBucket::new(16, 16);
        for _ in 0..16 {
            assert!(b.take(0));
        }
        let mut granted = 0;
        for ms in 1..=1_000u64 {
            if b.take(ms) {
                granted += 1;
            }
        }
        assert_eq!(granted, 16);
    }

    #[test]
    fn time_going_backwards_grants_nothing() {
        let mut b = TokenBucket::new(1, 1);
        assert!(b.take(10_000));
        assert!(!b.take(0));
        assert!(!b.take(10_500));
        assert!(b.take(11_000));
    }
}
