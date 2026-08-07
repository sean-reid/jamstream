//! Scenario runner: a real `ServerCore` plus N real `ClientCore`s wired
//! through the seeded network simulator on a 2.5 ms master tick. One process,
//! no sockets, no threads. For a fixed builder configuration and seed, every
//! packet's size and send instant is fixed by the tick schedule, so the
//! seeded network draws identically and the media path replays exactly, even
//! though the handshake bytes themselves use fresh Noise keys per run.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use jamstream_engine::JitterStats;
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Issuer, Token};
use jamstream_protocol::media::{FrameDuration, MediaFrame};
use jamstream_protocol::transport::{Initiator, generate_keypair};
use jamstream_protocol::wire::{self, Packet};
use jamstream_session::{
    ClientCore, ClientEvent, ClientState, MemberStats, ServerConfig, ServerCore, ServerEvent,
};

use crate::clock::{SkewedClock, VirtualClock};
use crate::net::{EndpointId, Profile, SimNet};

/// Master tick: 2.5 ms, one media frame.
pub const TICK_US: u64 = 2_500;
/// Mono samples per tick at 48 kHz.
pub const FRAME_SAMPLES: usize = 120;
/// Interleaved stereo samples per tick.
pub const STEREO_FRAME: usize = FRAME_SAMPLES * 2;
/// Impulse detection threshold in the playout recording.
pub const DETECT_THRESHOLD: f32 = 0.05;

const SERVER_ENDPOINT: EndpointId = EndpointId(0);
/// Fixed unix time handed to token verification; tokens never expire here.
const NOW_UNIX: u64 = 1_700_000_000;
const MEMBER_TIMEOUT_MS: u64 = 10_000;

/// What a musician's virtual microphone produces, indexed by capture sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source {
    Silence,
    Sine {
        hz: f32,
        amp: f32,
    },
    /// Single-sample 1.0 spikes every `period_samples`, silence between.
    ImpulseTrain {
        period_samples: u32,
    },
}

impl Source {
    fn render(self, first_sample: u64, out: &mut [f32]) {
        match self {
            Source::Silence => out.fill(0.0),
            Source::Sine { hz, amp } => {
                for (j, s) in out.iter_mut().enumerate() {
                    let n = (first_sample + j as u64) as f64;
                    *s = (f64::from(hz) * std::f64::consts::TAU * n / 48_000.0).sin() as f32 * amp;
                }
            }
            Source::ImpulseTrain { period_samples } => {
                out.fill(0.0);
                for (j, s) in out.iter_mut().enumerate() {
                    if (first_sample + j as u64) % u64::from(period_samples) == 0 {
                        *s = 1.0;
                    }
                }
            }
        }
    }
}

/// Aggregate datagram counters over every client<->server link.
#[derive(Debug, Default, Clone, Copy)]
pub struct Traffic {
    pub sent: u64,
    pub delivered: u64,
    pub dropped: u64,
}

/// Wall-clock cost of `ServerCore::tick`, split by whether that tick also had
/// to fan a 20 ms broadcast frame out to the listeners (one tick in eight).
///
/// Two kinds of number live here and they gate different things. The ratio of
/// the medians is dimensionless, so a runner three times slower than a laptop
/// moves both and not the ratio, but it is blind to anything the two ticks
/// share: doubling per-tick work shared by both moves the ratio *down*.
/// `broadcast_p99_us` is the absolute one, against the 2.5 ms deadline, and it
/// needs the runner named (see `perf_budget_secs` in the gate suite).
#[derive(Debug, Default, Clone, Copy)]
pub struct TickCost {
    pub broadcast_ticks: usize,
    pub ordinary_ticks: usize,
    pub broadcast_median_us: f64,
    pub ordinary_median_us: f64,
    /// 99th percentile of the broadcast ticks. The deadline is a per-tick
    /// deadline and the broadcast tick is the expensive one, so a mean over
    /// all ticks would divide the tick at risk into the seven that are not.
    pub broadcast_p99_us: f64,
    /// Worst single broadcast tick in the window, for the log.
    pub broadcast_max_us: f64,
    /// Mean over ticks of both kinds: what the 2.5 ms tick budget is actually
    /// spent against once the broadcast tick is amortized.
    pub amortized_mean_us: f64,
    /// Broadcast frames `ServerCore` reports encoding over the window, which
    /// is a count and not a timing: one per 20 ms whatever the audience size.
    pub broadcast_encodes: u64,
}

