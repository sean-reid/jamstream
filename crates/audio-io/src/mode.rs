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
//! only one test touches it is one added test away from a race, so the rule
//! is held two ways instead. A new test in this file fails the build
//! until its author has said which kind it is, and a test anywhere in the
//! crate that reaches the cell claims it, so a second toucher fails under
//! libtest, which is where the two of them share a process.

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
    read(process_cell())
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn set_active_device_mode(mode: DeviceMode) {
    write(process_cell(), mode);
}

/// The one cell the process has, reached only from here so that the claim
/// below cannot be walked past by a test that does not know about it.
fn process_cell() -> &'static AtomicU8 {
    #[cfg(test)]
    claim_the_process_value();
    &ACTIVE
}

/// Fails the second test to reach the process-wide cell. Under libtest the
/// whole binary is one process and each test gets a thread, so two touchers
/// would race; this makes the second one a failure carrying its own fix.
/// Nextest gives each test a process, where there is nothing to race.
#[cfg(test)]
fn claim_the_process_value() {
    use std::sync::{Mutex, PoisonError};

    static OWNER: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);
    let mut owner = OWNER.lock().unwrap_or_else(PoisonError::into_inner);
    let me = std::thread::current().id();
    assert!(
        *owner.get_or_insert(me) == me,
        "a second test reached the process-wide device mode; test the \
         encoding through read() and write() on a cell the test owns"
    );
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

/// One of the tests below reaches the process-wide cell and the other must
/// not, which a diff of a third one does not show. Counting this file's own
/// tests holds the build until whoever adds one has answered that, in every
/// runner, including the ones the claim above cannot speak for.
#[cfg(test)]
const _: () = assert!(
    xtask::source::lines_equal(include_str!("mode.rs"), "#[test]") == 2,
    "a new test in mode.rs exercises the encoding through read() and write() \
     on a cell it owns, and then raises the count above. The process-wide \
     cell has one toucher; a second one races it under libtest."
);

#[cfg(test)]
mod tests {
    use super::*;

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
    /// client's status line depend on. The claim this makes on the cell is
    /// taken by the accessors themselves, so nothing here has to remember.
    #[test]
    fn the_public_accessors_share_the_process_wide_value() {
        set_active_device_mode(DeviceMode::Exclusive);
        assert_eq!(active_device_mode(), Some(DeviceMode::Exclusive));
        set_active_device_mode(DeviceMode::Shared);
        assert_eq!(active_device_mode(), Some(DeviceMode::Shared));
    }
}
