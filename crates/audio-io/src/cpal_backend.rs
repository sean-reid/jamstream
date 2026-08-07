//! Real devices via cpal: CoreAudio on macOS, WASAPI shared mode on
//! Windows, PipeWire/ALSA on Linux. On Windows this is the fallback half of
//! `WindowsBackend`, which prefers the direct WASAPI exclusive-mode path; see
//! `backend()` in lib.rs.
//!
//! The handler always sees the session rate; the #347 ladder decides how
//! each direction gets there, in one place, [`plan_direction`], which lives
//! with the rest of the rate policy in [`crate::cpal_policy`]. A device
//! that runs at the session rate opens as it is. A device that does not is
//! asked for it anyway where the host can be trusted to answer honestly
//! ([`verifies_negotiated_rate`]): CoreAudio moves the whole device clock,
//! WASAPI render and PipeWire convert in the OS. When the host refuses, or
//! cannot be trusted to try (ALSA), the stream opens at the device's own
//! rate with the boundary converter from [`crate::resample`] wrapped around
//! that direction's handler half. A host that only refuses once the stream is
//! up, which is how PipeWire negotiates, kills it from the error callback
//! instead; the device is demoted there so the reopen lands on the converter
//! rather than repeating the same attempt. The refusal that used to be the
//! whole policy survives only for a device whose native-rate open itself
//! fails, and for a Bluetooth hands-free microphone, which has no rate worth
//! carrying (#330).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::cpal_policy::{
    DirectionPlan, RateContext, plan_direction, rate_outcome, verifies_negotiated_rate,
};
use crate::format::map_frames;
use crate::rate::{RateOutcome, RateOutcomes, log_rate_outcomes};
use crate::resample::{MAX_CHUNK_FRAMES, converting_capture, converting_playback, session_frames};
use crate::types::{
    AudioBackend, AudioError, DeviceInfo, Direction, DuplexHandler, FormFactor, Result,
    StreamConfig, StreamHandle,
};
use crate::wasapi_policy;

type CaptureFn = Box<dyn FnMut(&[f32]) + Send>;
type PlaybackFn = Box<dyn FnMut(&mut [f32]) + Send>;

/// Platform default cpal host.
pub struct CpalBackend {
    host: cpal::Host,
    /// Devices whose clock this app set and then lost the stream on: the
    /// contested-clock demotion (#347). Another app snapping the nominal
    /// rate back kills the stream; re-setting the clock on the reopen would
    /// play tug-of-war with a musician's other software, so a demoted
    /// device is opened at its own rate through the boundary converter for
    /// the rest of this backend's life, which is the session's. No retry,
    /// no fight. Shared with every stream handle, which is what records the
    /// demotion when it dies.
    demoted: Arc<Mutex<HashSet<String>>>,
}