impl TickCost {
    /// Broadcast tick cost in ordinary ticks. 1.0 would mean fanning out to
    /// every listener is free; the cost of per-listener work shows up here
    /// multiplied by the listener count. NaN with nothing measured, so a
    /// scenario that forgot to ask for the measurement fails its gate rather
    /// than passing it.
    pub fn fanout_ratio(&self) -> f64 {
        self.broadcast_median_us / self.ordinary_median_us
    }
}

struct SimClient {
    endpoint: EndpointId,
    addr: SocketAddr,
    core: ClientCore,
    role: Role,
    jti: TokenId,
    source: Source,
    skew: Option<SkewedClock>,
    frames_emitted: u64,
    /// Raw mode: fractional device-sample accumulators (a +-ppm device
    /// delivers 120 * (1 +- ppm e-6) samples per master tick) and the
    /// capture sample index the source renders from.
    capture_acc: f64,
    playout_acc: f64,
    capture_samples: u64,
    /// Full interleaved stereo playout; only kept when `keep_audio` is set.
    recording: Vec<f32>,
    /// Per tick: (peak abs, sum of squares) over that tick's playout frame.
    /// Always kept; silence and rms gates read this, not the raw audio.
    meter: Vec<(f32, f32)>,
    events: Vec<ClientEvent>,
    /// While set, every outgoing datagram is replaced by seeded garbage of
    /// the same length (a client gone haywire, from the server's view).
    garbage: bool,
    /// While set, this client's audio driver is frozen: no capture is
    /// delivered and no playout is asked for. See `set_driver_stalled`.
    stalled: bool,
}

pub struct ScenarioBuilder {
    seed: u64,
    profile: Profile,
    overrides: HashMap<usize, Profile>,
    musicians: usize,
    listeners: usize,
    skews: HashMap<usize, i32>,
    sources: HashMap<usize, Source>,
    duration_ms: u64,
    keep_audio: bool,
    raw_audio: bool,
    tick_cost: bool,
}

