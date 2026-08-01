//! WASAPI exclusive mode, driven straight through `IAudioClient`.
//!
//! Shared mode costs 20-30 ms of device latency; exclusive mode costs about
//! 10, which on a 30 ms mouth-to-ear budget is the difference between playable
//! and not. cpal has no exclusive mode, so this backend talks to the
//! `wasapi` crate directly (the CamillaDSP precedent) and keeps cpal as the
//! automatic fallback.
//!
//! [`WindowsBackend`] is what [`crate::backend`] returns: it tries exclusive
//! first and drops to the cpal shared-mode path when the driver, another
//! application, or the user's endpoint settings say no. Which mode won is
//! visible through [`crate::active_device_mode`].
//!
//! ## Threading and COM
//!
//! One thread per direction, each of which owns its `IAudioClient` from
//! creation to teardown: COM objects are never sent between threads, and COM
//! is only ever initialized on threads this module spawns, so the caller's
//! apartment (eframe's UI thread, in particular) is left alone. Both threads
//! are promoted through MMCSS "Pro Audio" before they touch the device.
//!
//! Opening is a rendezvous: each thread creates and initializes its client,
//! reports the negotiated period, and only then waits for its half of the
//! [`DuplexHandler`]. That way a failure on either side is reported while the
//! parent still owns the handler, and the handler can be handed to cpal
//! instead. Both threads are always joined before the fallback opens the same
//! endpoint, because an initialized exclusive client holds it even before
//! `Start`.
//!
//! Opening blocks the caller until both threads report, exactly as cpal's
//! `build_*_stream` blocks on `IAudioClient::Initialize`; a driver that wedges
//! inside `Initialize` therefore stalls the open on either path.
//!
//! ## Real-time discipline
//!
//! Every buffer a device thread touches is allocated during the open, before
//! the stream starts. The steady-state loop is `WaitForSingleObject`,
//! `GetBuffer`/`ReleaseBuffer`, and a fixed-size format conversion: no
//! allocation, no locks, and no logging. The only logging on a device thread
//! happens during setup and once at teardown, off the audio path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wasapi::{
    AudioCaptureClient, AudioClient, AudioRenderClient, Device, DeviceEnumerator,
    Direction as WasapiDirection, Handle, SampleType, StreamMode, WasapiError, WaveFormat,
    deinitialize, initialize_mta,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AVRT_PRIORITY_CRITICAL, AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
    AvSetMmThreadPriority, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::core::w;

use crate::cpal_backend::CpalBackend;
use crate::format::{self, FormatSpec, SampleFormat, StageLayout};
use crate::mode::{DeviceMode, set_active_device_mode, set_render_conversion};
use crate::types::{
    AudioBackend, AudioError, DeviceInfo, Direction, DuplexHandler, FormFactor, Result,
    StreamConfig, StreamHandle,
};
use crate::wasapi_policy::{
    self as policy, EVENT_WAIT_MS, ExclusiveFailure, Fallback, MAX_CONSECUTIVE_TIMEOUTS, RetryGate,
};

/// Packets read per buffer event before going back to the wait. Exclusive mode
/// delivers one period per event; the extra passes only matter after a
/// scheduling hiccup, and the bound keeps a misbehaving driver from live-
/// locking the thread.
const CAPTURE_DRAIN_LIMIT: usize = 4;

/// Byte alignment the buffer size is rounded to. Every Intel High Definition
/// Audio device requires multiples of 128 bytes and other drivers do not mind,
/// so it is unconditional.
const PERIOD_ALIGN_BYTES: u32 = 128;

/// Sample rate and channel count [`AudioBackend::devices`] probes exclusive
/// support with. It has no [`StreamConfig`] to consult, and this is the only
/// configuration jamstream ever opens.
const PROBE_RATE: u32 = 48_000;
const PROBE_CHANNELS: u16 = 2;

type CaptureFn = Box<dyn FnMut(&[f32]) + Send>;
type PlaybackFn = Box<dyn FnMut(&mut [f32]) + Send>;

// ---------------------------------------------------------------------------
// Public backend: exclusive first, cpal shared mode second.
// ---------------------------------------------------------------------------

/// The Windows backend: WASAPI exclusive mode with an automatic shared-mode
/// fallback.
///
/// Device ids are `IMMDevice::GetId` strings, which is exactly what cpal's
/// WASAPI host uses too, so a device the user picked from [`Self::devices`]
/// resolves identically on either path.
pub struct WindowsBackend {
    exclusive: ExclusiveBackend,
    shared: CpalBackend,
    /// Suppresses exclusive probes that just failed for the same request; see
    /// [`policy::retry_cooldown`]. `Mutex` because `open_duplex` takes `&self`.
    gate: Mutex<RetryGate<RequestKey>>,
}

/// What makes two open requests "the same" for cooldown purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestKey {
    capture: Option<String>,
    playback: Option<String>,
    config: StreamConfig,
}

impl WindowsBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            exclusive: ExclusiveBackend,
            shared: CpalBackend::new(),
            gate: Mutex::new(RetryGate::new()),
        }
    }

    fn cooldown_remaining(&self, key: &RequestKey) -> Option<Duration> {
        self.gate.lock().ok()?.remaining(key, Instant::now())
    }

    fn note_failure(&self, key: RequestKey, cooldown: Duration) {
        if let Ok(mut gate) = self.gate.lock() {
            gate.block(key, cooldown, Instant::now());
        }
    }

    fn clear_cooldown(&self) {
        if let Ok(mut gate) = self.gate.lock() {
            gate.clear();
        }
    }

    fn open_shared(
        &self,
        capture: Option<&str>,
        playback: Option<&str>,
        config: StreamConfig,
        handler: DuplexHandler,
    ) -> Result<Box<dyn StreamHandle>> {
        let stream = self
            .shared
            .open_duplex(capture, playback, config, handler)?;
        set_active_device_mode(DeviceMode::Shared);
        tracing::info!(
            latency_frames = ?stream.latency_frames(),
            "wasapi shared mode active (cpal)"
        );
        Ok(stream)
    }
}

impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for WindowsBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsBackend").finish_non_exhaustive()
    }
}

impl AudioBackend for WindowsBackend {
    /// Endpoints as WASAPI sees them. `min_buffer_frames`/`max_buffer_frames`
    /// are the exclusive-mode period bounds for endpoints that accept
    /// exclusive mode at 48 kHz stereo, and `None` for those that do not: not
    /// every device does, and a device that will only ever run shared mode has
    /// no buffer range this backend can promise.
    ///
    /// Falls back to cpal's enumeration if the WASAPI enumeration itself
    /// fails, so a broken exclusive path never costs the user their device
    /// list.
    ///
    /// Form factors come from cpal's enumeration of the same endpoints: the
    /// `wasapi` crate exposes no property-store access, cpal's WASAPI host
    /// already decodes `PKEY_AudioEndpoint_FormFactor` and the Bluetooth
    /// enumerator, and the ids are the same `IMMDevice::GetId` strings on
    /// both paths, so the decode is borrowed rather than duplicated.
    fn devices(&self) -> Result<Vec<DeviceInfo>> {
        match self.exclusive.devices() {
            Ok(mut devices) => {
                if let Ok(shared) = self.shared.devices() {
                    for device in &mut devices {
                        if let Some(described) = shared
                            .iter()
                            .find(|s| s.id == device.id && s.direction == device.direction)
                        {
                            device.form_factor = described.form_factor;
                        }
                    }
                }
                Ok(devices)
            }
            Err(err) => {
                tracing::warn!(%err, "wasapi enumeration failed, using cpal's");
                self.shared.devices()
            }
        }
    }

    fn open_duplex(
        &self,
        capture: Option<&str>,
        playback: Option<&str>,
        config: StreamConfig,
        handler: DuplexHandler,
    ) -> Result<Box<dyn StreamHandle>> {
        // The user said no to exclusive: go straight to shared, with no probe
        // to fall back from. The probe would grab the endpoint for a moment
        // even when it fails, which is exactly the interruption they opted
        // out of (#331).
        if !config.allow_exclusive {
            tracing::info!("exclusive mode is disallowed by the request; opening shared");
            return self.open_shared(capture, playback, config, handler);
        }

        let key = RequestKey {
            capture: capture.map(str::to_owned),
            playback: playback.map(str::to_owned),
            config,
        };

        if let Some(remaining) = self.cooldown_remaining(&key) {
            tracing::debug!(
                remaining_ms = remaining.as_millis(),
                "skipping the exclusive-mode probe, it failed recently for this request"
            );
            return self.open_shared(capture, playback, config, handler);
        }

        let failure = match self.exclusive.try_open(capture, playback, config, handler) {
            Ok(stream) => {
                self.clear_cooldown();
                set_active_device_mode(DeviceMode::Exclusive);
                // Exclusive mode negotiated the wire format with the driver
                // itself; there is no audio engine in between to convert.
                set_render_conversion(false);
                tracing::info!(
                    latency_frames = ?stream.latency_frames(),
                    "wasapi exclusive mode active"
                );
                return Ok(stream);
            }
            Err(failure) => failure,
        };

        let decision = policy::fallback_decision(failure.failure);
        let cooldown = policy::retry_cooldown(failure.failure);
        tracing::warn!(
            reason = failure.failure.as_str(),
            detail = %failure.detail,
            ?decision,
            cooldown_ms = cooldown.as_millis(),
            "wasapi exclusive mode unavailable"
        );

        if decision == Fallback::Reject {
            return Err(failure.as_error());
        }
        self.note_failure(key, cooldown);
        match failure.handler {
            Some(handler) => self.open_shared(capture, playback, config, handler),
            // The handler was already in flight to a device thread that then
            // died. Nothing to fall back with; the client reopens shortly with
            // a fresh bridge.
            None => Err(failure.as_error()),
        }
    }
}

// ---------------------------------------------------------------------------
// Exclusive-mode backend
// ---------------------------------------------------------------------------

/// Exclusive mode only, with no fallback of its own. Not public: exclusive
/// mode alone is not a shippable configuration, so the only way to get one is
/// through [`WindowsBackend`].
struct ExclusiveBackend;

/// A failed exclusive open, carrying the handler back to the caller so it can
/// be handed to the fallback backend instead.
struct OpenFailure {
    failure: ExclusiveFailure,
    detail: String,
    handler: Option<DuplexHandler>,
}

impl OpenFailure {
    fn as_error(&self) -> AudioError {
        policy::open_error(self.failure, &self.detail)
    }
}

/// What a device thread reports back once it has an initialized client, or why
/// it could not get one.
struct Report {
    failure: ExclusiveFailure,
    detail: String,
}

impl Report {
    fn new(failure: ExclusiveFailure, detail: impl Into<String>) -> Self {
        Self {
            failure,
            detail: detail.into(),
        }
    }

    fn from_wasapi(context: &str, err: &WasapiError) -> Self {
        Self::new(classify(err), format!("{context}: {err}"))
    }
}

type ReadyResult = std::result::Result<u32, Report>;

impl ExclusiveBackend {
    /// Enumerate on a dedicated thread so COM is initialized somewhere we own.
    fn devices(&self) -> Result<Vec<DeviceInfo>> {
        thread::Builder::new()
            .name("jamstream-wasapi-enum".into())
            .spawn(|| {
                let _com = ComGuard::init()
                    .map_err(|e| AudioError::Backend(format!("COM initialisation: {e}")))?;
                enumerate()
            })
            .map_err(|e| AudioError::Backend(format!("spawning the enumeration thread: {e}")))?
            .join()
            .map_err(|_| AudioError::Backend("the enumeration thread panicked".into()))?
    }

