//! WavBackend: deterministic offline pump against generated fixtures.
//!
//! The passthrough handler below echoes captured samples into playback via a
//! shared queue. WavStream::pump runs capture before playback within one
//! call, so the capture file matches the input WAV at zero offset.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use jamstream_audio_io::{
    AudioError, CallbackBridge, DuplexHandler, StreamConfig, StreamHandle, WavBackend,
};

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("jamstream-audio-io-{}-{name}", std::process::id()))
}

fn config(channels: u16) -> StreamConfig {
    StreamConfig {
        sample_rate: 48_000,
        buffer_frames: 240,
        channels,
        ..StreamConfig::default()
    }
}

/// Float WAV with a per-channel sine so channels are distinguishable:
/// channel 0 carries 440 Hz, channel 1 carries 880, at half scale.
fn write_sine(path: &PathBuf, channels: u16, frames: usize, rate: u32) -> Vec<f32> {
    let spec = hound::WavSpec {
        channels,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    let mut samples = Vec::with_capacity(frames * usize::from(channels));
    for i in 0..frames {
        for ch in 0..channels {
            let hz = 440.0 * f32::from(ch + 1);
            let s = 0.5 * (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin();
            writer.write_sample(s).unwrap();
            samples.push(s);
        }
    }
    writer.finalize().unwrap();
    samples
}

fn read_all(path: &PathBuf) -> (hound::WavSpec, Vec<f32>) {
    let mut reader = hound::WavReader::open(path).unwrap();
    let spec = reader.spec();
    let samples = reader.samples::<f32>().map(|s| s.unwrap()).collect();
    (spec, samples)
}

fn passthrough() -> DuplexHandler {
    let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
    let capture_q = Arc::clone(&queue);
    DuplexHandler::new(
        move |samples: &[f32]| capture_q.lock().unwrap().extend(samples.iter().copied()),
        move |out: &mut [f32]| {
            let mut q = queue.lock().unwrap();
            for slot in out.iter_mut() {
                *slot = q.pop_front().unwrap_or(0.0);
            }
        },
    )
}

/// Pump in odd chunks and compare the capture file to the input WAV.
fn passthrough_case(name: &str, wav_channels: u16, stream_channels: u16) {
    let input_path = temp_path(&format!("{name}-in.wav"));
    let output_path = temp_path(&format!("{name}-out.wav"));
    let frames = 480;
    let input = write_sine(&input_path, wav_channels, frames, 48_000);

    let backend = WavBackend::new(Some(input_path.clone()), Some(output_path.clone()));
    let mut stream = backend
        .open_offline(config(stream_channels), passthrough())
        .unwrap();

    // Odd sizes summing to exactly the input length.
    for chunk in [7usize, 13, 31, 64, 111, 254] {
        stream.pump(chunk).unwrap();
    }
    assert!(!stream.exhausted(), "input fully consumed but not overrun");
    // One pump past the end: capture side is silence and exhaustion latches.
    stream.pump(16).unwrap();
    assert!(stream.exhausted());
    stream.finish().unwrap();

    // Expected capture-side samples in the stream's channel layout.
    let src = usize::from(wav_channels);
    let dst = usize::from(stream_channels);
    let expected: Vec<f32> = input
        .chunks_exact(src)
        .flat_map(|frame| (0..dst).map(|ch| frame[ch.min(src - 1)]))
        .collect();

    let (spec, written) = read_all(&output_path);
    assert_eq!(spec.channels, stream_channels);
    assert_eq!(spec.sample_rate, 48_000);
    assert_eq!(written.len(), (frames + 16) * dst);
    assert_eq!(
        &written[..expected.len()],
        &expected[..],
        "zero offset passthrough"
    );
    assert!(
        written[expected.len()..].iter().all(|&s| s == 0.0),
        "exhausted region must be silence"
    );

    let _ = std::fs::remove_file(&input_path);
    let _ = std::fs::remove_file(&output_path);
}

#[test]
fn mono_passthrough_odd_chunks() {
    passthrough_case("mono", 1, 1);
}

#[test]
fn stereo_passthrough_odd_chunks() {
    passthrough_case("stereo", 2, 2);
}

#[test]
fn mono_input_fans_out_to_stereo_stream() {
    passthrough_case("mono-up", 1, 2);
}

#[test]
fn stereo_input_downmixes_to_first_channel() {
    passthrough_case("stereo-down", 2, 1);
}

#[test]
fn no_input_file_is_silence_and_immediately_exhausted() {
    let output_path = temp_path("silent-out.wav");
    let backend = WavBackend::new(None, Some(output_path.clone()));
    let mut stream = backend.open_offline(config(1), passthrough()).unwrap();
    assert!(!stream.exhausted());
    stream.pump(37).unwrap();
    assert!(stream.exhausted());
    stream.finish().unwrap();

    let (_, written) = read_all(&output_path);
    assert_eq!(written.len(), 37);
    assert!(written.iter().all(|&s| s == 0.0));
    let _ = std::fs::remove_file(&output_path);
}

#[test]
fn int16_input_is_scaled() {
    let input_path = temp_path("i16-in.wav");
    let output_path = temp_path("i16-out.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&input_path, spec).unwrap();
    for v in [0i16, 16_384, -16_384, i16::MAX, i16::MIN] {
        writer.write_sample(v).unwrap();
    }
    writer.finalize().unwrap();

    let backend = WavBackend::new(Some(input_path.clone()), Some(output_path.clone()));
    let mut stream = backend.open_offline(config(1), passthrough()).unwrap();
    stream.pump(5).unwrap();
    stream.finish().unwrap();

    let (_, written) = read_all(&output_path);
    let expected = [0.0, 0.5, -0.5, 32_767.0 / 32_768.0, -1.0];
    assert_eq!(&written[..], &expected[..]);
    let _ = std::fs::remove_file(&input_path);
    let _ = std::fs::remove_file(&output_path);
}

/// WASAPI shared mode, modelled: the request says 120 frames, the device
/// calls back at its own 480-frame period, and the handle reports the period
/// the way cpal's `Stream::buffer_size` does on that host. Pumps accumulate
/// until a full period is owed, because a real device does not call back
/// mid-period either.
#[test]
fn a_device_period_overrides_the_request_and_batches_callbacks() {
    let sizes = Arc::new(Mutex::new(Vec::<usize>::new()));
    let capture_sizes = Arc::clone(&sizes);
    let handler = DuplexHandler::new(
        move |samples: &[f32]| capture_sizes.lock().unwrap().push(samples.len()),
        move |_out: &mut [f32]| {},
    );
    let backend = WavBackend::new(None, None).with_device_period(480);
    let cfg = StreamConfig {
        buffer_frames: 120,
        ..config(2)
    };
    let mut stream = backend.open_offline(cfg, handler).unwrap();
    assert_eq!(
        stream.buffer_frames(),
        Some(480),
        "the handle must report the period, not the request"
    );

    for _ in 0..3 {
        stream.pump(120).unwrap();
    }
    assert!(
        sizes.lock().unwrap().is_empty(),
        "three 120-frame pumps are short of a period; no callback yet"
    );
    stream.pump(120).unwrap();
    assert_eq!(
        sizes.lock().unwrap().as_slice(),
        &[480 * 2],
        "the fourth pump completes one period: one callback, 480 stereo frames"
    );
}

/// Without a period the fake delivers what it is pumped, which is a device
/// that honoured the request, so the handle reports the request.
#[test]
fn without_a_period_the_request_is_the_negotiated_size() {
    let backend = WavBackend::new(None, None);
    let stream = backend.open_offline(config(2), passthrough()).unwrap();
    assert_eq!(stream.buffer_frames(), Some(240));
}

/// The 44.1 kHz interface, modelled: the session runs at 48 kHz, the device
/// opens at its own rate, and the boundary converter carries the difference
/// (#347 rung 3) instead of the old refusal. The handle reports the callback
/// size in session-rate frames, so everything sized around callbacks keeps
/// one clock: 240 device frames per callback are ceil(240 * 160/147) = 262
/// handler-side frames.
#[test]
fn a_44_1_device_opens_a_48_khz_session_through_the_converter() {
    let backend = WavBackend::new(None, None).with_device_rate(44_100);
    let stream = backend.open_offline(config(2), passthrough()).unwrap();
    assert_eq!(stream.device_rate(), 44_100);
    assert_eq!(stream.buffer_frames(), Some(262));
    let (capture_ms, playback_ms) = stream
        .resample_added_ms()
        .expect("a converting stream reports its added latency");
    assert!(capture_ms > 0.0 && playback_ms > 0.0);
}

/// A device at the session rate converts nothing and says so, which is what
/// the disclosure surface reads to stay silent.
#[test]
fn a_48_k_device_reports_no_conversion_and_no_added_latency() {
    let backend = WavBackend::new(None, None);
    let stream = backend.open_offline(config(2), passthrough()).unwrap();
    assert_eq!(stream.device_rate(), 48_000);
    assert_eq!(stream.resample_added_ms(), None);
    assert_eq!(stream.buffer_frames(), Some(240), "no scaling at unity");
}

/// The Windows shape on a 44.1 endpoint: the device ignores the request and
/// calls back at its own 441-frame (10 ms) period, which is exactly 480
/// session-rate frames per callback on the handler side.
#[test]
fn a_44_1_device_period_is_reported_in_session_rate_frames() {
    let backend = WavBackend::new(None, None)
        .with_device_rate(44_100)
        .with_device_period(441);
    let cfg = StreamConfig {
        buffer_frames: 120,
        ..config(2)
    };
    let stream = backend.open_offline(cfg, passthrough()).unwrap();
    assert_eq!(stream.buffer_frames(), Some(480));
}

/// The refusal that remains after rung 3: a device the backend cannot feed
/// at the device's own rate. The session-rate mismatch converts; the input
/// fixture must still match the device clock, the way 44.1 tests need 44.1
/// fixtures.
#[test]
fn an_input_wav_must_match_the_device_rate_not_the_session_rate() {
    let input_path = temp_path("48k-on-44-1-in.wav");
    write_sine(&input_path, 1, 480, 48_000);
    let backend = WavBackend::new(Some(input_path.clone()), None).with_device_rate(44_100);
    let err = backend.open_offline(config(1), passthrough()).unwrap_err();
    let AudioError::Unsupported(msg) = err else {
        panic!("expected Unsupported, got {err:?}");
    };
    assert!(msg.contains("44100") && msg.contains("48000"), "{msg}");
    let _ = std::fs::remove_file(&input_path);
}

/// The offline backend reports a form factor like the real hosts do: Unknown
/// by default, which is what a host that cannot decode one reports, and the
/// configured shape on both endpoints when a test models a Bluetooth headset.
#[test]
fn devices_carry_the_modelled_form_factor() {
    use jamstream_audio_io::{AudioBackend, FormFactor};

    let plain = WavBackend::new(None, None);
    for device in plain.devices().unwrap() {
        assert_eq!(device.form_factor, FormFactor::Unknown, "{}", device.id);
    }

    let headset = WavBackend::new(None, None).with_form_factor(FormFactor::Bluetooth);
    let devices = headset.devices().unwrap();
    assert_eq!(devices.len(), 2);
    for device in devices {
        assert_eq!(device.form_factor, FormFactor::Bluetooth, "{}", device.id);
    }
}

#[test]
fn a_device_at_44_1_opens_at_its_own_rate() {
    let backend = WavBackend::new(None, None).with_device_rate(44_100);
    let cfg = StreamConfig {
        sample_rate: 44_100,
        ..config(2)
    };
    let mut stream = backend.open_offline(cfg, passthrough()).unwrap();
    stream.pump(441).unwrap();
    assert!(!stream.errored());
}

/// Every open publishes the render-conversion report the way a real backend
/// does, and no OS ever converts this device: a mismatched rate runs through
/// the crate's own boundary converter, which is a different disclosure. Safe
/// alongside parallel tests because every wav open publishes the same value.
#[test]
fn an_open_reports_that_nothing_is_converting() {
    let backend = WavBackend::new(None, None);
    let _stream = backend.open_offline(config(2), passthrough()).unwrap();
    assert_eq!(jamstream_audio_io::active_render_conversion(), Some(false));
}

/// Device loss is observable offline, so the caller's device-gone path is no
/// longer reachable only from real hardware.
#[test]
fn a_lost_device_is_reported_through_the_stream_handle() {
    let backend = WavBackend::new(None, None).with_device_loss_after(480);
    let mut stream = backend.open_offline(config(2), passthrough()).unwrap();
    stream.pump(240).unwrap();
    assert!(!stream.errored(), "the device is still there at 240 frames");
    stream.pump(240).unwrap();
    assert!(stream.errored(), "the device was pulled at 480 frames");
    // And it stays gone, the way a real invalidated stream does.
    stream.pump(240).unwrap();
    assert!(stream.errored());
}

/// The unplug is one event, not a property of every future stream: the
/// stream that answers the reopen models the replacement device, so a
/// caller's device-gone path can prove the session survives the swap.
#[test]
fn the_stream_after_the_unplug_keeps_running() {
    let backend = WavBackend::new(None, None).with_device_loss_after(480);
    let mut first = backend.open_offline(config(2), passthrough()).unwrap();
    first.pump(480).unwrap();
    assert!(first.errored(), "the first stream loses its device");
    let mut second = backend.open_offline(config(2), passthrough()).unwrap();
    second.pump(960).unwrap();
    assert!(!second.errored(), "the replacement device stays plugged in");
}

#[test]
fn a_device_loss_can_be_triggered_on_demand() {
    let backend = WavBackend::new(None, None);
    let mut stream = backend.open_offline(config(1), passthrough()).unwrap();
    stream.pump(64).unwrap();
    assert!(!stream.errored());
    stream.report_device_lost();
    assert!(stream.errored());
}

/// Stereo float WAV whose samples are their own indices, so any dropped or
/// padded sample breaks an exact equality somewhere downstream.
fn write_ramp(path: &PathBuf, frames: usize) -> Vec<f32> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    let samples: Vec<f32> = (0..frames * 2).map(|i| i as f32).collect();
    for &s in &samples {
        writer.write_sample(s).unwrap();
    }
    writer.finalize().unwrap();
    samples
}

/// Drives one pump of `frames` with the playout ring topped up first and the
/// capture ring drained after, the way the client's worker services the
/// bridge between device bites. `next_play` is the ramp value the playout
/// side continues from.
fn service_and_pump(
    engine: &mut jamstream_audio_io::EngineSide,
    stream: &mut jamstream_audio_io::WavStream,
    frames: usize,
    next_play: &mut f32,
    captured: &mut Vec<f32>,
    scratch: &mut [f32],
) {
    loop {
        let chunk: Vec<f32> = (0..240).map(|i| *next_play + i as f32).collect();
        let pushed = engine.push_playout(&chunk);
        *next_play += pushed as f32;
        if pushed < chunk.len() {
            break;
        }
    }
    stream.pump(frames).unwrap();
    let got = engine.pull_captured(scratch);
    captured.extend_from_slice(&scratch[..got]);
}

/// The whole shape of the ring defect, through the bridge: a device that
/// ignores the 120-frame request and calls back at a 480-frame period, over
/// a ring sized from the delivered callback with the client's 2x headroom.
/// The engine services 120-frame bites between callbacks; every captured
/// sample must survive and every rendered sample must be the one pushed,
/// with both counters still at zero.
#[test]
fn a_device_period_beyond_the_request_survives_a_ring_sized_for_it() {
    let input_path = temp_path("period-ring-in.wav");
    let output_path = temp_path("period-ring-out.wav");
    let input = write_ramp(&input_path, 4_800);

    // 2x the 480-frame callback, stereo: the client's sizing convention.
    let capacity = 2 * 480 * 2;
    let (device, mut engine) = CallbackBridge::new(capacity);
    let backend = WavBackend::new(Some(input_path.clone()), Some(output_path.clone()))
        .with_device_period(480);
    let cfg = StreamConfig {
        buffer_frames: 120,
        ..config(2)
    };
    let mut stream = backend.open_offline(cfg, device.into_handler()).unwrap();

    let mut captured = Vec::new();
    let mut next_play = 0.0f32;
    let mut scratch = vec![0.0f32; capacity];
    for _ in 0..40 {
        service_and_pump(
            &mut engine,
            &mut stream,
            120,
            &mut next_play,
            &mut captured,
            &mut scratch,
        );
    }
    stream.finish().unwrap();

    assert_eq!(engine.overruns(), 0, "capture must fit the sized ring");
    assert_eq!(engine.underruns(), 0, "render must never find the ring dry");
    // No dropped tail on capture: 40 pumps cover ten full periods, and every
    // one of those samples came through in order.
    assert_eq!(captured.len(), 10 * 480 * 2);
    assert_eq!(&captured[..], &input[..captured.len()]);
    // No padded silence on render: the file is the pushed ramp, contiguous.
    let (_, written) = read_all(&output_path);
    let expected: Vec<f32> = (0..written.len()).map(|i| i as f32).collect();
    assert_eq!(written.len(), 10 * 480 * 2);
    assert_eq!(written, expected);

    let _ = std::fs::remove_file(&input_path);
    let _ = std::fs::remove_file(&output_path);
}

/// The counter-case that was #323: the same device against a ring sized from
/// the request alone. The bridge counters move, which is what the client now
/// watches for; the fake would have caught the defect had anything looked.
#[test]
fn a_device_period_beyond_the_request_overwhelms_a_request_sized_ring() {
    let input_path = temp_path("period-starve-in.wav");
    let input = write_ramp(&input_path, 4_800);

    let capacity = 2 * 120 * 2;
    let (device, mut engine) = CallbackBridge::new(capacity);
    let backend = WavBackend::new(Some(input_path.clone()), None).with_device_period(480);
    let cfg = StreamConfig {
        buffer_frames: 120,
        ..config(2)
    };
    let mut stream = backend.open_offline(cfg, device.into_handler()).unwrap();

    let mut captured = Vec::new();
    let mut next_play = 0.0f32;
    let mut scratch = vec![0.0f32; capacity];
    for _ in 0..40 {
        service_and_pump(
            &mut engine,
            &mut stream,
            120,
            &mut next_play,
            &mut captured,
            &mut scratch,
        );
    }

    assert!(
        engine.overruns() > 0,
        "a 480-frame callback cannot fit a 240-frame ring"
    );
    assert!(
        engine.underruns() > 0,
        "a 480-frame pull drains a 240-frame ring and pads the rest"
    );
    assert!(
        captured.len() < 10 * 480 * 2,
        "the dropped capture tails must be visible in the total"
    );
    assert_ne!(
        &captured[..],
        &input[..captured.len()],
        "what survives is no longer contiguous"
    );

    let _ = std::fs::remove_file(&input_path);
}

/// Positive-going zero crossings, i.e. whole cycles of a sine.
fn cycles(samples: &[f32]) -> usize {
    samples
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count()
}

fn rms(samples: &[f32]) -> f64 {
    (samples
        .iter()
        .map(|&s| f64::from(s) * f64::from(s))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt()
}

/// Longest run of consecutive exact-zero samples: underrun padding writes
/// literal 0.0, running audio does not.
fn longest_zero_run(samples: &[f32]) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for &s in samples {
        run = if s == 0.0 { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    longest
}

/// Every other sample of an interleaved stereo signal: channel 0.
fn left(samples: &[f32]) -> Vec<f32> {
    samples.iter().copied().step_by(2).collect()
}

/// Rung 3 spanning the bridge, both directions at once, the way a session
/// runs: a 44.1 kHz device captures a 440 Hz sine into the 48 kHz ring while
/// its playback side renders a 440 Hz sine pushed at 48 kHz. The engine side
/// must see session-rate audio, sample-accurately: the count scaled by
/// exactly 160/147 and the pitch still 440 (an unconverted path reads 479
/// here). The capture file must hold device-rate audio at pitch and level
/// with no padded runs at steady state, and neither bridge counter may move.
fn bridge_conversion_case(name: &str, period: Option<u32>) {
    let input_path = temp_path(&format!("{name}-in.wav"));
    let output_path = temp_path(&format!("{name}-out.wav"));
    let device_frames_total = 88_200usize; // two seconds at 44.1 kHz
    write_sine(&input_path, 2, device_frames_total, 44_100);

    // The client's sizing convention: 2x the callback size the handle
    // reports, in session-rate frames, stereo.
    let callback_frames = period.unwrap_or(120);
    let session_callback = (callback_frames as usize * 48_000).div_ceil(44_100);
    let capacity = 2 * session_callback * 2;
    let (device, mut engine) = jamstream_audio_io::CallbackBridge::new(capacity);

    let mut backend = WavBackend::new(Some(input_path.clone()), Some(output_path.clone()))
        .with_device_rate(44_100);
    if let Some(p) = period {
        backend = backend.with_device_period(p);
    }
    let cfg = StreamConfig {
        buffer_frames: 120,
        ..config(2)
    };
    let mut stream = backend.open_offline(cfg, device.into_handler()).unwrap();
    assert_eq!(stream.buffer_frames(), Some(session_callback as u32));

    // Three seconds of 440 Hz at 48 kHz to render, more than the run needs.
    let play: Vec<f32> = (0..3 * 48_000 * 2)
        .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * (i / 2) as f32 / 48_000.0).sin())
        .collect();
    let mut play_pos = 0usize;
    let mut captured = Vec::new();
    let mut scratch = vec![0.0f32; capacity];
    for _ in 0..device_frames_total / 120 {
        // Top up the playout ring, then pump one device bite, then drain
        // capture: the worker's servicing pattern.
        loop {
            let end = (play_pos + 240).min(play.len());
            let want = end - play_pos;
            if want == 0 {
                break;
            }
            let pushed = engine.push_playout(&play[play_pos..end]);
            play_pos += pushed;
            if pushed < want {
                break;
            }
        }
        stream.pump(120).unwrap();
        let got = engine.pull_captured(&mut scratch);
        captured.extend_from_slice(&scratch[..got]);
    }
    stream.finish().unwrap();
    assert!(play_pos < play.len(), "the render source ran out");

    assert_eq!(engine.overruns(), 0, "capture must fit the sized ring");
    assert_eq!(engine.underruns(), 0, "render must never find the ring dry");

    // Capture direction: 88 200 device frames are exactly 96 000 session
    // frames; anything beyond buffering slack means dropped or invented
    // audio.
    let captured_frames = captured.len() / 2;
    let expected = device_frames_total * 160 / 147;
    assert!(
        captured_frames <= expected && expected - captured_frames <= 3 * 120,
        "expected ~{expected} session frames, got {captured_frames}"
    );
    let steady = left(&captured[2 * 480..]);
    let secs = steady.len() as f64 / 48_000.0;
    let hz = cycles(&steady) as f64 / secs;
    assert!(
        (hz - 440.0).abs() < 2.0,
        "capture pitch moved: {hz:.2} Hz out of 440 in"
    );
    let level = rms(&steady);
    assert!(
        (level - 0.3536).abs() < 0.02,
        "capture level moved: rms {level:.4}"
    );

    // Playback direction: the file is device-rate audio of the pushed sine.
    let (spec, written) = read_all(&output_path);
    assert_eq!(
        spec.sample_rate, 44_100,
        "the file runs on the device clock"
    );
    assert_eq!(written.len(), device_frames_total * 2);
    let tail = left(&written[2 * 4_410..]);
    let secs = tail.len() as f64 / 44_100.0;
    let hz = cycles(&tail) as f64 / secs;
    assert!(
        (hz - 440.0).abs() < 2.0,
        "render pitch moved: {hz:.2} Hz out of 440 in"
    );
    let run = longest_zero_run(&tail);
    assert!(
        run < 240,
        "steady-state render holds a {run}-sample zero run"
    );
    let level = rms(&tail);
    assert!(
        (level - 0.3536).abs() < 0.02,
        "render level moved: rms {level:.4}"
    );

    let _ = std::fs::remove_file(&input_path);
    let _ = std::fs::remove_file(&output_path);
}

#[test]
fn a_44_1_device_carries_a_48_khz_session_through_the_bridge() {
    bridge_conversion_case("bridge-44-1", None);
}

/// The combined Windows shape: 44.1 kHz device clock and a 441-frame device
/// period, pumped in the same 120-frame bites the client uses.
#[test]
fn a_44_1_device_period_carries_a_48_khz_session_through_the_bridge() {
    bridge_conversion_case("bridge-44-1-period", Some(441));
}

#[test]
fn non_48k_input_is_rejected() {
    let input_path = temp_path("44k-in.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 44_100,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(&input_path, spec).unwrap();
    writer.write_sample(0.0f32).unwrap();
    writer.finalize().unwrap();

    let backend = WavBackend::new(Some(input_path.clone()), None);
    let err = backend.open_offline(config(1), passthrough()).unwrap_err();
    assert!(matches!(err, AudioError::Unsupported(_)));
    let _ = std::fs::remove_file(&input_path);
}
