//! Verifies that audio *content* survives a round trip through real hardware,
//! which the callback-counting smoke test in `cpal_devices.rs` cannot show.
//!
//! Ignored by default and skipped when no loopback device is present, because
//! it needs a virtual device whose output feeds its own input: BlackHole on
//! macOS, VB-CABLE on Windows, a PipeWire or PulseAudio null sink on Linux.
//! Run it with
//!
//! ```text
//! cargo test -p jamstream-audio-io --test hardware_loopback -- --ignored --nocapture
//! ```
//!
//! A 440 Hz tone is generated on the test thread, encoded and decoded with the
//! session's real Opus settings, pushed through the real `CallbackBridge`, and
//! played to the loopback device. The same stream captures it back, and the
//! captured samples are checked in the frequency domain. That covers the parts
//! only real hardware exercises: the backend's sample format conversion, its
//! channel layout handling, buffer size negotiation, and the two device
//! threads driving the rings.
//!
//! Failures this catches that offline tests do not: a conversion that mangles
//! amplitude or sign, a channel swap or interleave error, a negotiated buffer
//! size the rings cannot keep up with, and non-finite samples reaching the
//! device (which on Windows previously turned a NaN into full-scale positive,
//! because `f32::min` returns the other operand on NaN).

#![cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]

use std::time::{Duration, Instant};

use jamstream_audio_io::{CallbackBridge, Direction, StreamConfig, backend};
use jamstream_engine::{Channels, Decoder, Encoder};
use jamstream_protocol::media::FrameDuration;

/// Substrings that identify a loopback device on each platform. Matched
/// case-insensitively against the device name.
const LOOPBACK_NAMES: &[&str] = &[
    "blackhole",  // macOS
    "cable",      // VB-CABLE on Windows
    "loopback",   // several virtual drivers
    "null",       // PipeWire/PulseAudio null sink
    "monitor of", // PulseAudio monitor source
];

const TONE_HZ: f64 = 440.0;
/// Control frequencies either side of the tone. Both land on an exact bin at
/// the analysis length below, as does the tone itself.
const CONTROL_HZ: [f64; 2] = [300.0, 700.0];
const TONE_AMPLITUDE: f32 = 0.25;

/// Discarded before analysis: device startup, the loopback driver's own
/// buffering, and the encoder reaching steady state.
const WARMUP: Duration = Duration::from_millis(700);
/// Samples per channel analyzed. 24000 at 48 kHz is 0.5 s and puts 440, 300,
/// and 700 Hz all exactly on a bin, so the Goertzel magnitudes are directly
/// comparable without windowing.
const ANALYSIS_SAMPLES: usize = 24_000;

