//! CallbackBridge: ordering across ring wrap, silence on underrun, drop on
//! overrun, counters visible from the engine side, and the two rings sized
//! apart from each other.

use jamstream_audio_io::CallbackBridge;

/// Capture direction: device pushes chunks of 7, engine drains with a 5-wide
/// buffer. Capacity 32 forces thousands of wraps; every sample must come out
/// once and in order.
#[test]
fn capture_round_trip_preserves_order_across_wrap() {
    let (mut device, mut engine) = CallbackBridge::new(32, 32);
    let mut next_push = 0u32;
    let mut next_pull = 0u32;
    let mut chunk = [0.0f32; 7];
    let mut out = [0.0f32; 5];

    for _ in 0..2_000 {
        for slot in chunk.iter_mut() {
            *slot = next_push as f32;
            next_push += 1;
        }
        device.on_capture(&chunk);

        let mut drained = 0;
        while drained < chunk.len() {
            let got = engine.pull_captured(&mut out);
            assert!(got > 0, "ring should not be empty mid-drain");
            for &v in &out[..got] {
                assert_eq!(v, next_pull as f32);
                next_pull += 1;
            }
            drained += got;
        }
    }
    assert_eq!(next_pull, 14_000);
    assert_eq!(engine.overruns(), 0);
    assert_eq!(engine.underruns(), 0);
}

/// Playout direction, same shape: engine pushes, device callback drains.
#[test]
fn playout_round_trip_preserves_order_across_wrap() {
    let (mut device, mut engine) = CallbackBridge::new(32, 32);
    let mut next_push = 0u32;
    let mut next_pull = 0u32;
    let mut chunk = [0.0f32; 5];
    let mut out = [0.0f32; 5];

    for _ in 0..2_000 {
        for slot in chunk.iter_mut() {
            *slot = next_push as f32;
            next_push += 1;
        }
        assert_eq!(engine.push_playout(&chunk), chunk.len());

        device.on_playback(&mut out);
        for &v in &out {
            assert_eq!(v, next_pull as f32);
            next_pull += 1;
        }
    }
    assert_eq!(engine.underruns(), 0);
}

#[test]
fn playback_underrun_fills_silence_and_counts() {
    let (mut device, mut engine) = CallbackBridge::new(16, 16);

    let mut out = [1.0f32; 4];
    device.on_playback(&mut out);
    assert_eq!(out, [0.0; 4], "empty ring must play silence");
    assert_eq!(engine.underruns(), 1);

    // Partial fill: available samples come through, the tail is silence.
    assert_eq!(engine.push_playout(&[7.0, 8.0]), 2);
    let mut out = [1.0f32; 4];
    device.on_playback(&mut out);
    assert_eq!(out, [7.0, 8.0, 0.0, 0.0]);
    assert_eq!(engine.underruns(), 2);
}

#[test]
fn capture_overrun_drops_tail_and_counts() {
    let (mut device, mut engine) = CallbackBridge::new(4, 4);

    device.on_capture(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    assert_eq!(engine.overruns(), 1);

    let mut out = [0.0f32; 8];
    let got = engine.pull_captured(&mut out);
    assert_eq!(got, 4, "only the capacity worth of samples survives");
    assert_eq!(&out[..4], &[1.0, 2.0, 3.0, 4.0]);
}

/// The two capacities belong to their own rings. Capture is drained to empty
/// by its consumer and playout is kept full by its producer, so a client may
/// take a deep capture ring and a shallow playout one. Transposing the
/// arguments fails here.
#[test]
fn the_two_rings_are_sized_apart() {
    let (mut device, mut engine) = CallbackBridge::new(16, 4);

    device.on_capture(&[1.0; 16]);
    assert_eq!(engine.overruns(), 0, "the capture ring holds sixteen");

    assert_eq!(
        engine.push_playout(&[2.0; 16]),
        4,
        "the playout ring holds four"
    );
}

/// Capture that arrives before the consumer's first drain is destroyed unless
/// the ring can hold it, which is every session's first moments: the device
/// runs on the sound card's clock while the worker still has work to do.
///
/// Counted in callbacks rather than timed, so a loaded runner cannot change the
/// answer. A real CoreAudio device delivers 120-frame callbacks 2.5 ms apart
/// and takes past 20 ms to hand the stream over, so eight of them is the window
/// a ring has to survive; two callbacks of capacity holds 5 ms of it.
#[test]
fn a_ring_holds_what_arrives_before_the_consumer_starts() {
    const CALLBACK: usize = 240;
    const BRING_UP: usize = 8;

    for (capacity, want_drops) in [(2 * CALLBACK, true), (16 * CALLBACK, false)] {
        let (mut device, mut engine) = CallbackBridge::new(capacity, CALLBACK);
        for _ in 0..BRING_UP {
            device.on_capture(&[1.0; CALLBACK]);
        }
        let mut buf = vec![0.0f32; capacity];
        let got = engine.pull_captured(&mut buf);
        let overruns = engine.overruns();
        assert_eq!(
            overruns > 0,
            want_drops,
            "a ring of {capacity} samples saw {overruns} overruns holding \
             {BRING_UP} callbacks of {CALLBACK}, and the first drain took {got}"
        );
    }
}

/// The DuplexHandler produced by into_handler shares the same rings and
/// counters as the methods on DeviceSide.
#[test]
fn counters_visible_through_handler_path() {
    let (device, mut engine) = CallbackBridge::new(4, 4);
    let mut handler = device.into_handler();

    handler.on_capture(&[1.0, 2.0, 3.0, 4.0, 5.0]);
    let mut out = [9.0f32; 3];
    handler.on_playback(&mut out);

    assert_eq!(out, [0.0; 3]);
    assert_eq!(engine.overruns(), 1);
    assert_eq!(engine.underruns(), 1);

    let mut captured = [0.0f32; 4];
    assert_eq!(engine.pull_captured(&mut captured), 4);
    assert_eq!(captured, [1.0, 2.0, 3.0, 4.0]);
}
