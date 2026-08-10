//! Client-side session core for musicians and listeners. Sans-io: the
//! desktop app, headless CLI, and harness own the socket and clock, feed
//! datagrams and capture frames in, and pull playout audio and events out.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;

use jamstream_engine::{
    Channels, CodecError, Decoder, DriftCompensator, Encoder, JitterBuffer, JitterStats,
    MediaPacket, Pull, RedundancyPolicy,
};
use jamstream_protocol::control::{
    BroadcastReadiness, ControlLink, ControlMsg, DestinationStatus, MAX_AVATAR_BYTES, MAX_NAME_LEN,
    MemberInfo, RecordOp, RecordingState, StreamOp,
};
use jamstream_protocol::ids::{MemberId, Role, TokenId};
use jamstream_protocol::invite::Invite;
use jamstream_protocol::media::{FrameDuration, MediaFrame};
use jamstream_protocol::transport::{Initiator, Session, Welcome, cookie_reply_key};
use jamstream_protocol::wire::{
    self, CHANNEL_CONTROL, CHANNEL_MEDIA, CookieReplyKey, Packet, RejectKey,
};

use crate::SessionError;
use crate::avatar::{
    AVATAR_CHUNKS_PER_POLL, AvatarCache, AvatarHash, AvatarRx, AvatarTx, RxStep, avatar_hash,
};
use crate::limits::TokenBucket;

/// The mix tick, in this file's units. The server's constant, not a second
/// copy of it: the two halves of one session count the same samples.
const TICK_SAMPLES: u64 = crate::server::TICK_SAMPLES as u64;
const UPLINK_BITRATE: u32 = 128_000;
const CONNECTION_TIMEOUT_MS: u64 = 10_000;
const PING_INTERVAL_MS: u64 = 1_000;
/// First init resend after 500 ms, doubling to the cap while Connecting.
const INIT_RESEND_MS: u64 = 500;
const INIT_RESEND_MAX_MS: u64 = 2_000;
/// A rejected client keeps trying, slowly. A reject is authenticated, so it
/// is a true statement about the server that answered, but it is a statement
/// about one moment: a migration or a redeploy can answer the next init, and
/// a client that gave up for good would need the user to restart it.
const REJECT_RETRY_MS: u64 = 5_000;
const REJECT_RETRY_MAX_MS: u64 = 60_000;
/// Cookied inits this client will send the instant a challenge arrives,
/// and how fast that allowance comes back.
///
/// Answering at once saves up to a whole resend interval, so it is worth
/// doing. A challenge only opens against the exact init this client sent,
/// under a key derived from `server_pk`, so a forger needs both; an invite
/// holder watching the path has both, and without a budget that would be a
/// lever for making this client emit a packet per forged challenge, at
/// whatever rate the attacker chose. Four covers a rotation and a retry;
/// past that the resend timer carries it.
const COOKIE_ANSWER_BURST: u32 = 4;
const COOKIE_ANSWER_PER_SEC: u32 = 1;
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
    /// The recorder's state, as the server sees it: a take starting or
    /// ending, an upload draining, or a failure with the reason. Sent to
    /// every member, so any client can show the room it is being recorded.
    RecordStatus {
        state: RecordingState,
        stems: bool,
    },
    /// Whether the session can broadcast at all, as the server's relay probe
    /// answers it. Arrives on change and once at join; a session that never
    /// emits one is a server that cannot answer, which reads as "assume it
    /// works" rather than as a failure.
    BroadcastReadiness(BroadcastReadiness),
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
    /// The server says, in a packet only it could have produced, that the
    /// role this invite names has no free seat. Emitted at most once per
    /// connection attempt; the client keeps trying, because a seat frees
    /// when somebody leaves. See [`ClientCore::session_full`].
    SessionFull,
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
    /// Whether the server has said this invite's role is full. Still
    /// `Connecting`: the retry is what gets this client in when a seat frees.
    pub session_full: bool,
}

