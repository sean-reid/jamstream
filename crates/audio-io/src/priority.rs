//! Scheduling priority for the threads that keep a device fed.
//!
//! A device callback is scheduled in real time by the platform. The thread that
//! fills the ring those callbacks drain is an ordinary thread, and an ordinary
//! thread can be set aside for longer than a ring holds, at which point the
//! device pads silence. Both sides of the boundary have to ask for the same
//! thing, so the ask lives here beside the backends rather than in the callers.

use std::time::Duration;

/// What the platform granted the calling thread, which is not always what was
/// asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadPriority {
    /// A real-time class: a mach time-constraint policy on macOS, MMCSS
    /// "Pro Audio" on Windows.
    RealTime,
    /// Above ordinary threads but not real time, which is the Windows
    /// consolation when MMCSS will not register the thread.
    Raised,
    /// Nothing was asked for, because this platform offers nothing that works
    /// without a privilege an installed app cannot count on having.
    Unchanged,
    /// The platform refused. The thread keeps the priority it had, and the
    /// refusal is in the log with the platform's own words in it.
    Refused,
}

/// The priority of the thread that took it, plus whatever the platform wants
/// released afterwards, which on Windows includes process-wide timer
/// resolution. Dropping this puts all of it back.
pub struct AudioPriority {
    granted: ThreadPriority,
    held: imp::Held,
}

impl AudioPriority {
    /// Raises the calling thread to the priority audio work needs, for as long
    /// as the returned value lives.
    ///
    /// `period` is the cadence the thread has to keep, which is the deadline the
    /// platform schedules it against where it takes one.
    #[must_use]
    pub fn raise_current_thread(period: Duration) -> AudioPriority {
        let (granted, held) = imp::hold(period);
        AudioPriority { granted, held }
    }

    /// What this thread was granted.
    #[must_use]
    pub fn granted(&self) -> ThreadPriority {
        self.granted
    }
}

impl Drop for AudioPriority {
    fn drop(&mut self) {
        self.held.release();
    }
}

#[cfg(target_os = "windows")]
pub(crate) use imp::MmcssGuard;

#[cfg(target_os = "windows")]
mod imp {
    use std::time::Duration;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Media::{TIMERR_NOERROR, timeBeginPeriod, timeEndPeriod};
    use windows::Win32::System::Threading::{
        AVRT_PRIORITY, AVRT_PRIORITY_NORMAL, AvRevertMmThreadCharacteristics,
        AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, GetCurrentThread, SetThreadPriority,
        THREAD_PRIORITY_TIME_CRITICAL,
    };
    use windows::core::w;

    use super::ThreadPriority;

    /// Timer resolution a loop paced by `thread::sleep` needs, in milliseconds.
    /// The default granularity is coarser than an audio tick, so a sleep of one
    /// tick returns whenever the next coarse tick comes around. Since Windows 10
    /// 2004 this affects only the process that asks.
    const TIMER_MS: u32 = 1;

    pub(super) struct Held {
        mmcss: Option<MmcssGuard>,
        timer_raised: bool,
    }

    pub(super) fn hold(_period: Duration) -> (ThreadPriority, Held) {
        // SAFETY: no preconditions; the period is a plain millisecond count and
        // the matching timeEndPeriod is in `release`.
        let timer = unsafe { timeBeginPeriod(TIMER_MS) };
        if timer != TIMERR_NOERROR {
            tracing::warn!(
                code = timer,
                "timeBeginPeriod failed; a sleeping thread wakes at the system granularity"
            );
        }
        // MMCSS "Pro Audio" at its own default priority, not the critical rung
        // the device threads take: this thread feeds them and does not race
        // them.
        let mmcss = MmcssGuard::promote(AVRT_PRIORITY_NORMAL);
        let granted = mmcss.granted();
        (
            granted,
            Held {
                mmcss: Some(mmcss),
                timer_raised: timer == TIMERR_NOERROR,
            },
        )
    }

    impl Held {
        pub(super) fn release(&mut self) {
            self.mmcss = None;
            if std::mem::take(&mut self.timer_raised) {
                // SAFETY: pairs with the timeBeginPeriod in `hold`, once, since
                // `take` clears the flag.
                let _ = unsafe { timeEndPeriod(TIMER_MS) };
            }
        }
    }