impl ScenarioBuilder {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            profile: crate::profiles::profile("regional-fiber").clone(),
            overrides: HashMap::new(),
            musicians: 2,
            listeners: 0,
            skews: HashMap::new(),
            sources: HashMap::new(),
            duration_ms: 10_000,
            keep_audio: true,
            raw_audio: false,
            tick_cost: false,
        }
    }

    pub fn profile(mut self, profile: &Profile) -> Self {
        self.profile = profile.clone();
        self
    }

    /// Overrides the client<->server link for one client index.
    pub fn link_override(mut self, client: usize, profile: &Profile) -> Self {
        self.overrides.insert(client, profile.clone());
        self
    }

    pub fn musicians(mut self, n: usize) -> Self {
        self.musicians = n;
        self
    }

    pub fn listeners(mut self, n: usize) -> Self {
        self.listeners = n;
        self
    }

    /// Sample-clock skew for one client; positive runs fast.
    pub fn skew_ppm(mut self, client: usize, ppm: i32) -> Self {
        self.skews.insert(client, ppm);
        self
    }

    pub fn source(mut self, client: usize, source: Source) -> Self {
        self.sources.insert(client, source);
        self
    }

    pub fn duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = ms;
        self
    }

    /// Full playout recordings cost ~384 KB per client per virtual second;
    /// long soaks turn this off and rely on the per-tick meter instead.
    pub fn keep_audio(mut self, keep: bool) -> Self {
        self.keep_audio = keep;
        self
    }

    /// Drives every client through the raw device-paced APIs
    /// (`push_capture_raw`/`pull_playout_raw`) instead of the exact-frame
    /// ones: each master tick a client's virtual device produces and
    /// consumes `120 * (1 + skew_ppm e-6)` samples, accumulated
    /// fractionally, so drift arrives as a sample-rate error the client's
    /// compensators must steer out.
    pub fn raw_audio(mut self, raw: bool) -> Self {
        self.raw_audio = raw;
        self
    }

    /// Times every `ServerCore::tick` and files it under [`TickCost`]. Off by
    /// default: two `Instant::now` calls per tick would otherwise sit inside
    /// the inner loop of every soak.
    pub fn measure_tick_cost(mut self, measure: bool) -> Self {
        self.tick_cost = measure;
        self
    }

    pub fn build(self) -> Scenario {
        let total = self.musicians + self.listeners;
        assert!(total > 0, "scenario needs at least one client");
        for &idx in self
            .overrides
            .keys()
            .chain(self.skews.keys())
            .chain(self.sources.keys())
        {
            assert!(
                idx < total,
                "client index {idx} out of range (total {total})"
            );
        }

        let issuer = Issuer::generate();
        let kp = generate_keypair();
        let session_id = SessionId::generate();
        // Scenario-sized caps, with headroom so an out-of-band raw member can
        // also be admitted; production capacity lives in
        // jamstream_session::limits.
        let server = ServerCore::new(
            ServerConfig::new(
                session_id,
                kp.private.to_vec(),
                kp.public,
                issuer.public_key(),
            )
            .with_capacity(self.musicians + 2, self.listeners + 2)
            .with_member_timeout_ms(MEMBER_TIMEOUT_MS),
        );
        let server_addr: SocketAddr = "198.51.100.1:43210".parse().expect("server addr");

        let mut net = SimNet::new(self.seed);
        let mut clients = Vec::with_capacity(total);
        let mut endpoint_by_addr = HashMap::new();
        for i in 0..total {
            let endpoint = EndpointId((i + 1) as u16);
            let addr: SocketAddr = format!("10.0.{}.{}:40000", i / 200, i % 200 + 1)
                .parse()
                .expect("client addr");
            net.link(
                endpoint,
                SERVER_ENDPOINT,
                self.overrides.get(&i).unwrap_or(&self.profile),
            );
            let role = if i < self.musicians {
                Role::Musician
            } else {
                Role::Listener
            };
            let invite = issuer.mint(
                session_id,
                vec![server_addr],
                kp.public,
                Token {
                    member_id: MemberId(i as u16),
                    role,
                    name_hint: None,
                    expires_unix: u64::MAX,
                    jti: TokenId::generate(),
                },
            );
            let (core, init) = ClientCore::connect(&invite, 0).expect("client connect");
            net.send(0, endpoint, SERVER_ENDPOINT, init);
            endpoint_by_addr.insert(addr, i);
            clients.push(SimClient {
                endpoint,
                addr,
                core,
                role,
                jti: invite.token.jti,
                source: self.sources.get(&i).copied().unwrap_or(Source::Silence),
                skew: self.skews.get(&i).map(|&ppm| SkewedClock::new(ppm)),
                frames_emitted: 0,
                capture_acc: 0.0,
                playout_acc: 0.0,
                capture_samples: 0,
                recording: Vec::new(),
                meter: Vec::new(),
                events: Vec::new(),
                garbage: false,
                stalled: false,
            });
        }

        Scenario {
            clock: VirtualClock::new(),
            net,
            server,
            issuer,
            session_id,
            server_pk: kp.public,
            server_addr,
            endpoint_by_addr,
            clients,
            server_events: Vec::new(),
            duration_ms: self.duration_ms,
            keep_audio: self.keep_audio,
            raw_audio: self.raw_audio,
            tick_cost: self.tick_cost,
            bcast_tick_ns: Vec::new(),
            other_tick_ns: Vec::new(),
            bcast_encodes_at_reset: 0,
            tick: 0,
            garbage_lcg: self.seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }
}