pub struct ClientCore {
    invite: Invite,
    state: ClientState,
    initiator: Option<Initiator>,
    /// The exact init bytes on the wire; the version reject MAC covers them.
    init_packet: Vec<u8>,
    /// Authenticates either reject answering this connection attempt.
    /// Derived from the handshake, so it changes on every reconnect.
    reject_key: Option<RejectKey>,
    /// Set by an authenticated capacity reject: the role this invite names
    /// is full. Suppresses the connection timeout, because a timeout would
    /// report the wrong thing about a session that is answering.
    session_full: bool,
    /// The same first flight wrapped in the cookie a challenge handed over,
    /// sent alongside the plain init and never instead of it.
    cookied_init: Option<Vec<u8>>,
    cookie_answers: TokenBucket,
    /// Opens cookie challenges; a pure function of the invite's `server_pk`,
    /// derived once. See [`jamstream_protocol::transport::cookie_reply_key`].
    cookie_reply_key: CookieReplyKey,
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
    /// Own display name, kept and re-announced exactly like the avatar: the
    /// server rebuilds a member's name from the token's hint at every
    /// admission, so a name that did not ride each join would silently
    /// revert on reconnect.
    own_name: Option<String>,
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

/// The invite's candidate server addresses, tried in order until one
/// answers.
///
/// Sans-io like the rest of this file: it owns no socket and no clock. It
/// only says which address the driver should be talking to, and moves on
/// when the driver reports that the current one timed out.
///
/// An invite has carried a list since the beginning and nothing offered a
/// second entry, so every driver read `addresses[0]` and stopped. A locally
/// hosted session now offers loopback as well as the LAN address, which is
/// what lets a same-machine join stay on the machine, so the second entry
/// has to be reachable from somewhere.
///
/// Rotation is cyclic. The window a driver gives each address is its own
/// business, and a driver with a long overall deadline should come back
/// round rather than give up on an address that was merely slow to boot.
///
/// Trying an address that turns out to belong to a stranger is safe: the
/// handshake is Noise IK against the server static key in the invite, so
/// nothing but that server can complete it. A wrong address costs one
/// timeout, not a wrong session.
#[derive(Debug, Clone)]
pub struct ServerCandidates {
    addresses: Vec<SocketAddr>,
    idx: usize,
}

impl ServerCandidates {
    /// Fails on an invite with no addresses at all. `Invite::decode`
    /// already refuses those, so this is for an invite built in memory.
    pub fn new(invite: &Invite) -> Result<Self, SessionError> {
        if invite.addresses.is_empty() {
            return Err(SessionError::Protocol(jamstream_protocol::Error::Invite(
                "has no server address",
            )));
        }
        Ok(ServerCandidates {
            addresses: invite.addresses.clone(),
            idx: 0,
        })
    }

    /// The address to be talking to now.
    pub fn current(&self) -> SocketAddr {
        self.addresses[self.idx]
    }

    /// True when there is more than one address, so a timeout is worth
    /// answering with a different destination rather than the same one.
    pub fn has_alternatives(&self) -> bool {
        self.addresses.len() > 1
    }

