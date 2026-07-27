//! In-memory loopback of ServerCore against several ClientCores: a tiny
//! shuttle pumps datagrams between fixed fake addresses while virtual time
//! advances in 2.5 ms steps. No sockets, no threads, no real clock.

use std::net::SocketAddr;

use blake2::{Blake2s256, Digest};
use jamstream_protocol::control::{
    AVATAR_CHUNK_BYTES, ControlLink, ControlMsg, MAX_AVATAR_BYTES, MemberInfo,
};
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::media::{FrameDuration, MediaFrame};
use jamstream_protocol::transport::{Initiator, Session, generate_keypair};
use jamstream_protocol::wire::{self, Packet};
use jamstream_session::{
    ClientCore, ClientEvent, ClientState, ServerConfig, ServerCore, ServerEvent,
};
use proptest::prelude::*;

const STEP_MS: f64 = 2.5;

/// Datagrams at or above this size are counted as avatar chunk traffic by
/// the shuttle: an AvatarChunk seals to a bit over its 8 KB payload while
/// every other datagram (media, rosters, chat) stays under ~1.5 KB, so the
/// count observes chunk transfers without unsealing anything.
/// A sealed avatar chunk is the only control datagram that comes near a
/// kilobyte; media frames and every other control message are far smaller,
/// so this cleanly separates "an avatar moved" from ordinary traffic.
const BIG_DGRAM_BYTES: usize = 900;

fn addr_of(n: u8) -> SocketAddr {
    format!("10.0.0.{n}:5000").parse().unwrap()
}

struct TestClient {
    addr: SocketAddr,
    core: ClientCore,
    role: Role,
    /// Some(hz): push a sine at this frequency every step (0.0 = silence).
    tone_hz: Option<f32>,
    frames_pushed: u64,
    playout: Vec<f32>,
    blocked: bool,
    /// Some(n): drop every nth client-to-server MEDIA datagram; control and
    /// handshake traffic passes. Models a lossy uplink the client cannot
    /// observe from its own downlink.
    drop_uplink_media_nth: Option<u64>,
    uplink_media_seen: u64,
    /// Deliver uplink media in bursts of two every other step. The
    /// interarrival jitter grows the server's buffer target, which is what
    /// makes a piggybacked redundant copy arrive in time to be usable.
    uplink_media_stutter: bool,
    stutter_queue: Vec<Vec<u8>>,
    stutter_step: u64,
    /// Some(n): drop every nth server-to-client datagram of any kind; the
    /// reliable control layer recovers, media does not.
    drop_downlink_nth: Option<u64>,
    downlink_seen: u64,
    events: Vec<ClientEvent>,
}

struct Harness {
    issuer: Issuer,
    server_pk: [u8; 32],
    session_id: SessionId,
    server: ServerCore,
    clients: Vec<TestClient>,
    t: f64,
    now_unix: u64,
    to_server: Vec<(SocketAddr, Vec<u8>)>,
    server_events: Vec<ServerEvent>,
    /// Datagrams >= BIG_DGRAM_BYTES shuttled in either direction.
    big_dgrams: u64,
}

impl Harness {
    fn new(max_musicians: usize, max_listeners: usize) -> Self {
        let issuer = Issuer::generate();
        let kp = generate_keypair();
        let session_id = SessionId::generate();
        let server = ServerCore::new(ServerConfig {
            session_id,
            server_private: kp.private.to_vec(),
            server_public: kp.public,
            issuer_pk: issuer.public_key(),
            max_musicians,
            max_listeners,
            member_timeout_ms: 10_000,
        });
        Self {
            issuer,
            server_pk: kp.public,
            session_id,
            server,
            clients: Vec::new(),
            t: 0.0,
            now_unix: 1_000,
            to_server: Vec::new(),
            server_events: Vec::new(),
            big_dgrams: 0,
        }
    }

    fn now_ms(&self) -> u64 {
        self.t as u64
    }

    fn mint(&self, member: u16, role: Role) -> Invite {
        self.issuer.mint(
            self.session_id,
            vec![addr_of(1)],
            self.server_pk,
            Token {
                member_id: MemberId(member),
                role,
                name_hint: None,
                expires_unix: u64::MAX,
                jti: TokenId::generate(),
            },
        )
    }

    fn add_client(&mut self, invite: &Invite, tone_hz: Option<f32>) -> usize {
        let idx = self.clients.len();
        let addr = addr_of(10 + idx as u8);
        let (core, first) = ClientCore::connect(invite, self.now_ms()).unwrap();
        self.to_server.push((addr, first));
        self.clients.push(TestClient {
            addr,
            core,
            role: invite.token.role,
            tone_hz,
            frames_pushed: 0,
            playout: Vec::new(),
            blocked: false,
            drop_uplink_media_nth: None,
            uplink_media_seen: 0,
            uplink_media_stutter: false,
            stutter_queue: Vec::new(),
            stutter_step: 0,
            drop_downlink_nth: None,
            downlink_seen: 0,
            events: Vec::new(),
        });
        idx
    }

    /// One 2.5 ms step: capture and poll every client, deliver to the
    /// server, tick, deliver back, then pull playout.
    fn step(&mut self) {
        let now = self.now_ms();
        for i in 0..self.clients.len() {
            let c = &mut self.clients[i];
            let mut dgs: Vec<Vec<u8>> = Vec::new();
            if c.role == Role::Musician
                && let Some(hz) = c.tone_hz
            {
                let mut pcm = [0.0f32; 120];
                for (j, s) in pcm.iter_mut().enumerate() {
                    let n = (c.frames_pushed * 120 + j as u64) as f32;
                    *s = (std::f32::consts::TAU * hz * n / 48_000.0).sin() * 0.5;
                }
                c.frames_pushed += 1;
                for d in c.core.push_capture(now, &pcm) {
                    c.uplink_media_seen += 1;
                    let dropped = c
                        .drop_uplink_media_nth
                        .is_some_and(|n| c.uplink_media_seen % n == 0);
                    if dropped {
                        continue;
                    }
                    if c.uplink_media_stutter {
                        c.stutter_queue.push(d);
                    } else {
                        dgs.push(d);
                    }
                }
                c.stutter_step += 1;
                if c.stutter_step % 2 == 0 {
                    dgs.append(&mut c.stutter_queue);
                }
            }
            dgs.extend(c.core.poll(now));
            let (addr, blocked) = (c.addr, c.blocked);
            if !blocked {
                self.to_server.extend(dgs.into_iter().map(|d| (addr, d)));
            }
        }

        let batch = std::mem::take(&mut self.to_server);
        let mut to_clients = Vec::new();
        for (src, dg) in batch {
            if dg.len() >= BIG_DGRAM_BYTES {
                self.big_dgrams += 1;
            }
            to_clients.extend(self.server.handle_datagram(now, self.now_unix, src, &dg));
        }
        to_clients.extend(self.server.tick(now));
        self.server_events.extend(self.server.events());

        for (addr, dg) in to_clients {
            if dg.len() >= BIG_DGRAM_BYTES {
                self.big_dgrams += 1;
            }
            let Some(i) = self.clients.iter().position(|c| c.addr == addr) else {
                continue;
            };
            let c = &mut self.clients[i];
            if c.blocked {
                continue;
            }
            c.downlink_seen += 1;
            if c.drop_downlink_nth
                .is_some_and(|n| c.downlink_seen % n == 0)
            {
                continue;
            }
            let replies = c.core.handle_datagram(now, &dg);
            self.to_server
                .extend(replies.into_iter().map(|d| (addr, d)));
        }

        for c in &mut self.clients {
            let mut buf = [0.0f32; 240];
            c.core.pull_playout(&mut buf);
            c.playout.extend_from_slice(&buf);
            c.events.extend(c.core.events());
        }
        self.t += STEP_MS;
    }

