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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::resample::{converting_capture, converting_playback, session_frames};
use crate::types::{
    AudioBackend, AudioError, DeviceInfo, Direction, DuplexHandler, FormFactor, Result,
    StreamConfig, StreamHandle,
};

const WAV_CAPTURE_ID: &str = "wav-capture";
const WAV_PLAYBACK_ID: &str = "wav-playback";

/// How one direction of a modelled device reaches the session rate: the rung
/// of the #347 ladder it lands on, as the real backends report it.
///
/// The rungs are not interchangeable, and the difference is observable. A
/// clock this app moved and an OS converter both leave the stream itself at
/// the session rate, so a stream on either runs at the session rate here too,
/// files and pump included. Only the boundary converter leaves the device on
/// its own clock. Nothing here can report a rung the stream is not on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRung {
    /// The device runs at the session rate; nothing bridges anything.
    Native,
    /// The backend moved the device's whole clock from `from` to the session
    /// rate, the way CoreAudio does inside the open (rung 2).
    ClockSet { from: u32 },
    /// The device engine runs at `device` and the OS carries the session-rate
    /// stream over it: WASAPI render's AUTOCONVERTPCM, the PipeWire graph.
    OsConverted { device: u32 },
    /// The device runs at `device` and the boundary converter carries the
    /// difference (rung 3), so the stream, its files and its pump all move in
    /// device-rate frames.
    Converted { device: u32 },
}

impl DeviceRung {
    /// The rung as it applies to a session at `session`: one that names the
    /// session rate bridges nothing, whatever it claims.
    fn at(self, session: u32) -> DeviceRung {
        match self {
            DeviceRung::ClockSet { from } if from == session => DeviceRung::Native,
            DeviceRung::OsConverted { device } if device == session => DeviceRung::Native,
            DeviceRung::Converted { device } if device == session => DeviceRung::Native,
            other => other,
        }
    }

    /// The clock this direction's stream runs at.
    fn stream_rate(self, session: u32) -> u32 {
        match self {
            DeviceRung::Converted { device } => device,
            _ => session,
        }
    }

    /// The outcome the backend reports, taking the converter's own figure
    /// where the converter is what bridged the difference.
    fn outcome(self, added_ms: Option<f32>) -> crate::RateOutcome {
        match self {
            DeviceRung::Native => crate::RateOutcome::Native,
            DeviceRung::ClockSet { from } => crate::RateOutcome::ClockSet { from },
            DeviceRung::OsConverted { device } => crate::RateOutcome::OsConverted { device },
            DeviceRung::Converted { device } => crate::RateOutcome::Resampled {
                device,
                added_ms: added_ms.expect("a converting direction has a converter to ask"),
            },
        }
    }
}

/// Offline [`AudioBackend`] backed by WAV files via hound.
#[derive(Debug, Clone)]
pub struct WavBackend {
    input_wav: Option<PathBuf>,
    capture_output: Option<PathBuf>,
    /// The rung each direction lands on, capture then playback.
    rungs: (DeviceRung, DeviceRung),
    device_period: Option<u32>,
    form_factor: FormFactor,
    lose_device_after: Option<u64>,
    /// Whether every stream loses the device, or only the first one to reach
    /// the threshold.
    loss_repeats: bool,
    /// Latched by the stream that hit the loss threshold. Shared, so the
    /// unplug happens once per backend however many streams reopen after it.
    loss_fired: Arc<AtomicBool>,
    /// The rungs every open after the first one lands on, when they differ.
    reopen_rungs: Option<(DeviceRung, DeviceRung)>,
    /// The refusal every open after the first one answers with: a device the
    /// backend will not open, as distinct from the input file not matching
    /// the device clock, which is this backend's own bookkeeping and nothing
    /// a real one can produce.
    refuse_reopen: Option<AudioError>,
    /// How many opens the refusal covers, and None for every one of them.
    refuse_reopens: Option<u32>,
    /// Streams opened so far. Shared, so the count survives the clone a
    /// caller keeps to watch it and a caller that moved the backend away.
    opens: Arc<AtomicU32>,
}

