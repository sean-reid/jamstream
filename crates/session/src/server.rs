//! Server-side session core: admission, per-member encrypted transport, the
//! 2.5 ms mix tick, and control-plane fanout. Sans-io: jamstreamd owns the
//! socket and the clock and calls in with datagrams and timestamps.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};

use blake2::{Blake2s256, Digest};
use ed25519_dalek::VerifyingKey;
use jamstream_engine::{
    Channels, Decoder, Encoder, Fader, JitterBuffer, JitterStats, Limiter, MediaPacket, Metronome,
    Pull, mix_into,
};
use jamstream_protocol::PROTOCOL_VERSION;
use jamstream_protocol::control::{
    BroadcastReadiness, ControlLink, ControlMsg, DestinationStatus, MAX_AVATAR_BYTES, MAX_NAME_LEN,
    MAX_STREAM_KEY_LEN, MemberInfo, RecordOp, RecordingState, StreamOp,
};
use jamstream_protocol::ids::{HOST_MEMBER_ID, MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::verify_token;
use jamstream_protocol::media::{FrameDuration, MediaFrame};
use jamstream_protocol::transport::{self, Responder, Session, Welcome};
use jamstream_protocol::wire::{self, CHANNEL_CONTROL, CHANNEL_MEDIA, COOKIE_BYTES, Packet};

use crate::avatar::{AVATAR_CHUNKS_PER_POLL, AvatarCache, AvatarHash, AvatarRx, AvatarTx, RxStep};
use crate::limits::{
    DEFAULT_MEMBER_TIMEOUT_MS, FANOUT_BURST, FANOUT_REFILL_PER_SEC, MAX_LISTENERS, MAX_MUSICIANS,
    MEMBER_QUIET_AFTER_MS, SERVER_LOG_BURST, SERVER_LOG_HIGH_WATER, SERVER_LOG_PER_SEC,
    TokenBucket, VIOLATION_BURST, VIOLATION_REFILL_PER_SEC,
};
use crate::logtail::LogTail;

/// Samples per mix tick: 2.5 ms at 48 kHz.
pub const TICK_SAMPLES: usize = 120;
/// Interleaved stereo floats per tick.
const MIX_LEN: usize = TICK_SAMPLES * 2;
/// Broadcast frames span this many ticks (20 ms).
const BCAST_TICKS: u64 = 8;
const BCAST_LEN: usize = MIX_LEN * BCAST_TICKS as usize;
const PERSONAL_MIX_BITRATE: u32 = 192_000;
const BROADCAST_BITRATE: u32 = 128_000;
const CLICK_GAIN: f32 = 0.7;
/// At most one version reject per source slot per this interval.
const REJECT_INTERVAL_MS: u64 = 1_000;
/// Slots the reject limiter keeps, indexed by a keyed hash of the source
/// network. Sources that collide share one slot's allowance, which costs an
/// honest mismatched client at most a second of extra silence and buys an O(1)
/// per-packet check with no allocation and nothing to evict.
const REJECT_SLOTS: usize = 256;
const _: () = assert!(REJECT_SLOTS.is_power_of_two());
/// Domain separator for the limiter's slot key, so nothing derived from the
/// static private key can be mistaken for anything else derived from it.
const SLOT_KEY_DOMAIN: &[u8] = b"jamstream-limiter-slot-v1";
/// Version rejects emitted per second across every source, and the burst
/// allowed. A client on the wrong version needs exactly one to show its user
/// what to update, so this is generous for the honest case while capping
/// reflected volume at roughly 16 * 49 = 784 bytes per second on the wire.
const REJECT_RATE_PER_SEC: u32 = 16;
const REJECT_BURST: u32 = 16;
/// Handshake inits the server will pay a Diffie-Hellman for per second, and
/// the burst it allows. `Responder::read_init` performs an X25519 before
/// anything about the sender is known, on the same task that runs the 2.5 ms
/// mix tick, so an unbudgeted flood is a tick overrun for everyone already
/// playing. A full session is 30 members, each sending one init and resending
/// at most twice a second while connecting, so a whole band arriving at once
/// and retrying sits inside the burst; past that, joining degrades and the
/// session keeps playing, which is the trade the budget exists to make.
const INIT_RATE_PER_SEC: u32 = 32;
const INIT_BURST: u32 = 64;
/// Per source network share of that budget, so one host flooding from a real
/// address cannot spend the whole allowance. Sized for a rehearsal room: a
/// band behind one NAT is one network here, and ten musicians arriving
/// together with a resend each is 20 inits.
const INIT_SLOT_RATE_PER_SEC: u32 = 8;
const INIT_SLOT_BURST: u32 = 24;
/// How long one cookie secret is good for, and therefore the longest a stolen
/// cookie is worth carrying. Two minutes, WireGuard's interval: a joining
/// client's whole run of resends fits inside one, so the round trip is paid
/// once.
const COOKIE_ROTATION_MS: u64 = 120_000;
/// Inbound inits per second above which the cookie round trip engages, and
/// the burst allowed before it does.
///
/// Below the Diffie-Hellman budget on purpose, so cookies come on before that
/// budget starts dropping honest inits rather than after. A full session is 30
/// members and all 30 arriving at once fits the burst, so the case anyone
/// actually has stays a single round trip; sustained traffic above the rate is
/// not a band arriving.
const COOKIE_TRIGGER_RATE_PER_SEC: u32 = 24;
const COOKIE_TRIGGER_BURST: u32 = 48;
/// Ceiling on cookie challenges per second.
///
/// A ceiling, not a fair share. A limiter keyed on the source cannot tell an
/// honest init from a spoofed one, so a cap low enough to matter to a flood
/// would starve exactly the client the cookie exists to let through. What it
/// bounds is an unbounded send loop on the task that owns the mix tick: at
/// 57 bytes a challenge this is under 1 MB/s, and per packet a challenge is
/// always smaller than the init that drew it (see
/// [`CHALLENGE_MIN_INIT_BYTES`]), so the reflected flood is always less than
/// the inbound one. Past the ceiling an init draws silence, which is what it
/// drew before any of this existed.
const CHALLENGE_RATE_PER_SEC: u32 = 16_384;
/// Shortest handshake init that earns a reject. A reject is 21 bytes and a
/// real Noise IK first message is over 90, so answering anything shorter
/// would make the server an amplifier by size: `[1, 9, 0]` in, 21 bytes out.
const REJECT_MIN_INIT_BYTES: usize = 48;
/// Shortest handshake init that earns a cookie challenge. Higher than the
/// reject floor because the encrypted challenge is 57 bytes against the
/// reject's 21; above this floor the challenge is still always the smaller
/// packet. A real Noise IK first flight is over 180 bytes, so no honest init
/// is anywhere near either floor.
const CHALLENGE_MIN_INIT_BYTES: usize = 64;
/// Queue depth at which the avatar pacer stops feeding a link. Well clear of
/// [`MAX_PENDING`], so the link's hard cap only ever refuses bulk, never a
/// roster or a chat. A round trip's worth of chunks on a 45 ms path is about
/// 36, so this also lets a transfer run at full speed on any real link.
const AVATAR_FEED_HIGH_WATER: usize = 64;
/// Uplink Stats reports go to each musician this often.
const STATS_INTERVAL_MS: u64 = 1_000;
/// While anything is configured for broadcast, every member is told the
/// on-air state at least this often. Transitions are sent immediately.
const STREAM_STATUS_INTERVAL_MS: u64 = 1_000;
/// Meter fall per 2.5 ms tick for the broadcast cards, matching the client's
/// own ballistics: roughly a 170 ms half-life.
const BCAST_LEVEL_DECAY: f32 = 0.99;
/// How long a cached handshake response answers an identical resent init.
const RESP_CACHE_MS: u64 = 5_000;
/// A connected member silent this long may be replaced by a fresh init
/// (fast rejoin) without waiting for the full member timeout.
const REJOIN_SILENCE_MS: u64 = 2_000;
/// Total avatar bytes the server keeps; roster-referenced hashes are
/// pinned, the rest evict least-recently-referenced first.
const AVATAR_CACHE_BYTES: usize = 16 * 1024 * 1024;
const LIMITER_CEILING_DB: f32 = -1.0;
/// 1 ms of lookahead; broadcast listeners never notice.
const LIMITER_LOOKAHEAD_SAMPLES: usize = 48;

type Outgoing = Vec<(SocketAddr, Vec<u8>)>;

pub struct ServerConfig {
    pub session_id: SessionId,
    /// X25519 static private key, from provider user-data.
    pub server_private: Vec<u8>,
    /// Public half of `server_private`. Token signatures bind it, so the core
    /// needs it explicitly.
    pub server_public: [u8; 32],
    pub issuer_pk: VerifyingKey,
    /// Musicians admitted at once, the host's seat included. Defaults to
    /// [`MAX_MUSICIANS`], the capacity every host surface offers.
    pub max_musicians: usize,
    /// Listeners admitted at once. Defaults to [`MAX_LISTENERS`].
    pub max_listeners: usize,
    pub member_timeout_ms: u64,
}

impl ServerConfig {
    /// Session identity plus the shipped session shape: [`MAX_MUSICIANS`]
    /// musicians including the host, [`MAX_LISTENERS`] listeners, and the
    /// default member timeout. Every production caller wants exactly this,
    /// so the capacity the server enforces cannot drift from the capacity
    /// the CLI and the desktop app offer.
    pub fn new(
        session_id: SessionId,
        server_private: Vec<u8>,
        server_public: [u8; 32],
        issuer_pk: VerifyingKey,
    ) -> ServerConfig {
        ServerConfig {
            session_id,
            server_private,
            server_public,
            issuer_pk,
            max_musicians: MAX_MUSICIANS,
            max_listeners: MAX_LISTENERS,
            member_timeout_ms: DEFAULT_MEMBER_TIMEOUT_MS,
        }
    }

    /// Narrows (or widens) the admission caps. The simulation harness sizes
    /// them to its scenario; production uses the defaults.
    pub fn with_capacity(mut self, max_musicians: usize, max_listeners: usize) -> ServerConfig {
        self.max_musicians = max_musicians;
        self.max_listeners = max_listeners;
        self
    }

    /// Overrides how long a silent member is held on the roster.
    pub fn with_member_timeout_ms(mut self, member_timeout_ms: u64) -> ServerConfig {
        self.member_timeout_ms = member_timeout_ms;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    MusicianCountChanged(usize),
    MemberJoined {
        id: MemberId,
        name: String,
    },
    MemberDisconnected {
        id: MemberId,
    },
    MemberRevoked {
        id: MemberId,
    },
    ProtocolViolation {
        id: MemberId,
        what: &'static str,
    },
    /// A member ran their violation budget out and was dropped. Their token
    /// stays valid: they can hand back a fresh handshake once the budget
    /// refills, which is what keeps a buggy client from being locked out of
    /// the session for good.
    MemberEjected {
        id: MemberId,
        violations: u64,
    },
    /// The host revoked a token id that was not already revoked. The core
    /// holds the list in memory only; whoever drives it is responsible for
    /// writing this down before the process can exit, or a restart hands the
    /// invite back.
    TokenRevoked {
        jti: TokenId,
    },
    /// An accepted host request for the broadcast pipeline. The core does not
    /// run processes, so it hands the op to whatever drives it (jamstreamd's
    /// runtime, which owns the stream worker) and stays deterministic. The op
    /// may carry a stream key: its `Debug` is redacted, and nothing in the
    /// core logs, stores, or relays it.
    StreamCtl(StreamOp),
    /// An accepted host request for the recorder, handed to the driver the
    /// same way as [`ServerEvent::StreamCtl`]. The driver reports back with
    /// [`ServerCore::set_record_status`].
    RecordCtl(RecordOp),
}

/// One member as the broadcast card renderer needs them, borrowed from the
/// core for the duration of one tick.
#[derive(Debug, Clone, Copy)]
pub struct BroadcastMember<'a> {
    pub id: MemberId,
    pub name: &'a str,
    pub connected: bool,
    /// Meter values with ballistics already applied.
    pub level_peak: f32,
    pub level_rms: f32,
    /// Content hash and bytes, when the server has them cached.
    pub avatar: Option<(&'a [u8; 32], &'a [u8])>,
}

/// Everything the broadcast pipeline needs from one mix tick. Valid until the
/// next [`ServerCore::tick`]: the audio slice is this tick's slot of the
/// broadcast accumulator, which the next tick overwrites eight ticks later.
#[derive(Debug)]
pub struct BroadcastTick<'a> {
    /// Post-limiter broadcast stereo for this tick: 240 interleaved samples.
    pub audio: &'a [f32],
    /// Musicians, in member id order, capped by the caller's card limit.
    pub members: Vec<BroadcastMember<'a>>,
    pub listeners: usize,
    /// Bumps on every roster change, so a caller can skip resending an
    /// unchanged roster to the renderer.
    pub roster_epoch: u64,
}

/// One musician's decoded audio from the last tick, pre-mix, with the
/// broadcast fader it mixes through. What the recorder's stem tap reads.
#[derive(Debug, Clone, Copy)]
pub struct Stem<'a> {
    pub id: MemberId,
    /// Decoded mono uplink for this tick.
    pub pcm: &'a [f32; TICK_SAMPLES],
    /// Host-set broadcast fader, unity when unset.
    pub fader: Fader,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemberStats {
    pub id: MemberId,
    pub role: Role,
    pub connected: bool,
    pub rtt_ms_last: Option<f32>,
    pub jitter: JitterStats,
    pub violations: u64,
}

/// Cached handshake response for idempotent retry: if the client's
/// HandshakeResp was lost, its resent (byte-identical) init gets the same
/// response back, paired with the transport state created on first receipt.
struct RespCache {
    init_hash: [u8; 32],
    resp: Vec<u8>,
    at_ms: u64,
}

