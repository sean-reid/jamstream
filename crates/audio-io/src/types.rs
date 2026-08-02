//! Core types shared by every backend.

/// Direction of a device endpoint. A duplex hardware device is reported as
/// two entries, one per direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Capture,
    Playback,
}

/// Physical shape of an endpoint, as far as the host reports one.
///
/// Windows exposes it as `PKEY_AudioEndpoint_FormFactor` plus the device
/// enumerator (Bluetooth endpoints arrive through BTHENUM), PipeWire through
/// device properties; CoreAudio reports nothing, so macOS devices are
/// [`Unknown`](Self::Unknown). `Bluetooth` wins over the device kind because
/// the connection, not the shape, is what decides whether a capture endpoint
/// can run at 48 kHz. Consumed next by the client's device picker, so a
/// musician can see which microphone is the Bluetooth one before it refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormFactor {
    Speakers,
    Headphones,
    /// Earphones with an attached microphone: on Windows this is what a
    /// Bluetooth hands-free endpoint usually reports.
    Headset,
    Microphone,
    LineLevel,
    /// Any endpoint on a Bluetooth transport, whatever its shape.
    Bluetooth,
    /// Digital display audio: HDMI or DisplayPort.
    Hdmi,
    Unknown,
}

/// A single device endpoint as reported by a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Backend-specific stable identifier, suitable for persisting in config.
    pub id: String,
    /// Human-readable name for UI display.
    pub name: String,
    pub is_default: bool,
    pub direction: Direction,
    /// What the endpoint physically is, where the host says.
    pub form_factor: FormFactor,
    /// Supported buffer size bounds in frames, when the backend can report them.
    pub min_buffer_frames: Option<u32>,
    pub max_buffer_frames: Option<u32>,
}

/// Requested stream parameters. The whole system runs 48 kHz f32; backends
/// reject other rates rather than resampling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    pub sample_rate: u32,
    /// Requested frames per device callback. Backends fall back to the
    /// nearest supported size; the negotiated value is visible through
    /// [`StreamHandle::buffer_frames`].
    pub buffer_frames: u32,
    /// Channel count the handler sees, on both the capture and playback
    /// side. Backends convert to and from the device's native layout.
    pub channels: u16,
    /// Whether the open may take the device exclusively. Only the Windows
    /// backend has the choice: exclusive costs about 10 ms and mutes every
    /// other stream on the endpoint, shared costs 20-30 ms and coexists.
    /// `false` skips the exclusive probe entirely rather than trying and
    /// falling back, so the answer is the user's, not the driver's. Other
    /// platforms ignore it; [`crate::active_device_mode`] reports what ran.
    pub allow_exclusive: bool,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            buffer_frames: 240,
            channels: 2,
            allow_exclusive: true,
        }
    }
}

/// Clone so a modelled backend can answer every open with the same refusal;
/// the variants carry nothing but their own words.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AudioError {
    #[error("audio device is gone or was never present")]
    DeviceGone,
    #[error("unsupported audio configuration: {0}")]
    Unsupported(String),
    #[error("audio backend error: {0}")]
    Backend(String),
}

impl AudioError {
    /// The message without the variant's own prefix, for embedding inside
    /// another [`AudioError`] that already says what kind it is: composing
    /// full Displays stacked "unsupported audio configuration:" twice in one
    /// sentence.
    pub fn detail(&self) -> &str {
        match self {
            AudioError::DeviceGone => "audio device is gone or was never present",
            AudioError::Unsupported(msg) | AudioError::Backend(msg) => msg,
        }
    }
}

pub type Result<T> = std::result::Result<T, AudioError>;

type CaptureFn = Box<dyn FnMut(&[f32]) + Send>;
type PlaybackFn = Box<dyn FnMut(&mut [f32]) + Send>;

/// The pair of callbacks a backend drives from its device threads.
///
/// Capture and playback run on separate device threads with real backends,
/// so the two closures are independent and individually Send. Both must be
/// real-time safe: no allocation, no locks, no blocking.
pub struct DuplexHandler {
    capture: CaptureFn,
    playback: PlaybackFn,
}