    /// Moves to the next candidate and returns it, wrapping at the end.
    pub fn advance(&mut self) -> SocketAddr {
        self.idx = (self.idx + 1) % self.addresses.len();
        self.current()
    }
}

impl ClientCore {
    /// Starts a connection. The returned datagram is the handshake init and
    /// must go on the wire; `poll` resends it until the server answers.
    pub fn connect(invite: &Invite, now_ms: u64) -> Result<(Self, Vec<u8>), SessionError> {
        let (initiator, init_packet) = Initiator::new(invite)?;
        let (encoder, decoder, decode_len) = Self::media_state(invite.token.role)?;
        let reject_key = initiator.reject_key().cloned();
        let core = Self {
            invite: invite.clone(),
            state: ClientState::Connecting,
            initiator: Some(initiator),
            init_packet: init_packet.clone(),
            reject_key,
            session_full: false,
            cookied_init: None,
            cookie_answers: TokenBucket::new(COOKIE_ANSWER_BURST, COOKIE_ANSWER_PER_SEC),
            cookie_reply_key: cookie_reply_key(&invite.server_pk),
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
            own_name: None,
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
        self.reject_key = initiator.reject_key().cloned();
        self.session_full = false;
        // A cookie is bound to an address, not to a handshake, but the init it
        // wraps is gone; the next challenge supplies another. The answer
        // budget deliberately survives, so a reconnect loop is not a way
        // round it.
        self.cookied_init = None;
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
                // Rejected keeps retrying, so a server that starts answering
                // (a migration, a redeploy) is let in.
                if !matches!(
                    self.state,
                    ClientState::Connecting | ClientState::Rejected { .. }
                ) {
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
                        // A seat freed and the retry caught it.
                        self.session_full = false;
                        self.last_server_ms = now_ms;
                        self.last_ping_ms = now_ms;
                        self.events.push(ClientEvent::Joined);
                        // Re-announce on every join: on a cache hit the
                        // server asks for nothing and no chunk moves.
                        if let Some((hash, len)) = self.own_avatar {
                            let _ = self.link.send(ControlMsg::SetAvatar { hash, len });
                        }
                        // The name too: admission rebuilt it from the token.
                        if let Some(name) = self.own_name.clone() {
                            let _ = self.link.send(ControlMsg::SetName { name });
                        }
                    }
                    Err(retry) => {
                        // Keep the handshake state and the init bytes. A
                        // forged response is free to send for anyone who can
                        // see this client's address, and starting over on
                        // each one would leave the genuine response, computed
                        // against the init already sent, unable to verify.
                        // Logged at debug because an attacker sets the rate.
                        tracing::debug!("handshake response failed to verify");
                        self.initiator = retry.into_initiator();
                    }
                }
            }
            Packet::VersionReject { ours, theirs, mac } => {
                // Honored only when the MAC binds the exact init packet we
                // sent under the secret this handshake shares with the
                // server. Nothing an invite carries can produce one.
                let ok = self.state == ClientState::Connecting
                    && self.reject_key.as_ref().is_some_and(|key| {
                        wire::verify_version_reject(key, ours, theirs, &mac, &self.init_packet)
                    });
                if ok {
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
                    self.last_init_ms = now_ms;
                    self.init_resend_ms = REJECT_RETRY_MS;
                }
            }
            Packet::CapacityReject { mac } => {
                // Same MAC key as the version reject and the same reason to
                // trust it: only the server, holding the static private key,
                // and this one client, holding the per-connection key behind
                // the init, can derive it. An invite carries neither, so no
                // on-path attacker can invent fullness.
                //
                // Acted on at most once per connection attempt. A capacity
                // reject can be replayed by anyone who saw it, and a second
                // one that reset the retry timer would let a replayer hold
                // this client off the session indefinitely.
                let ok = !self.session_full
                    && self.state == ClientState::Connecting
                    && self.reject_key.as_ref().is_some_and(|key| {
                        wire::verify_capacity_reject(key, &mac, &self.init_packet)
                    });
                if ok {
                    self.session_full = true;
                    self.events.push(ClientEvent::SessionFull);
                    self.last_init_ms = now_ms;
                    self.init_resend_ms = REJECT_RETRY_MS;
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
            Packet::CookieChallenge { nonce, sealed } => {
                // The server is under handshake load and wants the address
                // proved before it spends a Diffie-Hellman. The cookie
                // arrives sealed to the exact init this client sent (the
                // AEAD's additional data), under a key only `server_pk`
                // derives, so an off-path forger cannot mint one this open
                // accepts: a forged challenge dies here instead of replacing
                // a working cookie with one the server would refuse. The
                // open costs one hash and one ChaCha pass, the going rate
                // for parsing.
                if self.state != ClientState::Connecting {
                    return out;
                }
                let Some(cookie) = wire::open_cookie_challenge(
                    &self.cookie_reply_key,
                    &nonce,
                    &sealed,
                    &self.init_packet,
                ) else {
                    return out;
                };
                let Ok(Packet::HandshakeInit { version, noise }) = wire::parse(&self.init_packet)
                else {
                    return out;
                };
                let cookied = wire::build_cookied_init(&cookie, version, noise);
                // A repeated challenge carrying the cookie already held costs
                // nothing at all, which is most replays. The budget bounds
                // what remains: an invite holder on the path holds both the
                // key and the init and can still forge, and even a valid
                // challenge must neither pump this client for packets nor,
                // when it cannot be answered, evict the cookie the next
                // resend offers. The cookied init stays an addition to the
                // plain one, never a replacement.
                if self.cookied_init.as_deref() == Some(cookied.as_slice())
                    || !self.cookie_answers.take(now_ms)
                {
                    return out;
                }
                self.cookied_init = Some(cookied.clone());
                out.push(self.init_packet.clone());
                out.push(cookied);
            }
            // Clients never receive either of these.
            Packet::HandshakeInit { .. } | Packet::CookiedInit { .. } => {}
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
                // A full session is an authenticated statement about one
                // moment, so it draws the reject backoff and no timeout at
                // all: the server is answering, and a seat frees when
                // somebody leaves. Reporting a timeout instead would be the
                // one thing this client knows to be false.
                let (timeout, backoff_cap) = if self.session_full {
                    (false, REJECT_RETRY_MAX_MS)
                } else {
                    (true, INIT_RESEND_MAX_MS)
                };
                if timeout && now_ms.saturating_sub(self.last_server_ms) >= CONNECTION_TIMEOUT_MS {
                    self.state = ClientState::TimedOut;
                    self.events.push(ClientEvent::TimedOut);
                } else if now_ms.saturating_sub(self.last_init_ms) >= self.init_resend_ms {
                    // Same bytes every time: the server answers a resent
                    // identical init with its cached response.
                    self.last_init_ms = now_ms;
                    self.init_resend_ms = (self.init_resend_ms * 2).min(backoff_cap);
                    out.push(self.init_packet.clone());
                    // Both forms while a cookie is held. A server not under
                    // load takes the plain one; a server that is takes the
                    // cookied one; a forged cookie costs one wasted datagram
                    // rather than the join.
                    if let Some(cookied) = self.cookied_init.as_ref() {
                        out.push(cookied.clone());
                    }
                }
            }
            ClientState::Joined => {
                // Silence for the timeout, or a control link that has given
                // up retransmitting. The second never follows from the first:
                // any authenticated packet, media included, counts as being
                // heard from, so a server that keeps mixing while acking
                // nothing would otherwise hold this client forever.
                if now_ms.saturating_sub(self.last_server_ms) >= CONNECTION_TIMEOUT_MS
                    || self.link.is_dead()
                {
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
            // A rejected client has something to show its user, and it keeps
            // handing the same init back at a minute's spacing. That costs
            // the server one packet a minute and means a session that
            // migrates or is redeployed onto a build this client can talk to
            // is joined without anyone restarting anything. No timeout here:
            // the state is already the answer.
            ClientState::Rejected { .. }
                if now_ms.saturating_sub(self.last_init_ms) >= self.init_resend_ms =>
            {
                self.last_init_ms = now_ms;
                self.init_resend_ms = (self.init_resend_ms * 2).min(REJECT_RETRY_MAX_MS);
                out.push(self.init_packet.clone());
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

    /// Whether the server has told this client, in a packet only it could
    /// have produced, that the role its invite names is full. True only while
    /// `Connecting`: the client keeps offering the same init, so a seat that
    /// frees is taken without anybody restarting anything.
    pub fn session_full(&self) -> bool {
        self.session_full
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
            session_full: self.session_full,
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

    /// Sets this member's display name on the roster, for everyone. Kept and
    /// re-announced on every join like the avatar, because admission rebuilds
    /// the name from the token's hint. Callable before joining; the announce
    /// then rides the join. Empty after trimming or past [`MAX_NAME_LEN`]
    /// bytes is refused here, with the same cap the roster itself enforces.
    pub fn set_name(&mut self, name: &str) -> Result<(), SessionError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(SessionError::InvalidParam("name is empty"));
        }
        if name.len() > MAX_NAME_LEN {
            return Err(SessionError::InvalidParam("name is too long"));
        }
        self.own_name = Some(name.to_owned());
        if self.state == ClientState::Joined {
            self.link.send(ControlMsg::SetName {
                name: name.to_owned(),
            })?;
        }
        Ok(())
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

    /// Host-only server-side: start or stop the session recording. The
    /// server counts a violation against anyone else.
    pub fn record_ctl(&mut self, op: RecordOp) -> Result<(), SessionError> {
        self.require_joined()?;
        self.link.send(ControlMsg::RecordCtl { op })?;
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

    /// Asks the server to include this member's own signal in the personal
    /// mix it sends back, instead of removing it.
    pub fn set_hear_self(&mut self, enabled: bool) -> Result<(), SessionError> {
        self.require_joined()?;
        self.link.send(ControlMsg::HearSelf { enabled })?;
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
            ControlMsg::RecordStatus { state, stems } => {
                self.events.push(ClientEvent::RecordStatus { state, stems });
            }
            ControlMsg::BroadcastReadiness { state } => {
                self.events.push(ClientEvent::BroadcastReadiness(state));
            }
            // The session server's own log, which only the host is sent.
            // Reported rather than surfaced: it is what a host reads after a
            // failure, not something to draw in a room full of musicians.
            ControlMsg::ServerLog { line } => {
                tracing::info!(
                    target: crate::logtail::SERVER_LOG_TARGET,
                    session = %self.invite.session_id.hex(),
                    "{line}"
                );
            }
            // The server never sends these; ignore.
            ControlMsg::MixerSet { .. }
            | ControlMsg::ClickEnable { .. }
            | ControlMsg::BroadcastAudition { .. }
            | ControlMsg::HearSelf { .. }
            | ControlMsg::Revoke { .. }
            | ControlMsg::SetAvatar { .. }
            | ControlMsg::StreamCtl { .. }
            | ControlMsg::RecordCtl { .. }
            | ControlMsg::SetName { .. } => {}
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
    use jamstream_protocol::PROTOCOL_VERSION;
    use jamstream_protocol::ids::SessionId;
    use jamstream_protocol::invite::{Issuer, Token};
    use jamstream_protocol::transport::{
        Keypair, Responder, cookie_key, generate_keypair, reject_key_for_init,
    };
    use jamstream_protocol::wire::TYPE_HANDSHAKE_INIT;

    /// The address the test server believes the client connects from; the
    /// client never checks it, it just echoes the cookie it decrypts.
    const CHALLENGE_SRC: &str = "203.0.113.7";

    fn invite(role: Role) -> Invite {
        invite_and_server(role).0
    }

    fn invite_and_server(role: Role) -> (Invite, Keypair) {
        let issuer = Issuer::generate();
        let kp = generate_keypair();
        let invite = issuer.mint(
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
        );
        (invite, kp)
    }

    /// The real server's handshake response to `init`, so a test can prove a
    /// client actually gets in rather than asserting about state alone.
    fn handshake_response(server: &Keypair, inv: &Invite, init: &[u8]) -> Vec<u8> {
        let Ok(Packet::HandshakeInit { version, noise }) = wire::parse(init) else {
            panic!("expected an init");
        };
        let (hp, responder) =
            Responder::read_init(&server.private, &inv.session_id, version, noise)
                .expect("the server reads the init it was sent");
        let (_, packet) = responder
            .respond(&Welcome {
                member_id: hp.token.member_id,
                sample_clock: 0,
            })
            .expect("respond");
        packet
    }

    /// The challenge a real server under load sends in answer to `init`,
    /// sealed the way the server seals it: under the reply key from its
    /// public half, bound to the exact init bytes, carrying the cookie of
    /// `epoch`. Distinct epochs give distinct cookies, which is what a test
    /// that needs many valid challenges for one init varies.
    fn genuine_challenge(server: &Keypair, init: &[u8], epoch: u64) -> Vec<u8> {
        wire::build_cookie_challenge(
            &jamstream_protocol::transport::cookie_reply_key(&server.public),
            &cookie_key(&server.private, epoch),
            CHALLENGE_SRC.parse().unwrap(),
            init,
        )
    }

    /// The cookie inside [`genuine_challenge`] for the same epoch: what the
    /// client is expected to echo in its cookied init.
    fn challenge_cookie(server: &Keypair, epoch: u64) -> [u8; wire::COOKIE_BYTES] {
        wire::cookie_for(
            &cookie_key(&server.private, epoch),
            CHALLENGE_SRC.parse().unwrap(),
        )
    }

    /// The reject a real server would send in answer to `init`, keyed the way
    /// the server keys it: on the secret it shares with the client that sent
    /// this exact init.
    fn genuine_reject(server: &Keypair, inv: &Invite, init: &[u8], theirs: u16) -> Vec<u8> {
        let Ok(Packet::HandshakeInit { version, noise }) = wire::parse(init) else {
            panic!("expected an init");
        };
        let key = reject_key_for_init(&server.private, &inv.session_id, version, noise)
            .expect("server derives the reject key");
        wire::build_version_reject(&key, theirs, version, init)
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

    /// An invite holder has everything a MAC keyed on the server public key
    /// needs, so a reject cannot be trusted on that alone: one 21-byte packet
    /// from a revoked member would wedge a victim for good.
    #[test]
    fn a_reject_forged_by_an_invite_holder_is_ignored() {
        let (inv, server) = invite_and_server(Role::Musician);
        let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();

        // Everything an invite holder has: the server's public key, the
        // session id, and the victim's init off the wire. None of it keys
        // the MAC, and a second handshake of their own derives another key.
        let (other, _) = Initiator::new(&inv).unwrap();
        let forged = wire::build_version_reject(other.reject_key().unwrap(), 2, 1, &init);
        assert!(core.handle_datagram(1, &forged).is_empty());
        assert_eq!(*core.state(), ClientState::Connecting);
        assert!(core.events().is_empty());

        // The server's own, over the same init, is honored and mapped to the
        // client's perspective.
        core.handle_datagram(2, &genuine_reject(&server, &inv, &init, 9));
        assert_eq!(
            *core.state(),
            ClientState::Rejected {
                ours: PROTOCOL_VERSION,
                theirs: 9
            }
        );
        assert_eq!(
            core.events(),
            vec![ClientEvent::Rejected {
                ours: PROTOCOL_VERSION,
                theirs: 9
            }]
        );
    }

    /// Anyone who can see a connecting client's address can spray handshake
    /// responses, so none of them may consume the handshake state and mint a
    /// fresh init: that leaves the server's answer to the init already sent
    /// unreadable, and the spray alone keeps the client out of the session.
    #[test]
    fn forged_handshake_responses_do_not_disturb_the_handshake() {
        let (mut core, first) = ClientCore::connect(&invite(Role::Musician), 0).unwrap();
        for i in 0..64u8 {
            let forged = wire::build_handshake_resp(&[i; 96]);
            assert!(core.handle_datagram(1, &forged).is_empty());
            assert_eq!(*core.state(), ClientState::Connecting);
        }
        // Same init bytes on the wire, so the server's cached response and
        // its transport state still pair with what this client sent.
        assert_eq!(core.poll(500), vec![first]);
    }

    /// Rejected is a report, not an ending: the client keeps offering the
    /// same init at a widening interval, so a session that migrates or is
    /// redeployed onto a build it can talk to is joined without a restart.
    #[test]
    fn a_rejected_client_keeps_trying_and_can_still_join() {
        let (inv, server) = invite_and_server(Role::Musician);
        let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
        core.handle_datagram(1, &genuine_reject(&server, &inv, &init, 9));
        assert!(matches!(core.state(), ClientState::Rejected { .. }));

        // Backoff from 5 s, doubling, and the same bytes every time.
        assert!(core.poll(4_000).is_empty());
        assert_eq!(core.poll(5_001), vec![init.clone()]);
        assert!(core.poll(12_000).is_empty());
        assert_eq!(core.poll(15_002), vec![init.clone()]);
        // No timeout while rejected: the state is already the answer, and a
        // client that gave up for good would need the user to restart it.
        core.poll(600_000);
        assert!(matches!(core.state(), ClientState::Rejected { .. }));
    }

    /// The reject a real server sends when the role is full, keyed the way
    /// the server keys it: on the secret it shares with the client that sent
    /// this exact init.
    fn genuine_capacity_reject(server: &Keypair, inv: &Invite, init: &[u8]) -> Vec<u8> {
        let Ok(Packet::HandshakeInit { version, noise }) = wire::parse(init) else {
            panic!("expected an init");
        };
        let key = reject_key_for_init(&server.private, &inv.session_id, version, noise)
            .expect("server derives the reject key");
        wire::build_capacity_reject(&key, init)
    }

    /// A sold-out session says so, keeps the same init on offer, and joins when
    /// a seat frees, with no timeout in between. Dropping the init instead
    /// leaves it indistinguishable from a server that is down.
    #[test]
    fn a_full_session_is_reported_and_kept_trying_for() {
        let (inv, server) = invite_and_server(Role::Listener);
        let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();

        assert!(
            core.handle_datagram(1, &genuine_capacity_reject(&server, &inv, &init))
                .is_empty()
        );
        assert!(core.session_full());
        assert!(core.stats().session_full);
        assert_eq!(core.events(), vec![ClientEvent::SessionFull]);
        // Still Connecting: this is a report about a moment, not an ending.
        assert_eq!(*core.state(), ClientState::Connecting);

        // The reject backoff, and the same bytes every time, so the server's
        // cached response still pairs with what this client sent.
        assert!(core.poll(4_000).is_empty());
        assert_eq!(core.poll(5_002), vec![init.clone()]);
        assert!(core.poll(12_000).is_empty());
        assert_eq!(core.poll(15_003), vec![init.clone()]);
        // And no timeout: the server is answering, so a timeout would report
        // the one thing this client knows to be false.
        core.poll(600_000);
        assert_eq!(*core.state(), ClientState::Connecting);
        assert!(core.session_full());
        assert!(core.events().is_empty());

        // A seat frees and the retry catches it.
        let resp = handshake_response(&server, &inv, &init);
        core.handle_datagram(600_001, &resp);
        assert_eq!(*core.state(), ClientState::Joined);
        assert!(!core.session_full());
    }

    /// Nothing an invite carries produces the MAC, and a reject that is not
    /// about the init this client sent is somebody else's.
    #[test]
    fn a_forged_or_stale_capacity_reject_is_ignored() {
        let (inv, server) = invite_and_server(Role::Listener);
        let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();

        // Everything an invite holder has: the server public key, the session
        // id, and the victim's init off the wire. Their own handshake derives
        // a different key.
        let (other, other_init) = Initiator::new(&inv).unwrap();
        let forged = wire::build_capacity_reject(other.reject_key().unwrap(), &init);
        assert!(core.handle_datagram(1, &forged).is_empty());
        assert!(!core.session_full());
        assert!(core.events().is_empty());

        // The server's own, but about a different connection attempt.
        let stale = genuine_capacity_reject(&server, &inv, &other_init);
        assert!(core.handle_datagram(2, &stale).is_empty());
        assert!(!core.session_full());

        // A version reject's MAC is not a capacity reject's, even though the
        // two share a key.
        let Ok(Packet::VersionReject { mac, .. }) =
            wire::parse(&genuine_reject(&server, &inv, &init, 9))
        else {
            panic!("expected a version reject");
        };
        let mut swapped = vec![wire::TYPE_CAPACITY_REJECT];
        swapped.extend_from_slice(&mac);
        assert!(core.handle_datagram(3, &swapped).is_empty());
        assert!(!core.session_full());
        assert_eq!(*core.state(), ClientState::Connecting);

        // The genuine article still lands.
        core.handle_datagram(4, &genuine_capacity_reject(&server, &inv, &init));
        assert!(core.session_full());
    }

    /// A capacity reject can be replayed by anybody who saw one, so it is
    /// acted on once per connection attempt. A second that reset the retry
    /// timer would let a replayer hold this client off the session for as
    /// long as it cared to keep sending.
    #[test]
    fn a_replayed_capacity_reject_cannot_hold_a_client_off_the_session() {
        let (inv, server) = invite_and_server(Role::Listener);
        let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
        let reject = genuine_capacity_reject(&server, &inv, &init);
        core.handle_datagram(0, &reject);
        assert_eq!(core.events(), vec![ClientEvent::SessionFull]);

        // Sprayed for the whole backoff window: one event, and the resend due
        // at 5 s still happens on time.
        for ms in 1..5_000u64 {
            assert!(core.handle_datagram(ms, &reject).is_empty());
        }
        assert!(core.events().is_empty());
        assert_eq!(core.poll(5_001), vec![init]);
    }

    /// A challenge does not prove the server sent it (any invite holder on
    /// the path holds the reply key and this init), so a client that swapped
    /// its plain init for a cookied one would lose its join to a forged
    /// challenge. Both forms go out, always, and the server takes whichever it
    /// can use.
    #[test]
    fn a_cookie_is_offered_alongside_the_plain_init_never_instead_of_it() {
        let (inv, server) = invite_and_server(Role::Musician);
        let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
        let answer = core.handle_datagram(10, &genuine_challenge(&server, &init, 7));

        // Answered at once, because waiting for the resend timer would cost up
        // to two seconds, and the plain init goes with it. The cookie echoed
        // is the one the server sealed, decrypted for real.
        let Ok(Packet::HandshakeInit { version, noise }) = wire::parse(&init) else {
            panic!("expected an init");
        };
        let cookied = wire::build_cookied_init(&challenge_cookie(&server, 7), version, noise);
        assert_eq!(answer, vec![init.clone(), cookied.clone()]);
        // The Noise message is byte-identical in both forms, so the server's
        // cached response still pairs with whichever one it read.
        let Ok(Packet::CookiedInit { noise: cn, .. }) = wire::parse(&cookied) else {
            panic!("expected a cookied init");
        };
        assert_eq!(cn, noise);

        // And every resend from here carries both.
        assert_eq!(core.poll(500), vec![init.clone(), cookied.clone()]);
        assert_eq!(core.poll(1_500), vec![init, cookied]);
        assert_eq!(*core.state(), ClientState::Connecting);
    }

    /// Answering a challenge must not be a lever for making the client emit
    /// packets at whatever rate a sender picks. A forger without the server
    /// key now gets nothing at all; a sender who can seal valid challenges
    /// (an invite holder watching the path) still gets no more than the
    /// budget.
    #[test]
    fn forged_challenges_cannot_pump_a_client_for_packets() {
        let (inv, server) = invite_and_server(Role::Musician);
        let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();

        // Forged under the wrong key: sealed for real, just not by anyone
        // holding this server's public key. Every one is dropped unopened.
        let stranger = generate_keypair();
        for i in 0..2_000u64 {
            assert!(
                core.handle_datagram(1, &genuine_challenge(&stranger, &init, i))
                    .is_empty()
            );
        }

        // Validly sealed, a different cookie every time, which is the
        // expensive case: an unchanged one costs nothing at all. Two
        // datagrams per answer, and the burst is all a sender gets in an
        // instant.
        let mut sent = 0;
        for i in 0..2_000u64 {
            sent += core
                .handle_datagram(1, &genuine_challenge(&server, &init, i))
                .len();
        }
        assert_eq!(sent, 2 * COOKIE_ANSWER_BURST as usize);
        // And no faster than the refill from there.
        let mut later = 0;
        for i in 2_000..4_000u64 {
            later += core
                .handle_datagram(1_500, &genuine_challenge(&server, &init, i))
                .len();
        }
        assert_eq!(later, 2 * COOKIE_ANSWER_PER_SEC as usize);
        // Nothing about the handshake moved: the state is still usable and the
        // plain init on the wire is still the one the server will answer.
        assert_eq!(*core.state(), ClientState::Connecting);
    }

    /// Every cookie this client holds is one it answered with. The stored
    /// cookie is what the next resend offers, so a challenge that arrives with
    /// the answer budget already spent must not replace it: doing so would let
    /// an injected challenge point the client at a cookie the server will
    /// refuse, where a plain init would only draw another challenge. The AEAD
    /// binding stops a forger who lacks the key or the init; this guard prices
    /// out the sender who has both, so an eviction costs one of the same
    /// tokens that bound the answers.
    #[test]
    fn a_challenge_we_cannot_answer_does_not_replace_the_cookie_we_hold() {
        let (inv, server) = invite_and_server(Role::Musician);
        let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
        let Ok(Packet::HandshakeInit { version, noise }) = wire::parse(&init) else {
            panic!("expected an init");
        };

        // A thousand distinct validly sealed challenges in one instant. Only
        // the burst is answered, so only the burst is adopted.
        let mut answered = Vec::new();
        for i in 0..1_000u64 {
            if !core
                .handle_datagram(1, &genuine_challenge(&server, &init, i))
                .is_empty()
            {
                answered.push(i);
            }
        }
        assert_eq!(
            answered,
            (0..u64::from(COOKIE_ANSWER_BURST)).collect::<Vec<u64>>()
        );

        // The resend carries the last cookie this client put on the wire, not
        // the last one somebody sent it.
        let last_answered = *answered.last().unwrap();
        assert_eq!(
            core.poll(3_000),
            vec![
                init.clone(),
                wire::build_cookied_init(&challenge_cookie(&server, last_answered), version, noise)
            ],
            "an unanswerable challenge replaced the cookie on the wire"
        );

        // Once the bucket refills the next challenge is adopted, so a client
        // whose cookie really did go stale is not stuck with it.
        let fresh = challenge_cookie(&server, 9_999);
        assert_eq!(
            core.handle_datagram(4_000, &genuine_challenge(&server, &init, 9_999)),
            vec![
                init.clone(),
                wire::build_cookied_init(&fresh, version, noise)
            ]
        );
        assert_eq!(
            core.poll(9_000),
            vec![
                init.clone(),
                wire::build_cookied_init(&fresh, version, noise)
            ]
        );
    }

    /// A challenge is bound, through the AEAD's additional data, to the one
    /// init it answers. Sealed over anybody else's init, under the right key
    /// and by the right server, it is still not an answer to ours: it draws
    /// no packet, spends no budget, and cannot displace the cookie this
    /// client already holds. That is what keeps an off-path forgery from
    /// costing the client its cookie for free.
    #[test]
    fn a_challenge_for_a_different_init_is_rejected() {
        let (inv, server) = invite_and_server(Role::Musician);
        let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
        let Ok(Packet::HandshakeInit { version, noise }) = wire::parse(&init) else {
            panic!("expected an init");
        };

        // A genuine challenge first, so there is a held cookie to defend.
        assert_eq!(
            core.handle_datagram(1, &genuine_challenge(&server, &init, 0))
                .len(),
            2
        );

        // The same server's challenges for a different connection attempt,
        // which is what a replayed or misdirected challenge is. Distinct
        // cookies every time; not one opens, so not one is answered.
        let (_, other_init) = Initiator::new(&inv).unwrap();
        for i in 0..1_000u64 {
            assert!(
                core.handle_datagram(2, &genuine_challenge(&server, &other_init, i))
                    .is_empty()
            );
        }

        // The resend still offers the cookie sealed for this init, and the
        // budget those rejects never touched still answers a genuine rotation
        // at once.
        assert_eq!(
            core.poll(3_000),
            vec![
                init.clone(),
                wire::build_cookied_init(&challenge_cookie(&server, 0), version, noise)
            ],
            "a challenge for somebody else's init displaced the held cookie"
        );
        assert_eq!(
            core.handle_datagram(3_001, &genuine_challenge(&server, &init, 1))
                .len(),
            2
        );
    }

    /// A challenge is only meaningful while a handshake is in flight. Joined,
    /// it is either stale or somebody probing, and either way it must not put
    /// a second init on the wire.
    #[test]
    fn a_challenge_after_joining_is_ignored() {
        let (inv, server) = invite_and_server(Role::Musician);
        let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
        core.handle_datagram(1, &handshake_response(&server, &inv, &init));
        assert_eq!(*core.state(), ClientState::Joined);
        assert!(
            core.handle_datagram(2, &genuine_challenge(&server, &init, 7))
                .is_empty()
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

    /// Both ends of the deadline. The one-millisecond-early poll has to leave
    /// the client connecting: asserting only that it produced at most one
    /// datagram was satisfied by producing none, which is what a client that
    /// had already given up does, so the whole ten seconds could shrink to
    /// half a second with this green.
    #[test]
    fn connecting_times_out_after_ten_seconds() {
        let (mut core, _) = ClientCore::connect(&invite(Role::Musician), 0).unwrap();
        assert!(core.poll(9_999).len() <= 1);
        assert_eq!(*core.state(), ClientState::Connecting);
        assert!(!core.events().contains(&ClientEvent::TimedOut));
        core.poll(10_000);
        assert_eq!(*core.state(), ClientState::TimedOut);
        assert!(core.events().contains(&ClientEvent::TimedOut));
    }

    /// One address behaves exactly as it always did: the driver has nothing
    /// to fail over to and should not pretend otherwise.
    #[test]
    fn a_single_address_offers_no_alternatives() {
        let inv = invite(Role::Musician);
        let mut candidates = ServerCandidates::new(&inv).unwrap();
        let only: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        assert_eq!(candidates.current(), only);
        assert!(!candidates.has_alternatives());
        // Advancing anyway is not an error, it just stays put.
        assert_eq!(candidates.advance(), only);
    }

    /// A locally hosted session offers loopback and the LAN address, and a
    /// driver walks them in order and comes back round: a long overall
    /// deadline should not be spent on one address that was slow to boot.
    #[test]
    fn candidates_are_walked_in_order_and_wrap() {
        let mut inv = invite(Role::Musician);
        let loopback: SocketAddr = "127.0.0.1:43210".parse().unwrap();
        let lan: SocketAddr = "192.168.1.12:43210".parse().unwrap();
        inv.addresses = vec![loopback, lan];

        let mut candidates = ServerCandidates::new(&inv).unwrap();
        assert!(candidates.has_alternatives());
        assert_eq!(candidates.current(), loopback);
        assert_eq!(candidates.advance(), lan);
        assert_eq!(candidates.current(), lan);
        assert_eq!(candidates.advance(), loopback);
        assert_eq!(candidates.advance(), lan);
    }

    /// Invite::decode refuses an empty list, so this only happens to an
    /// invite built in memory, and it must not panic on an index.
    #[test]
    fn an_invite_with_no_addresses_is_an_error_not_a_panic() {
        let mut inv = invite(Role::Musician);
        inv.addresses.clear();
        assert!(ServerCandidates::new(&inv).is_err());
    }

    /// The addresses are not part of what the issuer signs, and the
    /// handshake authenticates the server by its static key, so adding one
    /// changes nothing about who can answer.
    #[test]
    fn a_second_address_does_not_change_what_the_invite_proves() {
        let (mut inv, _) = invite_and_server(Role::Musician);
        let signed = inv.signature;
        inv.addresses
            .insert(0, "127.0.0.1:5000".parse::<SocketAddr>().unwrap());
        assert_eq!(inv.signature, signed);
        let round_trip = Invite::decode(&inv.encode()).unwrap();
        assert_eq!(round_trip.addresses, inv.addresses);
        assert_eq!(round_trip.server_pk, inv.server_pk);
    }

    #[test]
    fn push_capture_is_empty_until_joined() {
        let (mut core, _) = ClientCore::connect(&invite(Role::Musician), 0).unwrap();
        assert!(core.push_capture(0, &[0.0; 120]).is_empty());
        let (mut listener, _) = ClientCore::connect(&invite(Role::Listener), 0).unwrap();
        assert!(listener.push_capture(0, &[0.0; 120]).is_empty());
    }

    #[test]
    fn raw_capture_is_empty_until_joined() {
        let (mut core, _) = ClientCore::connect(&invite(Role::Musician), 0).unwrap();
        // Device-paced deliveries that straddle frame boundaries.
        assert!(core.push_capture_raw(0, &[0.0; 250]).is_empty());
        assert!(core.push_capture_raw(0, &[0.0; 119]).is_empty());
        assert!(core.push_capture_raw(0, &[0.0; 1]).is_empty());
    }

    /// A device hands over whatever its callback size is, which is never the
    /// 2.5 ms frame, and every sample it hands over owes one datagram per
    /// frame's worth. Joined, because unjoined the count is zero for any input
    /// and says nothing about the pacing.
    #[test]
    fn raw_capture_paces_odd_lengths_onto_the_frame_clock() {
        let (inv, server) = invite_and_server(Role::Musician);
        let (mut core, init) = ClientCore::connect(&inv, 0).unwrap();
        core.handle_datagram(1, &handshake_response(&server, &inv, &init));
        assert_eq!(*core.state(), ClientState::Joined);

        let mut pushed = 0usize;
        let mut sent = 0usize;
        for len in [250usize, 119, 1].iter().cycle().take(30) {
            pushed += len;
            let dgs = core.push_capture_raw(1, &vec![0.25; *len]);
            assert!(dgs.iter().all(|d| !d.is_empty()), "an empty datagram");
            sent += dgs.len();
        }
        // Exactly the completed frames: the remainder of the last push stays
        // staged for the next one, and nothing in between is dropped.
        assert_eq!(
            sent,
            pushed / TICK_SAMPLES as usize,
            "{pushed} samples in odd-length pushes left as {sent} datagrams"
        );
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