struct Member {
    role: Role,
    name: String,
    jti: TokenId,
    addr: Option<SocketAddr>,
    session: Option<Session>,
    resp_cache: Option<RespCache>,
    link: ControlLink,
    jitter: JitterBuffer,
    /// Mono uplink decoder; musicians only.
    decoder: Option<Decoder>,
    /// Personal mix encoder; musicians only. Personal mixes genuinely differ
    /// per member, so each needs its own encoder state. Listeners all get the
    /// same broadcast frame and share [`ServerCore::bcast_encoder`].
    encoder: Option<Encoder>,
    faders: BTreeMap<MemberId, Fader>,
    click_enabled: bool,
    connected: bool,
    last_heard_ms: u64,
    /// Published on the roster: silent for longer than
    /// [`MEMBER_QUIET_AFTER_MS`] but not yet timed out. Stored rather than
    /// recomputed per read because it is the transition that has to send a
    /// roster, and the roster is edge-triggered.
    quiet: bool,
    rtt_ms_last: Option<f32>,
    send_seq: u32,
    /// Lifetime count, for the stats surface.
    violations: u64,
    /// What is left of this member's allowance for illegal packets. Survives
    /// disconnect and rejoin, so ejection is not undone by a handshake.
    violation_budget: TokenBucket,
    /// Allowance for messages that cost a fanout to every member or a piece
    /// of per-hash state. Connection-scoped, unlike the violation budget: a
    /// reconnecting client asks for every avatar on the roster again, and
    /// each reconnection costs it a full handshake anyway.
    fanout_budget: TokenBucket,
    /// Jitter stats snapshot at the last Stats report; deltas against it
    /// give the per-window uplink numbers.
    stats_prev: JitterStats,
    /// Broadcast card meters, updated per tick while the tap is on.
    level_peak: f32,
    level_rms: f32,
    /// Announced avatar (content hash, declared length). Survives
    /// disconnect and rejoin like the fader table.
    avatar: Option<(AvatarHash, u32)>,
    /// Inbound reassembly of this member's own avatar upload.
    avatar_rx: Option<AvatarRx>,
    /// Outbound trains for this member. Only the head streams, so each
    /// link carries one train at a time and different members' transfers
    /// progress independently.
    avatar_tx: VecDeque<AvatarTx>,
}

pub struct ServerCore {
    cfg: ServerConfig,
    members: BTreeMap<MemberId, Member>,
    revoked: HashSet<TokenId>,
    metronome: Metronome,
    metronome_enabled: bool,
    sample_clock: u64,
    tick_count: u64,
    limiter: Limiter,
    /// Per-tick stereo scratch, reused.
    mix_buf: Vec<f32>,
    /// Encoded-frame scratch, reused.
    pkt_buf: Vec<u8>,
    /// This tick's decoded musician audio, reused.
    decoded: Vec<(MemberId, [f32; TICK_SAMPLES])>,
    /// 20 ms broadcast accumulator.
    bcast_accum: Vec<f32>,
    /// The one encoder every listener's stream comes out of, built when the
    /// first listener is admitted. Encoded-frame scratch beside it, kept
    /// apart from `pkt_buf` so the payload survives the fanout loop.
    bcast_encoder: Option<Encoder>,
    bcast_pkt: Vec<u8>,
    bcast_clock: u64,
    /// Broadcast frames encoded since the core was built. Every listener gets
    /// byte-identical audio, so this must advance by exactly one per fan-out
    /// tick however many listeners are connected; encoding per listener once
    /// cost 20 x 190 us inside a 2500 us tick.
    bcast_encodes: u64,
    /// Accumulator slot the last tick wrote; the broadcast tap reads it.
    bcast_slot: usize,
    /// While set, per-member card meters are maintained. Off costs nothing.
    bcast_tap: bool,
    /// Host-set broadcast faders; absent members mix at unity.
    bcast_faders: BTreeMap<MemberId, Fader>,
    /// While set, the host's downlink carries the broadcast mix instead of
    /// their personal mix. Connection-scoped: cleared whenever the host
    /// disconnects or is readmitted.
    audition: bool,
    avatar_cache: AvatarCache,
    /// Members waiting for avatar bytes the server is still fetching from
    /// the owner; served when the upload completes and verifies. Entries
    /// for waiters who disconnect or hashes never completed are skipped or
    /// linger harmlessly (a few dozen bytes each).
    avatar_waiters: BTreeMap<AvatarHash, Vec<MemberId>>,
    /// When each slot last emitted a version reject. Fixed length
    /// [`REJECT_SLOTS`], never resized.
    reject_seen: Vec<Option<u64>>,
    reject_budget: TokenBucket,
    /// Blake2s with the slot key already absorbed, cloned per lookup. See
    /// [`ServerCore::limiter_slot`] for why the slot function is keyed.
    slot_hasher: Blake2s256,
    /// What an unauthenticated peer may spend of the handshake's asymmetric
    /// crypto, globally and per source network. Same fixed table and same
    /// slot function as the reject limiter: a source port is attacker-chosen,
    /// so anything keyed finer than a network limits nothing.
    init_slot_budget: Vec<TokenBucket>,
    init_budget: TokenBucket,
    /// Inits this core has paid a Diffie-Hellman for. The quantity the budget
    /// exists to bound, so a test can assert it rather than infer it.
    init_reads: u64,
    /// Drains as inits arrive; empty means the cookie round trip is engaged.
    /// A detector, not a limiter: nothing is dropped for failing to take one.
    cookie_trigger: TokenBucket,
    challenge_budget: TokenBucket,
    /// Seals every cookie challenge; a pure function of the static public
    /// key, derived once. See [`transport::cookie_reply_key`].
    cookie_reply_key: wire::CookieReplyKey,
    /// Cookie challenges emitted, so a test can assert the round trip
    /// engaged rather than infer it from an absence.
    challenges: u64,
    events: Vec<ServerEvent>,
    last_musician_count: usize,
    last_stats_ms: u64,
    /// Latest per-destination broadcast status, as the pipeline reported it.
    /// Key-free by construction: this goes to every member.
    stream_status: Vec<DestinationStatus>,
    last_stream_status_ms: u64,
    /// Whether this session can broadcast at all, as the driver's relay probe
    /// last answered. None until it has answered once, which is the only state
    /// that reads as "assume it works": a surface that dimmed Go Live before
    /// the first probe would refuse a broadcast the session can serve.
    broadcast_ready: Option<BroadcastReadiness>,
    /// Latest recorder state, as the driver reported it.
    record_status: RecordingState,
    /// Whether stems are captured alongside the mix; fixed for the session.
    record_stems: bool,
    roster_epoch: u64,
    /// Set by any roster change, cleared by the next tick's fanout.
    roster_dirty: bool,
    /// The server process's own log, when a binary published one. None under
    /// the harness and under every test that does not ask for it, which is
    /// what keeps a core with no subscriber behind it deterministic.
    log_tail: Option<LogTail>,
    log_budget: TokenBucket,
}

impl ServerCore {
    pub fn new(cfg: ServerConfig) -> Self {
        let slot_hasher = slot_hasher(&cfg.server_private);
        let cookie_reply_key = transport::cookie_reply_key(&cfg.server_public);
        Self {
            cfg,
            members: BTreeMap::new(),
            revoked: HashSet::new(),
            metronome: Metronome {
                bpm: 120,
                beats_per_bar: 4,
            },
            metronome_enabled: false,
            sample_clock: 0,
            tick_count: 0,
            limiter: Limiter::new(LIMITER_CEILING_DB, LIMITER_LOOKAHEAD_SAMPLES),
            mix_buf: vec![0.0; MIX_LEN],
            pkt_buf: Vec::new(),
            decoded: Vec::new(),
            bcast_accum: vec![0.0; BCAST_LEN],
            bcast_encoder: None,
            bcast_pkt: Vec::new(),
            bcast_clock: 0,
            bcast_encodes: 0,
            bcast_slot: 0,
            bcast_tap: false,
            bcast_faders: BTreeMap::new(),
            audition: false,
            avatar_cache: AvatarCache::new(AVATAR_CACHE_BYTES),
            avatar_waiters: BTreeMap::new(),
            reject_seen: vec![None; REJECT_SLOTS],
            reject_budget: TokenBucket::new(REJECT_BURST, REJECT_RATE_PER_SEC),
            slot_hasher,
            init_slot_budget: vec![
                TokenBucket::new(INIT_SLOT_BURST, INIT_SLOT_RATE_PER_SEC);
                REJECT_SLOTS
            ],
            init_budget: TokenBucket::new(INIT_BURST, INIT_RATE_PER_SEC),
            init_reads: 0,
            cookie_trigger: TokenBucket::new(COOKIE_TRIGGER_BURST, COOKIE_TRIGGER_RATE_PER_SEC),
            challenge_budget: TokenBucket::new(CHALLENGE_RATE_PER_SEC, CHALLENGE_RATE_PER_SEC),
            cookie_reply_key,
            challenges: 0,
            events: Vec::new(),
            last_musician_count: 0,
            last_stats_ms: 0,
            stream_status: Vec::new(),
            last_stream_status_ms: 0,
            broadcast_ready: None,
            record_status: RecordingState::Idle,
            record_stems: false,
            roster_epoch: 0,
            roster_dirty: false,
            log_tail: crate::logtail::installed(),
            log_budget: TokenBucket::new(SERVER_LOG_BURST, SERVER_LOG_PER_SEC),
        }
    }

    /// Feeds one datagram from the socket. Returns datagrams to send.
    pub fn handle_datagram(
        &mut self,
        now_ms: u64,
        now_unix: u64,
        src: SocketAddr,
        data: &[u8],
    ) -> Outgoing {
        let mut out = Vec::new();
        match wire::parse(data) {
            Ok(Packet::HandshakeInit { version, noise }) => {
                if version != PROTOCOL_VERSION {
                    // Left outside the cookie gate: this path has its own,
                    // much tighter budget, and a client on the wrong version
                    // has to be told so even while the server is under load.
                    self.version_reject(now_ms, src, version, noise, data, &mut out);
                } else if self.cookie_required(now_ms) {
                    // Under handshake load an unauthenticated init buys a
                    // 16-byte MAC and not an X25519: whoever sent it has to
                    // come back from the address it claims first.
                    self.cookie_challenge(now_ms, src, data, &mut out);
                } else {
                    self.admit(now_ms, now_unix, src, noise, &mut out);
                }
            }
            Ok(Packet::CookiedInit {
                cookie,
                version,
                noise,
            }) => {
                // Spent whether or not the cookie holds, so a flood of
                // cookied inits keeps the round trip engaged rather than
                // letting it switch itself back off.
                self.cookie_required(now_ms);
                // A wrong version draws nothing here: the reject's MAC covers
                // the exact bytes the sender sent, and a client that reached
                // this point sent a plain init too, which is what draws one.
                if version == PROTOCOL_VERSION && self.cookie_valid(now_ms, src, &cookie) {
                    self.admit(now_ms, now_unix, src, noise, &mut out);
                }
            }
            Ok(Packet::Transport {
                member,
                counter,
                ciphertext,
            }) => {
                self.handle_transport(now_ms, src, member, counter, ciphertext, &mut out);
            }
            // The server never receives these legitimately.
            Ok(Packet::HandshakeResp { .. })
            | Ok(Packet::VersionReject { .. })
            | Ok(Packet::CapacityReject { .. })
            | Ok(Packet::CookieChallenge { .. })
            | Err(_) => {}
        }
        out
    }

