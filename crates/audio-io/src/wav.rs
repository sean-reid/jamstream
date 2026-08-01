//! Deterministic offline backend for tests and the headless client.
//!
//! No real time: the caller drives the stream with [`WavStream::pump`],
//! which delivers input WAV samples to the handler exactly like a device
//! callback would and appends whatever the handler plays out to the capture
//! file. Within one pump call capture runs before playback, so a handler
//! that echoes its captured chunk produces output aligned with the input at
//! zero offset.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::types::{
    AudioBackend, AudioError, DeviceInfo, Direction, DuplexHandler, Result, StreamConfig,
    StreamHandle,
};

const WAV_CAPTURE_ID: &str = "wav-capture";
const WAV_PLAYBACK_ID: &str = "wav-playback";

/// Offline [`AudioBackend`] backed by WAV files via hound.
#[derive(Debug, Clone)]
pub struct WavBackend {
    input_wav: Option<PathBuf>,
    capture_output: Option<PathBuf>,
    device_rate: u32,
    device_period: Option<u32>,
    lose_device_after: Option<u64>,
    /// The rate every open after the first one runs at, when it differs.
    /// Shared, so the count survives the clone a caller keeps.
    reopen_rate: Option<u32>,
    opened: Arc<AtomicBool>,
}

impl WavBackend {
    /// `input_wav` feeds the handler's capture side (silence if `None`);
    /// `capture_output` receives everything the handler plays out.
    #[must_use]
    pub fn new(input_wav: Option<PathBuf>, capture_output: Option<PathBuf>) -> Self {
        Self {
            input_wav,
            capture_output,
            device_rate: 48_000,
            device_period: None,
            lose_device_after: None,
            reopen_rate: None,
            opened: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Models an interface running at `rate`: opening at any other rate fails
    /// the way a real device does, instead of playing back pitch shifted.
    #[must_use]
    pub fn with_device_rate(mut self, rate: u32) -> Self {
        self.device_rate = rate;
        self
    }

    /// Models a device that ignores the requested buffer size and calls back
    /// at its own period of `frames`, the way WASAPI shared mode does (~480
    /// frames at 48 kHz against a 120-frame request). Pumped frames accumulate
    /// and the handler runs once per full period, so the caller sees the same
    /// burstiness a real device delivers; [`StreamHandle::buffer_frames`]
    /// reports the period, exactly as cpal reports it on that host.
    #[must_use]
    pub fn with_device_period(mut self, frames: u32) -> Self {
        self.device_period = Some(frames);
        self
    }

    /// Models a device unplugged mid-session: once this many frames have been
    /// pumped the stream reports [`StreamHandle::errored`], so the caller's
    /// device-gone path runs offline instead of only against real hardware.
    #[must_use]
    pub fn with_device_loss_after(mut self, frames: u64) -> Self {
        self.lose_device_after = Some(frames);
        self
    }

    /// Models the interface a musician swaps to mid-song: the first open is the
    /// device they started on, and every one after it runs at `rate`, so a
    /// reopen is refused the way [`Self::with_device_rate`] refuses a join.
    /// Pair it with [`Self::with_device_loss_after`] for the whole sequence a
    /// swapped cable puts a running session through.
    #[must_use]
    pub fn refusing_reopen_at(mut self, rate: u32) -> Self {
        self.reopen_rate = Some(rate);
        self
    }

    /// Concrete-typed variant of [`AudioBackend::open_duplex`] so callers can
    /// reach [`WavStream::pump`] without downcasting.
    pub fn open_offline(&self, config: StreamConfig, handler: DuplexHandler) -> Result<WavStream> {
        let rate = match self.reopen_rate {
            Some(rate) if self.opened.swap(true, Ordering::Relaxed) => rate,
            _ => self.device_rate,
        };
        if config.sample_rate != rate {
            return Err(AudioError::Unsupported(format!(
                "wav device runs at {rate} Hz and will not open at {} Hz",
                config.sample_rate
            )));
        }
        if config.channels == 0 {
            return Err(AudioError::Unsupported("zero channels".into()));
        }

        let input = match &self.input_wav {
            Some(path) => read_input(path, config.channels, config.sample_rate)?,
            None => Vec::new(),
        };

        let writer = match &self.capture_output {
            Some(path) => {
                let spec = hound::WavSpec {
                    channels: config.channels,
                    sample_rate: config.sample_rate,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                };
                Some(hound::WavWriter::create(path, spec).map_err(wav_err)?)
            }
            None => None,
        };

        Ok(WavStream {
            handler,
            input,
            pos: 0,
            exhausted: false,
            writer,
            channels: usize::from(config.channels),
            capture_buf: Vec::new(),
            playback_buf: Vec::new(),
            pumped_frames: 0,
            lose_device_after: self.lose_device_after,
            errored: false,
            period: self.device_period.map(|p| p as usize),
            pending: 0,
            buffer_frames: self.device_period.unwrap_or(config.buffer_frames),
        })
    }
}

impl AudioBackend for WavBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![
            DeviceInfo {
                id: WAV_CAPTURE_ID.into(),
                name: "WAV file capture".into(),
                is_default: true,
                direction: Direction::Capture,
                min_buffer_frames: None,
                max_buffer_frames: None,
            },
            DeviceInfo {
                id: WAV_PLAYBACK_ID.into(),
                name: "WAV file playback".into(),
                is_default: true,
                direction: Direction::Playback,
                min_buffer_frames: None,
                max_buffer_frames: None,
            },
        ])
    }

    fn open_duplex(
        &self,
        _capture: Option<&str>,
        _playback: Option<&str>,
        config: StreamConfig,
        handler: DuplexHandler,
    ) -> Result<Box<dyn StreamHandle>> {
        Ok(Box::new(self.open_offline(config, handler)?))
    }
}

