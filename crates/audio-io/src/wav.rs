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

use crate::resample::{converting_capture, converting_playback, session_frames};
use crate::types::{
    AudioBackend, AudioError, DeviceInfo, Direction, DuplexHandler, FormFactor, Result,
    StreamConfig, StreamHandle,
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
    form_factor: FormFactor,
    lose_device_after: Option<u64>,
    /// Latched by the stream that hit the loss threshold. Shared, so the
    /// unplug happens once per backend however many streams reopen after it.
    loss_fired: Arc<AtomicBool>,
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
            form_factor: FormFactor::Unknown,
            lose_device_after: None,
            loss_fired: Arc::new(AtomicBool::new(false)),
            reopen_rate: None,
            opened: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Models an interface clocked at `rate`: a session at any other rate
    /// opens through the boundary converter (#347 rung 3), so the handler
    /// keeps seeing session-rate audio while [`WavStream::pump`], the input
    /// WAV, and the capture output all move in device-rate frames.
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
    /// reports the period, exactly as cpal reports it on that host, scaled to
    /// session-rate frames when the stream converts.
    #[must_use]
    pub fn with_device_period(mut self, frames: u32) -> Self {
        self.device_period = Some(frames);
        self
    }

    /// Models the form factor the host would report for both endpoints, the
    /// way pairing one Bluetooth headset yields a capture and a playback
    /// device with the same shape. The default is `Unknown`, which is also
    /// what a real host reports when it cannot decode one, so callers must
    /// treat `Unknown` as "no information", never as "not Bluetooth".
    #[must_use]
    pub fn with_form_factor(mut self, form_factor: FormFactor) -> Self {
        self.form_factor = form_factor;
        self
    }

    /// Models a device unplugged mid-session: once this many frames have been
    /// pumped the stream reports [`StreamHandle::errored`], so the caller's
    /// device-gone path runs offline instead of only against real hardware.
    /// An unplug happens once: the stream that answers the reopen models the
    /// replacement device and keeps running.
    #[must_use]
    pub fn with_device_loss_after(mut self, frames: u64) -> Self {
        self.lose_device_after = Some(frames);
        self
    }

    /// Models the interface a musician swaps to mid-song: the first open is
    /// the device they started on, and every one after it runs at `rate`,
    /// converting at the boundary when the session rate differs, exactly as
    /// [`Self::with_device_rate`] does from the start. Pair it with
    /// [`Self::with_device_loss_after`] for the whole sequence a swapped
    /// cable puts a running session through.
    #[must_use]
    pub fn reopening_at(mut self, rate: u32) -> Self {
        self.reopen_rate = Some(rate);
        self
    }

    /// Concrete-typed variant of [`AudioBackend::open_duplex`] so callers can
    /// reach [`WavStream::pump`] without downcasting.
    ///
    /// A device rate other than `config.sample_rate` opens with the boundary
    /// converter wrapped around each handler half, the #347 rung 3 shape:
    /// the stream, its files, and its pump run in device-rate frames while
    /// the handler keeps its session-rate view.
    pub fn open_offline(&self, config: StreamConfig, handler: DuplexHandler) -> Result<WavStream> {
        let rate = match self.reopen_rate {
            Some(rate) if self.opened.swap(true, Ordering::Relaxed) => rate,
            _ => self.device_rate,
        };
        if config.channels == 0 {
            return Err(AudioError::Unsupported("zero channels".into()));
        }

        let input = match &self.input_wav {
            Some(path) => read_input(path, config.channels, rate)?,
            None => Vec::new(),
        };

        let writer = match &self.capture_output {
            Some(path) => {
                let spec = hound::WavSpec {
                    channels: config.channels,
                    sample_rate: rate,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                };
                Some(hound::WavWriter::create(path, spec).map_err(wav_err)?)
            }
            None => None,
        };

        let (handler, resample_added_ms) = if config.sample_rate == rate {
            (handler, None)
        } else {
            let (capture, playback) = handler.into_parts();
            let (capture, capture_ms) =
                converting_capture(capture, config.sample_rate, rate, config.channels);
            let (playback, playback_ms) =
                converting_playback(playback, config.sample_rate, rate, config.channels);
            (
                DuplexHandler::from_parts(capture, playback),
                Some((capture_ms, playback_ms)),
            )
        };

        // Report the rate outcomes exactly like a real backend: no OS ever
        // converts this device, so each direction is native or on the
        // boundary converter, never anything else.
        crate::rate::log_rate_outcomes(&rate_outcomes(rate, resample_added_ms));

        let device_frames = self.device_period.unwrap_or(config.buffer_frames);
        Ok(WavStream {
            handler,
            input,
            pos: 0,
            exhausted: false,
            writer,
            channels: usize::from(config.channels),
            device_rate: rate,
            resample_added_ms,
            capture_buf: Vec::new(),
            playback_buf: Vec::new(),
            pumped_frames: 0,
            lose_device_after: self
                .lose_device_after
                .filter(|_| !self.loss_fired.load(Ordering::Relaxed)),
            loss_fired: Arc::clone(&self.loss_fired),
            errored: false,
            period: self.device_period.map(|p| p as usize),
            pending: 0,
            buffer_frames: session_frames(device_frames, config.sample_rate, rate),
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
                form_factor: self.form_factor,
                min_buffer_frames: None,
                max_buffer_frames: None,
            },
            DeviceInfo {
                id: WAV_PLAYBACK_ID.into(),
                name: "WAV file playback".into(),
                is_default: true,
                direction: Direction::Playback,
                form_factor: self.form_factor,
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

/// Offline stream. The caller advances virtual time with [`pump`](Self::pump);
/// pumped frames are device-rate frames, the clock the modelled device runs
/// at, so a pacing caller must pace from [`device_rate`](Self::device_rate).
pub struct WavStream {
    handler: DuplexHandler,
    /// Input samples already converted to the configured channel layout.
    input: Vec<f32>,
    pos: usize,
    exhausted: bool,
    writer: Option<hound::WavWriter<BufWriter<File>>>,
    channels: usize,
    device_rate: u32,
    /// `(capture, playback)` latency the boundary converter adds, when this
    /// stream converts.
    resample_added_ms: Option<(f32, f32)>,
    capture_buf: Vec<f32>,
    playback_buf: Vec<f32>,
    pumped_frames: u64,
    lose_device_after: Option<u64>,
    /// Backend-shared latch marking the modelled unplug as spent.
    loss_fired: Arc<AtomicBool>,
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
            self.loss_fired.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Reports the device as lost from now on, the way a real backend's error
    /// callback would. Pumping still works; the flag is what the caller polls.
    pub fn report_device_lost(&mut self) {
        self.errored = true;
    }

    /// The rate the modelled device is clocked at. [`pump`](Self::pump)
    /// frames represent wall time at this rate, never at the session rate:
    /// a 44.1 kHz stream pumped at 48 000 frames per second would run 8.8%
    /// fast, far past what any drift compensator absorbs.
    #[must_use]
    pub fn device_rate(&self) -> u32 {
        self.device_rate
    }

    /// Latency the boundary converter adds in milliseconds, as
    /// `(capture, playback)`, or `None` when the device runs at the session
    /// rate and nothing converts. The figures come from the converter's own
    /// constructor; the rate disclosure reads them at open.
    #[must_use]
    pub fn resample_added_ms(&self) -> Option<(f32, f32)> {
        self.resample_added_ms
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

    fn rate_outcomes(&self) -> Option<crate::RateOutcomes> {
        Some(rate_outcomes(self.device_rate, self.resample_added_ms))
    }

    fn close(mut self: Box<Self>) {
        let _ = self.finish_inner();
    }
}

/// The offline stream's outcomes from its own state: each direction is
/// native or on the boundary converter, never anything else.
fn rate_outcomes(device_rate: u32, resample_added_ms: Option<(f32, f32)>) -> crate::RateOutcomes {
    let outcome = |added_ms: Option<f32>| match added_ms {
        Some(added_ms) => crate::RateOutcome::Resampled {
            device: device_rate,
            added_ms,
        },
        None => crate::RateOutcome::Native,
    };
    crate::RateOutcomes {
        capture: outcome(resample_added_ms.map(|(c, _)| c)),
        playback: outcome(resample_added_ms.map(|(_, p)| p)),
    }
}

fn wav_err(e: hound::Error) -> AudioError {
    AudioError::Backend(e.to_string())
}

/// Read the whole input file, asserting the device's rate, and convert to
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