impl CpalBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            host: cpal::default_host(),
            demoted: Arc::new(Mutex::new(HashSet::new())),
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
            let (name, form_factor) = match device.description() {
                Ok(d) => (
                    d.name().to_string(),
                    form_factor(d.device_type(), d.interface_type()),
                ),
                Err(_) => (id.id().to_string(), FormFactor::Unknown),
            };

            if let Ok(config) = device.default_input_config() {
                let (min, max) = buffer_bounds(config.buffer_size());
                out.push(DeviceInfo {
                    id: id.id().to_string(),
                    name: name.clone(),
                    is_default: default_in.as_ref() == Some(&id),
                    direction: Direction::Capture,
                    form_factor,
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
                    form_factor,
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
        let rates = RateContext {
            rate: config.sample_rate,
            host: self.host.id().name(),
        };
        let host_converts = verifies_negotiated_rate(rates.host);
        let in_device = self.find_device(capture, Direction::Capture)?;
        let out_device = self.find_device(playback, Direction::Playback)?;
        let in_side = SideContext {
            device: &in_device,
            direction: Direction::Capture,
            native: in_device.default_input_config().map_err(|e| map_err(&e))?,
            form: form_factor_of(&in_device),
        };
        let out_side = SideContext {
            device: &out_device,
            direction: Direction::Playback,
            native: out_device
                .default_output_config()
                .map_err(|e| map_err(&e))?,
            form: form_factor_of(&out_device),
        };
        let in_plan = plan_direction(
            rates,
            Direction::Capture,
            &in_side.native,
            in_device
                .supported_input_configs()
                .map_err(|e| map_err(&e))?,
            host_converts,
            in_side.form,
            device_demoted(&self.demoted, &in_device),
        )?;
        let out_plan = plan_direction(
            rates,
            Direction::Playback,
            &out_side.native,
            out_device
                .supported_output_configs()
                .map_err(|e| map_err(&e))?,
            host_converts,
            out_side.form,
            device_demoted(&self.demoted, &out_device),
        )?;

        let (on_capture, on_playback) = handler.into_parts();
        let flags = Arc::new(StreamFlags::default());

        let (input, in_rate, in_added) =
            open_capture_side(&in_side, in_plan, &config, rates, on_capture, &flags)?;
        let (output, out_rate, out_added) =
            open_playback_side(&out_side, out_plan, &config, rates, on_playback, &flags)?;

        // The rung each direction landed on, carried on the handle so a
        // reopen racing a read can never show one stream the other's
        // outcome.
        let rate = RateOutcomes {
            capture: rate_outcome(rates, in_side.native.sample_rate(), in_rate, in_added),
            playback: rate_outcome(rates, out_side.native.sample_rate(), out_rate, out_added),
        };
        log_rate_outcomes(&rate);

        // The devices whose clock this open moved, remembered on the handle:
        // if this stream dies, they are demoted rather than fought over.
        let mut clock_set = Vec::new();
        if matches!(rate.capture, RateOutcome::ClockSet { .. })
            && let Ok(id) = in_device.id()
        {
            clock_set.push(id.id().to_string());
        }
        if matches!(rate.playback, RateOutcome::ClockSet { .. })
            && let Ok(id) = out_device.id()
        {
            clock_set.push(id.id().to_string());
        }

        // The devices running at a rate they never advertised, because the
        // host was trusted to refuse what it could not carry. Only a
        // direction that is still on its attempt counts: one that already
        // fell through to the converter has nothing left to demote.
        let mut attempted = Vec::new();
        if in_plan.attempted
            && in_added.is_none()
            && let Ok(id) = in_device.id()
        {
            attempted.push(id.id().to_string());
        }
        if out_plan.attempted
            && out_added.is_none()
            && let Ok(id) = out_device.id()
        {
            attempted.push(id.id().to_string());
        }

        // Negotiated callback sizes, per host: the WASAPI shared-mode device
        // period, the ALSA period, CoreAudio's device frame size, PipeWire's
        // last quantum (the request until the first callback lands). Each is
        // scaled from its direction's opened rate to session-rate frames, the
        // unit `buffer_frames` promises. Their sum is the best latency
        // estimate cpal exposes; the larger one is what a callback-sized
        // consumer has to absorb.
        let in_frames = input
            .buffer_size()
            .ok()
            .map(|n| session_frames(n, config.sample_rate, in_rate));
        let out_frames = output
            .buffer_size()
            .ok()
            .map(|n| session_frames(n, config.sample_rate, out_rate));
        let latency_frames = match (in_frames, out_frames) {
            (Some(i), Some(o)) => Some(i + o),
            (one, other) => one.or(other),
        };
        let buffer_frames = match (in_frames, out_frames) {
            (Some(i), Some(o)) => Some(i.max(o)),
            (one, other) => one.or(other),
        };

        // Started last, with nothing left to do but hand the stream over. cpal
        // 0.18 streams start paused, and this used to play them the moment both
        // were built, ahead of the rung report, the demotion bookkeeping, and
        // the callback-size queries. Everything that arrives in that window
        // arrives in a ring the caller has not been handed yet, and a device
        // that calls back promptly puts tens of milliseconds of capture there
        // (#436). CoreAudio happens to take about that long to deliver its
        // first callback, so it hid the window rather than avoiding it. The
        // Windows exclusive path already waits: its device threads do not start
        // until the handler halves are handed over at the end of the open.
        input.play().map_err(|e| map_err(&e))?;
        output.play().map_err(|e| map_err(&e))?;

        Ok(Box::new(CpalStreamHandle {
            input,
            output,
            flags,
            latency_frames,
            buffer_frames,
            rate,
            clock_set,
            attempted,
            demoted: Arc::clone(&self.demoted),
            demotion_noted: AtomicBool::new(false),
        }))
    }
}

/// Whether a device sits in the backend's contested-clock demotion set.
fn device_demoted(demoted: &Mutex<HashSet<String>>, device: &cpal::Device) -> bool {
    device
        .id()
        .is_ok_and(|id| demoted.lock().is_ok_and(|set| set.contains(id.id())))
}

/// Everything a dead stream demotes, so every later open in this backend
/// takes those devices at their own rate through the boundary converter.
///
/// The clock this app set is demoted whatever killed the stream (#347's
/// contested-clock decision: no retry, no fight). A device running at a rate
/// it never advertised is demoted only when the host is what refused it,
/// because an unplug says nothing about the rate and putting a working device
/// on the converter for good would cost latency for nothing.
fn demote_dead_stream(
    demoted: &Mutex<HashSet<String>>,
    clock_set: &[String],
    attempted: &[String],
    config_refused: bool,
) {
    demote(
        demoted,
        clock_set,
        "a stream that had moved this device's clock died",
    );
    if config_refused {
        demote(
            demoted,
            attempted,
            "the host refused the rate this stream was opened at, after it came up",
        );
    }
}

/// Records devices as demoted, once each, logging `why` the first time.
fn demote(demoted: &Mutex<HashSet<String>>, devices: &[String], why: &str) {
    let Ok(mut set) = demoted.lock() else { return };
    for id in devices {
        if set.insert(id.clone()) {
            tracing::warn!(
                device = %id,
                why,
                "opening this device through the boundary converter from now on"
            );
        }
    }
}

/// One direction's device, native config, and reported shape: what the open
/// path needs to build the stream and to word a refusal.
struct SideContext<'a> {
    device: &'a cpal::Device,
    direction: Direction,
    native: cpal::SupportedStreamConfig,
    form: FormFactor,
}

