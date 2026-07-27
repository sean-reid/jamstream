//! Client-side session core for musicians and listeners. Sans-io: the
//! desktop app, headless CLI, and harness own the socket and clock, feed
//! datagrams and capture frames in, and pull playout audio and events out.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use jamstream_engine::{
    Channels, CodecError, Decoder, DriftCompensator, Encoder, JitterBuffer, JitterStats,
    MediaPacket, Pull, RedundancyPolicy,
};
use jamstream_protocol::control::{
    ControlLink, ControlMsg, DestinationStatus, MAX_AVATAR_BYTES, MemberInfo, StreamOp,
};
use jamstream_protocol::ids::{MemberId, Role, TokenId};
use jamstream_protocol::invite::Invite;
use jamstream_protocol::media::{FrameDuration, MediaFrame};
use jamstream_protocol::transport::{Initiator, Session, Welcome};
use jamstream_protocol::wire::{self, CHANNEL_CONTROL, CHANNEL_MEDIA, Packet};

use crate::SessionError;
use crate::avatar::{
    AVATAR_CHUNKS_PER_POLL, AvatarCache, AvatarHash, AvatarRx, AvatarTx, RxStep, avatar_hash,
};

const TICK_SAMPLES: u64 = 120;
const UPLINK_BITRATE: u32 = 128_000;
const CONNECTION_TIMEOUT_MS: u64 = 10_000;
const PING_INTERVAL_MS: u64 = 1_000;
/// First init resend after 500 ms, doubling to the cap while Connecting.
const INIT_RESEND_MS: u64 = 500;
const INIT_RESEND_MAX_MS: u64 = 2_000;
/// Reports of clean link required before redundancy turns back off.
const REDUNDANCY_OFF_HOLD: u32 = 10;
/// Playout steering samples the local jitter buffer this often, matching the
/// once-per-second cadence of the server's uplink Stats reports.
const PLAYOUT_STEER_FRAMES: u64 = 48_000;
/// Client-side avatar byte budget; roster-referenced hashes and our own
/// avatar are pinned, the rest evict least-recently-referenced first.
const AVATAR_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// PI controller steering a `DriftCompensator` from a jitter-buffer depth.
/// Depth persistently above the setpoint means production outruns
/// consumption, so the ratio steers down (consume more per frame); depth
/// pinned below steers up. The plant integrates the rate error at
/// 4e-4 frames/s/ppm, so KP puts the crossover at ~0.12 rad/s and KI the PI
/// zero a factor ~3.6 below it: convergent in tens of seconds, no ringing.
#[derive(Debug, Default)]
struct DepthSteer {
    fast: Option<f64>,
    slow: Option<f64>,
    integral_ppm: f64,
    steer_ppm: f64,
}

impl DepthSteer {
    const KP: f64 = 300.0;
    const KI: f64 = 10.0;
    /// Anti-windup: the integral alone never exceeds the compensator's
    /// +-500 ppm authority.
    const INTEGRAL_CLAMP: f64 = 450.0;
    /// Slow integral leak (tau ~500 updates) purges bias picked up while the
    /// jitter buffer's own grow/shrink moves depth for its own reasons; the
    /// residual error it costs is ~0.1 frames, well inside depth tolerance.
    const LEAK: f64 = 0.998;
    const ALPHA_FAST: f64 = 0.4;
    const ALPHA_SLOW: f64 = 0.03;

