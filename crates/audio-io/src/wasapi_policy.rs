//! Exclusive-mode failure classification, the shared-mode fallback decision
//! table, and device period arithmetic.
//!
//! None of this touches a Windows API, so all of it is unit-testable on any
//! host. The HRESULT values are duplicated here as plain `i32`s for exactly
//! that reason; a `cfg(windows)` test asserts they still equal the constants
//! in the `windows` crate, so the duplication cannot drift silently.

use std::time::{Duration, Instant};

/// `AUDCLNT_E_*` and `E_*` values we key decisions on.
///
/// Sourced from `windows::Win32::Media::Audio` (0.62) and verified against it
/// by [`tests::constants_match_the_windows_crate`] on Windows.
mod hr {
    pub(super) const AUDCLNT_E_NOT_INITIALIZED: i32 = 0x8889_0001_u32 as i32;
    pub(super) const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004_u32 as i32;
    pub(super) const AUDCLNT_E_BUFFER_TOO_LARGE: i32 = 0x8889_0006_u32 as i32;
    pub(super) const AUDCLNT_E_UNSUPPORTED_FORMAT: i32 = 0x8889_0008_u32 as i32;
    pub(super) const AUDCLNT_E_DEVICE_IN_USE: i32 = 0x8889_000A_u32 as i32;
    pub(super) const AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED: i32 = 0x8889_000E_u32 as i32;
    pub(super) const AUDCLNT_E_ENDPOINT_CREATE_FAILED: i32 = 0x8889_000F_u32 as i32;
    pub(super) const AUDCLNT_E_SERVICE_NOT_RUNNING: i32 = 0x8889_0010_u32 as i32;
    pub(super) const AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED: i32 = 0x8889_0019_u32 as i32;
    pub(super) const AUDCLNT_E_INVALID_DEVICE_PERIOD: i32 = 0x8889_0020_u32 as i32;
    pub(super) const AUDCLNT_E_RESOURCES_INVALIDATED: i32 = 0x8889_0026_u32 as i32;
    pub(super) const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
}

/// Why an exclusive-mode open did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExclusiveFailure {
    /// The request itself is impossible (zero channels). Not a device problem;
    /// shared mode would reject it too.
    InvalidConfig,
    /// The requested endpoint is not present, or vanished mid-open.
    DeviceNotFound,
    /// The driver rejected every format we offered at the requested rate.
    UnsupportedFormat,
    /// Another process already holds the endpoint in exclusive mode.
    DeviceInUse,
    /// "Allow applications to take exclusive control" is off for the endpoint.
    ExclusiveNotAllowed,
    /// The driver wants a buffer size we did not align to; we retry once with
    /// the size it names, so reaching here means the retry failed too.
    BufferSizeNotAligned,
    /// The period we asked for is outside what the driver accepts.
    InvalidDevicePeriod,
    /// The driver could not create the endpoint (typically a busy or wedged
    /// device that does not report `DEVICE_IN_USE`).
    EndpointCreateFailed,
    /// The endpoint was invalidated: unplugged, disabled, or its resources
    /// were pulled out from under us.
    DeviceInvalidated,
    /// The Windows audio service is not running.
    ServiceNotRunning,
    /// Anything else, including a thread that died before reporting.
    Other,
}

impl ExclusiveFailure {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConfig => "invalid configuration",
            Self::DeviceNotFound => "device not found",
            Self::UnsupportedFormat => "no exclusive-mode format accepted",
            Self::DeviceInUse => "device held exclusively by another application",
            Self::ExclusiveNotAllowed => "exclusive mode disabled for this endpoint",
            Self::BufferSizeNotAligned => "driver rejected the buffer alignment",
            Self::InvalidDevicePeriod => "driver rejected the device period",
            Self::EndpointCreateFailed => "driver failed to create the endpoint",
            Self::DeviceInvalidated => "device invalidated",
            Self::ServiceNotRunning => "windows audio service not running",
            Self::Other => "unclassified wasapi error",
        }
    }
}

/// What to do when exclusive mode did not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fallback {
    /// Open the cpal shared-mode stream instead. Higher latency beats silence.
    Shared,
    /// Report the error to the caller: shared mode cannot succeed either.
    Reject,
}

