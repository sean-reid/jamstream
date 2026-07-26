//! Server-side session core: admission, per-member encrypted transport, the
//! 2.5 ms mix tick, and control-plane fanout. Sans-io: jamstreamd owns the
//! socket and the clock and calls in with datagrams and timestamps.

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;

use ed25519_dalek::VerifyingKey;
use jamstream_engine::{
    Channels, Decoder, Encoder, Fader, JitterBuffer, JitterStats, Limiter, MediaPacket, Metronome,
    Pull, mix_into,
};
use jamstream_protocol::PROTOCOL_VERSION;
use jamstream_protocol::control::{ControlLink, ControlMsg, MemberInfo};
use jamstream_protocol::ids::{HOST_MEMBER_ID, MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::verify_token;
use jamstream_protocol::media::{FrameDuration, MediaFrame};
use jamstream_protocol::transport::{Responder, Session, Welcome};
use jamstream_protocol::wire::{self, CHANNEL_CONTROL, CHANNEL_MEDIA, Packet};

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
/// At most one version reject per source address per this interval.
const REJECT_INTERVAL_MS: u64 = 1_000;
const REJECT_MAP_MAX: usize = 64;
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
    pub max_musicians: usize,
    pub max_listeners: usize,
    pub member_timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    MusicianCountChanged(usize),
    MemberJoined { id: MemberId, name: String },
    MemberDisconnected { id: MemberId },
    MemberRevoked { id: MemberId },
    ProtocolViolation { id: MemberId, what: &'static str },
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

struct Member {
    role: Role,
    name: String,
    jti: TokenId,
    addr: Option<SocketAddr>,
    session: Option<Session>,
    link: ControlLink,
    jitter: JitterBuffer,
    /// Mono uplink decoder; musicians only.
    decoder: Option<Decoder>,
    /// Personal mix (musician) or broadcast (listener) downlink encoder.
    encoder: Encoder,
    faders: BTreeMap<MemberId, Fader>,
    click_enabled: bool,
    connected: bool,
    last_heard_ms: u64,
    rtt_ms_last: Option<f32>,
    send_seq: u32,
    violations: u64,
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
    bcast_clock: u64,
    reject_last: BTreeMap<SocketAddr, u64>,
    events: Vec<ServerEvent>,
    last_musician_count: usize,
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
            bcast_clock: 0,
            reject_last: BTreeMap::new(),
            events: Vec::new(),
            last_musician_count: 0,
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
            self.decoded.push((id, pcm));
        }

        let mut click = [0.0f32; TICK_SAMPLES];
        if self.metronome_enabled {
            self.metronome.render(clock, &mut click, CLICK_GAIN);
        }

        // Personal stereo mixes, each excluding its own member and shaped by
        // that member's fader table.
        let sources: Vec<(MemberId, &[f32])> =
            self.decoded.iter().map(|(id, b)| (*id, &b[..])).collect();
        for (&id, m) in self.members.iter_mut() {
            if !m.connected || m.role != Role::Musician {
                continue;
            }
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
                if let (Some(s), Some(a)) = (m.session.as_mut(), m.addr)
                    && let Ok(dg) = s.seal(id, &frame)
                {
                    out.push((a, dg));
                }
            }
        }

        // Broadcast mix: everyone, default faders (BroadcastMixSet is post-v1),
        // through the limiter, accumulated into a 20 ms frame.
        mix_into(&sources, |_| Fader::default(), None, &mut self.mix_buf);
        self.limiter.process(&mut self.mix_buf);
        let idx = (self.tick_count % BCAST_TICKS) as usize;
        if idx == 0 {
            self.bcast_clock = clock;
        }
        self.bcast_accum[idx * MIX_LEN..(idx + 1) * MIX_LEN].copy_from_slice(&self.mix_buf);
        if idx as u64 == BCAST_TICKS - 1 {
            for (&id, m) in self.members.iter_mut() {
                if !m.connected || m.role != Role::Listener {
                    continue;
                }
                if m.encoder
                    .encode(&self.bcast_accum, &mut self.pkt_buf)
                    .is_ok()
                {
                    let frame = MediaFrame {
                        seq: m.send_seq,
                        timestamp: self.bcast_clock,
                        duration: FrameDuration::Ms20,
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
        }
        self.tick_count += 1;

        // Control-plane retransmits and acks.
        for (&id, m) in self.members.iter_mut() {
            if !m.connected {
                continue;
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
        let mut timed_out = Vec::new();
        for (&id, m) in self.members.iter_mut() {
            if m.connected && now_ms.saturating_sub(m.last_heard_ms) >= self.cfg.member_timeout_ms {
                m.connected = false;
                m.addr = None;
                m.session = None;
                timed_out.push(id);
            }
        }
        if !timed_out.is_empty() {
            for id in timed_out {
                self.events.push(ServerEvent::MemberDisconnected { id });
            }
            self.queue_roster();
            self.note_musician_count();
        }

        out
    }

    /// Drains accumulated events.
    pub fn events(&mut self) -> Vec<ServerEvent> {
        std::mem::take(&mut self.events)
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

    fn version_reject(
        &mut self,
        now_ms: u64,
        src: SocketAddr,
        theirs: u16,
        init_packet: &[u8],
        out: &mut Outgoing,
    ) {
        let recent = self
            .reject_last
            .get(&src)
            .is_some_and(|&t| now_ms.saturating_sub(t) < REJECT_INTERVAL_MS);
        if recent {
            return;
        }
        self.reject_last.insert(src, now_ms);
        if self.reject_last.len() > REJECT_MAP_MAX {
            self.reject_last
                .retain(|_, t| now_ms.saturating_sub(*t) < REJECT_INTERVAL_MS);
        }
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
        if self.members.get(&id).is_some_and(|m| m.connected) {
            return;
        }
        let connected_in_role = self
            .members
            .values()
            .filter(|m| m.connected && m.role == token.role)
            .count();
        let cap = match token.role {
            Role::Musician => self.cfg.max_musicians,
            Role::Listener => self.cfg.max_listeners,
        };
        if connected_in_role >= cap {
            tracing::debug!(member = id.0, "admission refused: role at capacity");
            return;
        }
        let media = match token.role {
            Role::Musician => {
                Encoder::new(Channels::Stereo, FrameDuration::Ms2_5, PERSONAL_MIX_BITRATE).and_then(
                    |e| Decoder::new(Channels::Mono, FrameDuration::Ms2_5).map(|d| (e, Some(d))),
                )
            }
            Role::Listener => {
                Encoder::new(Channels::Stereo, FrameDuration::Ms20, BROADCAST_BITRATE)
                    .map(|e| (e, None))
            }
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

        let name = token
            .name_hint
            .clone()
            .unwrap_or_else(|| format!("member {}", id.0));
        // A rejoin keeps the member's mixer state; everything stream-scoped
        // starts fresh with the new transport.
        let prev = self.members.remove(&id);
        let (faders, click_enabled) =
            prev.map_or((BTreeMap::new(), true), |p| (p.faders, p.click_enabled));
        self.members.insert(
            id,
            Member {
                role: token.role,
                name: name.clone(),
                jti: token.jti,
                addr: Some(src),
                session: Some(session),
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
                violations: 0,
            },
        );
        out.push((src, resp));
        self.events.push(ServerEvent::MemberJoined { id, name });
        self.queue_roster();
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
                        m.violations += 1;
                        self.events.push(ServerEvent::ProtocolViolation {
                            id: member,
                            what: "media from listener",
                        });
                        return;
                    }
                    match MediaFrame::decode(&plain) {
                        Ok(f) => m.jitter.push(MediaPacket {
                            seq: f.seq,
                            timestamp: f.timestamp,
                            payload: f.payload.to_vec(),
                            redundant: f.redundant.map(<[u8]>::to_vec),
                        }),
                        Err(_) => {
                            m.violations += 1;
                            self.events.push(ServerEvent::ProtocolViolation {
                                id: member,
                                what: "malformed media frame",
                            });
                        }
                    }
                    return;
                }
                Ok((CHANNEL_CONTROL, _)) => match m.link.receive(&plain) {
                    Ok(msgs) => msgs,
                    Err(_) => {
                        m.violations += 1;
                        self.events.push(ServerEvent::ProtocolViolation {
                            id: member,
                            what: "malformed control packet",
                        });
                        return;
                    }
                },
                _ => {
                    m.violations += 1;
                    self.events.push(ServerEvent::ProtocolViolation {
                        id: member,
                        what: "unknown channel",
                    });
                    return;
                }
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
                    self.violation(from, "non-finite fader");
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
                    self.violation(from, "metronome set by non-host");
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
                    self.violation(from, "revoke by non-host");
                    return;
                }
                self.revoked.insert(jti);
                let target = self
                    .members
                    .iter()
                    .find(|(_, m)| m.jti == jti)
                    .map(|(&id, _)| id);
                if let Some(id) = target {
                    if let Some(m) = self.members.get_mut(&id)
                        && m.connected
                    {
                        // Best effort Bye: one flight, the link dies with
                        // the member. Silence also ejects via timeout.
                        let _ = m.link.send(ControlMsg::Bye {
                            reason: "invite revoked".into(),
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
                    self.members.remove(&id);
                    self.events.push(ServerEvent::MemberRevoked { id });
                    self.queue_roster();
                    self.note_musician_count();
                }
            }
            ControlMsg::Bye { .. } => {
                if let Some(m) = self.members.get_mut(&from)
                    && m.connected
                {
                    m.connected = false;
                    m.addr = None;
                    m.session = None;
                    self.events
                        .push(ServerEvent::MemberDisconnected { id: from });
                    self.queue_roster();
                    self.note_musician_count();
                }
            }
            ControlMsg::Roster(_) => self.violation(from, "roster from client"),
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

    fn queue_roster(&mut self) {
        let roster: Vec<MemberInfo> = self
            .members
            .iter()
            .map(|(&id, m)| MemberInfo {
                id,
                role: m.role,
                name: m.name.clone(),
                connected: m.connected,
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

    fn violation(&mut self, id: MemberId, what: &'static str) {
        if let Some(m) = self.members.get_mut(&id) {
            m.violations += 1;
        }
        self.events
            .push(ServerEvent::ProtocolViolation { id, what });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamstream_protocol::invite::{Issuer, Token};
    use jamstream_protocol::transport::{Initiator, generate_keypair};

    fn addr(n: u8) -> SocketAddr {
        format!("10.0.0.{n}:5000").parse().unwrap()
    }

    fn server_with_issuer() -> (ServerCore, Issuer, [u8; 32]) {
        let issuer = Issuer::generate();
        let kp = generate_keypair();
        let public = kp.public;
        let core = ServerCore::new(ServerConfig {
            session_id: SessionId([7u8; 16]),
            server_private: kp.private.to_vec(),
            server_public: public,
            issuer_pk: issuer.public_key(),
            max_musicians: 10,
            max_listeners: 20,
            member_timeout_ms: 10_000,
        });
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
        let init = wire::build_handshake_init(9, &[0xAA; 40]);
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

    #[test]
    fn transport_for_unknown_member_is_dropped() {
        let (mut core, _issuer, _public) = server_with_issuer();
        let pkt = wire::build_transport(MemberId(9), 0, &[1, 2, 3, 4]);
        assert!(core.handle_datagram(0, 0, addr(4), &pkt).is_empty());
        assert!(core.tick(0).is_empty());
    }
}