/// Builds the capture stream for its plan. An attempted session-rate open
/// that the host refuses falls to the boundary converter at the device's own
/// rate (#347 rung 3); the refusal survives only when that native-rate open
/// fails too. Returns the stream, the rate it opened at, and the converter's
/// added latency when this direction converts.
fn open_capture_side(
    side: &SideContext<'_>,
    plan: DirectionPlan,
    config: &StreamConfig,
    rates: RateContext,
    on_capture: CaptureFn,
    flags: &Arc<StreamFlags>,
) -> Result<(cpal::Stream, u32, Option<f32>)> {
    let refused = |e: &AudioError| rates.refused(side.direction, &side.native, side.form, Some(e));
    if plan.convert {
        let device_rate = plan.open.sample_rate();
        let (wrapped, added) =
            converting_capture(on_capture, config.sample_rate, device_rate, config.channels);
        let stream = build_input(side.device, &plan.open, config, wrapped, flags)
            .map_err(|e| refused(&e))?;
        return Ok((stream, device_rate, Some(added)));
    }
    if !plan.attempted {
        let stream = build_input(side.device, &plan.open, config, on_capture, flags)?;
        return Ok((stream, plan.open.sample_rate(), None));
    }
    // The attempt: a session-rate config the device never advertised, put to
    // a host that reports honestly. cpal consumes the callback even when the
    // build fails, so it rides in a slot the fallback can take back.
    let (shim, slot) = recoverable_capture(on_capture);
    let failure = match build_input(side.device, &plan.open, config, shim, flags) {
        Ok(stream) => return Ok((stream, plan.open.sample_rate(), None)),
        Err(e) => e,
    };
    let Some(inner) = slot.lock().ok().and_then(|mut s| s.take()) else {
        return Err(refused(&failure));
    };
    tracing::info!(
        host = rates.host,
        device_rate = side.native.sample_rate(),
        error = %failure,
        "capture will not open at the session rate; converting at the device's own"
    );
    let device_rate = side.native.sample_rate();
    let (wrapped, added) =
        converting_capture(inner, config.sample_rate, device_rate, config.channels);
    let stream =
        build_input(side.device, &side.native, config, wrapped, flags).map_err(|e| refused(&e))?;
    Ok((stream, device_rate, Some(added)))
}

/// [`open_capture_side`]'s mirror for the playback half.
fn open_playback_side(
    side: &SideContext<'_>,
    plan: DirectionPlan,
    config: &StreamConfig,
    rates: RateContext,
    on_playback: PlaybackFn,
    flags: &Arc<StreamFlags>,
) -> Result<(cpal::Stream, u32, Option<f32>)> {
    let refused = |e: &AudioError| rates.refused(side.direction, &side.native, side.form, Some(e));
    if plan.convert {
        let device_rate = plan.open.sample_rate();
        let (wrapped, added) = converting_playback(
            on_playback,
            config.sample_rate,
            device_rate,
            config.channels,
        );
        let stream = build_output(side.device, &plan.open, config, wrapped, flags)
            .map_err(|e| refused(&e))?;
        return Ok((stream, device_rate, Some(added)));
    }
    if !plan.attempted {
        let stream = build_output(side.device, &plan.open, config, on_playback, flags)?;
        return Ok((stream, plan.open.sample_rate(), None));
    }
    let (shim, slot) = recoverable_playback(on_playback);
    let failure = match build_output(side.device, &plan.open, config, shim, flags) {
        Ok(stream) => return Ok((stream, plan.open.sample_rate(), None)),
        Err(e) => e,
    };
    let Some(inner) = slot.lock().ok().and_then(|mut s| s.take()) else {
        return Err(refused(&failure));
    };
    tracing::info!(
        host = rates.host,
        device_rate = side.native.sample_rate(),
        error = %failure,
        "playback will not open at the session rate; converting at the device's own"
    );
    let device_rate = side.native.sample_rate();
    let (wrapped, added) =
        converting_playback(inner, config.sample_rate, device_rate, config.channels);
    let stream =
        build_output(side.device, &side.native, config, wrapped, flags).map_err(|e| refused(&e))?;
    Ok((stream, device_rate, Some(added)))
}