    fn run(&mut self, steps: usize) {
        for _ in 0..steps {
            self.step();
        }
    }

    fn run_ms(&mut self, ms: u64) {
        self.run((ms as f64 / STEP_MS) as usize);
    }

    /// Coarse 100 ms hops for timeout scenarios: control keepalives flow,
    /// no capture or playout. Legal because the cores take time as input.
    fn advance_quiet(&mut self, ms: u64) {
        for _ in 0..ms / 100 {
            self.t += 100.0;
            let now = self.now_ms();
            for i in 0..self.clients.len() {
                let c = &mut self.clients[i];
                let dgs = c.core.poll(now);
                let (addr, blocked) = (c.addr, c.blocked);
                if !blocked {
                    self.to_server.extend(dgs.into_iter().map(|d| (addr, d)));
                }
            }
            let batch = std::mem::take(&mut self.to_server);
            let mut to_clients = Vec::new();
            for (src, dg) in batch {
                if dg.len() >= BIG_DGRAM_BYTES {
                    self.big_dgrams += 1;
                }
                to_clients.extend(self.server.handle_datagram(now, self.now_unix, src, &dg));
            }
            to_clients.extend(self.server.tick(now));
            self.server_events.extend(self.server.events());
            for (addr, dg) in to_clients {
                if dg.len() >= BIG_DGRAM_BYTES {
                    self.big_dgrams += 1;
                }
                let Some(i) = self.clients.iter().position(|c| c.addr == addr) else {
                    continue;
                };
                if self.clients[i].blocked {
                    continue;
                }
                let replies = self.clients[i].core.handle_datagram(now, &dg);
                self.to_server
                    .extend(replies.into_iter().map(|d| (addr, d)));
            }
            for c in &mut self.clients {
                c.events.extend(c.core.events());
            }
        }
    }

    fn clear_playouts(&mut self) {
        for c in &mut self.clients {
            c.playout.clear();
        }
    }

    fn last_roster(&self, i: usize) -> Option<&Vec<MemberInfo>> {
        self.clients[i].events.iter().rev().find_map(|e| match e {
            ClientEvent::Roster(r) => Some(r),
            _ => None,
        })
    }
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

fn tail_rms(h: &Harness, i: usize, samples: usize) -> f32 {
    let p = &h.clients[i].playout;
    rms(&p[p.len().saturating_sub(samples)..])
}

/// Goertzel amplitude of one tone in an interleaved stereo window,
/// mono-summed. A center-panned sine pushed at amplitude a reads ~a * 0.71
/// (the constant-power pan weight); absent tones read near zero, so a 0.1
/// present / 0.02 absent split discriminates cleanly.
fn tone_amp(stereo: &[f32], hz: f32) -> f32 {
    let n = stereo.len() / 2;
    assert!(n > 0, "empty tone window");
    let w = std::f32::consts::TAU * hz / 48_000.0;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for pair in stereo.chunks_exact(2) {
        let x = 0.5 * (pair[0] + pair[1]);
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0);
    2.0 * power.sqrt() / n as f32
}

fn tail_tone(h: &Harness, i: usize, samples: usize, hz: f32) -> f32 {
    let p = &h.clients[i].playout;
    tone_amp(&p[p.len().saturating_sub(samples)..], hz)
}

/// A protocol-level member driven directly, bypassing ClientCore, for
/// crafting traffic an honest client cannot produce.
struct RawMember {
    id: MemberId,
    addr: SocketAddr,
    session: Session,
    link: ControlLink,
}

fn raw_join(h: &mut Harness, invite: &Invite, addr: SocketAddr) -> RawMember {
    let (init, pkt) = Initiator::new(invite).unwrap();
    let now = h.now_ms();
    let replies = h.server.handle_datagram(now, h.now_unix, addr, &pkt);
    let (_, resp) = replies
        .into_iter()
        .find(|(a, _)| *a == addr)
        .expect("handshake response");
    let Packet::HandshakeResp { noise } = wire::parse(&resp).unwrap() else {
        panic!("expected handshake response");
    };
    let (session, welcome) = init.finish(noise).unwrap();
    RawMember {
        id: welcome.member_id,
        addr,
        session,
        link: ControlLink::new(),
    }
}

impl RawMember {
    fn send_control(&mut self, h: &mut Harness, msg: ControlMsg) {
        self.link.send(msg).unwrap();
        let now = h.now_ms();
        for dg in self.link.poll(now) {
            let sealed = self.session.seal(self.id, &dg).unwrap();
            // Replies (acks) are dropped; this member never listens.
            let _ = h
                .server
                .handle_datagram(now, h.now_unix, self.addr, &sealed);
        }
    }

