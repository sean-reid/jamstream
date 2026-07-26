//! Client-side session core for musicians and listeners. Sans-io: the
//! desktop app, headless CLI, and harness own the socket and clock, feed
//! datagrams and capture frames in, and pull playout audio and events out.

use std::collections::VecDeque;

use jamstream_engine::{
    Channels, CodecError, Decoder, Encoder, JitterBuffer, JitterStats, MediaPacket, Pull,
    RedundancyPolicy,
};
use jamstream_protocol::control::{ControlLink, ControlMsg, MemberInfo};
use jamstream_protocol::ids::{MemberId, Role, TokenId};
use jamstream_protocol::invite::Invite;
use jamstream_protocol::media::{FrameDuration, MediaFrame};
use jamstream_protocol::transport::{Initiator, Session, Welcome};
use jamstream_protocol::wire::{self, CHANNEL_CONTROL, CHANNEL_MEDIA, Packet};

use crate::SessionError;

const TICK_SAMPLES: u64 = 120;
const UPLINK_BITRATE: u32 = 128_000;
const CONNECTION_TIMEOUT_MS: u64 = 10_000;
const PING_INTERVAL_MS: u64 = 1_000;
const INIT_RESEND_MS: u64 = 1_000;
const LOSS_REPORT_INTERVAL_MS: u64 = 1_000;
/// Reports of clean link required before redundancy turns back off.
const REDUNDANCY_OFF_HOLD: u32 = 10;

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
    prev_payload: Option<Vec<u8>>,
    pkt_buf: Vec<u8>,
    frames_sent: u64,
    events: Vec<ClientEvent>,
    last_server_ms: u64,
    last_ping_ms: u64,
    last_init_ms: u64,
    last_loss_report_ms: u64,
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
            prev_payload: None,
            pkt_buf: Vec::new(),
            frames_sent: 0,
            events: Vec::new(),
            last_server_ms: now_ms,
            last_ping_ms: now_ms,
            last_init_ms: now_ms,
            last_loss_report_ms: now_ms,
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
        self.prev_payload = None;
        self.frames_sent = 0;
        self.last_server_ms = now_ms;
        self.last_init_ms = now_ms;
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
                        self.last_loss_report_ms = now_ms;
                        self.events.push(ClientEvent::Joined);
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

    /// Periodic housekeeping: handshake resends, keepalive pings, redundancy
    /// policy updates, control retransmits, and the connection timeout.
    pub fn poll(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        match self.state {
            ClientState::Connecting => {
                if now_ms.saturating_sub(self.last_server_ms) >= CONNECTION_TIMEOUT_MS {
                    self.state = ClientState::TimedOut;
                    self.events.push(ClientEvent::TimedOut);
                } else if now_ms.saturating_sub(self.last_init_ms) >= INIT_RESEND_MS {
                    self.last_init_ms = now_ms;
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
                if now_ms.saturating_sub(self.last_loss_report_ms) >= LOSS_REPORT_INTERVAL_MS {
                    self.last_loss_report_ms = now_ms;
                    // v1: downlink loss stands in for uplink loss. A Stats
                    // control message carrying the peer's report is the
                    // proper input once the protocol grows one.
                    self.redundancy.report(self.jitter.loss_ratio_recent());
                }
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
        }
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
            ControlMsg::Roster(members) => self.events.push(ClientEvent::Roster(members)),
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
            ControlMsg::Bye { reason } => {
                self.state = ClientState::Ejected {
                    reason: reason.clone(),
                };
                self.events.push(ClientEvent::Ejected { reason });
            }
            // The server never sends these; ignore.
            ControlMsg::MixerSet { .. }
            | ControlMsg::ClickEnable { .. }
            | ControlMsg::Revoke { .. } => {}
        }
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
}
