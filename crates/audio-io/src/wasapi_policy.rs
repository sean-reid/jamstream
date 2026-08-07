//! Exclusive-mode failure classification, the shared-mode fallback decision
//! table, device period arithmetic, and the device thread's liveness rule.
//!
//! None of this touches a Windows API, so all of it is unit-testable on any
//! host, which is the point: `wasapi_backend` itself only compiles on Windows
//! and only runs against a real endpoint. The HRESULT values are duplicated
//! here as plain `i32`s for the same reason; a `cfg(windows)` test asserts they
//! still equal the constants in the `windows` crate, so the duplication cannot
//! drift silently.

use std::time::{Duration, Instant};

use crate::types::AudioError;

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
    pub(super) const E_ACCESSDENIED: i32 = 0x8007_0005_u32 as i32;
}

/// What to do about a Windows microphone privacy denial, appended to both the
/// exclusive-path error and the cpal shared-path one so the two paths cannot
/// drift apart.
pub(crate) const MIC_PRIVACY_REMEDY: &str = "allow desktop apps to access your \
     microphone in Settings, Privacy and security, Microphone";

/// Why an exclusive-mode open did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExclusiveFailure {
    /// The request itself is impossible (zero channels). Not a device problem;
    /// shared mode would reject it too.
    InvalidConfig,
    /// The requested endpoint is not present, or vanished mid-open.
    DeviceNotFound,
    /// The requested endpoint is one this machine has facing the other way: a
    /// playback endpoint asked for as capture, or the reverse.
    WrongDirection,
    /// The driver rejected every format we offered at the requested rate.
    UnsupportedFormat,
    /// Another process already holds the endpoint in exclusive mode.
    DeviceInUse,
    /// Windows denied access to the endpoint (`E_ACCESSDENIED`), which in
    /// practice is the microphone privacy toggle: "Let desktop apps access
    /// your microphone" is off.
    AccessDenied,
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
            Self::DeviceNotFound => "requested endpoint is not present",
            Self::WrongDirection => "requested endpoint faces the other direction",
            Self::UnsupportedFormat => "no exclusive-mode format accepted",
            Self::DeviceInUse => "device held exclusively by another application",
            Self::AccessDenied => "microphone access denied by Windows privacy settings",
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
/// A condition falls back to shared mode only when a shared-mode stream can
/// actually survive it: a driver that refuses our exclusive formats still
/// talks to the audio engine, and a disabled exclusive-mode checkbox is
/// exactly the case the fallback exists for. The rest reject instead.
/// A malformed request would be rejected by shared mode too, and the clearer
/// error is the useful one. A device held exclusively by another application
/// blocks shared clients as well: `AUDCLNT_E_DEVICE_IN_USE` fails shared-mode
/// `Initialize` just like exclusive, so falling back only traded the
/// classifier's words for cpal's generic "temporarily busy" error (#324).
/// The microphone privacy toggle blocks every open the same way (#329).
pub(crate) const fn fallback_decision(failure: ExclusiveFailure) -> Fallback {
    match failure {
        ExclusiveFailure::InvalidConfig
        | ExclusiveFailure::DeviceInUse
        | ExclusiveFailure::AccessDenied => Fallback::Reject,
        ExclusiveFailure::DeviceNotFound
        | ExclusiveFailure::WrongDirection
        | ExclusiveFailure::UnsupportedFormat
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
/// without a cooldown an endpoint that will never open exclusively would pay
/// (and log) a doomed exclusive probe twice a second forever. The durations
/// track how likely the condition is to clear on its own: a settings toggle or
/// a driver's format list will not change mid-session, a wedged driver might
/// recover, and an invalidated device means the next open sees different
/// hardware, so it gets no cooldown at all. Rejected conditions never arm the
/// gate at all; their entries exist so the table stays total.
pub(crate) const fn retry_cooldown(failure: ExclusiveFailure) -> Duration {
    match failure {
        // Static properties of the endpoint, its driver, or a settings toggle.
        ExclusiveFailure::ExclusiveNotAllowed
        | ExclusiveFailure::WrongDirection
        | ExclusiveFailure::UnsupportedFormat
        | ExclusiveFailure::BufferSizeNotAligned
        | ExclusiveFailure::InvalidDevicePeriod
        | ExclusiveFailure::AccessDenied => Duration::from_secs(60),
        // Might clear when another application lets go or a driver recovers.
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
        // "Let desktop apps access your microphone" is off; the driver never
        // even sees the request.
        hr::E_ACCESSDENIED => ExclusiveFailure::AccessDenied,
        _ => ExclusiveFailure::Other,
    }
}

/// The error a caller sees when exclusive mode failed and the shared-mode
/// fallback could not stand in for it.
///
/// The three variants are three different jobs for the caller: `DeviceGone`
/// means reopen on whatever device is there now, `Unsupported` means the
/// request or the endpoint will never work and a human has to change
/// something, and `Backend` means something went wrong that a retry might get
/// past.
pub(crate) fn open_error(failure: ExclusiveFailure, detail: &str) -> AudioError {
    let reason = failure.as_str();
    match failure {
        ExclusiveFailure::DeviceNotFound | ExclusiveFailure::DeviceInvalidated => {
            AudioError::DeviceGone
        }
        ExclusiveFailure::InvalidConfig
        | ExclusiveFailure::UnsupportedFormat
        | ExclusiveFailure::WrongDirection => {
            AudioError::Unsupported(format!("{reason}: {detail}"))
        }
        // A privacy denial is a setting a human has to flip, so the error
        // carries the remedy along with the classification.
        ExclusiveFailure::AccessDenied => {
            AudioError::Unsupported(format!("{reason}: {detail}; {MIC_PRIVACY_REMEDY}"))
        }
        _ => AudioError::Backend(format!("{reason}: {detail}")),
    }
}

/// Where a requested endpoint id sits in the enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endpoint {
    /// Position of the id among the ids given for the direction being opened.
    At(usize),
    /// Nothing to open, and why not.
    Missing(ExclusiveFailure),
}

/// Find the endpoint a requested id names among those the machine enumerates
/// for the direction being opened.
///
/// Resolving inside one direction's own endpoints is what keeps a playback
/// endpoint from ever being opened for capture. `others` lists the opposite
/// direction's ids and is called only when the wanted direction does not hold
/// the id, because an endpoint the machine has facing the other way is a
/// different problem from one it does not have at all, and sending someone to
/// look for a missing device that is plugged in and working wastes their
/// evening.
pub(crate) fn find_endpoint(
    id: &str,
    wanted: &[&str],
    others: impl FnOnce() -> Vec<String>,
) -> Endpoint {
    if let Some(index) = wanted.iter().position(|candidate| *candidate == id) {
        return Endpoint::At(index);
    }
    if others().iter().any(|candidate| candidate == id) {
        return Endpoint::Missing(ExclusiveFailure::WrongDirection);
    }
    Endpoint::Missing(ExclusiveFailure::DeviceNotFound)
}

/// How long a device thread blocks on its buffer event before looking at the
/// stop flag again. Also the granularity of stream teardown: both threads are
/// signalled together, so a close costs about this much in the worst case.
pub(crate) const EVENT_WAIT_MS: u32 = 50;

/// Consecutive event waits that may expire before the stream is declared dead.
/// At [`EVENT_WAIT_MS`] this is half a second of silence from a device that
/// should be signalling every few milliseconds, which lines up with the
/// client's own 500 ms reopen cadence.
pub(crate) const MAX_CONSECUTIVE_TIMEOUTS: u32 = 10;

/// True once the device has missed enough consecutive buffer events to be dead
/// rather than late.
pub(crate) const fn stream_is_dead(consecutive_timeouts: u32) -> bool {
    consecutive_timeouts >= MAX_CONSECUTIVE_TIMEOUTS
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

/// The buffer range to advertise for an endpoint whose driver reports
/// `min_period_hns` as its minimum exclusive-mode period, or None when this
/// backend would never open it.
///
/// The floor is ours as well as the driver's: below [`MIN_PERIOD_FRAMES`] the
/// callback rate stops being serviceable whatever the driver claims, and the
/// ceiling is simply the largest period this backend will ever ask for, so
/// promising more would be promising something it will not do. A driver whose
/// own minimum is above that ceiling gets None rather than an inverted range,
/// because "no exclusive bounds" is what the enumeration already means by an
/// endpoint exclusive mode is not available on.
pub(crate) fn exclusive_period_bounds(min_period_hns: i64, sample_rate: u32) -> Option<(u32, u32)> {
    let device_min = period_frames(min_period_hns, sample_rate);
    (device_min <= MAX_PERIOD_FRAMES)
        .then_some((device_min.max(MIN_PERIOD_FRAMES), MAX_PERIOD_FRAMES))
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
    fn conditions_a_shared_stream_survives_fall_back_to_shared() {
        for failure in [
            ExclusiveFailure::DeviceNotFound,
            ExclusiveFailure::WrongDirection,
            ExclusiveFailure::UnsupportedFormat,
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

    /// Shared mode cannot save any of these: it rejects a malformed request
    /// too, `AUDCLNT_E_DEVICE_IN_USE` fails shared-mode `Initialize` while
    /// another process holds the endpoint exclusively, and the microphone
    /// privacy toggle blocks shared and exclusive opens alike.
    #[test]
    fn conditions_shared_mode_cannot_save_are_rejected() {
        for failure in [
            ExclusiveFailure::InvalidConfig,
            ExclusiveFailure::DeviceInUse,
            ExclusiveFailure::AccessDenied,
        ] {
            assert_eq!(fallback_decision(failure), Fallback::Reject, "{failure:?}");
        }
    }

    /// The whole point of rejecting DeviceInUse: the words the user sees name
    /// the exclusive holder instead of cpal's generic "temporarily busy", which
    /// is what falling back produces.
    #[test]
    fn a_device_held_exclusively_rejects_with_the_classifier_words() {
        let message = open_error(
            ExclusiveFailure::DeviceInUse,
            "IAudioClient::Initialize: 0x8889000A",
        )
        .to_string();
        assert!(
            message.contains("device held exclusively by another application"),
            "{message}"
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
            retry_cooldown(ExclusiveFailure::AccessDenied),
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
        // A picked endpoint does not turn around mid-session, so re-probing it
        // twice a second only fills the log.
        assert_eq!(
            retry_cooldown(ExclusiveFailure::WrongDirection),
            Duration::from_secs(60)
        );
    }

    /// The two endpoint ids from a Windows 10 joiner's log. The data flow is
    /// the third field of the prefix: 0 is render, 1 is capture.
    const RENDER_ID: &str = "{0.0.0.00000000}.{49c3b8e4-1fb7-4d7c-8ee3-1f4b30ccb591}";
    const CAPTURE_ID: &str = "{0.0.1.00000000}.{9d2f0b3c-6a41-4f0e-9c7a-2b8e5d1a7f60}";

    /// The endpoint being opened is the one the requested direction lists, so
    /// an id belonging to the other direction cannot resolve to a device at
    /// all, and it must not be reported as a device that is not there: the
    /// device is present, plugged in, and working, and "device not found" sends
    /// its owner looking for hardware instead of at their selection.
    #[test]
    fn an_id_from_the_other_direction_is_a_direction_mismatch_not_a_missing_device() {
        let found = find_endpoint(RENDER_ID, &[CAPTURE_ID], || vec![RENDER_ID.to_owned()]);
        assert_eq!(
            found,
            Endpoint::Missing(ExclusiveFailure::WrongDirection),
            "a render id offered as capture is a mismatch"
        );
        let message = open_error(ExclusiveFailure::WrongDirection, RENDER_ID).to_string();
        assert!(message.contains("faces the other direction"), "{message}");
        assert!(message.contains(RENDER_ID), "{message}");
        assert_ne!(
            ExclusiveFailure::WrongDirection.as_str(),
            ExclusiveFailure::DeviceNotFound.as_str()
        );
    }

    #[test]
    fn an_id_no_direction_has_is_a_missing_device() {
        let found = find_endpoint("{0.0.1.00000000}.{gone}", &[CAPTURE_ID], || {
            vec![RENDER_ID.to_owned()]
        });
        assert_eq!(found, Endpoint::Missing(ExclusiveFailure::DeviceNotFound));
    }

    /// The id resolves to its place in the direction's own list, and the
    /// opposite direction is not enumerated to find that out: enumeration
    /// happens on the open path, which runs on every reopen.
    #[test]
    fn an_id_this_direction_holds_resolves_without_looking_at_the_other() {
        let asked = std::cell::Cell::new(false);
        let found = find_endpoint(
            CAPTURE_ID,
            &["{0.0.1.00000000}.{other}", CAPTURE_ID],
            || {
                asked.set(true);
                Vec::new()
            },
        );
        assert_eq!(found, Endpoint::At(1));
        assert!(
            !asked.get(),
            "the other direction was enumerated for nothing"
        );
    }

    /// A mismatch still leaves the user with sound: shared mode is opened
    /// rather than the stream failing, and the gate stops the doomed exclusive
    /// probe from repeating on every reopen.
    #[test]
    fn a_direction_mismatch_falls_back_and_stops_re_deciding() {
        assert_eq!(
            fallback_decision(ExclusiveFailure::WrongDirection),
            Fallback::Shared
        );
        let now = Instant::now();
        let mut gate = RetryGate::new();
        gate.block(
            "request",
            retry_cooldown(ExclusiveFailure::WrongDirection),
            now,
        );
        assert!(gate.remaining(&"request", now).is_some());
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
            (0x8007_0005_u32, ExclusiveFailure::AccessDenied),
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
            0x8007_0005_u32,
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
            ExclusiveFailure::WrongDirection,
            ExclusiveFailure::UnsupportedFormat,
            ExclusiveFailure::DeviceInUse,
            ExclusiveFailure::AccessDenied,
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

    /// The caller acts on the variant, not the text, so each failure has to
    /// land on the variant whose recovery is the right one.
    #[test]
    fn open_errors_carry_the_recovery_the_caller_should_attempt() {
        for failure in [
            ExclusiveFailure::DeviceNotFound,
            ExclusiveFailure::DeviceInvalidated,
        ] {
            assert!(
                matches!(open_error(failure, "detail"), AudioError::DeviceGone),
                "{failure:?} means reopen on whatever is there now"
            );
        }
        for failure in [
            ExclusiveFailure::InvalidConfig,
            ExclusiveFailure::UnsupportedFormat,
            ExclusiveFailure::AccessDenied,
        ] {
            assert!(
                matches!(open_error(failure, "detail"), AudioError::Unsupported(_)),
                "{failure:?} needs a human to change something"
            );
        }
        for failure in [
            ExclusiveFailure::DeviceInUse,
            ExclusiveFailure::ExclusiveNotAllowed,
            ExclusiveFailure::BufferSizeNotAligned,
            ExclusiveFailure::InvalidDevicePeriod,
            ExclusiveFailure::EndpointCreateFailed,
            ExclusiveFailure::ServiceNotRunning,
            ExclusiveFailure::Other,
        ] {
            assert!(
                matches!(open_error(failure, "detail"), AudioError::Backend(_)),
                "{failure:?}"
            );
        }
    }

    /// Whatever the variant, the driver's own words have to survive into the
    /// message: they are the only thing that says which device and which call.
    #[test]
    fn an_open_error_keeps_the_detail_it_was_given() {
        for failure in [
            ExclusiveFailure::InvalidConfig,
            ExclusiveFailure::UnsupportedFormat,
            ExclusiveFailure::DeviceInUse,
            ExclusiveFailure::AccessDenied,
            ExclusiveFailure::ExclusiveNotAllowed,
            ExclusiveFailure::BufferSizeNotAligned,
            ExclusiveFailure::InvalidDevicePeriod,
            ExclusiveFailure::EndpointCreateFailed,
            ExclusiveFailure::ServiceNotRunning,
            ExclusiveFailure::Other,
        ] {
            let message = open_error(failure, "IAudioClient::Initialize: 0x88890008").to_string();
            assert!(message.contains("0x88890008"), "{failure:?}: {message}");
            assert!(message.contains(failure.as_str()), "{failure:?}: {message}");
        }
    }

    /// The privacy toggle is a setting a human has to flip, so the error the
    /// user reads carries the walk to it, not just the classification.
    #[test]
    fn a_privacy_denial_names_the_setting_to_flip() {
        let message =
            open_error(ExclusiveFailure::AccessDenied, "IAudioClient::Initialize").to_string();
        assert!(
            message.contains("microphone access denied by Windows privacy settings"),
            "{message}"
        );
        assert!(message.contains(MIC_PRIVACY_REMEDY), "{message}");
        assert!(
            message.contains("Settings, Privacy and security, Microphone"),
            "{message}"
        );
    }

    /// A late device gets waited for; a silent one gets declared dead. The
    /// budget is what makes those different, and it has to stay inside the
    /// client's own 500 ms reopen cadence or a dead stream is reported after
    /// the client has already given up on it.
    #[test]
    fn a_stream_is_dead_only_after_the_whole_timeout_budget() {
        assert!(!stream_is_dead(0));
        assert!(!stream_is_dead(MAX_CONSECUTIVE_TIMEOUTS - 1));
        assert!(stream_is_dead(MAX_CONSECUTIVE_TIMEOUTS));
        assert!(stream_is_dead(u32::MAX));
        assert_eq!(EVENT_WAIT_MS * MAX_CONSECUTIVE_TIMEOUTS, 500);
    }

    /// The advertised range is a promise about what the backend will open, so
    /// its floor is ours where the driver would go lower, and its ceiling is
    /// ours regardless.
    #[test]
    fn advertised_period_bounds_never_promise_what_we_will_not_ask_for() {
        // 3 ms, the usual Windows minimum: well above our floor, so it stands.
        assert_eq!(
            exclusive_period_bounds(30_000, 48_000),
            Some((144, MAX_PERIOD_FRAMES))
        );
        // A driver claiming 0.1 ms is raised to our floor rather than promised.
        assert_eq!(
            exclusive_period_bounds(1_000, 48_000),
            Some((MIN_PERIOD_FRAMES, MAX_PERIOD_FRAMES))
        );
        // A driver whose own minimum is 200 ms is past the ceiling, so there is
        // no range to advertise rather than an inverted one.
        assert_eq!(exclusive_period_bounds(2_000_000, 48_000), None);
        // The boundary itself is usable, and one frame past it is not.
        assert_eq!(
            exclusive_period_bounds(period_100ns(MAX_PERIOD_FRAMES, 48_000), 48_000),
            Some((MAX_PERIOD_FRAMES, MAX_PERIOD_FRAMES))
        );
        assert_eq!(
            exclusive_period_bounds(period_100ns(MAX_PERIOD_FRAMES + 3, 48_000), 48_000),
            None
        );
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
            ExclusiveFailure::WrongDirection,
            ExclusiveFailure::UnsupportedFormat,
            ExclusiveFailure::DeviceInUse,
            ExclusiveFailure::AccessDenied,
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
        use windows::Win32::Foundation::{E_ACCESSDENIED, E_INVALIDARG};
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
            (hr::E_ACCESSDENIED, E_ACCESSDENIED),
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