    fn send_media(&mut self, h: &mut Harness, frame: &[u8]) {
        let sealed = self.session.seal(self.id, frame).unwrap();
        let now = h.now_ms();
        let _ = h
            .server
            .handle_datagram(now, h.now_unix, self.addr, &sealed);
    }
}

#[test]
fn three_musicians_join_and_roster() {
    let mut h = Harness::new(10, 20);
    for id in 0..3u16 {
        let inv = h.mint(id, Role::Musician);
        h.add_client(&inv, Some(0.0));
    }
    h.run_ms(1_500);

    assert_eq!(h.server.musicians_connected(), 3);
    for i in 0..3 {
        assert_eq!(*h.clients[i].core.state(), ClientState::Joined);
        assert!(h.clients[i].events.contains(&ClientEvent::Joined));
        let roster = h.last_roster(i).expect("roster event");
        assert_eq!(roster.len(), 3);
        assert!(roster.iter().all(|m| m.connected));
        assert_eq!(
            roster.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![MemberId(0), MemberId(1), MemberId(2)]
        );
    }
    let joined = h
        .server_events
        .iter()
        .filter(|e| matches!(e, ServerEvent::MemberJoined { .. }))
        .count();
    assert_eq!(joined, 3);
    assert!(h.server_events.contains(&ServerEvent::MemberJoined {
        id: MemberId(0),
        name: "member 0".into()
    }));
    assert!(
        h.server_events
            .contains(&ServerEvent::MusicianCountChanged(3))
    );
    // Keepalive pings produced RTT samples on both sides.
    assert!(
        h.clients[0]
            .events
            .iter()
            .any(|e| matches!(e, ClientEvent::RttSample { .. }))
    );
    assert!(
        h.server
            .stats()
            .iter()
            .all(|s| s.connected && s.violations == 0)
    );
}

#[test]
fn audio_flows_and_excludes_self() {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_c = h.mint(2, Role::Musician);
    let a = h.add_client(&inv_a, Some(440.0));
    let b = h.add_client(&inv_b, Some(0.0));
    let c = h.add_client(&inv_c, Some(0.0));
    h.run_ms(1_000);

    let win = 48_000; // last 0.5 s of interleaved stereo
    assert!(
        tail_rms(&h, b, win) > 0.02,
        "B should hear A's tone, rms {}",
        tail_rms(&h, b, win)
    );
    assert!(tail_rms(&h, c, win) > 0.02);
    // Minus-self: A's personal mix carries only B and C, who push silence.
    assert!(
        tail_rms(&h, a, win) < 5e-3,
        "A's mix must exclude A, rms {}",
        tail_rms(&h, a, win)
    );
}

#[test]
fn fader_mute_applies_per_member() {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_c = h.mint(2, Role::Musician);
    let _a = h.add_client(&inv_a, Some(440.0));
    let b = h.add_client(&inv_b, Some(0.0));
    let c = h.add_client(&inv_c, Some(0.0));
    h.run_ms(500);

    h.clients[b]
        .core
        .set_fader(MemberId(0), 0.0, 0.0, true)
        .unwrap();
    h.run_ms(250); // propagate
    h.clear_playouts();
    h.run_ms(1_000);

    let win = 48_000;
    assert!(
        tail_rms(&h, b, win) < 5e-3,
        "B muted A, rms {}",
        tail_rms(&h, b, win)
    );
    assert!(
        tail_rms(&h, c, win) > 0.02,
        "C still hears A, rms {}",
        tail_rms(&h, c, win)
    );
}

#[test]
fn chat_from_field_is_forced_by_server() {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    h.add_client(&inv_a, Some(0.0));
    h.add_client(&inv_b, Some(0.0));
    h.run_ms(500);

    h.clients[0].core.send_chat("hello").unwrap();
    h.run_ms(100);
    for i in 0..2 {
        assert!(h.clients[i].events.contains(&ClientEvent::Chat {
            from: MemberId(0),
            text: "hello".into()
        }));
    }

    // A raw member (id 3) lies in the from field; receivers must see the
    // authenticated sender id instead.
    let inv = h.mint(3, Role::Musician);
    let mut raw = raw_join(&mut h, &inv, addr_of(99));
    raw.send_control(
        &mut h,
        ControlMsg::Chat {
            from: MemberId(0),
            text: "spoof".into(),
        },
    );
    h.run_ms(100);
    for i in 0..2 {
        assert!(h.clients[i].events.contains(&ClientEvent::Chat {
            from: MemberId(3),
            text: "spoof".into()
        }));
        assert!(!h.clients[i].events.contains(&ClientEvent::Chat {
            from: MemberId(0),
            text: "spoof".into()
        }));
    }
}

#[test]
fn metronome_host_controls_and_clicks() {
    let mut h = Harness::new(10, 20);
    for id in 0..3u16 {
        let inv = h.mint(id, Role::Musician);
        h.add_client(&inv, Some(0.0));
    }
    h.run_ms(500);

    h.clients[0].core.set_metronome(120, 4, true).unwrap();
    h.run_ms(250);
    for i in 0..3 {
        assert!(
            h.clients[i]
                .events
                .contains(&ClientEvent::MetronomeChanged {
                    bpm: 120,
                    beats_per_bar: 4,
                    enabled: true
                }),
            "client {i} missed the metronome change"
        );
    }

    h.clear_playouts();
    h.run_ms(1_500);
    let win = 96_000; // last second
    for i in 0..3 {
        let r = tail_rms(&h, i, win);
        assert!(r > 0.005, "client {i} click rms {r}");
        // Bursty, not continuous: clicks live at beat positions.
        let p = &h.clients[i].playout;
        let tail = &p[p.len() - win..];
        let loud = tail.chunks(240).filter(|b| rms(b) > 0.02).count();
        let total = tail.chunks(240).count();
        assert!(
            loud > 0 && loud * 3 < total,
            "client {i}: clicks should be sparse, {loud}/{total} loud blocks"
        );
    }

    // Non-host MetronomeSet is ignored: no state change reaches anyone.
    for c in &mut h.clients {
        c.events.clear();
    }
    h.clients[2].core.set_metronome(240, 3, true).unwrap();
    h.run_ms(250);
    assert!(
        h.clients[0]
            .events
            .iter()
            .all(|e| !matches!(e, ClientEvent::MetronomeChanged { .. }))
    );
    assert!(h.server_events.iter().any(|e| matches!(
        e,
        ServerEvent::ProtocolViolation {
            id: MemberId(2),
            what: "metronome set by non-host"
        }
    )));
}

#[test]
fn listener_receives_broadcast_and_cannot_send_media() {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_l = h.mint(5, Role::Listener);
    h.add_client(&inv_a, Some(440.0));
    h.add_client(&inv_b, Some(0.0));
    let l = h.add_client(&inv_l, None);
    h.run_ms(2_000);

    assert_eq!(*h.clients[l].core.state(), ClientState::Joined);
    assert!(
        tail_rms(&h, l, 48_000) > 0.02,
        "listener broadcast rms {}",
        tail_rms(&h, l, 48_000)
    );

    // A raw listener pushing media is a protocol violation, not a crash.
    let inv6 = h.mint(6, Role::Listener);
    let mut raw = raw_join(&mut h, &inv6, addr_of(98));
    let frame = MediaFrame {
        seq: 0,
        timestamp: 0,
        duration: FrameDuration::Ms2_5,
        stereo: false,
        payload: &[1, 2, 3],
        redundant: None,
    }
    .encode();
    raw.send_media(&mut h, &frame);
    h.run(1);
    assert!(h.server_events.iter().any(|e| matches!(
        e,
        ServerEvent::ProtocolViolation {
            id: MemberId(6),
            what: "media from listener"
        }
    )));
}

#[test]
fn broadcast_fader_mute_reaches_listeners_not_monitors() {
    let mut h = Harness::new(10, 20);
    let inv_host = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_c = h.mint(2, Role::Musician);
    let inv_l = h.mint(5, Role::Listener);
    let host = h.add_client(&inv_host, Some(440.0));
    h.add_client(&inv_b, Some(660.0));
    let c = h.add_client(&inv_c, Some(0.0));
    let l = h.add_client(&inv_l, None);
    h.run_ms(2_000);

    let win = 48_000; // last 0.5 s
    assert!(
        tail_tone(&h, l, win, 440.0) > 0.1 && tail_tone(&h, l, win, 660.0) > 0.1,
        "listener should hear both tones before the mute: 440 {} 660 {}",
        tail_tone(&h, l, win, 440.0),
        tail_tone(&h, l, win, 660.0)
    );

    h.clients[host]
        .core
        .set_broadcast_fader(MemberId(1), 0.0, 0.0, true)
        .unwrap();
    h.run_ms(250); // propagate
    h.clear_playouts();
    h.run_ms(1_000);

    // The listener lost B's tone and kept the host's.
    assert!(
        tail_tone(&h, l, win, 440.0) > 0.1,
        "host tone gone from broadcast: {}",
        tail_tone(&h, l, win, 440.0)
    );
    assert!(
        tail_tone(&h, l, win, 660.0) < 0.02,
        "B still audible in broadcast: {}",
        tail_tone(&h, l, win, 660.0)
    );
    // Personal mixes are untouched: C and the host still monitor B.
    assert!(
        tail_tone(&h, c, win, 660.0) > 0.1,
        "broadcast mute leaked into C's monitor: {}",
        tail_tone(&h, c, win, 660.0)
    );
    assert!(
        tail_tone(&h, host, win, 660.0) > 0.1,
        "broadcast mute leaked into the host's monitor: {}",
        tail_tone(&h, host, win, 660.0)
    );
}

#[test]
fn non_host_broadcast_controls_are_violations() {
    let mut h = Harness::new(10, 20);
    let inv_host = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_l = h.mint(5, Role::Listener);
    let host = h.add_client(&inv_host, Some(440.0));
    let b = h.add_client(&inv_b, Some(0.0));
    let l = h.add_client(&inv_l, None);
    h.run_ms(2_000);

    // B tries to mute the host in the broadcast and to audition it.
    h.clients[b]
        .core
        .set_broadcast_fader(MemberId(0), 0.0, 0.0, true)
        .unwrap();
    h.clients[b].core.set_broadcast_audition(true).unwrap();
    h.run_ms(250);

    for what in [
        "broadcast mix set by non-host",
        "broadcast audition by non-host",
    ] {
        assert!(
            h.server_events.iter().any(|e| matches!(
                e,
                ServerEvent::ProtocolViolation { id: MemberId(1), what: w } if *w == what
            )),
            "missing violation: {what}"
        );
    }
    // Nothing was relayed.
    for i in [host, b, l] {
        assert!(
            h.clients[i]
                .events
                .iter()
                .all(|e| !matches!(e, ClientEvent::BroadcastMixChanged { .. })),
            "client {i} received a relay for a refused change"
        );
    }

    // And nothing changed: the listener still hears the tone B tried to
    // mute, and the host still gets a minus-self personal mix.
    h.clear_playouts();
    h.run_ms(1_000);
    let win = 48_000;
    assert!(
        tail_tone(&h, l, win, 440.0) > 0.1,
        "non-host mute took effect: {}",
        tail_tone(&h, l, win, 440.0)
    );
    assert!(
        tail_tone(&h, host, win, 440.0) < 0.02,
        "non-host audition took effect: {}",
        tail_tone(&h, host, win, 440.0)
    );
}

#[test]
fn audition_swaps_host_playout_to_broadcast_and_back() {
    let mut h = Harness::new(10, 20);
    let inv_host = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let host = h.add_client(&inv_host, Some(440.0));
    h.add_client(&inv_b, Some(660.0));
    h.run_ms(1_000);

    let win = 48_000;
    // Personal mix: minus-self, so B's tone only.
    assert!(tail_tone(&h, host, win, 660.0) > 0.1);
    assert!(tail_tone(&h, host, win, 440.0) < 0.02);

    h.clients[host].core.set_broadcast_audition(true).unwrap();
    h.run_ms(250);
    h.clear_playouts();
    h.run_ms(1_000);
    // The broadcast mix includes the host's own signal; hearing what the
    // stream hears is the point of auditioning.
    assert!(
        tail_tone(&h, host, win, 440.0) > 0.1,
        "audition should carry the host's own tone: {}",
        tail_tone(&h, host, win, 440.0)
    );
    assert!(
        tail_tone(&h, host, win, 660.0) > 0.1,
        "audition should still carry B's tone: {}",
        tail_tone(&h, host, win, 660.0)
    );

    h.clients[host].core.set_broadcast_audition(false).unwrap();
    // Bounded settle: control delivery plus the host's jitter buffer
    // draining the last auditioned frames.
    h.run_ms(250);
    h.clear_playouts();
    h.run_ms(1_000);
    assert!(
        tail_tone(&h, host, win, 440.0) < 0.02,
        "minus-self not restored after audition off: {}",
        tail_tone(&h, host, win, 440.0)
    );
    assert!(tail_tone(&h, host, win, 660.0) > 0.1);
}

#[test]
fn broadcast_fader_changes_relay_to_all_members() {
    let mut h = Harness::new(10, 20);
    let inv_host = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_l = h.mint(5, Role::Listener);
    let host = h.add_client(&inv_host, Some(0.0));
    let b = h.add_client(&inv_b, Some(0.0));
    let l = h.add_client(&inv_l, None);
    h.run_ms(500);

    h.clients[host]
        .core
        .set_broadcast_fader(MemberId(1), -6.0, 0.25, false)
        .unwrap();
    h.run_ms(250);
    let expected = ClientEvent::BroadcastMixChanged {
        target: MemberId(1),
        gain_db: -6.0,
        pan: 0.25,
        muted: false,
    };
    for i in [host, b, l] {
        assert!(
            h.clients[i].events.contains(&expected),
            "client {i} missed the broadcast mix relay: {:?}",
            h.clients[i].events
        );
    }
}

#[test]
fn revoke_ejects_and_blocks_rejoin() {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_c = h.mint(2, Role::Musician);
    h.add_client(&inv_a, Some(0.0));
    let b = h.add_client(&inv_b, Some(0.0));
    h.add_client(&inv_c, Some(0.0));
    h.run_ms(500);
    assert_eq!(h.server.musicians_connected(), 3);

    h.clients[0].core.revoke(inv_b.token.jti).unwrap();
    h.run_ms(250);
    assert_eq!(
        *h.clients[b].core.state(),
        ClientState::Ejected {
            reason: "invite revoked".into()
        }
    );
    assert!(
        h.clients[b]
            .events
            .iter()
            .any(|e| matches!(e, ClientEvent::Ejected { .. }))
    );
    assert!(
        h.server_events
            .contains(&ServerEvent::MemberRevoked { id: MemberId(1) })
    );
    assert_eq!(h.server.musicians_connected(), 2);
    let roster = h.last_roster(0).expect("roster after revoke");
    assert!(roster.iter().all(|m| m.id != MemberId(1)));

    // The same token cannot come back: refusal is silent.
    let now = h.now_ms();
    let init = h.clients[b].core.reconnect(now).unwrap();
    let baddr = h.clients[b].addr;
    h.to_server.push((baddr, init));
    h.run_ms(1_000);
    assert_ne!(*h.clients[b].core.state(), ClientState::Joined);
    assert_eq!(h.server.musicians_connected(), 2);
}

#[test]
fn timeout_then_rejoin_with_same_token() {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_c = h.mint(2, Role::Musician);
    h.add_client(&inv_a, Some(0.0));
    let b = h.add_client(&inv_b, Some(0.0));
    let c = h.add_client(&inv_c, Some(0.0));
    h.run_ms(500);
    assert_eq!(h.server.musicians_connected(), 3);

    // B falls off the network for more than 10 s of virtual time.
    h.clients[b].blocked = true;
    h.advance_quiet(11_000);
    assert!(
        h.server_events
            .contains(&ServerEvent::MemberDisconnected { id: MemberId(1) })
    );
    assert_eq!(h.server.musicians_connected(), 2);
    assert_eq!(*h.clients[b].core.state(), ClientState::TimedOut);
    let roster = h.last_roster(0).expect("roster after timeout");
    assert!(roster.iter().any(|m| m.id == MemberId(1) && !m.connected));

    // Fresh handshake, same token: welcome back.
    h.clients[b].blocked = false;
    let now = h.now_ms();
    let init = h.clients[b].core.reconnect(now).unwrap();
    let baddr = h.clients[b].addr;
    h.to_server.push((baddr, init));
    h.run_ms(500);
    assert_eq!(*h.clients[b].core.state(), ClientState::Joined);
    assert_eq!(h.server.musicians_connected(), 3);
    let roster = h.last_roster(c).expect("roster after rejoin");
    assert_eq!(roster.len(), 3);
    assert!(roster.iter().all(|m| m.connected));
}

#[test]
fn version_reject_rate_limited_and_verified() {
    let mut h = Harness::new(10, 20);
    let src = addr_of(50);
    let fake_init = wire::build_handshake_init(3, &[0x5A; 64]);
    let now = h.now_ms();

    let out = h.server.handle_datagram(now, h.now_unix, src, &fake_init);
    assert_eq!(out.len(), 1);
    let Ok(Packet::VersionReject { ours, theirs, mac }) = wire::parse(&out[0].1) else {
        panic!("expected a version reject");
    };
    assert_eq!((ours, theirs), (1, 3));
    assert!(wire::verify_version_reject(
        &h.server_pk,
        ours,
        theirs,
        &mac,
        &fake_init
    ));
    // Same source inside the window: silence.
    assert!(
        h.server
            .handle_datagram(now + 500, h.now_unix, src, &fake_init)
            .is_empty()
    );
    // After the window: answered again.
    assert_eq!(
        h.server
            .handle_datagram(now + 1_500, h.now_unix, src, &fake_init)
            .len(),
        1
    );

    // A client presented with a forged reject (wrong MAC key) ignores it.
    let inv = h.mint(0, Role::Musician);
    let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
    let forged = wire::build_version_reject(&[0xEE; 32], 1, 1, &init);
    assert!(core.handle_datagram(1, &forged).is_empty());
    assert_eq!(*core.state(), ClientState::Connecting);
    assert!(core.events().is_empty());
}

#[test]
fn musician_capacity_enforced() {
    let mut h = Harness::new(10, 20);
    let invites: Vec<Invite> = (0..11u16).map(|i| h.mint(i, Role::Musician)).collect();
    for inv in &invites {
        h.add_client(inv, Some(0.0));
    }
    h.run_ms(500);

    assert_eq!(h.server.musicians_connected(), 10);
    let joined = h
        .clients
        .iter()
        .filter(|c| *c.core.state() == ClientState::Joined)
        .count();
    assert_eq!(joined, 10);
    // Refusal is a silent drop: the 11th client keeps retrying until its
    // own connection timeout, indistinguishable from packet loss.
    assert_eq!(*h.clients[10].core.state(), ClientState::Connecting);
}

fn run_media_scenario() -> (Vec<f32>, Vec<ServerEvent>, Vec<ClientEvent>) {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    h.add_client(&inv_a, Some(440.0));
    let b = h.add_client(&inv_b, Some(0.0));
    h.run_ms(1_000);
    (
        h.clients[b].playout.clone(),
        h.server_events.clone(),
        h.clients[b].events.clone(),
    )
}

#[test]
fn media_path_is_deterministic_after_join() {
    // Handshakes use fresh randomness, so wire bytes differ between runs;
    // behavior after join must not: same pushed frames, same event ordering,
    // bit-identical pulled audio.
    let (p1, se1, ce1) = run_media_scenario();
    let (p2, se2, ce2) = run_media_scenario();
    assert_eq!(p1.len(), p2.len());
    assert!(
        p1.iter().zip(&p2).all(|(x, y)| x.to_bits() == y.to_bits()),
        "playout audio must be bit-identical across runs"
    );
    assert_eq!(se1, se2);
    assert_eq!(ce1, ce2);
    assert!(rms(&p1[p1.len() - 48_000..]) > 0.02, "and it carried audio");
}

#[test]
fn garbage_datagrams_never_panic() {
    let mut h = Harness::new(10, 20);
    let inv = h.mint(0, Role::Musician);
    h.add_client(&inv, Some(0.0));
    h.run_ms(250);
    assert_eq!(*h.clients[0].core.state(), ClientState::Joined);

    let mut lcg: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        lcg = lcg
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        lcg
    };
    for i in 0..500 {
        let len = (next() % 200) as usize;
        let mut data: Vec<u8> = (0..len).map(|_| (next() >> 32) as u8).collect();
        if !data.is_empty() && i % 3 == 0 {
            // Bias toward valid type tags to reach deeper parse paths.
            data[0] = (next() % 5) as u8;
        }
        let src = addr_of(60 + (next() % 4) as u8);
        let now = h.now_ms();
        h.server.handle_datagram(now, h.now_unix, src, &data);
        h.clients[0].core.handle_datagram(now, &data);
        if i % 50 == 0 {
            h.step();
        }
    }
    h.run_ms(100);
    assert_eq!(*h.clients[0].core.state(), ClientState::Joined);
    let _ = h.server.stats();
    let _ = h.clients[0].core.stats();
}

