//! What the current stream got from the device: the sharing mode.
//!
//! Windows can open a stream two ways with materially different latency, and
//! whether exclusive mode was available depends on the endpoint, its driver,
//! and what other applications are doing. The UI needs to be able to say which
//! one the user ended up with, but [`crate::StreamHandle`] is a stable seam
//! shared with the offline and cpal backends, so the answer is published here
//! instead of widening the trait. The rate-outcome report in `rate.rs` rides
//! the same seam for the same reason.
//!
//! There is exactly one device stream per process, so a single value is
//! accurate. It is set when a stream opens and left alone when one closes: the
//! last mode that was actually running is more useful to display than nothing.

use std::sync::atomic::{AtomicU8, Ordering};

/// How the running device stream is talking to the endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceMode {
    /// WASAPI exclusive mode: the device is ours alone, at about 10 ms.
    Exclusive,
    /// The device is shared with the system mixer, at about 20-30 ms.
    Shared,
}

const UNSET: u8 = 0;
const EXCLUSIVE: u8 = 1;
const SHARED: u8 = 2;

static ACTIVE: AtomicU8 = AtomicU8::new(UNSET);

/// The sharing mode of the most recently opened device stream.
///
/// `None` before any stream has opened, and on platforms where the distinction
/// does not exist: only the Windows backend reports a mode, because CoreAudio
/// and PipeWire have no shared/exclusive split to report.
#[must_use]
pub fn active_device_mode() -> Option<DeviceMode> {
    match ACTIVE.load(Ordering::Relaxed) {
        EXCLUSIVE => Some(DeviceMode::Exclusive),
        SHARED => Some(DeviceMode::Shared),
        _ => None,
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn set_active_device_mode(mode: DeviceMode) {
    let value = match mode {
        DeviceMode::Exclusive => EXCLUSIVE,
        DeviceMode::Shared => SHARED,
    };
    ACTIVE.store(value, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_mode_follows_the_last_open() {
        // Serialized by being the only test that touches the global.
        set_active_device_mode(DeviceMode::Exclusive);
        assert_eq!(active_device_mode(), Some(DeviceMode::Exclusive));
        set_active_device_mode(DeviceMode::Shared);
        assert_eq!(active_device_mode(), Some(DeviceMode::Shared));
    }
}