/// Wraps a handler half so a failed build can recover it: cpal takes the data
/// callback by value and drops it on failure, and the native-rate retry needs
/// the same closure back. After a successful build the shim claims the inner
/// closure from the slot on the first callback; the lock is uncontended by
/// construction, because the only other locker is the failure path, which
/// runs strictly before any callback can (a failed build leaves no stream,
/// and a built stream is paused until both directions have opened). If it
/// ever were contended the shim gives up that one buffer and claims on the
/// next callback, because a device thread must never block on a lock.
fn recoverable_capture(inner: CaptureFn) -> (CaptureFn, Arc<Mutex<Option<CaptureFn>>>) {
    let slot = Arc::new(Mutex::new(Some(inner)));
    let shared = Arc::clone(&slot);
    let mut claimed: Option<CaptureFn> = None;
    let shim: CaptureFn = Box::new(move |samples: &[f32]| {
        if claimed.is_none() {
            let Ok(mut slot) = shared.try_lock() else {
                return;
            };
            claimed = slot.take();
        }
        if let Some(inner) = claimed.as_mut() {
            inner(samples);
        }
    });
    (shim, slot)
}

/// [`recoverable_capture`]'s mirror for the playback half.
fn recoverable_playback(inner: PlaybackFn) -> (PlaybackFn, Arc<Mutex<Option<PlaybackFn>>>) {
    let slot = Arc::new(Mutex::new(Some(inner)));
    let shared = Arc::clone(&slot);
    let mut claimed: Option<PlaybackFn> = None;
    let shim: PlaybackFn = Box::new(move |out: &mut [f32]| {
        if claimed.is_none() {
            let Ok(mut slot) = shared.try_lock() else {
                return;
            };
            claimed = slot.take();
        }
        if let Some(inner) = claimed.as_mut() {
            inner(out);
        }
    });
    (shim, slot)
}

fn map_err(e: &cpal::Error) -> AudioError {
    match e.kind() {
        cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::HostUnavailable => {
            AudioError::DeviceGone
        }
        cpal::ErrorKind::UnsupportedConfig | cpal::ErrorKind::UnsupportedOperation => {
            AudioError::Unsupported(e.to_string())
        }
        _ => {
            let message = e.to_string();
            if is_windows_access_denied(&message) {
                // Same walk to the setting as the exclusive path's
                // AccessDenied classification, so the two paths agree.
                AudioError::Unsupported(format!(
                    "microphone access denied by Windows privacy settings ({message}); {}",
                    wasapi_policy::MIC_PRIVACY_REMEDY
                ))
            } else {
                AudioError::Backend(message)
            }
        }
    }
}

/// True when a cpal error is Windows' `E_ACCESSDENIED` (0x80070005), which in
/// practice is "Let desktop apps access your microphone" switched off.
///
/// cpal's WASAPI host has no classification for it: the HRESULT falls through
/// to `ErrorKind::BackendError` carrying `io::Error`'s rendering of the code,
/// which is the raw value (-2147024891 is 0x80070005 as an i32) or, where the
/// system message table resolves it, "Access is denied". Matching the message
/// is the only handle cpal leaves.
fn is_windows_access_denied(message: &str) -> bool {
    message.contains("0x80070005")
        || message.contains("-2147024891")
        || message.contains("Access is denied")
}

/// [`FormFactor`] from cpal's decoded device description. cpal fills the
/// description from `PKEY_AudioEndpoint_FormFactor` and the device enumerator
/// on WASAPI and from device properties on PipeWire; CoreAudio fills nothing,
/// so macOS devices land on `Unknown`. Bluetooth is checked first because the
/// transport, not the shape, is what the 48 kHz refusal keys on.
fn form_factor(device_type: cpal::DeviceType, interface: cpal::InterfaceType) -> FormFactor {
    if interface == cpal::InterfaceType::Bluetooth {
        return FormFactor::Bluetooth;
    }
    match device_type {
        cpal::DeviceType::Speaker => FormFactor::Speakers,
        cpal::DeviceType::Headphones => FormFactor::Headphones,
        cpal::DeviceType::Headset => FormFactor::Headset,
        cpal::DeviceType::Microphone => FormFactor::Microphone,
        _ => match interface {
            cpal::InterfaceType::Line => FormFactor::LineLevel,
            cpal::InterfaceType::Hdmi | cpal::InterfaceType::DisplayPort => FormFactor::Hdmi,
            _ => FormFactor::Unknown,
        },
    }
}