    /// Try to open both directions in exclusive mode.
    ///
    /// On failure the handler comes back in the [`OpenFailure`] and both device
    /// threads have already been joined, so the endpoint is free for the
    /// shared-mode fallback.
    fn try_open(
        &self,
        capture: Option<&str>,
        playback: Option<&str>,
        config: StreamConfig,
        handler: DuplexHandler,
    ) -> std::result::Result<Box<dyn StreamHandle>, OpenFailure> {
        if config.channels == 0 {
            return Err(OpenFailure {
                failure: ExclusiveFailure::InvalidConfig,
                detail: "zero channels".into(),
                handler: Some(handler),
            });
        }

        let (on_capture, on_playback) = handler.into_parts();
        let errored = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let (capture_ready_tx, capture_ready_rx) = mpsc::channel::<ReadyResult>();
        let (capture_handler_tx, capture_handler_rx) = mpsc::channel::<CaptureFn>();
        let (render_ready_tx, render_ready_rx) = mpsc::channel::<ReadyResult>();
        let (render_handler_tx, render_handler_rx) = mpsc::channel::<PlaybackFn>();

        let capture_params = ThreadParams {
            device_id: capture.map(str::to_owned),
            direction: Direction::Capture,
            config,
            errored: Arc::clone(&errored),
            stop: Arc::clone(&stop),
        };
        let render_params = ThreadParams {
            device_id: playback.map(str::to_owned),
            direction: Direction::Playback,
            config,
            errored: Arc::clone(&errored),
            stop: Arc::clone(&stop),
        };

        let capture_thread = match thread::Builder::new()
            .name("jamstream-wasapi-capture".into())
            .spawn(move || capture_loop(&capture_params, &capture_ready_tx, &capture_handler_rx))
        {
            Ok(handle) => handle,
            Err(e) => {
                return Err(OpenFailure {
                    failure: ExclusiveFailure::Other,
                    detail: format!("spawning the capture thread: {e}"),
                    handler: recovered(on_capture, on_playback),
                });
            }
        };

        let render_thread = match thread::Builder::new()
            .name("jamstream-wasapi-render".into())
            .spawn(move || render_loop(&render_params, &render_ready_tx, &render_handler_rx))
        {
            Ok(handle) => handle,
            Err(e) => {
                drop(capture_handler_tx);
                stop.store(true, Ordering::Release);
                let _ = capture_thread.join();
                return Err(OpenFailure {
                    failure: ExclusiveFailure::Other,
                    detail: format!("spawning the render thread: {e}"),
                    handler: recovered(on_capture, on_playback),
                });
            }
        };

        let capture_ready = recv_ready(&capture_ready_rx, Direction::Capture);
        let render_ready = recv_ready(&render_ready_rx, Direction::Playback);

        let (capture_frames, render_frames) = match (capture_ready, render_ready) {
            (Ok(c), Ok(r)) => (c, r),
            (capture_ready, render_ready) => {
                // Dropping the handler senders releases whichever side did get
                // ready; it never started, so no audio was lost. Joining is
                // mandatory: an initialized exclusive client owns the endpoint
                // the fallback is about to ask for.
                drop(capture_handler_tx);
                drop(render_handler_tx);
                stop.store(true, Ordering::Release);
                let _ = capture_thread.join();
                let _ = render_thread.join();
                let report = capture_ready
                    .err()
                    .or_else(|| render_ready.err())
                    .unwrap_or_else(|| Report::new(ExclusiveFailure::Other, "unreported failure"));
                return Err(OpenFailure {
                    failure: report.failure,
                    detail: report.detail,
                    handler: recovered(on_capture, on_playback),
                });
            }
        };

        // Handing over the handler halves is what starts the streams.
        let sent = (
            capture_handler_tx.send(on_capture),
            render_handler_tx.send(on_playback),
        );
        if let (Ok(()), Ok(())) = sent {
            return Ok(Box::new(ExclusiveStream {
                errored,
                stop,
                threads: vec![capture_thread, render_thread],
                latency_frames: Some(capture_frames + render_frames),
                buffer_frames: Some(capture_frames.max(render_frames)),
            }));
        }

        stop.store(true, Ordering::Release);
        let _ = capture_thread.join();
        let _ = render_thread.join();
        // A half that reached a dead thread's channel is gone with it, so the
        // handler can only be offered to the fallback if both came back.
        let handler = match sent {
            (Err(c), Err(r)) => recovered(c.0, r.0),
            _ => None,
        };
        Err(OpenFailure {
            failure: ExclusiveFailure::Other,
            detail: "a device thread died between reporting ready and starting".into(),
            handler,
        })
    }
}

/// Rebuild the handler an aborted open never handed over.
fn recovered(capture: CaptureFn, playback: PlaybackFn) -> Option<DuplexHandler> {
    Some(DuplexHandler::from_parts(capture, playback))
}

