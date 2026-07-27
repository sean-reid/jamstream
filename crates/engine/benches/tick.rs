//! One 2.5 ms server mix tick, end to end, against the 2500 us budget.
//!
//! The sequence here mirrors `ServerCore::tick` in crates/session: pull and
//! decode every musician's uplink, render the click, build and limit the
//! broadcast mix, then per musician build a personal mix, encode it, pack a
//! media frame and seal it. Every eighth tick the 20 ms broadcast frame is
//! encoded once and sealed per listener. The session crate is not a
//! dependency here, so this reproduces the sequence out of the same engine
//! and protocol pieces the server drives rather than calling the core; if
//! the core's shape changes, this file has to follow.
//!
//! Running it prints a per-tick distribution table before criterion starts.
//! That table is the artefact: a mean tells you nothing about a tick that
//! runs long one time in eight, and the eighth tick is the broadcast one.

use std::collections::HashMap;
use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::Criterion;

use jamstream_engine::{
    Channels, Decoder, Encoder, Fader, JitterBuffer, Limiter, MediaPacket, Metronome, Pull,
    mix_into,
};
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Issuer, Token};
use jamstream_protocol::media::{FrameDuration, MediaFrame};
use jamstream_protocol::transport::{Initiator, Responder, Session, Welcome, generate_keypair};
use jamstream_protocol::wire::{self, Packet};

/// Everything below is copied from crates/session/src/server.rs and
/// crates/session/src/client.rs. Divergence here makes the whole table lie.
const TICK_SAMPLES: usize = 120;
const MIX_LEN: usize = TICK_SAMPLES * 2;
const BCAST_TICKS: u64 = 8;
const BCAST_LEN: usize = MIX_LEN * BCAST_TICKS as usize;
const UPLINK_BITRATE: u32 = 128_000;
const PERSONAL_MIX_BITRATE: u32 = 192_000;
const BROADCAST_BITRATE: u32 = 128_000;
const CLICK_GAIN: f32 = 0.7;
const LIMITER_CEILING_DB: f32 = -1.0;
const LIMITER_LOOKAHEAD_SAMPLES: usize = 48;

/// The tick deadline. 48 kHz, 120 samples.
const BUDGET: Duration = Duration::from_micros(2_500);

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

/// A real Noise IK transport for one member. The handshake is setup cost,
/// paid once per member per session, and never enters a measurement.
fn server_session(member: MemberId) -> Session {
    let issuer = Issuer::generate();
    let server = generate_keypair();
    let token = Token {
        member_id: member,
        role: Role::Musician,
        name_hint: None,
        expires_unix: 4_000_000_000,
        jti: TokenId::generate(),
    };
    let invite = issuer.mint(
        SessionId::generate(),
        vec!["192.0.2.4:43210".parse().unwrap()],
        server.public,
        token,
    );
    let (_initiator, init_packet) = Initiator::new(&invite).expect("initiator");
    let Ok(Packet::HandshakeInit { version, noise }) = wire::parse(&init_packet) else {
        unreachable!("initiator produced an init packet")
    };
    let (hp, responder) =
        Responder::read_init(&server.private, &invite.session_id, version, noise).expect("read");
    let welcome = Welcome {
        member_id: hp.token.member_id,
        sample_clock: 0,
    };
    responder.respond(&welcome).expect("respond").0
}

struct Musician {
    id: MemberId,
    jitter: JitterBuffer,
    decoder: Decoder,
    encoder: Encoder,
    session: Session,
    faders: HashMap<MemberId, Fader>,
    send_seq: u32,
    uplink_seq: u32,
}

struct Listener {
    id: MemberId,
    session: Session,
    send_seq: u32,
}

struct Rig {
    musicians: Vec<Musician>,
    listeners: Vec<Listener>,
    bcast_faders: HashMap<MemberId, Fader>,
    bcast_encoder: Encoder,
    bcast_accum: Vec<f32>,
    bcast_pkt: Vec<u8>,
    limiter: Limiter,
    metronome: Metronome,
    decoded: Vec<(MemberId, [f32; TICK_SAMPLES])>,
    mix_buf: Vec<f32>,
    pkt_buf: Vec<u8>,
    /// Encoded mono uplink frames, cycled so no decoder ever sees the same
    /// packet twice in a row.
    uplink: Vec<Vec<u8>>,
    out: Vec<Vec<u8>>,
    clock: u64,
    tick_count: u64,
}