/// The fallback decision table.
///
/// Everything that describes a *device* condition falls back to shared mode,
/// because every one of those conditions still leaves a working shared-mode
/// endpoint: an exclusive holder does not block shared clients, a driver that
/// refuses our formats still talks to the audio engine, and a disabled
/// exclusive-mode checkbox is exactly the case the fallback exists for. Only a
/// malformed request is rejected, since shared mode would reject it too and
/// the clearer error is the useful one.
pub(crate) const fn fallback_decision(failure: ExclusiveFailure) -> Fallback {
    match failure {
        ExclusiveFailure::InvalidConfig => Fallback::Reject,
        ExclusiveFailure::DeviceNotFound
        | ExclusiveFailure::UnsupportedFormat
        | ExclusiveFailure::DeviceInUse
        | ExclusiveFailure::ExclusiveNotAllowed
        | ExclusiveFailure::BufferSizeNotAligned
        | ExclusiveFailure::InvalidDevicePeriod
        | ExclusiveFailure::EndpointCreateFailed
        | ExclusiveFailure::DeviceInvalidated
        | ExclusiveFailure::ServiceNotRunning
        | ExclusiveFailure::Other => Fallback::Shared,
    }
}

/// How long to stop attempting exclusive mode for the same request after this
/// failure.
///
/// The client reopens a dead or reconfigured stream on a 500 ms cadence, so
/// without a cooldown a musician whose interface is owned by a DAW would pay
/// (and log) a doomed exclusive probe twice a second forever. The durations
/// track how likely the condition is to clear on its own: a settings toggle or
/// a driver's format list will not change mid-session, another application's
/// grip might, and an invalidated device means the next open sees different
/// hardware, so it gets no cooldown at all.
pub(crate) const fn retry_cooldown(failure: ExclusiveFailure) -> Duration {
    match failure {
        // Static properties of the endpoint or its driver.
        ExclusiveFailure::ExclusiveNotAllowed
        | ExclusiveFailure::UnsupportedFormat
        | ExclusiveFailure::BufferSizeNotAligned
        | ExclusiveFailure::InvalidDevicePeriod => Duration::from_secs(60),
        // Might clear when another application lets go.
        ExclusiveFailure::DeviceInUse
        | ExclusiveFailure::EndpointCreateFailed
        | ExclusiveFailure::ServiceNotRunning
        | ExclusiveFailure::Other => Duration::from_secs(10),
        // The next open is against a different device, or is not a device
        // problem at all: retry immediately.
        ExclusiveFailure::DeviceNotFound
        | ExclusiveFailure::DeviceInvalidated
        | ExclusiveFailure::InvalidConfig => Duration::ZERO,
    }
}

/// Classify a WASAPI HRESULT.
pub(crate) const fn classify_hresult(code: i32) -> ExclusiveFailure {
    match code {
        hr::AUDCLNT_E_DEVICE_IN_USE => ExclusiveFailure::DeviceInUse,
        hr::AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED => ExclusiveFailure::ExclusiveNotAllowed,
        hr::AUDCLNT_E_UNSUPPORTED_FORMAT => ExclusiveFailure::UnsupportedFormat,
        hr::AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED => ExclusiveFailure::BufferSizeNotAligned,
        hr::AUDCLNT_E_INVALID_DEVICE_PERIOD | hr::AUDCLNT_E_BUFFER_TOO_LARGE => {
            ExclusiveFailure::InvalidDevicePeriod
        }
        hr::AUDCLNT_E_ENDPOINT_CREATE_FAILED => ExclusiveFailure::EndpointCreateFailed,
        hr::AUDCLNT_E_DEVICE_INVALIDATED
        | hr::AUDCLNT_E_RESOURCES_INVALIDATED
        | hr::AUDCLNT_E_NOT_INITIALIZED => ExclusiveFailure::DeviceInvalidated,
        hr::AUDCLNT_E_SERVICE_NOT_RUNNING => ExclusiveFailure::ServiceNotRunning,
        // A bad period or format reaches IAudioClient::Initialize as
        // E_INVALIDARG on some drivers rather than a specific AUDCLNT code.
        hr::E_INVALIDARG => ExclusiveFailure::InvalidDevicePeriod,
        _ => ExclusiveFailure::Other,
    }
}

