#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("packet too short or malformed")]
    Malformed,
    #[error("unknown packet type {0:#04x}")]
    UnknownPacketType(u8),
    #[error("protocol version {theirs} is not supported (this build speaks {ours})")]
    VersionMismatch { ours: u16, theirs: u16 },
    #[error("handshake failed")]
    Handshake(#[from] snow::Error),
    #[error("invite is not valid: {0}")]
    Invite(&'static str),
    #[error("token rejected: {0}")]
    Token(&'static str),
    #[error("replayed or expired packet counter")]
    Replay,
    #[error("decryption failed")]
    Decrypt,
    #[error("encoding failed")]
    Encode(#[from] postcard::Error),
    #[error("control link is backed up")]
    LinkFull,
}
