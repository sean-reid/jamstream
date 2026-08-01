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
    }
}

/// 48 kHz float WAV with a per-channel sine so channels are distinguishable.
fn write_sine(path: &PathBuf, channels: u16, frames: usize) -> Vec<f32> {
    let spec = hound::WavSpec {
        channels,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    let mut samples = Vec::with_capacity(frames * usize::from(channels));
    for i in 0..frames {
        for ch in 0..channels {
            let hz = 440.0 * f32::from(ch + 1);
            let s = 0.5 * (2.0 * std::f32::consts::PI * hz * i as f32 / 48_000.0).sin();
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
    let input = write_sine(&input_path, wav_channels, frames);

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

/// The 44.1 kHz interface, modelled: the session runs at 48 kHz and the fake
/// refuses the same way the cpal backend now does, rather than opening and
/// playing sharp.
#[test]
fn a_device_at_44_1_refuses_a_48_khz_session() {
    let backend = WavBackend::new(None, None).with_device_rate(44_100);
    let err = backend.open_offline(config(2), passthrough()).unwrap_err();
    let AudioError::Unsupported(msg) = err else {
        panic!("expected Unsupported, got {err:?}");
    };
    assert!(msg.contains("44100") && msg.contains("48000"), "{msg}");
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
/// does, and this backend never converts: a mismatched rate is refused, so an
/// open stream is running at the device rate. Safe alongside parallel tests
/// because every wav open publishes the same value.
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
