//! Real devices via cpal: CoreAudio on macOS, WASAPI shared mode on
//! Windows, PipeWire/ALSA on Linux. On Windows this is the fallback half of
//! `WindowsBackend`, which prefers the direct WASAPI exclusive-mode path; see
//! `backend()` in lib.rs.
//!
//! Every stream runs at the session rate or not at all, because jamstream
//! never resamples. Which of those two happens is decided in one place,
//! [`plan_open`], and it turns on whether the host reports a stream it could
//! not open at the rate asked for: see [`verifies_negotiated_rate`]. The OS
//! itself may still resample on the render side (WASAPI's AUTOCONVERTPCM,
//! PipeWire's graph); [`render_conversion`] detects that and every open
//! discloses it through [`crate::active_render_conversion`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::format::map_frames;
use crate::mode::set_render_conversion;
use crate::resample::MAX_CHUNK_FRAMES;
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
        let converts = verifies_negotiated_rate(rates.host);
        let in_device = self.find_device(capture, Direction::Capture)?;
        let out_device = self.find_device(playback, Direction::Playback)?;
        let in_form = form_factor_of(&in_device);
        let out_form = form_factor_of(&out_device);
        let in_native = in_device.default_input_config().map_err(|e| map_err(&e))?;
        let out_native = out_device
            .default_output_config()
            .map_err(|e| map_err(&e))?;
        let (in_open, in_attempted) = plan_open(
            &in_native,
            in_device
                .supported_input_configs()
                .map_err(|e| map_err(&e))?,
            config.sample_rate,
            converts,
        )
        .ok_or_else(|| rates.refused(Direction::Capture, &in_native, in_form, None))?;
        let (out_open, out_attempted) = plan_open(
            &out_native,
            out_device
                .supported_output_configs()
                .map_err(|e| map_err(&e))?,
            config.sample_rate,
            converts,
        )
        .ok_or_else(|| rates.refused(Direction::Playback, &out_native, out_form, None))?;

        let (on_capture, on_playback) = handler.into_parts();
        let errored = Arc::new(AtomicBool::new(false));

        let input =
            build_input(&in_device, &in_open, &config, on_capture, &errored).map_err(|e| {
                if in_attempted {
                    rates.refused(Direction::Capture, &in_native, in_form, Some(&e))
                } else {
                    e
                }
            })?;
        let output =
            build_output(&out_device, &out_open, &config, on_playback, &errored).map_err(|e| {
                if out_attempted {
                    rates.refused(Direction::Playback, &out_native, out_form, Some(&e))
                } else {
                    e
                }
            })?;

        // cpal 0.18 streams start paused.
        input.play().map_err(|e| map_err(&e))?;
        output.play().map_err(|e| map_err(&e))?;

        // Render-side conversion disclosure: once per open, plus the report
        // the UI polls. The device's default config is its engine rate (the
        // WASAPI mix format, the PipeWire graph rate), which is what an OS
        // converter would be bridging to.
        if let Some(converting) =
            render_conversion(rates.host, out_native.sample_rate(), config.sample_rate)
        {
            set_render_conversion(converting);
            if converting {
                tracing::warn!(
                    device_rate = out_native.sample_rate(),
                    stream_rate = config.sample_rate,
                    host = rates.host,
                    "the OS is resampling the render stream to the playback \
                     device's rate, adding latency the buffer sizes do not show"
                );
            }
        }

        // Negotiated callback sizes, per host: the WASAPI shared-mode device
        // period, the ALSA period, CoreAudio's device frame size, PipeWire's
        // last quantum (the request until the first callback lands). Their sum
        // is the best latency estimate cpal exposes; the larger one is what a
        // callback-sized consumer has to absorb.
        let (in_frames, out_frames) = (input.buffer_size().ok(), output.buffer_size().ok());
        let latency_frames = match (in_frames, out_frames) {
            (Some(i), Some(o)) => Some(i + o),
            (one, other) => one.or(other),
        };
        let buffer_frames = match (in_frames, out_frames) {
            (Some(i), Some(o)) => Some(i.max(o)),
            (one, other) => one.or(other),
        };

        Ok(Box::new(CpalStreamHandle {
            input,
            output,
            errored,
            latency_frames,
            buffer_frames,
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

/// Whether `host` reports a stream it could not open at the rate that was
/// asked for, instead of quietly running it at another one.
///
/// A host that reports can be asked for the session rate even when the device
/// does not advertise it: either the host converts and the stream is correct,
/// or it refuses and we get an error. A host that does not report has to be
/// taken at its word, because the failure mode is a session playing sharp and
/// fast with nothing to show for it.
///
/// Unknown hosts are treated as not reporting. Only three can be the default
/// host in the shapes we build, and all three are listed.
const fn rate_policy(host: &str) -> Option<bool> {
    // `const fn` cannot match on &str, so this is a byte comparison chain.
    match host.as_bytes() {
        // Verifies the negotiated format in its param_changed handler and
        // invalidates the stream on a mismatch, and converts otherwise, so a
        // 44.1 kHz graph carries a 48 kHz client stream correctly.
        b"PipeWire" => Some(true),
        // Sets the device's nominal rate itself, and refuses up front when
        // the rate is not among those the device reports.
        b"CoreAudio" => Some(true),
        // Output endpoints convert through AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
        // capture endpoints do not, and IAudioClient::Initialize fails rather
        // than resampling.
        b"WASAPI" => Some(true),
        // snd_pcm_hw_params_set_rate is called with ValueOr::Nearest and the
        // result is never read back, so a 44.1 kHz card asked for 48 kHz
        // simply runs at 44.1.
        b"ALSA" => Some(false),
        _ => None,
    }
}

fn verifies_negotiated_rate(host: &str) -> bool {
    rate_policy(host).unwrap_or(false)
}

/// Whether a render stream that opened at `stream_rate` is being resampled by
/// the OS, given that the device's own engine runs at `device_rate`.
///
/// Acceptable by design since the #347 decision, but it must be disclosed:
/// the conversion adds latency the buffer arithmetic never sees. Per host,
/// because the same rate mismatch means different things:
///
/// - WASAPI opens output with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, so the
///   engine keeps its mix rate and converts our stream into it.
/// - PipeWire keeps its graph rate and converts client streams the same way.
/// - CoreAudio sets the device's nominal rate to the stream's, so a stream
///   that opened is really running at its rate; a device that could not
///   reclock was refused up front.
/// - ALSA converts nothing: a stream that opened at the session rate is
///   running at it.
///
/// `None` for a host the table does not know, which is also what the report
/// shows: an unknown host earns no claim either way.
const fn render_conversion(host: &str, device_rate: u32, stream_rate: u32) -> Option<bool> {
    // `const fn` cannot match on &str, so this is a byte comparison chain.
    match host.as_bytes() {
        b"WASAPI" | b"PipeWire" => Some(device_rate != stream_rate),
        b"CoreAudio" | b"ALSA" => Some(false),
        _ => None,
    }
}

/// The device config to open at `rate`, and whether opening it is an attempt.
///
/// An attempt is a config the device never advertised, offered to a host that
/// [reports a rate it could not honour](verifies_negotiated_rate). None means
/// there is nothing worth trying: the device does not run at the session rate
/// and this host would play everything at the wrong pitch and speed rather
/// than say so.
fn plan_open(
    native: &cpal::SupportedStreamConfig,
    supported: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    rate: u32,
    host_converts: bool,
) -> Option<(cpal::SupportedStreamConfig, bool)> {
    if let Some(config) = config_at_rate(native, supported, rate) {
        return Some((config, false));
    }
    host_converts.then(|| {
        let attempt = cpal::SupportedStreamConfig::new(
            native.channels(),
            rate,
            *native.buffer_size(),
            native.sample_format(),
        );
        (attempt, true)
    })
}

/// The device config to open at `rate`: the device's own when it already runs
/// there, else a supported range that covers it.
fn config_at_rate(
    native: &cpal::SupportedStreamConfig,
    supported: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    rate: u32,
) -> Option<cpal::SupportedStreamConfig> {
    if native.sample_rate() == rate {
        return Some(*native);
    }
    // f32 first, because that is the sample type the streams are built with,
    // then the native channel layout, then the widest.
    supported
        .filter_map(|r| r.try_with_sample_rate(rate))
        .max_by_key(|c| {
            (
                c.sample_format() == cpal::SampleFormat::F32,
                c.channels() == native.channels(),
                c.channels(),
            )
        })
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

/// Native rates that mark a telephony endpoint: the Bluetooth hands-free
/// profile and its wideband variant. A capture device at one of these has no
/// 48 kHz mode to switch to, whatever its settings pages suggest.
const fn is_telephony_rate(rate: u32) -> bool {
    matches!(rate, 8_000 | 16_000)
}

/// What the person at the keyboard can do about a device that will not run at
/// the session rate. jamstream does not resample, so this sentence is the
/// entire remedy, and it is per host rather than per platform because the two
/// Linux hosts fail for opposite reasons.
fn rate_remedy(host: &str, rate: u32) -> String {
    match host {
        // mmsys.cpl by name, because Windows 11's Settings app no longer has
        // Recording and Playback tabs to walk to; the applet is the one entry
        // point that exists on every Windows this can run on.
        "WASAPI" => format!(
            "set that device to {rate} Hz: run mmsys.cpl (Settings > System > \
             Sound > More sound settings), Recording or Playback, the device's \
             Properties, Advanced, Default Format"
        ),
        "CoreAudio" => format!(
            "check Audio MIDI Setup, Format, for a {rate} Hz entry on that \
             device, and use another device if there is none"
        ),
        // Deliberately not "change default.clock.rate": PipeWire converts
        // between its graph rate and a client stream, so the graph rate is
        // never what refused us, and sending someone to edit a config file
        // and restart a daemon for no gain is worse than saying nothing.
        "PipeWire" => format!(
            "PipeWire converts sample rates, so its graph rate is not the \
             problem; this device has no {rate} Hz mode, so use another one"
        ),
        // Reached only when no sound server is running, since cpal prefers
        // PipeWire when it is. ALSA hands the card its own rate untouched.
        "ALSA" => format!(
            "ALSA is driving the card directly and converts nothing; start \
             PipeWire, or use a device with a {rate} Hz mode"
        ),
        _ => format!("run that device at {rate} Hz"),
    }
}

/// The session rate and the host, which every refusal message needs.
#[derive(Clone, Copy)]
struct RateContext<'a> {
    rate: u32,
    host: &'a str,
}

impl RateContext<'_> {
    /// This device will not run at the session rate. `refusal` is the host's
    /// own error when the open was attempted, and None when the device never
    /// advertised the rate and this host cannot be trusted to try.
    ///
    /// A capture endpoint at a telephony rate, or on a Bluetooth or headset
    /// form factor, gets its own remedy: its hands-free mode has no 48 kHz
    /// setting anywhere, so pointing at the host's rate settings would send
    /// the user hunting for an entry that does not exist (#330).
    fn refused(
        self,
        direction: Direction,
        native: &cpal::SupportedStreamConfig,
        form: FormFactor,
        refusal: Option<&AudioError>,
    ) -> AudioError {
        let side = match direction {
            Direction::Capture => "capture",
            Direction::Playback => "playback",
        };
        let rate = self.rate;
        // detail(), not Display: this string becomes another Unsupported,
        // whose Display supplies the one prefix the sentence gets.
        let detail = match refusal {
            Some(err) => format!(" ({})", err.detail()),
            None => String::new(),
        };
        let telephony_mic = direction == Direction::Capture
            && (is_telephony_rate(native.sample_rate())
                || matches!(form, FormFactor::Bluetooth | FormFactor::Headset));
        let remedy = if telephony_mic {
            format!(
                "that is a Bluetooth or headset microphone with no {rate} Hz \
                 mode, so use another capture device"
            )
        } else {
            rate_remedy(self.host, rate)
        };
        AudioError::Unsupported(format!(
            "{side} device runs at {} Hz and will not open at {rate} Hz{detail}; {remedy}",
            native.sample_rate(),
        ))
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
        //
        // This is also the second half of the rate guarantee. PipeWire
        // negotiates asynchronously, so a stream that comes back at some rate
        // other than the one asked for is reported here as UnsupportedConfig
        // rather than by `build_*_stream`, and latching on it is what stops
        // that stream from playing on at the wrong pitch.
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
    open: &cpal::SupportedStreamConfig,
    config: &StreamConfig,
    mut on_capture: CaptureFn,
    errored: &Arc<AtomicBool>,
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
            make_error_callback(errored),
            None,
        )
        .map_err(|e| map_err(&e))
}

fn build_output(
    device: &cpal::Device,
    open: &cpal::SupportedStreamConfig,
    config: &StreamConfig,
    mut on_playback: PlaybackFn,
    errored: &Arc<AtomicBool>,
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
    buffer_frames: Option<u32>,
}

impl StreamHandle for CpalStreamHandle {
    fn latency_frames(&self) -> Option<u32> {
        self.latency_frames
    }

    fn buffer_frames(&self) -> Option<u32> {
        self.buffer_frames
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

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::{
        SampleFormat, SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
    };

    const BUF: SupportedBufferSize = SupportedBufferSize::Range { min: 64, max: 4096 };

    fn native(rate: u32, channels: u16) -> SupportedStreamConfig {
        SupportedStreamConfig::new(channels, rate, BUF, SampleFormat::F32)
    }

    fn range(lo: u32, hi: u32, channels: u16, format: SampleFormat) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(channels, lo, hi, BUF, format)
    }

    #[test]
    fn a_device_already_at_the_session_rate_opens_as_it_is() {
        let native = native(48_000, 2);
        let chosen = config_at_rate(&native, std::iter::empty(), 48_000).expect("native rate");
        assert_eq!(chosen.sample_rate(), 48_000);
        assert_eq!(chosen.channels(), 2);
    }

    #[test]
    fn a_44_1_device_that_also_does_48_opens_at_48() {
        let native = native(44_100, 2);
        let ranges = [
            range(44_100, 44_100, 2, SampleFormat::F32),
            range(44_100, 96_000, 4, SampleFormat::I16),
            range(44_100, 96_000, 2, SampleFormat::F32),
        ];
        let chosen =
            config_at_rate(&native, ranges.into_iter(), 48_000).expect("48 kHz is in range");
        assert_eq!(chosen.sample_rate(), 48_000);
        // f32 and the native layout win over the wider i16 range.
        assert_eq!(chosen.sample_format(), SampleFormat::F32);
        assert_eq!(chosen.channels(), 2);
    }

    /// The bug this guards: a 44.1 kHz-only device used to be opened at 48 kHz
    /// anyway, which plays the session sharp and fast instead of failing.
    #[test]
    fn a_44_1_only_device_is_refused_rather_than_pitch_shifted() {
        let native = native(44_100, 2);
        let ranges = [
            range(8_000, 44_100, 2, SampleFormat::F32),
            range(22_050, 44_100, 1, SampleFormat::I16),
        ];
        assert!(config_at_rate(&native, ranges.into_iter(), 48_000).is_none());
        let err = ctx("ALSA").refused(Direction::Capture, &native, FormFactor::Unknown, None);
        let AudioError::Unsupported(msg) = err else {
            panic!("expected Unsupported, got {err:?}");
        };
        assert!(msg.contains("44100") && msg.contains("48000"), "{msg}");
    }

    #[test]
    fn an_i16_only_device_at_the_session_rate_still_opens() {
        let native = native(44_100, 2);
        let ranges = [range(44_100, 48_000, 2, SampleFormat::I16)];
        let chosen = config_at_rate(&native, ranges.into_iter(), 48_000).expect("rate is in range");
        assert_eq!(chosen.sample_rate(), 48_000);
        assert_eq!(chosen.sample_format(), SampleFormat::I16);
    }

    fn ctx(host: &str) -> RateContext<'_> {
        RateContext { rate: 48_000, host }
    }

    /// The regression this whole path exists for: a PipeWire graph at 44.1 kHz
    /// advertises 44100 and nothing else, and used to be refused, even though
    /// PipeWire would have carried a 48 kHz client stream correctly.
    #[test]
    fn a_graph_that_advertises_only_44_1_is_attempted_on_a_converting_host() {
        let native = native(44_100, 2);
        let ranges = [range(44_100, 44_100, 2, SampleFormat::F32)];
        let (open, attempted) = plan_open(&native, ranges.into_iter(), 48_000, true)
            .expect("a converting host is asked, not pre-refused");
        assert!(attempted);
        assert_eq!(open.sample_rate(), 48_000);
        assert_eq!(open.channels(), 2);
    }

    /// And the other half: on a host that would run the card at 44.1 without
    /// saying so, there is nothing to attempt.
    #[test]
    fn the_same_device_is_still_refused_on_a_host_that_converts_nothing() {
        let native = native(44_100, 2);
        let ranges = [range(44_100, 44_100, 2, SampleFormat::F32)];
        assert!(plan_open(&native, ranges.into_iter(), 48_000, false).is_none());
    }

    #[test]
    fn an_advertised_rate_is_never_an_attempt() {
        let native = native(44_100, 2);
        let ranges = [range(44_100, 96_000, 2, SampleFormat::F32)];
        let (open, attempted) =
            plan_open(&native, ranges.into_iter(), 48_000, true).expect("48 kHz is advertised");
        assert!(!attempted);
        assert_eq!(open.sample_rate(), 48_000);
    }

    #[test]
    fn alsa_is_not_trusted_to_report_the_rate_it_gave_us() {
        assert_eq!(rate_policy("ALSA"), Some(false));
        assert!(!verifies_negotiated_rate("ALSA"));
    }

    #[test]
    fn the_hosts_that_report_a_rate_they_could_not_honour() {
        for host in ["PipeWire", "CoreAudio", "WASAPI"] {
            assert_eq!(rate_policy(host), Some(true), "{host}");
            assert!(verifies_negotiated_rate(host), "{host}");
        }
    }

    /// An unclassified host keeps the conservative behaviour, because being
    /// wrong the other way is a session that plays sharp and never says why.
    #[test]
    fn an_unknown_host_is_not_attempted() {
        assert_eq!(rate_policy("Frobnicator"), None);
        assert!(!verifies_negotiated_rate("Frobnicator"));
    }

    /// The table above is keyed on cpal's own host names, so a rename in cpal
    /// would silently drop every host back to the conservative branch. Needs
    /// no device: constructing a host does not open one.
    #[test]
    fn the_default_host_is_one_the_table_knows() {
        let name = cpal::default_host().id().name();
        assert!(
            rate_policy(name).is_some(),
            "cpal's default host is {name:?}, which the rate table does not classify"
        );
    }

    /// An attempt that the host then refuses has to read as a rate refusal,
    /// not as whatever the host called it. The composed message carries the
    /// inner error's text without its variant prefix, so the sentence a
    /// musician reads says "unsupported audio configuration:" exactly once.
    #[test]
    fn a_refused_attempt_names_the_rates_and_carries_the_host_error() {
        let native = native(44_100, 2);
        let refusal = AudioError::Unsupported("ASBD not supported".into());
        let err = ctx("CoreAudio").refused(
            Direction::Capture,
            &native,
            FormFactor::Unknown,
            Some(&refusal),
        );
        let full = err.to_string();
        assert_eq!(
            full.matches("unsupported audio configuration:").count(),
            1,
            "{full}"
        );
        let AudioError::Unsupported(msg) = err else {
            panic!("expected Unsupported");
        };
        assert!(msg.starts_with("capture device runs at 44100 Hz"), "{msg}");
        assert!(msg.contains("48000 Hz"), "{msg}");
        assert!(msg.contains("(ASBD not supported)"), "{msg}");
        assert!(msg.contains("Audio MIDI Setup"), "{msg}");
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

    /// The remedy is the whole feature, so each host gets the one that is true
    /// for it. PipeWire's is the one that matters: pointing a musician at
    /// default.clock.rate is pointing them at a setting that will not help.
    #[test]
    fn each_host_gets_a_remedy_that_is_true_for_it() {
        let windows = rate_remedy("WASAPI", 48_000);
        // mmsys.cpl is the load-bearing part: Windows 11's Settings app has
        // no Recording or Playback tab to send anyone to.
        assert!(windows.contains("mmsys.cpl"), "{windows}");
        assert!(windows.contains("More sound settings"), "{windows}");
        assert!(windows.contains("Advanced, Default Format"), "{windows}");

        let macos = rate_remedy("CoreAudio", 48_000);
        assert!(macos.contains("Audio MIDI Setup, Format"), "{macos}");

        let pipewire = rate_remedy("PipeWire", 48_000);
        assert!(pipewire.contains("converts sample rates"), "{pipewire}");
        assert!(!pipewire.contains("clock.rate"), "{pipewire}");
        assert!(!pipewire.contains("restart"), "{pipewire}");

        let alsa = rate_remedy("ALSA", 48_000);
        assert!(alsa.contains("PipeWire"), "{alsa}");

        for host in ["WASAPI", "CoreAudio", "PipeWire", "ALSA", "Frobnicator"] {
            assert!(rate_remedy(host, 48_000).contains("48000"), "{host}");
        }
    }

    /// A Bluetooth hands-free capture endpoint has no 48000 entry on any
    /// settings page, so the remedy must say to use another microphone rather
    /// than send the user hunting for one. Both signals trigger it: a
    /// telephony native rate even when the form factor did not decode, and a
    /// Bluetooth or headset form factor even at a non-telephony rate (Windows
    /// swaps a headset between profiles underneath a running session).
    #[test]
    fn a_telephony_or_bluetooth_mic_is_told_to_use_another_device() {
        let cases = [
            (16_000, FormFactor::Unknown),
            (8_000, FormFactor::Unknown),
            (16_000, FormFactor::Bluetooth),
            (44_100, FormFactor::Bluetooth),
            (44_100, FormFactor::Headset),
        ];
        for (rate, form) in cases {
            let native = native(rate, 1);
            let err = ctx("WASAPI").refused(Direction::Capture, &native, form, None);
            let AudioError::Unsupported(msg) = err else {
                panic!("expected Unsupported");
            };
            assert!(
                msg.contains("Bluetooth or headset microphone"),
                "{rate} Hz {form:?}: {msg}"
            );
            assert!(
                msg.contains("use another capture device"),
                "{rate} Hz {form:?}: {msg}"
            );
            assert!(
                !msg.contains("Default Format"),
                "no pointer at a settings entry that does not exist: {msg}"
            );
        }
    }

    /// The special remedy stays capture-only and telephony-only: a 44.1 kHz
    /// interface still gets the host's settings walk, and Bluetooth speakers
    /// refusing playback are not a hands-free microphone problem.
    #[test]
    fn other_refusals_keep_the_host_remedy() {
        let interface = native(44_100, 2);
        let err = ctx("WASAPI").refused(Direction::Capture, &interface, FormFactor::Unknown, None);
        assert!(err.detail().contains("Default Format"), "{err}");

        let bt_speakers = native(44_100, 2);
        let err = ctx("WASAPI").refused(
            Direction::Playback,
            &bt_speakers,
            FormFactor::Bluetooth,
            None,
        );
        assert!(err.detail().contains("Default Format"), "{err}");
        assert!(!err.detail().contains("capture device"), "{err}");
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

    /// The disclosure table: a rate mismatch means OS conversion only on the
    /// hosts that convert. CoreAudio moved the device clock, ALSA never opened
    /// a rate the card does not run, and an unknown host earns no claim.
    #[test]
    fn render_conversion_is_claimed_only_where_the_os_converts() {
        for host in ["WASAPI", "PipeWire"] {
            assert_eq!(
                render_conversion(host, 44_100, 48_000),
                Some(true),
                "{host}"
            );
            assert_eq!(
                render_conversion(host, 48_000, 48_000),
                Some(false),
                "{host}"
            );
        }
        for host in ["CoreAudio", "ALSA"] {
            assert_eq!(
                render_conversion(host, 44_100, 48_000),
                Some(false),
                "{host}"
            );
            assert_eq!(
                render_conversion(host, 48_000, 48_000),
                Some(false),
                "{host}"
            );
        }
        assert_eq!(render_conversion("Frobnicator", 44_100, 48_000), None);
    }

    #[test]
    fn telephony_rates_are_the_hands_free_profiles_only() {
        assert!(is_telephony_rate(8_000));
        assert!(is_telephony_rate(16_000));
        for rate in [22_050, 44_100, 48_000, 96_000] {
            assert!(!is_telephony_rate(rate), "{rate}");
        }
    }
}