impl Rig {
    fn new(musicians: usize, listeners: usize) -> Self {
        let ids: Vec<MemberId> = (0..musicians).map(|i| MemberId(i as u16)).collect();
        let faders: HashMap<MemberId, Fader> = ids
            .iter()
            .enumerate()
            .map(|(i, &id)| {
                (
                    id,
                    Fader {
                        gain_db: -3.0 + i as f32 * 0.25,
                        pan: -0.6 + i as f32 * 0.1,
                        muted: false,
                    },
                )
            })
            .collect();

        let mut uplink_enc =
            Encoder::new(Channels::Mono, FrameDuration::Ms2_5, UPLINK_BITRATE).expect("encoder");
        let pcm = signal(TICK_SAMPLES * 64, 3);
        let uplink: Vec<Vec<u8>> = pcm
            .chunks_exact(TICK_SAMPLES)
            .map(|chunk| {
                let mut out = Vec::new();
                uplink_enc.encode(chunk, &mut out).expect("encode");
                out
            })
            .collect();

        Self {
            musicians: ids
                .iter()
                .map(|&id| Musician {
                    id,
                    jitter: JitterBuffer::new(),
                    decoder: Decoder::new(Channels::Mono, FrameDuration::Ms2_5).expect("decoder"),
                    encoder: Encoder::new(
                        Channels::Stereo,
                        FrameDuration::Ms2_5,
                        PERSONAL_MIX_BITRATE,
                    )
                    .expect("encoder"),
                    session: server_session(id),
                    faders: faders.clone(),
                    send_seq: 0,
                    uplink_seq: 0,
                })
                .collect(),
            listeners: (0..listeners)
                .map(|i| {
                    let id = MemberId(1_000 + i as u16);
                    Listener {
                        id,
                        session: server_session(id),
                        send_seq: 0,
                    }
                })
                .collect(),
            bcast_faders: faders,
            bcast_encoder: Encoder::new(Channels::Stereo, FrameDuration::Ms20, BROADCAST_BITRATE)
                .expect("encoder"),
            bcast_accum: vec![0.0; BCAST_LEN],
            bcast_pkt: Vec::new(),
            limiter: Limiter::new(LIMITER_CEILING_DB, LIMITER_LOOKAHEAD_SAMPLES),
            metronome: Metronome {
                bpm: 120,
                beats_per_bar: 4,
            },
            decoded: Vec::new(),
            mix_buf: vec![0.0; MIX_LEN],
            pkt_buf: Vec::new(),
            uplink,
            out: Vec::new(),
            clock: 0,
            tick_count: 0,
        }
    }

    /// Arrival of one uplink packet per musician. Outside the tick because
    /// the server does it on the socket path, not the mix path.
    fn deliver_uplinks(&mut self) {
        for m in self.musicians.iter_mut() {
            let payload = self.uplink[m.uplink_seq as usize % self.uplink.len()].clone();
            m.jitter.push(MediaPacket {
                seq: m.uplink_seq,
                timestamp: u64::from(m.uplink_seq) * TICK_SAMPLES as u64,
                payload,
                redundant: None,
            });
            m.uplink_seq = m.uplink_seq.wrapping_add(1);
        }
    }

    fn tick(&mut self) {
        let clock = self.clock;
        self.clock += TICK_SAMPLES as u64;
        self.out.clear();

        self.decoded.clear();
        for m in self.musicians.iter_mut() {
            let mut pcm = [0.0f32; TICK_SAMPLES];
            let pulled = m.jitter.pull();
            let result = match &pulled {
                Pull::Frame(p) | Pull::Recovered(p) => m.decoder.decode(Some(p), &mut pcm, false),
                Pull::Missing => m.decoder.decode(None, &mut pcm, false),
                Pull::Waiting => Ok(()),
            };
            if result.is_err() {
                pcm = [0.0; TICK_SAMPLES];
            }
            self.decoded.push((m.id, pcm));
        }

        let mut click = [0.0f32; TICK_SAMPLES];
        self.metronome.render(clock, &mut click, CLICK_GAIN);

        let sources: Vec<(MemberId, &[f32])> =
            self.decoded.iter().map(|(id, b)| (*id, &b[..])).collect();

        let idx = (self.tick_count % BCAST_TICKS) as usize;
        {
            let faders = &self.bcast_faders;
            let slot = &mut self.bcast_accum[idx * MIX_LEN..(idx + 1) * MIX_LEN];
            mix_into(
                &sources,
                |t| faders.get(&t).copied().unwrap_or_default(),
                None,
                slot,
            );
            self.limiter.process(slot);
        }

        for m in self.musicians.iter_mut() {
            mix_into(
                &sources,
                |t| m.faders.get(&t).copied().unwrap_or_default(),
                Some(m.id),
                &mut self.mix_buf,
            );
            for (pair, &c) in self.mix_buf.chunks_exact_mut(2).zip(click.iter()) {
                pair[0] += c;
                pair[1] += c;
            }
            if m.encoder.encode(&self.mix_buf, &mut self.pkt_buf).is_ok() {
                let frame = MediaFrame {
                    seq: m.send_seq,
                    timestamp: clock,
                    duration: FrameDuration::Ms2_5,
                    stereo: true,
                    payload: &self.pkt_buf,
                    redundant: None,
                }
                .encode();
                m.send_seq = m.send_seq.wrapping_add(1);
                if let Ok(dg) = m.session.seal(m.id, &frame) {
                    self.out.push(dg);
                }
            }
        }

        if idx as u64 == BCAST_TICKS - 1
            && !self.listeners.is_empty()
            && self
                .bcast_encoder
                .encode(&self.bcast_accum, &mut self.bcast_pkt)
                .is_ok()
        {
            for l in self.listeners.iter_mut() {
                let frame = MediaFrame {
                    seq: l.send_seq,
                    timestamp: clock,
                    duration: FrameDuration::Ms20,
                    stereo: true,
                    payload: &self.bcast_pkt,
                    redundant: None,
                }
                .encode();
                l.send_seq = l.send_seq.wrapping_add(1);
                if let Ok(dg) = l.session.seal(l.id, &frame) {
                    self.out.push(dg);
                }
            }
        }

        self.tick_count += 1;
        black_box(&self.out);
    }
}