    /// MMCSS registration for the lifetime of a thread.
    ///
    /// "Pro Audio" is the MMCSS task Windows reserves for low-latency audio
    /// work; it is what lets a 5 ms callback survive alongside a browser. MMCSS
    /// can be unavailable (group policy, or a container without the scheduler),
    /// so a plain time-critical thread priority is the documented consolation
    /// prize.
    pub(crate) struct MmcssGuard {
        handle: Option<HANDLE>,
        granted: ThreadPriority,
    }

    impl MmcssGuard {
        pub(crate) fn promote(priority: AVRT_PRIORITY) -> MmcssGuard {
            let mut task_index = 0u32;
            // SAFETY: the task name is a static null-terminated wide literal and
            // `task_index` is a live local; both outlive the call, which is all
            // AvSetMmThreadCharacteristicsW requires. It affects only the calling
            // thread, and the handle it returns is reverted in `Drop`.
            match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
                Ok(handle) => {
                    // SAFETY: `handle` is the live MMCSS registration just returned
                    // for this thread, and the priority is a valid AVRT_PRIORITY.
                    if let Err(err) = unsafe { AvSetMmThreadPriority(handle, priority) } {
                        tracing::warn!(%err, "AvSetMmThreadPriority failed; MMCSS default priority");
                    }
                    MmcssGuard {
                        handle: Some(handle),
                        granted: ThreadPriority::RealTime,
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, "MMCSS unavailable, falling back to thread priority");
                    // SAFETY: GetCurrentThread returns a pseudo-handle to this
                    // thread that needs no closing, and the priority constant is a
                    // valid THREAD_PRIORITY.
                    let raised = unsafe {
                        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL)
                    };
                    let granted = match raised {
                        Ok(()) => ThreadPriority::Raised,
                        Err(err) => {
                            tracing::warn!(%err, "SetThreadPriority failed; audio thread runs at normal priority");
                            ThreadPriority::Refused
                        }
                    };
                    MmcssGuard {
                        handle: None,
                        granted,
                    }
                }
            }
        }

        pub(crate) fn granted(&self) -> ThreadPriority {
            self.granted
        }
    }

    impl Drop for MmcssGuard {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                // SAFETY: `handle` came from AvSetMmThreadCharacteristicsW on this
                // thread and is reverted exactly once, since `take` consumes it.
                let _ = unsafe { AvRevertMmThreadCharacteristics(handle) };
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::time::Duration;

    use mach2::kern_return::KERN_SUCCESS;
    use mach2::mach_time::{mach_timebase_info, mach_timebase_info_data_t};
    use mach2::port::mach_port_t;
    use mach2::thread_policy::{
        THREAD_STANDARD_POLICY, THREAD_STANDARD_POLICY_COUNT, THREAD_TIME_CONSTRAINT_POLICY,
        THREAD_TIME_CONSTRAINT_POLICY_COUNT, thread_policy_set, thread_policy_t,
        thread_standard_policy, thread_time_constraint_policy,
    };

    use super::ThreadPriority;

    /// Share of the period the thread declares as its own work. The kernel holds
    /// a real-time thread to what it declared, and half a period is the ask
    /// every audio host makes; work reaching the whole period is a thread that
    /// wants the core to itself, which the kernel is right to refuse.
    const COMPUTATION_SHARE: u32 = 2;

    pub(super) struct Held {
        thread: Option<mach_port_t>,
    }

    pub(super) fn hold(period: Duration) -> (ThreadPriority, Held) {
        let mut timebase = mach_timebase_info_data_t { numer: 0, denom: 0 };
        // SAFETY: `timebase` is a live local and is the only thing the call
        // writes to.
        if unsafe { mach_timebase_info(&mut timebase) } != KERN_SUCCESS || timebase.numer == 0 {
            tracing::warn!("mach_timebase_info failed; the worker thread stays at normal priority");
            return (ThreadPriority::Refused, Held { thread: None });
        }
        let ticks = |d: Duration| -> u32 {
            let ticks = d.as_nanos() * u128::from(timebase.denom) / u128::from(timebase.numer);
            u32::try_from(ticks).unwrap_or(u32::MAX)
        };
        let mut policy = thread_time_constraint_policy {
            period: ticks(period),
            computation: ticks(period / COMPUTATION_SHARE),
            constraint: ticks(period),
            // Preemptible, because this thread takes a lock the paint thread
            // also takes: a thread the kernel may not preempt is one that can
            // hold a core against the process it shares state with.
            preemptible: 1,
        };
        // SAFETY: pthread_self is the calling thread and the port it maps to
        // needs no release, unlike the one mach_thread_self hands out.
        let thread = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
        // SAFETY: the policy is a live local of the flavor named, and the count
        // is the one mach derives from that flavor's own size.
        let set = unsafe {
            thread_policy_set(
                thread,
                THREAD_TIME_CONSTRAINT_POLICY,
                (&raw mut policy) as thread_policy_t,
                THREAD_TIME_CONSTRAINT_POLICY_COUNT,
            )
        };
        if set != KERN_SUCCESS {
            tracing::warn!(
                code = set,
                "thread_policy_set refused a real-time policy; the worker thread stays at \
                 normal priority"
            );
            return (ThreadPriority::Refused, Held { thread: None });
        }
        (
            ThreadPriority::RealTime,
            Held {
                thread: Some(thread),
            },
        )
    }

    impl Held {
        pub(super) fn release(&mut self) {
            let Some(thread) = self.thread.take() else {
                return;
            };
            let mut standard = thread_standard_policy { no_data: 0 };
            // SAFETY: the standard policy carries no data, so mach reads none of
            // the live local this points at, and the thread port is the one the
            // policy was set on.
            let _ = unsafe {
                thread_policy_set(
                    thread,
                    THREAD_STANDARD_POLICY,
                    (&raw mut standard) as thread_policy_t,
                    THREAD_STANDARD_POLICY_COUNT,
                )
            };
        }
    }
}

