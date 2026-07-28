//! Recording against a real session core: two musicians over the real
//! handshake and codec path, the recorder tapping the same post-limiter
//! broadcast accumulator the stream pipeline reads, files decoded afterwards
//! with an implementation the encoder does not share a line with.

use std::net::SocketAddr;

use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_server::flac::to_i16;
use jamstream_server::record::{DiskSink, RecordPayload, Recorder, RecordingState};
use jamstream_session::{ClientCore, ServerConfig, ServerCore};

const STEP_MS: f64 = 2.5;
// 2026-07-28 19:30:05 UTC.
const STAMP: u64 = 1_785_267_005;

struct Member {
    addr: SocketAddr,
    core: ClientCore,
    tone_hz: f32,
    frames: u64,
}

struct Rig {
    issuer: Issuer,
    server_pk: [u8; 32],
    session_id: SessionId,
    server: ServerCore,
    members: Vec<Member>,
    t: f64,
}

impl Rig {
    fn new(tones: &[f32]) -> Rig {
        let issuer = Issuer::generate();
        let kp = generate_keypair();
        let session_id = SessionId::generate();
        let server = ServerCore::new(ServerConfig::new(
            session_id,
            kp.private.to_vec(),
            kp.public,
            issuer.public_key(),
        ));
        let mut rig = Rig {
            issuer,
            server_pk: kp.public,
            session_id,
            server,
            members: Vec::new(),
            t: 0.0,
        };
        for &hz in tones {
            rig.add_member(hz);
        }
        rig
    }

    fn add_member(&mut self, hz: f32) {
        let i = self.members.len();
        let addr: SocketAddr = format!("10.0.0.{}:5000", 10 + i).parse().unwrap();
        let invite: Invite = self.issuer.mint(
            self.session_id,
            vec![addr],
            self.server_pk,
            Token {
                member_id: MemberId(i as u16),
                role: Role::Musician,
                name_hint: None,
                expires_unix: u64::MAX,
                jti: TokenId::generate(),
            },
        );
        let now = self.t as u64;
        let (core, first) = ClientCore::connect(&invite, now).unwrap();
        let out = self.server.handle_datagram(now, 1_000, addr, &first);
        let mut member = Member {
            addr,
            core,
            tone_hz: hz,
            frames: 0,
        };
        for (_, dg) in out {
            for reply in member.core.handle_datagram(now, &dg) {
                self.server.handle_datagram(now, 1_000, addr, &reply);
            }
        }
        self.members.push(member);
    }

    /// One 2.5 ms step of the whole rig; the recorder tap is fed by the
    /// caller right after this returns, exactly as jamstreamd would.
    fn step(&mut self) {
        let now = self.t as u64;
        let mut to_server: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
        for m in &mut self.members {
            let mut pcm = [0.0f32; 120];
            for (j, s) in pcm.iter_mut().enumerate() {
                let n = (m.frames * 120 + j as u64) as f32;
                *s = (std::f32::consts::TAU * m.tone_hz * n / 48_000.0).sin() * 0.5;
            }
            m.frames += 1;
            for dg in m.core.push_capture(now, &pcm) {
                to_server.push((m.addr, dg));
            }
            for dg in m.core.poll(now) {
                to_server.push((m.addr, dg));
            }
        }
        let mut to_members = Vec::new();
        for (src, dg) in to_server {
            to_members.extend(self.server.handle_datagram(now, 1_000, src, &dg));
        }
        to_members.extend(self.server.tick(now));
        for (dst, dg) in to_members {
            if let Some(m) = self.members.iter_mut().find(|m| m.addr == dst) {
                for reply in m.core.handle_datagram(now, &dg) {
                    self.server.handle_datagram(now, 1_000, m.addr, &reply);
                }
            }
        }
        self.t += STEP_MS;
    }