impl DuplexHandler {
    pub fn new(
        capture: impl FnMut(&[f32]) + Send + 'static,
        playback: impl FnMut(&mut [f32]) + Send + 'static,
    ) -> Self {
        Self {
            capture: Box::new(capture),
            playback: Box::new(playback),
        }
    }

    /// Interleaved captured samples at the configured channel count.
    pub fn on_capture(&mut self, samples: &[f32]) {
        (self.capture)(samples);
    }

    /// Fill `out` (interleaved, configured channel count) with playout audio.
    /// The buffer arrives zeroed; leaving it untouched plays silence.
    pub fn on_playback(&mut self, out: &mut [f32]) {
        (self.playback)(out);
    }

    /// Split into the two halves so a backend can move each onto its own
    /// device thread.
    pub(crate) fn into_parts(self) -> (CaptureFn, PlaybackFn) {
        (self.capture, self.playback)
    }

    /// Reassemble halves from [`into_parts`](Self::into_parts): after the
    /// boundary converter wrapped each half for a mismatched-rate device,
    /// and on Windows when the exclusive-mode path fails to open and hands
    /// the handler back for the shared-mode fallback to try.
    pub(crate) fn from_parts(capture: CaptureFn, playback: PlaybackFn) -> Self {
        Self { capture, playback }
    }
}

impl std::fmt::Debug for DuplexHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuplexHandler").finish_non_exhaustive()
    }
}

/// A running duplex stream. Dropping the handle also stops the stream, but
/// [`close`](StreamHandle::close) is the explicit path.
pub trait StreamHandle: Send {
    /// Best-effort estimate of device round-trip latency in frames, i.e. the
    /// sum of the negotiated capture and playback buffer sizes where known.
    fn latency_frames(&self) -> Option<u32>;

    /// Largest frames-per-callback the device actually delivers, across both
    /// directions, where the backend can report it. A host is free to ignore
    /// the requested [`StreamConfig::buffer_frames`] (WASAPI shared mode
    /// calls back at the device period), so anything sized around callbacks
    /// must be sized from this, not from the request.
    ///
    /// The unit is frames per callback as the handler sees them, at the
    /// session rate: a backend converting for a mismatched-rate device
    /// scales its device-rate callback size up before reporting, so ring
    /// sizing never mixes clocks.
    fn buffer_frames(&self) -> Option<u32>;

    /// True once the backend reported a fatal stream error (device unplugged,
    /// configuration invalidated). The app should surface a device-gone state
    /// and reopen.
    fn errored(&self) -> bool {
        false
    }

    /// How each direction of this stream reaches the session rate (#347):
    /// natively, over a device clock the backend moved, through an OS
    /// converter, or through the boundary resampler with its disclosed cost.
    /// `None` when the backend cannot say.
    fn rate_outcomes(&self) -> Option<crate::RateOutcomes> {
        None
    }

    fn close(self: Box<Self>);
}

/// The real-audio-device boundary. One production implementation per
/// platform plus the offline WAV implementation for tests and headless runs.
pub trait AudioBackend: Send {
    fn devices(&self) -> Result<Vec<DeviceInfo>>;

    /// Open a capture and a playback stream as one logical duplex stream.
    /// `capture` and `playback` are device ids from [`devices`](Self::devices);
    /// `None` selects the system default for that direction.
    fn open_duplex(
        &self,
        capture: Option<&str>,
        playback: Option<&str>,
        config: StreamConfig,
        handler: DuplexHandler,
    ) -> Result<Box<dyn StreamHandle>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default answers for latency, which is the product's point; saying
    /// no is a per-machine choice the client plumbs through (#331).
    #[test]
    fn exclusive_is_allowed_unless_someone_says_otherwise() {
        assert!(StreamConfig::default().allow_exclusive);
    }
}