pub struct Scenario {
    clock: VirtualClock,
    net: SimNet,
    server: ServerCore,
    issuer: Issuer,
    session_id: SessionId,
    server_pk: [u8; 32],
    server_addr: SocketAddr,
    endpoint_by_addr: HashMap<SocketAddr, usize>,
    clients: Vec<SimClient>,
    server_events: Vec<ServerEvent>,
    duration_ms: u64,
    keep_audio: bool,
    raw_audio: bool,
    tick_cost: bool,
    bcast_tick_ns: Vec<u64>,
    other_tick_ns: Vec<u64>,
    /// `ServerCore::broadcast_encodes` when the timing window opened, so the
    /// count reported covers the window and not the joins before it.
    bcast_encodes_at_reset: u64,
    tick: u64,
    garbage_lcg: u64,
}

impl Scenario {
    /// One 2.5 ms master tick: advance time, deliver due packets to their
    /// cores, run the server mix tick, then let every client capture, poll,
    /// and pull playout.
    pub fn step(&mut self) {
        self.clock.advance_us(TICK_US);
        let now_us = self.clock.now_us();
        let now_ms = self.clock.now_ms();

        for d in self.net.poll(now_us) {
            if d.to == SERVER_ENDPOINT {
                let idx = (d.from.0 - 1) as usize;
                let src = self.clients[idx].addr;
                let out = self
                    .server
                    .handle_datagram(now_ms, NOW_UNIX, src, &d.payload);
                self.route_server_out(now_us, out);
            } else {
                let idx = (d.to.0 - 1) as usize;
                let replies = self.clients[idx].core.handle_datagram(now_ms, &d.payload);
                self.forward_client(now_us, idx, replies);
            }
        }

        let out = if self.tick_cost {
            let encodes_before = self.server.broadcast_encodes();
            let started = Instant::now();
            let out = self.server.tick(now_ms);
            let elapsed = started.elapsed().as_nanos() as u64;
            self.file_tick_cost(elapsed, encodes_before);
            out
        } else {
            self.server.tick(now_ms)
        };
        self.route_server_out(now_us, out);
        self.server_events.extend(self.server.events());

        for idx in 0..self.clients.len() {
            if self.clients[idx].stalled {
                self.step_client_stalled(now_us, now_ms, idx);
            } else if self.raw_audio {
                self.step_client_raw(now_us, now_ms, idx);
            } else {
                self.step_client_exact(now_us, now_ms, idx);
            }
            let c = &mut self.clients[idx];
            let events = c.core.events();
            c.events.extend(events);
        }
        self.tick += 1;
    }

    /// Exact-frame drive: a skewed client emits a frame each time its own
    /// clock crosses a 2.5 ms boundary: +200 ppm occasionally emits two
    /// frames in one master tick, -200 ppm occasionally emits none.
    fn step_client_exact(&mut self, now_us: u64, now_ms: u64, idx: usize) {
        let due = match self.clients[idx].skew {
            Some(sk) => sk.map(now_us) / TICK_US,
            None => now_us / TICK_US,
        };
        while self.clients[idx].frames_emitted < due {
            let mut pcm = [0.0f32; FRAME_SAMPLES];
            let first = self.clients[idx].frames_emitted * FRAME_SAMPLES as u64;
            self.clients[idx].source.render(first, &mut pcm);
            self.clients[idx].frames_emitted += 1;
            if self.clients[idx].role == Role::Musician {
                let dgs = self.clients[idx].core.push_capture(now_ms, &pcm);
                self.forward_client(now_us, idx, dgs);
            }
        }

        let dgs = self.clients[idx].core.poll(now_ms);
        self.forward_client(now_us, idx, dgs);

        let mut buf = [0.0f32; STEREO_FRAME];
        self.clients[idx].core.pull_playout(&mut buf);
        self.record(idx, &buf);
    }

