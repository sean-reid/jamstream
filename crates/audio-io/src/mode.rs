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
//!
//! The encoding is split from the cell it lives in so a test can exercise it
//! on a cell of its own. A process-wide value serialized by a comment saying
//! only one test touches it is one added test away from a race, and the
//! coverage job runs these suites under libtest, with no process per test to
//! hide it (#395). The one test that has to use the real cell claims it, so
//! a second toucher fails rather than races.

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
    read(&ACTIVE)
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn set_active_device_mode(mode: DeviceMode) {
    write(&ACTIVE, mode);
}

fn read(cell: &AtomicU8) -> Option<DeviceMode> {
    match cell.load(Ordering::Relaxed) {
        EXCLUSIVE => Some(DeviceMode::Exclusive),
        SHARED => Some(DeviceMode::Shared),
        _ => None,
    }
}

fn write(cell: &AtomicU8, mode: DeviceMode) {
    let value = match mode {
        DeviceMode::Exclusive => EXCLUSIVE,
        DeviceMode::Shared => SHARED,
    };
    cell.store(value, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Fails the second test to reach the process-wide cell, which is the
    /// half the split cannot enforce on its own. Under libtest the whole
    /// binary is one process, so two touchers would race; this turns that
    /// into a failure with the fix in the message instead of a flake.
    fn claim_the_process_value() {
        static CLAIMED: AtomicBool = AtomicBool::new(false);
        assert!(
            !CLAIMED.swap(true, Ordering::Relaxed),
            "a second test reached the process-wide mode; test the encoding \
             through read() and write() on a cell the test owns"
        );
    }

    #[test]
    fn reported_mode_follows_the_last_open() {
        let cell = AtomicU8::new(UNSET);
        assert_eq!(read(&cell), None, "nothing has opened yet");
        write(&cell, DeviceMode::Exclusive);
        assert_eq!(read(&cell), Some(DeviceMode::Exclusive));
        write(&cell, DeviceMode::Shared);
        assert_eq!(read(&cell), Some(DeviceMode::Shared));
    }

    /// The public pair really is a pair, over the one cell the process has.
    /// That the setter and the getter reach the same cell is the only thing
    /// the split cannot check, and it is what the Windows backend and the
    /// client's status line depend on.
    #[test]
    fn the_public_accessors_share_the_process_wide_value() {
        claim_the_process_value();
        set_active_device_mode(DeviceMode::Exclusive);
        assert_eq!(active_device_mode(), Some(DeviceMode::Exclusive));
        set_active_device_mode(DeviceMode::Shared);
        assert_eq!(active_device_mode(), Some(DeviceMode::Shared));
    }
}