impl WavBackend {
    /// `input_wav` feeds the handler's capture side (silence if `None`);
    /// `capture_output` receives everything the handler plays out.
    #[must_use]
    pub fn new(input_wav: Option<PathBuf>, capture_output: Option<PathBuf>) -> Self {
        Self {
            input_wav,
            capture_output,
            rungs: (DeviceRung::Native, DeviceRung::Native),
            device_period: None,
            form_factor: FormFactor::Unknown,
            lose_device_after: None,
            loss_repeats: false,
            loss_fired: Arc::new(AtomicBool::new(false)),
            reopen_rungs: None,
            refuse_reopen: None,
            refuse_reopens: None,
            opens: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Streams this backend has opened, the reopens included. A caller that
    /// moved the backend into a runtime keeps a clone to read it, which is
    /// how a test tells a bounded retry loop from an unbounded one.
    #[must_use]
    pub fn opens(&self) -> u32 {
        self.opens.load(Ordering::Relaxed)
    }

    /// Models an interface clocked at `rate`: a session at any other rate
    /// opens through the boundary converter (#347 rung 3), so the handler
    /// keeps seeing session-rate audio while [`WavStream::pump`], the input
    /// WAV, and the capture output all move in device-rate frames.
    #[must_use]
    pub fn with_device_rate(mut self, rate: u32) -> Self {
        let rung = DeviceRung::Converted { device: rate };
        self.rungs = (rung, rung);
        self
    }

    /// Models the two endpoints reaching the session rate by different
    /// routes, which is the ordinary case the moment the microphone and the
    /// speakers are different hardware: a 44.1 kHz interface beside 48 kHz
    /// monitors converts capture only, and a host that moves one device's
    /// clock has not touched the other's.
    #[must_use]
    pub fn with_direction_rungs(mut self, capture: DeviceRung, playback: DeviceRung) -> Self {
        self.rungs = (capture, playback);
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
    /// replacement device and keeps running. See
    /// [`losing_device_every`](Self::losing_device_every) for the device that
    /// never comes back.
    #[must_use]
    pub fn with_device_loss_after(mut self, frames: u64) -> Self {
        self.lose_device_after = Some(frames);
        self.loss_repeats = false;
        self
    }

    /// Models a device that will not stay open: every stream, the reopens
    /// included, loses the device after this many pumped frames. Zero latches
    /// the stream at open, before the caller can poll it once, which is the
    /// shape a WASAPI exclusive endpoint another process holds and a PipeWire
    /// graph refusing the rate both arrive in, and the shape the retry loop
    /// has to be bounded against.
    #[must_use]
    pub fn losing_device_every(mut self, frames: u64) -> Self {
        self.lose_device_after = Some(frames);
        self.loss_repeats = true;
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
        let rung = DeviceRung::Converted { device: rate };
        self.reopen_rungs = Some((rung, rung));
        self
    }

    /// Models a device the backend will not open: every open after the first
    /// one fails with `error`, in the device's own words, the way a real
    /// backend refuses one it cannot carry. Pair it with
    /// [`Self::with_device_loss_after`] for the interface a musician swaps to
    /// that turns out to be unusable.
    #[must_use]
    pub fn refusing_reopen(mut self, error: AudioError) -> Self {
        self.refuse_reopen = Some(error);
        self.refuse_reopens = None;
        self
    }

    /// Models a device still held by the stream it is replacing: the next
    /// `count` opens after the first are refused with `error` and the one after
    /// them succeeds. A WASAPI endpoint and a macOS aggregate both refuse for a
    /// few hundred milliseconds after a close, so the caller spends a bounded
    /// stretch with no stream and then gets one back.
    #[must_use]
    pub fn refusing_reopens(mut self, count: u32, error: AudioError) -> Self {
        self.refuse_reopen = Some(error);
        self.refuse_reopens = Some(count);
        self
    }

    /// Concrete-typed variant of [`AudioBackend::open_duplex`] so callers can
    /// reach [`WavStream::pump`] without downcasting.
    ///
    /// A direction on [`DeviceRung::Converted`] opens with the boundary
    /// converter wrapped around its handler half, the #347 rung 3 shape: that
    /// direction's file and its share of the pump run in device-rate frames
    /// while the handler keeps its session-rate view. Every other rung leaves
    /// the direction at the session rate, because that is what the real
    /// backends leave it at.
    pub fn open_offline(&self, config: StreamConfig, handler: DuplexHandler) -> Result<WavStream> {
        let opened = self.opens.fetch_add(1, Ordering::Relaxed);
        let reopen = opened > 0;
        if reopen
            && let Some(err) = &self.refuse_reopen
            && self.refuse_reopens.is_none_or(|n| opened <= n)
        {
            return Err(err.clone());
        }
        let session = config.sample_rate;
        let (capture_rung, playback_rung) = match self.reopen_rungs {
            Some(rungs) if reopen => rungs,
            _ => self.rungs,
        };
        let capture_rung = capture_rung.at(session);
        let playback_rung = playback_rung.at(session);
        let capture_rate = capture_rung.stream_rate(session);
        let playback_rate = playback_rung.stream_rate(session);
        if config.channels == 0 {
            return Err(AudioError::Unsupported("zero channels".into()));
        }

        let input = match &self.input_wav {
            Some(path) => read_input(path, config.channels, capture_rate)?,
            None => Vec::new(),
        };

        let writer = match &self.capture_output {
            Some(path) => {
                let spec = hound::WavSpec {
                    channels: config.channels,
                    sample_rate: playback_rate,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                };
                Some(hound::WavWriter::create(path, spec).map_err(wav_err)?)
            }
            None => None,
        };

        let (capture, playback) = handler.into_parts();
        let (capture, capture_ms) = if capture_rate == session {
            (capture, None)
        } else {
            let (wrapped, ms) = converting_capture(capture, session, capture_rate, config.channels);
            (wrapped, Some(ms))
        };
        let (playback, playback_ms) = if playback_rate == session {
            (playback, None)
        } else {
            let (wrapped, ms) =
                converting_playback(playback, session, playback_rate, config.channels);
            (wrapped, Some(ms))
        };
        let handler = DuplexHandler::from_parts(capture, playback);

        let outcomes = crate::RateOutcomes {
            capture: capture_rung.outcome(capture_ms),
            playback: playback_rung.outcome(playback_ms),
        };
        crate::rate::log_rate_outcomes(&outcomes);

        // A repeating loss belongs to every stream; a one-shot unplug is
        // spent once some stream has served it.
        let loss = self
            .lose_device_after
            .filter(|_| self.loss_repeats || !self.loss_fired.load(Ordering::Relaxed));
        if loss == Some(0) {
            self.loss_fired.store(true, Ordering::Relaxed);
        }

        // Callbacks are reported as the larger of the two directions in
        // session-rate frames, the way a real backend reports the size
        // anything callback-sized has to absorb.
        let device_frames = self.device_period.unwrap_or(config.buffer_frames);
        let buffer_frames = session_frames(device_frames, session, capture_rate)
            .max(session_frames(device_frames, session, playback_rate));
        Ok(WavStream {
            handler,
            input,
            pos: 0,
            exhausted: false,
            writer,
            channels: usize::from(config.channels),
            capture_rate,
            playback_rate,
            play_debt: 0.0,
            resample_added_ms: (capture_ms, playback_ms),
            outcomes,
            capture_buf: Vec::new(),
            playback_buf: Vec::new(),
            pumped_frames: 0,
            lose_device_after: loss,
            loss_fired: Arc::clone(&self.loss_fired),
            // A device that is already gone when the stream comes up: the
            // handle answers errored on the caller's first poll, with no
            // pump in between.
            errored: loss == Some(0),
            period: self.device_period.map(|p| p as usize),
            pending: 0,
            buffer_frames,
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
/// pumped frames are capture-device frames, the clock the modelled capture
/// endpoint runs at, so a pacing caller must pace from
/// [`device_rate`](Self::device_rate).
pub struct WavStream {
    handler: DuplexHandler,
    /// Input samples already converted to the configured channel layout.
    input: Vec<f32>,
    pos: usize,
    exhausted: bool,
    writer: Option<hound::WavWriter<BufWriter<File>>>,
    channels: usize,
    capture_rate: u32,
    playback_rate: u32,
    /// Playback frames owed but not yet whole, while the two endpoints run on
    /// different clocks. Zero for the whole run when they agree, so a single
    /// device is delivered frame for frame as it always was.
    play_debt: f64,
    /// `(capture, playback)` latency the boundary converter adds, per
    /// direction that converts.
    resample_added_ms: (Option<f32>, Option<f32>),
    outcomes: crate::RateOutcomes,
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

    /// One device callback pair of `frames` on the capture clock: capture
    /// into the handler, then its playout into the capture output file.
    ///
    /// The playback endpoint gets the frames its own clock owes over the same
    /// span, which is the same count whenever the two agree and the ratio
    /// between them when they do not. Two endpoints on two clocks free-run
    /// against each other on real hardware too.
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

        self.play_debt +=
            frames as f64 * f64::from(self.playback_rate) / f64::from(self.capture_rate);
        let play_frames = self.play_debt as usize;
        self.play_debt -= play_frames as f64;
        self.playback_buf.clear();
        self.playback_buf.resize(play_frames * self.channels, 0.0);
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

    /// The rate the modelled capture endpoint is clocked at.
    /// [`pump`](Self::pump) frames represent wall time at this rate, never at
    /// the session rate: a 44.1 kHz stream pumped at 48 000 frames per second
    /// would run 8.8% fast, far past what any drift compensator absorbs.
    #[must_use]
    pub fn device_rate(&self) -> u32 {
        self.capture_rate
    }

    /// Latency the boundary converter adds in milliseconds, per direction,
    /// as `(capture, playback)`; None on a direction that does not convert.
    /// The figures come from the converter's own constructor; the rate
    /// disclosure reads them at open.
    #[must_use]
    pub fn resample_added_ms(&self) -> (Option<f32>, Option<f32>) {
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
        Some(self.outcomes)
    }

    fn close(mut self: Box<Self>) {
        let _ = self.finish_inner();
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
