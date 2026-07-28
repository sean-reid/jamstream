//! In-memory loopback of ServerCore against several ClientCores: a tiny
//! shuttle pumps datagrams between fixed fake addresses while virtual time
//! advances in 2.5 ms steps. No sockets, no threads, no real clock.

use std::net::SocketAddr;

use blake2::{Blake2s256, Digest};
use jamstream_protocol::Error as ProtocolError;
use jamstream_protocol::control::{
    AVATAR_CHUNK_BYTES, ControlLink, ControlMsg, DestinationState, DestinationStatus,
    MAX_AVATAR_BYTES, MAX_NAME_LEN, MemberInfo, RecordOp, StreamKey, StreamOp, StreamPlatform,
};
use jamstream_protocol::ids::DestinationId;
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::media::{FrameDuration, MediaFrame};
use jamstream_protocol::transport::{Initiator, Session, generate_keypair, reject_key_for_init};
use jamstream_protocol::wire::{self, Packet};
use jamstream_session::{
    ClientCore, ClientEvent, ClientState, MAX_LISTENERS, MAX_MUSICIANS, ServerConfig, ServerCore,
    ServerEvent, VIOLATION_BURST,
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

/// A member whose downlink the test decrypts itself, so it can scan the
/// plaintext the server actually relays rather than trusting a client core to
/// surface it. It never acks, which only means the server keeps retransmitting
/// for the length of a test.
struct Sniffer {
    id: MemberId,
    addr: SocketAddr,
    session: Session,
    /// Every plaintext control or media payload delivered to this member.
    seen: Vec<Vec<u8>>,
    /// Transport counter of each of those datagrams, which is the AEAD nonce.
    counters: Vec<u64>,
}

struct Harness {
    issuer: Issuer,
    server_pk: [u8; 32],
    /// The server's static private key, so a test can build the one thing
    /// only the server can build: an authenticated version reject.
    server_private: Vec<u8>,
    session_id: SessionId,
    server: ServerCore,
    clients: Vec<TestClient>,
    sniffers: Vec<Sniffer>,
    t: f64,
    now_unix: u64,
    to_server: Vec<(SocketAddr, Vec<u8>)>,
    server_events: Vec<ServerEvent>,
    /// Datagrams >= BIG_DGRAM_BYTES shuttled in either direction.
    big_dgrams: u64,
    /// Bytes the server has emitted, so a test can price one inbound packet
    /// in outbound bytes.
    server_out_bytes: u64,
    /// Wall nanoseconds each `ServerCore::tick` in `step` took, for the CPU
    /// budget measurement. Two clock reads per 2.5 ms step is nothing next to
    /// the work they bracket.
    tick_nanos: Vec<u64>,
}

impl Harness {
    fn new(max_musicians: usize, max_listeners: usize) -> Self {
        let issuer = Issuer::generate();
        let kp = generate_keypair();
        let session_id = SessionId::generate();
        let server = ServerCore::new(
            ServerConfig::new(
                session_id,
                kp.private.to_vec(),
                kp.public,
                issuer.public_key(),
            )
            .with_capacity(max_musicians, max_listeners),
        );
        Self {
            issuer,
            server_pk: kp.public,
            server_private: kp.private.to_vec(),
            session_id,
            server,
            clients: Vec::new(),
            sniffers: Vec::new(),
            t: 0.0,
            now_unix: 1_000,
            to_server: Vec::new(),
            server_events: Vec::new(),
            big_dgrams: 0,
            server_out_bytes: 0,
            tick_nanos: Vec::new(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.t as u64
    }

    fn mint(&self, member: u16, role: Role) -> Invite {
        self.mint_named(member, role, None)
    }

    fn mint_named(&self, member: u16, role: Role, name_hint: Option<String>) -> Invite {
        self.issuer.mint(
            self.session_id,
            vec![addr_of(1)],
            self.server_pk,
            Token {
                member_id: MemberId(member),
                role,
                name_hint,
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
        let started = std::time::Instant::now();
        let ticked = self.server.tick(now);
        self.tick_nanos.push(started.elapsed().as_nanos() as u64);
        to_clients.extend(ticked);
        self.server_events.extend(self.server.events());

        for (addr, dg) in to_clients {
            self.server_out_bytes += dg.len() as u64;
            if dg.len() >= BIG_DGRAM_BYTES {
                self.big_dgrams += 1;
            }
            if self.sniff(addr, &dg) {
                continue;
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

    /// Admits a member whose downlink we decrypt in the test. Returns its id.
    fn add_sniffer(&mut self, invite: &Invite, addr: SocketAddr) -> MemberId {
        let raw = raw_join(self, invite, addr);
        let id = raw.id;
        self.sniffers.push(Sniffer {
            id: raw.id,
            addr: raw.addr,
            session: raw.session,
            seen: Vec::new(),
            counters: Vec::new(),
        });
        id
    }

    fn sniffer(&self, id: MemberId) -> &Sniffer {
        self.sniffers
            .iter()
            .find(|s| s.id == id)
            .expect("no sniffer with that id")
    }

    /// True when the datagram belonged to a sniffer, whose plaintext is
    /// recorded instead of being handed to a client core.
    fn sniff(&mut self, addr: SocketAddr, dg: &[u8]) -> bool {
        let Some(s) = self.sniffers.iter_mut().find(|s| s.addr == addr) else {
            return false;
        };
        if let Ok(Packet::Transport {
            member,
            counter,
            ciphertext,
        }) = wire::parse(dg)
            && member == s.id
            && let Ok(plain) = s.session.open(counter, ciphertext)
        {
            s.seen.push(plain);
            s.counters.push(counter);
        }
        true
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
    raw_join_attempt(h, invite, addr).expect("handshake response")
}

/// `raw_join` for the cases where refusal is the expected outcome. Admission
/// refusals are silent by design, so None means the server dropped the init.
fn raw_join_attempt(h: &mut Harness, invite: &Invite, addr: SocketAddr) -> Option<RawMember> {
    let (init, pkt) = Initiator::new(invite).unwrap();
    let now = h.now_ms();
    let replies = h.server.handle_datagram(now, h.now_unix, addr, &pkt);
    let (_, resp) = replies.into_iter().find(|(a, _)| *a == addr)?;
    let Packet::HandshakeResp { noise } = wire::parse(&resp).unwrap() else {
        panic!("expected handshake response");
    };
    let (session, welcome) = init.finish(noise).unwrap();
    Some(RawMember {
        id: welcome.member_id,
        addr,
        session,
        link: ControlLink::new(),
    })
}

impl RawMember {
    /// Returns the bytes this member put on the wire. Whatever the server
    /// answers is priced into `h.server_out_bytes`; acks are fed back to this
    /// member's own link so its queue drains, everything else is dropped.
    fn send_control(&mut self, h: &mut Harness, msg: ControlMsg) -> u64 {
        if let Err(err) = self.link.send(msg) {
            // The server stops acking a member it has dropped, so this
            // member's own queue backs up and nothing more goes out.
            assert!(matches!(err, ProtocolError::LinkFull), "{err}");
            return 0;
        }
        let now = h.now_ms();
        let mut sent = 0;
        for dg in self.link.poll(now) {
            let sealed = self.session.seal(self.id, &dg).unwrap();
            sent += sealed.len() as u64;
            for (_, reply) in h
                .server
                .handle_datagram(now, h.now_unix, self.addr, &sealed)
            {
                h.server_out_bytes += reply.len() as u64;
                if let Ok(Packet::Transport {
                    member,
                    counter,
                    ciphertext,
                }) = wire::parse(&reply)
                    && member == self.id
                    && let Ok(plain) = self.session.open(counter, ciphertext)
                {
                    let _ = self.link.receive(&plain);
                }
            }
        }
        sent
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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

/// Every listener receives byte-identical broadcast audio, so the frame is
/// encoded once per 20 ms whatever the audience size and only the seal
/// differs per member. Encoding it per listener measured 20 x 190 us inside
/// one 2500 us tick, which is a tick overrun at capacity.
#[test]
fn a_broadcast_frame_is_encoded_once_per_fanout_tick() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let musician = h.mint(0, Role::Musician);
    h.add_client(&musician, Some(440.0));
    for i in 0..MAX_LISTENERS as u16 {
        let invite = h.mint(100 + i, Role::Listener);
        h.add_client(&invite, None);
    }
    h.run_ms(200);
    let listeners = h
        .clients
        .iter()
        .filter(|c| c.role == Role::Listener && *c.core.state() == ClientState::Joined)
        .count();
    assert_eq!(listeners, MAX_LISTENERS);

    // 400 ms is 160 ticks, which is 20 broadcast frames at 20 ms each.
    let before = h.server.broadcast_encodes();
    h.run_ms(400);
    assert_eq!(h.server.broadcast_encodes() - before, 20);
}

#[test]
fn listener_receives_broadcast_and_cannot_send_media() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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

/// The broadcast frame is encoded once and handed to every listener, which
/// is only safe if the per-member parts of the packet stay per member. A
/// shared sequence number would make each listener's jitter buffer discard
/// the others' frames as duplicates, and a repeated transport counter would
/// be AEAD nonce reuse. Read off the wire, not off a client core.
#[test]
fn listeners_share_the_payload_but_not_seq_or_nonce() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    h.add_client(&inv_a, Some(440.0));
    h.add_client(&inv_b, Some(660.0));
    let early = h.add_sniffer(&h.mint(5, Role::Listener), addr_of(60));
    h.run_ms(1_000);
    // Joining later gives the two listeners different seq for the same
    // broadcast frame, which is the whole point of the assertion below.
    let late = h.add_sniffer(&h.mint(6, Role::Listener), addr_of(61));
    h.run_ms(1_000);

    let media = |s: &Sniffer| -> Vec<(u32, Vec<u8>)> {
        s.seen
            .iter()
            .filter(|p| matches!(wire::split_channel(p), Ok((wire::CHANNEL_MEDIA, _))))
            .map(|p| {
                let f = MediaFrame::decode(p).expect("media frame");
                assert_eq!(f.duration, FrameDuration::Ms20);
                (f.seq, f.payload.to_vec())
            })
            .collect()
    };
    let a = media(h.sniffer(early));
    let b = media(h.sniffer(late));
    assert!(b.len() > 20, "late listener got {} frames", b.len());
    assert!(a.len() > b.len(), "{} vs {}", a.len(), b.len());

    // Each listener's seq counts its own frames from zero.
    for (name, frames) in [("early", &a), ("late", &b)] {
        let seqs: Vec<u32> = frames.iter().map(|(s, _)| *s).collect();
        let expected: Vec<u32> = (0..frames.len() as u32).collect();
        assert_eq!(seqs, expected, "{name} listener seq");
    }
    // Same broadcast frames, aligned at the tail: identical Opus payloads
    // under different sequence numbers.
    let skew = a.len() - b.len();
    for (i, (seq_b, payload_b)) in b.iter().enumerate() {
        let (seq_a, payload_a) = &a[i + skew];
        assert_eq!(payload_a, payload_b, "frame {i} payload differs");
        assert_ne!(seq_a, seq_b, "frame {i} shares a sequence number");
    }

    // No transport counter is ever reused within a member: that counter is
    // the AEAD nonce, and the receiver's replay window rejects a repeat.
    for id in [early, late] {
        let c = &h.sniffer(id).counters;
        assert!(
            c.windows(2).all(|w| w[1] > w[0]),
            "member {} counters not strictly increasing",
            id.0
        );
    }
}

/// A listener admitted mid-session now meets an encoder that has been
/// running since the first listener joined, instead of one built for them.
/// Their decoder enters a stream in progress, which Opus handles, but it is
/// a real behavioral change and worth an assertion: they must hear the room
/// as clearly as the listener who was there from the start.
#[test]
fn a_listener_joining_mid_stream_hears_the_running_broadcast() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_early = h.mint(5, Role::Listener);
    h.add_client(&inv_a, Some(440.0));
    h.add_client(&inv_b, Some(660.0));
    let early = h.add_client(&inv_early, None);
    h.run_ms(3_000);

    let inv_late = h.mint(6, Role::Listener);
    let late = h.add_client(&inv_late, None);
    h.run_ms(2_000);
    assert_eq!(*h.clients[late].core.state(), ClientState::Joined);

    let win = 48_000; // last 0.5 s
    for hz in [440.0, 660.0] {
        let (e, l) = (tail_tone(&h, early, win, hz), tail_tone(&h, late, win, hz));
        assert!(l > 0.1, "late listener {hz} Hz amplitude {l}");
        assert!(
            (l - e).abs() < 0.25 * e,
            "late listener {hz} Hz {l} against the early listener's {e}"
        );
    }
}

/// Fills the session: MAX_MUSICIANS musicians and MAX_LISTENERS - 1
/// listeners, all silent so tick output is control traffic plus the personal
/// mixes, and leaves one listener seat for a raw member the test drives.
fn full_session() -> (Harness, Invite) {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    for id in 0..MAX_MUSICIANS as u16 {
        let inv = h.mint(id, Role::Musician);
        h.add_client(&inv, Some(0.0));
    }
    for i in 0..MAX_LISTENERS as u16 - 1 {
        let inv = h.mint(100 + i, Role::Listener);
        h.add_client(&inv, None);
    }
    h.run_ms(500);
    assert_eq!(h.server.musicians_connected(), MAX_MUSICIANS);
    let spare = h.mint(200, Role::Listener);
    (h, spare)
}

fn set_avatar(nonce: u8) -> ControlMsg {
    ControlMsg::SetAvatar {
        hash: [nonce; 32],
        len: 4_096,
    }
}

/// A member sending SetAvatar with a fresh hash made the server clone the
/// whole roster into all 30 links plus send an AvatarRequest back, for one
/// small inbound packet: measured at 67 bytes in and about 15 KB out, 224
/// times, sustained for as long as the member kept sending. At 400 packets a
/// second that is 6 MB/s of egress on the host's bill, a flood against every
/// other member, and unbounded queue growth on 30 links.
///
/// The honest cost of one avatar change really is one roster to everyone, so
/// the gate is on the sustained total rather than the per-packet ratio: the
/// burst is paid once, then the flood is metered and its sender ejected.
#[test]
fn a_set_avatar_flood_is_not_an_egress_amplifier() {
    let (mut h, spare) = full_session();
    let mut raw = raw_join(&mut h, &spare, addr_of(95));
    h.run_ms(200);

    // One second of this session with nobody misbehaving.
    let steps = 400;
    h.server_out_bytes = 0;
    h.run(steps);
    let baseline = h.server_out_bytes;

    h.server_out_bytes = 0;
    let mut inbound = 0;
    for i in 0..steps {
        inbound += raw.send_control(&mut h, set_avatar((i % 250) as u8 + 1));
        h.step();
    }
    let extra = h.server_out_bytes.saturating_sub(baseline);
    println!(
        "SetAvatar flood: {steps} packets, {inbound} bytes in, {extra} extra bytes out, \
         {:.0}x (baseline {baseline})",
        extra as f64 / inbound as f64
    );
    // 400 unmetered fanouts cost about 6 MB. The metered burst costs about
    // 12 rosters to 30 members, well under 300 KB.
    assert!(
        extra < 300_000,
        "one second of SetAvatar flood cost {extra} bytes of egress"
    );
    // And it ends: the flood runs the sender's violation budget out.
    assert!(
        h.server_events.iter().any(|e| matches!(
            e,
            ServerEvent::MemberEjected {
                id: MemberId(200),
                ..
            }
        )),
        "the flooder was never ejected"
    );
}

/// The violation counter was incremented in five places and read in none, so
/// a listener invite, the cheapest credential a host hands out, bought the
/// right to send illegal packets at line rate forever. Now it buys
/// VIOLATION_BURST of them.
#[test]
fn a_violation_flood_ejects_the_member_and_holds_the_rejoin() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let inv_host = h.mint(0, Role::Musician);
    h.add_client(&inv_host, Some(440.0));
    h.run_ms(200);

    // A listener sending media: one violation per packet, no rate limit of
    // its own, so it is the cheapest way to exhaust the budget.
    let inv_l = h.mint(7, Role::Listener);
    let mut raw = raw_join(&mut h, &inv_l, addr_of(97));
    let frame = MediaFrame {
        seq: 0,
        timestamp: 0,
        duration: FrameDuration::Ms2_5,
        stereo: false,
        payload: &[1, 2, 3],
        redundant: None,
    }
    .encode();
    for _ in 0..VIOLATION_BURST + 8 {
        raw.send_media(&mut h, &frame);
    }
    h.run(1);

    let ejected: Vec<&ServerEvent> = h
        .server_events
        .iter()
        .filter(|e| {
            matches!(
                e,
                ServerEvent::MemberEjected {
                    id: MemberId(7),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(ejected.len(), 1, "{:?}", h.server_events);
    // Exactly the budget was spent: the packets after ejection are dropped by
    // the disconnected check, not counted again.
    let violations = h
        .server
        .stats()
        .into_iter()
        .find(|s| s.id == MemberId(7))
        .expect("ejected member stays on the roster")
        .violations;
    // The budget tolerates VIOLATION_BURST; the next one ejects.
    assert_eq!(violations, u64::from(VIOLATION_BURST) + 1);
    assert_eq!(h.server.musicians_connected(), 1, "the band plays on");

    // A fresh handshake does not buy a fresh reputation: readmission waits
    // for the budget to refill.
    let mut rejected = raw_join_attempt(&mut h, &inv_l, addr_of(96));
    assert!(rejected.is_none(), "ejected member was readmitted at once");
    h.advance_quiet(2_000);
    rejected = raw_join_attempt(&mut h, &inv_l, addr_of(96));
    assert!(
        rejected.is_some(),
        "the budget refills, so an ejected member can come back"
    );
}

#[test]
fn broadcast_fader_mute_reaches_listeners_not_monitors() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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

fn add_dest(id: u16, platform: StreamPlatform, key: &str) -> StreamOp {
    StreamOp::AddDestination {
        id: DestinationId(id),
        platform,
        key: StreamKey::new(key),
    }
}

fn status(id: u16, state: DestinationState) -> DestinationStatus {
    DestinationStatus {
        id: DestinationId(id),
        platform: StreamPlatform::Twitch,
        state,
        bitrate_kbps: 2_628,
        dropped_frames: 0,
    }
}

fn stream_statuses(h: &Harness, i: usize) -> Vec<Vec<DestinationStatus>> {
    h.clients[i]
        .events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::StreamStatus(d) => Some(d.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn stream_ctl_from_a_non_host_is_a_violation() {
    let mut h = Harness::new(10, 20);
    let inv_host = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_l = h.mint(5, Role::Listener);
    h.add_client(&inv_host, Some(440.0));
    let b = h.add_client(&inv_b, Some(0.0));
    let l = h.add_client(&inv_l, None);
    h.run_ms(1_000);

    // A musician and a listener both try to point the stream somewhere.
    h.clients[b]
        .core
        .stream_ctl(add_dest(1, StreamPlatform::Twitch, "not-your-key"))
        .unwrap();
    h.clients[l].core.stream_ctl(StreamOp::Stop).unwrap();
    h.run_ms(250);

    let violations = h
        .server_events
        .iter()
        .filter(|e| {
            matches!(
                e,
                ServerEvent::ProtocolViolation {
                    what: "stream control by non-host",
                    ..
                }
            )
        })
        .count();
    assert_eq!(violations, 2, "{:?}", h.server_events);
    // Nothing reached the pipeline.
    assert!(
        !h.server_events
            .iter()
            .any(|e| matches!(e, ServerEvent::StreamCtl(_))),
        "a non-host op was accepted"
    );

    // The host's identical op is accepted and surfaces for the driver.
    h.clients[0]
        .core
        .stream_ctl(add_dest(1, StreamPlatform::Twitch, "host-key"))
        .unwrap();
    h.clients[0].core.stream_ctl(StreamOp::Start).unwrap();
    h.run_ms(250);
    let ops: Vec<&StreamOp> = h
        .server_events
        .iter()
        .filter_map(|e| match e {
            ServerEvent::StreamCtl(op) => Some(op),
            _ => None,
        })
        .collect();
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[1], StreamOp::Start));
}

/// `ClientCore::record_ctl` only checks that we are joined, so this server
/// check is the whole of what stops a listener from ending the band's take.
#[test]
fn record_ctl_from_a_non_host_is_a_violation() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let inv_host = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_l = h.mint(5, Role::Listener);
    h.add_client(&inv_host, Some(440.0));
    let b = h.add_client(&inv_b, Some(660.0));
    let l = h.add_client(&inv_l, None);
    h.run_ms(1_000);

    // The host is recording.
    h.clients[0].core.record_ctl(RecordOp::Start).unwrap();
    h.run_ms(250);
    assert_eq!(
        record_ops(&h),
        vec![RecordOp::Start],
        "the host's own take never started"
    );

    // A musician and a listener both try to stop it.
    h.clients[b].core.record_ctl(RecordOp::Stop).unwrap();
    h.clients[l].core.record_ctl(RecordOp::Stop).unwrap();
    h.run_ms(250);
    let refused: Vec<MemberId> = h
        .server_events
        .iter()
        .filter_map(|e| match e {
            ServerEvent::ProtocolViolation {
                id,
                what: "record control by non-host",
            } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(refused, vec![MemberId(1), MemberId(5)]);
    // And the take is untouched: nothing reached the recorder's driver.
    assert_eq!(record_ops(&h), vec![RecordOp::Start]);

    // The host's identical op is the one that ends it.
    h.clients[0].core.record_ctl(RecordOp::Stop).unwrap();
    h.run_ms(250);
    assert_eq!(record_ops(&h), vec![RecordOp::Start, RecordOp::Stop]);
}

/// A revoke is the one control message whose effect outlives the session:
/// `runtime.rs` persists the list, so a listener that could revoke would
/// permanently invalidate somebody else's invite. `ClientCore::revoke` only
/// checks that we are joined, so the server check is all there is.
#[test]
fn revoke_by_a_non_host_is_a_violation() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let inv_host = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_l = h.mint(5, Role::Listener);
    h.add_client(&inv_host, Some(440.0));
    let b = h.add_client(&inv_b, Some(660.0));
    let l = h.add_client(&inv_l, None);
    h.run_ms(1_000);
    assert_eq!(h.server.musicians_connected(), 2);

    // A musician goes for the host's invite and a listener goes for the
    // musician's. Both are refused, and neither jti reaches the revocation
    // list the driver persists.
    h.clients[b].core.revoke(inv_host.token.jti).unwrap();
    h.clients[l].core.revoke(inv_b.token.jti).unwrap();
    h.run_ms(500);
    let refused: Vec<MemberId> = h
        .server_events
        .iter()
        .filter_map(|e| match e {
            ServerEvent::ProtocolViolation {
                id,
                what: "revoke by non-host",
            } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(refused, vec![MemberId(1), MemberId(5)]);
    assert!(
        !h.server_events
            .iter()
            .any(|e| matches!(e, ServerEvent::TokenRevoked { .. })),
        "a non-host revoke reached the persisted list: {:?}",
        h.server_events
    );
    assert!(
        !h.server_events
            .iter()
            .any(|e| matches!(e, ServerEvent::MemberRevoked { .. }))
    );

    // Everyone is still seated and still playing.
    assert_eq!(h.server.musicians_connected(), 2);
    for i in [0, b, l] {
        assert_eq!(*h.clients[i].core.state(), ClientState::Joined);
    }
    h.clear_playouts();
    h.run_ms(1_000);
    assert!(
        tail_tone(&h, b, 48_000, 440.0) > 0.1,
        "the musician who was targeted stopped hearing the host: {}",
        tail_tone(&h, b, 48_000, 440.0)
    );

    // The host's identical revoke is the one that lands.
    h.clients[0].core.revoke(inv_b.token.jti).unwrap();
    h.run_ms(500);
    assert!(h.server_events.contains(&ServerEvent::TokenRevoked {
        jti: inv_b.token.jti
    }));
    assert!(
        h.server_events
            .contains(&ServerEvent::MemberRevoked { id: MemberId(1) })
    );
}

/// A NaN fader is not a rounding problem: `mix_into` multiplies by it, so one
/// packet would silence the personal mix it lands in, and on the broadcast
/// path the mix that goes to every listener and into the recording. Neither
/// path can be reached through `ClientCore`, which range-checks first, so the
/// traffic is crafted.
#[test]
fn a_non_finite_fader_is_refused_on_both_mix_paths() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    // The host is driven raw, because only member 0 can reach the broadcast
    // fader set and only crafted traffic can carry a NaN there. Two ordinary
    // musicians and a listener supply the audio the guard is protecting.
    let inv_host = h.mint(0, Role::Musician);
    let inv_a = h.mint(1, Role::Musician);
    let inv_b = h.mint(2, Role::Musician);
    let inv_l = h.mint(5, Role::Listener);
    let a = h.add_client(&inv_a, Some(440.0));
    let b = h.add_client(&inv_b, Some(660.0));
    let l = h.add_client(&inv_l, None);
    let mut raw_host = raw_join(&mut h, &inv_host, addr_of(90));
    h.run_ms(1_000);

    let mut expected = 0;
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for (gain, pan) in [(bad, 0.0), (0.0, bad)] {
            for target in [MemberId(1), MemberId(2)] {
                raw_host.send_control(
                    &mut h,
                    ControlMsg::MixerSet {
                        target,
                        gain_db: gain,
                        pan,
                        muted: false,
                    },
                );
                raw_host.send_control(
                    &mut h,
                    ControlMsg::BroadcastMixSet {
                        target,
                        gain_db: gain,
                        pan,
                        muted: false,
                    },
                );
                expected += 2;
            }
        }
    }
    h.run_ms(250);

    // Every one of these is a violation, so the whole batch has to fit inside
    // the burst or the member is ejected partway and the count means nothing.
    assert!(expected < VIOLATION_BURST as usize);
    let refused = h
        .server_events
        .iter()
        .filter(|e| {
            matches!(
                e,
                ServerEvent::ProtocolViolation {
                    id: MemberId(0),
                    what: "non-finite fader",
                }
            )
        })
        .count();
    assert_eq!(
        refused, expected,
        "one of the two fader paths took a non-finite value: {:?}",
        h.server_events
    );
    // And neither one was relayed as an accepted change.
    for i in [a, b, l] {
        assert!(
            h.clients[i]
                .events
                .iter()
                .all(|e| !matches!(e, ClientEvent::BroadcastMixChanged { .. })),
            "client {i} was told a non-finite fader had been accepted"
        );
    }

    // Nothing went quiet: the personal mixes and the broadcast that feeds
    // every listener and the recording still carry every tone.
    h.clear_playouts();
    h.run_ms(1_000);
    let win = 48_000;
    assert!(
        tail_tone(&h, a, win, 660.0) > 0.1,
        "musician 1 lost musician 2's tone: {}",
        tail_tone(&h, a, win, 660.0)
    );
    assert!(
        tail_tone(&h, b, win, 440.0) > 0.1,
        "musician 2 lost musician 1's tone: {}",
        tail_tone(&h, b, win, 440.0)
    );
    for hz in [440.0, 660.0] {
        assert!(
            tail_tone(&h, l, win, hz) > 0.1,
            "the broadcast lost {hz} Hz: {}",
            tail_tone(&h, l, win, hz)
        );
    }
}

/// `ControlLink` refuses to carry a roster naming anyone past MAX_NAME_LEN, so
/// a name hint longer than the cap would not break the member who brought it,
/// it would stop roster fanout for the whole session. The cap is applied at
/// admission, and nothing else stands behind it.
#[test]
fn a_name_hint_past_the_cap_cannot_stop_the_roster() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let inv_host = h.mint_named(0, Role::Musician, Some("ana".into()));
    // One byte over is the case that matters: at the cap the hint is kept.
    let long = "n".repeat(MAX_NAME_LEN + 1);
    let inv_b = h.mint_named(1, Role::Musician, Some(long.clone()));
    let inv_c = h.mint_named(2, Role::Musician, Some("z".repeat(MAX_NAME_LEN)));
    h.add_client(&inv_host, Some(440.0));
    let b = h.add_client(&inv_b, Some(660.0));
    let c = h.add_client(&inv_c, Some(0.0));
    h.run_ms(1_000);

    // Everyone joined and everyone has a roster: the oversized hint was
    // dropped for its own member rather than charged to the session.
    assert_eq!(h.server.musicians_connected(), 3);
    for i in [0, b, c] {
        let roster = h.last_roster(i).unwrap_or_else(|| {
            panic!("client {i} never got a roster, so the oversized name broke fanout")
        });
        assert_eq!(roster.len(), 3, "client {i} roster {roster:?}");
        assert!(
            roster.iter().all(|m| m.name.len() <= MAX_NAME_LEN),
            "client {i} was handed a name past the cap: {roster:?}"
        );
        let names: Vec<&str> = roster.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names[0], "ana");
        assert_eq!(names[1], "member 1", "the oversized hint was not replaced");
        assert_eq!(names[2], "z".repeat(MAX_NAME_LEN));
    }
    assert_ne!(long.len(), MAX_NAME_LEN);
}

/// The click is per member, decided by the member and not the host: enabling
/// the metronome must not put a click in the monitor of somebody who turned it
/// off, and turning it off must not take it away from anybody else.
#[test]
fn the_click_is_enabled_per_member() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let inv_host = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_l = h.mint(5, Role::Listener);
    h.add_client(&inv_host, Some(0.0));
    let b = h.add_client(&inv_b, Some(0.0));
    let l = h.add_client(&inv_l, None);
    h.run_ms(1_000);

    // Silent musicians, so the only thing in any mix is the click.
    h.clients[b].core.set_click(false).unwrap();
    h.clients[0].core.set_metronome(120, 4, true).unwrap();
    h.run_ms(500);
    h.clear_playouts();
    h.run_ms(2_000);
    let win = 96_000;
    assert!(
        tail_rms(&h, 0, win) > 0.005,
        "the host opted in and heard nothing: {}",
        tail_rms(&h, 0, win)
    );
    assert!(
        tail_rms(&h, b, win) < 1e-4,
        "musician 1 turned the click off and still heard it: {}",
        tail_rms(&h, b, win)
    );
    // Listeners never hear it at all: it is not in the broadcast mix.
    assert!(
        tail_rms(&h, l, win) < 1e-4,
        "the click reached the broadcast: {}",
        tail_rms(&h, l, win)
    );

    // Opting back in is enough on its own; the host does not re-send anything.
    h.clients[b].core.set_click(true).unwrap();
    h.run_ms(250);
    h.clear_playouts();
    h.run_ms(2_000);
    assert!(
        tail_rms(&h, b, win) > 0.005,
        "musician 1 opted back in and heard nothing: {}",
        tail_rms(&h, b, win)
    );
    assert!(tail_rms(&h, l, win) < 1e-4);

    // And opting out again is not a violation, whoever does it: a listener
    // has a click flag too, it just has no mix to put it in.
    let before = h.server_events.len();
    h.clients[l].core.set_click(false).unwrap();
    h.run_ms(250);
    assert!(
        h.server_events[before..]
            .iter()
            .all(|e| !matches!(e, ServerEvent::ProtocolViolation { .. })),
        "{:?}",
        &h.server_events[before..]
    );
}

fn record_ops(h: &Harness) -> Vec<RecordOp> {
    h.server_events
        .iter()
        .filter_map(|e| match e {
            ServerEvent::RecordCtl(op) => Some(*op),
            _ => None,
        })
        .collect()
}

#[test]
fn stream_status_reaches_every_member() {
    let mut h = Harness::new(10, 20);
    let inv_host = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_l = h.mint(5, Role::Listener);
    h.add_client(&inv_host, Some(440.0));
    h.add_client(&inv_b, Some(660.0));
    h.add_client(&inv_l, None);
    h.run_ms(1_000);

    // Nothing configured: no status traffic at all.
    for i in 0..3 {
        assert!(stream_statuses(&h, i).is_empty());
    }

    // The driver reports what the pipeline sees. Every member hears about it,
    // musician and listener alike.
    let now = h.now_ms();
    h.server
        .set_stream_status(now, vec![status(1, DestinationState::Connecting)]);
    h.run_ms(250);
    for i in 0..3 {
        let seen = stream_statuses(&h, i);
        assert_eq!(seen.len(), 1, "client {i} saw {seen:?}");
        assert_eq!(seen[0][0].state, DestinationState::Connecting);
        assert_eq!(seen[0][0].bitrate_kbps, 2_628);
    }

    // A transition goes out immediately.
    let now = h.now_ms();
    h.server
        .set_stream_status(now, vec![status(1, DestinationState::Live)]);
    h.run_ms(100);
    for i in 0..3 {
        let seen = stream_statuses(&h, i);
        assert_eq!(seen.len(), 2, "client {i} saw {seen:?}");
        assert_eq!(seen[1][0].state, DestinationState::Live);
    }

    // Unchanged status settles to the once-a-second heartbeat rather than a
    // message per driver poll.
    for _ in 0..30 {
        let now = h.now_ms();
        h.server
            .set_stream_status(now, vec![status(1, DestinationState::Live)]);
        h.run_ms(100);
    }
    for i in 0..3 {
        let n = stream_statuses(&h, i).len();
        assert!((4..=6).contains(&n), "client {i} got {n} statuses in 3 s");
    }

    // A member joining mid-broadcast is told at once, not up to a second later.
    let inv_late = h.mint(6, Role::Listener);
    let late = h.add_client(&inv_late, None);
    h.run_ms(150);
    let seen = stream_statuses(&h, late);
    assert_eq!(seen.len(), 1, "late joiner saw {seen:?}");
    assert_eq!(seen[0][0].state, DestinationState::Live);

    // And clearing it tells everyone once, then goes quiet.
    let before: Vec<usize> = (0..4).map(|i| stream_statuses(&h, i).len()).collect();
    let now = h.now_ms();
    h.server.set_stream_status(now, Vec::new());
    h.run_ms(1_500);
    for (i, was) in before.iter().enumerate() {
        let seen = stream_statuses(&h, i);
        assert_eq!(seen.len(), was + 1, "client {i} saw {seen:?}");
        assert!(seen.last().expect("nonempty").is_empty());
    }
}

/// The one property the whole key-handling design exists for: a stream key
/// the host sends is never relayed to anyone. Asserted against the plaintext
/// bytes the server actually seals, not against a client core's events.
#[test]
fn stream_keys_never_appear_in_anything_the_server_relays() {
    const KEY: &str = "live_424242_donotleak";
    let mut h = Harness::new(10, 20);
    let inv_host = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_snoop = h.mint(7, Role::Listener);
    h.add_client(&inv_host, Some(440.0));
    h.add_client(&inv_b, Some(0.0));
    h.add_sniffer(&inv_snoop, addr_of(60));
    h.run_ms(1_000);

    h.clients[0]
        .core
        .stream_ctl(add_dest(1, StreamPlatform::Twitch, KEY))
        .unwrap();
    h.clients[0].core.stream_ctl(StreamOp::Start).unwrap();
    h.run_ms(500);
    // The op reached the driver, so the key really was in flight.
    assert!(
        h.server_events
            .iter()
            .any(|e| matches!(e, ServerEvent::StreamCtl(StreamOp::AddDestination { .. }))),
        "the host's add never arrived"
    );

    // The pipeline reports status, which is the only stream traffic that fans
    // out. Include the destination in a failed state, since the reason string
    // is the one status field that carries free text.
    let now = h.now_ms();
    h.server.set_stream_status(
        now,
        vec![status(
            1,
            DestinationState::Failed {
                reason: "pusher exited: connection refused".to_owned(),
            },
        )],
    );
    h.run_ms(1_000);

    let needle = KEY.as_bytes();
    let contains = |bytes: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);

    // Every plaintext byte the server sealed to a member.
    let snoop = h.sniffer(MemberId(7));
    assert!(!snoop.seen.is_empty(), "the sniffer received nothing");
    for plain in &snoop.seen {
        assert!(!contains(plain), "key found in a relayed datagram");
    }
    // And every message the honest clients decoded, serialized back to bytes.
    for i in 0..h.clients.len() {
        for status in stream_statuses(&h, i) {
            let bytes = postcard::to_allocvec(&ControlMsg::StreamStatus {
                destinations: status,
            })
            .unwrap();
            assert!(
                !contains(&bytes),
                "key found in a status sent to client {i}"
            );
        }
        let debug = format!("{:?}", h.clients[i].events);
        assert!(!debug.contains(KEY), "key found in client {i} events");
    }
    // The server's own copy of the status is key-free too.
    let bytes = postcard::to_allocvec(h.server.stream_status()).unwrap();
    assert!(!contains(&bytes));
}

#[test]
fn broadcast_tap_exposes_post_limiter_audio_and_card_state() {
    let mut h = Harness::new(10, 20);
    let inv_host = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    h.add_client(&inv_host, Some(440.0));
    h.add_client(&inv_b, Some(0.0));
    h.run_ms(1_000);

    // Off by default: no metering work, no levels.
    assert!(!h.server.broadcast_tap());
    let tick = h.server.broadcast_tick();
    assert_eq!(tick.audio.len(), 240);
    assert_eq!(tick.members.len(), 2);
    assert!(tick.members.iter().all(|m| m.level_peak == 0.0));
    let epoch = tick.roster_epoch;

    h.server.set_broadcast_tap(true);
    h.run_ms(500);
    let tick = h.server.broadcast_tick();
    // The host is sending a 440 Hz tone at 0.5, the other musician silence.
    let host = tick.members.iter().find(|m| m.id == MemberId(0)).unwrap();
    let other = tick.members.iter().find(|m| m.id == MemberId(1)).unwrap();
    assert!(host.level_peak > 0.1, "host peak {}", host.level_peak);
    assert!(host.level_rms > 0.05, "host rms {}", host.level_rms);
    // Not exactly zero: a silent musician's Opus round trip leaves denormals.
    assert!(other.level_peak < 1e-6, "silent peak {}", other.level_peak);
    assert!(host.connected);
    assert_eq!(host.name, "member 0");
    // The audio slice is the broadcast mix, so the tone is in it.
    let rms = (tick.audio.iter().map(|s| s * s).sum::<f32>() / tick.audio.len() as f32).sqrt();
    assert!(rms > 0.05, "broadcast slice is silent: {rms}");
    // Listeners are counted separately; a roster change bumps the epoch.
    assert_eq!(tick.listeners, 0);
    assert_eq!(tick.roster_epoch, epoch);

    let inv_l = h.mint(5, Role::Listener);
    h.add_client(&inv_l, None);
    h.run_ms(200);
    let tick = h.server.broadcast_tick();
    assert_eq!(tick.listeners, 1);
    assert_eq!(tick.members.len(), 2, "listeners are not carded");
    assert!(tick.roster_epoch > epoch);

    // Turning the tap off clears the meters.
    h.server.set_broadcast_tap(false);
    let tick = h.server.broadcast_tick();
    assert!(tick.members.iter().all(|m| m.level_peak == 0.0));
}

#[test]
fn the_stem_tap_carries_decoded_members_and_their_broadcast_faders() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let inv_host = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let host = h.add_client(&inv_host, Some(440.0));
    h.add_client(&inv_b, Some(0.0));
    h.run_ms(1_000);

    // Right after a tick, the tap holds each connected musician's decoded
    // audio at unity until the host sets a broadcast fader.
    let stems: Vec<_> = h.server.stems().collect();
    assert_eq!(stems.len(), 2);
    let tone = stems.iter().find(|s| s.id == MemberId(0)).unwrap();
    let silent = stems.iter().find(|s| s.id == MemberId(1)).unwrap();
    assert!(rms(tone.pcm) > 0.1, "tone stem rms {}", rms(tone.pcm));
    assert!(
        rms(silent.pcm) < 1e-6,
        "silent stem rms {}",
        rms(silent.pcm)
    );
    assert_eq!(
        (tone.fader.gain_db, tone.fader.pan, tone.fader.muted),
        (0.0, 0.0, false)
    );

    // The tap reports the fader the broadcast mix runs the member through,
    // and the pcm stays pre-fader: the recorder applies it off the tick.
    h.clients[host]
        .core
        .set_broadcast_fader(MemberId(0), -6.0, 0.25, false)
        .unwrap();
    h.run_ms(250);
    let stems: Vec<_> = h.server.stems().collect();
    let tone = stems.iter().find(|s| s.id == MemberId(0)).unwrap();
    assert_eq!(
        (tone.fader.gain_db, tone.fader.pan, tone.fader.muted),
        (-6.0, 0.25, false)
    );
    assert!(rms(tone.pcm) > 0.1, "pre-mix pcm was attenuated");

    // A member who leaves stops appearing; the tap never yields stale audio.
    h.clients[host].core.leave("done").unwrap();
    h.clients[host].tone_hz = None;
    h.run_ms(250);
    assert!(h.server.stems().all(|s| s.id != MemberId(0)));
}

#[test]
fn audition_swaps_host_playout_to_broadcast_and_back() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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

/// What jamstreamd does with a panic it caught partway through a datagram:
/// drop that one peer, whose state is what stopped being trustworthy, and keep
/// serving everyone else. Before this the unwind left the run loop and took the
/// whole session down.
#[test]
fn dropping_one_peer_leaves_the_rest_of_the_session_playing() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let inv_a = h.mint(0, Role::Musician);
    let inv_b = h.mint(1, Role::Musician);
    let inv_c = h.mint(2, Role::Musician);
    let a = h.add_client(&inv_a, Some(440.0));
    let b = h.add_client(&inv_b, Some(660.0));
    let c = h.add_client(&inv_c, Some(0.0));
    h.run_ms(1_000);
    assert_eq!(h.server.musicians_connected(), 3);

    let baddr = h.clients[b].addr;
    assert_eq!(h.server.drop_peer(baddr), Some(MemberId(1)));
    // An address nobody holds is not an error: an admission that panicked
    // before it created a member leaves nothing to drop.
    assert_eq!(h.server.drop_peer(addr_of(210)), None);
    h.run_ms(250);
    assert_eq!(h.server.musicians_connected(), 2);
    assert!(
        h.server_events
            .contains(&ServerEvent::MemberDisconnected { id: MemberId(1) })
    );

    // A and C keep hearing each other; B's tone is gone from A's mix.
    h.clear_playouts();
    h.run_ms(1_000);
    let win = 48_000;
    assert!(
        tail_tone(&h, c, win, 440.0) > 0.1,
        "C lost A's tone: {}",
        tail_tone(&h, c, win, 440.0)
    );
    assert!(
        tail_tone(&h, a, win, 660.0) < 0.02,
        "dropped peer still in the mix: {}",
        tail_tone(&h, a, win, 660.0)
    );

    // B's token is untouched: a fresh handshake gets them back in.
    let now = h.now_ms();
    let init = h.clients[b].core.reconnect(now).unwrap();
    h.to_server.push((baddr, init));
    h.run_ms(500);
    assert_eq!(*h.clients[b].core.state(), ClientState::Joined);
    assert_eq!(h.server.musicians_connected(), 3);
}

/// A peer that keeps media flowing while acking nothing is heard from
/// constantly, so the 10 s member timeout never reaps it and the client's own
/// silence timeout never fires either. Both links give up retransmitting
/// after their 36 attempts, and that is what ends it: the server frees the
/// seat and the client stops pretending it is in a session. Before, the
/// give-up flag was set and read by nobody.
#[test]
fn a_control_link_that_gives_up_ends_the_connection_at_both_ends() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let inv = h.mint(0, Role::Musician);
    let idx = h.add_client(&inv, Some(0.0));
    h.run_ms(250);
    assert_eq!(*h.clients[idx].core.state(), ClientState::Joined);
    let addr = h.clients[idx].addr;

    // 100 ms hops carrying one media frame each way and no control at all:
    // the client's acks and pings are dropped on the way out, so nothing
    // either side sends is ever acknowledged.
    let mut server_reaped = None;
    let mut client_gave_up = None;
    for _ in 0..800 {
        h.t += 100.0;
        let now = h.now_ms();
        let media = h.clients[idx].core.push_capture(now, &[0.0; 120]);
        let _ = h.clients[idx].core.poll(now);
        let mut to_client = Vec::new();
        for dg in media {
            to_client.extend(h.server.handle_datagram(now, h.now_unix, addr, &dg));
        }
        to_client.extend(h.server.tick(now));
        for (a, dg) in to_client {
            if a == addr {
                let _ = h.clients[idx].core.handle_datagram(now, &dg);
            }
        }
        if server_reaped.is_none()
            && h.server
                .events()
                .iter()
                .any(|e| matches!(e, ServerEvent::MemberDisconnected { id } if *id == MemberId(0)))
        {
            server_reaped = Some(now);
        }
        if client_gave_up.is_none() && *h.clients[idx].core.state() == ClientState::TimedOut {
            client_gave_up = Some(now);
        }
    }

    // The give-up horizon is 65 s. Well past the member timeout, which is
    // the point: this reaps something the timeout cannot see.
    let reaped = server_reaped.expect("server never reaped the member");
    let gave_up = client_gave_up.expect("client never gave up");
    assert!(
        (64_000..70_000).contains(&reaped),
        "server reaped at {reaped} ms"
    );
    assert!(
        (64_000..70_000).contains(&gave_up),
        "client gave up at {gave_up} ms"
    );
    assert_eq!(h.server.musicians_connected(), 0);
}

#[test]
fn timeout_then_rejoin_with_same_token() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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

/// An attacker on the same wifi sees the init leave and answers it with
/// garbage before the server can. The join must still happen: snow restores
/// its symmetric state on a failed read, so the client keeps the handshake it
/// started and the server's real response completes it.
#[test]
fn a_sprayed_handshake_response_cannot_keep_a_client_out() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let inv = h.mint(0, Role::Musician);
    let idx = h.add_client(&inv, Some(0.0));
    let victim = h.clients[idx].addr;

    // One forged response per step for the first 100 ms, arriving ahead of
    // the server's own answer in every step.
    for _ in 0..40 {
        let now = h.now_ms();
        let forged = wire::build_handshake_resp(&[0xA5; 96]);
        h.clients[idx].core.handle_datagram(now, &forged);
        h.step();
    }
    assert_eq!(*h.clients[idx].core.state(), ClientState::Joined);
    assert_eq!(h.server.musicians_connected(), 1);
    assert_eq!(h.clients[idx].addr, victim);
}

/// The reject this server would send in answer to `init`, which is the only
/// party that can build one: the key is a secret between it and the client
/// that sent the init.
fn reject_for(h: &Harness, init: &[u8], theirs: u16) -> Vec<u8> {
    let Ok(Packet::HandshakeInit { version, noise }) = wire::parse(init) else {
        panic!("expected an init");
    };
    let key = reject_key_for_init(&h.server_private, &h.session_id, version, noise)
        .expect("server derives the reject key");
    wire::build_version_reject(&key, theirs, version, init)
}

#[test]
fn version_reject_rate_limited_and_verified() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let src = addr_of(50);
    let inv = h.mint(0, Role::Musician);
    // A client from the future: readable by this server, refused by it.
    let (future, fake_init) = Initiator::new_claiming_version(&inv, 3).unwrap();
    let now = h.now_ms();

    let out = h.server.handle_datagram(now, h.now_unix, src, &fake_init);
    assert_eq!(out.len(), 1);
    let Ok(Packet::VersionReject { ours, theirs, mac }) = wire::parse(&out[0].1) else {
        panic!("expected a version reject");
    };
    assert_eq!((ours, theirs), (1, 3));
    assert!(wire::verify_version_reject(
        future.reject_key().unwrap(),
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
    // An init nobody could have written against this session is answered
    // with silence, because there is nobody to authenticate a reject to.
    let garbage = wire::build_handshake_init(3, &[0x5A; 96]);
    assert!(
        h.server
            .handle_datagram(now + 3_000, h.now_unix, addr_of(51), &garbage)
            .is_empty()
    );

    // A reject forged by another invite holder is ignored. Holding an invite
    // is holding the server's public key, which the MAC is no longer keyed on.
    let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
    let (other, _) = Initiator::new(&inv).unwrap();
    let forged = wire::build_version_reject(other.reject_key().unwrap(), 1, 1, &init);
    assert!(core.handle_datagram(1, &forged).is_empty());
    assert_eq!(*core.state(), ClientState::Connecting);
    assert!(core.events().is_empty());
}

/// A reject is a report, not an ending. The client keeps handing the same
/// init back at a widening interval, so a session that migrates or is
/// redeployed onto a build it can talk to is joined without a restart. This
/// used to be terminal: `poll` did nothing at all in the rejected state.
#[test]
fn a_rejected_client_joins_when_the_server_starts_answering() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let inv = h.mint(0, Role::Musician);
    let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
    core.handle_datagram(1, &reject_for(&h, &init, 2));
    assert_eq!(*core.state(), ClientState::Rejected { ours: 1, theirs: 2 });

    let retry = core.poll(6_000);
    assert_eq!(retry, vec![init]);
    let replies = h
        .server
        .handle_datagram(6_000, h.now_unix, addr_of(60), &retry[0]);
    assert_eq!(replies.len(), 1);
    core.handle_datagram(6_001, &replies[0].1);
    assert_eq!(*core.state(), ClientState::Joined);
}

// The capacity every host surface offers seats against, enforced here.
// MAX_MUSICIANS counts the host (member 0 joins as a musician like anyone
// else), so a full band is MAX_MUSICIANS members and the next one is
// refused.
#[test]
fn musician_capacity_enforced() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let over_cap = MAX_MUSICIANS + 1;
    let invites: Vec<Invite> = (0..over_cap as u16)
        .map(|i| h.mint(i, Role::Musician))
        .collect();
    for inv in &invites {
        h.add_client(inv, Some(0.0));
    }
    h.run_ms(500);

    assert_eq!(h.server.musicians_connected(), MAX_MUSICIANS);
    let joined = h
        .clients
        .iter()
        .filter(|c| *c.core.state() == ClientState::Joined)
        .count();
    assert_eq!(joined, MAX_MUSICIANS);
    // The over-cap client is told the band is full and keeps its init on
    // offer, which is what gets it in when somebody leaves.
    assert_eq!(
        *h.clients[MAX_MUSICIANS].core.state(),
        ClientState::Connecting
    );
    assert!(h.clients[MAX_MUSICIANS].core.session_full());
    assert!(
        h.clients[MAX_MUSICIANS]
            .events
            .contains(&ClientEvent::SessionFull)
    );
}

/// Steps the harness for `ms` while `per_step` handshake inits arrive from
/// addresses nobody is listening at, which is what a spoofed flood looks like:
/// the server cannot tell them from an honest first flight and cannot reach
/// the senders to find out.
fn flood_ms(h: &mut Harness, ms: u64, per_step: usize) {
    let init = wire::build_handshake_init(jamstream_protocol::PROTOCOL_VERSION, &[0xAA; 96]);
    let mut n: u32 = 0;
    for _ in 0..(ms as f64 / STEP_MS) as usize {
        for _ in 0..per_step {
            n = n.wrapping_add(1);
            let src: SocketAddr = format!("198.18.{}.{}:9000", (n >> 8) & 0xFF, n & 0xFF)
                .parse()
                .unwrap();
            h.to_server.push((src, init.clone()));
        }
        h.step();
    }
}

/// The whole point of the cookie round trip: a flood must not keep an honest
/// client out of the session.
///
/// The rate limiter alone bounded what a flood could take out of the mix tick,
/// which protects the people already playing, but it cannot tell a spoofed
/// source from a real one, so an honest init was dropped along with the flood
/// and that client joined only once the flood stopped. Real client cores, real
/// server core, real packets: the flood is delivered to `handle_datagram` from
/// addresses that never answer, exactly as the socket would.
#[test]
fn an_honest_client_joins_through_an_init_flood() {
    let mut h = Harness::new(MAX_MUSICIANS, 0);
    let host = h.mint(0, Role::Musician);
    h.add_client(&host, Some(440.0));
    h.run_ms(300);
    assert_eq!(
        h.server.cookie_challenges(),
        0,
        "an ordinary join must stay at one round trip"
    );

    // The flood starts, and the musician arrives into the middle of it.
    flood_ms(&mut h, 500, 4);
    assert!(
        h.server.cookie_challenges() > 0,
        "the flood was never asked for a cookie"
    );
    let late = h.mint(1, Role::Musician);
    let i = h.add_client(&late, Some(0.0));
    let reads_before = h.server.handshake_reads();
    flood_ms(&mut h, 3_000, 4);

    assert_eq!(
        *h.clients[i].core.state(),
        ClientState::Joined,
        "the honest client did not get in through the flood"
    );
    assert_eq!(h.server.musicians_connected(), 2);
    // And it is hearing the host, so it really is in the session rather than
    // merely holding a roster entry.
    let tail = tail_rms(&h, i, 48_000);
    assert!(tail > 0.02, "the late musician heard nothing, rms {tail}");

    // Meanwhile the flood bought almost no asymmetric crypto. It sent 1600
    // inits a second for 3 seconds; the trigger's burst was already gone, so
    // what is left is its refill, a couple of dozen a second.
    let bought = h.server.handshake_reads() - reads_before;
    assert!(
        bought < 120,
        "4800 spoofed inits bought {bought} Diffie-Hellmans"
    );

    // The flood stops, the trigger refills, and the next join is back to one
    // round trip with no cookie in it.
    h.run_ms(3_000);
    let challenges = h.server.cookie_challenges();
    let third = h.mint(2, Role::Musician);
    let j = h.add_client(&third, Some(0.0));
    h.run_ms(500);
    assert_eq!(*h.clients[j].core.state(), ClientState::Joined);
    assert_eq!(
        h.server.cookie_challenges(),
        challenges,
        "a quiet session still asked for a cookie"
    );
}

/// The listener half of the same rule, which the server enforced with
/// nothing holding it to it. It also pins the two caps as separate counters:
/// a sold-out gallery must not cost the band a seat, which is the failure a
/// single shared count would produce. The refusal must cost the session
/// nothing either: the gallery keeps hearing the broadcast, and the refused
/// client is told the gallery is full rather than left to time out, which is
/// what it used to get.
#[test]
fn listener_capacity_enforced() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let host = h.mint(0, Role::Musician);
    h.add_client(&host, Some(440.0));
    let over_cap = MAX_LISTENERS + 1;
    let invites: Vec<Invite> = (0..over_cap as u16)
        .map(|i| h.mint(100 + i, Role::Listener))
        .collect();
    for inv in &invites {
        h.add_client(inv, None);
    }
    h.run_ms(500);

    assert_eq!(h.server.broadcast_tick().listeners, MAX_LISTENERS);
    let joined = h
        .clients
        .iter()
        .filter(|c| c.role == Role::Listener && *c.core.state() == ClientState::Joined)
        .count();
    assert_eq!(joined, MAX_LISTENERS);
    // The over-cap listener's token verified, so it is told the truth in a
    // packet only this server could have produced, and it keeps its init on
    // offer instead of waiting out a timeout it would have to misreport.
    let refused = 1 + MAX_LISTENERS;
    assert_eq!(*h.clients[refused].core.state(), ClientState::Connecting);
    assert!(h.clients[refused].core.session_full());
    assert!(
        h.clients[refused]
            .events
            .contains(&ClientEvent::SessionFull),
        "the refused listener was never told the gallery was full"
    );
    // Exactly once, however many rejects the server sends: a capacity reject
    // is replayable by anyone who saw one.
    assert_eq!(
        h.clients[refused]
            .events
            .iter()
            .filter(|e| **e == ClientEvent::SessionFull)
            .count(),
        1
    );

    // A full gallery leaves the band's seats alone.
    let late = h.mint(1, Role::Musician);
    let i = h.add_client(&late, Some(0.0));
    h.run_ms(500);
    assert_eq!(*h.clients[i].core.state(), ClientState::Joined);
    assert_eq!(h.server.musicians_connected(), 2);
    assert_eq!(h.server.broadcast_tick().listeners, MAX_LISTENERS);

    // And it leaves the gallery alone: the broadcast keeps flowing to every
    // admitted listener while the refused one's retries go unanswered.
    h.clear_playouts();
    h.run_ms(1_000);
    let win = 48_000; // last 0.5 s
    for l in 1..=MAX_LISTENERS {
        assert!(
            tail_rms(&h, l, win) > 0.02,
            "listener {l} lost the broadcast after the refusal, rms {}",
            tail_rms(&h, l, win)
        );
    }
    assert!(
        tail_rms(&h, refused, win) < 1e-6,
        "the refused listener heard audio, rms {}",
        tail_rms(&h, refused, win)
    );

    // Past the 10 s connection timeout the refused client is still trying,
    // and never claims to have timed out: the server is answering it, so a
    // timeout would be the one thing it knows to be false. The admitted
    // members ride keepalives through it and stay seated.
    h.advance_quiet(11_000);
    assert_eq!(*h.clients[refused].core.state(), ClientState::Connecting);
    assert!(!h.clients[refused].events.contains(&ClientEvent::TimedOut));
    assert!(h.clients[refused].core.session_full());
    assert_eq!(h.server.musicians_connected(), 2);
    assert_eq!(h.server.broadcast_tick().listeners, MAX_LISTENERS);

    // And the retry is the point of not giving up: one listener leaves, and
    // the refused one takes the seat with no user restarting anything.
    h.clients[1].core.leave("making room").unwrap();
    h.run_ms(500);
    assert_eq!(h.server.broadcast_tick().listeners, MAX_LISTENERS - 1);
    // The retry interval has widened to tens of seconds by now, so give it
    // room rather than assuming which resend lands.
    h.advance_quiet(60_000);
    assert_eq!(*h.clients[refused].core.state(), ClientState::Joined);
    assert!(!h.clients[refused].core.session_full());
    assert_eq!(h.server.broadcast_tick().listeners, MAX_LISTENERS);
}

/// Not a gate, a measurement:
/// `cargo test -p jamstream-session --release --test loopback -- --ignored
/// --nocapture tick_cost_at_capacity`. Release matters: the test profile's
/// opt-level of 1 reaches libopus too, so a debug number says nothing about
/// the shipped server. The deadline is 2500 us per tick, and the one tick in
/// eight that also encodes the 20 ms broadcast frame is the one at risk.
#[test]
#[ignore = "measurement, not a gate"]
fn tick_cost_at_capacity() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    for id in 0..MAX_MUSICIANS as u16 {
        // Distinct tones, not silence: silence is the cheapest thing Opus
        // ever encodes and would flatter the measurement.
        let inv = h.mint(id, Role::Musician);
        h.add_client(&inv, Some(110.0 * f32::from(id + 1)));
    }
    for i in 0..MAX_LISTENERS as u16 {
        let inv = h.mint(100 + i, Role::Listener);
        h.add_client(&inv, None);
    }
    h.run_ms(1_000);
    assert_eq!(h.server.musicians_connected(), MAX_MUSICIANS);
    assert_eq!(h.server.broadcast_tick().listeners, MAX_LISTENERS);

    h.tick_nanos.clear();
    h.run_ms(10_000);
    let ticks = std::mem::take(&mut h.tick_nanos);

    // The broadcast frame lands on one phase of the eight-tick cycle. Find
    // which by mean rather than assuming where the warmup left the counter.
    let group = |p: usize| -> Vec<u64> { ticks.iter().skip(p).step_by(8).copied().collect() };
    let mean_us = |v: &[u64]| v.iter().sum::<u64>() as f64 / v.len() as f64 / 1_000.0;
    let min_us = |v: &[u64]| *v.iter().min().unwrap() as f64 / 1_000.0;
    let phase = (0..8)
        .max_by(|&a, &b| mean_us(&group(a)).total_cmp(&mean_us(&group(b))))
        .unwrap();
    let bcast = group(phase);
    let plain: Vec<u64> = (0..8).filter(|&p| p != phase).flat_map(group).collect();
    println!(
        "tick cost, {} musicians and {} listeners, {} ticks\n  \
         broadcast tick: min {:.0} us, mean {:.0} us\n  \
         other ticks:    min {:.0} us, mean {:.0} us\n  \
         amortized:      {:.0} us per tick, {:.0}% of the 2500 us budget",
        MAX_MUSICIANS,
        MAX_LISTENERS,
        ticks.len(),
        min_us(&bcast),
        mean_us(&bcast),
        min_us(&plain),
        mean_us(&plain),
        mean_us(&ticks),
        100.0 * mean_us(&ticks) / 2_500.0,
    );
}