#[test]
#[ignore = "requires a loopback audio device (BlackHole, VB-CABLE, or a null sink)"]
fn a_tone_survives_the_round_trip_through_real_hardware() {
    let backend = backend();
    let devices = backend.devices().expect("device enumeration");

    // A usable loopback reports the same id in both directions, so one duplex
    // stream can play into it and record what comes back.
    let loopback = devices
        .iter()
        .filter(|d| d.direction == Direction::Capture)
        .filter(|d| is_loopback(&d.name))
        .find(|c| {
            devices
                .iter()
                .any(|p| p.direction == Direction::Playback && p.id == c.id)
        });
    let Some(loopback) = loopback else {
        println!(
            "no loopback device among {} endpoints, skipping. Install BlackHole (macOS), \
             VB-CABLE (Windows), or create a null sink (Linux) to run this test.",
            devices.len()
        );
        return;
    };
    println!("loopback device: {:?} id={}", loopback.name, loopback.id);

    let config = StreamConfig::default();
    let channels = config.channels as usize;
    let frame_samples = FrameDuration::Ms2_5.samples() as usize;
    let frame_len = frame_samples * channels;

    // Four device callbacks of slack in each ring. Enough to absorb the
    // scheduling jitter of a test thread that is not real-time, small enough
    // that a backend which negotiated a much larger buffer shows up as
    // underruns rather than being hidden.
    let capacity = config.buffer_frames as usize * channels * 4;
    let (device, mut engine) = CallbackBridge::new(capacity);

    let mut encoder = Encoder::new(Channels::Stereo, FrameDuration::Ms2_5, 96_000)
        .expect("opus encoder at the session's 2.5 ms stereo settings");
    let mut decoder = Decoder::new(Channels::Stereo, FrameDuration::Ms2_5).expect("opus decoder");

    let stream = backend
        .open_duplex(
            Some(&loopback.id),
            Some(&loopback.id),
            config,
            device.into_handler(),
        )
        .expect("open duplex on the loopback device");
    println!("negotiated latency_frames={:?}", stream.latency_frames());

    let mut phase: f64 = 0.0;
    let phase_step = std::f64::consts::TAU * TONE_HZ / f64::from(config.sample_rate);
    let mut pcm = vec![0.0f32; frame_len];
    let mut packet: Vec<u8> = Vec::with_capacity(1500);
    let mut decoded = vec![0.0f32; frame_len];
    // Carries the tail of a frame the ring could not accept in one go.
    let mut pending: usize = frame_len;

    let mut scratch = vec![0.0f32; capacity];
    let mut captured: Vec<f32> = Vec::with_capacity(48_000 * channels * 3);
    // Accounting that distinguishes "the loopback did not carry the tone" from
    // "the playback side never consumed it", which look identical downstream.
    let mut pushed_total: usize = 0;

    // Long enough for the warmup plus the analysis window, with headroom for a
    // device that takes its time starting.
    let deadline = Instant::now() + WARMUP + Duration::from_millis(1_200);
    while Instant::now() < deadline {
        // Refill the playout ring, generating and coding a new frame whenever
        // the previous one has been fully handed over.
        loop {
            if pending == frame_len {
                for f in 0..frame_samples {
                    let s = (phase.sin() as f32) * TONE_AMPLITUDE;
                    phase += phase_step;
                    for c in 0..channels {
                        pcm[f * channels + c] = s;
                    }
                }
                if phase > std::f64::consts::TAU {
                    phase -= std::f64::consts::TAU;
                }
                encoder.encode(&pcm, &mut packet).expect("encode one frame");
                decoder
                    .decode(Some(&packet), &mut decoded, false)
                    .expect("decode one frame");
                pending = 0;
            }
            let pushed = engine.push_playout(&decoded[pending..]);
            pending += pushed;
            pushed_total += pushed;
            if pushed == 0 {
                break;
            }
        }

        let got = engine.pull_captured(&mut scratch);
        captured.extend_from_slice(&scratch[..got]);

        // Roughly two device callbacks. Short enough to keep the ring fed,
        // long enough not to spin.
        std::thread::sleep(Duration::from_millis(5));
    }

    let underruns = engine.underruns();
    let overruns = engine.overruns();
    let errored = stream.errored();
    stream.close();

    println!(
        "pushed {pushed_total} samples to playout, captured {} ({} per channel), \
         underruns={underruns} overruns={overruns}",
        captured.len(),
        captured.len() / channels
    );
    assert!(!errored, "backend reported a fatal stream error");

    // If playback never ran, the ring fills once and every later push returns
    // zero, which downstream is indistinguishable from a loopback that did not
    // carry the tone. Separate the two so the failure names the right layer.
    assert!(
        pushed_total > capacity * 4,
        "only {pushed_total} samples were accepted for playout against a ring of {capacity}. \
         The playback stream is not consuming audio, so nothing was ever played."
    );

    // Nothing non-finite or out of range may reach or leave a device.
    let bad = captured
        .iter()
        .position(|s| !s.is_finite() || s.abs() > 1.001);
    assert!(
        bad.is_none(),
        "sample {:?} at index {:?} is non-finite or outside [-1, 1]",
        bad.map(|i| captured[i]),
        bad
    );

    // Capture that is bit-exact silence is not a weak or distorted signal, it
    // is an input delivering nothing. On macOS that is what the system feeds a
    // process without the Microphone privacy permission: silence, not an error,
    // and it applies to every input device including virtual loopbacks. Name it
    // rather than reporting it as a frequency-domain failure, which sends the
    // reader looking at the codec.
    if captured.iter().all(|s| *s == 0.0) {
        panic!(
            "every one of the {} captured samples is exactly 0.0 while playback consumed \
             {pushed_total}. That is an input delivering no audio at all, not a degraded \
             signal. On macOS, grant Microphone access to the terminal running this test \
             (System Settings, Privacy and Security, Microphone); the OS substitutes \
             silence for processes without it, and a bare test binary is not the signed \
             app bundle that declares NSMicrophoneUsageDescription. On Linux check that \
             the loopback source is not muted.",
            captured.len(),
        );
    }

    let warmup_samples = (WARMUP.as_secs_f64() * f64::from(config.sample_rate)) as usize * channels;
    let needed = warmup_samples + ANALYSIS_SAMPLES * channels;
    assert!(
        captured.len() >= needed,
        "captured {} samples, need {needed} for a {} ms warmup plus a {} sample window. \
         The device is delivering far less audio than real time.",
        captured.len(),
        WARMUP.as_millis(),
        ANALYSIS_SAMPLES,
    );

    // One channel is enough for the frequency check; both are compared below.
    let left: Vec<f32> = captured[warmup_samples..needed]
        .iter()
        .step_by(channels)
        .copied()
        .collect();
    assert_eq!(left.len(), ANALYSIS_SAMPLES);

    let rate = f64::from(config.sample_rate);
    let tone = goertzel(&left, TONE_HZ, rate);
    let controls: Vec<f64> = CONTROL_HZ
        .iter()
        .map(|f| goertzel(&left, *f, rate))
        .collect();
    let rms = (left
        .iter()
        .map(|s| f64::from(*s) * f64::from(*s))
        .sum::<f64>()
        / left.len() as f64)
        .sqrt();
    println!(
        "440 Hz magnitude {tone:.5}, controls {controls:?}, rms {rms:.5}, \
         expected tone amplitude {TONE_AMPLITUDE}"
    );

    // A silent or disconnected path is the failure this exists to catch, so
    // check absolute level before the ratio: white noise would pass a ratio
    // test at a low enough level.
    assert!(
        tone > f64::from(TONE_AMPLITUDE) * 0.25,
        "440 Hz magnitude {tone:.5} is far below the {TONE_AMPLITUDE} tone that was played. \
         Audio is not reaching the capture side."
    );
    for (freq, mag) in CONTROL_HZ.iter().zip(&controls) {
        assert!(
            tone > mag * 8.0,
            "440 Hz magnitude {tone:.5} is not dominant over {freq} Hz at {mag:.5}. \
             The captured signal is not the tone that was played."
        );
    }

    // A channel swap or interleave error on a mono source shows up as one
    // silent channel.
    if channels == 2 {
        let right: Vec<f32> = captured[warmup_samples + 1..needed]
            .iter()
            .step_by(channels)
            .copied()
            .collect();
        let right_tone = goertzel(&right[..ANALYSIS_SAMPLES.min(right.len())], TONE_HZ, rate);
        println!("right channel 440 Hz magnitude {right_tone:.5}");
        assert!(
            right_tone > tone * 0.5,
            "left channel has {tone:.5} at 440 Hz but right has {right_tone:.5}; \
             the channel layout is not being handled symmetrically"
        );
    }
}

