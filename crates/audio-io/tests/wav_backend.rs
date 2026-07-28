//! WavBackend: deterministic offline pump against generated fixtures.
//!
//! The passthrough handler below echoes captured samples into playback via a
//! shared queue. WavStream::pump runs capture before playback within one
//! call, so the capture file matches the input WAV at zero offset.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use jamstream_audio_io::{AudioError, DuplexHandler, StreamConfig, StreamHandle, WavBackend};

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