fn run_media_scenario() -> (Vec<f32>, Vec<ServerEvent>, Vec<ClientEvent>) {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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

/// The bounds and rate limits added for #43 must not touch normal load. Full
/// house, 10 musicians and 20 listeners, a 256 KB avatar (the largest legal
/// one, a train of 256 chunks) and a burst of chat: every member ends up with
/// the avatar bytes and every chat line.
#[test]
fn a_full_session_still_moves_a_max_avatar_and_a_chat_burst() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
    let mut musicians = Vec::new();
    for id in 0..MAX_MUSICIANS as u16 {
        let inv = h.mint(id, Role::Musician);
        musicians.push(h.add_client(&inv, Some(0.0)));
    }
    let mut listeners = Vec::new();
    for i in 0..MAX_LISTENERS as u16 {
        let inv = h.mint(100 + i, Role::Listener);
        listeners.push(h.add_client(&inv, None));
    }
    h.run_ms(500);
    assert_eq!(h.server.musicians_connected(), MAX_MUSICIANS);

    let bytes = pattern(MAX_AVATAR_BYTES, 5);
    let hash = h.clients[musicians[0]].core.set_avatar(&bytes).unwrap();
    // A fast typist's burst, right at the fanout allowance.
    for n in 0..12 {
        h.clients[musicians[1]]
            .core
            .send_chat(&format!("line {n}"))
            .unwrap();
    }
    // A 256 KB train is 256 chunks at two chunks per tick per hop, so 320 ms
    // to reach the server and 320 ms out to each member, whose links run in
    // parallel. 3 s is ample and keeps the test off the slow list.
    h.run_ms(3_000);

    for &i in musicians.iter().chain(listeners.iter()) {
        assert!(
            has_avatar_ready(&h, i, MemberId(0), hash),
            "member index {i} never got the avatar"
        );
        assert_eq!(
            h.clients[i].core.avatar_bytes(&hash),
            Some(bytes.as_slice()),
            "member index {i} got the wrong bytes"
        );
        let chats = h.clients[i]
            .events
            .iter()
            .filter(|e| matches!(e, ClientEvent::Chat { from, .. } if *from == MemberId(1)))
            .count();
        assert_eq!(chats, 12, "member index {i} saw {chats} of 12 chat lines");
    }
    let violations: u64 = h.server.stats().iter().map(|s| s.violations).sum();
    assert_eq!(violations, 0, "{:?}", h.server_events);
}

#[test]
fn avatar_replacement_converges_on_the_new_hash() {
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
    let mut h = Harness::new(MAX_MUSICIANS, MAX_LISTENERS);
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
        let mut server = ServerCore::new(
            ServerConfig::new(sid, kp.private.to_vec(), kp.public, issuer.public_key())
                .with_capacity(2, 2),
        );
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
