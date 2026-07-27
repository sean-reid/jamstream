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
    ControlLink, ControlMsg, DestinationStatus, MAX_AVATAR_BYTES, MAX_NAME_LEN, MAX_STREAM_KEY_LEN,
    MemberInfo, StreamOp,
};
use jamstream_protocol::ids::{HOST_MEMBER_ID, MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::verify_token;
use jamstream_protocol::media::{FrameDuration, MediaFrame};
use jamstream_protocol::transport::{Responder, Session, Welcome};
use jamstream_protocol::wire::{self, CHANNEL_CONTROL, CHANNEL_MEDIA, Packet};

use crate::avatar::{AVATAR_CHUNKS_PER_POLL, AvatarCache, AvatarHash, AvatarRx, AvatarTx, RxStep};
use crate::limits::{
    DEFAULT_MEMBER_TIMEOUT_MS, FANOUT_BURST, FANOUT_REFILL_PER_SEC, MAX_LISTENERS, MAX_MUSICIANS,
    TokenBucket, VIOLATION_BURST, VIOLATION_REFILL_PER_SEC,
};

/// Samples per mix tick: 2.5 ms at 48 kHz.
const TICK_SAMPLES: usize = 120;
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
/// Slots the reject limiter keeps, indexed by a hash of the source network.
/// Sources that collide share one slot's allowance, which costs an honest
/// mismatched client at most a second of extra silence and buys an O(1)
/// per-packet check with no allocation and nothing to evict.
const REJECT_SLOTS: usize = 256;
const _: () = assert!(REJECT_SLOTS.is_power_of_two());
/// Version rejects emitted per second across every source, and the burst
/// allowed. A client on the wrong version needs exactly one to show its user
/// what to update, so this is generous for the honest case while capping
/// reflected volume at roughly 16 * 49 = 784 bytes per second on the wire.
const REJECT_RATE_PER_SEC: u32 = 16;
const REJECT_BURST: u32 = 16;
/// Shortest handshake init that earns a reject. A reject is 21 bytes and a
/// real Noise IK first message is over 90, so answering anything shorter
/// would make the server an amplifier by size: `[1, 9, 0]` in, 21 bytes out.
const REJECT_MIN_INIT_BYTES: usize = 48;
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
    /// Public half of `server_private`. Token signatures bind it, and the
    /// version reject is MAC'd with it, so the core needs it explicitly.
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
    events: Vec<ServerEvent>,
    last_musician_count: usize,
    last_stats_ms: u64,
    /// Latest per-destination broadcast status, as the pipeline reported it.
    /// Key-free by construction: this goes to every member.
    stream_status: Vec<DestinationStatus>,
    last_stream_status_ms: u64,
    roster_epoch: u64,
    /// Set by any roster change, cleared by the next tick's fanout.
    roster_dirty: bool,
}

impl ServerCore {
    pub fn new(cfg: ServerConfig) -> Self {
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
            bcast_slot: 0,
            bcast_tap: false,
            bcast_faders: BTreeMap::new(),
            audition: false,
            avatar_cache: AvatarCache::new(AVATAR_CACHE_BYTES),
            avatar_waiters: BTreeMap::new(),
            reject_seen: vec![None; REJECT_SLOTS],
            reject_budget: TokenBucket::new(REJECT_BURST, REJECT_RATE_PER_SEC),
            events: Vec::new(),
            last_musician_count: 0,
            last_stats_ms: 0,
            stream_status: Vec::new(),
            last_stream_status_ms: 0,
            roster_epoch: 0,
            roster_dirty: false,
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
                    self.version_reject(now_ms, src, version, data, &mut out);
                } else {
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
            Ok(Packet::HandshakeResp { .. }) | Ok(Packet::VersionReject { .. }) | Err(_) => {}
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

        // Timeout scan: keep state so the same token can rejoin, free the
        // address binding and transport.
        let timed_out: Vec<MemberId> = self
            .members
            .iter()
            .filter(|(_, m)| {
                m.connected && now_ms.saturating_sub(m.last_heard_ms) >= self.cfg.member_timeout_ms
            })
            .map(|(&id, _)| id)
            .collect();
        if !timed_out.is_empty() {
            for id in timed_out {
                self.disconnect_member(id);
                self.events.push(ServerEvent::MemberDisconnected { id });
            }
            self.queue_roster();
            self.note_musician_count();
        }

        out
    }

