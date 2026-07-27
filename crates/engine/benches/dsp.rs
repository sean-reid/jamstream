//! Component costs at the settings the session actually ships: 48 kHz,
//! 2.5 ms musician frames, a 20 ms broadcast frame, a 48 sample limiter
//! lookahead. Every configuration here is copied from the server and client
//! cores, so a number that moves is a number the tick budget feels.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use jamstream_engine::{
    Channels, Decoder, DriftCompensator, Encoder, Fader, JitterBuffer, Limiter, MediaPacket,
    Metronome, mix_into,
};
use jamstream_protocol::ids::MemberId;
use jamstream_protocol::media::FrameDuration;

/// crates/session/src/client.rs, musician uplink.
const UPLINK_BITRATE: u32 = 128_000;
/// crates/session/src/server.rs, personal stereo mix.
const PERSONAL_MIX_BITRATE: u32 = 192_000;
/// crates/session/src/server.rs, the 20 ms listener frame.
const BROADCAST_BITRATE: u32 = 128_000;
const LIMITER_CEILING_DB: f32 = -1.0;
const LIMITER_LOOKAHEAD_SAMPLES: usize = 48;
const TICK_SAMPLES: usize = 120;

/// A signal with harmonics and a little noise. Silence and pure sines are
/// both cheap to encode; this keeps the codec doing the work it does on a
/// real instrument.
fn signal(len: usize, seed: u32) -> Vec<f32> {
    let mut state = seed | 1;
    (0..len)
        .map(|i| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (state >> 8) as f32 / (1 << 24) as f32 - 0.5;
            let t = i as f32 / 48_000.0;
            let f = 110.0 + (seed % 7) as f32 * 30.0;
            let tone = (core::f32::consts::TAU * f * t).sin() * 0.5
                + (core::f32::consts::TAU * f * 2.0 * t).sin() * 0.25
                + (core::f32::consts::TAU * f * 3.0 * t).sin() * 0.12;
            (tone + noise * 0.05) * 0.7
        })
        .collect()
}

/// Encoders and decoders carry state across frames; a first call on a fresh
/// one is not the steady-state cost. Both are warmed before measurement.
fn opus(c: &mut Criterion) {
    let mut g = c.benchmark_group("opus");
    let cases: [(&str, Channels, FrameDuration, u32); 3] = [
        (
            "mono_2_5ms_128k",
            Channels::Mono,
            FrameDuration::Ms2_5,
            UPLINK_BITRATE,
        ),
        (
            "stereo_2_5ms_192k",
            Channels::Stereo,
            FrameDuration::Ms2_5,
            PERSONAL_MIX_BITRATE,
        ),
        (
            "stereo_20ms_128k",
            Channels::Stereo,
            FrameDuration::Ms20,
            BROADCAST_BITRATE,
        ),
    ];

    for (name, channels, duration, bitrate) in cases {
        let len = duration.samples() as usize * channels.count();
        let pcm = signal(len * 64, 3);
        let mut enc = Encoder::new(channels, duration, bitrate).expect("encoder");
        let mut packet = Vec::new();
        for chunk in pcm.chunks_exact(len).take(8) {
            enc.encode(chunk, &mut packet).expect("warm");
        }

        let mut frame = 0usize;
        let frames = pcm.len() / len;
        g.bench_function(format!("encode/{name}"), |b| {
            b.iter(|| {
                let chunk = &pcm[frame * len..(frame + 1) * len];
                frame = (frame + 1) % frames;
                enc.encode(black_box(chunk), &mut packet).expect("encode");
                black_box(&packet);
            })
        });

        // Decode wants a stream of distinct packets for the same reason:
        // decoding one packet over and over would sit in a warmer cache than
        // the tick ever sees.
        let mut enc = Encoder::new(channels, duration, bitrate).expect("encoder");
        let packets: Vec<Vec<u8>> = pcm
            .chunks_exact(len)
            .map(|chunk| {
                let mut out = Vec::new();
                enc.encode(chunk, &mut out).expect("encode");
                out
            })
            .collect();
        let mut dec = Decoder::new(channels, duration).expect("decoder");
        let mut out = vec![0.0f32; len];
        for p in packets.iter().take(8) {
            dec.decode(Some(p), &mut out, false).expect("warm");
        }
        let mut i = 0usize;
        g.bench_function(format!("decode/{name}"), |b| {
            b.iter(|| {
                let p = &packets[i];
                i = (i + 1) % packets.len();
                dec.decode(Some(black_box(p)), &mut out, false)
                    .expect("decode");
                black_box(&out);
            })
        });

        g.bench_function(format!("decode_plc/{name}"), |b| {
            b.iter(|| {
                dec.decode(None, &mut out, false).expect("plc");
                black_box(&out);
            })
        });
    }
    g.finish();
}