    /// Raw device-paced drive: the client's virtual sound card runs at
    /// `120 * (1 + skew_ppm e-6)` samples per master tick, accumulated
    /// fractionally and delivered in whole samples, capture and playout
    /// alike, through the raw client APIs.
    fn step_client_raw(&mut self, now_us: u64, now_ms: u64, idx: usize) {
        let ppm = self.clients[idx].skew.map_or(0, |sk| sk.skew_ppm());
        let rate = FRAME_SAMPLES as f64 * (1.0 + f64::from(ppm) * 1e-6);

        self.clients[idx].capture_acc += rate;
        let n = self.clients[idx].capture_acc as usize;
        self.clients[idx].capture_acc -= n as f64;
        if self.clients[idx].role == Role::Musician && n > 0 {
            let mut pcm = [0.0f32; 2 * FRAME_SAMPLES];
            let first = self.clients[idx].capture_samples;
            self.clients[idx].source.render(first, &mut pcm[..n]);
            self.clients[idx].capture_samples += n as u64;
            let dgs = self.clients[idx].core.push_capture_raw(now_ms, &pcm[..n]);
            self.forward_client(now_us, idx, dgs);
        }

        let dgs = self.clients[idx].core.poll(now_ms);
        self.forward_client(now_us, idx, dgs);

        self.clients[idx].playout_acc += rate;
        let m = self.clients[idx].playout_acc as usize;
        self.clients[idx].playout_acc -= m as f64;
        let mut buf = [0.0f32; 4 * FRAME_SAMPLES];
        self.clients[idx].core.pull_playout_raw(&mut buf[..m * 2]);
        self.record(idx, &buf[..m * 2]);
    }

    /// A frozen audio driver. The device thread delivers no capture and asks
    /// for no playout, so this tick's frames simply never happen: the
    /// swallowed capture is dropped rather than replayed, leaving the resumed
    /// uplink with a hole in its frame clock and contiguous sequence numbers,
    /// and the downlink jitter buffer filling with nobody reading it. A partial
    /// pump catch-up here would hide the whole scenario. The socket side
    /// keeps running, because a stalled device thread does not stop the
    /// network thread: the member stays joined and keeps answering pings.
    fn step_client_stalled(&mut self, now_us: u64, now_ms: u64, idx: usize) {
        if self.raw_audio {
            let ppm = self.clients[idx].skew.map_or(0, |sk| sk.skew_ppm());
            let rate = FRAME_SAMPLES as f64 * (1.0 + f64::from(ppm) * 1e-6);
            self.clients[idx].capture_acc += rate;
            let n = self.clients[idx].capture_acc as usize;
            self.clients[idx].capture_acc -= n as f64;
            self.clients[idx].capture_samples += n as u64;
            self.clients[idx].playout_acc += rate;
            let m = self.clients[idx].playout_acc as usize;
            self.clients[idx].playout_acc -= m as f64;
        } else {
            self.clients[idx].frames_emitted = match self.clients[idx].skew {
                Some(sk) => sk.map(now_us) / TICK_US,
                None => now_us / TICK_US,
            };
        }

        let dgs = self.clients[idx].core.poll(now_ms);
        self.forward_client(now_us, idx, dgs);

        // A dead device plays nothing, and the meter must stay tick-aligned
        // for `rms_of` and `longest_silence_ms` to index by tick.
        self.record(idx, &[0.0; STEREO_FRAME]);
    }

    /// One per-tick meter entry (peak, energy) over this tick's playout,
    /// plus the raw audio when `keep_audio` is set.
    fn record(&mut self, idx: usize, buf: &[f32]) {
        let c = &mut self.clients[idx];
        let mut peak = 0.0f32;
        let mut energy = 0.0f32;
        for &s in buf {
            peak = peak.max(s.abs());
            energy += s * s;
        }
        c.meter.push((peak, energy));
        if self.keep_audio {
            c.recording.extend_from_slice(buf);
        }
    }

    pub fn run_ticks(&mut self, ticks: u64) {
        for _ in 0..ticks {
            self.step();
        }
    }

    pub fn run_ms(&mut self, ms: u64) {
        self.run_ticks(ms * 1_000 / TICK_US);
    }

    /// Steps until virtual time reaches `ms` since scenario start.
    pub fn run_until_ms(&mut self, ms: u64) {
        while self.clock.now_ms() < ms {
            self.step();
        }
    }