    /// Drives one 2.5 ms mix tick: decode uplinks, produce personal mixes and
    /// the broadcast mix, poll control links, scan for timeouts.
    pub fn tick(&mut self, now_ms: u64) -> Outgoing {
        let mut out = Vec::new();
        let clock = self.sample_clock;
        self.sample_clock += TICK_SAMPLES as u64;

        // Decode every connected musician's frame for this tick. Waiting
        // (buffer still filling) yields silence without touching the decoder.
        self.decoded.clear();
        for (&id, m) in self.members.iter_mut() {
            if !m.connected || m.role != Role::Musician {
                continue;
            }
            let mut pcm = [0.0f32; TICK_SAMPLES];
            let pulled = m.jitter.pull();
            if let Some(dec) = m.decoder.as_mut() {
                let result = match &pulled {
                    Pull::Frame(p) | Pull::Recovered(p) => dec.decode(Some(p), &mut pcm, false),
                    Pull::Missing => dec.decode(None, &mut pcm, false),
                    Pull::Waiting => Ok(()),
                };
                if result.is_err() {
                    pcm = [0.0; TICK_SAMPLES];
                }
            }
            if self.bcast_tap {
                // Card meters, computed once here rather than by a second
                // pass over the audio: peak with a slow fall, rms smoothed
                // the same way, so the broadcast's only moving element looks
                // like the client's own meters.
                let mut peak = 0.0f32;
                let mut sum_sq = 0.0f32;
                for &s in &pcm {
                    peak = peak.max(s.abs());
                    sum_sq += s * s;
                }
                let rms = (sum_sq / TICK_SAMPLES as f32).sqrt();
                m.level_peak = peak.max(m.level_peak * BCAST_LEVEL_DECAY);
                m.level_rms = rms.max(m.level_rms * BCAST_LEVEL_DECAY);
            }
            self.decoded.push((id, pcm));
        }

        let mut click = [0.0f32; TICK_SAMPLES];
        if self.metronome_enabled {
            self.metronome.render(clock, &mut click, CLICK_GAIN);
        }

        let sources: Vec<(MemberId, &[f32])> =
            self.decoded.iter().map(|(id, b)| (*id, &b[..])).collect();

        // Broadcast mix first: everyone through the host-set broadcast
        // faders (unity default) and the brickwall limiter, straight into
        // this tick's slot of the 20 ms accumulator. It runs before the
        // personal pass so an auditioning host can be fed the identical
        // post-limiter signal.
        let idx = (self.tick_count % BCAST_TICKS) as usize;
        self.bcast_slot = idx;
        if idx == 0 {
            self.bcast_clock = clock;
        }
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

        // Personal stereo mixes, each excluding its own member and shaped by
        // that member's fader table. An auditioning host instead gets this
        // tick's broadcast slice: the exact post-limiter stereo signal
        // listeners get, the host's own signal included, because hearing
        // what the stream hears is the point of auditioning. It still rides
        // the host's Ms2_5 musician encoder, so latency and cadence stay
        // musician-grade. No click either: listeners never hear it.
        let audition_pcm: Option<&[f32]> = if self.audition {
            Some(&self.bcast_accum[idx * MIX_LEN..(idx + 1) * MIX_LEN])
        } else {
            None
        };
        for (&id, m) in self.members.iter_mut() {
            if !m.connected || m.role != Role::Musician {
                continue;
            }
            let pcm: &[f32] = match audition_pcm {
                Some(b) if id == HOST_MEMBER_ID => b,
                _ => {
                    mix_into(
                        &sources,
                        |t| m.faders.get(&t).copied().unwrap_or_default(),
                        Some(id),
                        &mut self.mix_buf,
                    );
                    if self.metronome_enabled && m.click_enabled {
                        for (pair, &c) in self.mix_buf.chunks_exact_mut(2).zip(click.iter()) {
                            pair[0] += c;
                            pair[1] += c;
                        }
                    }
                    &self.mix_buf
                }
            };
            let Some(enc) = m.encoder.as_mut() else {
                continue;
            };
            if enc.encode(pcm, &mut self.pkt_buf).is_ok() {
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
                if let (Some(s), Some(a)) = (m.session.as_mut(), m.addr)
                    && let Ok(dg) = s.seal(id, &frame)
                {
                    out.push((a, dg));
                }
            }
        }

        // The accumulator holds a full 20 ms broadcast frame. Every listener
        // gets byte-identical PCM, so it is encoded once here and only the
        // sequence number and the seal differ per member: encoding it per
        // listener measured 20 x 190 us inside one 2500 us tick.
        let want_bcast = idx as u64 == BCAST_TICKS - 1
            && self
                .members
                .values()
                .any(|m| m.connected && m.role == Role::Listener);
        if want_bcast
            && let Some(enc) = self.bcast_encoder.as_mut()
            && enc.encode(&self.bcast_accum, &mut self.bcast_pkt).is_ok()
        {
            self.bcast_encodes += 1;
            for (&id, m) in self.members.iter_mut() {
                if !m.connected || m.role != Role::Listener {
                    continue;
                }
                let frame = MediaFrame {
                    seq: m.send_seq,
                    timestamp: self.bcast_clock,
                    duration: FrameDuration::Ms20,
                    stereo: true,
                    payload: &self.bcast_pkt,
                    redundant: None,
                }
                .encode();
                m.send_seq = m.send_seq.wrapping_add(1);
                if let (Some(s), Some(a)) = (m.session.as_mut(), m.addr)
                    && let Ok(dg) = s.seal(id, &frame)
                {
                    out.push((a, dg));
                }
            }
        }
        self.tick_count += 1;

        // Once a second, tell each musician what its uplink looks like from
        // here: their redundancy policy runs on our numbers, not a proxy.
        if now_ms.saturating_sub(self.last_stats_ms) >= STATS_INTERVAL_MS {
            self.last_stats_ms = now_ms;
            for m in self.members.values_mut() {
                if !m.connected || m.role != Role::Musician {
                    continue;
                }
                let cur = m.jitter.stats();
                let prev = std::mem::replace(&mut m.stats_prev, cur);
                let pulled = cur.pulled.saturating_sub(prev.pulled);
                let lost = cur.lost.saturating_sub(prev.lost);
                let recovered = cur.recovered.saturating_sub(prev.recovered);
                let pct = |n: u64| {
                    if pulled == 0 {
                        0.0
                    } else {
                        100.0 * n as f32 / pulled as f32
                    }
                };
                let _ = m.link.send(ControlMsg::Stats {
                    // Wire loss counts even when redundancy papered over it.
                    uplink_loss_pct: pct(lost + recovered),
                    uplink_jitter_depth: cur.depth_frames.min(usize::from(u16::MAX)) as u16,
                    uplink_recovered_pct: pct(recovered),
                });
            }
        }

        // Whatever changed the roster this tick, it fans out once.
        self.flush_roster();

        self.feed_server_log(now_ms);

        // Control-plane retransmits and acks. Avatar trains are fed here,
        // capped per tick, so bulk bytes never starve normal control
        // traffic on the ordered link (see the avatar module comment).
        for (&id, m) in self.members.iter_mut() {
            if !m.connected {
                continue;
            }
            let mut fed = 0;
            // Bulk stops while the link is backed up, so the queue's hard cap
            // is never reached by avatar chunks and a roster or a chat always
            // has room. A stalled train resumes when the acks catch up.
            while fed < AVATAR_CHUNKS_PER_POLL && m.link.pending_len() < AVATAR_FEED_HIGH_WATER {
                let Some(tx) = m.avatar_tx.front_mut() else {
                    break;
                };
                match self
                    .avatar_cache
                    .get(tx.hash())
                    .and_then(|bytes| tx.next_chunk(bytes))
                {
                    Some(chunk) => {
                        let _ = m.link.send(chunk);
                        fed += 1;
                    }
                    // Train finished (or its bytes were evicted mid-train,
                    // which pinning prevents for roster hashes): drop it.
                    None => {
                        m.avatar_tx.pop_front();
                    }
                }
            }
            let dgs = m.link.poll(now_ms);
            if let (Some(s), Some(a)) = (m.session.as_mut(), m.addr) {
                for dg in dgs {
                    if let Ok(p) = s.seal(id, &dg) {
                        out.push((a, p));
                    }
                }
            }
        }

        // Reap scan: keep state so the same token can rejoin, free the
        // address binding and transport.
        //
        // Silence for the member timeout is the usual path. A control link
        // that has given up retransmitting is the other one, and the timeout
        // never catches it: a peer that keeps media flowing while acking
        // nothing stays "heard from" forever, and the server can no longer
        // tell it anything. It reaches that state 65 s after a frame first
        // went unacked, so nothing on a merely bad link gets here.
        let gone: Vec<MemberId> = self
            .members
            .iter()
            .filter(|(_, m)| {
                m.connected
                    && (now_ms.saturating_sub(m.last_heard_ms) >= self.cfg.member_timeout_ms
                        || m.link.is_dead())
            })
            .map(|(&id, _)| id)
            .collect();
        if !gone.is_empty() {
            for id in gone {
                self.disconnect_member(id);
                self.events.push(ServerEvent::MemberDisconnected { id });
            }
            self.queue_roster();
            self.note_musician_count();
        }

        // Quiet scan, over whoever survived the reap. Same input as the scan
        // above, a different threshold, and it publishes rather than reaps: a
        // member unheard from for MEMBER_QUIET_AFTER_MS is still connected and
        // still holding their seat, but the roster now says nobody has heard
        // from them, a state a client otherwise has no way to show.
        //
        // Only a change queues a roster, so a session where everyone is
        // talking sends nothing extra ever. It runs after the reap so a
        // member being dropped this same tick is reported gone rather than
        // gone and quiet.
        let mut moved = false;
        for m in self.members.values_mut() {
            let quiet =
                m.connected && now_ms.saturating_sub(m.last_heard_ms) >= MEMBER_QUIET_AFTER_MS;
            if quiet != m.quiet {
                m.quiet = quiet;
                moved = true;
            }
        }
        if moved {
            self.queue_roster();
        }

        out
    }

    /// Tells every connected member the session is over and returns the
    /// datagrams to send. One flight each, no retransmit: the process is going
    /// away, and a client that misses this finds out by timeout. Members are
    /// marked disconnected, so a caller that keeps running (the harness) sees a
    /// clean roster.
    pub fn shutdown(&mut self, now_ms: u64, reason: &str) -> Outgoing {
        let mut out = Vec::new();
        // Before the Bye, so the last lines the server wrote ride the same
        // flight and arrive ahead of the word that the session is over. This
        // is the moment the whole mechanism exists for: a machine that is
        // being destroyed writes its most useful line last.
        self.feed_server_log(now_ms);
        let connected: Vec<MemberId> = self
            .members
            .iter()
            .filter(|(_, m)| m.connected)
            .map(|(&id, _)| id)
            .collect();
        for id in connected {
            self.farewell(now_ms, id, reason, &mut out);
            self.disconnect_member(id);
            self.events.push(ServerEvent::MemberDisconnected { id });
        }
        self.note_musician_count();
        out
    }

    /// Drops whichever connected member is bound to this address, for a driver
    /// that caught a panic partway through that member's datagram and cannot
    /// trust what was left behind. No Bye: the transport state that would carry
    /// one is exactly what stopped being trustworthy. Their token stays valid,
    /// so a client on the receiving end of somebody else's bug comes back with
    /// a fresh handshake.
    pub fn drop_peer(&mut self, addr: SocketAddr) -> Option<MemberId> {
        let id = self
            .members
            .iter()
            .find(|(_, m)| m.connected && m.addr == Some(addr))
            .map(|(&id, _)| id)?;
        self.disconnect_member(id);
        self.events.push(ServerEvent::MemberDisconnected { id });
        self.queue_roster();
        self.note_musician_count();
        Some(id)
    }

    /// Seeds the revocation list from whatever the driver persisted, before
    /// the first datagram arrives. The core keeps the list in memory only, so
    /// without this a restart, which `Restart=on-failure` makes cheap to
    /// provoke, handed every revoked invite back.
    pub fn restore_revoked(&mut self, jtis: Vec<TokenId>) {
        self.revoked.extend(jtis);
    }

    /// Drains accumulated events.
    pub fn events(&mut self) -> Vec<ServerEvent> {
        std::mem::take(&mut self.events)
    }

    /// Turns per-member card metering on or off. The broadcast pipeline wants
    /// it while streaming; nothing else does, and off it costs nothing.
    pub fn set_broadcast_tap(&mut self, on: bool) {
        if self.bcast_tap == on {
            return;
        }
        self.bcast_tap = on;
        if !on {
            for m in self.members.values_mut() {
                m.level_peak = 0.0;
                m.level_rms = 0.0;
            }
        }
    }

    pub fn broadcast_tap(&self) -> bool {
        self.bcast_tap
    }

    /// The last tick's broadcast audio on its own, for a consumer that wants
    /// the samples and nothing else. [`ServerCore::broadcast_tick`] also
    /// builds the card roster, which costs an allocation and an avatar cache
    /// lookup per musician; the recorder wants none of it and calls 400 times
    /// a second.
    pub fn broadcast_audio(&self) -> &[f32] {
        let start = self.bcast_slot * MIX_LEN;
        &self.bcast_accum[start..start + MIX_LEN]
    }

