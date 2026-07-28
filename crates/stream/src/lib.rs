//! The broadcast pipeline: one encode on the session VM, a localhost relay,
//! and one `ffmpeg -c copy` pusher per destination, supervised.
//!
//! Shape of the thing, from the audio tick outwards:
//!
//! ```text
//!   ServerCore mix tick (2.5 ms, post-limiter broadcast stereo)
//!        |  audio every tick, levels every tick, roster on change
//!        v
//!   Pipeline  ---- renders a card frame every 1600 audio samples ---->
//!        |  s16le on stdin, yuv420p on a named FIFO
//!        v
//!   ffmpeg encode (x264 veryfast zerolatency CBR 2500k, AAC-LC 128k)
//!        |  RTMP
//!        v
//!   MediaMTX on 127.0.0.1:1935
//!        |                    |
//!   ffmpeg -c copy       ffmpeg -c copy      (one per destination)
//!        v                    v
//!     Twitch               YouTube
//! ```
//!
//! Three properties drive the design:
//!
//! - **Audio is the master clock.** Video frame emission is a pure function
//!   of audio samples consumed, so the two timelines cannot drift. See
//!   [`cadence`].
//! - **One destination's failure touches no other.** That is why there is a
//!   process per destination instead of ffmpeg's `tee` muxer: a dead
//!   destination is one dead pusher, restarted on its own backoff schedule,
//!   while the encode and every other pusher keep running.
//! - **Stream keys never reach argv or persistent disk.** See [`keys`] and
//!   the security note on [`pipeline::Pipeline`].
//!
//! Every process interaction goes through [`proc::ProcessHost`], so the
//! supervisor's behaviour is tested against a scriptable fake and the real
//! `std::process` implementation is a thin adapter.

pub mod cadence;
pub mod keys;
pub mod pipeline;
pub mod platform;
pub mod proc;
pub mod worker;
pub mod yuv;

pub use cadence::VideoCadence;
pub use pipeline::{
    Levels, Pipeline, PipelineEvent, Roster, StreamConfig, StreamError, StreamMember,
};
pub use platform::{PlatformCatalog, PlatformSpec};
pub use proc::{Exit, ProcId, ProcSpec, ProcessHost, StdProcessHost, Stdin};
pub use worker::{StreamWorker, TickPayload};

/// Audio sample rate everywhere in JamStream. The wire protocol's own
/// constant: the encoder is fed session audio, so a second number here is a
/// resample nobody asked for.
pub use jamstream_protocol::SAMPLE_RATE;
/// Interleaved stereo samples in one 2.5 ms session tick.
pub const TICK_STEREO_SAMPLES: usize = 240;