#[test]
fn redundancy_engages_on_server_reported_uplink_loss() {
    // Only client-to-server media drops; the client's own downlink is
    // clean, so the old downlink proxy would never have fired. The server's
    // Stats reports must turn redundancy on.
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let a = h.add_client(&inv_a, Some(440.0));
    h.add_client(&inv_b, Some(0.0));
    h.clients[a].drop_uplink_media_nth = Some(10);
    h.clients[a].uplink_media_stutter = true;
    h.run_ms(4_000);

    assert_eq!(*h.clients[a].core.state(), ClientState::Joined);
    let stats = h.clients[a].core.stats();
    assert!(
        stats.redundancy_active,
        "10% uplink loss must engage redundancy: {stats:?}"
    );
    let loss = stats.uplink_loss_pct.expect("a Stats report arrived");
    assert!(loss > 1.0, "reported uplink loss {loss}%");
    // Downlink was untouched: local jitter buffer saw no wire loss.
    assert_eq!(stats.jitter.lost + stats.jitter.recovered, 0);
    // And the piggybacked copies actually repaired the server's uplink.
    let m = h
        .server
        .stats()
        .into_iter()
        .find(|m| m.id == MemberId(0))
        .expect("member 0");
    assert!(
        m.jitter.recovered > 0,
        "server should recover dropped frames from redundancy: {:?}",
        m.jitter
    );
}