    /// The last tick's broadcast audio and card state, for the stream
    /// pipeline. Call it right after [`ServerCore::tick`]: the audio slice is
    /// the accumulator slot that tick wrote.
    pub fn broadcast_tick(&self) -> BroadcastTick<'_> {
        let members =
            self.members
                .iter()
                .filter(|(_, m)| m.role == Role::Musician)
                .map(|(&id, m)| BroadcastMember {
                    id,
                    name: &m.name,
                    connected: m.connected,
                    level_peak: m.level_peak,
                    level_rms: m.level_rms,
                    // Both borrows live as long as the tick: the hash is the
                    // member's own field, the bytes are the server's cache.
                    avatar: m.avatar.as_ref().and_then(|(hash, _)| {
                        self.avatar_cache.get(hash).map(|bytes| (hash, bytes))
                    }),
                })
                .collect();
        BroadcastTick {
            audio: self.broadcast_audio(),
            members,
            listeners: self
                .members
                .values()
                .filter(|m| m.connected && m.role == Role::Listener)
                .count(),
            roster_epoch: self.roster_epoch,
        }
    }

    /// The last tick's decoded musician audio, for the recorder's stem tap.
    /// Call it right after [`ServerCore::tick`], like
    /// [`ServerCore::broadcast_tick`]; it reads state the tick already built,
    /// so it costs nothing when nobody records.
    pub fn stems(&self) -> impl Iterator<Item = Stem<'_>> {
        self.decoded.iter().map(|(id, pcm)| Stem {
            id: *id,
            pcm,
            fader: self.bcast_faders.get(id).copied().unwrap_or_default(),
        })
    }

    /// Publishes the broadcast pipeline's per-destination status. Fans out
    /// immediately on any change and at least once a second while anything is
    /// configured, so every member (not just the host) sees the on-air state.
    /// The caller is the pipeline's driver; the status it passes carries no
    /// stream key by construction.
    pub fn set_stream_status(&mut self, now_ms: u64, destinations: Vec<DestinationStatus>) {
        let changed = destinations != self.stream_status;
        let due = !destinations.is_empty()
            && now_ms.saturating_sub(self.last_stream_status_ms) >= STREAM_STATUS_INTERVAL_MS;
        self.stream_status = destinations;
        if changed || due {
            self.last_stream_status_ms = now_ms;
            self.queue_stream_status();
        }
    }

    pub fn stream_status(&self) -> &[DestinationStatus] {
        &self.stream_status
    }

    /// Publishes whether this session can broadcast at all, from the driver's
    /// relay probe. On change only: it answers the same way for hours at a
    /// time, and every member holds the latest answer.
    ///
    /// Sent to everyone rather than the host alone, like the on-air state: a
    /// musician who can see the room is not being broadcast is better informed
    /// than one who cannot.
    pub fn set_broadcast_readiness(&mut self, state: BroadcastReadiness) {
        if self.broadcast_ready.as_ref() == Some(&state) {
            return;
        }
        self.broadcast_ready = Some(state.clone());
        let msg = ControlMsg::BroadcastReadiness { state };
        for m in self.members.values_mut().filter(|m| m.connected) {
            let _ = m.link.send(msg.clone());
        }
    }

    /// The last readiness answer, or None while the probe has not answered.
    pub fn broadcast_readiness(&self) -> Option<&BroadcastReadiness> {
        self.broadcast_ready.as_ref()
    }

    /// Recorder state from its driver, broadcast to every member on change.
    /// Unlike stream status there is no periodic re-send: the recorder has
    /// no per-second numbers, only transitions, and the latest snapshot is
    /// always sufficient by the message's contract.
    pub fn set_record_status(&mut self, state: RecordingState, stems: bool) {
        if state == self.record_status && stems == self.record_stems {
            return;
        }
        self.record_status = state;
        self.record_stems = stems;
        let msg = ControlMsg::RecordStatus {
            state: self.record_status.clone(),
            stems: self.record_stems,
        };
        for m in self.members.values_mut().filter(|m| m.connected) {
            let _ = m.link.send(msg.clone());
        }
    }

    pub fn record_status(&self) -> &RecordingState {
        &self.record_status
    }

    /// Broadcast frames encoded since this core was built. The listener
    /// stream is encoded once per 20 ms and sealed per member, so this is the
    /// count a caller compares against ticks rather than against listeners.
    pub fn broadcast_encodes(&self) -> u64 {
        self.bcast_encodes
    }

    pub fn musicians_connected(&self) -> usize {
        self.members
            .values()
            .filter(|m| m.connected && m.role == Role::Musician)
            .count()
    }

    pub fn stats(&self) -> Vec<MemberStats> {
        self.members
            .iter()
            .map(|(&id, m)| MemberStats {
                id,
                role: m.role,
                connected: m.connected,
                rtt_ms_last: m.rtt_ms_last,
                jitter: m.jitter.stats(),
                violations: m.violations,
            })
            .collect()
    }

    /// The only reply an unauthenticated peer ever draws, so every step here
    /// is a step an attacker gets to run at line rate. It is ordered cheapest
    /// first and does no work at all once either limiter says no: the source
    /// port is attacker-chosen, so the earlier `ip:port` key made every
    /// packet a fresh key, and the retain-on-insert that kept that map from
    /// growing turned a flood into quadratic work.
    fn version_reject(
        &mut self,
        now_ms: u64,
        src: SocketAddr,
        theirs: u16,
        noise: &[u8],
        init_packet: &[u8],
        out: &mut Outgoing,
    ) {
        if init_packet.len() < REJECT_MIN_INIT_BYTES {
            return;
        }
        let slot = self.limiter_slot(src.ip());
        let recent =
            self.reject_seen[slot].is_some_and(|t| now_ms.saturating_sub(t) < REJECT_INTERVAL_MS);
        if recent {
            return;
        }
        // Checked after the per-slot gate and left unstamped when it fails,
        // so an exhausted budget does not consume an honest client's turn.
        if !self.reject_budget.take(now_ms) {
            return;
        }
        // Both limiters are spent before the key derivation, not after: the
        // derivation reads the Noise first message, which is two X25519
        // operations and an AEAD open, and a flood of unreadable inits must
        // buy the attacker REJECT_RATE_PER_SEC of those and no more.
        self.reject_seen[slot] = Some(now_ms);
        let Some(key) = transport::reject_key_for_init(
            &self.cfg.server_private,
            &self.cfg.session_id,
            theirs,
            noise,
        ) else {
            // Not a first flight this build can read, so there is nobody to
            // authenticate a reject to. Silence, as for any other garbage.
            return;
        };
        out.push((
            src,
            wire::build_version_reject(&key, PROTOCOL_VERSION, theirs, init_packet),
        ));
    }

    /// Which limiter slot a source network falls in.
    ///
    /// [`REJECT_SLOTS`] slots for the whole internet, so sources collide by
    /// design and a collision costs the pair an interval of each other's
    /// allowance. That is only acceptable while nobody can choose who they
    /// collide with: with a public hash an attacker could search offline for
    /// addresses landing on a chosen victim's slot and spend that victim's
    /// share on purpose, which turns a limiter meant to bound a flood into a
    /// way to hold one client off the session, silently and at 8 packets a
    /// second. Keyed on the server's static private key, the same address
    /// lands somewhere different on every session and the best an attacker
    /// can do is collide by luck.
    ///
    /// Granularity is one IPv4 address or one IPv6 /64, tagged by family: a
    /// band behind one NAT is one source here, and a v6 host holding a whole
    /// /64 cannot spread over the table by walking it.
    fn limiter_slot(&self, ip: IpAddr) -> usize {
        let mut h = self.slot_hasher.clone();
        match ip {
            IpAddr::V4(v4) => {
                h.update([4u8]);
                h.update(v4.octets());
            }
            IpAddr::V6(v6) => {
                h.update([6u8]);
                h.update(&v6.octets()[..8]);
            }
        }
        let out: [u8; 32] = h.finalize().into();
        let x = u64::from_le_bytes(out[..8].try_into().expect("8 bytes"));
        (x as usize) & (REJECT_SLOTS - 1)
    }

    /// Spends one unauthenticated peer's share of the handshake budget. The
    /// per-network bucket goes first so a single flooding host cannot empty
    /// the global one, and neither is charged when the other refuses.
    fn init_budget_take(&mut self, now_ms: u64, src: SocketAddr) -> bool {
        let slot = self.limiter_slot(src.ip());
        if !self.init_slot_budget[slot].available(now_ms) || !self.init_budget.available(now_ms) {
            return false;
        }
        self.init_slot_budget[slot].take(now_ms) && self.init_budget.take(now_ms)
    }

    /// Handshake inits this core has paid a Diffie-Hellman for. Bounded per
    /// second by construction; the tick has to survive whatever an
    /// unauthenticated flood asks for.
    pub fn handshake_reads(&self) -> u64 {
        self.init_reads
    }

    /// Cookie challenges emitted. Zero on a session nobody is flooding,
    /// which is the property that keeps an ordinary join at one round trip.
    pub fn cookie_challenges(&self) -> u64 {
        self.challenges
    }

    /// Whether the cookie round trip is engaged, spending one init's worth of
    /// the trigger.
    ///
    /// Rate-triggered rather than always on because the round trip costs a
    /// joining client a whole extra flight, and a session nobody is attacking
    /// should not pay for one. The bucket drains on inits that would
    /// otherwise reach admission, so it measures exactly the pressure the
    /// cookie relieves, and refills when the flood stops.
    fn cookie_required(&mut self, now_ms: u64) -> bool {
        !self.cookie_trigger.take(now_ms)
    }

    /// Answers an init with a sealed cookie instead of reading it.
    ///
    /// The cookie is encrypted under a key derived from the static public
    /// key, with the init's exact bytes as the AEAD's additional data, so the
    /// reply is bound to the one init it answers: nobody who did not see this
    /// init, and nobody who does not know `server_pk`, can produce a
    /// challenge the client will accept. Cheaper than proof of who sent it,
    /// which would cost the Diffie-Hellman the cookie exists to avoid; an
    /// invite holder on the path still knows both, which is WireGuard's
    /// residue too, and against that the client keeps offering its plain init
    /// alongside the cookied one.
    fn cookie_challenge(
        &mut self,
        now_ms: u64,
        src: SocketAddr,
        init_packet: &[u8],
        out: &mut Outgoing,
    ) {
        // Same rule as the version reject's floor: never answer a stub with a
        // bigger packet. Above the floor the challenge is always the smaller.
        if init_packet.len() < CHALLENGE_MIN_INIT_BYTES {
            return;
        }
        if !self.challenge_budget.take(now_ms) {
            return;
        }
        self.challenges += 1;
        let key = transport::cookie_key(&self.cfg.server_private, cookie_epoch(now_ms));
        out.push((
            src,
            wire::build_cookie_challenge(&self.cookie_reply_key, &key, src.ip(), init_packet),
        ));
    }

    /// Whether a cookie is one this server handed to this address.
    ///
    /// The previous epoch is accepted as well as the current one, so a
    /// rotation does not invalidate a cookie already in flight. Nothing was
    /// stored when the cookie was issued, so both are recomputed: two hashes
    /// over a few dozen bytes, against the 30 to 50 microseconds of the
    /// Diffie-Hellman this stands in front of.
    fn cookie_valid(&self, now_ms: u64, src: SocketAddr, cookie: &[u8; COOKIE_BYTES]) -> bool {
        let epoch = cookie_epoch(now_ms);
        [epoch, epoch.saturating_sub(1)].iter().any(|&e| {
            let key = transport::cookie_key(&self.cfg.server_private, e);
            wire::cookie_matches(&key, src.ip(), cookie)
        })
    }

    /// Emits an authenticated capacity reject, through the same per-source
    /// gate and global budget as the version reject.
    ///
    /// Both are packets the server sends in answer to an inbound one, and the
    /// total reflected volume is exactly what those limits exist to bound, so
    /// they share them rather than each getting an allowance. A suppressed
    /// reject costs an honest client one more resend, which it was going to
    /// send anyway. The key comes out of the handshake the caller already
    /// read: one X25519, no second pass over the Noise message.
    fn capacity_reject(
        &mut self,
        now_ms: u64,
        src: SocketAddr,
        responder: &Responder,
        init_packet: &[u8],
        out: &mut Outgoing,
    ) {
        let slot = self.limiter_slot(src.ip());
        if self.reject_seen[slot].is_some_and(|t| now_ms.saturating_sub(t) < REJECT_INTERVAL_MS) {
            return;
        }
        // Left unstamped when the budget refuses, as on the version reject
        // path, so an exhausted budget does not consume a client's turn.
        if !self.reject_budget.take(now_ms) {
            return;
        }
        self.reject_seen[slot] = Some(now_ms);
        let Some(key) = responder.reject_key(&self.cfg.server_private) else {
            return;
        };
        out.push((src, wire::build_capacity_reject(&key, init_packet)));
    }

    /// Full admission path for a version-matched handshake init. Every
    /// refusal upstream of the token check is silent: to an unauthenticated
    /// peer the server looks like packet loss. Capacity is checked after it,
    /// so that one refusal is answered.
    fn admit(
        &mut self,
        now_ms: u64,
        now_unix: u64,
        src: SocketAddr,
        noise: &[u8],
        out: &mut Outgoing,
    ) {
        // Everything below this line costs asymmetric crypto, so the budget
        // is spent first and a refusal is silent. An honest client resends
        // its init, so a drop here is packet loss to it; the flood it is
        // sharing the server with is what made the difference.
        if !self.init_budget_take(now_ms, src) {
            return;
        }
        self.init_reads += 1;
        let Ok((hp, responder)) = Responder::read_init(
            &self.cfg.server_private,
            &self.cfg.session_id,
            PROTOCOL_VERSION,
            noise,
        ) else {
            return;
        };
        if verify_token(
            &self.cfg.issuer_pk,
            &self.cfg.session_id,
            &self.cfg.server_public,
            &hp.token,
            &hp.signature,
            now_unix,
        )
        .is_err()
        {
            tracing::debug!("handshake dropped: token failed verification");
            return;
        }
        let token = hp.token;
        if self.revoked.contains(&token.jti) {
            return;
        }
        let id = token.member_id;
        // An ejected member gets back in when their violation budget does,
        // not the moment they redo the handshake. Without this, ejection cost
        // an abusive peer one handshake and bought them nothing.
        if let Some(m) = self.members.get_mut(&id)
            && !m.violation_budget.available(now_ms)
        {
            tracing::debug!(member = id.0, "handshake dropped: violation budget");
            return;
        }
        let init_hash: [u8; 32] = Blake2s256::digest(noise).into();
        if let Some(m) = self.members.get(&id)
            && m.connected
        {
            // Idempotent retry: the client lost our HandshakeResp and resent
            // the byte-identical init. Resend the cached response; it pairs
            // with the transport state created on first receipt, so no new
            // state is made.
            if let Some(cache) = m.resp_cache.as_ref()
                && now_ms.saturating_sub(cache.at_ms) <= RESP_CACHE_MS
                && cache.init_hash == init_hash
            {
                out.push((src, cache.resp.clone()));
                return;
            }
            // Live member, different (or cache-expired) init: silent drop.
            // A replayed stale init lands here or, past the silence window,
            // on the fast-rejoin path below; either way the replayer lacks
            // the ephemeral key behind the init and can never complete the
            // handshake or produce transport traffic.
            if now_ms.saturating_sub(m.last_heard_ms) <= REJOIN_SILENCE_MS {
                return;
            }
            // Fast rejoin: the member went quiet and is back with a fresh
            // handshake before the full timeout. Tear down the old
            // connection state and admit fresh below.
        }
        // Capacity counts everyone in the role, the host included: member 0
        // is a musician like the rest, so `max_musicians` is the size of the
        // band, not the number of guests.
        let connected_in_role = self
            .members
            .iter()
            .filter(|(mid, m)| **mid != id && m.connected && m.role == token.role)
            .count();
        let cap = match token.role {
            Role::Musician => self.cfg.max_musicians,
            Role::Listener => self.cfg.max_listeners,
        };
        if connected_in_role >= cap {
            tracing::debug!(member = id.0, "admission refused: role at capacity");
            // This peer's token verified, so it holds an invite this session
            // issued and telling it the truth makes the server no kind of
            // oracle: everything upstream of the token check is still
            // answered with silence. Without this a listener joining a full
            // gallery waited out its own 10 s timeout and could not tell a
            // sold-out session from a server that was down, so its next move
            // was to retry a join that could never succeed.
            //
            // The MAC covers the plain init's bytes whichever framing carried
            // the Noise message here, so a client that offered both the plain
            // and the cookied form verifies the reject either way.
            let init_packet = wire::build_handshake_init(PROTOCOL_VERSION, noise);
            self.capacity_reject(now_ms, src, &responder, &init_packet, out);
            return;
        }
        // One encoder serves every listener, built on the first one to join.
        if token.role == Role::Listener && self.bcast_encoder.is_none() {
            match Encoder::new(Channels::Stereo, FrameDuration::Ms20, BROADCAST_BITRATE) {
                Ok(e) => self.bcast_encoder = Some(e),
                Err(_) => {
                    tracing::error!("broadcast encoder construction failed at admission");
                    return;
                }
            }
        }
        let media = match token.role {
            Role::Musician => {
                Encoder::new(Channels::Stereo, FrameDuration::Ms2_5, PERSONAL_MIX_BITRATE).and_then(
                    |e| {
                        Decoder::new(Channels::Mono, FrameDuration::Ms2_5)
                            .map(|d| (Some(e), Some(d)))
                    },
                )
            }
            Role::Listener => Ok((None, None)),
        };
        let Ok((encoder, decoder)) = media else {
            tracing::error!("codec construction failed at admission");
            return;
        };
        let welcome = Welcome {
            member_id: id,
            sample_clock: self.sample_clock,
        };
        let Ok((session, resp)) = responder.respond(&welcome) else {
            return;
        };

        // The roster carries the name to everyone, and `ControlLink` refuses
        // to carry one past MAX_NAME_LEN, so a hint longer than the cap would
        // silently break roster fanout for the whole session rather than for
        // the member who brought it.
        let name = token
            .name_hint
            .clone()
            .filter(|n| n.len() <= MAX_NAME_LEN)
            .unwrap_or_else(|| format!("member {}", id.0));
        // A rejoin keeps the member's mixer state; everything stream-scoped
        // starts fresh with the new transport. Audition never survives a
        // (re)join: the host comes back on their personal mix.
        if id == HOST_MEMBER_ID {
            self.audition = false;
        }
        let prev = self.members.remove(&id);
        // The violation record follows the member across a rejoin for the
        // same reason the fader table does: a fresh handshake is not a fresh
        // reputation.
        let (faders, click_enabled, avatar, violations, violation_budget) = prev.map_or_else(
            || {
                (
                    BTreeMap::new(),
                    true,
                    None,
                    0,
                    TokenBucket::new(VIOLATION_BURST, VIOLATION_REFILL_PER_SEC),
                )
            },
            |p| {
                (
                    p.faders,
                    p.click_enabled,
                    p.avatar,
                    p.violations,
                    p.violation_budget,
                )
            },
        );
        self.members.insert(
            id,
            Member {
                role: token.role,
                name: name.clone(),
                jti: token.jti,
                addr: Some(src),
                session: Some(session),
                resp_cache: Some(RespCache {
                    init_hash,
                    resp: resp.clone(),
                    at_ms: now_ms,
                }),
                link: ControlLink::new(),
                jitter: JitterBuffer::new(),
                decoder,
                encoder,
                faders,
                click_enabled,
                connected: true,
                last_heard_ms: now_ms,
                quiet: false,
                rtt_ms_last: None,
                send_seq: 0,
                violations,
                violation_budget,
                fanout_budget: TokenBucket::new(FANOUT_BURST, FANOUT_REFILL_PER_SEC),
                stats_prev: JitterStats::default(),
                level_peak: 0.0,
                level_rms: 0.0,
                avatar,
                avatar_rx: None,
                avatar_tx: VecDeque::new(),
            },
        );
        out.push((src, resp));
        self.events.push(ServerEvent::MemberJoined { id, name });
        self.queue_roster();
        // A member who joins mid-broadcast learns they are on air now, not up
        // to a second later.
        if !self.stream_status.is_empty()
            && let Some(m) = self.members.get_mut(&id)
        {
            let _ = m.link.send(ControlMsg::StreamStatus {
                destinations: self.stream_status.clone(),
            });
        }
        // And whether the session can broadcast at all, which changes at most
        // once or twice a session: a host who joins after the answer arrived
        // would otherwise wait for it to change, which it never does.
        if let Some(state) = self.broadcast_ready.clone()
            && let Some(m) = self.members.get_mut(&id)
        {
            let _ = m.link.send(ControlMsg::BroadcastReadiness { state });
        }
        // Same for a take: a mid-take joiner is being recorded and gets told
        // so before their first note, not on the next transition.
        if self.record_status != RecordingState::Idle
            && let Some(m) = self.members.get_mut(&id)
        {
            let _ = m.link.send(ControlMsg::RecordStatus {
                state: self.record_status.clone(),
                stems: self.record_stems,
            });
        }
        self.note_musician_count();
    }

    fn handle_transport(
        &mut self,
        now_ms: u64,
        src: SocketAddr,
        member: MemberId,
        counter: u64,
        ciphertext: &[u8],
        out: &mut Outgoing,
    ) {
        // Nothing on this path counts a violation inline: the borrow of the
        // member ends first, so every violation goes through `violation`,
        // which is the one place the ejection threshold is applied.
        let msgs = {
            let Some(m) = self.members.get_mut(&member) else {
                return;
            };
            if !m.connected {
                // Disconnected members rejoin with a fresh handshake.
                return;
            }
            let Some(session) = m.session.as_mut() else {
                return;
            };
            let Ok(plain) = session.open(counter, ciphertext) else {
                return;
            };
            // Authenticated packet from a new address: NAT rebind.
            if m.addr != Some(src) {
                m.addr = Some(src);
            }
            m.last_heard_ms = now_ms;
            match wire::split_channel(&plain) {
                Ok((CHANNEL_MEDIA, _)) => {
                    if m.role != Role::Musician {
                        Err("media from listener")
                    } else {
                        match MediaFrame::decode(&plain) {
                            Ok(f) => {
                                m.jitter.push(MediaPacket {
                                    seq: f.seq,
                                    timestamp: f.timestamp,
                                    payload: f.payload.to_vec(),
                                    redundant: f.redundant.map(<[u8]>::to_vec),
                                });
                                // Media never polls the control link: at 400
                                // frames a second per musician it must not.
                                Ok(None)
                            }
                            Err(_) => Err("malformed media frame"),
                        }
                    }
                }
                Ok((CHANNEL_CONTROL, _)) => m
                    .link
                    .receive(&plain)
                    .map(Some)
                    .map_err(|_| "malformed control packet"),
                _ => Err("unknown channel"),
            }
        };
        let msgs = match msgs {
            Ok(Some(msgs)) => msgs,
            Ok(None) => return,
            Err(what) => {
                self.violation(now_ms, member, what);
                return;
            }
        };
        for msg in msgs {
            self.handle_control(now_ms, member, msg, out);
        }
        // Flush acks and any immediate replies (Pong) in the same call.
        self.flush_member_link(now_ms, member, out);
    }

    fn handle_control(&mut self, now_ms: u64, from: MemberId, msg: ControlMsg, out: &mut Outgoing) {
        match msg {
            // The from field is forced to the authenticated sender; the
            // client-supplied value is never trusted.
            ControlMsg::Chat { text, .. } => {
                if !self.take_fanout(now_ms, from) {
                    return;
                }
                let relay = ControlMsg::Chat { from, text };
                for m in self.members.values_mut().filter(|m| m.connected) {
                    let _ = m.link.send(relay.clone());
                }
            }
            ControlMsg::MixerSet {
                target,
                gain_db,
                pan,
                muted,
            } => {
                if !gain_db.is_finite() || !pan.is_finite() {
                    self.violation(now_ms, from, "non-finite fader");
                    return;
                }
                if let Some(m) = self.members.get_mut(&from) {
                    m.faders.insert(
                        target,
                        Fader {
                            gain_db: gain_db.clamp(-96.0, 24.0),
                            pan: pan.clamp(-1.0, 1.0),
                            muted,
                        },
                    );
                }
            }
            ControlMsg::MetronomeSet {
                bpm,
                beats_per_bar,
                enabled,
            } => {
                if from != HOST_MEMBER_ID {
                    self.violation(now_ms, from, "metronome set by non-host");
                    return;
                }
                self.metronome = Metronome { bpm, beats_per_bar };
                self.metronome_enabled = enabled;
                let relay = ControlMsg::MetronomeSet {
                    bpm,
                    beats_per_bar,
                    enabled,
                };
                for m in self.members.values_mut().filter(|m| m.connected) {
                    let _ = m.link.send(relay.clone());
                }
            }
            ControlMsg::ClickEnable { enabled } => {
                if let Some(m) = self.members.get_mut(&from) {
                    m.click_enabled = enabled;
                }
            }
            ControlMsg::BroadcastMixSet {
                target,
                gain_db,
                pan,
                muted,
            } => {
                if from != HOST_MEMBER_ID {
                    self.violation(now_ms, from, "broadcast mix set by non-host");
                    return;
                }
                if !gain_db.is_finite() || !pan.is_finite() {
                    self.violation(now_ms, from, "non-finite fader");
                    return;
                }
                let gain_db = gain_db.clamp(-96.0, 24.0);
                let pan = pan.clamp(-1.0, 1.0);
                self.bcast_faders.insert(
                    target,
                    Fader {
                        gain_db,
                        pan,
                        muted,
                    },
                );
                // Relay the accepted (clamped) values so UIs can mirror.
                let relay = ControlMsg::BroadcastMixSet {
                    target,
                    gain_db,
                    pan,
                    muted,
                };
                for m in self.members.values_mut().filter(|m| m.connected) {
                    let _ = m.link.send(relay.clone());
                }
            }
            ControlMsg::BroadcastAudition { enabled } => {
                if from != HOST_MEMBER_ID {
                    self.violation(now_ms, from, "broadcast audition by non-host");
                    return;
                }
                self.audition = enabled;
            }
            ControlMsg::Ping { nonce, sent_ms } => {
                if let Some(m) = self.members.get_mut(&from) {
                    let _ = m.link.send(ControlMsg::Pong { nonce, sent_ms });
                }
            }
            ControlMsg::Pong { sent_ms, .. } => {
                if let Some(m) = self.members.get_mut(&from) {
                    m.rtt_ms_last = Some(now_ms.saturating_sub(sent_ms) as f32);
                }
            }
            ControlMsg::Revoke { jti } => {
                if from != HOST_MEMBER_ID {
                    self.violation(now_ms, from, "revoke by non-host");
                    return;
                }
                if self.revoked.insert(jti) {
                    // The driver persists on this: the list lives in memory
                    // here, and the whole feature was one crash away from
                    // being silently undone.
                    self.events.push(ServerEvent::TokenRevoked { jti });
                }
                let target = self
                    .members
                    .iter()
                    .find(|(_, m)| m.jti == jti)
                    .map(|(&id, _)| id);
                if let Some(id) = target {
                    self.farewell(now_ms, id, "invite revoked", out);
                    if id == HOST_MEMBER_ID {
                        self.audition = false;
                    }
                    self.members.remove(&id);
                    self.events.push(ServerEvent::MemberRevoked { id });
                    self.queue_roster();
                    self.note_musician_count();
                }
            }
            ControlMsg::Bye { .. } => {
                if self.members.get(&from).is_some_and(|m| m.connected) {
                    self.disconnect_member(from);
                    self.events
                        .push(ServerEvent::MemberDisconnected { id: from });
                    self.queue_roster();
                    self.note_musician_count();
                }
            }
            ControlMsg::SetAvatar { hash, len } => {
                if len == 0 || len as usize > MAX_AVATAR_BYTES {
                    self.violation(now_ms, from, "avatar length out of range");
                    return;
                }
                // A re-announce on rejoin costs nothing and is not charged. A
                // change costs a roster to every member plus a request back,
                // which was 224 outbound bytes for every inbound byte.
                let unchanged = self
                    .members
                    .get(&from)
                    .is_some_and(|m| m.avatar == Some((hash, len)));
                if !unchanged && !self.take_fanout(now_ms, from) {
                    return;
                }
                let have_bytes = self.avatar_cache.contains(&hash);
                if have_bytes {
                    self.avatar_cache.touch(&hash);
                }
                let Some(m) = self.members.get_mut(&from) else {
                    return;
                };
                m.avatar = Some((hash, len));
                if have_bytes {
                    m.avatar_rx = None;
                } else if !(unchanged && m.avatar_rx.as_ref().is_some_and(|rx| *rx.hash() == hash))
                {
                    // Pull the bytes from the owner; a replacement discards
                    // any half-reassembled previous upload.
                    m.avatar_rx = Some(AvatarRx::new(hash, Some(len)));
                    let _ = m.link.send(ControlMsg::AvatarRequest { hash });
                }
                // An idempotent re-announce (rejoin) changes no roster state.
                if !unchanged {
                    self.queue_roster();
                }
            }
            ControlMsg::AvatarChunk {
                hash,
                index,
                total,
                data,
            } => {
                let step = match self.members.get_mut(&from) {
                    Some(m) => match m.avatar_rx.as_mut() {
                        Some(rx) if *rx.hash() == hash => {
                            let step = rx.push(index, total, &data);
                            if !matches!(step, Ok(RxStep::More)) {
                                m.avatar_rx = None;
                            }
                            step
                        }
                        Some(_) => {
                            m.avatar_rx = None;
                            Err("avatar chunk for wrong hash")
                        }
                        None => Err("unsolicited avatar chunk"),
                    },
                    None => return,
                };
                match step {
                    Ok(RxStep::More) => {}
                    Ok(RxStep::Done(bytes)) => {
                        let pins: BTreeSet<AvatarHash> = self
                            .members
                            .values()
                            .filter_map(|m| m.avatar.map(|(h, _)| h))
                            .collect();
                        self.avatar_cache.insert(hash, bytes, &pins);
                        // Serve everyone who asked while the upload ran.
                        for id in self.avatar_waiters.remove(&hash).unwrap_or_default() {
                            if let Some(w) = self.members.get_mut(&id)
                                && w.connected
                            {
                                w.avatar_tx.push_back(AvatarTx::new(hash));
                            }
                        }
                    }
                    Err(what) => self.violation(now_ms, from, what),
                }
            }
            // Deliberately not metered: a request costs one lookup, one owner
            // scan, and at most one message to the owner, and a client joining
            // a full session legitimately sends one per roster entry. What
            // needed bounding was the waiter map it writes into.
            ControlMsg::AvatarRequest { hash } => {
                if self.avatar_cache.contains(&hash) {
                    self.avatar_cache.touch(&hash);
                    if let Some(m) = self.members.get_mut(&from)
                        && !m.avatar_tx.iter().any(|t| *t.hash() == hash)
                    {
                        m.avatar_tx.push_back(AvatarTx::new(hash));
                    }
                    return;
                }
                let owner = self.members.iter().find_map(|(&id, m)| {
                    m.avatar
                        .and_then(|(h, len)| (h == hash).then_some((id, len)))
                });
                let Some((owner_id, len)) = owner else {
                    // Unknown hash: almost certainly a race against a
                    // member who just left; not worth a violation.
                    return;
                };
                self.note_avatar_waiter(hash, from);
                // Nudge the owner if no upload is running (say its first
                // train failed validation); otherwise the in-flight upload
                // will serve the waiter on completion.
                if let Some(o) = self.members.get_mut(&owner_id)
                    && o.connected
                    && o.avatar_rx.is_none()
                {
                    o.avatar_rx = Some(AvatarRx::new(hash, Some(len)));
                    let _ = o.link.send(ControlMsg::AvatarRequest { hash });
                }
            }
            ControlMsg::StreamCtl { op } => {
                if from != HOST_MEMBER_ID {
                    self.violation(now_ms, from, "stream control by non-host");
                    return;
                }
                if let StreamOp::AddDestination { key, .. } = &op
                    && (key.is_empty() || key.len() > MAX_STREAM_KEY_LEN)
                {
                    self.violation(now_ms, from, "stream key out of range");
                    return;
                }
                // The core owns no processes: the op goes to the driver, which
                // owns the stream worker. Nothing here stores or relays it.
                self.events.push(ServerEvent::StreamCtl(op));
            }
            ControlMsg::RecordCtl { op } => {
                if from == HOST_MEMBER_ID {
                    // Same shape as StreamCtl: the driver owns the recorder.
                    self.events.push(ServerEvent::RecordCtl(op));
                } else {
                    self.violation(now_ms, from, "record control by non-host");
                }
            }
            ControlMsg::SetName { name } => {
                // Self only, like Chat's forced `from`: the sender is the
                // target. The link already refused anything past
                // MAX_NAME_LEN; empty after trimming is dropped here the way
                // an oversized name_hint is dropped at admission, costing
                // the sender their name and the session nothing.
                let name = name.trim();
                if name.is_empty() {
                    tracing::debug!(member = from.0, "empty SetName ignored");
                    return;
                }
                if let Some(m) = self.members.get_mut(&from)
                    && m.name != name
                {
                    m.name = name.to_owned();
                    self.queue_roster();
                }
            }
            ControlMsg::Roster(_) => self.violation(now_ms, from, "roster from client"),
            ControlMsg::Stats { .. } => self.violation(now_ms, from, "stats from client"),
            ControlMsg::StreamStatus { .. } => {
                self.violation(now_ms, from, "stream status from client")
            }
            ControlMsg::RecordStatus { .. } => {
                self.violation(now_ms, from, "record status from client")
            }
            ControlMsg::BroadcastReadiness { .. } => {
                self.violation(now_ms, from, "broadcast readiness from client")
            }
            ControlMsg::ServerLog { .. } => self.violation(now_ms, from, "server log from client"),
        }
    }

    fn flush_member_link(&mut self, now_ms: u64, id: MemberId, out: &mut Outgoing) {
        if let Some(m) = self.members.get_mut(&id)
            && m.connected
        {
            let dgs = m.link.poll(now_ms);
            if let (Some(s), Some(a)) = (m.session.as_mut(), m.addr) {
                for dg in dgs {
                    if let Ok(p) = s.seal(id, &dg) {
                        out.push((a, p));
                    }
                }
            }
        }
    }

    /// Moves whatever the server process has written to its log onto the
    /// host's link, a line per message.
    ///
    /// Host only. The log names members, addresses, and bucket paths, and the
    /// host is the party who launched the machine and pays for it; nobody else
    /// in the room has any business reading it.
    ///
    /// Lines go as they are written rather than being gathered for the end. A
    /// session that ends because the VM is being destroyed gets a single
    /// flight with no retransmit, so a log that waited for that moment would
    /// be delivering its first byte at the worst possible one; everything sent
    /// during the session has the ordered link's retransmits behind it
    /// instead. What the ring drops is counted and said out loud, because a
    /// gap a reader cannot see is worse than no log at all.
    fn feed_server_log(&mut self, now_ms: u64) {
        let Some(tail) = self.log_tail.as_ref() else {
            return;
        };
        let Some(host) = self.members.get_mut(&HOST_MEMBER_ID) else {
            return;
        };
        if !host.connected {
            return;
        }
        while host.link.pending_len() < SERVER_LOG_HIGH_WATER {
            let dropped = tail.dropped();
            let line = if dropped > 0 {
                format!("[{dropped} earlier server log line(s) dropped]")
            } else {
                match tail.take(1).pop() {
                    Some(line) => line,
                    None => break,
                }
            };
            if !self.log_budget.take(now_ms) {
                break;
            }
            if dropped > 0 {
                tail.clear_dropped();
            }
            let _ = host.link.send(ControlMsg::ServerLog { line });
        }
    }

    fn queue_stream_status(&mut self) {
        let msg = ControlMsg::StreamStatus {
            destinations: self.stream_status.clone(),
        };
        for m in self.members.values_mut().filter(|m| m.connected) {
            let _ = m.link.send(msg.clone());
        }
    }

    /// Marks the roster stale. The fanout itself waits for the next tick: a
    /// roster is the widest message the server sends, about 640 bytes to every
    /// member, and sending one per inbound packet was most of the measured
    /// 224x egress amplification. Coalescing costs at most 2.5 ms of latency
    /// on a join or leave notification.
    fn queue_roster(&mut self) {
        self.roster_dirty = true;
    }

    fn flush_roster(&mut self) {
        if !std::mem::take(&mut self.roster_dirty) {
            return;
        }
        self.roster_epoch += 1;
        let roster: Vec<MemberInfo> = self
            .members
            .iter()
            .map(|(&id, m)| MemberInfo {
                id,
                role: m.role,
                name: m.name.clone(),
                connected: m.connected,
                avatar_hash: m.avatar.map(|(h, _)| h),
                quiet: m.quiet,
            })
            .collect();
        for m in self.members.values_mut().filter(|m| m.connected) {
            let _ = m.link.send(ControlMsg::Roster(roster.clone()));
        }
    }

    fn note_musician_count(&mut self) {
        let count = self.musicians_connected();
        if count != self.last_musician_count {
            self.last_musician_count = count;
            self.events.push(ServerEvent::MusicianCountChanged(count));
        }
    }

    /// Spends one of a member's fanout tokens, charging a violation and
    /// answering false when the allowance is gone. The messages metered here
    /// each cost the server work on every other member's behalf, so at line
    /// rate one of them is a flood against the whole session and a multiplier
    /// on the host's egress bill.
    fn take_fanout(&mut self, now_ms: u64, from: MemberId) -> bool {
        let Some(m) = self.members.get_mut(&from) else {
            return false;
        };
        if m.fanout_budget.take(now_ms) {
            return true;
        }
        self.violation(now_ms, from, "control rate exceeded");
        false
    }

    /// Records that a member is waiting for avatar bytes the server does not
    /// have yet. Only a hash some member currently announces can be waited
    /// on, so entries for hashes nobody announces any more are dead and are
    /// dropped here rather than when a train completes: a member alternating
    /// SetAvatar and AvatarRequest on its own hash completes no train and would
    /// leave one permanent entry per pair of packets.
    fn note_avatar_waiter(&mut self, hash: AvatarHash, waiter: MemberId) {
        let cap = self.cfg.max_musicians + self.cfg.max_listeners;
        if self.avatar_waiters.len() >= cap && !self.avatar_waiters.contains_key(&hash) {
            let announced: BTreeSet<AvatarHash> = self
                .members
                .values()
                .filter_map(|m| m.avatar.map(|(h, _)| h))
                .collect();
            self.avatar_waiters.retain(|h, _| announced.contains(h));
            // At most one announced hash per member, so the prune always
            // frees a slot unless every entry is live, in which case there is
            // nothing to wait on that is not already tracked.
            if self.avatar_waiters.len() >= cap {
                return;
            }
        }
        let waiters = self.avatar_waiters.entry(hash).or_default();
        if !waiters.contains(&waiter) {
            waiters.push(waiter);
        }
    }

    /// Charges one protocol violation against a member, ejecting them once
    /// they run their budget out. Every violation site funnels through here,
    /// which is what keeps an admitted peer, a listener invite included, from
    /// sending illegal packets at line rate forever.
    fn violation(&mut self, now_ms: u64, id: MemberId, what: &'static str) {
        let Some(m) = self.members.get_mut(&id) else {
            self.events
                .push(ServerEvent::ProtocolViolation { id, what });
            return;
        };
        m.violations += 1;
        let violations = m.violations;
        // The budget is a bucket, not a lifetime total: a client with a
        // systematic bug trickles rather than being locked out of the session
        // for good, while a flood exhausts it in VIOLATION_BURST packets.
        let ejected = !m.violation_budget.take(now_ms);
        self.events
            .push(ServerEvent::ProtocolViolation { id, what });
        if ejected {
            self.disconnect_member(id);
            self.events
                .push(ServerEvent::MemberEjected { id, violations });
            self.queue_roster();
            self.note_musician_count();
        }
    }

    /// Best-effort Bye to one member: a single flight, no retry, because the
    /// member is going away and so, on the shutdown path, is the process.
    /// Silence would eject them by timeout anyway; this only means they learn
    /// why and when instead of ten seconds later.
    fn farewell(&mut self, now_ms: u64, id: MemberId, reason: &str, out: &mut Outgoing) {
        let Some(m) = self.members.get_mut(&id) else {
            return;
        };
        if !m.connected {
            return;
        }
        let _ = m.link.send(ControlMsg::Bye {
            reason: reason.to_owned(),
        });
        let dgs = m.link.poll(now_ms);
        if let (Some(s), Some(a)) = (m.session.as_mut(), m.addr) {
            for dg in dgs {
                if let Ok(p) = s.seal(id, &dg) {
                    out.push((a, p));
                }
            }
        }
    }

    /// Tears down one member's connection state, keeping the roster entry and
    /// everything that survives a rejoin (faders, click, announced avatar,
    /// violation budget). Shared by the timeout scan, `Bye`, and ejection.
    fn disconnect_member(&mut self, id: MemberId) {
        let Some(m) = self.members.get_mut(&id) else {
            return;
        };
        if !m.connected {
            return;
        }
        m.connected = false;
        // Gone, not quiet. A member who came back would otherwise be listed
        // as both, and the roster would say "here but silent" about somebody
        // who had just finished a fresh handshake.
        m.quiet = false;
        m.addr = None;
        m.session = None;
        m.resp_cache = None;
        // Transfers are connection-scoped; the announced hash stays.
        m.avatar_rx = None;
        m.avatar_tx.clear();
        if id == HOST_MEMBER_ID {
            self.audition = false;
        }
    }
}

