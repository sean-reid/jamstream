//! The sample-rate ladder for the cpal backend: which rung a direction
//! opens on, which hosts can be asked for a rate a device never advertised,
//! what a mismatch between the device rate and the opened rate means on each
//! host, and what to tell someone holding a device no rung can carry.
//!
//! None of this opens a device or calls a cpal host, so all of it is
//! unit-testable anywhere, which is the point: `cpal_backend` needs a real
//! endpoint to exercise and the ladder does not, while the ladder is the half
//! with the decisions in it. `wasapi_policy` splits off `wasapi_backend` for
//! the same reason.

use crate::rate::RateOutcome;
use crate::types::{AudioError, Direction, FormFactor, Result};

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
pub(crate) const fn rate_policy(host: &str) -> Option<bool> {
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

pub(crate) fn verifies_negotiated_rate(host: &str) -> bool {
    rate_policy(host).unwrap_or(false)
}

/// The rung one direction's open landed on, from what actually happened.
///
/// The converter's presence decides rung 3. Otherwise a native rate other
/// than the session's means the host bridged the difference, and which way
/// is a property of the host, because the same mismatch means different
/// things:
///
/// - CoreAudio sets the device's nominal rate to the stream's inside
///   `build_*_stream`, so the whole device clock moved: rung 2.
/// - WASAPI opens output with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, so the
///   engine keeps its mix rate and converts our stream into it; PipeWire
///   does the same between its graph rate and a client stream.
/// - On ALSA and unknown hosts a stream only ever opens at a rate the card
///   advertised, whatever the default config reads, which is rung 1.
pub(crate) fn rate_outcome(
    rates: RateContext,
    native_rate: u32,
    opened_rate: u32,
    resample_added_ms: Option<f32>,
) -> RateOutcome {
    if let Some(added_ms) = resample_added_ms {
        return RateOutcome::Resampled {
            device: opened_rate,
            added_ms,
        };
    }
    if native_rate == rates.rate {
        return RateOutcome::Native;
    }
    match rates.host {
        "CoreAudio" => RateOutcome::ClockSet { from: native_rate },
        "WASAPI" | "PipeWire" => RateOutcome::OsConverted {
            device: native_rate,
        },
        _ => RateOutcome::Native,
    }
}

/// How one direction opens, decided by [`plan_direction`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectionPlan {
    /// The config to build the stream with.
    pub(crate) open: cpal::SupportedStreamConfig,
    /// The config was never advertised; the host is being asked to honour it,
    /// and a refusal falls to the converter at the device's own rate.
    pub(crate) attempted: bool,
    /// Open at the device's own rate with the boundary converter wrapped
    /// around this direction's handler half, rung 3 of the ladder.
    pub(crate) convert: bool,
}