#[test]
fn redundancy_stays_off_when_only_downlink_is_lossy() {
    // The reverse: server-to-client datagrams drop, uplink is clean. The
    // server reports a clean uplink, so redundancy must stay off even
    // though the client sees downlink loss locally.
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let a = h.add_client(&inv_a, Some(440.0));
    h.add_client(&inv_b, Some(440.0));
    h.clients[a].drop_downlink_nth = Some(10);
    h.run_ms(4_000);

    assert_eq!(*h.clients[a].core.state(), ClientState::Joined);
    let stats = h.clients[a].core.stats();
    assert!(
        !stats.redundancy_active,
        "clean uplink must not engage redundancy: {stats:?}"
    );
    let loss = stats.uplink_loss_pct.expect("a Stats report arrived");
    assert!(loss < 1.0, "reported uplink loss {loss}%");
    // The downlink loss is real and visible locally, just not the input.
    assert!(stats.jitter.lost + stats.jitter.recovered > 0);
    let m = h
        .server
        .stats()
        .into_iter()
        .find(|m| m.id == MemberId(0))
        .expect("member 0");
    assert_eq!(m.jitter.recovered, 0, "no redundant copies were sent");
}

#[test]
fn lost_handshake_resp_is_recovered_by_identical_retry() {
    let mut h = Harness::new(10, 20);
    let inv = h.mint(0, Role::Musician);
    let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
    let src = addr_of(40);

    // The server admits and answers, but the response never arrives.
    let out = h.server.handle_datagram(0, h.now_unix, src, &init);
    assert_eq!(out.len(), 1);
    let lost_resp = out[0].1.clone();
    assert_eq!(h.server.musicians_connected(), 1);

    // 500 ms later the client resends the byte-identical init and must get
    // the byte-identical cached response, not a fresh admission.
    let resent = core.poll(500);
    assert_eq!(resent, vec![init.clone()]);
    let out = h.server.handle_datagram(500, h.now_unix, src, &resent[0]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].1, lost_resp, "retry must return the cached response");
    assert_eq!(h.server.musicians_connected(), 1);

    core.handle_datagram(501, &out[0].1);
    assert_eq!(*core.state(), ClientState::Joined);

    // The response pairs with the transport state from the first receipt:
    // a keepalive ping round trip completes end to end.
    let mut back = Vec::new();
    for d in core.poll(1_501) {
        back.extend(h.server.handle_datagram(1_501, h.now_unix, src, &d));
    }
    assert!(!back.is_empty());
    for (_, d) in back {
        core.handle_datagram(1_502, &d);
    }
    assert!(
        core.events()
            .iter()
            .any(|e| matches!(e, ClientEvent::RttSample { .. })),
        "ping round trip over the recovered transport"
    );
}

