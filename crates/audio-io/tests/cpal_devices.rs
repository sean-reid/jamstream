//! Real-device smoke test. Ignored by default: CI runners have no audio
//! hardware. Run locally with `cargo test -p jamstream-audio-io -- --ignored`.
//!
//! This counts callbacks and samples rather than inspecting them, so it passes
//! on a machine producing pure silence. That is deliberate, because it has to
//! work on any pair of default devices. `hardware_loopback.rs` checks the
//! audio content and needs a loopback device to do it.
//!
//! The second half is the saved-selection round trip. The id a backend mints
//! is the whole of what a client persists, and the next launch has nothing but
//! that string to find the device again with, so an id that does not survive a
//! re-enumeration or does not resolve on an open drops a saved interface back
//! to the system default with nothing to say about it. Both ends of that are
//! otherwise only tested against fakes, and a fake agrees with itself.
//!
//! Asking for it is asserting you have devices, so a machine with none fails
//! here rather than printing a note and passing: a note and a pass means the
//! only way to run this reports success whether or not it tested anything.

#![cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use jamstream_audio_io::{
    AudioBackend, DeviceInfo, Direction, DuplexHandler, StreamConfig, backend,
};

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

    let ran = run_duplex(&*backend, None, None);
    println!("latency_frames={:?}", ran.latency_frames);
    // On Windows this is the only way to see whether the WASAPI exclusive path
    // or the cpal shared-mode fallback won, and roughly what each costs; on the
    // other platforms there is no such distinction to report.
    println!("mode={:?}", jamstream_audio_io::active_device_mode());
    #[cfg(target_os = "windows")]
    assert!(
        jamstream_audio_io::active_device_mode().is_some(),
        "the windows backend must report the sharing mode it opened"
    );

    println!(
        "captured {} samples, played {} samples in 200 ms",
        ran.captured, ran.played
    );
    assert!(!ran.errored, "stream reported an error");
    assert!(ran.captured > 0, "capture callbacks never fired");
    assert!(ran.played > 0, "playback callbacks never fired");

    saved_ids_reopen_the_same_devices(&*backend, &devices);
}

/// The session's stream shape, so a hand check opens what the product opens.
fn config() -> StreamConfig {
    StreamConfig {
        sample_rate: 48_000,
        buffer_frames: 240,
        channels: 2,
        ..StreamConfig::default()
    }
}

/// What one duplex run moved, read off the handle before it closed.
struct Ran {
    captured: usize,
    played: usize,
    errored: bool,
    latency_frames: Option<u32>,
}

/// Runs a duplex stream for 200 ms against the given device ids, where `None`
/// is the system default for that direction.
fn run_duplex(backend: &dyn AudioBackend, capture: Option<&str>, playback: Option<&str>) -> Ran {
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

    let handle = backend
        .open_duplex(capture, playback, config(), handler)
        .unwrap_or_else(|err| {
            panic!("open duplex stream capture={capture:?} playback={playback:?}: {err}")
        });
    let latency_frames = handle.latency_frames();
    std::thread::sleep(Duration::from_millis(200));
    let ran = Ran {
        captured: captured.load(Ordering::Relaxed),
        played: played.load(Ordering::Relaxed),
        errored: handle.errored(),
        latency_frames,
    };
    handle.close();
    ran
}

/// Saves the default pair the way a client does, by id alone, then reopens
/// from the saved strings against a fresh enumeration and asserts the devices
/// that came back are the ones the ids named.
fn saved_ids_reopen_the_same_devices(backend: &dyn AudioBackend, saved: &[DeviceInfo]) {
    let default_of = |direction: Direction| {
        saved
            .iter()
            .find(|d| d.direction == direction && d.is_default)
            .unwrap_or_else(|| {
                panic!(
                    "no {direction:?} endpoint among {} calls itself the default, so there \
                     is no id for a client to save",
                    saved.len()
                )
            })
    };
    let picks = [
        default_of(Direction::Capture),
        default_of(Direction::Playback),
    ];

    // The next launch enumerates from scratch and holds nothing but the ids.
    let today = backend
        .devices()
        .expect("device enumeration on the next launch");
    for pick in picks {
        println!(
            "saved {:?} id={} name={:?}",
            pick.direction, pick.id, pick.name
        );
        assert!(
            !pick.id.is_empty(),
            "the default {:?} endpoint has an empty id, which persists as a \
             selection nothing can look up",
            pick.direction
        );
        let again: Vec<&DeviceInfo> = today
            .iter()
            .filter(|d| d.id == pick.id && d.direction == pick.direction)
            .collect();
        assert_eq!(
            again.len(),
            1,
            "the saved {:?} id {} names {} endpoints in a fresh enumeration, so a \
             lookup by that id cannot land on one device",
            pick.direction,
            pick.id,
            again.len()
        );
        assert_eq!(
            again[0].name, pick.name,
            "the saved {:?} id {} names {:?} in a fresh enumeration and named {:?} \
             when it was saved, so a saved selection reopens a different device",
            pick.direction, pick.id, again[0].name, pick.name
        );
    }

    let [capture, playback] = picks;
    let ran = run_duplex(backend, Some(&capture.id), Some(&playback.id));
    println!(
        "reopened by id: captured {} samples, played {} samples in 200 ms, \
         latency_frames={:?}",
        ran.captured, ran.played, ran.latency_frames
    );
    assert!(
        !ran.errored,
        "the stream opened against the saved ids reported an error"
    );
    assert!(
        ran.captured > 0,
        "no capture callback fired on the device saved as {}",
        capture.id
    );
    assert!(
        ran.played > 0,
        "no playback callback fired on the device saved as {}",
        playback.id
    );

    // A backend that quietly opened the default when an id resolves to nothing
    // would pass everything above whatever the saved ids named.
    let unknown = format!("{}~gone", capture.id);
    let refused = backend
        .open_duplex(
            Some(&unknown),
            None,
            config(),
            DuplexHandler::new(|_: &[f32]| {}, |out: &mut [f32]| out.fill(0.0)),
        )
        .err()
        .unwrap_or_else(|| {
            panic!(
                "id {unknown} names no device and the open succeeded anyway, so an \
                 open by id falls back to the default and nothing above says the \
                 saved ids were honoured"
            )
        });
    println!("an id naming no device is refused: {refused}");
}
