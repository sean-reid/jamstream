//! What the current stream got from the device: the sharing mode, and whether
//! the OS is resampling the render side.
//!
//! Windows can open a stream two ways with materially different latency, and
//! whether exclusive mode was available depends on the endpoint, its driver,
//! and what other applications are doing. The UI needs to be able to say which
//! one the user ended up with, but [`crate::StreamHandle`] is a stable seam
//! shared with the offline and cpal backends, so the answer is published here
//! instead of widening the trait. The render-conversion report rides the same
//! seam for the same reason.
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

const CONVERTING: u8 = 1;
const AT_DEVICE_RATE: u8 = 2;

static RENDER_CONVERSION: AtomicU8 = AtomicU8::new(UNSET);

/// Whether the OS is resampling the most recently opened render stream.
///
/// Windows shared mode opens output with `AUTOCONVERTPCM`, so a playback
/// device whose engine runs at 44.1 kHz carries a 48 kHz stream with the OS
/// converting in between; PipeWire does the same between its graph rate and a
/// client stream. Acceptable by design since the #347 decision, but disclosed:
/// `Some(true)` when the render stream rides an OS converter (with its
/// unaccounted latency), `Some(false)` when the device really runs at the
/// stream rate, `None` before any stream has opened or when the backend
/// cannot tell. The capture side needs no report because no backend converts
/// it: capture at the wrong rate is refused outright.
///
/// Consumed next by the client's session screen, alongside
/// [`active_device_mode`].
#[must_use]
pub fn active_render_conversion() -> Option<bool> {
    match RENDER_CONVERSION.load(Ordering::Relaxed) {
        CONVERTING => Some(true),
        AT_DEVICE_RATE => Some(false),
        _ => None,
    }
}

pub(crate) fn set_render_conversion(converting: bool) {
    let value = if converting {
        CONVERTING
    } else {
        AT_DEVICE_RATE
    };
    RENDER_CONVERSION.store(value, Ordering::Relaxed);
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

    #[test]
    fn conversion_report_follows_the_last_open() {
        // Serialized by being the only test that touches this global.
        set_render_conversion(true);
        assert_eq!(active_render_conversion(), Some(true));
        set_render_conversion(false);
        assert_eq!(active_render_conversion(), Some(false));
    }
}