fn form_factor_of(device: &cpal::Device) -> FormFactor {
    device
        .description()
        .map(|d| form_factor(d.device_type(), d.interface_type()))
        .unwrap_or(FormFactor::Unknown)
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

/// What one stream's error callback has to tell the poller: that the stream
/// is dead, and whether the host killed it by refusing the config the open
/// asked for.
///
/// The second flag is the asynchronous half of the rate ladder. PipeWire
/// negotiates after the build returns, so its refusal of an attempted
/// session-rate open arrives at the error callback as `UnsupportedConfig`
/// rather than out of `build_*_stream`. A synchronous refusal falls through
/// to the converter inside the open; this is what lets the asynchronous one
/// do the same on the next open instead of producing the identical plan
/// forever, which left rung 3 unreachable on that host.
#[derive(Debug, Default)]
struct StreamFlags {
    errored: AtomicBool,
    rate_refused: AtomicBool,
}

/// A refusal of the config the stream was opened at, as opposed to any other
/// way a stream can die. cpal reports a negotiated-format mismatch this way
/// on every host that verifies one.
fn is_config_refusal(kind: cpal::ErrorKind) -> bool {
    matches!(kind, cpal::ErrorKind::UnsupportedConfig)
}

fn make_error_callback(flags: &Arc<StreamFlags>) -> impl FnMut(cpal::Error) + Send + 'static {
    let flags = Arc::clone(flags);
    move |e: cpal::Error| {
        // Informational kinds do not invalidate the stream; everything else
        // (device gone, stream invalidated, backend failure) means the app
        // must surface a device-gone state and reopen. Latching on the
        // refusal is also what stops a stream that came back at the wrong
        // rate from playing on at the wrong pitch.
        let kind = e.kind();
        if matches!(
            kind,
            cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied
        ) {
            return;
        }
        // Ordered so a poller that sees the death also sees the reason.
        if is_config_refusal(kind) {
            flags.rate_refused.store(true, Ordering::Relaxed);
        }
        flags.errored.store(true, Ordering::Release);
    }
}

fn build_input(
    device: &cpal::Device,
    open: &cpal::SupportedStreamConfig,
    config: &StreamConfig,
    mut on_capture: CaptureFn,
    flags: &Arc<StreamFlags>,
) -> Result<cpal::Stream> {
    let device_ch = usize::from(open.channels().max(1));
    let handler_ch = usize::from(config.channels);
    let mut scratch = vec![0.0f32; MAX_CHUNK_FRAMES * handler_ch];

    let stream_config = cpal::StreamConfig {
        channels: open.channels(),
        sample_rate: open.sample_rate(),
        buffer_size: choose_buffer_size(open, config.buffer_frames),
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
            make_error_callback(flags),
            None,
        )
        .map_err(|e| map_err(&e))
}

fn build_output(
    device: &cpal::Device,
    open: &cpal::SupportedStreamConfig,
    config: &StreamConfig,
    mut on_playback: PlaybackFn,
    flags: &Arc<StreamFlags>,
) -> Result<cpal::Stream> {
    let device_ch = usize::from(open.channels().max(1));
    let handler_ch = usize::from(config.channels);
    let mut scratch = vec![0.0f32; MAX_CHUNK_FRAMES * handler_ch];

    let stream_config = cpal::StreamConfig {
        channels: open.channels(),
        sample_rate: open.sample_rate(),
        buffer_size: choose_buffer_size(open, config.buffer_frames),
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
            make_error_callback(flags),
            None,
        )
        .map_err(|e| map_err(&e))
}

struct CpalStreamHandle {
    input: cpal::Stream,
    output: cpal::Stream,
    flags: Arc<StreamFlags>,
    latency_frames: Option<u32>,
    buffer_frames: Option<u32>,
    rate: RateOutcomes,
    /// Devices whose clock this stream's open moved, joined to the backend's
    /// `demoted` set the first time [`StreamHandle::errored`] reads true.
    clock_set: Vec<String>,
    /// Devices this stream asked for a rate they never advertised; demoted
    /// with the clock-set ones when the host is what refused it.
    attempted: Vec<String>,
    demoted: Arc<Mutex<HashSet<String>>>,
    demotion_noted: AtomicBool,
}