fn recv_ready(rx: &Receiver<ReadyResult>, direction: Direction) -> ReadyResult {
    match rx.recv() {
        Ok(result) => result,
        // Only reachable if the thread panicked; the loops report before they
        // can exit any other way.
        Err(_) => Err(Report::new(
            ExclusiveFailure::Other,
            format!("the {direction:?} device thread ended without reporting"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Stream handle
// ---------------------------------------------------------------------------

struct ExclusiveStream {
    errored: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    latency_frames: Option<u32>,
    buffer_frames: Option<u32>,
}

impl ExclusiveStream {
    /// Signal both threads, then join. Both flags are set before either join so
    /// the two waits expire concurrently.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl StreamHandle for ExclusiveStream {
    fn latency_frames(&self) -> Option<u32> {
        self.latency_frames
    }

    fn buffer_frames(&self) -> Option<u32> {
        self.buffer_frames
    }

    fn errored(&self) -> bool {
        self.errored.load(Ordering::Acquire)
    }

    fn close(mut self: Box<Self>) {
        self.shutdown();
    }
}

impl Drop for ExclusiveStream {
    fn drop(&mut self) {
        // Idempotent: `close` already drained the thread list.
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Device threads
// ---------------------------------------------------------------------------

struct ThreadParams {
    device_id: Option<String>,
    direction: Direction,
    config: StreamConfig,
    errored: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

/// Everything a direction needs once its client is initialized.
struct Prepared {
    client: AudioClient,
    event: Handle,
    spec: FormatSpec,
    /// The period the driver actually gave us, in frames.
    buffer_frames: u32,
}

/// COM for the lifetime of a thread we own.
///
/// Bind it first in the function that needs it: locals drop in reverse
/// declaration order, so every COM object created afterwards is released before
/// `CoUninitialize` runs. Nothing here ever initializes COM on a thread it did
/// not spawn, which keeps eframe's UI apartment out of it.
struct ComGuard;

impl ComGuard {
    fn init() -> std::result::Result<Self, String> {
        initialize_mta()
            .ok()
            .map(|()| Self)
            .map_err(|e| e.to_string())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        deinitialize();
    }
}

fn capture_loop(
    params: &ThreadParams,
    ready: &Sender<ReadyResult>,
    handler_rx: &Receiver<CaptureFn>,
) {
    let _com = match ComGuard::init() {
        Ok(guard) => guard,
        Err(e) => {
            let _ = ready.send(Err(Report::new(
                ExclusiveFailure::Other,
                format!("COM initialisation: {e}"),
            )));
            return;
        }
    };
    let _mmcss = MmcssGuard::promote();
    let prepared = match prepare(params) {
        Ok(prepared) => prepared,
        Err(report) => {
            let _ = ready.send(Err(report));
            return;
        }
    };
    let client = match prepared.client.get_audiocaptureclient() {
        Ok(client) => client,
        Err(e) => {
            let _ = ready.send(Err(Report::from_wasapi("IAudioCaptureClient", &e)));
            return;
        }
    };
    if ready.send(Ok(prepared.buffer_frames)).is_err() {
        return;
    }
    // Blocks until the parent commits to this stream. An error means it gave
    // up on the other direction, so this client must go away unstarted.
    let Ok(mut on_capture) = handler_rx.recv() else {
        return;
    };

    let mut stage = CaptureStage::new(&prepared, params.config.channels);
    if let Err(e) = prepared.client.start_stream() {
        fail(params, "starting the capture stream", &e);
        return;
    }

    let mut timeouts = 0;
    'wait: while !params.stop.load(Ordering::Acquire) {
        if !wait_for_period(&prepared.event, &mut timeouts) {
            device_lost(params);
            break;
        }
        for _ in 0..CAPTURE_DRAIN_LIMIT {
            match stage.deliver(&client, &mut on_capture) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    fail(params, "reading from the capture device", &e);
                    break 'wait;
                }
            }
        }
    }
    let _ = prepared.client.stop_stream();
}

fn render_loop(
    params: &ThreadParams,
    ready: &Sender<ReadyResult>,
    handler_rx: &Receiver<PlaybackFn>,
) {
    let _com = match ComGuard::init() {
        Ok(guard) => guard,
        Err(e) => {
            let _ = ready.send(Err(Report::new(
                ExclusiveFailure::Other,
                format!("COM initialisation: {e}"),
            )));
            return;
        }
    };
    let _mmcss = MmcssGuard::promote();
    let prepared = match prepare(params) {
        Ok(prepared) => prepared,
        Err(report) => {
            let _ = ready.send(Err(report));
            return;
        }
    };
    let client = match prepared.client.get_audiorenderclient() {
        Ok(client) => client,
        Err(e) => {
            let _ = ready.send(Err(Report::from_wasapi("IAudioRenderClient", &e)));
            return;
        }
    };
    if ready.send(Ok(prepared.buffer_frames)).is_err() {
        return;
    }
    let Ok(mut on_playback) = handler_rx.recv() else {
        return;
    };

    let mut stage = RenderStage::new(&prepared, params.config.channels);
    // Prime the whole buffer before Start, per the event-driven render
    // pattern: starting an empty exclusive buffer glitches the first period.
    if let Err(e) = stage.write(&client, &mut on_playback, prepared.buffer_frames as usize) {
        fail(params, "priming the render buffer", &e);
        return;
    }
    if let Err(e) = prepared.client.start_stream() {
        fail(params, "starting the render stream", &e);
        return;
    }

    let mut timeouts = 0;
    while !params.stop.load(Ordering::Acquire) {
        if !wait_for_period(&prepared.event, &mut timeouts) {
            device_lost(params);
            break;
        }
        // Exclusive event-driven mode hands back the whole buffer every period.
        let available = match prepared.client.get_available_space_in_frames() {
            Ok(frames) => frames.min(prepared.buffer_frames),
            Err(e) => {
                fail(params, "querying render buffer space", &e);
                break;
            }
        };
        if let Err(e) = stage.write(&client, &mut on_playback, available as usize) {
            fail(params, "writing to the render device", &e);
            break;
        }
    }
    let _ = prepared.client.stop_stream();
}

/// Wait for one buffer event. Returns false once the stream has gone quiet for
/// [`MAX_CONSECUTIVE_TIMEOUTS`] waits, which is a dead device rather than a
/// late one.
fn wait_for_period(event: &Handle, timeouts: &mut u32) -> bool {
    match event.wait_for_event(EVENT_WAIT_MS) {
        Ok(()) => {
            *timeouts = 0;
            true
        }
        Err(_) => {
            *timeouts += 1;
            !policy::stream_is_dead(*timeouts)
        }
    }
}

/// Latch the stream as errored. Called off the steady-state path only: the app
/// polls [`StreamHandle::errored`], closes, and reopens.
fn fail(params: &ThreadParams, context: &str, err: &WasapiError) {
    let lost = matches!(err, WasapiError::Windows(w) if policy::is_device_loss(w.code().0));
    tracing::warn!(
        direction = ?params.direction,
        %err,
        device_lost = lost,
        "wasapi exclusive stream failed: {context}"
    );
    params.errored.store(true, Ordering::Release);
}

fn device_lost(params: &ThreadParams) {
    tracing::warn!(
        direction = ?params.direction,
        waits = MAX_CONSECUTIVE_TIMEOUTS,
        "wasapi exclusive stream stopped signalling buffer events"
    );
    params.errored.store(true, Ordering::Release);
}

/// Resolve the endpoint, negotiate a format, and initialize an exclusive
/// event-driven client for it.
fn prepare(params: &ThreadParams) -> std::result::Result<Prepared, Report> {
    let wanted = wasapi_direction(params.direction);
    let enumerator =
        DeviceEnumerator::new().map_err(|e| Report::from_wasapi("IMMDeviceEnumerator", &e))?;
    let device = match &params.device_id {
        None => enumerator
            .get_default_device(&wanted)
            .map_err(|e| Report::new(classify(&e), format!("default device: {e}")))?,
        Some(id) => {
            let device = enumerator
                .get_device(id)
                .map_err(|e| Report::new(classify(&e), format!("device {id}: {e}")))?;
            if device.get_direction() != wanted {
                return Err(Report::new(
                    ExclusiveFailure::DeviceNotFound,
                    format!("device {id} is not a {:?} endpoint", params.direction),
                ));
            }
            device
        }
    };

    let mut client = device
        .get_iaudioclient()
        .map_err(|e| Report::from_wasapi("IAudioClient", &e))?;
    let config = params.config;
    let native_channels = device
        .get_device_format()
        .ok()
        .map(|wave| wave.get_nchannels());
    let (spec, wave) = negotiate(
        &client,
        config.sample_rate,
        config.channels,
        native_channels,
    )
    .ok_or_else(|| {
        Report::new(
            ExclusiveFailure::UnsupportedFormat,
            format!(
                "no exclusive format at {} Hz for {} channels",
                config.sample_rate, config.channels
            ),
        )
    })?;

    let requested = policy::clamp_period_frames(config.buffer_frames);
    let desired_hns = policy::period_100ns(requested, spec.sample_rate);
    let period_hns = client
        .calculate_aligned_period_near(desired_hns, Some(PERIOD_ALIGN_BYTES), &wave)
        .map_err(|e| Report::from_wasapi("aligning the device period", &e))?;

    if let Err(err) =
        client.initialize_client(&wave, &wanted, &StreamMode::EventsExclusive { period_hns })
    {
        // The one failure worth retrying in place: the driver names the buffer
        // size it wanted, and Initialize may only be called once per client.
        // https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-initialize
        if classify(&err) != ExclusiveFailure::BufferSizeNotAligned {
            return Err(Report::from_wasapi("IAudioClient::Initialize", &err));
        }
        let aligned = client
            .get_buffer_size()
            .map_err(|e| Report::from_wasapi("reading the aligned buffer size", &e))?;
        let period_hns = policy::period_100ns(aligned, spec.sample_rate);
        tracing::debug!(
            direction = ?params.direction,
            aligned_frames = aligned,
            "retrying IAudioClient::Initialize with the driver's buffer size"
        );
        client = device
            .get_iaudioclient()
            .map_err(|e| Report::from_wasapi("re-activating IAudioClient", &e))?;
        client
            .initialize_client(&wave, &wanted, &StreamMode::EventsExclusive { period_hns })
            .map_err(|e| Report::from_wasapi("IAudioClient::Initialize (aligned retry)", &e))?;
    }

    let event = client
        .set_get_eventhandle()
        .map_err(|e| Report::from_wasapi("IAudioClient::SetEventHandle", &e))?;
    let buffer_frames = client
        .get_buffer_size()
        .map_err(|e| Report::from_wasapi("IAudioClient::GetBufferSize", &e))?;

    tracing::info!(
        direction = ?params.direction,
        sample_format = ?spec.format,
        device_channels = spec.channels,
        handler_channels = config.channels,
        sample_rate = spec.sample_rate,
        requested_frames = config.buffer_frames,
        buffer_frames,
        "opened a wasapi exclusive-mode client"
    );

    Ok(Prepared {
        client,
        event,
        spec,
        buffer_frames,
    })
}

/// Offer formats to the driver best-first and return the first it accepts,
/// paired with the [`FormatSpec`] describing its byte layout.
///
/// The spec, not the returned [`WaveFormat`], is what conversion keys on:
/// `is_supported_exclusive_with_quirks` may hand back a plain `WAVEFORMATEX`
/// copy whose `SubFormat` GUID is zeroed, so only its framing fields are
/// trustworthy. A candidate whose framing disagrees with ours is skipped
/// rather than trusted.
fn negotiate(
    client: &AudioClient,
    sample_rate: u32,
    channels: u16,
    native_channels: Option<u16>,
) -> Option<(FormatSpec, WaveFormat)> {
    for spec in format::format_candidates(sample_rate, channels, native_channels) {
        let Ok(accepted) = client.is_supported_exclusive_with_quirks(&wave_format(spec)) else {
            continue;
        };
        if spec.frames_like(
            accepted.get_nchannels(),
            accepted.get_samplespersec(),
            accepted.get_blockalign(),
        ) {
            return Some((spec, accepted));
        }
        tracing::debug!(
            ?spec,
            accepted = ?accepted,
            "skipping an accepted format whose framing does not match the request"
        );
    }
    None
}

fn wave_format(spec: FormatSpec) -> WaveFormat {
    let sample_type = if spec.format.is_float() {
        SampleType::Float
    } else {
        SampleType::Int
    };
    WaveFormat::new(
        usize::from(spec.format.store_bits()),
        usize::from(spec.format.valid_bits()),
        &sample_type,
        spec.sample_rate as usize,
        usize::from(spec.channels),
        None,
    )
}

const fn wasapi_direction(direction: Direction) -> WasapiDirection {
    match direction {
        Direction::Capture => WasapiDirection::Capture,
        Direction::Playback => WasapiDirection::Render,
    }
}

/// Map a [`WasapiError`] onto the fallback table's vocabulary.
fn classify(err: &WasapiError) -> ExclusiveFailure {
    match err {
        WasapiError::Windows(w) => policy::classify_hresult(w.code().0),
        WasapiError::UnsupportedFormat | WasapiError::UnsupportedSubformat(_) => {
            ExclusiveFailure::UnsupportedFormat
        }
        WasapiError::DeviceNotFound(_) => ExclusiveFailure::DeviceNotFound,
        WasapiError::ClientNotInit | WasapiError::EventTimeout => {
            ExclusiveFailure::DeviceInvalidated
        }
        _ => ExclusiveFailure::Other,
    }
}

// ---------------------------------------------------------------------------
// Conversion stages (the real-time path)
// ---------------------------------------------------------------------------

/// Scratch buffers and layout for one direction. Everything is sized at
/// construction; the per-period methods only ever take subslices.
struct Stage {
    format: SampleFormat,
    block_align: usize,
    device_channels: usize,
    handler_channels: usize,
    bytes: Vec<u8>,
    device_floats: Vec<f32>,
    handler_floats: Vec<f32>,
}

impl Stage {
    /// `periods` buys headroom for a late wake-up that finds more than one
    /// period waiting. The sizing itself is [`StageLayout`], which is pure
    /// arithmetic and tested on every host.
    fn new(prepared: &Prepared, handler_channels: u16, periods: usize) -> Self {
        let layout = StageLayout::new(
            prepared.spec,
            handler_channels,
            prepared.buffer_frames,
            periods,
        );
        Self {
            format: layout.format,
            block_align: layout.block_align,
            device_channels: layout.device_channels,
            handler_channels: layout.handler_channels,
            bytes: vec![0; layout.byte_len()],
            device_floats: vec![0.0; layout.device_float_len()],
            handler_floats: vec![0.0; layout.handler_float_len()],
        }
    }

    const fn same_layout(&self) -> bool {
        self.device_channels == self.handler_channels
    }
}

struct CaptureStage(Stage);

impl CaptureStage {
    fn new(prepared: &Prepared, handler_channels: u16) -> Self {
        Self(Stage::new(prepared, handler_channels, 2))
    }

    /// Read one packet and hand it to the handler. Returns the frames read; 0
    /// means the device had nothing more this period.
    fn deliver(
        &mut self,
        client: &AudioCaptureClient,
        on_capture: &mut CaptureFn,
    ) -> std::result::Result<u32, WasapiError> {
        let stage = &mut self.0;
        let (frames, info) = client.read_from_device(&mut stage.bytes)?;
        if frames == 0 {
            return Ok(0);
        }
        let frames_usize = frames as usize;
        let device_samples = frames_usize * stage.device_channels;
        let same_layout = stage.same_layout();
        let decoded = &mut stage.device_floats[..device_samples];
        if info.flags.silent {
            decoded.fill(0.0);
        } else {
            format::decode_to_f32(
                &stage.bytes[..frames_usize * stage.block_align],
                stage.format,
                decoded,
            );
        }
        if same_layout {
            on_capture(decoded);
        } else {
            let mapped = &mut stage.handler_floats[..frames_usize * stage.handler_channels];
            format::map_frames(
                &stage.device_floats[..device_samples],
                stage.device_channels,
                mapped,
                stage.handler_channels,
            );
            on_capture(mapped);
        }
        Ok(frames)
    }
}

struct RenderStage(Stage);

impl RenderStage {
    fn new(prepared: &Prepared, handler_channels: u16) -> Self {
        // Render never writes more than one period per event, so one is enough.
        Self(Stage::new(prepared, handler_channels, 1))
    }

    /// Pull one period from the handler and write it to the device.
    fn write(
        &mut self,
        client: &AudioRenderClient,
        on_playback: &mut PlaybackFn,
        frames: usize,
    ) -> std::result::Result<(), WasapiError> {
        if frames == 0 {
            return Ok(());
        }
        let stage = &mut self.0;
        let device_samples = frames * stage.device_channels;
        if stage.same_layout() {
            let staged = &mut stage.device_floats[..device_samples];
            staged.fill(0.0);
            on_playback(staged);
        } else {
            let staged = &mut stage.handler_floats[..frames * stage.handler_channels];
            staged.fill(0.0);
            on_playback(staged);
            format::map_frames(
                &stage.handler_floats[..frames * stage.handler_channels],
                stage.handler_channels,
                &mut stage.device_floats[..device_samples],
                stage.device_channels,
            );
        }
        let bytes = frames * stage.block_align;
        format::encode_from_f32(
            &stage.device_floats[..device_samples],
            stage.format,
            &mut stage.bytes[..bytes],
        );
        client.write_to_device(frames, &stage.bytes[..bytes], None)
    }
}

// ---------------------------------------------------------------------------
// Thread scheduling
// ---------------------------------------------------------------------------

/// MMCSS registration for the lifetime of a device thread.
///
/// "Pro Audio" is the MMCSS task Windows reserves for low-latency audio work;
/// it is what lets a 5 ms callback survive alongside a browser. MMCSS can be
/// unavailable (group policy, or a container without the scheduler), so a
/// plain time-critical thread priority is the documented consolation prize.
struct MmcssGuard {
    handle: Option<HANDLE>,
}

impl MmcssGuard {
    fn promote() -> Self {
        let mut task_index = 0u32;
        // SAFETY: the task name is a static null-terminated wide literal and
        // `task_index` is a live local; both outlive the call, which is all
        // AvSetMmThreadCharacteristicsW requires. It affects only the calling
        // thread, and the handle it returns is reverted in `Drop`.
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
            Ok(handle) => {
                // SAFETY: `handle` is the live MMCSS registration just returned
                // for this thread, and the priority is a valid AVRT_PRIORITY.
                if let Err(err) = unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_CRITICAL) } {
                    tracing::warn!(%err, "AvSetMmThreadPriority failed; MMCSS default priority");
                }
                Self {
                    handle: Some(handle),
                }
            }
            Err(err) => {
                tracing::warn!(%err, "MMCSS unavailable, falling back to thread priority");
                // SAFETY: GetCurrentThread returns a pseudo-handle to this
                // thread that needs no closing, and the priority constant is a
                // valid THREAD_PRIORITY.
                if let Err(err) =
                    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) }
                {
                    tracing::warn!(%err, "SetThreadPriority failed; audio thread runs at normal priority");
                }
                Self { handle: None }
            }
        }
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

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

fn enumerate() -> Result<Vec<DeviceInfo>> {
    let enumerator = DeviceEnumerator::new()
        .map_err(|e| AudioError::Backend(format!("IMMDeviceEnumerator: {e}")))?;
    let mut out = Vec::new();
    for direction in [Direction::Capture, Direction::Playback] {
        let wanted = wasapi_direction(direction);
        let default_id = enumerator
            .get_default_device(&wanted)
            .ok()
            .and_then(|device| device.get_id().ok());
        let Ok(collection) = enumerator.get_device_collection(&wanted) else {
            continue;
        };
        for index in 0..collection.get_nbr_devices().unwrap_or(0) {
            // A device can vanish mid-enumeration; skip rather than fail.
            let Ok(device) = collection.get_device_at_index(index) else {
                continue;
            };
            let Ok(id) = device.get_id() else { continue };
            let name = device.get_friendlyname().unwrap_or_else(|_| id.clone());
            let (min_buffer_frames, max_buffer_frames) = exclusive_bounds(&device);
            out.push(DeviceInfo {
                is_default: default_id.as_ref() == Some(&id),
                id,
                name,
                direction,
                // The wasapi crate exposes no property-store access;
                // WindowsBackend::devices overlays the form factor from
                // cpal's enumeration of the same endpoints.
                form_factor: FormFactor::Unknown,
                min_buffer_frames,
                max_buffer_frames,
            });
        }
    }
    Ok(out)
}

/// Exclusive-mode period bounds in frames, or `(None, None)` when the endpoint
/// will not take exclusive mode at the probe configuration at all.
///
/// Deliberately probes the requested channel count only, not the device's own:
/// enumeration is synchronous for the UI, and each candidate costs the driver
/// an `IsFormatSupported` round trip (several, with the channel-mask quirks).
/// A device that would only work at its native channel count therefore reports
/// no bounds here and still gets the full negotiation when it is opened.
fn exclusive_bounds(device: &Device) -> (Option<u32>, Option<u32>) {
    let Ok(client) = device.get_iaudioclient() else {
        return (None, None);
    };
    let Some((spec, _)) = negotiate(&client, PROBE_RATE, PROBE_CHANNELS, None) else {
        return (None, None);
    };
    let Ok((_default_period, min_period)) = client.get_device_period() else {
        return (None, None);
    };
    let Some((min, max)) = policy::exclusive_period_bounds(min_period, spec.sample_rate) else {
        return (None, None);
    };
    (Some(min), Some(max))
}

/// What can be tested on Windows without a device.
///
/// Everything below runs in CI on the windows-latest runner, because none of
/// it opens an endpoint: error classification, the format WAVEFORMATEX we
/// offer, the cooldown key, and the gate wiring. The arithmetic these sit on
/// lives in `wasapi_policy` and `format`, which are compiled and tested on
/// every host.
///
/// What is left needs real hardware and is honestly out of reach here: the
/// exclusive-mode `Initialize` negotiation against a driver, the
/// buffer-alignment retry, the capture and render loops, MMCSS promotion, and
/// the handover that starts the streams. Those paths are verified by hand on
/// real hardware only. The hosted CI runners have no audio endpoint and no
/// workflow passes `--run-ignored`, so a regression there keeps every gate
/// green; nothing automated stands behind them. The two ignored tests,
/// `tests/cpal_devices.rs` and `tests/hardware_loopback.rs`, are the manual
/// pre-release checks: run both on a Windows machine with a real device
/// before shipping a release that touches this file.
#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::{Error as WinError, GUID, HRESULT};

    fn windows_error(code: u32) -> WasapiError {
        WasapiError::Windows(WinError::from_hresult(HRESULT(code as i32)))
    }

    /// Every arm of the classifier that is not the HRESULT delegation, because
    /// the fallback table and the cooldown are chosen from the result and a
    /// variant landing on `Other` gets the wrong one of both.
    #[test]
    fn wasapi_errors_classify_to_their_conditions() {
        assert_eq!(
            classify(&WasapiError::UnsupportedFormat),
            ExclusiveFailure::UnsupportedFormat
        );
        assert_eq!(
            classify(&WasapiError::UnsupportedSubformat(GUID::zeroed())),
            ExclusiveFailure::UnsupportedFormat
        );
        assert_eq!(
            classify(&WasapiError::DeviceNotFound("Speakers".into())),
            ExclusiveFailure::DeviceNotFound
        );
        assert_eq!(
            classify(&WasapiError::ClientNotInit),
            ExclusiveFailure::DeviceInvalidated
        );
        assert_eq!(
            classify(&WasapiError::EventTimeout),
            ExclusiveFailure::DeviceInvalidated
        );
        assert_eq!(
            classify(&WasapiError::RenderToCaptureDevice),
            ExclusiveFailure::Other
        );
    }

    /// The HRESULT path is what the driver actually fails through, and it has
    /// to reach the same table `wasapi_policy` tests directly.
    #[test]
    fn a_windows_hresult_classifies_through_the_policy_table() {
        for code in [
            0x8889_000A_u32, // AUDCLNT_E_DEVICE_IN_USE
            0x8889_000E,     // AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED
            0x8889_0019,     // AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED
            0x8889_0004,     // AUDCLNT_E_DEVICE_INVALIDATED
            0x8000_4005,     // E_FAIL, deliberately unclassified
        ] {
            assert_eq!(
                classify(&windows_error(code)),
                policy::classify_hresult(code as i32),
                "{code:#010x}"
            );
        }
    }

    /// The alignment retry only happens for one classification, and it is the
    /// one failure `IAudioClient::Initialize` can be called again after.
    #[test]
    fn only_a_misaligned_buffer_is_retried_in_place() {
        assert_eq!(
            classify(&windows_error(0x8889_0019)),
            ExclusiveFailure::BufferSizeNotAligned
        );
        for other in [0x8889_0008_u32, 0x8889_000A, 0x8889_0004, 0x8007_0057] {
            assert_ne!(
                classify(&windows_error(other)),
                ExclusiveFailure::BufferSizeNotAligned,
                "{other:#010x} must not be retried in place"
            );
        }
    }

    #[test]
    fn directions_map_to_wasapi_endpoints() {
        assert_eq!(
            wasapi_direction(Direction::Capture),
            WasapiDirection::Capture
        );
        assert_eq!(
            wasapi_direction(Direction::Playback),
            WasapiDirection::Render
        );
    }

    /// The negotiation loop offers `wave_format(spec)` and then accepts the
    /// driver's reply only if it frames audio the way `spec` says. So every
    /// candidate we offer has to satisfy that check against its own
    /// WAVEFORMATEX, or a compliant driver echoing our request back would be
    /// rejected.
    #[test]
    fn every_offered_format_matches_the_spec_it_came_from() {
        for spec in format::format_candidates(48_000, 2, Some(6)) {
            let wave = wave_format(spec);
            assert!(
                spec.frames_like(
                    wave.get_nchannels(),
                    wave.get_samplespersec(),
                    wave.get_blockalign()
                ),
                "{spec:?} does not match the WAVEFORMATEX built from it"
            );
            assert_eq!(
                wave.get_bitspersample(),
                spec.format.store_bits(),
                "{spec:?}"
            );
            assert_eq!(
                wave.get_validbitspersample(),
                spec.format.valid_bits(),
                "{spec:?}"
            );
        }
    }

    fn key(capture: Option<&str>, playback: Option<&str>, buffer_frames: u32) -> RequestKey {
        RequestKey {
            capture: capture.map(str::to_owned),
            playback: playback.map(str::to_owned),
            config: StreamConfig {
                buffer_frames,
                ..StreamConfig::default()
            },
        }
    }

    /// The cooldown is keyed on the whole request, because a user who changes
    /// device or buffer size has changed the question, and the old verdict says
    /// nothing about the new one.
    #[test]
    fn the_cooldown_key_is_the_whole_request() {
        assert_eq!(
            key(Some("a"), Some("b"), 240),
            key(Some("a"), Some("b"), 240)
        );
        assert_ne!(
            key(Some("a"), Some("b"), 240),
            key(Some("a"), Some("b"), 480)
        );
        assert_ne!(
            key(Some("a"), Some("b"), 240),
            key(Some("c"), Some("b"), 240)
        );
        assert_ne!(
            key(Some("a"), Some("b"), 240),
            key(Some("a"), Some("c"), 240)
        );
        // The system default is a different request from naming that same
        // device explicitly: the default can move underneath us.
        assert_ne!(key(None, None, 240), key(Some("a"), Some("b"), 240));
    }

    /// The gate wiring on the backend, as opposed to the gate itself, which
    /// `wasapi_policy` tests against an injected clock. Opens nothing.
    #[test]
    fn a_failure_gates_that_request_and_success_clears_it() {
        let backend = WindowsBackend::new();
        let failed = key(Some("interface"), Some("interface"), 240);
        let other = key(Some("headphones"), Some("headphones"), 240);

        assert!(backend.cooldown_remaining(&failed).is_none());

        backend.note_failure(failed.clone(), Duration::from_secs(60));
        let remaining = backend
            .cooldown_remaining(&failed)
            .expect("the request that failed is gated");
        assert!(remaining <= Duration::from_secs(60));
        assert!(
            backend.cooldown_remaining(&other).is_none(),
            "a different device must not inherit the verdict"
        );

        backend.clear_cooldown();
        assert!(backend.cooldown_remaining(&failed).is_none());
    }

    /// A failure that carries no cooldown must not gate anything, or a device
    /// that was merely unplugged and plugged back in would be stuck in shared
    /// mode for the rest of the session.
    #[test]
    fn a_zero_cooldown_gates_nothing() {
        let backend = WindowsBackend::new();
        let request = key(None, None, 240);
        backend.note_failure(
            request.clone(),
            policy::retry_cooldown(ExclusiveFailure::DeviceInvalidated),
        );
        assert!(backend.cooldown_remaining(&request).is_none());
    }
}