    /// Runs out the builder-configured session duration.
    pub fn run_to_end(&mut self) {
        self.run_until_ms(self.duration_ms);
    }

    /// Steps until every client reports Joined; panics with the per-client
    /// states if that takes longer than `max_ticks`. Returns the join tick.
    pub fn join_all_or_panic(&mut self, max_ticks: u64) -> u64 {
        for _ in 0..max_ticks {
            if self.all_joined() {
                return self.tick;
            }
            self.step();
        }
        if self.all_joined() {
            return self.tick;
        }
        let states: Vec<ClientState> = self
            .clients
            .iter()
            .map(|c| c.core.state().clone())
            .collect();
        panic!("not every client joined within {max_ticks} ticks: {states:?}");
    }

    fn all_joined(&self) -> bool {
        self.clients
            .iter()
            .all(|c| *c.core.state() == ClientState::Joined)
    }

    pub fn current_tick(&self) -> u64 {
        self.tick
    }

    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    pub fn client_state(&self, client: usize) -> ClientState {
        self.clients[client].core.state().clone()
    }

    pub fn client_events(&self, client: usize) -> &[ClientEvent] {
        &self.clients[client].events
    }

    pub fn server_events(&self) -> &[ServerEvent] {
        &self.server_events
    }

    pub fn client_jitter(&self, client: usize) -> JitterStats {
        self.clients[client].core.stats().jitter
    }

    pub fn server_member_stats(&self) -> Vec<MemberStats> {
        self.server.stats()
    }

    pub fn musicians_connected(&self) -> usize {
        self.server.musicians_connected()
    }

    /// Interleaved stereo playout; empty unless built with `keep_audio(true)`.
    pub fn recording(&self, client: usize) -> &[f32] {
        &self.clients[client].recording
    }

    pub fn set_source(&mut self, client: usize, source: Source) {
        self.clients[client].source = source;
    }

    pub fn set_garbage(&mut self, client: usize, garbage: bool) {
        self.clients[client].garbage = garbage;
    }

    /// Freezes or thaws one client's audio driver, the way a multi-second
    /// process stall under load freezes a real one. While stalled the harness
    /// skips that client's capture and playout entirely (see
    /// `step_client_stalled`); everything else about the session keeps
    /// running, so the member neither leaves nor times out.
    pub fn set_driver_stalled(&mut self, client: usize, stalled: bool) {
        self.clients[client].stalled = stalled;
    }

    /// Clean Bye from a joined client; the server drops it from the roster,
    /// the client core itself stays Joined until its own timeout.
    pub fn leave(&mut self, client: usize) {
        self.clients[client]
            .core
            .leave("scenario leave")
            .expect("leave requires a joined client");
    }

    /// Fresh handshake with the original invite.
    pub fn reconnect(&mut self, client: usize) {
        let now_ms = self.clock.now_ms();
        let init = self.clients[client]
            .core
            .reconnect(now_ms)
            .expect("reconnect");
        let ep = self.clients[client].endpoint;
        self.net
            .send(self.clock.now_us(), ep, SERVER_ENDPOINT, init);
    }

    /// Host (client 0) revokes `target`'s invite mid-session.
    pub fn host_revoke(&mut self, target: usize) {
        let jti = self.clients[target].jti;
        self.clients[0]
            .core
            .revoke(jti)
            .expect("revoke requires the host to be joined");
    }

