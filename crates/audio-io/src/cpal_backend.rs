//! Real devices via cpal: CoreAudio on macOS, WASAPI shared mode on
//! Windows, PipeWire/ALSA on Linux. On Windows this is the fallback half of
//! `WindowsBackend`, which prefers the direct WASAPI exclusive-mode path; see
//! `backend()` in lib.rs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::format::map_frames;
use crate::types::{
    AudioBackend, AudioError, DeviceInfo, Direction, DuplexHandler, Result, StreamConfig,
    StreamHandle,
};

/// Largest per-callback chunk converted in one pass. Bigger device callbacks
/// are processed in slices of this many frames, so the conversion scratch
/// buffers stay fixed after stream construction.
const MAX_CHUNK_FRAMES: usize = 4096;

type CaptureFn = Box<dyn FnMut(&[f32]) + Send>;
type PlaybackFn = Box<dyn FnMut(&mut [f32]) + Send>;

/// Platform default cpal host.
pub struct CpalBackend {
    host: cpal::Host,
}

impl CpalBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }

    fn find_device(&self, id: Option<&str>, direction: Direction) -> Result<cpal::Device> {
        match id {
            None => match direction {
                Direction::Capture => self.host.default_input_device(),
                Direction::Playback => self.host.default_output_device(),
            }
            .ok_or(AudioError::DeviceGone),
            Some(wanted) => {
                for device in self.host.devices().map_err(|e| map_err(&e))? {
                    let matches_id = device.id().is_ok_and(|d| d.id() == wanted);
                    let matches_dir = match direction {
                        Direction::Capture => device.supports_input(),
                        Direction::Playback => device.supports_output(),
                    };
                    if matches_id && matches_dir {
                        return Ok(device);
                    }
                }
                Err(AudioError::DeviceGone)
            }
        }
    }
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CpalBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpalBackend").finish_non_exhaustive()
    }
}

impl AudioBackend for CpalBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>> {
        let default_in = self.host.default_input_device().and_then(|d| d.id().ok());
        let default_out = self.host.default_output_device().and_then(|d| d.id().ok());

        let mut out = Vec::new();
        for device in self.host.devices().map_err(|e| map_err(&e))? {
            // A device can vanish mid-enumeration; skip rather than fail.
            let Ok(id) = device.id() else { continue };
            let name = device
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_else(|_| id.id().to_string());

            if let Ok(config) = device.default_input_config() {
                let (min, max) = buffer_bounds(config.buffer_size());
                out.push(DeviceInfo {
                    id: id.id().to_string(),
                    name: name.clone(),
                    is_default: default_in.as_ref() == Some(&id),
                    direction: Direction::Capture,
                    min_buffer_frames: min,
                    max_buffer_frames: max,
                });
            }
            if let Ok(config) = device.default_output_config() {
                let (min, max) = buffer_bounds(config.buffer_size());
                out.push(DeviceInfo {
                    id: id.id().to_string(),
                    name,
                    is_default: default_out.as_ref() == Some(&id),
                    direction: Direction::Playback,
                    min_buffer_frames: min,
                    max_buffer_frames: max,
                });
            }
        }
        Ok(out)
    }

    fn open_duplex(
        &self,
        capture: Option<&str>,
        playback: Option<&str>,
        config: StreamConfig,
        handler: DuplexHandler,
    ) -> Result<Box<dyn StreamHandle>> {
        if config.channels == 0 {
            return Err(AudioError::Unsupported("zero channels".into()));
        }
        let in_device = self.find_device(capture, Direction::Capture)?;
        let out_device = self.find_device(playback, Direction::Playback)?;
        let in_native = in_device.default_input_config().map_err(|e| map_err(&e))?;
        let out_native = out_device
            .default_output_config()
            .map_err(|e| map_err(&e))?;

        let (on_capture, on_playback) = handler.into_parts();
        let errored = Arc::new(AtomicBool::new(false));

        let input = build_input(&in_device, &in_native, &config, on_capture, &errored)?;
        let output = build_output(&out_device, &out_native, &config, on_playback, &errored)?;

        // cpal 0.18 streams start paused.
        input.play().map_err(|e| map_err(&e))?;
        output.play().map_err(|e| map_err(&e))?;

        // Negotiated callback sizes are the best latency estimate cpal
        // exposes; sum both directions when both are known.
        let latency_frames = match (input.buffer_size().ok(), output.buffer_size().ok()) {
            (Some(i), Some(o)) => Some(i + o),
            (one, other) => one.or(other),
        };

        Ok(Box::new(CpalStreamHandle {
            input,
            output,
            errored,
            latency_frames,
        }))
    }
}