    /// One controller step from a depth sample (once per second). `floor`
    /// is the minimum setpoint in frames; the setpoint otherwise tracks a
    /// slow EWMA of the depth itself, so steering regulates the rate without
    /// fighting a buffer whose own target sits higher.
    fn update(&mut self, depth: f64, floor: f64) {
        let fast = *self.fast.get_or_insert(depth);
        let fast = fast + Self::ALPHA_FAST * (depth - fast);
        self.fast = Some(fast);
        let slow = *self.slow.get_or_insert(depth);
        let slow = slow + Self::ALPHA_SLOW * (depth - slow);
        self.slow = Some(slow);

        let e = fast - slow.max(floor);
        self.integral_ppm = (self.integral_ppm * Self::LEAK + Self::KI * e)
            .clamp(-Self::INTEGRAL_CLAMP, Self::INTEGRAL_CLAMP);
        self.steer_ppm = -(Self::KP * e + self.integral_ppm);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientState {
    Connecting,
    Joined,
    Rejected { ours: u16, theirs: u16 },
    Ejected { reason: String },
    TimedOut,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Joined,
    Roster(Vec<MemberInfo>),
    Chat {
        from: MemberId,
        text: String,
    },
    MetronomeChanged {
        bpm: u16,
        beats_per_bar: u8,
        enabled: bool,
    },
    RttSample {
        ms: f32,
    },
    /// The host changed one member's broadcast fader; the server relays it
    /// to everyone so UIs can mirror broadcast mix state.
    BroadcastMixChanged {
        target: MemberId,
        gain_db: f32,
        pan: f32,
        muted: bool,
    },
    /// The broadcast's per-destination state, as the server sees it. Sent to
    /// every member, so any client can show the room it is on air. Never
    /// carries a stream key.
    StreamStatus(Vec<DestinationStatus>),
    /// A member's avatar bytes are cached and hash-verified; fetch them
    /// with `avatar_bytes`. Emitted once per (member, hash).
    AvatarReady {
        member: MemberId,
        hash: [u8; 32],
    },
    Ejected {
        reason: String,
    },
    Rejected {
        ours: u16,
        theirs: u16,
    },
    TimedOut,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientStats {
    pub jitter: JitterStats,
    pub state: ClientState,
    pub rtt_ms_last: Option<f32>,
    /// Server's view of our uplink, from its latest Stats report. None
    /// until the first report arrives.
    pub uplink_loss_pct: Option<f32>,
    pub uplink_jitter_depth: Option<u16>,
    pub uplink_recovered_pct: Option<f32>,
    /// Whether capture frames currently carry the previous payload.
    pub redundancy_active: bool,
}

pub struct ClientCore {
    invite: Invite,
    state: ClientState,
    initiator: Option<Initiator>,
    /// The exact init bytes on the wire; the version reject MAC covers them.
    init_packet: Vec<u8>,
    session: Option<Session>,
    welcome: Option<Welcome>,
    link: ControlLink,
    jitter: JitterBuffer,
    /// Mono capture encoder; musicians only.
    encoder: Option<Encoder>,
    /// Stereo downlink decoder: Ms2_5 personal mix or Ms20 broadcast.
    decoder: Decoder,
    /// Listener decode scratch (one 20 ms stereo frame).
    decode_buf: Vec<f32>,
    /// Listener playout FIFO bridging 20 ms frames to 2.5 ms pulls.
    fifo: VecDeque<f32>,
    redundancy: RedundancyPolicy,
    /// Latest Stats report from the server: (loss pct, depth, recovered pct).
    uplink_report: Option<(f32, u16, f32)>,
    /// Raw-path capture pacing, created on first `push_capture_raw` and
    /// steered from the server's uplink depth reports. The exact-frame
    /// `push_capture` never touches it.
    capture_comp: Option<DriftCompensator>,
    capture_steer: DepthSteer,
    /// Raw-path playout pacing, steered from the local jitter buffer depth.
    playout_comp: Option<DriftCompensator>,
    playout_steer: DepthSteer,
    /// Resampled playout awaiting delivery to arbitrary-length raw pulls.
    playout_stage: VecDeque<f32>,
    playout_frames_since_steer: u64,
    /// Own avatar (hash, length); the bytes live pinned in the cache. Kept
    /// across reconnects and re-announced on every join.
    own_avatar: Option<(AvatarHash, u32)>,
    avatar_cache: AvatarCache,
    /// Hashes requested from the server and not yet received.
    avatar_requested: BTreeSet<AvatarHash>,
    /// Single inbound train; the server streams one train per link.
    avatar_rx: Option<AvatarRx>,
    /// Outbound trains answering the server's AvatarRequest for our bytes.
    avatar_tx: VecDeque<AvatarTx>,
    /// Latest roster, for cache pinning and AvatarReady fanout.
    roster: Vec<MemberInfo>,
    /// Hash last surfaced as AvatarReady per member, deduping the event.
    avatar_announced: BTreeMap<MemberId, AvatarHash>,
    prev_payload: Option<Vec<u8>>,
    pkt_buf: Vec<u8>,
    frames_sent: u64,
    events: Vec<ClientEvent>,
    last_server_ms: u64,
    last_ping_ms: u64,
    last_init_ms: u64,
    init_resend_ms: u64,
    rtt_ms_last: Option<f32>,
    ping_nonce: u32,
}

impl ClientCore {
    /// Starts a connection. The returned datagram is the handshake init and
    /// must go on the wire; `poll` resends it until the server answers.
    pub fn connect(invite: &Invite, now_ms: u64) -> Result<(Self, Vec<u8>), SessionError> {
        let (initiator, init_packet) = Initiator::new(invite)?;
        let (encoder, decoder, decode_len) = Self::media_state(invite.token.role)?;
        let core = Self {
            invite: invite.clone(),
            state: ClientState::Connecting,
            initiator: Some(initiator),
            init_packet: init_packet.clone(),
            session: None,
            welcome: None,
            link: ControlLink::new(),
            jitter: JitterBuffer::new(),
            encoder,
            decoder,
            decode_buf: vec![0.0; decode_len],
            fifo: VecDeque::new(),
            redundancy: RedundancyPolicy::new(REDUNDANCY_OFF_HOLD),
            uplink_report: None,
            capture_comp: None,
            capture_steer: DepthSteer::default(),
            playout_comp: None,
            playout_steer: DepthSteer::default(),
            playout_stage: VecDeque::new(),
            playout_frames_since_steer: 0,
            own_avatar: None,
            avatar_cache: AvatarCache::new(AVATAR_CACHE_BYTES),
            avatar_requested: BTreeSet::new(),
            avatar_rx: None,
            avatar_tx: VecDeque::new(),
            roster: Vec::new(),
            avatar_announced: BTreeMap::new(),
            prev_payload: None,
            pkt_buf: Vec::new(),
            frames_sent: 0,
            events: Vec::new(),
            last_server_ms: now_ms,
            last_ping_ms: now_ms,
            last_init_ms: now_ms,
            init_resend_ms: INIT_RESEND_MS,
            rtt_ms_last: None,
            ping_nonce: 0,
        };
        Ok((core, init_packet))
    }

    /// Fresh handshake with the same invite, e.g. after a timeout or a
    /// server-side disconnect. Stream state resets; the token is reused.
    pub fn reconnect(&mut self, now_ms: u64) -> Result<Vec<u8>, SessionError> {
        let (initiator, init_packet) = Initiator::new(&self.invite)?;
        let (encoder, decoder, decode_len) = Self::media_state(self.invite.token.role)?;
        self.state = ClientState::Connecting;
        self.initiator = Some(initiator);
        self.init_packet = init_packet.clone();
        self.session = None;
        self.welcome = None;
        self.link = ControlLink::new();
        self.jitter = JitterBuffer::new();
        self.encoder = encoder;
        self.decoder = decoder;
        self.decode_buf = vec![0.0; decode_len];
        self.fifo.clear();
        self.uplink_report = None;
        self.capture_comp = None;
        self.capture_steer = DepthSteer::default();
        self.playout_comp = None;
        self.playout_steer = DepthSteer::default();
        self.playout_stage.clear();
        self.playout_frames_since_steer = 0;
        // Transfers are connection-scoped; the cache and own avatar are
        // identity and survive, so a rejoin re-announces without re-upload.
        self.avatar_requested.clear();
        self.avatar_rx = None;
        self.avatar_tx.clear();
        self.roster.clear();
        self.avatar_announced.clear();
        self.prev_payload = None;
        self.frames_sent = 0;
        self.last_server_ms = now_ms;
        self.last_init_ms = now_ms;
        self.init_resend_ms = INIT_RESEND_MS;
        Ok(init_packet)
    }

    /// Feeds one datagram from the socket. Returns datagrams to send back.
    pub fn handle_datagram(&mut self, now_ms: u64, data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let Ok(pkt) = wire::parse(data) else {
            return out;
        };
        match pkt {
            Packet::HandshakeResp { noise } => {
                if self.state != ClientState::Connecting {
                    return out;
                }
                let Some(initiator) = self.initiator.take() else {
                    return out;
                };
                match initiator.finish(noise) {
                    Ok((session, welcome)) => {
                        self.session = Some(session);
                        self.welcome = Some(welcome);
                        self.state = ClientState::Joined;
                        self.last_server_ms = now_ms;
                        self.last_ping_ms = now_ms;
                        self.events.push(ClientEvent::Joined);
                        // Re-announce on every join: on a cache hit the
                        // server asks for nothing and no chunk moves.
                        if let Some((hash, len)) = self.own_avatar {
                            let _ = self.link.send(ControlMsg::SetAvatar { hash, len });
                        }
                    }
                    Err(_) => {
                        // A forged or corrupt response consumed the handshake
                        // state; rebuild so poll() keeps retrying. The server
                        // may hold a half-open admission until its timeout.
                        tracing::warn!("handshake response failed to verify");
                        if let Ok((initiator, init_packet)) = Initiator::new(&self.invite) {
                            self.initiator = Some(initiator);
                            self.init_packet = init_packet;
                        }
                    }
                }
            }
            Packet::VersionReject { ours, theirs, mac } => {
                // Only honored when the MAC binds the server key from our
                // invite to the exact init packet we sent.
                if self.state == ClientState::Connecting
                    && wire::verify_version_reject(
                        &self.invite.server_pk,
                        ours,
                        theirs,
                        &mac,
                        &self.init_packet,
                    )
                {
                    // Wire fields are from the server's perspective; ours is
                    // the version this client speaks.
                    self.state = ClientState::Rejected {
                        ours: theirs,
                        theirs: ours,
                    };
                    self.events.push(ClientEvent::Rejected {
                        ours: theirs,
                        theirs: ours,
                    });
                }
            }
            Packet::Transport {
                counter,
                ciphertext,
                ..
            } => {
                let Some(session) = self.session.as_mut() else {
                    return out;
                };
                let Ok(plain) = session.open(counter, ciphertext) else {
                    return out;
                };
                self.last_server_ms = now_ms;
                match wire::split_channel(&plain) {
                    Ok((CHANNEL_MEDIA, _)) => {
                        if let Ok(f) = MediaFrame::decode(&plain) {
                            self.jitter.push(MediaPacket {
                                seq: f.seq,
                                timestamp: f.timestamp,
                                payload: f.payload.to_vec(),
                                redundant: f.redundant.map(<[u8]>::to_vec),
                            });
                        }
                    }
                    Ok((CHANNEL_CONTROL, _)) => {
                        if let Ok(msgs) = self.link.receive(&plain) {
                            for msg in msgs {
                                self.handle_control(now_ms, msg);
                            }
                        }
                        // Acks and Pongs go out in the same call.
                        self.flush_link(now_ms, &mut out);
                    }
                    _ => {}
                }
            }
            // Clients never receive an init.
            Packet::HandshakeInit { .. } => {}
        }
        out
    }

    /// One 2.5 ms mono capture frame in, sealed media datagrams out.
    /// Empty unless Joined as a musician.
    pub fn push_capture(&mut self, _now_ms: u64, pcm_mono_120: &[f32]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if self.state != ClientState::Joined {
            return out;
        }
        let (Some(encoder), Some(welcome)) = (self.encoder.as_mut(), self.welcome.as_ref()) else {
            return out;
        };
        if encoder.encode(pcm_mono_120, &mut self.pkt_buf).is_err() {
            tracing::warn!("capture encode failed");
            return out;
        }
        let redundant = if self.redundancy.active() {
            self.prev_payload.as_deref()
        } else {
            None
        };
        let frame = MediaFrame {
            seq: self.frames_sent as u32,
            timestamp: welcome.sample_clock + self.frames_sent * TICK_SAMPLES,
            duration: FrameDuration::Ms2_5,
            stereo: false,
            payload: &self.pkt_buf,
            redundant,
        }
        .encode();
        if let Some(session) = self.session.as_mut()
            && let Ok(p) = session.seal(welcome.member_id, &frame)
        {
            out.push(p);
        }
        self.frames_sent += 1;
        let prev = self.prev_payload.get_or_insert_with(Vec::new);
        std::mem::swap(prev, &mut self.pkt_buf);
        out
    }

    /// Fills one 2.5 ms interleaved stereo playout frame (240 floats).
    /// Silence while the jitter buffer is still filling.
    pub fn pull_playout(&mut self, out_stereo_240: &mut [f32]) {
        match self.invite.token.role {
            Role::Musician => {
                let decoded = match self.jitter.pull() {
                    Pull::Frame(p) | Pull::Recovered(p) => {
                        self.decoder.decode(Some(&p), out_stereo_240, false)
                    }
                    Pull::Missing => self.decoder.decode(None, out_stereo_240, false),
                    Pull::Waiting => {
                        out_stereo_240.fill(0.0);
                        Ok(())
                    }
                };
                if decoded.is_err() {
                    out_stereo_240.fill(0.0);
                }
            }
            Role::Listener => {
                // Bridge 20 ms broadcast frames to the 2.5 ms pull cadence.
                while self.fifo.len() < out_stereo_240.len() {
                    let decoded = match self.jitter.pull() {
                        Pull::Frame(p) | Pull::Recovered(p) => {
                            self.decoder.decode(Some(&p), &mut self.decode_buf, false)
                        }
                        Pull::Missing => self.decoder.decode(None, &mut self.decode_buf, false),
                        Pull::Waiting => break,
                    };
                    if decoded.is_err() {
                        self.decode_buf.fill(0.0);
                    }
                    self.fifo.extend(self.decode_buf.iter().copied());
                }
                for slot in out_stereo_240.iter_mut() {
                    *slot = self.fifo.pop_front().unwrap_or(0.0);
                }
            }
        }
    }

    /// Device-paced capture: arbitrary-length mono samples in, one sealed
    /// media datagram out per completed 2.5 ms frame (zero or several per
    /// call). A capture `DriftCompensator`, steered from the server's view
    /// of our uplink, repaces the device clock onto the frame clock so
    /// sustained drift never reaches the server's jitter buffer.
    pub fn push_capture_raw(&mut self, now_ms: u64, samples: &[f32]) -> Vec<Vec<u8>> {
        let mut comp = self
            .capture_comp
            .take()
            .unwrap_or_else(|| DriftCompensator::new(TICK_SAMPLES as usize, 1));
        comp.steer(self.capture_steer.steer_ppm);
        comp.push(samples);
        let mut out = Vec::new();
        let mut frame = [0.0f32; TICK_SAMPLES as usize];
        while comp.pull_frame(&mut frame) {
            out.append(&mut self.push_capture(now_ms, &frame));
        }
        self.capture_comp = Some(comp);
        out
    }

    /// Device-paced playout: fills an arbitrary-length interleaved stereo
    /// buffer from the jitter pull path through a playout
    /// `DriftCompensator` steered by the local depth-vs-target error.
    pub fn pull_playout_raw(&mut self, out: &mut [f32]) {
        debug_assert_eq!(out.len() % 2, 0, "interleaved stereo");
        let mut comp = self
            .playout_comp
            .take()
            .unwrap_or_else(|| DriftCompensator::new(TICK_SAMPLES as usize, 2));
        comp.steer(self.playout_steer.steer_ppm);
        let mut filled = 0;
        while filled < out.len() {
            if let Some(s) = self.playout_stage.pop_front() {
                out[filled] = s;
                filled += 1;
                continue;
            }
            let mut frame = [0.0f32; TICK_SAMPLES as usize * 2];
            while !comp.pull_frame(&mut frame) {
                // The jitter pull always yields audio (silence while
                // waiting, PLC on a miss), so this feeds until a chunk fits.
                let mut decoded = [0.0f32; TICK_SAMPLES as usize * 2];
                self.pull_playout(&mut decoded);
                comp.push(&decoded);
            }
            self.playout_stage.extend(frame);
        }
        self.playout_frames_since_steer += (out.len() / 2) as u64;
        if self.playout_frames_since_steer >= PLAYOUT_STEER_FRAMES {
            self.playout_frames_since_steer = 0;
            let js = self.jitter.stats();
            // Floor at target + 1: the slack position redundancy needs, and
            // the highest depth the shrink path leaves alone.
            self.playout_steer
                .update(js.depth_frames as f64, (js.target_frames + 1) as f64);
        }
        self.playout_comp = Some(comp);
    }

    /// Periodic housekeeping: handshake resends, keepalive pings, redundancy
    /// policy updates, control retransmits, and the connection timeout.
    pub fn poll(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        match self.state {
            ClientState::Connecting => {
                if now_ms.saturating_sub(self.last_server_ms) >= CONNECTION_TIMEOUT_MS {
                    self.state = ClientState::TimedOut;
                    self.events.push(ClientEvent::TimedOut);
                } else if now_ms.saturating_sub(self.last_init_ms) >= self.init_resend_ms {
                    // Same bytes every time: the server answers a resent
                    // identical init with its cached response.
                    self.last_init_ms = now_ms;
                    self.init_resend_ms = (self.init_resend_ms * 2).min(INIT_RESEND_MAX_MS);
                    out.push(self.init_packet.clone());
                }
            }
            ClientState::Joined => {
                if now_ms.saturating_sub(self.last_server_ms) >= CONNECTION_TIMEOUT_MS {
                    self.state = ClientState::TimedOut;
                    self.events.push(ClientEvent::TimedOut);
                    return out;
                }
                if now_ms.saturating_sub(self.last_ping_ms) >= PING_INTERVAL_MS {
                    self.last_ping_ms = now_ms;
                    self.ping_nonce = self.ping_nonce.wrapping_add(1);
                    let _ = self.link.send(ControlMsg::Ping {
                        nonce: self.ping_nonce,
                        sent_ms: now_ms,
                    });
                }
                // Avatar upload pacing, same rule as the server: at most a
                // couple of chunks per poll so bulk bytes never starve
                // pings or chat on the ordered link.
                let mut fed = 0;
                while fed < AVATAR_CHUNKS_PER_POLL {
                    let Some(tx) = self.avatar_tx.front_mut() else {
                        break;
                    };
                    match self
                        .avatar_cache
                        .get(tx.hash())
                        .and_then(|bytes| tx.next_chunk(bytes))
                    {
                        Some(chunk) => {
                            let _ = self.link.send(chunk);
                            fed += 1;
                        }
                        None => {
                            self.avatar_tx.pop_front();
                        }
                    }
                }
                // Redundancy is fed by the server's Stats reports as they
                // arrive (once a second); no downlink proxy here.
                self.flush_link(now_ms, &mut out);
            }
            _ => {}
        }
        out
    }

    /// Drains accumulated events.
    pub fn events(&mut self) -> Vec<ClientEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn state(&self) -> &ClientState {
        &self.state
    }

    pub fn member_id(&self) -> Option<MemberId> {
        self.welcome.as_ref().map(|w| w.member_id)
    }

    pub fn stats(&self) -> ClientStats {
        ClientStats {
            jitter: self.jitter.stats(),
            state: self.state.clone(),
            rtt_ms_last: self.rtt_ms_last,
            uplink_loss_pct: self.uplink_report.map(|(loss, _, _)| loss),
            uplink_jitter_depth: self.uplink_report.map(|(_, depth, _)| depth),
            uplink_recovered_pct: self.uplink_report.map(|(_, _, rec)| rec),
            redundancy_active: self.redundancy.active(),
        }
    }

    /// Sets or replaces this member's avatar. The bytes are hashed
    /// (Blake2s-256, the avatar's identity), cached locally, and the hash
    /// announced; the server pulls the bytes only when its cache lacks
    /// them. Callable before joining: the announce then rides every join.
    pub fn set_avatar(&mut self, bytes: &[u8]) -> Result<[u8; 32], SessionError> {
        if bytes.is_empty() || bytes.len() > MAX_AVATAR_BYTES {
            return Err(SessionError::InvalidParam("avatar size out of range"));
        }
        let hash = avatar_hash(bytes);
        let mut pins = self.avatar_pins();
        pins.insert(hash);
        self.avatar_cache.insert(hash, bytes.to_vec(), &pins);
        self.own_avatar = Some((hash, bytes.len() as u32));
        // Drop queued trains for a replaced avatar so the server never
        // sees stale chunks after the new announce.
        self.avatar_tx.retain(|t| *t.hash() == hash);
        if self.state == ClientState::Joined {
            self.link.send(ControlMsg::SetAvatar {
                hash,
                len: bytes.len() as u32,
            })?;
        }
        Ok(hash)
    }

    /// Verified avatar bytes for a hash seen on the roster, if cached.
    pub fn avatar_bytes(&self, hash: &[u8; 32]) -> Option<&[u8]> {
        self.avatar_cache.get(hash)
    }

    pub fn send_chat(&mut self, text: &str) -> Result<(), SessionError> {
        let from = self.require_joined()?;
        self.link.send(ControlMsg::Chat {
            from,
            text: text.to_owned(),
        })?;
        Ok(())
    }

    pub fn set_fader(
        &mut self,
        target: MemberId,
        gain_db: f32,
        pan: f32,
        muted: bool,
    ) -> Result<(), SessionError> {
        self.require_joined()?;
        if !gain_db.is_finite() || !(-96.0..=24.0).contains(&gain_db) {
            return Err(SessionError::InvalidParam("gain_db out of range"));
        }
        if !pan.is_finite() || !(-1.0..=1.0).contains(&pan) {
            return Err(SessionError::InvalidParam("pan out of range"));
        }
        self.link.send(ControlMsg::MixerSet {
            target,
            gain_db,
            pan,
            muted,
        })?;
        Ok(())
    }

    /// Host-only server-side; shapes one member's fader in the broadcast
    /// mix. Sent regardless of our member id: enforcement is the server's.
    pub fn set_broadcast_fader(
        &mut self,
        target: MemberId,
        gain_db: f32,
        pan: f32,
        muted: bool,
    ) -> Result<(), SessionError> {
        self.require_joined()?;
        if !gain_db.is_finite() || !(-96.0..=24.0).contains(&gain_db) {
            return Err(SessionError::InvalidParam("gain_db out of range"));
        }
        if !pan.is_finite() || !(-1.0..=1.0).contains(&pan) {
            return Err(SessionError::InvalidParam("pan out of range"));
        }
        self.link.send(ControlMsg::BroadcastMixSet {
            target,
            gain_db,
            pan,
            muted,
        })?;
        Ok(())
    }

    /// Host-only server-side; while enabled the host's downlink carries the
    /// broadcast mix (own signal included) instead of the personal mix.
    pub fn set_broadcast_audition(&mut self, enabled: bool) -> Result<(), SessionError> {
        self.require_joined()?;
        self.link.send(ControlMsg::BroadcastAudition { enabled })?;
        Ok(())
    }

    /// Host-only server-side: add or remove a broadcast destination, or start
    /// and stop the stream. The server counts a violation against anyone else.
    /// A key inside the op is already inside the transport encryption; the
    /// server keeps it in memory only.
    pub fn stream_ctl(&mut self, op: StreamOp) -> Result<(), SessionError> {
        self.require_joined()?;
        self.link.send(ControlMsg::StreamCtl { op })?;
        Ok(())
    }

    /// Host-only server-side; the server ignores it from anyone else.
    pub fn set_metronome(
        &mut self,
        bpm: u16,
        beats_per_bar: u8,
        enabled: bool,
    ) -> Result<(), SessionError> {
        self.require_joined()?;
        if !(1..=400).contains(&bpm) {
            return Err(SessionError::InvalidParam("bpm out of range"));
        }
        if !(1..=16).contains(&beats_per_bar) {
            return Err(SessionError::InvalidParam("beats_per_bar out of range"));
        }
        self.link.send(ControlMsg::MetronomeSet {
            bpm,
            beats_per_bar,
            enabled,
        })?;
        Ok(())
    }

    pub fn set_click(&mut self, enabled: bool) -> Result<(), SessionError> {
        self.require_joined()?;
        self.link.send(ControlMsg::ClickEnable { enabled })?;
        Ok(())
    }

    /// Host-only server-side; invalidates one invite and ejects its member.
    pub fn revoke(&mut self, jti: TokenId) -> Result<(), SessionError> {
        self.require_joined()?;
        self.link.send(ControlMsg::Revoke { jti })?;
        Ok(())
    }

    pub fn leave(&mut self, reason: &str) -> Result<(), SessionError> {
        self.require_joined()?;
        self.link.send(ControlMsg::Bye {
            reason: reason.to_owned(),
        })?;
        Ok(())
    }

    fn media_state(role: Role) -> Result<(Option<Encoder>, Decoder, usize), CodecError> {
        match role {
            Role::Musician => Ok((
                Some(Encoder::new(
                    Channels::Mono,
                    FrameDuration::Ms2_5,
                    UPLINK_BITRATE,
                )?),
                Decoder::new(Channels::Stereo, FrameDuration::Ms2_5)?,
                FrameDuration::Ms2_5.samples() as usize * 2,
            )),
            Role::Listener => Ok((
                None,
                Decoder::new(Channels::Stereo, FrameDuration::Ms20)?,
                FrameDuration::Ms20.samples() as usize * 2,
            )),
        }
    }

    fn handle_control(&mut self, now_ms: u64, msg: ControlMsg) {
        match msg {
            ControlMsg::Roster(members) => {
                self.sync_avatars(&members);
                self.roster = members.clone();
                self.events.push(ClientEvent::Roster(members));
            }
            ControlMsg::Chat { from, text } => self.events.push(ClientEvent::Chat { from, text }),
            ControlMsg::MetronomeSet {
                bpm,
                beats_per_bar,
                enabled,
            } => self.events.push(ClientEvent::MetronomeChanged {
                bpm,
                beats_per_bar,
                enabled,
            }),
            ControlMsg::Ping { nonce, sent_ms } => {
                let _ = self.link.send(ControlMsg::Pong { nonce, sent_ms });
            }
            ControlMsg::Pong { sent_ms, .. } => {
                let ms = now_ms.saturating_sub(sent_ms) as f32;
                self.rtt_ms_last = Some(ms);
                self.events.push(ClientEvent::RttSample { ms });
            }
            ControlMsg::Stats {
                uplink_loss_pct,
                uplink_jitter_depth,
                uplink_recovered_pct,
            } => {
                self.uplink_report =
                    Some((uplink_loss_pct, uplink_jitter_depth, uplink_recovered_pct));
                // The server's view of our uplink drives the redundancy
                // decision; the policy sanitizes garbage values itself.
                self.redundancy.report(uplink_loss_pct / 100.0);
                // And its jitter depth drives raw-path capture pacing.
                // Floor at 2: the server buffer's clean-link target plus
                // its one frame of redundancy slack.
                self.capture_steer
                    .update(f64::from(uplink_jitter_depth), 2.0);
            }
            ControlMsg::Bye { reason } => {
                self.state = ClientState::Ejected {
                    reason: reason.clone(),
                };
                self.events.push(ClientEvent::Ejected { reason });
            }
            ControlMsg::BroadcastMixSet {
                target,
                gain_db,
                pan,
                muted,
            } => self.events.push(ClientEvent::BroadcastMixChanged {
                target,
                gain_db,
                pan,
                muted,
            }),
            // The server wants our avatar bytes. Anything but our current
            // hash is a stale request from before a replacement; per client
            // style it is dropped silently rather than flagged.
            ControlMsg::AvatarRequest { hash } => {
                if self.own_avatar.is_some_and(|(h, _)| h == hash)
                    && !self.avatar_tx.iter().any(|t| *t.hash() == hash)
                {
                    self.avatar_tx.push_back(AvatarTx::new(hash));
                }
            }
            ControlMsg::AvatarChunk {
                hash,
                index,
                total,
                data,
            } => self.handle_avatar_chunk(hash, index, total, &data),
            ControlMsg::StreamStatus { destinations } => {
                self.events.push(ClientEvent::StreamStatus(destinations));
            }
            // The server never sends these; ignore.
            ControlMsg::MixerSet { .. }
            | ControlMsg::ClickEnable { .. }
            | ControlMsg::BroadcastAudition { .. }
            | ControlMsg::Revoke { .. }
            | ControlMsg::SetAvatar { .. }
            | ControlMsg::StreamCtl { .. } => {}
        }
    }

    /// Requests roster hashes we lack and surfaces AvatarReady for the ones
    /// already cached (once per member and hash).
    fn sync_avatars(&mut self, roster: &[MemberInfo]) {
        self.avatar_announced
            .retain(|id, _| roster.iter().any(|m| m.id == *id));
        for mi in roster {
            let Some(hash) = mi.avatar_hash else {
                self.avatar_announced.remove(&mi.id);
                continue;
            };
            if self.avatar_cache.contains(&hash) {
                self.avatar_cache.touch(&hash);
                self.announce(mi.id, hash);
            } else if self.avatar_requested.insert(hash) {
                let _ = self.link.send(ControlMsg::AvatarRequest { hash });
            }
        }
    }

    /// One chunk from the server. Malformed or unsolicited trains are
    /// dropped silently: the client has no violation channel, and a stale
    /// train racing a replacement is expected, not hostile. A failed fetch
    /// is not retried until the hash next appears on a fresh connection.
    fn handle_avatar_chunk(&mut self, hash: AvatarHash, index: u16, total: u16, data: &[u8]) {
        if index == 0 && self.avatar_requested.contains(&hash) {
            self.avatar_rx = Some(AvatarRx::new(hash, None));
        }
        let Some(rx) = self.avatar_rx.as_mut().filter(|rx| *rx.hash() == hash) else {
            return;
        };
        match rx.push(index, total, data) {
            Ok(RxStep::More) => {}
            Ok(RxStep::Done(bytes)) => {
                self.avatar_rx = None;
                self.avatar_requested.remove(&hash);
                let pins = self.avatar_pins();
                self.avatar_cache.insert(hash, bytes, &pins);
                let ready: Vec<MemberId> = self
                    .roster
                    .iter()
                    .filter(|m| m.avatar_hash == Some(hash))
                    .map(|m| m.id)
                    .collect();
                for id in ready {
                    self.announce(id, hash);
                }
            }
            Err(_) => self.avatar_rx = None,
        }
    }

    fn announce(&mut self, member: MemberId, hash: AvatarHash) {
        if self.avatar_announced.get(&member) != Some(&hash) {
            self.avatar_announced.insert(member, hash);
            self.events.push(ClientEvent::AvatarReady { member, hash });
        }
    }

    /// Hashes eviction must never remove: everything the current roster
    /// references plus our own avatar.
    fn avatar_pins(&self) -> BTreeSet<AvatarHash> {
        let mut pins: BTreeSet<AvatarHash> =
            self.roster.iter().filter_map(|m| m.avatar_hash).collect();
        if let Some((h, _)) = self.own_avatar {
            pins.insert(h);
        }
        pins
    }

    fn flush_link(&mut self, now_ms: u64, out: &mut Vec<Vec<u8>>) {
        let Some(member) = self.welcome.as_ref().map(|w| w.member_id) else {
            return;
        };
        let dgs = self.link.poll(now_ms);
        if let Some(session) = self.session.as_mut() {
            for dg in dgs {
                if let Ok(p) = session.seal(member, &dg) {
                    out.push(p);
                }
            }
        }
    }

    fn require_joined(&self) -> Result<MemberId, SessionError> {
        if self.state != ClientState::Joined {
            return Err(SessionError::NotJoined);
        }
        self.member_id().ok_or(SessionError::NotJoined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamstream_protocol::ids::SessionId;
    use jamstream_protocol::invite::{Issuer, Token};
    use jamstream_protocol::transport::generate_keypair;
    use jamstream_protocol::wire::TYPE_HANDSHAKE_INIT;

    fn invite(role: Role) -> Invite {
        let issuer = Issuer::generate();
        let kp = generate_keypair();
        issuer.mint(
            SessionId::generate(),
            vec!["10.0.0.1:5000".parse().unwrap()],
            kp.public,
            Token {
                member_id: MemberId(1),
                role,
                name_hint: Some("t".into()),
                expires_unix: u64::MAX,
                jti: TokenId::generate(),
            },
        )
    }

    #[test]
    fn connect_emits_a_handshake_init() {
        let (core, first) = ClientCore::connect(&invite(Role::Musician), 0).unwrap();
        assert_eq!(first[0], TYPE_HANDSHAKE_INIT);
        assert_eq!(*core.state(), ClientState::Connecting);
        assert!(core.member_id().is_none());
    }

    #[test]
    fn control_senders_require_join() {
        let (mut core, _) = ClientCore::connect(&invite(Role::Musician), 0).unwrap();
        assert!(matches!(core.send_chat("hi"), Err(SessionError::NotJoined)));
        assert!(matches!(
            core.set_fader(MemberId(2), 0.0, 0.0, false),
            Err(SessionError::NotJoined)
        ));
        assert!(matches!(
            core.set_broadcast_fader(MemberId(2), 0.0, 0.0, false),
            Err(SessionError::NotJoined)
        ));
        assert!(matches!(
            core.set_broadcast_audition(true),
            Err(SessionError::NotJoined)
        ));
        assert!(matches!(core.leave("bye"), Err(SessionError::NotJoined)));
    }

    #[test]
    fn forged_version_reject_is_ignored() {
        let inv = invite(Role::Musician);
        let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
        // MAC keyed on the wrong server key.
        let forged = wire::build_version_reject(&[9u8; 32], 2, 1, &init);
        assert!(core.handle_datagram(1, &forged).is_empty());
        assert_eq!(*core.state(), ClientState::Connecting);
        assert!(core.events().is_empty());
        // The genuine article, MAC'd with the invite's server key over our
        // init packet, is honored and mapped to the client's perspective.
        let real = wire::build_version_reject(&inv.server_pk, 2, 1, &init);
        core.handle_datagram(2, &real);
        assert_eq!(*core.state(), ClientState::Rejected { ours: 1, theirs: 2 });
        assert_eq!(
            core.events(),
            vec![ClientEvent::Rejected { ours: 1, theirs: 2 }]
        );
    }

    #[test]
    fn init_resend_backs_off_while_connecting() {
        let (mut core, _) = ClientCore::connect(&invite(Role::Musician), 0).unwrap();
        assert!(core.poll(499).is_empty());
        assert_eq!(core.poll(500).len(), 1);
        // Doubling: 1 s, then 2 s, then capped at 2 s.
        assert!(core.poll(1_400).is_empty());
        assert_eq!(core.poll(1_500).len(), 1);
        assert!(core.poll(3_400).is_empty());
        assert_eq!(core.poll(3_500).len(), 1);
        assert!(core.poll(5_400).is_empty());
        assert_eq!(core.poll(5_500).len(), 1);
    }

    #[test]
    fn connecting_times_out_after_ten_seconds() {
        let (mut core, _) = ClientCore::connect(&invite(Role::Musician), 0).unwrap();
        assert!(core.poll(9_999).len() <= 1);
        core.poll(10_000);
        assert_eq!(*core.state(), ClientState::TimedOut);
        assert!(core.events().contains(&ClientEvent::TimedOut));
    }

    #[test]
    fn push_capture_is_empty_until_joined() {
        let (mut core, _) = ClientCore::connect(&invite(Role::Musician), 0).unwrap();
        assert!(core.push_capture(0, &[0.0; 120]).is_empty());
        let (mut listener, _) = ClientCore::connect(&invite(Role::Listener), 0).unwrap();
        assert!(listener.push_capture(0, &[0.0; 120]).is_empty());
    }

    #[test]
    fn raw_capture_accepts_odd_lengths_and_is_empty_until_joined() {
        let (mut core, _) = ClientCore::connect(&invite(Role::Musician), 0).unwrap();
        // Device-paced deliveries that straddle frame boundaries.
        assert!(core.push_capture_raw(0, &[0.0; 250]).is_empty());
        assert!(core.push_capture_raw(0, &[0.0; 119]).is_empty());
        assert!(core.push_capture_raw(0, &[0.0; 1]).is_empty());
    }

    #[test]
    fn raw_playout_fills_arbitrary_lengths_with_silence_before_media() {
        let (mut core, _) = ClientCore::connect(&invite(Role::Musician), 0).unwrap();
        // Not a multiple of the 2.5 ms frame; the stage bridges the rest.
        let mut small = vec![1.0f32; 202];
        core.pull_playout_raw(&mut small);
        assert!(small.iter().all(|&s| s == 0.0));
        let mut large = vec![1.0f32; 966];
        core.pull_playout_raw(&mut large);
        assert!(large.iter().all(|&s| s == 0.0));
    }
}