#[test]
fn fast_rejoin_after_silence_replaces_the_connection() {
    let mut h = Harness::new(10, 20);
    let inv = h.mint(0, Role::Musician);
    let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
    let src = addr_of(41);
    let out = h.server.handle_datagram(0, h.now_unix, src, &init);
    core.handle_datagram(1, &out[0].1);
    assert_eq!(*core.state(), ClientState::Joined);

    // 3 s of silence: past the 2 s rejoin window, under the 10 s timeout.
    // A fresh handshake (new init bytes, new address) is admitted.
    let init2 = core.reconnect(3_000).unwrap();
    assert_ne!(init2, init);
    let src2 = addr_of(42);
    let out = h.server.handle_datagram(3_000, h.now_unix, src2, &init2);
    assert_eq!(out.len(), 1, "fast rejoin must be answered");
    core.handle_datagram(3_001, &out[0].1);
    assert_eq!(*core.state(), ClientState::Joined);
    assert_eq!(h.server.musicians_connected(), 1);
}

#[test]
fn replayed_init_against_active_member_yields_nothing() {
    let mut h = Harness::new(10, 20);
    let inv = h.mint(0, Role::Musician);
    let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
    let src = addr_of(43);
    let out = h.server.handle_datagram(0, h.now_unix, src, &init);
    core.handle_datagram(1, &out[0].1);
    assert_eq!(*core.state(), ClientState::Joined);

    // The member stays active: a ping at 5.5 s refreshes last-heard.
    for d in core.poll(5_500) {
        h.server.handle_datagram(5_500, h.now_unix, src, &d);
    }

    // An attacker replays the captured init at 6 s: the response cache has
    // expired and the member was heard from 0.5 s ago, so the server stays
    // silent and keeps the existing connection.
    let attacker = addr_of(66);
    let out = h.server.handle_datagram(6_000, h.now_unix, attacker, &init);
    assert!(out.is_empty(), "replayed init must be dropped silently");
    assert_eq!(h.server.musicians_connected(), 1);

    // The real member is undisturbed: another round trip works and every
    // server reply still goes to the member's address.
    let mut back = Vec::new();
    for d in core.poll(6_600) {
        back.extend(h.server.handle_datagram(6_600, h.now_unix, src, &d));
    }
    assert!(!back.is_empty());
    assert!(back.iter().all(|(a, _)| *a == src));
    for (_, d) in back {
        core.handle_datagram(6_601, &d);
    }
    assert!(
        core.events()
            .iter()
            .any(|e| matches!(e, ClientEvent::RttSample { .. }))
    );
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn has_avatar_ready(h: &Harness, i: usize, member: MemberId, hash: [u8; 32]) -> bool {
    h.clients[i]
        .events
        .contains(&ClientEvent::AvatarReady { member, hash })
}

#[test]
fn avatar_round_trip_and_late_joiner() {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let a = h.add_client(&inv_a, Some(0.0));
    let b = h.add_client(&inv_b, Some(0.0));
    h.run_ms(500);

    // Not a multiple of the chunk size: exercises the short final chunk.
    let bytes = pattern(20_000, 7);
    let hash = h.clients[a].core.set_avatar(&bytes).unwrap();
    h.run_ms(1_000);

    let roster = h.last_roster(b).expect("roster after set_avatar");
    assert_eq!(
        roster
            .iter()
            .find(|m| m.id == MemberId(0))
            .unwrap()
            .avatar_hash,
        Some(hash),
        "roster must carry A's avatar hash"
    );
    assert!(has_avatar_ready(&h, b, MemberId(0), hash));
    assert_eq!(
        h.clients[b].core.avatar_bytes(&hash),
        Some(bytes.as_slice())
    );
    // A's own copy is announced straight from its local cache.
    assert!(has_avatar_ready(&h, a, MemberId(0), hash));

    // Late joiner: the roster hash is unknown to C, so C requests it from
    // the server's cache; the owner uploads nothing again.
    let uploads_before = h.big_dgrams;
    let inv_c = h.mint(2, Role::Musician);
    let c = h.add_client(&inv_c, Some(0.0));
    h.run_ms(1_000);
    assert!(has_avatar_ready(&h, c, MemberId(0), hash));
    assert_eq!(
        h.clients[c].core.avatar_bytes(&hash),
        Some(bytes.as_slice())
    );
    // Chunks did cross the wire for C (server to C), a cache-served train.
    assert!(h.big_dgrams > uploads_before);
}

#[test]
fn avatar_replacement_converges_on_the_new_hash() {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let a = h.add_client(&inv_a, Some(0.0));
    let b = h.add_client(&inv_b, Some(0.0));
    h.run_ms(500);

    let first = pattern(10_000, 1);
    let hash1 = h.clients[a].core.set_avatar(&first).unwrap();
    h.run_ms(1_000);
    assert!(has_avatar_ready(&h, b, MemberId(0), hash1));

    let second = pattern(30_000, 2);
    let hash2 = h.clients[a].core.set_avatar(&second).unwrap();
    assert_ne!(hash1, hash2);
    h.run_ms(1_000);

    let roster = h.last_roster(b).expect("roster after replacement");
    assert_eq!(
        roster
            .iter()
            .find(|m| m.id == MemberId(0))
            .unwrap()
            .avatar_hash,
        Some(hash2)
    );
    assert!(has_avatar_ready(&h, b, MemberId(0), hash2));
    assert_eq!(
        h.clients[b].core.avatar_bytes(&hash2),
        Some(second.as_slice())
    );
}

#[test]
fn returning_member_avatar_transfers_zero_chunks() {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let a = h.add_client(&inv_a, Some(0.0));
    let b = h.add_client(&inv_b, Some(0.0));
    h.run_ms(500);

    let bytes = pattern(40_000, 9);
    let hash = h.clients[a].core.set_avatar(&bytes).unwrap();
    h.run_ms(1_500);
    assert!(has_avatar_ready(&h, b, MemberId(0), hash));
    assert!(h.big_dgrams > 0, "the first transfer moved chunks");

    // A falls off and times out; the announced hash survives server-side.
    h.clients[a].blocked = true;
    h.advance_quiet(11_000);
    assert_eq!(*h.clients[a].core.state(), ClientState::TimedOut);

    // Fresh handshake, same avatar: the re-announce hits the server cache
    // and the counting shuttle must see no AvatarChunk in either direction.
    h.big_dgrams = 0;
    h.clients[a].blocked = false;
    let now = h.now_ms();
    let init = h.clients[a].core.reconnect(now).unwrap();
    let aaddr = h.clients[a].addr;
    h.to_server.push((aaddr, init));
    h.run_ms(1_500);

    assert_eq!(*h.clients[a].core.state(), ClientState::Joined);
    let roster = h.last_roster(b).expect("roster after rejoin");
    let ma = roster.iter().find(|m| m.id == MemberId(0)).unwrap();
    assert!(ma.connected);
    assert_eq!(ma.avatar_hash, Some(hash));
    assert_eq!(
        h.big_dgrams, 0,
        "returning avatar must transfer zero chunks"
    );
    // And B saw exactly one AvatarReady for the pair across the whole run.
    let readies = h.clients[b]
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                ClientEvent::AvatarReady {
                    member: MemberId(0),
                    ..
                }
            )
        })
        .count();
    assert_eq!(readies, 1);
}