/// True when this HRESULT means the endpoint itself is gone, as opposed to a
/// driver or plumbing fault on a device that is still there.
///
/// A running stream latches as errored either way, because the recovery is the
/// same (close, then reopen on the default endpoint); this only separates
/// "someone unplugged the interface" from "the driver misbehaved" in the log,
/// where the difference decides whether a human should suspect their hardware.
pub(crate) const fn is_device_loss(code: i32) -> bool {
    matches!(
        code,
        hr::AUDCLNT_E_DEVICE_INVALIDATED
            | hr::AUDCLNT_E_RESOURCES_INVALIDATED
            | hr::AUDCLNT_E_SERVICE_NOT_RUNNING
            | hr::AUDCLNT_E_NOT_INITIALIZED
    )
}

/// Floor on the exclusive-mode period we will ask for, in frames. Below this
/// the callback rate stops being serviceable even with MMCSS, and the driver
/// would raise it to its own minimum anyway.
pub(crate) const MIN_PERIOD_FRAMES: u32 = 32;

/// Ceiling on the exclusive-mode period we will ask for, in frames: 100 ms at
/// 48 kHz. Anything larger is past the point where exclusive mode is worth
/// having, and asking for it risks `AUDCLNT_E_BUFFER_TOO_LARGE`.
pub(crate) const MAX_PERIOD_FRAMES: u32 = 4_800;

/// Clamp a requested buffer size into the range we are willing to ask a driver
/// for. The driver still raises it to its own minimum period, and the value
/// actually negotiated is what the stream reports as its latency.
pub(crate) const fn clamp_period_frames(requested: u32) -> u32 {
    if requested < MIN_PERIOD_FRAMES {
        MIN_PERIOD_FRAMES
    } else if requested > MAX_PERIOD_FRAMES {
        MAX_PERIOD_FRAMES
    } else {
        requested
    }
}

/// Frames expressed as a period in 100 ns units, the unit
/// `IAudioClient::Initialize` takes.
///
/// Same rounding as `wasapi::calculate_period_100ns`, reimplemented here so
/// the arithmetic is testable off Windows.
pub(crate) fn period_100ns(frames: u32, sample_rate: u32) -> i64 {
    if sample_rate == 0 {
        return 0;
    }
    ((10_000.0 * 1000.0 / f64::from(sample_rate) * f64::from(frames)) + 0.5) as i64
}

/// Frames a period in 100 ns units corresponds to, rounded up so a converted
/// period never claims to hold fewer frames than it does.
pub(crate) fn period_frames(period_hns: i64, sample_rate: u32) -> u32 {
    if period_hns <= 0 {
        return 0;
    }
    let frames = (period_hns as f64 * f64::from(sample_rate) / 10_000_000.0).ceil();
    frames.max(0.0).min(f64::from(u32::MAX)) as u32
}

/// Holds off exclusive-mode probes for a request that just failed.
///
/// One request at a time is enough: the client only ever runs one duplex
/// stream, so a differing request means the user changed device or buffer size
/// and the old verdict no longer applies. Time is passed in rather than read so
/// the expiry logic is testable without sleeping.
#[derive(Debug)]
pub(crate) struct RetryGate<K> {
    blocked: Option<(K, Instant)>,
}

impl<K: PartialEq> RetryGate<K> {
    pub(crate) const fn new() -> Self {
        Self { blocked: None }
    }

    /// How much longer `key` stays gated, or `None` if it may be tried now.
    pub(crate) fn remaining(&self, key: &K, now: Instant) -> Option<Duration> {
        let (blocked, until) = self.blocked.as_ref()?;
        if blocked != key {
            return None;
        }
        // Zero remaining is expired, not "gated for no time".
        until
            .checked_duration_since(now)
            .filter(|remaining| !remaining.is_zero())
    }

    /// Gate `key` for `cooldown`. A zero cooldown clears the gate instead of
    /// setting one, so a failure that should be retried at once is not gated by
    /// a stale verdict about some other request.
    pub(crate) fn block(&mut self, key: K, cooldown: Duration, now: Instant) {
        self.blocked = (!cooldown.is_zero()).then(|| (key, now + cooldown));
    }