    /// Tells every connected member the session is over and returns the
    /// datagrams to send. One flight each, no retransmit: the process is going
    /// away, and a client that misses this finds out by timeout, which is what
    /// used to happen to everyone. Members are marked disconnected, so a
    /// caller that keeps running (the harness) sees a clean roster.
    pub fn shutdown(&mut self, now_ms: u64, reason: &str) -> Outgoing {
        let mut out = Vec::new();
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

    /// The last tick's broadcast audio and card state, for the stream
    /// pipeline. Call it right after [`ServerCore::tick`]: the audio slice is
    /// the accumulator slot that tick wrote.
    pub fn broadcast_tick(&self) -> BroadcastTick<'_> {
        let start = self.bcast_slot * MIX_LEN;
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
            audio: &self.bcast_accum[start..start + MIX_LEN],
            members,
            listeners: self
                .members
                .values()
                .filter(|m| m.connected && m.role == Role::Listener)
                .count(),
            roster_epoch: self.roster_epoch,
        }
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
        init_packet: &[u8],
        out: &mut Outgoing,
    ) {
        if init_packet.len() < REJECT_MIN_INIT_BYTES {
            return;
        }
        let slot = reject_slot(src.ip());
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
        self.reject_seen[slot] = Some(now_ms);
        out.push((
            src,
            wire::build_version_reject(
                &self.cfg.server_public,
                PROTOCOL_VERSION,
                theirs,
                init_packet,
            ),
        ));
    }

    /// Full admission path for a version-matched handshake init. Every
    /// refusal is silent: to an unauthenticated peer the server looks like
    /// packet loss.
    fn admit(
        &mut self,
        now_ms: u64,
        now_unix: u64,
        src: SocketAddr,
        noise: &[u8],
        out: &mut Outgoing,
    ) {
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
            ControlMsg::Roster(_) => self.violation(now_ms, from, "roster from client"),
            ControlMsg::Stats { .. } => self.violation(now_ms, from, "stats from client"),
            ControlMsg::StreamStatus { .. } => {
                self.violation(now_ms, from, "stream status from client")
            }
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
    /// on, so entries for hashes nobody announces any more are dead: they
    /// used to be dropped only when a train completed, which let a member
    /// alternating SetAvatar and AvatarRequest on its own hash leave one
    /// permanent entry per pair of packets.
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
    /// they run their budget out. Every violation site funnels through here;
    /// the counter used to be incremented in five places and read nowhere, so
    /// an admitted peer, a listener invite included, could send illegal
    /// packets at line rate forever.
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
fn reject_slot(ip: IpAddr) -> usize {
    let network = match ip {
        IpAddr::V4(v4) => u128::from(u32::from_be_bytes(v4.octets())),
        IpAddr::V6(v6) => u128::from_be_bytes(v6.octets()) & !((1u128 << 64) - 1),
    };
    let mut x = (network as u64) ^ ((network >> 64) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x as usize) & (REJECT_SLOTS - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamstream_protocol::invite::{Issuer, Token};
    use jamstream_protocol::transport::{Initiator, generate_keypair};

    fn addr(n: u8) -> SocketAddr {
        format!("10.0.0.{n}:5000").parse().unwrap()
    }

    /// A wrong-version init the size a real one would be: the Noise IK first
    /// message is over 90 bytes, and the server refuses to answer anything
    /// short enough to make the 21-byte reject an amplification.
    fn wrong_version_init() -> Vec<u8> {
        wire::build_handshake_init(9, &[0xAA; 96])
    }

    fn server_with_issuer() -> (ServerCore, Issuer, [u8; 32]) {
        let issuer = Issuer::generate();
        let kp = generate_keypair();
        let public = kp.public;
        let core = ServerCore::new(ServerConfig::new(
            SessionId([7u8; 16]),
            kp.private.to_vec(),
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
        let (mut core, _issuer, public) = server_with_issuer();
        let init = wrong_version_init();
        let out = core.handle_datagram(0, 0, addr(2), &init);
        assert_eq!(out.len(), 1);
        let Ok(Packet::VersionReject { ours, theirs, mac }) = wire::parse(&out[0].1) else {
            panic!("expected version reject");
        };
        assert_eq!((ours, theirs), (PROTOCOL_VERSION, 9));
        assert!(wire::verify_version_reject(
            &public, ours, theirs, &mac, &init
        ));
        // Within a second: silence. A different source still gets one.
        assert!(core.handle_datagram(500, 0, addr(2), &init).is_empty());
        assert_eq!(core.handle_datagram(500, 0, addr(3), &init).len(), 1);
        // After the interval the same source is answered again.
        assert_eq!(core.handle_datagram(1_500, 0, addr(2), &init).len(), 1);
    }

    /// A UDP source port is chosen by whoever sends the packet, so a limiter
    /// keyed on `ip:port` sees a fresh key every time and limits nothing. One
    /// host walking ports must draw one reject, not thousands.
    #[test]
    fn one_host_cannot_walk_source_ports_for_unlimited_rejects() {
        let (mut core, _issuer, _public) = server_with_issuer();
        let init = wrong_version_init();
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
        let (mut core, _issuer, _public) = server_with_issuer();
        let init = wrong_version_init();
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

    /// The reject is 21 bytes. Answering a 3-byte `[1, 9, 0]` would make the
    /// server an amplifier by size, which the threat model rules out.
    #[test]
    fn a_reject_is_never_larger_than_the_init_it_answers() {
        let (mut core, _issuer, _public) = server_with_issuer();
        for noise_len in [0usize, 1, 8, 44] {
            let short = wire::build_handshake_init(9, &vec![0xAA; noise_len]);
            assert!(
                core.handle_datagram(0, 0, addr(2), &short).is_empty(),
                "{}-byte init drew a reject",
                short.len()
            );
        }
        let init = wrong_version_init();
        let out = core.handle_datagram(0, 0, addr(2), &init);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].1.len() < init.len(),
            "reject {} bytes vs init {} bytes",
            out[0].1.len(),
            init.len()
        );
    }

    #[test]
    fn transport_for_unknown_member_is_dropped() {
        let (mut core, _issuer, _public) = server_with_issuer();
        let pkt = wire::build_transport(MemberId(9), 0, &[1, 2, 3, 4]);
        assert!(core.handle_datagram(0, 0, addr(4), &pkt).is_empty());
        assert!(core.tick(0).is_empty());
    }
}