/// Which reject slot a source claims. The network, not the address: a source
/// port is attacker-chosen, and a single IPv6 host routinely holds a whole
/// /64, so the low half of a v6 address is as free as the port was. The mix
/// is a fixed splitmix64 rather than a randomly seeded hasher because the
/// cores must replay identically under the harness; the worst an attacker
/// buys by computing a collision is one second of suppressed reject for the
/// address they collided with.
/// Which cookie secret is current. Derived from the caller's clock like
/// everything else in the core, so the harness replays a rotation exactly.
fn cookie_epoch(now_ms: u64) -> u64 {
    now_ms / COOKIE_ROTATION_MS
}

/// Blake2s with the limiter's slot key absorbed, ready to be cloned per
/// lookup. The key is a hash of the server's static private key, so it needs
/// no RNG and the core stays deterministic under the harness.
fn slot_hasher(server_private: &[u8]) -> Blake2s256 {
    let mut derive = Blake2s256::new();
    derive.update(SLOT_KEY_DOMAIN);
    derive.update(server_private);
    let key: [u8; 32] = derive.finalize().into();
    let mut hasher = Blake2s256::new();
    hasher.update(key);
    hasher
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamstream_protocol::invite::{Issuer, Token};
    use jamstream_protocol::transport::{Initiator, generate_keypair};

    fn addr(n: u8) -> SocketAddr {
        format!("10.0.0.{n}:5000").parse().unwrap()
    }

    /// A first flight from a client speaking a version this build does not,
    /// which is the only thing that draws a reject: the reject is
    /// authenticated with a secret recovered from the init, so an init the
    /// server cannot read is answered with silence like any other garbage.
    fn wrong_version_init(issuer: &Issuer, server_pk: [u8; 32]) -> (Initiator, Vec<u8>) {
        let invite = issuer.mint(
            SessionId([7u8; 16]),
            vec![addr(1)],
            server_pk,
            Token {
                member_id: MemberId(1),
                role: Role::Musician,
                name_hint: None,
                expires_unix: u64::MAX,
                jti: TokenId([1u8; 16]),
            },
        );
        Initiator::new_claiming_version(&invite, 9).unwrap()
    }

    /// A fixed server keypair. [`ServerCore::limiter_slot`] is keyed on the
    /// private key, so a test that asserts two sources have separate
    /// allowances needs to know they do not share a slot; with a generated key
    /// that would be true 255 runs in 256.
    fn fixed_server_keys() -> (Vec<u8>, [u8; 32]) {
        let private = vec![0x5Au8; 32];
        let public = transport::derive_public(&private).expect("32-byte private key");
        (private, public)
    }

    fn server_with_issuer() -> (ServerCore, Issuer, [u8; 32]) {
        let issuer = Issuer::generate();
        let (private, public) = fixed_server_keys();
        let core = ServerCore::new(ServerConfig::new(
            SessionId([7u8; 16]),
            private,
            public,
            issuer.public_key(),
        ));
        (core, issuer, public)
    }

    #[test]
    fn expired_token_is_refused_silently() {
        let (mut core, issuer, public) = server_with_issuer();
        let invite = issuer.mint(
            SessionId([7u8; 16]),
            vec![addr(1)],
            public,
            Token {
                member_id: MemberId(1),
                role: Role::Musician,
                name_hint: None,
                expires_unix: 100,
                jti: TokenId([1u8; 16]),
            },
        );
        let (_init, pkt) = Initiator::new(&invite).unwrap();
        let out = core.handle_datagram(0, 200, addr(1), &pkt);
        assert!(out.is_empty());
        assert!(core.events().is_empty());
        assert_eq!(core.musicians_connected(), 0);
    }

    #[test]
    fn version_reject_is_rate_limited_per_source() {
        let (mut core, issuer, public) = server_with_issuer();
        let (initiator, init) = wrong_version_init(&issuer, public);
        let out = core.handle_datagram(0, 0, addr(2), &init);
        assert_eq!(out.len(), 1);
        let Ok(Packet::VersionReject { ours, theirs, mac }) = wire::parse(&out[0].1) else {
            panic!("expected version reject");
        };
        assert_eq!((ours, theirs), (PROTOCOL_VERSION, 9));
        assert!(wire::verify_version_reject(
            initiator.reject_key().unwrap(),
            ours,
            theirs,
            &mac,
            &init
        ));
        // Within a second: silence. A different source still gets one.
        assert!(core.handle_datagram(500, 0, addr(2), &init).is_empty());
        assert_eq!(core.handle_datagram(500, 0, addr(3), &init).len(), 1);
        // After the interval the same source is answered again.
        assert_eq!(core.handle_datagram(1_500, 0, addr(2), &init).len(), 1);
    }

    /// Which limiter slot a source lands in must not be computable from the
    /// address alone.
    ///
    /// The table is [`REJECT_SLOTS`] slots for the whole internet, so sources
    /// share slots and a slot's allowance with them. With a public hash an
    /// attacker searches offline for addresses landing on one victim's slot,
    /// then spends that victim's share of the reject gate and the handshake
    /// budget on purpose: the victim's join is dropped silently and the reject
    /// that would have told it why never comes. Keyed on the static private
    /// key, the same address lands somewhere different on every session.
    #[test]
    fn a_sources_limiter_slot_is_not_computable_without_the_server_key() {
        let issuer = Issuer::generate();
        let core_for = |private: Vec<u8>| {
            let public = transport::derive_public(&private).expect("32-byte private key");
            ServerCore::new(ServerConfig::new(
                SessionId([7u8; 16]),
                private,
                public,
                issuer.public_key(),
            ))
        };
        let one = core_for(vec![1u8; 32]);
        let two = core_for(vec![2u8; 32]);

        // A slot is stable within one session: the limiters would not limit
        // anything if the same source moved between lookups.
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        assert_eq!(one.limiter_slot(ip), one.limiter_slot(ip));

        // Across sessions the mapping is a different one. Sampled over enough
        // addresses that agreeing everywhere cannot be coincidence: two
        // independent maps into 256 slots agree on 400 addresses with
        // probability 256^-400.
        let sample: Vec<IpAddr> = (0..400u32)
            .map(|n| IpAddr::from(std::net::Ipv4Addr::from(0x1400_0000 + n)))
            .collect();
        let moved = sample
            .iter()
            .filter(|ip| one.limiter_slot(**ip) != two.limiter_slot(**ip))
            .count();
        assert!(
            moved > 300,
            "only {moved} of 400 addresses moved slot between two servers, so the slot \
             function is not keyed on the server key"
        );

        // The whole table is still reachable, so keying did not collapse the
        // spread that makes per-source allowances mean anything.
        let slots: BTreeSet<usize> = sample.iter().map(|ip| one.limiter_slot(*ip)).collect();
        assert!(
            slots.len() > 150,
            "400 addresses filled only {} slots",
            slots.len()
        );

        // One v6 host holding a whole /64 is one source, and a v4 address is
        // not its v4-mapped v6 spelling.
        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "2001:db8::dead:beef".parse().unwrap();
        assert_eq!(one.limiter_slot(a), one.limiter_slot(b));
        let v4: IpAddr = "203.0.113.7".parse().unwrap();
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
        assert_ne!(one.limiter_slot(v4), one.limiter_slot(mapped));
    }

    /// A UDP source port is chosen by whoever sends the packet, so a limiter
    /// keyed on `ip:port` sees a fresh key every time and limits nothing. One
    /// host walking ports must draw one reject, not thousands.
    #[test]
    fn one_host_cannot_walk_source_ports_for_unlimited_rejects() {
        let (mut core, issuer, public) = server_with_issuer();
        let (_initiator, init) = wrong_version_init(&issuer, public);
        let mut rejects = 0;
        for port in 1_024..6_024u16 {
            let src: SocketAddr = format!("203.0.113.7:{port}").parse().unwrap();
            rejects += core.handle_datagram(0, 0, src, &init).len();
        }
        assert_eq!(rejects, 1, "5000 source ports drew {rejects} rejects");
    }

    /// Spoofed source addresses defeat any per-source key, so the total
    /// reject rate is capped too: the server is not a reflector whatever the
    /// source distribution.
    #[test]
    fn reject_volume_is_capped_across_all_sources() {
        let (mut core, issuer, public) = server_with_issuer();
        let (_initiator, init) = wrong_version_init(&issuer, public);
        let mut rejects = 0;
        // 40,000 distinct /24s at one instant: far more slots than the table
        // has, so only the global budget can hold this down.
        for a in 0..160u16 {
            for b in 0..250u16 {
                let src: SocketAddr = format!("198.18.{a}.{b}:9000").parse().unwrap();
                rejects += core.handle_datagram(0, 0, src, &init).len();
            }
        }
        assert_eq!(
            rejects, REJECT_BURST as usize,
            "40,000 distinct sources drew {rejects} rejects"
        );
        // The budget refills, so honest mismatched clients still get told.
        assert_eq!(core.handle_datagram(1_000, 0, addr(9), &init).len(), 1);
    }

    /// `Responder::read_init` performs an X25519 before anything about the
    /// sender is known, on the task that also runs the 2.5 ms mix tick. A
    /// flood must not be able to buy more of that than the budgets allow,
    /// however widely it spreads its source addresses.
    #[test]
    fn an_init_flood_cannot_buy_unbounded_asymmetric_crypto() {
        let (mut core, _issuer, _public) = server_with_issuer();
        let init = wire::build_handshake_init(PROTOCOL_VERSION, &[0xAA; 96]);
        // 40,000 distinct sources at one instant, far more than the table has
        // slots, so no per-source key can hold this down.
        let flood = |core: &mut ServerCore, now_ms: u64| {
            for a in 0..160u16 {
                for b in 0..250u16 {
                    let src: SocketAddr = format!("198.18.{a}.{b}:9000").parse().unwrap();
                    core.handle_datagram(now_ms, 0, src, &init);
                }
            }
        };
        flood(&mut core, 0);
        // The cookie trigger empties first, so what a spoofed flood buys is
        // its burst and then a 17-byte MAC per packet instead of an X25519.
        assert_eq!(core.handshake_reads(), u64::from(COOKIE_TRIGGER_BURST));
        assert!(
            core.cookie_challenges() > 0,
            "the flood was never asked for a cookie"
        );
        // And no faster than the trigger rate: the flood cannot buy more by
        // waiting, because it never comes back from the addresses it claims.
        flood(&mut core, 1_000);
        assert_eq!(
            core.handshake_reads(),
            u64::from(COOKIE_TRIGGER_BURST) + u64::from(COOKIE_TRIGGER_RATE_PER_SEC)
        );
        flood(&mut core, 2_000);
        assert_eq!(
            core.handshake_reads(),
            u64::from(COOKIE_TRIGGER_BURST) + 2 * u64::from(COOKIE_TRIGGER_RATE_PER_SEC)
        );
    }

    /// A single host cannot spend the whole allowance and leave a band
    /// arriving from anywhere else with nothing. Below the cookie trigger, so
    /// this is the per-network share of the Diffie-Hellman budget on its own.
    #[test]
    fn one_source_cannot_spend_the_whole_handshake_budget() {
        let (mut core, _issuer, _public) = server_with_issuer();
        let init = wire::build_handshake_init(PROTOCOL_VERSION, &[0xAA; 96]);
        // Walking source ports, which is what a limiter keyed on ip:port
        // would see as a fresh peer every time.
        for port in 1_024..1_064u16 {
            let src: SocketAddr = format!("203.0.113.7:{port}").parse().unwrap();
            core.handle_datagram(0, 0, src, &init);
        }
        assert_eq!(core.handshake_reads(), u64::from(INIT_SLOT_BURST));
        assert_eq!(core.cookie_challenges(), 0, "40 inits is not a flood");
        // And the rest of the budget is still there for everybody else.
        core.handle_datagram(0, 0, addr(9), &init);
        assert_eq!(core.handshake_reads(), u64::from(INIT_SLOT_BURST) + 1);
    }

    /// The cookie proves a source address is real, which is what makes the
    /// per-network share of the Diffie-Hellman budget bite: a spoofed flood
    /// spreads over every slot, a cookie holder cannot.
    #[test]
    fn a_cookie_holder_is_still_capped_at_its_networks_share() {
        let (mut core, _issuer, public) = server_with_issuer();
        let init = wire::build_handshake_init(PROTOCOL_VERSION, &[0xAA; 96]);
        let src: SocketAddr = "203.0.113.7:5000".parse().unwrap();

        // Drain the trigger so the round trip is engaged, then take the
        // cookie the server offers this address, opening it the way a client
        // does: against the init that drew it.
        for _ in 0..COOKIE_TRIGGER_BURST {
            core.handle_datagram(0, 0, src, &init);
        }
        let out = core.handle_datagram(0, 0, src, &init);
        let Ok(Packet::CookieChallenge { nonce, sealed }) = wire::parse(&out[0].1) else {
            panic!("expected a challenge, got {:?}", wire::parse(&out[0].1));
        };
        let cookie = wire::open_cookie_challenge(
            &transport::cookie_reply_key(&public),
            &nonce,
            &sealed,
            &init,
        )
        .expect("the challenge opens against the init that drew it");
        let cookied = wire::build_cookied_init(&cookie, PROTOCOL_VERSION, &[0xAA; 96]);
        let before = core.handshake_reads();

        // A thousand cookied inits from the address the cookie names, over a
        // whole second, walking ports the way a flood does.
        for i in 0..1_000u64 {
            let port = 5_000 + (i % 500) as u16;
            let from: SocketAddr = format!("203.0.113.7:{port}").parse().unwrap();
            core.handle_datagram(1_000 + i, 0, from, &cookied);
        }
        let bought = core.handshake_reads() - before;
        // One network's second: its refill, plus at most whatever the burst
        // had left when the run started.
        assert!(
            bought <= u64::from(INIT_SLOT_BURST) + u64::from(INIT_SLOT_RATE_PER_SEC),
            "a cookie holder bought {bought} Diffie-Hellmans in a second"
        );

        // The same cookie is no use from a different address: it is a MAC over
        // the source, so spoofing one does not carry the cookie with it.
        let elsewhere: SocketAddr = "198.51.100.9:5000".parse().unwrap();
        let before = core.handshake_reads();
        for i in 0..50u64 {
            core.handle_datagram(3_000 + i, 0, elsewhere, &cookied);
        }
        assert_eq!(
            core.handshake_reads(),
            before,
            "a cookie was accepted from an address it was not issued to"
        );
    }

    /// Under load a valid invite is not enough on its own: the round trip is
    /// the point, so the plain init draws a challenge and no join, and only the
    /// cookied form is read. Real invite, real handshake, real core.
    #[test]
    fn under_load_only_a_cookied_init_is_read() {
        let (mut core, issuer, public) = server_with_issuer();
        let filler = wire::build_handshake_init(PROTOCOL_VERSION, &[0xAA; 96]);
        for _ in 0..COOKIE_TRIGGER_BURST {
            core.handle_datagram(0, 0, addr(2), &filler);
        }
        let invite = issuer.mint(
            SessionId([7u8; 16]),
            vec![addr(1)],
            public,
            Token {
                member_id: MemberId(1),
                role: Role::Musician,
                name_hint: None,
                expires_unix: u64::MAX,
                jti: TokenId([1u8; 16]),
            },
        );
        let (_, init) = Initiator::new(&invite).unwrap();
        let honest = addr(9);

        // The plain init: a challenge, no read, no member. The cookie comes
        // out the way a client gets it, through the AEAD bound to this init.
        let reads = core.handshake_reads();
        let out = core.handle_datagram(0, 0, honest, &init);
        let Ok(Packet::CookieChallenge { nonce, sealed }) = wire::parse(&out[0].1) else {
            panic!("expected a challenge, got {:?}", wire::parse(&out[0].1));
        };
        let cookie = wire::open_cookie_challenge(
            &transport::cookie_reply_key(&public),
            &nonce,
            &sealed,
            &init,
        )
        .expect("the challenge opens against the init that drew it");
        assert_eq!(core.handshake_reads(), reads, "the init was read anyway");
        assert_eq!(core.musicians_connected(), 0);

        // A cookie that was not issued to this address, and a cookie with a
        // bit flipped: both silent, neither read.
        let elsewhere = wire::build_cookied_init(&cookie, PROTOCOL_VERSION, &[0xAA; 96]);
        assert!(core.handle_datagram(0, 0, addr(8), &elsewhere).is_empty());
        let mut tampered = cookie;
        tampered[0] ^= 0x01;
        let Ok(Packet::HandshakeInit { noise, .. }) = wire::parse(&init) else {
            panic!("expected an init");
        };
        let bad = wire::build_cookied_init(&tampered, PROTOCOL_VERSION, noise);
        assert!(core.handle_datagram(0, 0, honest, &bad).is_empty());
        assert_eq!(core.handshake_reads(), reads);

        // The genuine cookied init joins, even with the flood still running.
        let cookied = wire::build_cookied_init(&cookie, PROTOCOL_VERSION, noise);
        let out = core.handle_datagram(0, 0, honest, &cookied);
        assert!(matches!(
            wire::parse(&out[0].1),
            Ok(Packet::HandshakeResp { .. })
        ));
        assert_eq!(core.musicians_connected(), 1);
        assert_eq!(core.handshake_reads(), reads + 1);
    }

    /// A client offering both forms sends one handshake, not two. The Noise
    /// message is identical, so the second arrival is the idempotent-retry path
    /// and hands back the cached response rather than building fresh state.
    #[test]
    fn both_forms_of_one_init_are_one_handshake() {
        let (mut core, issuer, public) = server_with_issuer();
        let invite = issuer.mint(
            SessionId([7u8; 16]),
            vec![addr(1)],
            public,
            Token {
                member_id: MemberId(1),
                role: Role::Musician,
                name_hint: None,
                expires_unix: u64::MAX,
                jti: TokenId([1u8; 16]),
            },
        );
        let (_, init) = Initiator::new(&invite).unwrap();
        let Ok(Packet::HandshakeInit { noise, .. }) = wire::parse(&init) else {
            panic!("expected an init");
        };
        let cookied = wire::build_cookied_init(&[0u8; COOKIE_BYTES], PROTOCOL_VERSION, noise);

        // Not under load, so the plain one is admitted.
        let first = core.handle_datagram(0, 0, addr(9), &init);
        assert_eq!(core.handshake_reads(), 1);
        // The cookied one that followed it carries a cookie this server never
        // issued, so it is dropped without a second read.
        assert!(core.handle_datagram(0, 0, addr(9), &cookied).is_empty());
        assert_eq!(core.handshake_reads(), 1);
        // A resent plain init gets the cached response, byte for byte, and
        // creates no second member. It does cost a second read: the member the
        // cache is keyed on is inside the encrypted payload, so the message has
        // to be opened before the cache can be consulted.
        let again = core.handle_datagram(1, 0, addr(9), &init);
        assert_eq!(first[0].1, again[0].1);
        assert_eq!(core.handshake_reads(), 2);
        assert_eq!(core.musicians_connected(), 1);
    }

    /// A capacity reject drawn by a cookied init has to verify against the
    /// plain init, because the client sends both forms and holds only one set
    /// of bytes to check a MAC against. Getting this wrong would leave the
    /// reject silently unverifiable under load, which is the shape of bug that
    /// passes both halves' own tests.
    #[test]
    fn a_capacity_reject_drawn_by_a_cookied_init_verifies_against_the_plain_one() {
        let issuer = Issuer::generate();
        let kp = generate_keypair();
        let public = kp.public;
        let mut core = ServerCore::new(
            ServerConfig::new(
                SessionId([7u8; 16]),
                kp.private.to_vec(),
                public,
                issuer.public_key(),
            )
            .with_capacity(0, 0),
        );
        let invite = issuer.mint(
            SessionId([7u8; 16]),
            vec![addr(1)],
            public,
            Token {
                member_id: MemberId(1),
                role: Role::Musician,
                name_hint: None,
                expires_unix: u64::MAX,
                jti: TokenId([1u8; 16]),
            },
        );
        let (initiator, init) = Initiator::new(&invite).unwrap();
        let src = addr(9);

        // Take a real cookie, then answer with the cookied form.
        let filler = wire::build_handshake_init(PROTOCOL_VERSION, &[0xAA; 96]);
        for _ in 0..COOKIE_TRIGGER_BURST {
            core.handle_datagram(0, 0, addr(2), &filler);
        }
        let out = core.handle_datagram(0, 0, src, &init);
        let Ok(Packet::CookieChallenge { nonce, sealed }) = wire::parse(&out[0].1) else {
            panic!("expected a challenge");
        };
        let cookie = wire::open_cookie_challenge(
            &transport::cookie_reply_key(&public),
            &nonce,
            &sealed,
            &init,
        )
        .expect("the challenge opens against the init that drew it");
        let Ok(Packet::HandshakeInit { noise, .. }) = wire::parse(&init) else {
            panic!("expected an init");
        };
        let cookied = wire::build_cookied_init(&cookie, PROTOCOL_VERSION, noise);
        // Past the per-source reject interval the challenge stamped.
        let out = core.handle_datagram(2_000, 0, src, &cookied);
        let Ok(Packet::CapacityReject { mac }) = wire::parse(&out[0].1) else {
            panic!(
                "expected a capacity reject, got {:?}",
                wire::parse(&out[0].1)
            );
        };
        assert!(
            wire::verify_capacity_reject(initiator.reject_key().unwrap(), &mac, &init),
            "the reject did not verify against the plain init the client holds"
        );
    }

    /// A cookie is a MAC over one source address under a secret that rotates,
    /// so the previous epoch's cookie still works across a rotation and one
    /// from two epochs back does not.
    #[test]
    fn a_cookie_expires_one_rotation_after_the_secret_that_made_it() {
        let (mut core, _issuer, public) = server_with_issuer();
        let init = wire::build_handshake_init(PROTOCOL_VERSION, &[0xAA; 96]);
        let src: SocketAddr = "203.0.113.7:5000".parse().unwrap();
        for _ in 0..COOKIE_TRIGGER_BURST {
            core.handle_datagram(0, 0, src, &init);
        }
        let out = core.handle_datagram(0, 0, src, &init);
        let Ok(Packet::CookieChallenge { nonce, sealed }) = wire::parse(&out[0].1) else {
            panic!("expected a challenge");
        };
        let cookie = wire::open_cookie_challenge(
            &transport::cookie_reply_key(&public),
            &nonce,
            &sealed,
            &init,
        )
        .expect("the challenge opens against the init that drew it");
        let cookied = wire::build_cookied_init(&cookie, PROTOCOL_VERSION, &[0xAA; 96]);

        // Still the current epoch, and then the previous one: a rotation must
        // not invalidate a cookie already in flight.
        let read = |core: &mut ServerCore, now_ms: u64| {
            let before = core.handshake_reads();
            core.handle_datagram(now_ms, 0, src, &cookied);
            core.handshake_reads() > before
        };
        assert!(read(&mut core, COOKIE_ROTATION_MS - 1));
        assert!(read(&mut core, COOKIE_ROTATION_MS + 1));
        // Two rotations on, it is gone. Nothing was stored to expire; the
        // secret it was made under is simply no longer computed.
        assert!(!read(&mut core, 2 * COOKIE_ROTATION_MS + 1));
        assert!(!read(&mut core, 9 * COOKIE_ROTATION_MS));
    }

    /// A cookie challenge is 57 bytes. Answering a 3-byte `[1, 1, 0]` with one
    /// would make the server an amplifier by size, and a challenge is the one
    /// thing here that is not otherwise rate limited per source.
    #[test]
    fn a_challenge_is_never_larger_than_the_init_it_answers() {
        let (mut core, _issuer, _public) = server_with_issuer();
        let long = wire::build_handshake_init(PROTOCOL_VERSION, &[0xAA; 96]);
        for _ in 0..COOKIE_TRIGGER_BURST {
            core.handle_datagram(0, 0, addr(2), &long);
        }
        for noise_len in [0usize, 1, 8, 44, CHALLENGE_MIN_INIT_BYTES - 4] {
            let short = wire::build_handshake_init(PROTOCOL_VERSION, &vec![0xAA; noise_len]);
            assert!(
                core.handle_datagram(0, 0, addr(3), &short).is_empty(),
                "{}-byte init drew a challenge",
                short.len()
            );
        }
        let out = core.handle_datagram(0, 0, addr(4), &long);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].1.len() < long.len(),
            "challenge {} bytes vs init {} bytes",
            out[0].1.len(),
            long.len()
        );
    }

    /// The challenge is the only answer with no per-source gate in front of
    /// it, so the ceiling on it is what stops an unbounded send loop on the
    /// task that owns the mix tick.
    #[test]
    fn challenge_volume_has_a_ceiling() {
        let (mut core, _issuer, _public) = server_with_issuer();
        let init = wire::build_handshake_init(PROTOCOL_VERSION, &[0xAA; 96]);
        // Comfortably more inits at one instant than the ceiling allows.
        let flood = usize::try_from(CHALLENGE_RATE_PER_SEC).unwrap() * 2;
        for i in 0..flood {
            let src: SocketAddr = format!("198.18.{}.{}:9000", (i >> 8) & 0xFF, i & 0xFF)
                .parse()
                .unwrap();
            core.handle_datagram(0, 0, src, &init);
        }
        assert_eq!(core.cookie_challenges(), u64::from(CHALLENGE_RATE_PER_SEC));
        // Past the ceiling an init draws silence, which is what it drew before
        // any of this existed.
        assert!(core.handle_datagram(0, 0, addr(9), &init).is_empty());
        // A second on, the allowance is back and no faster than the rate.
        for i in 0..flood {
            let src: SocketAddr = format!("198.18.{}.{}:9000", (i >> 8) & 0xFF, i & 0xFF)
                .parse()
                .unwrap();
            core.handle_datagram(1_000, 0, src, &init);
        }
        assert_eq!(
            core.cookie_challenges(),
            2 * u64::from(CHALLENGE_RATE_PER_SEC)
        );
    }

    /// The reject is 21 bytes. Answering a 3-byte `[1, 9, 0]` would make the
    /// server an amplifier by size, which the threat model rules out.
    #[test]
    fn a_reject_is_never_larger_than_the_init_it_answers() {
        let (mut core, issuer, public) = server_with_issuer();
        for noise_len in [0usize, 1, 8, 44] {
            let short = wire::build_handshake_init(9, &vec![0xAA; noise_len]);
            assert!(
                core.handle_datagram(0, 0, addr(2), &short).is_empty(),
                "{}-byte init drew a reject",
                short.len()
            );
        }
        let (_initiator, init) = wrong_version_init(&issuer, public);
        let out = core.handle_datagram(0, 0, addr(2), &init);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].1.len() < init.len(),
            "reject {} bytes vs init {} bytes",
            out[0].1.len(),
            init.len()
        );
    }

    /// The reject carries a MAC over a secret recovered from the init, so an
    /// init this server cannot read leaves nobody to authenticate it to and
    /// draws no answer, whatever its version says.
    #[test]
    fn an_unreadable_init_draws_no_reject() {
        let (mut core, _issuer, _public) = server_with_issuer();
        let garbage = wire::build_handshake_init(9, &[0xAA; 96]);
        assert!(core.handle_datagram(0, 0, addr(2), &garbage).is_empty());
    }

    /// The capacity check runs after the token verifies, so the peer has
    /// already proven it holds an invite this session issued and can safely
    /// be told the truth. Silence instead leaves a listener joining a full
    /// gallery to wait out its own 10 s timeout, unable to tell a sold-out
    /// session from a server that is down.
    #[test]
    fn a_full_role_draws_an_authenticated_capacity_reject() {
        let issuer = Issuer::generate();
        let kp = generate_keypair();
        let public = kp.public;
        // One listener seat, so the second listener is over capacity.
        let mut core = ServerCore::new(
            ServerConfig::new(
                SessionId([7u8; 16]),
                kp.private.to_vec(),
                public,
                issuer.public_key(),
            )
            .with_capacity(4, 1),
        );
        let listener = |member: u16| {
            issuer.mint(
                SessionId([7u8; 16]),
                vec![addr(1)],
                public,
                Token {
                    member_id: MemberId(member),
                    role: Role::Listener,
                    name_hint: None,
                    expires_unix: u64::MAX,
                    jti: TokenId([member as u8; 16]),
                },
            )
        };

        // The first listener is admitted and gets a handshake response.
        let (_, first_init) = Initiator::new(&listener(10)).unwrap();
        let out = core.handle_datagram(0, 0, addr(10), &first_init);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            wire::parse(&out[0].1),
            Ok(Packet::HandshakeResp { .. })
        ));

        // The second draws a capacity reject its own handshake authenticates.
        let over = listener(11);
        let (initiator, init) = Initiator::new(&over).unwrap();
        let out = core.handle_datagram(0, 0, addr(11), &init);
        assert_eq!(out.len(), 1, "a full role must not answer with silence");
        assert_eq!(out[0].0, addr(11));
        let Ok(Packet::CapacityReject { mac }) = wire::parse(&out[0].1) else {
            panic!("expected a capacity reject, got {:?}", out[0].1);
        };
        assert!(wire::verify_capacity_reject(
            initiator.reject_key().unwrap(),
            &mac,
            &init
        ));
        assert!(out[0].1.len() < init.len(), "never an amplifier");
        // Refused means refused: no seat was taken.
        assert_eq!(core.broadcast_tick().listeners, 1);

        // Nobody else can forge one. An invite carries the server's public
        // key and nothing more, so a second handshake with the same invite
        // derives a different key.
        let (other, _) = Initiator::new(&over).unwrap();
        assert!(!wire::verify_capacity_reject(
            other.reject_key().unwrap(),
            &mac,
            &init
        ));

        // A musician seat is still free, and a musician still gets in: the
        // reject is about one role, not about the session.
        let musician = issuer.mint(
            SessionId([7u8; 16]),
            vec![addr(1)],
            public,
            Token {
                member_id: MemberId(0),
                role: Role::Musician,
                name_hint: None,
                expires_unix: u64::MAX,
                jti: TokenId([99u8; 16]),
            },
        );
        let (_, m_init) = Initiator::new(&musician).unwrap();
        // A second later, past the per-source reject interval.
        let out = core.handle_datagram(2_000, 0, addr(12), &m_init);
        assert!(matches!(
            wire::parse(&out[0].1),
            Ok(Packet::HandshakeResp { .. })
        ));
    }

    /// Everything upstream of the token check still gets silence: answering
    /// an arbitrary init would make the server an oracle for which invites
    /// exist and a reflector for whoever spoofed the source.
    #[test]
    fn an_unverified_peer_is_never_told_the_session_is_full() {
        let issuer = Issuer::generate();
        let stranger = Issuer::generate();
        let kp = generate_keypair();
        let public = kp.public;
        let mut core = ServerCore::new(
            ServerConfig::new(
                SessionId([7u8; 16]),
                kp.private.to_vec(),
                public,
                issuer.public_key(),
            )
            .with_capacity(1, 0),
        );
        let token = |member: u16| Token {
            member_id: MemberId(member),
            role: Role::Musician,
            name_hint: None,
            expires_unix: u64::MAX,
            jti: TokenId([member as u8; 16]),
        };
        // Fill the one musician seat.
        let host = issuer.mint(SessionId([7u8; 16]), vec![addr(1)], public, token(0));
        let (_, init) = Initiator::new(&host).unwrap();
        assert_eq!(core.handle_datagram(0, 0, addr(1), &init).len(), 1);

        // Signed by somebody else's issuer: silence, though the session is
        // as full for this peer as for anyone.
        let forged = stranger.mint(SessionId([7u8; 16]), vec![addr(1)], public, token(3));
        let (_, forged_init) = Initiator::new(&forged).unwrap();
        assert!(
            core.handle_datagram(2_000, 0, addr(3), &forged_init)
                .is_empty(),
            "an unsigned token drew a reject"
        );

        // Expired, and revoked: same silence.
        let mut expiring = token(4);
        expiring.expires_unix = 100;
        let expired = issuer.mint(SessionId([7u8; 16]), vec![addr(1)], public, expiring);
        let (_, expired_init) = Initiator::new(&expired).unwrap();
        assert!(
            core.handle_datagram(4_000, 200, addr(4), &expired_init)
                .is_empty(),
            "an expired token drew a reject"
        );

        let revoked = issuer.mint(SessionId([7u8; 16]), vec![addr(1)], public, token(5));
        core.restore_revoked(vec![revoked.token.jti]);
        let (_, revoked_init) = Initiator::new(&revoked).unwrap();
        assert!(
            core.handle_datagram(6_000, 0, addr(5), &revoked_init)
                .is_empty(),
            "a revoked token drew a reject"
        );

        // And garbage that never reaches a token at all.
        let garbage = wire::build_handshake_init(PROTOCOL_VERSION, &[0xAA; 96]);
        assert!(core.handle_datagram(8_000, 0, addr(6), &garbage).is_empty());
    }

    /// The reject is a packet the server sends because a packet arrived, so
    /// it shares the version reject's per-source gate and global budget. One
    /// invite holder replaying its own init cannot make the server a
    /// reflector.
    #[test]
    fn capacity_rejects_are_rate_limited_like_version_rejects() {
        let issuer = Issuer::generate();
        let kp = generate_keypair();
        let public = kp.public;
        let mut core = ServerCore::new(
            ServerConfig::new(
                SessionId([7u8; 16]),
                kp.private.to_vec(),
                public,
                issuer.public_key(),
            )
            .with_capacity(0, 0),
        );
        let invite = issuer.mint(
            SessionId([7u8; 16]),
            vec![addr(1)],
            public,
            Token {
                member_id: MemberId(1),
                role: Role::Musician,
                name_hint: None,
                expires_unix: u64::MAX,
                jti: TokenId([1u8; 16]),
            },
        );
        let (_, init) = Initiator::new(&invite).unwrap();

        // Counted by type, not by datagram: under load the same init draws a
        // cookie challenge, which is not a reject.
        let count = |out: &Outgoing| {
            out.iter()
                .filter(|(_, dg)| matches!(wire::parse(dg), Ok(Packet::CapacityReject { .. })))
                .count()
        };

        assert_eq!(count(&core.handle_datagram(0, 0, addr(2), &init)), 1);
        // Within the interval, the same source gets nothing back.
        assert!(core.handle_datagram(500, 0, addr(2), &init).is_empty());
        // Walking source ports does not buy another: the gate is keyed on the
        // network, and the port is chosen by whoever sends the packet.
        let mut answered = 0;
        for port in 1_024..1_124u16 {
            let src: SocketAddr = format!("10.0.0.2:{port}").parse().unwrap();
            answered += count(&core.handle_datagram(600, 0, src, &init));
        }
        assert_eq!(answered, 0, "{answered} rejects from one network in 100 ms");
        // Past the interval it is answered again.
        assert_eq!(count(&core.handle_datagram(1_500, 0, addr(2), &init)), 1);

        // And the total is capped however wide the sources are spread, which
        // is what a spoofed flood does.
        let mut fresh = ServerCore::new(
            ServerConfig::new(
                SessionId([7u8; 16]),
                kp.private.to_vec(),
                public,
                issuer.public_key(),
            )
            .with_capacity(0, 0),
        );
        let mut rejects = 0;
        for a in 0..40u16 {
            for b in 0..40u16 {
                let src: SocketAddr = format!("198.18.{a}.{b}:9000").parse().unwrap();
                rejects += count(&fresh.handle_datagram(0, 0, src, &init));
            }
        }
        // The cookie trigger and the init budget both bite before the reject
        // budget does: an init has to be read before its token can verify, and
        // that read is what those price.
        assert!(
            rejects <= REJECT_BURST as usize,
            "1600 sources drew {rejects} rejects"
        );
        assert!(rejects > 0, "the burst answered nobody");
    }

    #[test]
    fn transport_for_unknown_member_is_dropped() {
        let (mut core, _issuer, _public) = server_with_issuer();
        let pkt = wire::build_transport(MemberId(9), 0, &[1, 2, 3, 4]);
        assert!(core.handle_datagram(0, 0, addr(4), &pkt).is_empty());
        assert!(core.tick(0).is_empty());
    }
}