impl StreamHandle for CpalStreamHandle {
    fn latency_frames(&self) -> Option<u32> {
        self.latency_frames
    }

    fn buffer_frames(&self) -> Option<u32> {
        self.buffer_frames
    }

    fn errored(&self) -> bool {
        let errored = self.flags.errored.load(Ordering::Acquire);
        // A dead stream demotes what it has to before the caller's reopen can
        // ask for the same thing again. Polled off the RT path, so the lock
        // inside is fine.
        if errored
            && !(self.clock_set.is_empty() && self.attempted.is_empty())
            && !self.demotion_noted.swap(true, Ordering::Relaxed)
        {
            demote_dead_stream(
                &self.demoted,
                &self.clock_set,
                &self.attempted,
                self.flags.rate_refused.load(Ordering::Relaxed),
            );
        }
        errored
    }

    fn rate_outcomes(&self) -> Option<RateOutcomes> {
        Some(self.rate)
    }

    fn close(self: Box<Self>) {
        // Pause is best-effort; dropping the streams tears them down.
        let _ = self.input.pause();
        let _ = self.output.pause();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpal_policy::fixtures::{ctx, native, range};
    use cpal::SampleFormat;

    /// The demotion record itself: a dead clock-set stream's devices land in
    /// the set once each, and repeats are not re-announced.
    #[test]
    fn a_dead_clock_set_stream_demotes_its_devices_once() {
        let demoted = Mutex::new(HashSet::new());
        demote(&demoted, &["dev-a".to_owned(), "dev-b".to_owned()], "why");
        demote(&demoted, &["dev-a".to_owned()], "why");
        let set = demoted.lock().unwrap();
        assert_eq!(set.len(), 2);
        assert!(set.contains("dev-a") && set.contains("dev-b"));
    }

    /// The production error callback, fed the errors the hosts really send.
    /// A rerouted device and a refused realtime promotion leave the stream
    /// alive; everything else kills it, and a refusal of the config the
    /// stream was opened at is recorded as such, because it is the only kind
    /// the next open can do anything about.
    #[test]
    fn the_error_callback_latches_the_death_and_the_reason() {
        let cases = [
            (cpal::ErrorKind::DeviceChanged, false, false),
            (cpal::ErrorKind::RealtimeDenied, false, false),
            (cpal::ErrorKind::DeviceNotAvailable, true, false),
            (cpal::ErrorKind::StreamInvalidated, true, false),
            (cpal::ErrorKind::BackendError, true, false),
            // PipeWire's asynchronous answer, verbatim in shape.
            (cpal::ErrorKind::UnsupportedConfig, true, true),
        ];
        for (kind, dead, refused) in cases {
            let flags = Arc::new(StreamFlags::default());
            let mut callback = make_error_callback(&flags);
            callback(cpal::Error::with_message(
                kind,
                "Negotiated format mismatch: expected 2 channels at 48000 Hz, \
                 got 2 channels at 44100 Hz"
                    .to_owned(),
            ));
            assert_eq!(flags.errored.load(Ordering::Acquire), dead, "{kind:?}");
            assert_eq!(
                flags.rate_refused.load(Ordering::Relaxed),
                refused,
                "{kind:?}"
            );
        }
    }

    /// The asynchronous half of rung 3, end to end within this module: a
    /// PipeWire graph that advertises only 44.1 kHz is attempted at 48, comes
    /// up, and refuses from its param-changed handler. The refusal has to
    /// demote the device, or the next `plan_direction` produces the identical
    /// attempted plan and the converter is unreachable on that host forever.
    #[test]
    fn a_refusal_after_the_stream_came_up_reaches_the_converter_on_the_next_open() {
        let native = native(44_100, 2);
        let ranges = [range(44_100, 44_100, 2, SampleFormat::F32)];
        let plan = plan_direction(
            ctx("PipeWire"),
            Direction::Capture,
            &native,
            ranges.into_iter(),
            true,
            FormFactor::Unknown,
            false,
        )
        .expect("a converting host is asked");
        assert!(plan.attempted && !plan.convert);

        // The graph answers after the build returned, on the real callback.
        let flags = Arc::new(StreamFlags::default());
        let mut callback = make_error_callback(&flags);
        callback(cpal::Error::with_message(
            cpal::ErrorKind::UnsupportedConfig,
            "Negotiated format mismatch: expected 2 channels at 48000 Hz, \
             got 2 channels at 44100 Hz"
                .to_owned(),
        ));
        assert!(flags.errored.load(Ordering::Acquire), "the stream is dead");

        let demoted = Mutex::new(HashSet::new());
        demote_dead_stream(
            &demoted,
            &[],
            &["pipewire-in".to_owned()],
            flags.rate_refused.load(Ordering::Relaxed),
        );
        assert!(demoted.lock().unwrap().contains("pipewire-in"));

        let plan = plan_direction(
            ctx("PipeWire"),
            Direction::Capture,
            &native,
            ranges.into_iter(),
            true,
            FormFactor::Unknown,
            true,
        )
        .expect("the reopen takes the converter");
        assert!(plan.convert, "rung 3 must be reachable on PipeWire");
        assert!(!plan.attempted, "the same attempt must not be repeated");
        assert_eq!(plan.open.sample_rate(), 44_100);
    }

    /// And the other half: an unplug says nothing about the rate, so the
    /// device keeps its rung. Demoting on every death would put a device that
    /// was carrying the session fine on the converter, and its latency, for
    /// the rest of the session.
    #[test]
    fn an_unplug_does_not_demote_the_rate_a_device_was_carrying() {
        let flags = Arc::new(StreamFlags::default());
        let mut callback = make_error_callback(&flags);
        callback(cpal::Error::with_message(
            cpal::ErrorKind::DeviceNotAvailable,
            "device disappeared".to_owned(),
        ));
        let demoted = Mutex::new(HashSet::new());
        demote_dead_stream(
            &demoted,
            &[],
            &["usb-in".to_owned()],
            flags.rate_refused.load(Ordering::Relaxed),
        );
        assert!(demoted.lock().unwrap().is_empty());

        // A clock this app set is demoted whatever killed the stream: the
        // contest is with the other app, not with the cable.
        demote_dead_stream(&demoted, &["clocked".to_owned()], &[], false);
        assert!(demoted.lock().unwrap().contains("clocked"));
    }

    /// The #367 handoff, in the sequence cpal imposes: the build takes the
    /// callback by value and drops it on failure, so the native-rate retry
    /// can only run the handler the slot gives back. If that handoff broke,
    /// the fallback stream would run a dead closure and the session would go
    /// silent on exactly the devices rung 3 exists for, with every rate test
    /// still passing, because they all stop at the plan.
    ///
    /// The counter tells one closure from another: a rebuilt handler would
    /// start its count over.
    #[test]
    fn a_failed_attempt_hands_the_capture_handler_back_whole() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        let mut calls = 0usize;
        let inner: CaptureFn = Box::new(move |samples: &[f32]| {
            calls += 1;
            sink.lock().unwrap().push((calls, samples.to_vec()));
        });
        let (shim, slot) = recoverable_capture(inner);
        // The attempt failed, so cpal dropped what it was handed.
        drop(shim);
        let mut recovered = slot
            .lock()
            .unwrap()
            .take()
            .expect("the handler outlives the build that failed");
        recovered(&[0.25, -0.25]);
        recovered(&[0.5]);
        let heard = heard.lock().unwrap();
        assert_eq!(heard.len(), 2);
        assert_eq!(heard[0], (1, vec![0.25, -0.25]));
        assert_eq!(heard[1], (2, vec![0.5]), "the same closure, still counting");
    }