#[test]
fn tampered_avatar_train_is_a_violation_not_an_avatar() {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    h.add_client(&inv_a, Some(0.0));
    let b = h.add_client(&inv_b, Some(0.0));
    h.run_ms(500);

    // A raw member announces the hash of X and then streams Y: sizes and
    // train shape are valid, only the content lies.
    // Sized from the chunk constant so the train stays exactly two chunks
    // whatever that constant is.
    let two_chunks = AVATAR_CHUNK_BYTES + AVATAR_CHUNK_BYTES / 2;
    let x = pattern(two_chunks, 3);
    let y = pattern(two_chunks, 4);
    let hash: [u8; 32] = Blake2s256::digest(&x).into();
    let inv = h.mint(3, Role::Musician);
    let mut raw = raw_join(&mut h, &inv, addr_of(99));
    raw.send_control(
        &mut h,
        ControlMsg::SetAvatar {
            hash,
            len: x.len() as u32,
        },
    );
    // Roster propagates; B requests the hash and becomes a waiter.
    h.run_ms(250);
    raw.send_control(
        &mut h,
        ControlMsg::AvatarChunk {
            hash,
            index: 0,
            total: 2,
            data: y[..AVATAR_CHUNK_BYTES].to_vec(),
        },
    );
    raw.send_control(
        &mut h,
        ControlMsg::AvatarChunk {
            hash,
            index: 1,
            total: 2,
            data: y[AVATAR_CHUNK_BYTES..].to_vec(),
        },
    );
    h.run_ms(500);

    assert!(h.server_events.iter().any(|e| matches!(
        e,
        ServerEvent::ProtocolViolation {
            id: MemberId(3),
            what: "avatar hash mismatch"
        }
    )));
    // Nobody got bytes for the announced hash.
    for i in 0..2 {
        assert!(!has_avatar_ready(&h, i, MemberId(3), hash));
        assert!(h.clients[i].core.avatar_bytes(&hash).is_none());
    }
    // The member is flagged, not ejected: control traffic still works.
    raw.send_control(
        &mut h,
        ControlMsg::Chat {
            from: MemberId(3),
            text: "still here".into(),
        },
    );
    h.run_ms(100);
    assert!(h.clients[b].events.contains(&ClientEvent::Chat {
        from: MemberId(3),
        text: "still here".into()
    }));
}

