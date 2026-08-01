//! Real devices via cpal: CoreAudio on macOS, WASAPI shared mode on
//! Windows, PipeWire/ALSA on Linux. On Windows this is the fallback half of
//! `WindowsBackend`, which prefers the direct WASAPI exclusive-mode path; see
//! `backend()` in lib.rs.
//!
//! Every stream runs at the session rate or not at all, because jamstream
//! never resamples. Which of those two happens is decided in one place,
//! [`plan_open`], and it turns on whether the host reports a stream it could
//! not open at the rate asked for: see [`verifies_negotiated_rate`].

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
        let rates = RateContext {
            rate: config.sample_rate,
            host: self.host.id().name(),
        };
        let converts = verifies_negotiated_rate(rates.host);
        let in_device = self.find_device(capture, Direction::Capture)?;
        let out_device = self.find_device(playback, Direction::Playback)?;
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
        .ok_or_else(|| rates.refused(Direction::Capture, &in_native, None))?;
        let (out_open, out_attempted) = plan_open(
            &out_native,
            out_device
                .supported_output_configs()
                .map_err(|e| map_err(&e))?,
            config.sample_rate,
            converts,
        )
        .ok_or_else(|| rates.refused(Direction::Playback, &out_native, None))?;

        let (on_capture, on_playback) = handler.into_parts();
        let errored = Arc::new(AtomicBool::new(false));

        let input =
            build_input(&in_device, &in_open, &config, on_capture, &errored).map_err(|e| {
                if in_attempted {
                    rates.refused(Direction::Capture, &in_native, Some(&e))
                } else {
                    e
                }
            })?;
        let output =
            build_output(&out_device, &out_open, &config, on_playback, &errored).map_err(|e| {
                if out_attempted {
                    rates.refused(Direction::Playback, &out_native, Some(&e))
                } else {
                    e
                }
            })?;

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
    fn refused(
        self,
        direction: Direction,
        native: &cpal::SupportedStreamConfig,
        refusal: Option<&AudioError>,
    ) -> AudioError {
        let side = match direction {
            Direction::Capture => "capture",
            Direction::Playback => "playback",
        };
        let rate = self.rate;
        let detail = match refusal {
            Some(err) => format!(" ({err})"),
            None => String::new(),
        };
        AudioError::Unsupported(format!(
            "{side} device runs at {} Hz and will not open at {rate} Hz{detail}; {}",
            native.sample_rate(),
            rate_remedy(self.host, rate)
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
        let err = ctx("ALSA").refused(Direction::Capture, &native, None);
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
    /// not as whatever the host called it.
    #[test]
    fn a_refused_attempt_names_the_rates_and_carries_the_host_error() {
        let native = native(44_100, 2);
        let refusal = AudioError::Unsupported("ASBD not supported".into());
        let AudioError::Unsupported(msg) =
            ctx("CoreAudio").refused(Direction::Capture, &native, Some(&refusal))
        else {
            panic!("expected Unsupported");
        };
        assert!(msg.starts_with("capture device runs at 44100 Hz"), "{msg}");
        assert!(msg.contains("48000 Hz"), "{msg}");
        assert!(msg.contains("ASBD not supported"), "{msg}");
        assert!(msg.contains("Audio MIDI Setup"), "{msg}");
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
}