/// Warms the codecs, the limiter and the jitter buffers into steady state.
/// The first ticks of a session are not the ticks the budget is about.
fn warm(rig: &mut Rig, ticks: usize) {
    for _ in 0..ticks {
        rig.deliver_uplinks();
        rig.tick();
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

fn print_budget_table() {
    const WARMUP: usize = 400;
    const SAMPLES: usize = 4_000;

    println!();
    println!("mix tick against the 2500 us budget, {SAMPLES} ticks per row");
    println!("one uplink frame per musician per tick, no loss; the 20 ms broadcast");
    println!("frame is encoded on one tick in eight, which is what p90 and above show");
    println!();
    println!(
        "{:>4} {:>4} {:>9} {:>9} {:>9} {:>9} {:>9} {:>8} {:>8}",
        "musn", "lstn", "min", "p50", "p90", "p99", "max", "p50 %", "p99 %"
    );

    for musicians in [1usize, 2, 4, 6, 8, 10, 16, 20] {
        for listeners in [0usize, 2] {
            let mut rig = Rig::new(musicians, listeners);
            warm(&mut rig, WARMUP);
            let mut samples = Vec::with_capacity(SAMPLES);
            for _ in 0..SAMPLES {
                rig.deliver_uplinks();
                let start = Instant::now();
                rig.tick();
                samples.push(start.elapsed());
            }
            samples.sort_unstable();
            let us = |d: Duration| d.as_secs_f64() * 1e6;
            let pct = |d: Duration| 100.0 * d.as_secs_f64() / BUDGET.as_secs_f64();
            println!(
                "{musicians:>4} {listeners:>4} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>7.1}% {:>7.1}%",
                us(samples[0]),
                us(percentile(&samples, 0.50)),
                us(percentile(&samples, 0.90)),
                us(percentile(&samples, 0.99)),
                us(*samples.last().expect("samples")),
                pct(percentile(&samples, 0.50)),
                pct(percentile(&samples, 0.99)),
            );
        }
    }
    println!();
}

fn tick_benches(c: &mut Criterion) {
    let mut g = c.benchmark_group("tick");
    // Criterion's mean folds the broadcast tick into the other seven, which
    // is the right number for total CPU and the wrong one for the deadline.
    // The table above is where the deadline question is answered.
    for (musicians, listeners) in [(1usize, 0usize), (4, 0), (10, 0), (10, 2), (20, 2)] {
        let mut rig = Rig::new(musicians, listeners);
        warm(&mut rig, 400);
        // iter_custom, not iter: packet arrival is the socket path's work,
        // not the mix path's, so it happens between measurements exactly as
        // it does in the table above.
        g.bench_function(
            format!("{musicians}_musicians_{listeners}_listeners"),
            |b| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        rig.deliver_uplinks();
                        let start = Instant::now();
                        rig.tick();
                        total += start.elapsed();
                    }
                    total
                })
            },
        );
    }
    g.finish();
}

fn main() {
    print_budget_table();
    let mut c = Criterion::default().configure_from_args();
    tick_benches(&mut c);
    c.final_summary();
}
