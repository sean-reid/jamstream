//! The control plane: everything that is not audio, carried over the same
//! socket with a small reliability layer. Sequence numbers per link,
//! cumulative ack plus a 32-bit selective-ack bitmap, retransmit on timeout
//! with exponential backoff. The module is time-free: callers pass
//! milliseconds in, which keeps it deterministic under the harness.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use zeroize::Zeroize;

use crate::Error;
use crate::ids::{DestinationId, MemberId, Role, TokenId};
use crate::wire::CHANNEL_CONTROL;

pub const MAX_CHAT_LEN: usize = 1_000;
pub const MAX_NAME_LEN: usize = 64;
/// Longest accepted stream key. Twitch and YouTube keys are well under 100
/// characters; the cap only stops a host from stuffing the control plane.
pub const MAX_STREAM_KEY_LEN: usize = 256;
/// Longest accepted failure reason in a [`DestinationStatus`].
pub const MAX_STREAM_REASON_LEN: usize = 200;
/// Avatars are capped at 256 KB and identified by the Blake2s-256 of their
/// bytes; the hash is the cache key on both ends.
pub const MAX_AVATAR_BYTES: usize = 256 * 1024;
/// Payload bytes per `AvatarChunk`, sized so a sealed chunk datagram never
/// fragments: a 1500-byte ethernet MTU less 20 (IPv4) and 8 (UDP) leaves
/// 1472, of which our transport header takes 11, the AEAD tag 16, and the
/// channel byte plus postcard framing for the variant, 32-byte hash, two
/// indices, and the length prefix roughly 45. 1024 of payload leaves over
/// 300 bytes of headroom, which also covers IPv6 and light tunneling.
/// Fragmented UDP survives loopback and most LANs but is routinely
/// dropped across the internet, which is exactly where avatars have to
/// work. 256 chunks cover the largest avatar.
pub const AVATAR_CHUNK_BYTES: usize = 1024;
/// Every socket read buffer in the workspace is this size, so no legal
/// datagram is ever truncated. Nothing legitimate approaches it now that
/// chunks fit the MTU; the margin exists because the failure is silent
/// rather than loud: `recv` truncates and the receiver drops the datagram
/// as malformed, so an oversized message simply never arrives.
pub const MAX_DATAGRAM_BYTES: usize = 2 * 1024;

const RTO_INITIAL_MS: u64 = 100;
const RTO_MAX_MS: u64 = 2_000;
const MAX_SENDS: u32 = 20;

/// Sequence numbers past the cumulative ack that a receiver will hold while
/// waiting for the gap to fill. `ack_bits` advertises exactly 32 entries, so
/// anything further out cannot be selectively acked and gets retransmitted
/// whether or not it was buffered: holding it buys nothing and, before this
/// bound existed, cost about 2 KB of permanently live heap per packet from
/// any peer that simply never sent `recv_next`.
pub const RECV_WINDOW: u64 = 32;

/// Messages one link will hold queued or unacknowledged before refusing more.
/// The avatar pacer feeds two chunks per 2.5 ms tick and the queue drains on
/// ack, so the widest legitimate backlog is a round trip's worth: about 36 on
/// a 45 ms path. 128 is over three times that, and at a kilobyte per avatar
/// chunk it caps one link's queue at roughly 128 KB.
pub const MAX_PENDING: usize = 128;

/// Where a broadcast goes. V1 ships the two landscape platforms with
/// persistent, ungated keys; the requirements behind each one (ingest URL,
/// aspect, keyframe cadence) live as data in jamstream-stream, not here, so
/// the wire type stays a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamPlatform {
    Twitch,
    YouTube,
}

impl StreamPlatform {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamPlatform::Twitch => "twitch",
            StreamPlatform::YouTube => "youtube",
        }
    }
}

/// A platform stream key. Its own type so the secret cannot be printed by
/// accident: `Debug` redacts, and the bytes are wiped on drop. It is still
/// an ordinary `String` on the wire (inside the Noise transport), and the
/// server keeps it in memory only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamKey(String);

impl StreamKey {
    pub fn new(key: impl Into<String>) -> Self {
        StreamKey(key.into())
    }