    /// Joins a bare protocol-level listener directly against the server (an
    /// honest `ClientCore` listener has no encoder and cannot send media at
    /// all) and pushes one sealed media frame. The server must count it as a
    /// protocol violation without disturbing anyone else. The handshake here
    /// bypasses SimNet on purpose: the subject under test is the server's
    /// violation accounting, not the network.
    pub fn raw_listener_media(&mut self, member_id: u16) {
        let invite = self.issuer.mint(
            self.session_id,
            vec![self.server_addr],
            self.server_pk,
            Token {
                member_id: MemberId(member_id),
                role: Role::Listener,
                name_hint: None,
                expires_unix: u64::MAX,
                jti: TokenId::generate(),
            },
        );
        let (init, pkt) = Initiator::new(&invite).expect("raw initiator");
        let src: SocketAddr = "203.0.113.99:40000".parse().expect("raw addr");
        let now_ms = self.clock.now_ms();
        let replies = self.server.handle_datagram(now_ms, NOW_UNIX, src, &pkt);
        let (_, resp) = replies
            .into_iter()
            .find(|(a, _)| *a == src)
            .expect("handshake response for raw listener");
        let Ok(Packet::HandshakeResp { noise }) = wire::parse(&resp) else {
            panic!("expected a handshake response");
        };
        let (mut session, welcome) = init.finish(noise).expect("raw handshake finish");
        let frame = MediaFrame {
            seq: 0,
            timestamp: 0,
            duration: FrameDuration::Ms2_5,
            stereo: false,
            payload: &[1, 2, 3],
            redundant: None,
        }
        .encode();
        let sealed = session.seal(welcome.member_id, &frame).expect("seal");
        let _ = self.server.handle_datagram(now_ms, NOW_UNIX, src, &sealed);
        self.server_events.extend(self.server.events());
    }

    /// Mouth-to-ear latencies in ms: pairs each threshold crossing in the
    /// listener's recording (from `from_tick` on) with the impulse the
    /// emitter's source produced just before it. Capture sample k*period is
    /// generated during master tick k*period/120, and playout sample p is
    /// recorded during master tick p/120, so the sample-index difference is
    /// exactly the mouth-to-ear delay at 48 samples per ms.
    pub fn impulse_latencies(&self, emitter: usize, listener: usize, from_tick: u64) -> Vec<f32> {
        let Source::ImpulseTrain { period_samples } = self.clients[emitter].source else {
            panic!("client {emitter} is not an ImpulseTrain source");
        };
        assert!(self.keep_audio, "impulse_latencies needs keep_audio(true)");
        let period = u64::from(period_samples);
        let rec = &self.clients[listener].recording;
        let start = from_tick as usize * STEREO_FRAME;
        let mut latencies = Vec::new();
        // Refractory of half a period after each detection swallows codec
        // ring-out and the other stereo channel of the same event.
        let mut next_allowed = 0u64;
        for (i, &s) in rec.iter().enumerate().skip(start) {
            let p = (i / 2) as u64;
            if p < next_allowed || s.abs() <= DETECT_THRESHOLD {
                continue;
            }
            let emitted = (p / period) * period;
            latencies.push((p - emitted) as f32 / 48.0);
            next_allowed = p + period / 2;
        }
        latencies
    }

    /// RMS of the playout over `[from_tick, to_tick)`, from the per-tick meter.
    pub fn rms_of(&self, client: usize, from_tick: u64, to_tick: u64) -> f32 {
        let m = &self.clients[client].meter;
        let (a, b) = (from_tick as usize, (to_tick as usize).min(m.len()));
        assert!(a < b, "empty rms window {from_tick}..{to_tick}");
        let energy: f32 = m[a..b].iter().map(|&(_, e)| e).sum();
        (energy / ((b - a) * STEREO_FRAME) as f32).sqrt()
    }

    /// Longest run of ticks in `[from_tick, to_tick)` whose playout peak
    /// stays under `threshold`, in ms.
    pub fn longest_silence_ms(
        &self,
        client: usize,
        from_tick: u64,
        to_tick: u64,
        threshold: f32,
    ) -> f32 {
        let m = &self.clients[client].meter;
        let (a, b) = (from_tick as usize, (to_tick as usize).min(m.len()));
        assert!(a < b, "empty silence window {from_tick}..{to_tick}");
        let mut run = 0u64;
        let mut longest = 0u64;
        for &(peak, _) in &m[a..b] {
            if peak < threshold {
                run += 1;
                longest = longest.max(run);
            } else {
                run = 0;
            }
        }
        longest as f32 * (TICK_US as f32 / 1_000.0)
    }

