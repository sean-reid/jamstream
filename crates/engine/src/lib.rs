//! Shared DSP core for JamStream: Opus codec wrappers, jitter buffering,
//! loss redundancy policy, mixing, limiting, and the metronome. Everything
//! is deterministic and time-free; callers supply sample clocks and ticks.

pub mod codec;
pub mod jitter;
pub mod limiter;
pub mod metronome;
pub mod mixer;
pub mod redundancy;

pub use codec::{Channels, CodecError, Decoder, Encoder};
pub use jitter::{JitterBuffer, JitterStats, MediaPacket, Pull};
pub use limiter::Limiter;
pub use metronome::Metronome;
pub use mixer::{Fader, db_to_lin, mix_into};
pub use redundancy::RedundancyPolicy;
