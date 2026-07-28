//! Encode cost of the recording FLAC path, priced against the 2.5 ms tick.
//!
//! The signal is a band-shaped mix rather than silence or a lone sine:
//! constant subframes would make the encoder look free. Run with
//! `cargo bench -p jamstream-server`.

use criterion::{Criterion, criterion_group, criterion_main};
use jamstream_server::flac::FlacEncoder;

/// Interleaved stereo samples per 2.5 ms tick.
const TICK: usize = 240;
const TICKS: usize = 800; // 2 s

/// Ten detuned voices with slow envelopes, peaking near the limiter ceiling.
fn band_mix() -> Vec<f32> {
    let mut signal = vec![0.0f32; TICKS * TICK];
    for v in 0..10 {
        let hz = 110.0 * (v + 1) as f32 * 1.003;
        for (i, pair) in signal.chunks_exact_mut(2).enumerate() {
            let t = i as f32 / 48_000.0;
            let env = 0.06 + 0.03 * (std::f32::consts::TAU * 0.7 * t + v as f32).sin();
            let s = (std::f32::consts::TAU * hz * t).sin() * env;
            pair[0] += s;
            pair[1] += s * 0.9;
        }
    }
    signal
}

fn encode(c: &mut Criterion) {
    let signal = band_mix();
    let mut group = c.benchmark_group("record");
    // One iteration encodes 800 ticks, so per-tick cost is the reported
    // time over 800.
    group.bench_function("flac_encode_2s_stereo_mix", |b| {
        b.iter(|| {
            let mut enc = FlacEncoder::new();
            let mut out = enc.header().unwrap();
            for tick in signal.chunks_exact(TICK) {
                enc.push(tick, &mut out).unwrap();
            }
            enc.finish(&mut out).unwrap();
            out.len()
        });
    });
    group.finish();
}

criterion_group!(benches, encode);
criterion_main!(benches);