    /// Files one timed tick under broadcast or ordinary. A tick is a broadcast
    /// tick exactly when it encoded a broadcast frame, which the core counts
    /// itself, so the harness needs to know neither the 20 ms accumulator
    /// period nor which of the datagrams leaving the server was audio. It used
    /// to look for any datagram addressed to a listener, which also matched a
    /// tick that only retransmitted a roster to the gallery: those ticks are
    /// cheap, and averaging them into the broadcast median understated it.
    fn file_tick_cost(&mut self, elapsed_ns: u64, encodes_before: u64) {
        let broadcast = self.server.broadcast_encodes() > encodes_before;
        if broadcast {
            self.bcast_tick_ns.push(elapsed_ns);
        } else {
            self.other_tick_ns.push(elapsed_ns);
        }
    }

    /// Drops the samples collected so far. Called after the join and settle
    /// phase, whose ticks do handshake work no steady-state tick does.
    pub fn reset_tick_cost(&mut self) {
        self.bcast_tick_ns.clear();
        self.other_tick_ns.clear();
        self.bcast_encodes_at_reset = self.server.broadcast_encodes();
    }

    /// Empty unless built with `measure_tick_cost(true)`.
    pub fn tick_cost(&self) -> TickCost {
        // Nearest-rank, no interpolation: these are wall-clock samples and a
        // percentile between two of them is not a measurement of anything.
        let quantile = |v: &[u64], q: f64| -> f64 {
            if v.is_empty() {
                return 0.0;
            }
            let mut s = v.to_vec();
            s.sort_unstable();
            let rank = ((q * s.len() as f64).ceil() as usize).clamp(1, s.len()) - 1;
            s[rank] as f64 / 1_000.0
        };
        let total: u64 = self.bcast_tick_ns.iter().chain(&self.other_tick_ns).sum();
        let n = self.bcast_tick_ns.len() + self.other_tick_ns.len();
        TickCost {
            broadcast_ticks: self.bcast_tick_ns.len(),
            ordinary_ticks: self.other_tick_ns.len(),
            broadcast_median_us: quantile(&self.bcast_tick_ns, 0.5),
            ordinary_median_us: quantile(&self.other_tick_ns, 0.5),
            broadcast_p99_us: quantile(&self.bcast_tick_ns, 0.99),
            broadcast_max_us: quantile(&self.bcast_tick_ns, 1.0),
            amortized_mean_us: if n == 0 {
                0.0
            } else {
                total as f64 / n as f64 / 1_000.0
            },
            broadcast_encodes: self
                .server
                .broadcast_encodes()
                .saturating_sub(self.bcast_encodes_at_reset),
        }
    }

    pub fn traffic(&self) -> Traffic {
        let mut t = Traffic::default();
        for c in &self.clients {
            for (a, b) in [(c.endpoint, SERVER_ENDPOINT), (SERVER_ENDPOINT, c.endpoint)] {
                if let Some(s) = self.net.link_stats(a, b) {
                    t.sent += s.sent;
                    t.delivered += s.delivered;
                    t.dropped += s.dropped;
                }
            }
        }
        t
    }

    fn route_server_out(&mut self, now_us: u64, out: Vec<(SocketAddr, Vec<u8>)>) {
        for (addr, dg) in out {
            // Unknown destinations (a raw member joined out of band) drop.
            if let Some(&idx) = self.endpoint_by_addr.get(&addr) {
                let ep = self.clients[idx].endpoint;
                self.net.send(now_us, SERVER_ENDPOINT, ep, dg);
            }
        }
    }

    fn forward_client(&mut self, now_us: u64, idx: usize, dgs: Vec<Vec<u8>>) {
        let ep = self.clients[idx].endpoint;
        let garbage = self.clients[idx].garbage;
        for dg in dgs {
            let payload = if garbage {
                self.garbage_bytes(dg.len())
            } else {
                dg
            };
            self.net.send(now_us, ep, SERVER_ENDPOINT, payload);
        }
    }

    fn garbage_bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len)
            .map(|_| {
                self.garbage_lcg = self
                    .garbage_lcg
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (self.garbage_lcg >> 33) as u8
            })
            .collect()
    }
}