/// Every platform without an unprivileged way to ask. On Linux a real-time
/// scheduling class needs `RLIMIT_RTPRIO` or a session-bus privilege an
/// installed app cannot count on, so this asks for nothing rather than
/// pretending to have asked.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod imp {
    use std::time::Duration;

    use super::ThreadPriority;

    pub(super) struct Held;

    pub(super) fn hold(_period: Duration) -> (ThreadPriority, Held) {
        (ThreadPriority::Unchanged, Held)
    }

    impl Held {
        pub(super) fn release(&mut self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard has to come back with what this platform can actually give,
    /// and a platform with an API has to have used it: a priority that silently
    /// changes nothing on the platform a fault was reported on is worse than
    /// none, because it reads as fixed.
    #[test]
    fn the_calling_thread_gets_what_this_platform_offers() {
        let raised = AudioPriority::raise_current_thread(Duration::from_micros(2_500));
        let granted = raised.granted();
        println!("this platform granted {granted:?}");

        #[cfg(target_os = "macos")]
        assert_eq!(
            granted,
            ThreadPriority::RealTime,
            "macOS takes a mach time-constraint policy from any thread that asks"
        );
        // MMCSS registration can be refused by policy, and the fallback is a
        // priority every thread may set, so the floor is Raised.
        #[cfg(target_os = "windows")]
        assert!(
            matches!(granted, ThreadPriority::RealTime | ThreadPriority::Raised),
            "Windows has both calls; {granted:?} means neither was made"
        );
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(
            granted,
            ThreadPriority::Unchanged,
            "this platform has nothing to ask for, and has to say so"
        );
    }

    /// Raising and releasing repeatedly leaves the thread usable, which is what
    /// a reopened session does to the platform state a guard holds.
    #[test]
    fn a_guard_can_be_taken_and_given_back_again() {
        for _ in 0..4 {
            let raised = AudioPriority::raise_current_thread(Duration::from_micros(2_500));
            assert_ne!(raised.granted(), ThreadPriority::Refused);
        }
        let again = AudioPriority::raise_current_thread(Duration::from_micros(2_500));
        assert_ne!(again.granted(), ThreadPriority::Refused);
    }
}