fn mixer(c: &mut Criterion) {
    let mut g = c.benchmark_group("mixer");
    // A flat closure, not the server's fader map: this measures the mix
    // arithmetic. The map lookup is in the tick benchmark, where it belongs.
    let faders = |_| Fader {
        gain_db: -3.0,
        pan: 0.3,
        muted: false,
    };
    for n in [1usize, 4, 10, 20] {
        let buffers: Vec<Vec<f32>> = (0..n).map(|i| signal(TICK_SAMPLES, i as u32 + 1)).collect();
        let sources: Vec<(MemberId, &[f32])> = buffers
            .iter()
            .enumerate()
            .map(|(i, b)| (MemberId(i as u16), b.as_slice()))
            .collect();
        let mut out = vec![0.0f32; TICK_SAMPLES * 2];
        g.bench_function(format!("mix_into/{n}_sources_2_5ms"), |b| {
            b.iter(|| {
                mix_into(black_box(&sources), faders, None, &mut out);
                black_box(&out);
            })
        });
    }
    g.finish();
}

fn limiter(c: &mut Criterion) {
    let mut g = c.benchmark_group("limiter");
    for (name, pairs) in [("2_5ms", TICK_SAMPLES), ("20ms", TICK_SAMPLES * 8)] {
        // Hot enough to keep the gain computer working rather than idling at
        // unity, which is the expensive case and the one that matters.
        let source: Vec<f32> = signal(pairs * 2, 5).iter().map(|s| s * 2.0).collect();
        let mut buf = source.clone();
        let mut lim = Limiter::new(LIMITER_CEILING_DB, LIMITER_LOOKAHEAD_SAMPLES);
        lim.process(&mut buf);
        g.bench_function(format!("process/{name}"), |b| {
            b.iter(|| {
                buf.copy_from_slice(&source);
                lim.process(black_box(&mut buf));
                black_box(&buf);
            })
        });
    }
    g.finish();
}

fn jitter(c: &mut Criterion) {
    let mut g = c.benchmark_group("jitter");
    let payload: Vec<u8> = signal(60, 9).iter().map(|s| (s * 127.0) as u8).collect();
    // Push and pull are measured as a pair, one of each, because that is
    // exactly what a tick does and because isolating either needs a fresh
    // buffer per iteration whose teardown would outweigh a 100 ns routine.
    let mut buf = JitterBuffer::new();
    let mut seq = 0u32;
    for _ in 0..64 {
        buf.push(MediaPacket {
            seq,
            timestamp: u64::from(seq) * 120,
            payload: Vec::new(),
            redundant: None,
        });
        seq += 1;
        let _ = buf.pull();
    }
    g.bench_function("push_pull/steady_state", |b| {
        b.iter(|| {
            buf.push(MediaPacket {
                seq,
                timestamp: u64::from(seq) * 120,
                payload: black_box(&payload).clone(),
                redundant: None,
            });
            seq = seq.wrapping_add(1);
            black_box(buf.pull())
        })
    });
    g.bench_function("stats", |b| b.iter(|| black_box(buf.stats())));
    g.finish();
}

fn metronome(c: &mut Criterion) {
    let mut g = c.benchmark_group("metronome");
    let m = Metronome {
        bpm: 120,
        beats_per_bar: 4,
    };
    let mut out = vec![0.0f32; TICK_SAMPLES];
    let mut clock = 0u64;
    // The clock advances a tick per iteration, so click ticks and silent
    // ticks appear in the same proportion the session sees.
    g.bench_function("render/2_5ms", |b| {
        b.iter(|| {
            m.render(black_box(clock), &mut out, 0.7);
            clock += TICK_SAMPLES as u64;
            black_box(&out);
        })
    });
    g.finish();
}

fn drift(c: &mut Criterion) {
    let mut g = c.benchmark_group("drift");
    for (name, channels) in [("mono", 1usize), ("stereo", 2)] {
        let mut comp = DriftCompensator::new(TICK_SAMPLES, channels);
        // 200 ppm, the drifting-clock harness profile, so the resampler is
        // steering rather than passing audio through at unity.
        comp.steer(200.0);
        let input = signal(TICK_SAMPLES * channels, 11);
        let mut out = vec![0.0f32; TICK_SAMPLES * channels];
        // Push a chunk, drain whatever that made available. Steering means
        // the two rates differ, so an iteration occasionally yields no frame
        // and the backlog self-corrects; pushing blind would either drain the
        // buffer or hit its overflow memmove, and neither is the client's
        // behaviour.
        let tick = |comp: &mut DriftCompensator, out: &mut [f32]| {
            comp.push(&input);
            while comp.pull_frame(out) {
                black_box(&out);
            }
        };
        for _ in 0..16 {
            tick(&mut comp, &mut out);
        }
        g.bench_function(format!("push_pull/{name}_2_5ms"), |b| {
            b.iter(|| tick(black_box(&mut comp), black_box(&mut out)))
        });
    }
    g.finish();
}

criterion_group!(benches, opus, mixer, limiter, jitter, metronome, drift);
criterion_main!(benches);
