use serde::{Deserialize, Serialize};

/// Random per-session identifier, minted by the host CLI at create time.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub [u8; 16]);

impl SessionId {
    pub fn generate() -> Self {
        Self(crate::rand_bytes())
    }

    /// Lowercase hex, which is how every surface that keeps a file per
    /// session names it.
    pub fn hex(&self) -> String {
        hex(&self.0)
    }
}

impl std::fmt::Debug for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SessionId({})", hex(&self.0[..4]))
    }
}

/// Token identifier used for revocation. One per invite.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenId(pub [u8; 16]);

impl TokenId {
    pub fn generate() -> Self {
        Self(crate::rand_bytes())
    }
}

impl std::fmt::Debug for TokenId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TokenId({})", hex(&self.0[..4]))
    }
}

/// Small integer identifying a member within one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MemberId(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// Sends and receives audio, gets a personal mix.
    Musician,
    /// Receives the broadcast mix at relaxed latency. Cannot transmit audio.
    Listener,
}

/// The host holds a musician token whose member id is always 0.
pub const HOST_MEMBER_ID: MemberId = MemberId(0);

/// Small integer naming one broadcast destination within one session. The
/// host mints it, so add and remove refer to the same destination without a
/// server round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DestinationId(pub u16);

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