/// The sample-rate ladder for one direction. A device that advertises the
/// session rate opens at it (on CoreAudio that open moves the device clock;
/// rung 2 lives inside cpal). One that does not is attempted anyway on a host
/// that [reports a rate it could not honour](verifies_negotiated_rate), and
/// opens at its own rate through the converter everywhere else. The refusal
/// survives only for a Bluetooth hands-free microphone: converting an 8 or
/// 16 kHz voice-profile capture would carry the session in telephone
/// quality, and the honest answer there is another microphone.
///
/// `demoted` short-circuits the whole ladder to the converter while the
/// device runs away from the session rate: its clock was set once, another
/// app took it back, and asking again is the fight the demotion exists to
/// end. A demoted device found back at the session rate opens natively;
/// there is no contest left to lose.
pub(crate) fn plan_direction(
    rates: RateContext,
    direction: Direction,
    native: &cpal::SupportedStreamConfig,
    supported: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    host_converts: bool,
    form: FormFactor,
    demoted: bool,
) -> Result<DirectionPlan> {
    if demoted && native.sample_rate() != rates.rate {
        return Ok(DirectionPlan {
            open: *native,
            attempted: false,
            convert: true,
        });
    }
    if let Some(open) = config_at_rate(native, supported, rates.rate) {
        return Ok(DirectionPlan {
            open,
            attempted: false,
            convert: false,
        });
    }
    if telephony_mic(direction, native.sample_rate(), form) {
        return Err(rates.refused(direction, native, form, None));
    }
    if host_converts {
        let open = cpal::SupportedStreamConfig::new(
            native.channels(),
            rates.rate,
            *native.buffer_size(),
            native.sample_format(),
        );
        return Ok(DirectionPlan {
            open,
            attempted: true,
            convert: false,
        });
    }
    Ok(DirectionPlan {
        open: *native,
        attempted: false,
        convert: true,
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

/// Native rates that mark a telephony endpoint: the Bluetooth hands-free
/// profile and its wideband variant. A capture device at one of these has no
/// 48 kHz mode to switch to, whatever its settings pages suggest.
const fn is_telephony_rate(rate: u32) -> bool {
    matches!(rate, 8_000 | 16_000)
}

/// A capture endpoint in a hands-free voice profile: a telephony native rate,
/// or a Bluetooth or headset form factor. The one class of device the ladder
/// still refuses, because no rung helps it; both signals trigger it, since
/// Windows swaps a headset between profiles underneath a running session.
fn telephony_mic(direction: Direction, native_rate: u32, form: FormFactor) -> bool {
    direction == Direction::Capture
        && (is_telephony_rate(native_rate)
            || matches!(form, FormFactor::Bluetooth | FormFactor::Headset))
}

/// What the person at the keyboard can do about a device that will not open
/// at any rate the ladder can carry. Rare, because the converter takes the
/// rates the ladder cannot: this sentence is read only when the native-rate
/// open itself failed. Per host rather than per platform, because the two Linux
/// hosts fail for opposite reasons.
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
pub(crate) struct RateContext<'a> {
    pub(crate) rate: u32,
    pub(crate) host: &'a str,
}

impl RateContext<'_> {
    /// This device will not carry the session at all. `refusal` is the
    /// host's own error when an open was attempted, and None for the one
    /// refusal decided without asking: the hands-free microphone.
    ///
    /// A capture endpoint at a telephony rate, or on a Bluetooth or headset
    /// form factor, gets its own remedy: its hands-free mode has no 48 kHz
    /// setting anywhere, so pointing at the host's rate settings would send
    /// the user hunting for an entry that does not exist.
    pub(crate) fn refused(
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
        let remedy = if telephony_mic(direction, native.sample_rate(), form) {
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

/// The config shapes the ladder is exercised on, shared with the backend's
/// own tests so the two modules describe the same devices.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::RateContext;
    use cpal::{
        SampleFormat, SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
    };

    const BUF: SupportedBufferSize = SupportedBufferSize::Range { min: 64, max: 4096 };

    pub(crate) fn native(rate: u32, channels: u16) -> SupportedStreamConfig {
        SupportedStreamConfig::new(channels, rate, BUF, SampleFormat::F32)
    }

    pub(crate) fn range(
        lo: u32,
        hi: u32,
        channels: u16,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(channels, lo, hi, BUF, format)
    }

    pub(crate) fn ctx(host: &str) -> RateContext<'_> {
        RateContext { rate: 48_000, host }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{ctx, native, range};
    use super::*;
    use cpal::SampleFormat;

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

    /// The sample-rate ladder on a host that cannot be trusted to convert: a
    /// 44.1 kHz-only device opens at its own rate through the boundary
    /// converter rather than being refused. Opening it at 48 kHz anyway
    /// would play the session sharp and fast, and refusing it instead gives
    /// a musician nothing to act on.
    #[test]
    fn a_44_1_only_device_converts_rather_than_being_refused() {
        let native = native(44_100, 2);
        let ranges = [
            range(8_000, 44_100, 2, SampleFormat::F32),
            range(22_050, 44_100, 1, SampleFormat::I16),
        ];
        assert!(config_at_rate(&native, ranges.into_iter(), 48_000).is_none());
        let plan = plan_direction(
            ctx("ALSA"),
            Direction::Capture,
            &native,
            ranges.into_iter(),
            false,
            FormFactor::Unknown,
            false,
        )
        .expect("rung 3 takes what used to be refused");
        assert!(plan.convert);
        assert!(!plan.attempted);
        assert_eq!(
            plan.open.sample_rate(),
            44_100,
            "the device keeps its clock"
        );
    }

    #[test]
    fn an_i16_only_device_at_the_session_rate_still_opens() {
        let native = native(44_100, 2);
        let ranges = [range(44_100, 48_000, 2, SampleFormat::I16)];
        let chosen = config_at_rate(&native, ranges.into_iter(), 48_000).expect("rate is in range");
        assert_eq!(chosen.sample_rate(), 48_000);
        assert_eq!(chosen.sample_format(), SampleFormat::I16);
    }

    /// A PipeWire graph at 44.1 kHz advertises 44100 and nothing else, and must
    /// still be attempted: PipeWire carries a 48 kHz client stream correctly.
    #[test]
    fn a_graph_that_advertises_only_44_1_is_attempted_on_a_converting_host() {
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
        .expect("a converting host is asked, not pre-refused");
        assert!(plan.attempted);
        assert!(!plan.convert);
        assert_eq!(plan.open.sample_rate(), 48_000);
        assert_eq!(plan.open.channels(), 2);
    }

    /// And the other half: on a host that would run the card at 44.1 without
    /// saying so, the attempt is skipped and the converter takes it directly.
    #[test]
    fn the_same_device_converts_on_a_host_that_converts_nothing() {
        let native = native(44_100, 2);
        let ranges = [range(44_100, 44_100, 2, SampleFormat::F32)];
        let plan = plan_direction(
            ctx("ALSA"),
            Direction::Playback,
            &native,
            ranges.into_iter(),
            false,
            FormFactor::Unknown,
            false,
        )
        .expect("rung 3 is unconditional");
        assert!(plan.convert);
        assert_eq!(plan.open.sample_rate(), 44_100);
    }

    #[test]
    fn an_advertised_rate_is_never_an_attempt() {
        let native = native(44_100, 2);
        let ranges = [range(44_100, 96_000, 2, SampleFormat::F32)];
        let plan = plan_direction(
            ctx("CoreAudio"),
            Direction::Capture,
            &native,
            ranges.into_iter(),
            true,
            FormFactor::Unknown,
            false,
        )
        .expect("48 kHz is advertised");
        assert!(!plan.attempted);
        assert!(!plan.convert);
        assert_eq!(plan.open.sample_rate(), 48_000);
    }

    /// The one refusal the ladder keeps: a hands-free microphone has no rate
    /// worth carrying, so no rung is offered and the only remedy is another
    /// microphone. Both signals refuse, on converting and non-converting
    /// hosts alike, while Bluetooth playback still converts: the profile
    /// problem is capture's.
    #[test]
    fn a_hands_free_microphone_is_refused_not_converted() {
        for (rate, form, host_converts) in [
            (16_000, FormFactor::Unknown, true),
            (8_000, FormFactor::Unknown, false),
            (44_100, FormFactor::Bluetooth, true),
            (44_100, FormFactor::Headset, false),
        ] {
            let native = native(rate, 1);
            let ranges = [range(rate, rate, 1, SampleFormat::F32)];
            let err = plan_direction(
                ctx("WASAPI"),
                Direction::Capture,
                &native,
                ranges.into_iter(),
                host_converts,
                form,
                false,
            )
            .expect_err("a hands-free mic earns no rung");
            assert!(
                err.detail().contains("use another capture device"),
                "{rate} Hz {form:?}: {err}"
            );
        }
        let native = native(44_100, 2);
        let ranges = [range(44_100, 44_100, 2, SampleFormat::F32)];
        let plan = plan_direction(
            ctx("ALSA"),
            Direction::Playback,
            &native,
            ranges.into_iter(),
            false,
            FormFactor::Bluetooth,
            false,
        )
        .expect("Bluetooth speakers convert like any playback device");
        assert!(plan.convert);
    }

    /// The ladder's contested-clock rule: a device whose clock this app
    /// set and then lost the stream on is never asked again. While it runs
    /// away from the session rate the whole ladder short-circuits to the
    /// converter, even though 48 kHz is advertised; found back at the
    /// session rate it opens natively, because there is no contest left.
    #[test]
    fn a_demoted_device_converts_instead_of_reclaiming_the_clock() {
        let snapped_back = native(44_100, 2);
        let ranges = [range(44_100, 96_000, 2, SampleFormat::F32)];
        let plan = plan_direction(
            ctx("CoreAudio"),
            Direction::Capture,
            &snapped_back,
            ranges.into_iter(),
            true,
            FormFactor::Unknown,
            true,
        )
        .expect("demotion is not a refusal");
        assert!(plan.convert);
        assert!(!plan.attempted);
        assert_eq!(plan.open.sample_rate(), 44_100, "no clock is touched");

        let at_rate = native(48_000, 2);
        let plan = plan_direction(
            ctx("CoreAudio"),
            Direction::Capture,
            &at_rate,
            ranges.into_iter(),
            true,
            FormFactor::Unknown,
            true,
        )
        .expect("a device back at the session rate opens as it is");
        assert!(!plan.convert);
        assert!(!plan.attempted);
        assert_eq!(plan.open.sample_rate(), 48_000);
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

    /// The disclosure table: what a 44.1 kHz native rate means once the
    /// stream opened at 48 kHz is a property of the host. CoreAudio moved
    /// the whole device clock, WASAPI and PipeWire converted in the OS, and
    /// on ALSA or an unknown host a stream only opens at a rate the card
    /// advertised, so the mismatch in the default config is not a claim.
    /// A converter on the stream is rung 3 wherever it runs.
    #[test]
    fn rate_outcomes_follow_the_host_that_bridged_the_rate() {
        assert_eq!(
            rate_outcome(ctx("CoreAudio"), 44_100, 48_000, None),
            RateOutcome::ClockSet { from: 44_100 }
        );
        for host in ["WASAPI", "PipeWire"] {
            assert_eq!(
                rate_outcome(ctx(host), 44_100, 48_000, None),
                RateOutcome::OsConverted { device: 44_100 },
                "{host}"
            );
        }
        for host in ["ALSA", "Frobnicator"] {
            assert_eq!(
                rate_outcome(ctx(host), 44_100, 48_000, None),
                RateOutcome::Native,
                "{host}"
            );
        }
        for host in ["CoreAudio", "WASAPI", "PipeWire", "ALSA"] {
            assert_eq!(
                rate_outcome(ctx(host), 48_000, 48_000, None),
                RateOutcome::Native,
                "{host}: native is not news"
            );
            assert_eq!(
                rate_outcome(ctx(host), 44_100, 44_100, Some(3.2)),
                RateOutcome::Resampled {
                    device: 44_100,
                    added_ms: 3.2
                },
                "{host}: the converter is rung 3 everywhere"
            );
        }
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