#[test]
fn oversize_set_avatar_is_rejected() {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let a = h.add_client(&inv_a, Some(0.0));
    h.run_ms(500);

    // Client-side gate.
    assert!(matches!(
        h.clients[a]
            .core
            .set_avatar(&vec![0u8; MAX_AVATAR_BYTES + 1]),
        Err(jamstream_session::SessionError::InvalidParam(_))
    ));

    // Server-side gate against a client that skips the local check.
    let inv = h.mint(3, Role::Musician);
    let mut raw = raw_join(&mut h, &inv, addr_of(97));
    raw.send_control(
        &mut h,
        ControlMsg::SetAvatar {
            hash: [1u8; 32],
            len: (MAX_AVATAR_BYTES + 1) as u32,
        },
    );
    h.run_ms(250);
    assert!(h.server_events.iter().any(|e| matches!(
        e,
        ServerEvent::ProtocolViolation {
            id: MemberId(3),
            what: "avatar length out of range"
        }
    )));
    let roster = h.last_roster(a).expect("roster including the raw member");
    assert_eq!(
        roster
            .iter()
            .find(|m| m.id == MemberId(3))
            .unwrap()
            .avatar_hash,
        None,
        "a refused announce must not reach the roster"
    );
}

/// The starvation gate. Pacing feeds at most 2 chunks per 2.5 ms poll and
/// the link flushes its whole queue every poll, so a chat is sequenced
/// behind at most one allotment (2 x 8 KB) per hop and rides the next
/// flush: one shuttle step per hop on a lossless loopback, two hops
/// (sender uplink, receiver downlink). N = 4 steps (10 ms) doubles that
/// for scheduling slack. A full 256 KB avatar still crosses one hop in
/// 32 / 2 = 16 ticks (40 ms).
#[test]
fn chat_delivers_within_four_steps_while_a_max_avatar_streams() {
    let mut h = Harness::new(10, 20);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_c = h.mint(2, Role::Musician);
    let a = h.add_client(&inv_a, Some(0.0));
    let b = h.add_client(&inv_b, Some(0.0));
    let c = h.add_client(&inv_c, Some(0.0));
    h.run_ms(500);

    let bytes = pattern(MAX_AVATAR_BYTES, 5);
    let hash = h.clients[a].core.set_avatar(&bytes).unwrap();
    let total_chunks = (MAX_AVATAR_BYTES / AVATAR_CHUNK_BYTES) as u64;

    // Wait for the upload to start, then inject a chat mid-train.
    for _ in 0..40 {
        if h.big_dgrams > 0 {
            break;
        }
        h.step();
    }
    assert!(h.big_dgrams > 0, "upload never started");
    assert!(
        h.big_dgrams < total_chunks,
        "upload finished too soon to test"
    );
    h.clients[a].core.send_chat("during upload").unwrap();
    let before = h.big_dgrams;
    let mut delivered_in = None;
    for i in 1..=4 {
        h.step();
        if h.clients[b].events.contains(&ClientEvent::Chat {
            from: MemberId(0),
            text: "during upload".into(),
        }) {
            delivered_in = Some(i);
            break;
        }
    }
    assert!(
        delivered_in.is_some_and(|n| n <= 4),
        "chat starved by avatar upload: {delivered_in:?}"
    );
    assert!(
        h.big_dgrams > before,
        "avatar kept streaming around the chat"
    );

    // Same bound while the server fans trains out to B and C. The budget
    // follows the chunk count: the uplink train has to finish before the
    // server has anything to fan out, and pacing is two chunks per tick.
    for _ in 0..(2 * total_chunks as usize + 60) {
        if h.big_dgrams > total_chunks + 2 {
            break;
        }
        h.step();
    }
    assert!(
        h.big_dgrams > total_chunks + 2,
        "downlink trains never started"
    );
    h.clients[b].core.send_chat("during download").unwrap();
    let mut delivered_in = None;
    for i in 1..=4 {
        h.step();
        if h.clients[c].events.contains(&ClientEvent::Chat {
            from: MemberId(1),
            text: "during download".into(),
        }) {
            delivered_in = Some(i);
            break;
        }
    }
    assert!(
        delivered_in.is_some_and(|n| n <= 4),
        "chat starved by avatar downlink: {delivered_in:?}"
    );

    // And the transfer itself completes end to end.
    h.run_ms(2_000);
    for i in [b, c] {
        assert!(has_avatar_ready(&h, i, MemberId(0), hash));
        assert_eq!(
            h.clients[i].core.avatar_bytes(&hash),
            Some(bytes.as_slice())
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn cores_survive_arbitrary_datagrams(data in proptest::collection::vec(any::<u8>(), 0..256)) {
        let issuer = Issuer::generate();
        let kp = generate_keypair();
        let sid = SessionId::generate();
        let mut server = ServerCore::new(ServerConfig {
            session_id: sid,
            server_private: kp.private.to_vec(),
            server_public: kp.public,
            issuer_pk: issuer.public_key(),
            max_musicians: 2,
            max_listeners: 2,
            member_timeout_ms: 10_000,
        });
        let invite = issuer.mint(
            sid,
            vec![addr_of(1)],
            kp.public,
            Token {
                member_id: MemberId(0),
                role: Role::Musician,
                name_hint: None,
                expires_unix: u64::MAX,
                jti: TokenId::generate(),
            },
        );
        let (mut client, _init) = ClientCore::connect(&invite, 0).unwrap();
        server.handle_datagram(0, 0, addr_of(2), &data);
        client.handle_datagram(0, &data);
        server.tick(1);
        client.poll(1);
    }
}
