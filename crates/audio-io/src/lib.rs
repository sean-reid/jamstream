//! Audio device isolation for jamstream.
//!
//! Everything that touches a real sound card lives behind [`AudioBackend`],
//! so the engine, protocol, and harness stay deterministic. Production code
//! gets a platform backend from [`backend`]; tests and the headless client
//! use [`WavBackend`], which is driven by explicit pumps instead of time.
//! [`CallbackBridge`] is the RT-safe hop between device callbacks and the
//! network thread.

mod bridge;
mod types;
mod wav;

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
mod cpal_backend;

pub use bridge::{CallbackBridge, DeviceSide, EngineSide};
pub use types::{
    AudioBackend, AudioError, DeviceInfo, Direction, DuplexHandler, Result, StreamConfig,
    StreamHandle,
};
pub use wav::{WavBackend, WavStream};

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub use cpal_backend::CpalBackend;

/// Platform default backend.
///
/// Windows currently runs WASAPI shared mode through cpal; the exclusive
/// mode backend (direct `wasapi` crate, CamillaDSP precedent) will replace
/// the body of the Windows arm without touching callers.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[must_use]
pub fn backend() -> Box<dyn AudioBackend> {
    Box::new(CpalBackend::new())
}

/// Platform default backend. See the non-Windows variant for the contract.
#[cfg(target_os = "windows")]
#[must_use]
pub fn backend() -> Box<dyn AudioBackend> {
    // Tracked follow-up: swap in the exclusive-mode WASAPI backend here.
    Box::new(CpalBackend::new())
}
