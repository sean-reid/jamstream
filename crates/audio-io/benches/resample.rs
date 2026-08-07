//! What the device-boundary converter costs inside one device callback.
//!
//! Rung 3 of the sample-rate ladder wraps a direction's handler half in a sinc
//! converter and runs it on the device thread, so its cost comes straight out
//! of the callback deadline: 10 ms at these period sizes, and the rest of the
//! callback still has the channel map, the bridge push, and on a 44.1 kHz
//! device a second sinc stage in `DriftCompensator` further down the chain.
//! The module doc claims tens of microseconds per callback. This is the
//! number, so a change to the filter, the chunk size, or the rate table is a
//! change somebody can see.
//!
//! Both directions and both sides of unity, because the ratio inverts: a
//! 44.1 kHz device upsamples on capture and downsamples on playback, a 96 kHz
//! device does the opposite, and the sinc's work per output frame follows the
//! output side.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use jamstream_audio_io::resample::{
    CaptureFn, PlaybackFn, converting_capture, converting_playback,
};

/// The rate everything above the device layer runs at.
const SESSION: u32 = 48_000;

/// Device rates the ladder really converts for, on both sides of the session
/// rate: 44.1 kHz is the consumer interface rung 3 was written for, 96 kHz is
/// what most pro interfaces ship set to.
const DEVICE_RATES: [u32; 2] = [44_100, 96_000];

const CHANNELS: [(&str, u16); 2] = [("mono", 1), ("stereo", 2)];

/// One 10 ms device period in frames: the WASAPI shared-mode callback size,
/// and the order CoreAudio and PipeWire deliver at these rates. Measuring a
/// whole period is what makes the result readable as a fraction of the
/// deadline rather than as an abstract per-frame cost.
fn period(rate: u32) -> usize {
    rate as usize / 100
}

/// An instrument-shaped signal rather than silence or a pure tone: the sinc
/// itself is data-independent, but denormals in a decayed tail are not, and
/// silence is the one input a real session never sends.
fn signal(frames: usize, channels: u16, rate: u32) -> Vec<f32> {
    let mut state = 0x9E37_79B9u32;
    let mut out = Vec::with_capacity(frames * usize::from(channels));
    for i in 0..frames {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (state >> 8) as f32 / (1 << 24) as f32 - 0.5;
        let t = i as f32 / rate as f32;
        let tone = (core::f32::consts::TAU * 220.0 * t).sin() * 0.5
            + (core::f32::consts::TAU * 440.0 * t).sin() * 0.25
            + (core::f32::consts::TAU * 660.0 * t).sin() * 0.12;
        for _ in 0..channels {
            out.push((tone + noise * 0.05) * 0.7);
        }
    }
    out
}

/// Capture: device-rate callbacks in, fixed session-rate chunks out to the
/// handler the bridge push sits behind.
fn capture(c: &mut Criterion) {
    let mut g = c.benchmark_group("resample_capture");
    for device in DEVICE_RATES {
        for (name, channels) in CHANNELS {
            let frames = period(device);
            let pcm = signal(frames, channels, device);
            let inner: CaptureFn = Box::new(|chunk: &[f32]| {
                black_box(chunk);
            });
            let (mut convert, _added_ms) = converting_capture(inner, SESSION, device, channels);
            // The first callbacks fill the backlog and warm the filter state;
            // steady state is what a callback deadline actually meets.
            for _ in 0..8 {
                convert(&pcm);
            }
            g.bench_function(format!("{device}/{name}_{frames}_frames"), |b| {
                b.iter(|| convert(black_box(&pcm)))
            });
        }
    }
    g.finish();
}

/// Playback: device-rate requests in, session-rate chunks pulled from the
/// handler and converted down to what the device wants.
fn playback(c: &mut Criterion) {
    let mut g = c.benchmark_group("resample_playback");
    for device in DEVICE_RATES {
        for (name, channels) in CHANNELS {
            let frames = period(device);
            // The handler is pulled at the session rate, so its source table
            // is session-rate audio. Indexing round it keeps the handler off
            // the heap, which is what a device thread requires anyway.
            let table = signal(period(SESSION), channels, SESSION);
            let mut at = 0usize;
            let inner: PlaybackFn = Box::new(move |out: &mut [f32]| {
                for s in out.iter_mut() {
                    *s = table[at];
                    at = (at + 1) % table.len();
                }
            });
            let (mut convert, _added_ms) = converting_playback(inner, SESSION, device, channels);
            let mut out = vec![0.0f32; frames * usize::from(channels)];
            for _ in 0..8 {
                convert(&mut out);
            }
            g.bench_function(format!("{device}/{name}_{frames}_frames"), |b| {
                b.iter(|| {
                    convert(&mut out);
                    black_box(&out);
                })
            });
        }
    }
    g.finish();
}

criterion_group!(benches, capture, playback);
criterion_main!(benches);
