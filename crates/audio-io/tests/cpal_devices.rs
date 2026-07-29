//! Real-device smoke test. Ignored by default: CI runners have no audio
//! hardware. Run locally with `cargo test -p jamstream-audio-io -- --ignored`.
//!
//! This counts callbacks and samples rather than inspecting them, so it passes
//! on a machine producing pure silence. That is deliberate, because it has to
//! work on any pair of default devices. `hardware_loopback.rs` checks the
//! audio content and needs a loopback device to do it.
//!
//! Asking for it is asserting you have devices, so a machine with none fails
//! here rather than printing a note and passing. It used to do the latter,
//! which meant the only way to run it reported success whether or not it had
//! tested anything.

#![cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use jamstream_audio_io::{Direction, DuplexHandler, StreamConfig, backend};

#[test]
#[ignore = "requires real audio devices"]
fn enumerate_and_open_default_duplex() {
    let backend = backend();
    let devices = backend.devices().expect("device enumeration");
    for d in &devices {
        println!(
            "{:?} {:?} default={} buffer={:?}..{:?} id={}",
            d.direction, d.name, d.is_default, d.min_buffer_frames, d.max_buffer_frames, d.id
        );
    }

    assert!(
        devices.iter().any(|d| d.direction == Direction::Capture),
        "no capture endpoint among {} devices, so there is nothing to open. \
         This test only runs when it is asked for, and asking for it is saying \
         the machine has audio devices.",
        devices.len()
    );
    assert!(
        devices.iter().any(|d| d.direction == Direction::Playback),
        "no playback endpoint among {} devices, so there is nothing to open.",
        devices.len()
    );

    let captured = Arc::new(AtomicUsize::new(0));
    let played = Arc::new(AtomicUsize::new(0));
    let captured_cb = Arc::clone(&captured);
    let played_cb = Arc::clone(&played);
    let handler = DuplexHandler::new(
        move |samples: &[f32]| {
            captured_cb.fetch_add(samples.len(), Ordering::Relaxed);
        },
        move |out: &mut [f32]| {
            // Play silence; counting is the point.
            out.fill(0.0);
            played_cb.fetch_add(out.len(), Ordering::Relaxed);
        },
    );

    let config = StreamConfig {
        sample_rate: 48_000,
        buffer_frames: 240,
        channels: 2,
    };
    let handle = backend
        .open_duplex(None, None, config, handler)
        .expect("open default duplex stream");
    println!("latency_frames={:?}", handle.latency_frames());
    // On Windows this is the only way to see whether the WASAPI exclusive path
    // or the cpal shared-mode fallback won, and roughly what each costs; on the
    // other platforms there is no such distinction to report.
    println!("mode={:?}", jamstream_audio_io::active_device_mode());
    #[cfg(target_os = "windows")]
    assert!(
        jamstream_audio_io::active_device_mode().is_some(),
        "the windows backend must report the sharing mode it opened"
    );

    std::thread::sleep(Duration::from_millis(200));

    let captured = captured.load(Ordering::Relaxed);
    let played = played.load(Ordering::Relaxed);
    println!("captured {captured} samples, played {played} samples in 200 ms");
    assert!(!handle.errored(), "stream reported an error");
    assert!(captured > 0, "capture callbacks never fired");
    assert!(played > 0, "playback callbacks never fired");
    handle.close();
}
