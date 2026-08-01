//! Core types shared by every backend.

/// Direction of a device endpoint. A duplex hardware device is reported as
/// two entries, one per direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Capture,
    Playback,
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
    /// [`StreamHandle::latency_frames`].
    pub buffer_frames: u32,
    /// Channel count the handler sees, on both the capture and playback
    /// side. Backends convert to and from the device's native layout.
    pub channels: u16,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            buffer_frames: 240,
            channels: 2,
        }
    }
}

#[derive(Debug, thiserror::Error)]
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

    /// Reassemble halves from [`into_parts`](Self::into_parts). A backend that
    /// splits a handler and then fails to open needs to hand it back so
    /// another backend can try, which is what the Windows exclusive-mode path
    /// does before falling back to shared mode.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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

    /// True once the backend reported a fatal stream error (device unplugged,
    /// configuration invalidated). The app should surface a device-gone state
    /// and reopen.
    fn errored(&self) -> bool {
        false
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