    /// This tick's recorder payload, from the same accessors jamstreamd taps.
    fn capture(&self) -> RecordPayload {
        let mut payload = RecordPayload::default();
        payload
            .mix
            .copy_from_slice(self.server.broadcast_tick().audio);
        for stem in self.server.stems() {
            payload.push_stem(stem.id, stem.fader, stem.pcm);
        }
        payload
    }
}

fn decode(path: &std::path::Path) -> Vec<i32> {
    let mut reader = claxon::FlacReader::open(path).unwrap();
    reader.samples().map(|s| s.unwrap()).collect()
}

fn rms(samples: &[i32]) -> f64 {
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / samples.len() as f64).sqrt() / f64::from(i16::MAX)
}

#[test]
fn a_take_captures_the_broadcast_mix_and_aligned_stems() {
    let dir = std::env::temp_dir().join(format!("jamstream-take-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut rig = Rig::new(&[440.0, 660.0]);
    // Let the handshakes settle and the jitter buffers fill.
    for _ in 0..400 {
        rig.step();
    }
    assert_eq!(rig.server.musicians_connected(), 2);

    let mut rec = Recorder::new(DiskSink::new(&dir));
    rec.start(
        STAMP,
        Some(vec![
            (MemberId(0), "Ana".to_owned()),
            (MemberId(1), "Bo".to_owned()),
        ]),
    );
    let mut expected_mix: Vec<i32> = Vec::new();
    let ticks = 800; // 2 s
    for _ in 0..ticks {
        rig.step();
        let payload = rig.capture();
        expected_mix.extend(payload.mix.iter().map(|&s| to_i16(s)));
        rec.tick(&payload);
    }
    rec.stop();
    assert_eq!(rec.state(), &RecordingState::Idle);

    // The mix file is bit-exact against what the broadcast accumulator held:
    // the recording is what listeners heard, decoded by claxon, not flacenc.
    let mix = decode(&dir.join("jamstream-2026-07-28-1930-mix.flac"));
    assert_eq!(mix, expected_mix);
    assert!(rms(&mix) > 0.05, "the take is silent: rms {}", rms(&mix));

    // Both stems exist, run the full length of the take, and carry their
    // member: two tones in the mix, one tone each in the stems.
    let ana = decode(&dir.join("jamstream-2026-07-28-1930-Ana.flac"));
    let bo = decode(&dir.join("jamstream-2026-07-28-1930-Bo.flac"));
    assert_eq!(ana.len(), mix.len());
    assert_eq!(bo.len(), mix.len());
    assert!(rms(&ana) > 0.05 && rms(&bo) > 0.05);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_member_joining_mid_take_gets_a_stem_aligned_from_the_start() {
    let dir = std::env::temp_dir().join(format!("jamstream-late-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // One musician plays alone; the second joins after the take starts.
    let mut rig = Rig::new(&[440.0]);
    for _ in 0..400 {
        rig.step();
    }
    let mut rec = Recorder::new(DiskSink::new(&dir));
    rec.start(STAMP, Some(vec![(MemberId(0), "Ana".to_owned())]));
    for _ in 0..200 {
        rig.step();
        rec.tick(&rig.capture());
    }
    // The late member is unknown to the roster the take started with, so
    // their stem falls back to their id.
    rig.add_member(660.0);
    for _ in 0..400 {
        rig.step();
        rec.tick(&rig.capture());
    }
    rec.stop();

    let mix = decode(&dir.join("jamstream-2026-07-28-1930-mix.flac"));
    let late = decode(&dir.join("jamstream-2026-07-28-1930-member-1.flac"));
    assert_eq!(late.len(), mix.len(), "a late stem must be backfilled");
    // Silent until they joined, audible after.
    let head = &late[..200 * 240];
    let tail = &late[late.len() - 100 * 240..];
    assert!(head.iter().all(|&s| s == 0), "backfill was not silence");
    assert!(rms(tail) > 0.05, "late member missing from their stem");

    let _ = std::fs::remove_dir_all(&dir);
}