/// Offline stream. The caller advances virtual time with [`pump`](Self::pump).
pub struct WavStream {
    handler: DuplexHandler,
    /// Input samples already converted to the configured channel layout.
    input: Vec<f32>,
    pos: usize,
    exhausted: bool,
    writer: Option<hound::WavWriter<BufWriter<File>>>,
    channels: usize,
    capture_buf: Vec<f32>,
    playback_buf: Vec<f32>,
    pumped_frames: u64,
    lose_device_after: Option<u64>,
    errored: bool,
    /// Callback size the modelled device insists on; `None` delivers each
    /// pump as one callback, which is a device that honoured the request.
    period: Option<usize>,
    /// Frames pumped but not yet delivered, while a period is in force.
    pending: usize,
    buffer_frames: u32,
}

impl WavStream {
    /// Advance by `frames`: deliver the next input chunk to the handler's
    /// capture callback, then collect the same number of frames from its
    /// playback callback and append them to the capture output file.
    /// Input past end of file is silence; see [`exhausted`](Self::exhausted).
    ///
    /// Under [`WavBackend::with_device_period`] the frames accumulate instead,
    /// and the handler runs once per full period the total covers.
    pub fn pump(&mut self, frames: usize) -> Result<()> {
        let Some(period) = self.period else {
            return self.deliver(frames);
        };
        self.pending += frames;
        while self.pending >= period {
            self.deliver(period)?;
            self.pending -= period;
        }
        Ok(())
    }

    /// One device callback pair of `frames`: capture into the handler, then
    /// its playout into the capture output file.
    fn deliver(&mut self, frames: usize) -> Result<()> {
        let samples = frames * self.channels;
        self.capture_buf.clear();
        self.capture_buf.resize(samples, 0.0);
        let available = (self.input.len() - self.pos).min(samples);
        self.capture_buf[..available].copy_from_slice(&self.input[self.pos..self.pos + available]);
        self.pos += available;
        if available < samples {
            self.exhausted = true;
        }
        self.handler.on_capture(&self.capture_buf);

        self.playback_buf.clear();
        self.playback_buf.resize(samples, 0.0);
        self.handler.on_playback(&mut self.playback_buf);
        if let Some(writer) = &mut self.writer {
            for &s in &self.playback_buf {
                writer.write_sample(s).map_err(wav_err)?;
            }
        }
        self.pumped_frames += frames as u64;
        if self
            .lose_device_after
            .is_some_and(|f| self.pumped_frames >= f)
        {
            self.errored = true;
        }
        Ok(())
    }

    /// Reports the device as lost from now on, the way a real backend's error
    /// callback would. Pumping still works; the flag is what the caller polls.
    pub fn report_device_lost(&mut self) {
        self.errored = true;
    }

    /// True once a pump has run past the end of the input WAV (or from the
    /// first pump when there is no input file).
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.exhausted
    }

    /// Finalize the capture output file. Preferred over drop, which can only
    /// finalize best-effort.
    pub fn finish(mut self) -> Result<()> {
        self.finish_inner()
    }

    fn finish_inner(&mut self) -> Result<()> {
        match self.writer.take() {
            Some(writer) => writer.finalize().map_err(wav_err),
            None => Ok(()),
        }
    }
}

impl std::fmt::Debug for WavStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WavStream")
            .field("pos", &self.pos)
            .field("exhausted", &self.exhausted)
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

impl StreamHandle for WavStream {
    fn latency_frames(&self) -> Option<u32> {
        Some(0)
    }

    fn buffer_frames(&self) -> Option<u32> {
        Some(self.buffer_frames)
    }

    fn errored(&self) -> bool {
        self.errored
    }

    fn close(mut self: Box<Self>) {
        let _ = self.finish_inner();
    }
}

fn wav_err(e: hound::Error) -> AudioError {
    AudioError::Backend(e.to_string())
}

/// Read the whole input file, asserting the stream's rate, and convert to
/// `channels`: matching layouts copy through, mono fans out to every channel,
/// and a wider source contributes its first `channels` channels.
fn read_input(path: &PathBuf, channels: u16, sample_rate: u32) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path).map_err(wav_err)?;
    let spec = reader.spec();
    if spec.sample_rate != sample_rate {
        return Err(AudioError::Unsupported(format!(
            "input wav must be {sample_rate} Hz, got {}",
            spec.sample_rate
        )));
    }

    let raw: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => reader
            .samples::<f32>()
            .collect::<std::result::Result<_, _>>()
            .map_err(wav_err)?,
        (hound::SampleFormat::Int, bits @ 1..=32) => {
            let scale = 1.0 / (1i64 << (bits - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<std::result::Result<_, _>>()
                .map_err(wav_err)?
        }
        (format, bits) => {
            return Err(AudioError::Unsupported(format!(
                "input wav format {format:?} at {bits} bits"
            )));
        }
    };

    let src_ch = usize::from(spec.channels.max(1));
    let dst_ch = usize::from(channels);
    if src_ch == dst_ch {
        return Ok(raw);
    }
    let frames = raw.len() / src_ch;
    let mut out = Vec::with_capacity(frames * dst_ch);
    for frame in raw.chunks_exact(src_ch) {
        for ch in 0..dst_ch {
            out.push(frame[ch.min(src_ch - 1)]);
        }
    }
    Ok(out)
}