    /// The secret itself. Every call site is a place to audit.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for StreamKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StreamKey(<redacted {} bytes>)", self.0.len())
    }
}

impl Drop for StreamKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// What the host asks the broadcast pipeline to do. Destinations are named
/// by host-minted ids so add and remove need no round trip, and adding or
/// removing one mid-stream disturbs no other destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamOp {
    AddDestination {
        id: DestinationId,
        platform: StreamPlatform,
        key: StreamKey,
    },
    RemoveDestination {
        id: DestinationId,
    },
    /// Bring the encoder up. Destinations added before or after both apply.
    Start,
    /// Tear the encoder and every pusher down.
    Stop,
}

/// Per-destination lifecycle. `Failed` carries a reason a musician can act
/// on ("pusher exited: connection refused"), never a stream key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DestinationState {
    /// Configured, encoder not running.
    Idle,
    /// Pusher spawned, not yet observed healthy.
    Connecting,
    Live,
    Failed {
        reason: String,
    },
}

/// One destination as the server sees it. Deliberately key-free: this goes
/// to every member, not just the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationStatus {
    pub id: DestinationId,
    pub platform: StreamPlatform,
    pub state: DestinationState,
    /// Configured video plus audio bitrate for this destination; the copy
    /// pushers all carry the one encode, so it is the same for each.
    pub bitrate_kbps: u32,
    /// Frames the pipeline could not hand the encoder in time, cumulative.
    pub dropped_frames: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub id: MemberId,
    pub role: Role,
    pub name: String,
    pub connected: bool,
    /// Blake2s-256 of the member's avatar bytes; None when unset. Trailing
    /// field, but postcard encodes struct fields in order with no framing,
    /// so old roster bytes lack the Option tag byte and fail to decode
    /// (UnexpectedEnd) rather than misreading. That makes this a breaking
    /// change to the roster encoding, accepted deliberately while protocol
    /// version 1 is unreleased; no compat fixtures reference roster bytes.
    pub avatar_hash: Option<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMsg {
    /// Full roster snapshot; the server sends one on every change.
    Roster(Vec<MemberInfo>),
    Chat {
        from: MemberId,
        text: String,
    },
    /// A member shaping their own monitor mix.
    MixerSet {
        target: MemberId,
        gain_db: f32,
        pan: f32,
        muted: bool,
    },
    /// Host-controlled metronome; the click is mixed in server-side.
    MetronomeSet {
        bpm: u16,
        beats_per_bar: u8,
        enabled: bool,
    },
    /// A member enabling or disabling the click in their own mix.
    ClickEnable {
        enabled: bool,
    },
    Ping {
        nonce: u32,
        sent_ms: u64,
    },
    Pong {
        nonce: u32,
        sent_ms: u64,
    },
    /// Host invalidates one invite; the server drops the matching member.
    Revoke {
        jti: TokenId,
    },
    Bye {
        reason: String,
    },
    /// Server's once-per-second view of a musician's uplink, feeding the
    /// sender's redundancy decision. Percentages are 0..=100 over the last
    /// reporting window.
    ///
    /// Appended last on purpose: postcard encodes the variant index as a
    /// varint discriminant, so adding a variant at the end leaves every
    /// existing variant's bytes unchanged. A peer without this variant only
    /// fails to decode messages that actually carry it; protocol version 1
    /// is unreleased, so no such peer exists.
    Stats {
        uplink_loss_pct: f32,
        uplink_jitter_depth: u16,
        uplink_recovered_pct: f32,
    },
    /// Host shapes one member's fader in the broadcast mix (host to server);
    /// the server relays accepted changes to every connected member so UIs
    /// can mirror the state.
    ///
    /// Appended after Stats, same postcard append-safety rule: trailing
    /// variants leave every existing variant's bytes unchanged.
    BroadcastMixSet {
        target: MemberId,
        gain_db: f32,
        pan: f32,
        muted: bool,
    },
    /// Host to server: while enabled, the host hears the broadcast mix
    /// instead of their personal mix. Trailing variant, as above.
    BroadcastAudition {
        enabled: bool,
    },
    /// Client to server: announce or replace this member's avatar by
    /// content hash. The bytes follow only if the server asks for them.
    /// Trailing variant, same postcard append-safety rule as Stats.
    SetAvatar {
        hash: [u8; 32],
        len: u32,
    },
    /// One slice of avatar bytes, either direction. The control link is
    /// ordered and reliable, so a train arrives as index 0..total in order;
    /// every chunk except the last carries exactly `AVATAR_CHUNK_BYTES`.
    AvatarChunk {
        hash: [u8; 32],
        index: u16,
        total: u16,
        data: Vec<u8>,
    },
    /// Ask the other side for avatar bytes: the server asks the owning
    /// client when it lacks them, a client asks the server when a roster
    /// entry carries an unknown hash.
    AvatarRequest {
        hash: [u8; 32],
    },
    /// Host to server: drive the broadcast pipeline. Host-only; the server
    /// counts a violation against any other sender. The key inside rides
    /// the Noise transport, never touches disk outside the pusher's
    /// root-only spawn file, and never appears in any relayed message.
    /// Trailing variant, same postcard append-safety rule as Stats.
    StreamCtl {
        op: StreamOp,
    },
    /// Server to all: the on-air state, once a second while streaming and
    /// immediately on any transition. Every member sees it, not just the
    /// host, because everyone in the room deserves to know they are live.
    /// Trailing variant, as above.
    StreamStatus {
        destinations: Vec<DestinationStatus>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct CtlPacket {
    /// Next sequence number the sender expects: everything below it has
    /// been received.
    ack: u64,
    /// Bit i set means `ack + 1 + i` was received out of order.
    ack_bits: u32,
    frame: Option<(u64, ControlMsg)>,
}

#[derive(Debug)]
struct Pending {
    seq: u64,
    msg: ControlMsg,
    next_send_ms: u64,
    sends: u32,
}

/// One reliable, ordered control link. Each side of a connection owns one.
#[derive(Debug, Default)]
pub struct ControlLink {
    next_seq: u64,
    pending: VecDeque<Pending>,
    recv_next: u64,
    out_of_order: BTreeMap<u64, ControlMsg>,
    need_ack: bool,
    dead: bool,
}

impl ControlLink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a message for reliable delivery. It goes on the wire at the
    /// next `poll`.
    pub fn send(&mut self, msg: ControlMsg) -> Result<(), Error> {
        check_lengths(&msg)?;
        if self.pending.len() >= MAX_PENDING {
            return Err(Error::LinkFull);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.push_back(Pending {
            seq,
            msg,
            next_send_ms: 0,
            sends: 0,
        });
        Ok(())
    }

    /// Returns the plaintext datagrams due now: fresh sends, retransmits,
    /// and a bare ack when one is owed and nothing else is going out.
    /// Callers seal each one into its own transport packet.
    pub fn poll(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let ack = self.recv_next;
        let ack_bits = self.ack_bits();
        for p in self.pending.iter_mut() {
            if now_ms < p.next_send_ms {
                continue;
            }
            if p.sends >= MAX_SENDS {
                self.dead = true;
                continue;
            }
            let backoff = RTO_INITIAL_MS << p.sends.min(6);
            p.next_send_ms = now_ms + backoff.min(RTO_MAX_MS);
            p.sends += 1;
            out.push(encode(&CtlPacket {
                ack,
                ack_bits,
                frame: Some((p.seq, p.msg.clone())),
            }));
            self.need_ack = false;
        }
        if self.need_ack && out.is_empty() {
            out.push(encode(&CtlPacket {
                ack,
                ack_bits,
                frame: None,
            }));
            self.need_ack = false;
        }
        out
    }

    /// Ingests one received control datagram (channel byte included) and
    /// returns any messages now deliverable in order.
    pub fn receive(&mut self, buf: &[u8]) -> Result<Vec<ControlMsg>, Error> {
        if buf.first() != Some(&CHANNEL_CONTROL) {
            return Err(Error::Malformed);
        }
        let pkt: CtlPacket = postcard::from_bytes(&buf[1..])?;

        // Their ack state clears our pending queue: everything below the
        // cumulative ack, plus whatever the selective bitmap covers.
        self.pending.retain(|p| {
            if p.seq < pkt.ack {
                return false;
            }
            if p.seq > pkt.ack && p.seq - pkt.ack <= 32 {
                let bit = (p.seq - pkt.ack - 1) as u32;
                if (pkt.ack_bits >> bit) & 1 == 1 {
                    return false;
                }
            }
            true
        });

        let mut delivered = Vec::new();
        if let Some((seq, msg)) = pkt.frame {
            self.need_ack = true;
            // The same length rules `send` applies. Enforcing them only on
            // the sending side left the receiver willing to buffer anything
            // that fit in a datagram.
            check_lengths(&msg)?;
            // Beyond the window the frame is unusable rather than malformed,
            // which is what an honest peer produces when the frame that
            // would open the window was lost, so it is dropped as quietly as
            // a duplicate and the ack still goes back.
            let in_window = seq >= self.recv_next && seq < self.recv_next + RECV_WINDOW;
            if in_window && !self.out_of_order.contains_key(&seq) {
                self.out_of_order.insert(seq, msg);
                while let Some(next) = self.out_of_order.remove(&self.recv_next) {
                    delivered.push(next);
                    self.recv_next += 1;
                }
            }
        }
        Ok(delivered)
    }

    /// True once a message has been retransmitted past the give-up limit;
    /// the peer is unreachable and the caller should drop the connection.
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// Anything still awaiting acknowledgment?
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Messages queued for delivery or awaiting acknowledgment.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Frames held out of order, waiting for the gap ahead of them to fill.
    /// Bounded by [`RECV_WINDOW`].
    pub fn buffered(&self) -> usize {
        self.out_of_order.len()
    }

    fn ack_bits(&self) -> u32 {
        let mut bits = 0u32;
        for i in 0..32u64 {
            if self.out_of_order.contains_key(&(self.recv_next + 1 + i)) {
                bits |= 1 << i;
            }
        }
        bits
    }
}

/// Every variable-length field a control message carries, against its cap.
/// Both directions run it: a sender must not build an illegal message, and a
/// receiver must not hold one.
fn check_lengths(msg: &ControlMsg) -> Result<(), Error> {
    match msg {
        ControlMsg::Chat { text, .. } if text.len() > MAX_CHAT_LEN => Err(Error::Malformed),
        ControlMsg::Bye { reason } if reason.len() > MAX_CHAT_LEN => Err(Error::Malformed),
        ControlMsg::AvatarChunk { data, .. } if data.len() > AVATAR_CHUNK_BYTES => {
            Err(Error::Malformed)
        }
        ControlMsg::StreamCtl {
            op: StreamOp::AddDestination { key, .. },
        } if key.is_empty() || key.len() > MAX_STREAM_KEY_LEN => Err(Error::Malformed),
        ControlMsg::StreamStatus { destinations } => {
            let bad_reason = destinations.iter().any(|d| match &d.state {
                DestinationState::Failed { reason } => reason.len() > MAX_STREAM_REASON_LEN,
                _ => false,
            });
            if bad_reason {
                return Err(Error::Malformed);
            }
            Ok(())
        }
        ControlMsg::Roster(members) if members.iter().any(|m| m.name.len() > MAX_NAME_LEN) => {
            Err(Error::Malformed)
        }
        _ => Ok(()),
    }
}

fn encode(pkt: &CtlPacket) -> Vec<u8> {
    // Serialize straight into the datagram; no intermediate Vec.
    postcard::to_extend(pkt, vec![CHANNEL_CONTROL]).expect("control serialize")
}

#[cfg(test)]
mod tests {
    /// The reason AVATAR_CHUNK_BYTES is what it is: a sealed chunk must fit
    /// one unfragmented datagram. This pins the property rather than the
    /// constant, so any change to chunk size, hashing, or framing that
    /// would reintroduce ip fragmentation fails here.
    #[test]
    fn a_sealed_avatar_chunk_never_fragments() {
        use crate::ids::MemberId;
        let msg = super::ControlMsg::AvatarChunk {
            hash: [0xAB; 32],
            index: u16::MAX,
            total: u16::MAX,
            data: vec![0xCD; super::AVATAR_CHUNK_BYTES],
        };
        let mut link = super::ControlLink::new();
        link.send(msg)
            .expect("largest legal chunk must be sendable");
        for plain in link.poll(0) {
            // The transport adds an 11-byte header and a 16-byte aead tag.
            let sealed = plain.len() + 11 + 16;
            assert!(
                sealed < 1200,
                "sealed avatar chunk is {sealed} bytes, which fragments on a 1500 byte mtu"
            );
            let _ = MemberId(0);
        }
    }

    use super::*;

    fn chat(n: u64) -> ControlMsg {
        ControlMsg::Chat {
            from: MemberId(1),
            text: format!("message {n}"),
        }
    }

    /// Shuttles every due datagram from `a` to `b`, dropping the ones whose
    /// index appears in `drop`. Returns delivered messages.
    fn shuttle(
        a: &mut ControlLink,
        b: &mut ControlLink,
        now: u64,
        drop: &[usize],
    ) -> Vec<ControlMsg> {
        let mut delivered = Vec::new();
        for (i, dgram) in a.poll(now).into_iter().enumerate() {
            if drop.contains(&i) {
                continue;
            }
            delivered.extend(b.receive(&dgram).unwrap());
        }
        delivered
    }

    #[test]
    fn delivers_in_order() {
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        for n in 0..5 {
            a.send(chat(n)).unwrap();
        }
        let got = shuttle(&mut a, &mut b, 0, &[]);
        assert_eq!(got, (0..5).map(chat).collect::<Vec<_>>());
        // Acks flow back and clear the pending queue.
        shuttle(&mut b, &mut a, 1, &[]);
        assert!(!a.has_pending());
    }

    #[test]
    fn retransmits_after_loss() {
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        for n in 0..3 {
            a.send(chat(n)).unwrap();
        }
        // First transmission: middle datagram lost.
        let got = shuttle(&mut a, &mut b, 0, &[1]);
        assert_eq!(got, vec![chat(0)]);
        // Ack round trip tells `a` what survived.
        shuttle(&mut b, &mut a, 10, &[]);
        // After the RTO, only the lost message goes again.
        let resent = a.poll(200);
        assert_eq!(resent.len(), 1);
        let got: Vec<_> = resent
            .into_iter()
            .flat_map(|d| b.receive(&d).unwrap())
            .collect();
        assert_eq!(got, vec![chat(1), chat(2)]);
    }

    #[test]
    fn duplicates_deliver_once() {
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        a.send(chat(0)).unwrap();
        let dgrams = a.poll(0);
        assert_eq!(b.receive(&dgrams[0]).unwrap(), vec![chat(0)]);
        assert_eq!(b.receive(&dgrams[0]).unwrap(), vec![]);
    }

    #[test]
    fn bare_ack_when_nothing_to_say() {
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        a.send(chat(0)).unwrap();
        shuttle(&mut a, &mut b, 0, &[]);
        // `b` owes an ack and has nothing queued: exactly one bare ack.
        let acks = b.poll(1);
        assert_eq!(acks.len(), 1);
        a.receive(&acks[0]).unwrap();
        assert!(!a.has_pending());
        // Nothing further owed.
        assert!(b.poll(2).is_empty());
    }

    #[test]
    fn link_dies_after_giving_up() {
        let mut a = ControlLink::new();
        a.send(chat(0)).unwrap();
        let mut now = 0;
        for _ in 0..MAX_SENDS + 1 {
            a.poll(now);
            now += RTO_MAX_MS + 1;
        }
        assert!(a.is_dead());
    }

    #[test]
    fn rejects_oversized_chat() {
        let mut a = ControlLink::new();
        let big = ControlMsg::Chat {
            from: MemberId(1),
            text: "x".repeat(MAX_CHAT_LEN + 1),
        };
        assert!(a.send(big).is_err());
    }

    #[test]
    fn broadcast_messages_round_trip() {
        let msgs = [
            ControlMsg::BroadcastMixSet {
                target: MemberId(7),
                gain_db: -6.5,
                pan: 0.25,
                muted: true,
            },
            ControlMsg::BroadcastAudition { enabled: true },
        ];
        // Bare postcard round trip of the payload encoding.
        for m in &msgs {
            let bytes = postcard::to_allocvec(m).unwrap();
            let back: ControlMsg = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(&back, m);
        }
        // And through the reliable link.
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        for m in &msgs {
            a.send(m.clone()).unwrap();
        }
        assert_eq!(shuttle(&mut a, &mut b, 0, &[]), msgs);
    }

    #[test]
    fn avatar_messages_round_trip() {
        let msgs = [
            ControlMsg::SetAvatar {
                hash: [7u8; 32],
                len: 1_234,
            },
            ControlMsg::AvatarChunk {
                hash: [7u8; 32],
                index: 3,
                total: 5,
                data: vec![0xAB; AVATAR_CHUNK_BYTES],
            },
            ControlMsg::AvatarRequest { hash: [7u8; 32] },
        ];
        for m in &msgs {
            let bytes = postcard::to_allocvec(m).unwrap();
            let back: ControlMsg = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(&back, m);
        }
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        for m in &msgs {
            a.send(m.clone()).unwrap();
        }
        assert_eq!(shuttle(&mut a, &mut b, 0, &[]), msgs);
    }

    #[test]
    fn stream_messages_round_trip() {
        let msgs = [
            ControlMsg::StreamCtl {
                op: StreamOp::AddDestination {
                    id: DestinationId(1),
                    platform: StreamPlatform::Twitch,
                    key: StreamKey::new("live_1234_secret"),
                },
            },
            ControlMsg::StreamCtl {
                op: StreamOp::RemoveDestination {
                    id: DestinationId(1),
                },
            },
            ControlMsg::StreamCtl {
                op: StreamOp::Start,
            },
            ControlMsg::StreamCtl { op: StreamOp::Stop },
            ControlMsg::StreamStatus {
                destinations: vec![
                    DestinationStatus {
                        id: DestinationId(1),
                        platform: StreamPlatform::Twitch,
                        state: DestinationState::Live,
                        bitrate_kbps: 2_628,
                        dropped_frames: 0,
                    },
                    DestinationStatus {
                        id: DestinationId(2),
                        platform: StreamPlatform::YouTube,
                        state: DestinationState::Failed {
                            reason: "pusher exited: connection refused".into(),
                        },
                        bitrate_kbps: 2_628,
                        dropped_frames: 3,
                    },
                ],
            },
        ];
        for m in &msgs {
            let bytes = postcard::to_allocvec(m).unwrap();
            let back: ControlMsg = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(&back, m);
        }
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        for m in &msgs {
            a.send(m.clone()).unwrap();
        }
        assert_eq!(shuttle(&mut a, &mut b, 0, &[]), msgs);
    }

    /// The one property that matters more than the encoding: a stream key
    /// cannot leak through a log line.
    #[test]
    fn stream_key_debug_is_redacted() {
        let msg = ControlMsg::StreamCtl {
            op: StreamOp::AddDestination {
                id: DestinationId(4),
                platform: StreamPlatform::YouTube,
                key: StreamKey::new("super-secret-key"),
            },
        };
        let printed = format!("{msg:?}");
        assert!(!printed.contains("super-secret-key"), "{printed}");
        assert!(printed.contains("redacted"), "{printed}");
        // The value is still reachable where it is actually needed.
        let ControlMsg::StreamCtl {
            op: StreamOp::AddDestination { key, .. },
        } = &msg
        else {
            unreachable!()
        };
        assert_eq!(key.expose(), "super-secret-key");
    }

    #[test]
    fn rejects_empty_and_oversized_stream_keys() {
        let mut a = ControlLink::new();
        for key in [
            StreamKey::new(""),
            StreamKey::new("k".repeat(MAX_STREAM_KEY_LEN + 1)),
        ] {
            assert!(
                a.send(ControlMsg::StreamCtl {
                    op: StreamOp::AddDestination {
                        id: DestinationId(1),
                        platform: StreamPlatform::Twitch,
                        key,
                    },
                })
                .is_err()
            );
        }
        assert!(
            a.send(ControlMsg::StreamStatus {
                destinations: vec![DestinationStatus {
                    id: DestinationId(1),
                    platform: StreamPlatform::Twitch,
                    state: DestinationState::Failed {
                        reason: "x".repeat(MAX_STREAM_REASON_LEN + 1),
                    },
                    bitrate_kbps: 0,
                    dropped_frames: 0,
                }],
            })
            .is_err()
        );
    }

    /// Trailing variants must leave every earlier variant's bytes alone:
    /// postcard writes the variant index as a varint, so appending only
    /// widens the tag space.
    #[test]
    fn appending_stream_variants_left_earlier_encodings_alone() {
        let earlier = ControlMsg::Chat {
            from: MemberId(1),
            text: "hi".into(),
        };
        // Chat is variant index 1: tag byte 1, then the payload.
        let bytes = postcard::to_allocvec(&earlier).unwrap();
        assert_eq!(bytes[0], 1);
        // The new variants land last, after AvatarRequest.
        let ctl = postcard::to_allocvec(&ControlMsg::StreamCtl {
            op: StreamOp::Start,
        })
        .unwrap();
        assert_eq!(ctl[0], 15);
        let status = postcard::to_allocvec(&ControlMsg::StreamStatus {
            destinations: Vec::new(),
        })
        .unwrap();
        assert_eq!(status[0], 16);
    }

    #[test]
    fn rejects_oversized_avatar_chunk() {
        let mut a = ControlLink::new();
        let big = ControlMsg::AvatarChunk {
            hash: [0u8; 32],
            index: 0,
            total: 1,
            data: vec![0; AVATAR_CHUNK_BYTES + 1],
        };
        assert!(a.send(big).is_err());
    }

    /// Pins what postcard actually does with the trailing Option added to
    /// MemberInfo: old bytes are one byte short of the Option tag and fail
    /// to decode instead of misreading. Breaking pre-release, by decision.
    #[test]
    fn member_info_trailing_option_changed_the_roster_encoding() {
        #[derive(Serialize)]
        struct OldMemberInfo {
            id: MemberId,
            role: Role,
            name: String,
            connected: bool,
        }
        let old = postcard::to_allocvec(&OldMemberInfo {
            id: MemberId(3),
            role: Role::Musician,
            name: "ana".into(),
            connected: true,
        })
        .unwrap();
        assert!(postcard::from_bytes::<MemberInfo>(&old).is_err());

        let unset = MemberInfo {
            id: MemberId(3),
            role: Role::Musician,
            name: "ana".into(),
            connected: true,
            avatar_hash: None,
        };
        let bytes = postcard::to_allocvec(&unset).unwrap();
        // None costs exactly the one tag byte the old encoding lacked.
        assert_eq!(bytes.len(), old.len() + 1);
        assert_eq!(postcard::from_bytes::<MemberInfo>(&bytes).unwrap(), unset);

        let set = MemberInfo {
            avatar_hash: Some([9u8; 32]),
            ..unset
        };
        let bytes = postcard::to_allocvec(&set).unwrap();
        assert_eq!(postcard::from_bytes::<MemberInfo>(&bytes).unwrap(), set);
    }

    /// The reassembly buffer used to grow without limit for any peer that
    /// simply never sent the sequence number the receiver was waiting for.
    /// Each frame can carry a 1 KB avatar chunk, so 2,000 packets, one home
    /// connection's worth at 2,000 pps, pinned megabytes per second.
    #[test]
    fn out_of_order_growth_is_bounded_by_the_window() {
        let mut b = ControlLink::new();
        for seq in 1..2_001u64 {
            let dgram = encode(&CtlPacket {
                ack: 0,
                ack_bits: 0,
                frame: Some((
                    seq,
                    ControlMsg::AvatarChunk {
                        hash: [0x11; 32],
                        index: 0,
                        total: 256,
                        data: vec![0x22; AVATAR_CHUNK_BYTES],
                    },
                )),
            });
            // Nothing is deliverable while seq 0 is missing.
            assert!(b.receive(&dgram).unwrap().is_empty());
            assert!(
                b.buffered() <= RECV_WINDOW as usize,
                "buffered {} frames after seq {seq}",
                b.buffered()
            );
        }
        // The window's worth that was kept is genuinely usable: the frame
        // that opens it releases all of them at once.
        let open = encode(&CtlPacket {
            ack: 0,
            ack_bits: 0,
            frame: Some((0, chat(0))),
        });
        assert_eq!(b.receive(&open).unwrap().len(), RECV_WINDOW as usize);
        assert_eq!(b.buffered(), 0);
    }

    /// The window bound only holds if `ack_bits` cannot advertise past it,
    /// otherwise the sender would ask for frames the receiver threw away.
    #[test]
    fn ack_bits_never_advertises_past_the_window() {
        let mut b = ControlLink::new();
        for seq in 1..=RECV_WINDOW + 8 {
            let dgram = encode(&CtlPacket {
                ack: 0,
                ack_bits: 0,
                frame: Some((seq, chat(seq))),
            });
            b.receive(&dgram).unwrap();
        }
        // Bits 0..30 cover seq 1..31, all buffered; seq 32 and beyond were
        // refused, so the top bit stays clear.
        assert_eq!(b.ack_bits(), u32::MAX >> 1);
    }

    /// `send` refused oversized payloads and `receive` did not, so the caps
    /// bound only well-behaved peers. Crafted frames go straight to the
    /// encoder here because `send` is exactly what an attacker skips.
    #[test]
    fn receive_refuses_payloads_send_would_refuse() {
        let oversized = [
            ControlMsg::Chat {
                from: MemberId(1),
                text: "x".repeat(MAX_CHAT_LEN + 1),
            },
            ControlMsg::Bye {
                reason: "x".repeat(MAX_CHAT_LEN + 1),
            },
            ControlMsg::AvatarChunk {
                hash: [0; 32],
                index: 0,
                total: 1,
                data: vec![0; AVATAR_CHUNK_BYTES + 1],
            },
            ControlMsg::StreamStatus {
                destinations: vec![DestinationStatus {
                    id: DestinationId(1),
                    platform: StreamPlatform::Twitch,
                    state: DestinationState::Failed {
                        reason: "x".repeat(MAX_STREAM_REASON_LEN + 1),
                    },
                    bitrate_kbps: 0,
                    dropped_frames: 0,
                }],
            },
            ControlMsg::Roster(vec![MemberInfo {
                id: MemberId(1),
                role: Role::Musician,
                name: "n".repeat(MAX_NAME_LEN + 1),
                connected: true,
                avatar_hash: None,
            }]),
        ];
        for msg in oversized {
            let mut link = ControlLink::new();
            assert!(link.send(msg.clone()).is_err(), "send accepted {msg:?}");
            let dgram = encode(&CtlPacket {
                ack: 0,
                ack_bits: 0,
                frame: Some((0, msg.clone())),
            });
            assert!(
                link.receive(&dgram).is_err(),
                "receive accepted {msg:?} that send refused"
            );
            assert_eq!(link.buffered(), 0);
        }
    }

    #[test]
    fn survives_heavy_loss_and_reorder() {
        // Deterministic pseudo-random loss pattern; no rng dependency.
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        let total = 40u64;
        for n in 0..total {
            a.send(chat(n)).unwrap();
        }
        let mut received = Vec::new();
        let mut state = 0x2545F4914F6CDD1Du64;
        let mut now = 0u64;
        while received.len() < total as usize && now < 60_000 {
            for dgram in a.poll(now) {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                // Drop roughly a third of everything.
                if state >> 60 < 5 {
                    continue;
                }
                received.extend(b.receive(&dgram).unwrap());
            }
            for dgram in b.poll(now) {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                if state >> 60 < 5 {
                    continue;
                }
                a.receive(&dgram).unwrap();
            }
            now += 50;
        }
        assert_eq!(received, (0..total).map(chat).collect::<Vec<_>>());
        assert!(!a.is_dead());
    }
}