    pub(crate) fn clear(&mut self) {
        self.blocked = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_device_condition_falls_back_to_shared() {
        for failure in [
            ExclusiveFailure::DeviceNotFound,
            ExclusiveFailure::UnsupportedFormat,
            ExclusiveFailure::DeviceInUse,
            ExclusiveFailure::ExclusiveNotAllowed,
            ExclusiveFailure::BufferSizeNotAligned,
            ExclusiveFailure::InvalidDevicePeriod,
            ExclusiveFailure::EndpointCreateFailed,
            ExclusiveFailure::DeviceInvalidated,
            ExclusiveFailure::ServiceNotRunning,
            ExclusiveFailure::Other,
        ] {
            assert_eq!(
                fallback_decision(failure),
                Fallback::Shared,
                "{failure:?} must fall back rather than fail the open"
            );
        }
    }

    #[test]
    fn only_a_malformed_request_is_rejected() {
        assert_eq!(
            fallback_decision(ExclusiveFailure::InvalidConfig),
            Fallback::Reject
        );
    }

    #[test]
    fn cooldowns_match_how_likely_the_condition_is_to_clear() {
        assert_eq!(
            retry_cooldown(ExclusiveFailure::ExclusiveNotAllowed),
            Duration::from_secs(60)
        );
        assert_eq!(
            retry_cooldown(ExclusiveFailure::UnsupportedFormat),
            Duration::from_secs(60)
        );
        assert_eq!(
            retry_cooldown(ExclusiveFailure::DeviceInUse),
            Duration::from_secs(10)
        );
        assert_eq!(
            retry_cooldown(ExclusiveFailure::Other),
            Duration::from_secs(10)
        );
        assert_eq!(
            retry_cooldown(ExclusiveFailure::DeviceInvalidated),
            Duration::ZERO
        );
        assert_eq!(
            retry_cooldown(ExclusiveFailure::DeviceNotFound),
            Duration::ZERO
        );
    }

    #[test]
    fn hresults_classify_to_their_conditions() {
        let cases = [
            (0x8889_000A_u32, ExclusiveFailure::DeviceInUse),
            (0x8889_000E_u32, ExclusiveFailure::ExclusiveNotAllowed),
            (0x8889_0008_u32, ExclusiveFailure::UnsupportedFormat),
            (0x8889_0019_u32, ExclusiveFailure::BufferSizeNotAligned),
            (0x8889_0020_u32, ExclusiveFailure::InvalidDevicePeriod),
            (0x8889_0006_u32, ExclusiveFailure::InvalidDevicePeriod),
            (0x8007_0057_u32, ExclusiveFailure::InvalidDevicePeriod),
            (0x8889_000F_u32, ExclusiveFailure::EndpointCreateFailed),
            (0x8889_0004_u32, ExclusiveFailure::DeviceInvalidated),
            (0x8889_0026_u32, ExclusiveFailure::DeviceInvalidated),
            (0x8889_0001_u32, ExclusiveFailure::DeviceInvalidated),
            (0x8889_0010_u32, ExclusiveFailure::ServiceNotRunning),
            (0x8000_4005_u32, ExclusiveFailure::Other),
        ];
        for (code, want) in cases {
            assert_eq!(classify_hresult(code as i32), want, "code {code:#010x}");
        }
    }

    #[test]
    fn device_loss_is_the_invalidated_family_only() {
        for code in [
            0x8889_0004_u32,
            0x8889_0026_u32,
            0x8889_0010_u32,
            0x8889_0001_u32,
        ] {
            assert!(is_device_loss(code as i32), "{code:#010x} is device loss");
        }
        for code in [
            0x8889_000A_u32,
            0x8889_000E_u32,
            0x8889_0008_u32,
            0x8889_0019_u32,
            0x8007_0057_u32,
        ] {
            assert!(
                !is_device_loss(code as i32),
                "{code:#010x} is not device loss"
            );
        }
    }

    #[test]
    fn every_failure_has_a_message() {
        for failure in [
            ExclusiveFailure::InvalidConfig,
            ExclusiveFailure::DeviceNotFound,
            ExclusiveFailure::UnsupportedFormat,
            ExclusiveFailure::DeviceInUse,
            ExclusiveFailure::ExclusiveNotAllowed,
            ExclusiveFailure::BufferSizeNotAligned,
            ExclusiveFailure::InvalidDevicePeriod,
            ExclusiveFailure::EndpointCreateFailed,
            ExclusiveFailure::DeviceInvalidated,
            ExclusiveFailure::ServiceNotRunning,
            ExclusiveFailure::Other,
        ] {
            assert!(!failure.as_str().is_empty());
        }
    }

    #[test]
    fn buffer_size_clamps_into_the_askable_range() {
        assert_eq!(clamp_period_frames(240), 240);
        assert_eq!(clamp_period_frames(0), MIN_PERIOD_FRAMES);
        assert_eq!(clamp_period_frames(1), MIN_PERIOD_FRAMES);
        assert_eq!(clamp_period_frames(MIN_PERIOD_FRAMES), MIN_PERIOD_FRAMES);
        assert_eq!(clamp_period_frames(MAX_PERIOD_FRAMES), MAX_PERIOD_FRAMES);
        assert_eq!(clamp_period_frames(u32::MAX), MAX_PERIOD_FRAMES);
        assert_eq!(clamp_period_frames(4_801), MAX_PERIOD_FRAMES);
    }

    #[test]
    fn period_conversion_matches_the_wasapi_formula() {
        // 240 frames at 48 kHz is 5 ms, i.e. 50_000 units of 100 ns.
        assert_eq!(period_100ns(240, 48_000), 50_000);
        assert_eq!(period_100ns(480, 48_000), 100_000);
        assert_eq!(period_100ns(0, 48_000), 0);
        assert_eq!(period_100ns(240, 0), 0);
        // 3 ms is the Windows default shared-mode period.
        assert_eq!(period_100ns(144, 48_000), 30_000);
    }

    #[test]
    fn period_frames_round_trips_exact_periods() {
        // A frame is 208.33 ns, so only multiples of three land on a whole
        // number of 100 ns units and survive the trip unchanged.
        for frames in [96u32, 144, 240, 480, 960, 4_800] {
            let hns = period_100ns(frames, 48_000);
            assert_eq!(period_frames(hns, 48_000), frames, "{frames} frames");
        }
    }

    #[test]
    fn period_frames_never_under_reports() {
        // The value describes a device's minimum period, so rounding down
        // would advertise a buffer the device cannot actually run.
        for frames in [1u32, 32, 33, 128, 1_024] {
            let hns = period_100ns(frames, 48_000);
            let back = period_frames(hns, 48_000);
            assert!(
                back == frames || back == frames + 1,
                "{frames} frames became {back}"
            );
            assert!(back >= frames, "{frames} frames rounded down to {back}");
        }
        assert_eq!(period_frames(30_001, 48_000), 145);
        assert_eq!(period_frames(0, 48_000), 0);
        assert_eq!(period_frames(-1, 48_000), 0);
    }

    #[test]
    fn gate_blocks_only_the_request_that_failed() {
        let now = Instant::now();
        let mut gate = RetryGate::new();
        gate.block("device-a", Duration::from_secs(10), now);

        assert_eq!(
            gate.remaining(&"device-a", now),
            Some(Duration::from_secs(10))
        );
        assert_eq!(gate.remaining(&"device-b", now), None);
    }

    #[test]
    fn gate_expires_on_its_own() {
        let now = Instant::now();
        let mut gate = RetryGate::new();
        gate.block("device-a", Duration::from_secs(10), now);

        assert_eq!(
            gate.remaining(&"device-a", now + Duration::from_secs(9)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            gate.remaining(&"device-a", now + Duration::from_secs(10)),
            None
        );
        assert_eq!(
            gate.remaining(&"device-a", now + Duration::from_secs(11)),
            None
        );
    }

    #[test]
    fn a_zero_cooldown_leaves_nothing_gated() {
        let now = Instant::now();
        let mut gate = RetryGate::new();
        gate.block("device-a", Duration::from_secs(10), now);
        // A device-invalidated failure carries no cooldown, and must not leave
        // the previous verdict in place.
        gate.block("device-a", Duration::ZERO, now);
        assert_eq!(gate.remaining(&"device-a", now), None);
    }

    #[test]
    fn success_clears_the_gate() {
        let now = Instant::now();
        let mut gate = RetryGate::new();
        gate.block("device-a", Duration::from_secs(60), now);
        gate.clear();
        assert_eq!(gate.remaining(&"device-a", now), None);
    }

    #[test]
    fn a_new_failure_replaces_the_old_verdict() {
        let now = Instant::now();
        let mut gate = RetryGate::new();
        gate.block("device-a", Duration::from_secs(60), now);
        gate.block("device-b", Duration::from_secs(10), now);
        assert_eq!(gate.remaining(&"device-a", now), None);
        assert_eq!(
            gate.remaining(&"device-b", now),
            Some(Duration::from_secs(10))
        );
    }

    /// The whole point of the table: a device condition never fails the open,
    /// and whatever cooldown it carries is one the gate can actually apply.
    #[test]
    fn table_and_gate_agree_on_every_failure() {
        let now = Instant::now();
        for failure in [
            ExclusiveFailure::InvalidConfig,
            ExclusiveFailure::DeviceNotFound,
            ExclusiveFailure::UnsupportedFormat,
            ExclusiveFailure::DeviceInUse,
            ExclusiveFailure::ExclusiveNotAllowed,
            ExclusiveFailure::BufferSizeNotAligned,
            ExclusiveFailure::InvalidDevicePeriod,
            ExclusiveFailure::EndpointCreateFailed,
            ExclusiveFailure::DeviceInvalidated,
            ExclusiveFailure::ServiceNotRunning,
            ExclusiveFailure::Other,
        ] {
            let cooldown = retry_cooldown(failure);
            let mut gate = RetryGate::new();
            gate.block("request", cooldown, now);
            let gated = gate.remaining(&"request", now).is_some();
            assert_eq!(
                gated,
                !cooldown.is_zero(),
                "{failure:?} gating disagrees with its cooldown"
            );
            // Nothing is gated for longer than a minute: an exclusive-capable
            // device must not stay stuck in shared mode for a whole session.
            assert!(cooldown <= Duration::from_secs(60), "{failure:?}");
        }
    }

    /// The HRESULT values above are hand-copied so the table can be tested off
    /// Windows; on Windows, prove they are still the real ones.
    #[cfg(target_os = "windows")]
    #[test]
    fn constants_match_the_windows_crate() {
        use windows::Win32::Foundation::E_INVALIDARG;
        use windows::Win32::Media::Audio::{
            AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_BUFFER_TOO_LARGE, AUDCLNT_E_DEVICE_IN_USE,
            AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_ENDPOINT_CREATE_FAILED,
            AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED, AUDCLNT_E_INVALID_DEVICE_PERIOD,
            AUDCLNT_E_NOT_INITIALIZED, AUDCLNT_E_RESOURCES_INVALIDATED,
            AUDCLNT_E_SERVICE_NOT_RUNNING, AUDCLNT_E_UNSUPPORTED_FORMAT,
        };

        for (ours, theirs) in [
            (hr::AUDCLNT_E_NOT_INITIALIZED, AUDCLNT_E_NOT_INITIALIZED),
            (
                hr::AUDCLNT_E_DEVICE_INVALIDATED,
                AUDCLNT_E_DEVICE_INVALIDATED,
            ),
            (hr::AUDCLNT_E_BUFFER_TOO_LARGE, AUDCLNT_E_BUFFER_TOO_LARGE),
            (
                hr::AUDCLNT_E_UNSUPPORTED_FORMAT,
                AUDCLNT_E_UNSUPPORTED_FORMAT,
            ),
            (hr::AUDCLNT_E_DEVICE_IN_USE, AUDCLNT_E_DEVICE_IN_USE),
            (
                hr::AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED,
                AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED,
            ),
            (
                hr::AUDCLNT_E_ENDPOINT_CREATE_FAILED,
                AUDCLNT_E_ENDPOINT_CREATE_FAILED,
            ),
            (
                hr::AUDCLNT_E_SERVICE_NOT_RUNNING,
                AUDCLNT_E_SERVICE_NOT_RUNNING,
            ),
            (
                hr::AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED,
                AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED,
            ),
            (
                hr::AUDCLNT_E_INVALID_DEVICE_PERIOD,
                AUDCLNT_E_INVALID_DEVICE_PERIOD,
            ),
            (
                hr::AUDCLNT_E_RESOURCES_INVALIDATED,
                AUDCLNT_E_RESOURCES_INVALIDATED,
            ),
            (hr::E_INVALIDARG, E_INVALIDARG),
        ] {
            assert_eq!(ours, theirs.0, "{theirs:?}");
        }
    }

    /// And prove our period arithmetic still matches the crate's.
    #[cfg(target_os = "windows")]
    #[test]
    fn period_math_matches_the_wasapi_crate() {
        for frames in [32u32, 96, 144, 240, 480, 1_024, 4_800] {
            assert_eq!(
                period_100ns(frames, 48_000),
                wasapi::calculate_period_100ns(i64::from(frames), 48_000),
                "{frames} frames"
            );
        }
    }
}