    #[test]
    fn a_failed_attempt_hands_the_playback_handler_back_whole() {
        let mut n = 0.0f32;
        let inner: PlaybackFn = Box::new(move |out: &mut [f32]| {
            for s in out.iter_mut() {
                n += 1.0;
                *s = n;
            }
        });
        let (shim, slot) = recoverable_playback(inner);
        drop(shim);
        let mut recovered = slot
            .lock()
            .unwrap()
            .take()
            .expect("the handler outlives the build that failed");
        let mut buf = [0.0f32; 3];
        recovered(&mut buf);
        assert_eq!(buf, [1.0, 2.0, 3.0]);
        recovered(&mut buf);
        assert_eq!(buf, [4.0, 5.0, 6.0], "the same closure, continuing");
    }

    /// The other outcome: the attempt came up, so the shim is what cpal
    /// calls. Every callback reaches the handler, the first one included, and
    /// the claim happens once rather than per callback.
    #[test]
    fn a_successful_attempt_loses_no_callback_to_the_shim() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        let inner: CaptureFn = Box::new(move |samples: &[f32]| {
            sink.lock().unwrap().push(samples.to_vec());
        });
        let (mut shim, slot) = recoverable_capture(inner);
        shim(&[1.0]);
        shim(&[2.0, 3.0]);
        assert_eq!(
            *heard.lock().unwrap(),
            vec![vec![1.0], vec![2.0, 3.0]],
            "the first callback must not be the price of the shim"
        );
        assert!(
            slot.lock().unwrap().is_none(),
            "the handler is claimed out of the slot, once"
        );