/// Guards the half of the loopback test that needs no hardware: if the tone
/// generator or the codec settings were wrong, the ignored test above would
/// fail with "audio is not reaching the capture side" and send the reader
/// looking at the wrong layer. Runs everywhere, including CI.
#[test]
fn the_generated_tone_survives_the_codec() {
    let channels = 2usize;
    let frame_samples = FrameDuration::Ms2_5.samples() as usize;
    let frame_len = frame_samples * channels;
    let rate = 48_000.0f64;

    let mut encoder = Encoder::new(Channels::Stereo, FrameDuration::Ms2_5, 96_000).unwrap();
    let mut decoder = Decoder::new(Channels::Stereo, FrameDuration::Ms2_5).unwrap();

    let mut phase = 0.0f64;
    let phase_step = std::f64::consts::TAU * TONE_HZ / rate;
    let mut pcm = vec![0.0f32; frame_len];
    let mut packet = Vec::with_capacity(1500);
    let mut decoded = vec![0.0f32; frame_len];
    let mut out: Vec<f32> = Vec::new();

    // Enough frames to cover the analysis window plus codec priming.
    for _ in 0..(ANALYSIS_SAMPLES / frame_samples + 200) {
        for f in 0..frame_samples {
            let s = (phase.sin() as f32) * TONE_AMPLITUDE;
            phase += phase_step;
            for c in 0..channels {
                pcm[f * channels + c] = s;
            }
        }
        encoder.encode(&pcm, &mut packet).unwrap();
        decoder.decode(Some(&packet), &mut decoded, false).unwrap();
        out.extend_from_slice(&decoded);
    }

    // Skip priming, then analyze one channel exactly as the hardware test does.
    let skip = 200 * frame_len;
    let left: Vec<f32> = out[skip..skip + ANALYSIS_SAMPLES * channels]
        .iter()
        .step_by(channels)
        .copied()
        .collect();
    let tone = goertzel(&left, TONE_HZ, rate);
    let controls: Vec<f64> = CONTROL_HZ
        .iter()
        .map(|f| goertzel(&left, *f, rate))
        .collect();
    println!("codec-only: 440 Hz {tone:.5}, controls {controls:?}");

    assert!(
        tone > f64::from(TONE_AMPLITUDE) * 0.5,
        "the tone does not survive the codec: 440 Hz magnitude {tone:.5}"
    );
    for mag in &controls {
        assert!(tone > mag * 8.0, "codec output is not a clean 440 Hz tone");
    }
}

fn is_loopback(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    LOOPBACK_NAMES.iter().any(|n| lower.contains(n))
}

/// Magnitude of `freq` in `samples`, scaled so a pure sine of amplitude A
/// reads about A. `freq` should land on a bin, i.e. be an exact multiple of
/// `rate / samples.len()`, so no window is applied.
fn goertzel(samples: &[f32], freq: f64, rate: f64) -> f64 {
    let n = samples.len() as f64;
    let k = (n * freq / rate).round();
    let w = std::f64::consts::TAU * k / n;
    let coeff = 2.0 * w.cos();
    let mut s1 = 0.0f64;
    let mut s2 = 0.0f64;
    for &x in samples {
        let s0 = coeff * s1 - s2 + f64::from(x);
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    power.max(0.0).sqrt() / (n / 2.0)
}
