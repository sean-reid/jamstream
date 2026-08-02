//! Audio device isolation for jamstream.
//!
//! Everything that touches a real sound card lives behind [`AudioBackend`],
//! so the engine, protocol, and harness stay deterministic. Production code
//! gets a platform backend from [`backend`]; tests and the headless client
//! use [`WavBackend`], which is driven by explicit pumps instead of time.
//! [`CallbackBridge`] is the RT-safe hop between device callbacks and the
//! network thread.

mod bridge;
mod mode;
mod rate;
mod resample;
mod types;
mod wav;

/// Device-edge format negotiation and sample conversion. Only the Windows
/// exclusive path negotiates formats, but the conversion tables are pure
/// arithmetic, so they are built and tested on every host.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod format;

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
mod cpal_backend;

/// Failure classification and the shared-mode fallback table for the Windows
/// exclusive path. Built everywhere so its unit tests run on every host.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod wasapi_policy;

#[cfg(target_os = "windows")]
mod wasapi_backend;

pub use bridge::{CallbackBridge, DeviceSide, EngineSide};
pub use mode::{DeviceMode, active_device_mode};
pub use rate::{RateOutcome, RateOutcomes};
pub use types::{
    AudioBackend, AudioError, DeviceInfo, Direction, DuplexHandler, FormFactor, Result,
    StreamConfig, StreamHandle,
};
pub use wav::{DeviceRung, WavBackend, WavStream};

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub use cpal_backend::CpalBackend;

#[cfg(target_os = "windows")]
pub use wasapi_backend::WindowsBackend;

/// Platform default backend.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[must_use]
pub fn backend() -> Box<dyn AudioBackend> {
    Box::new(CpalBackend::new())
}

/// Platform default backend: WASAPI exclusive mode (about 10 ms of device
/// latency) with an automatic fall back to cpal's shared mode (20-30 ms) when
/// the endpoint, its driver, or another application will not allow exclusive
/// access. [`active_device_mode`] reports which one is running.
#[cfg(target_os = "windows")]
#[must_use]
pub fn backend() -> Box<dyn AudioBackend> {
    Box::new(WindowsBackend::new())
}