        let mut n = 0.0f32;
        let inner: PlaybackFn = Box::new(move |out: &mut [f32]| {
            for s in out.iter_mut() {
                n += 1.0;
                *s = n;
            }
        });
        let (mut shim, _slot) = recoverable_playback(inner);
        let mut buf = [0.0f32; 2];
        shim(&mut buf);
        assert_eq!(buf, [1.0, 2.0]);
        shim(&mut buf);
        assert_eq!(buf, [3.0, 4.0]);
    }

    /// The one branch that swallows a callback. It is unreachable in the open
    /// path (a failed build leaves no stream to call back, and a built stream
    /// stays paused until both directions are open), but a device thread must
    /// never block on a lock, so the answer if it ever were contended is one
    /// buffer given up and a claim on the next callback. Silence for one
    /// buffer, not a stalled device thread and not a permanent loss.
    #[test]
    fn a_contended_claim_costs_one_buffer_and_never_blocks() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        let inner: CaptureFn = Box::new(move |samples: &[f32]| {
            sink.lock().unwrap().push(samples.to_vec());
        });
        let (mut shim, slot) = recoverable_capture(inner);
        let held = slot.lock().expect("the recovery path holds it");
        shim(&[1.0]);
        assert!(
            heard.lock().unwrap().is_empty(),
            "a contended claim delivers nothing rather than waiting"
        );
        drop(held);
        shim(&[2.0]);
        assert_eq!(
            *heard.lock().unwrap(),
            vec![vec![2.0]],
            "the next callback claims the handler"
        );

        let inner: PlaybackFn = Box::new(|out: &mut [f32]| out.fill(0.75));
        let (mut shim, slot) = recoverable_playback(inner);
        let held = slot.lock().expect("the recovery path holds it");
        // The buffer arrives zeroed and untouched means silence, which is
        // what an unclaimed playback callback has to leave behind.
        let mut buf = [0.0f32; 2];
        shim(&mut buf);
        assert_eq!(buf, [0.0, 0.0]);
        drop(held);
        shim(&mut buf);
        assert_eq!(buf, [0.75, 0.75]);
    }

    /// The Windows microphone privacy toggle reaches this backend as cpal's
    /// unclassified BackendError carrying io::Error's rendering of
    /// E_ACCESSDENIED, in whichever of its shapes; every shape must land on
    /// the actionable message, and nothing else may.
    #[test]
    fn a_windows_privacy_denial_names_the_setting_on_the_shared_path() {
        for message in [
            "IAudioClient::Initialize failed: 0x80070005",
            "OS Error -2147024891 (FormatMessageW() returned error)",
            "Access is denied. (os error 5)",
        ] {
            let err = cpal::Error::with_message(cpal::ErrorKind::BackendError, message.to_owned());
            let mapped = map_err(&err);
            let AudioError::Unsupported(msg) = mapped else {
                panic!("expected Unsupported for {message:?}, got {mapped:?}");
            };
            assert!(
                msg.contains("microphone access denied by Windows privacy settings"),
                "{msg}"
            );
            assert!(
                msg.contains("Settings, Privacy and security, Microphone"),
                "{msg}"
            );
            assert!(msg.contains(message), "the host's own words survive: {msg}");
        }

        // Any other unclassified failure stays a plain backend error.
        let other = cpal::Error::with_message(cpal::ErrorKind::BackendError, "OS Error 1450");
        assert!(matches!(map_err(&other), AudioError::Backend(_)));
    }

    /// The decode is a straight table plus one precedence rule: the Bluetooth
    /// transport outranks the shape, because it is what the refusal keys on.
    #[test]
    fn form_factors_decode_from_the_cpal_description() {
        use cpal::{DeviceType as D, InterfaceType as I};
        let cases = [
            (D::Speaker, I::BuiltIn, FormFactor::Speakers),
            (D::Headphones, I::Usb, FormFactor::Headphones),
            (D::Headset, I::Usb, FormFactor::Headset),
            (D::Microphone, I::Usb, FormFactor::Microphone),
            (D::Unknown, I::Line, FormFactor::LineLevel),
            (D::Unknown, I::Hdmi, FormFactor::Hdmi),
            (D::Unknown, I::DisplayPort, FormFactor::Hdmi),
            (D::Headset, I::Bluetooth, FormFactor::Bluetooth),
            (D::Speaker, I::Bluetooth, FormFactor::Bluetooth),
            (D::Unknown, I::BuiltIn, FormFactor::Unknown),
            (D::Virtual, I::Virtual, FormFactor::Unknown),
        ];
        for (device_type, interface, want) in cases {
            assert_eq!(
                form_factor(device_type, interface),
                want,
                "{device_type:?} over {interface:?}"
            );
        }
    }
}