fn map_err(e: &cpal::Error) -> AudioError {
    match e.kind() {
        cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::HostUnavailable => {
            AudioError::DeviceGone
        }
        cpal::ErrorKind::UnsupportedConfig | cpal::ErrorKind::UnsupportedOperation => {
            AudioError::Unsupported(e.to_string())
        }
        _ => AudioError::Backend(e.to_string()),
    }
}

fn buffer_bounds(size: &cpal::SupportedBufferSize) -> (Option<u32>, Option<u32>) {
    match *size {
        cpal::SupportedBufferSize::Range { min, max } => (Some(min), Some(max)),
        cpal::SupportedBufferSize::Unknown => (None, None),
    }
}

/// Nearest supported size for the requested frames. Hosts round or validate
/// Fixed requests against this same range, so clamping up front avoids a
/// build failure; an unknown range falls back to the backend default size.
fn choose_buffer_size(native: &cpal::SupportedStreamConfig, requested: u32) -> cpal::BufferSize {
    match *native.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            cpal::BufferSize::Fixed(requested.clamp(min, max))
        }
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
    }
}

fn make_error_callback(errored: &Arc<AtomicBool>) -> impl FnMut(cpal::Error) + Send + 'static {
    let flag = Arc::clone(errored);
    move |e: cpal::Error| {
        // Informational kinds do not invalidate the stream; everything else
        // (device gone, stream invalidated, backend failure) means the app
        // must surface a device-gone state and reopen.
        if !matches!(
            e.kind(),
            cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied
        ) {
            flag.store(true, Ordering::Release);
        }
    }
}

fn build_input(
    device: &cpal::Device,
    native: &cpal::SupportedStreamConfig,
    config: &StreamConfig,
    mut on_capture: CaptureFn,
    errored: &Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    let device_ch = usize::from(native.channels().max(1));
    let handler_ch = usize::from(config.channels);
    let mut scratch = vec![0.0f32; MAX_CHUNK_FRAMES * handler_ch];

    let stream_config = cpal::StreamConfig {
        channels: native.channels(),
        sample_rate: config.sample_rate,
        buffer_size: choose_buffer_size(native, config.buffer_frames),
    };
    device
        .build_input_stream::<f32, _, _>(
            stream_config,
            move |data: &[f32], _| {
                for chunk in data.chunks(MAX_CHUNK_FRAMES * device_ch) {
                    let frames = chunk.len() / device_ch;
                    let dst = &mut scratch[..frames * handler_ch];
                    map_frames(chunk, device_ch, dst, handler_ch);
                    on_capture(dst);
                }
            },
            make_error_callback(errored),
            None,
        )
        .map_err(|e| map_err(&e))
}

fn build_output(
    device: &cpal::Device,
    native: &cpal::SupportedStreamConfig,
    config: &StreamConfig,
    mut on_playback: PlaybackFn,
    errored: &Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    let device_ch = usize::from(native.channels().max(1));
    let handler_ch = usize::from(config.channels);
    let mut scratch = vec![0.0f32; MAX_CHUNK_FRAMES * handler_ch];

    let stream_config = cpal::StreamConfig {
        channels: native.channels(),
        sample_rate: config.sample_rate,
        buffer_size: choose_buffer_size(native, config.buffer_frames),
    };
    device
        .build_output_stream::<f32, _, _>(
            stream_config,
            move |data: &mut [f32], _| {
                for chunk in data.chunks_mut(MAX_CHUNK_FRAMES * device_ch) {
                    let frames = chunk.len() / device_ch;
                    let src = &mut scratch[..frames * handler_ch];
                    src.fill(0.0);
                    on_playback(src);
                    map_frames(src, handler_ch, chunk, device_ch);
                }
            },
            make_error_callback(errored),
            None,
        )
        .map_err(|e| map_err(&e))
}

struct CpalStreamHandle {
    input: cpal::Stream,
    output: cpal::Stream,
    errored: Arc<AtomicBool>,
    latency_frames: Option<u32>,
}

impl StreamHandle for CpalStreamHandle {
    fn latency_frames(&self) -> Option<u32> {
        self.latency_frames
    }

    fn errored(&self) -> bool {
        self.errored.load(Ordering::Acquire)
    }

    fn close(self: Box<Self>) {
        // Pause is best-effort; dropping the streams tears them down.
        let _ = self.input.pause();
        let _ = self.output.pause();
    }
}
