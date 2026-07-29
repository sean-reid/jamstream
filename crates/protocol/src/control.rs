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
/// Destinations one session may point at, and so the longest `StreamStatus`
/// that decodes. The pipeline in jamstream-stream refuses to add a ninth and
/// re-exports this rather than keeping its own number, so the cap a host hits
/// and the cap the wire enforces cannot drift apart. Each destination is a
/// process on the session VM and another copy of the egress bill.
pub const MAX_DESTINATIONS: usize = 8;
/// Longest accepted failure reason in a [`RecordingState::Failed`].
pub const MAX_RECORD_REASON_LEN: usize = 200;
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
/// Attempts at one frame before the link declares the peer unreachable.
/// A frame is retired only when it arrives and an ack comes back, so at the
/// 50% loss in each direction the link is required to survive, one attempt
/// succeeds about a quarter of the time. 20 left a lone straggler failing
/// 0.25% of the time, measured over 60,000 lossy runs, which is what made
/// the delivery property flaky; at 36 the same measurement is 0 in 60,000.
/// The give-up horizon this buys, 65 s, is well past the 10 s member timeout
/// that actually reaps a vanished peer, so it only ever helps a peer that is
/// alive on a bad link.
const MAX_SENDS: u32 = 36;

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
    /// Frames the encoder's queue refused, cumulative and pipeline-wide.
    /// Genuine loss: the frame was never delivered, so the broadcast's video
    /// timeline is this many pictures short of its audio.
    ///
    /// This used to count repeats as well, which is what
    /// [`Self::repeated_frames`] is for now (#278). One number could not say
    /// which of the two a host was looking at, and they mean opposite things:
    /// a repeat says the machine is struggling, a drop says it has already
    /// failed to deliver.
    pub dropped_frames: u64,
    /// Catch-up frames the renderer had no time to draw, cumulative and
    /// pipeline-wide. Delivered, as a repeat of the previous picture: the
    /// frame count is what holds video in step with audio, so a frame with no
    /// time to draw goes out again rather than being skipped. Nothing is
    /// missing and A/V sync stays exact; the cost is a stutter.
    ///
    /// Trailing field, appended while protocol version 1 is unreleased.
    /// Postcard writes struct fields in order with no framing, so bytes
    /// written before it are short of the new encoding and fail to decode
    /// rather than misreading, the same note as `MemberInfo::avatar_hash`.
    pub repeated_frames: u64,
}

/// What the host asks the recorder to do. Whether stems are captured is set
/// at launch, not here; Start records whatever the session configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordOp {
    /// Begin a take.
    Start,
    /// End the take. Upload may still be in flight afterwards.
    Stop,
}

/// Recorder lifecycle. `Failed` carries a reason a musician can act on
/// ("bucket write refused"), shown beside the on-air lamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordingState {
    Idle,
    Recording,
    /// The take ended; its tail is still being written to storage.
    Uploading,
    Failed {
        reason: String,
    },
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
    /// The server has not heard from this member lately, but has not given up
    /// on them either. `connected` is still true; that is the point.
    ///
    /// With `connected` this is a three-state presence, which is what a
    /// musician mid-song needs: here, gone quiet, gone. Before it existed a
    /// client saw the roster before a member vanished and the roster after,
    /// with nothing in between, so a stall and a healthy player looked the
    /// same until the server gave up ten seconds later (#285). The server is
    /// the only party that can know: it is the only one that receives every
    /// member's packets.
    ///
    /// `jamstream_session::MEMBER_QUIET_AFTER_MS` is the threshold and
    /// `DEFAULT_MEMBER_TIMEOUT_MS` is when `connected` goes false, so the
    /// quiet window is the gap between them. Trailing field, so the same
    /// encoding note as `avatar_hash` applies: one byte, appended, and bytes
    /// written before it fail to decode rather than misreading.
    pub quiet: bool,
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
    /// Host to server: start or stop the session recording. Host-only; the
    /// server counts a violation against any other sender. Trailing variant,
    /// same postcard append-safety rule as Stats.
    RecordCtl {
        op: RecordOp,
    },
    /// Server to all: the recorder's state, immediately on any transition
    /// and to a member who joins mid-take. A full snapshot, so the latest
    /// one is always sufficient. Trailing variant, as above.
    RecordStatus {
        state: RecordingState,
        /// Whether per-member stems are captured alongside the mix, so
        /// surfaces can show what a take holds. Fixed for the session.
        stems: bool,
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
    /// The peer's `recv_next` as last heard, which is the base of the window
    /// it will accept. Frames at or past `peer_ack + RECV_WINDOW` stay off
    /// the wire until it advances.
    peer_ack: u64,
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
        // Flow control, matching the receiver's rule in `receive`: it refuses
        // anything at or past `recv_next + RECV_WINDOW`, so sending such a
        // frame only spends one of its MAX_SENDS attempts against a closed
        // window. Without this the queue's 128-deep cap let a sender put four
        // windows on the wire at once and give up on everything past the
        // first, which a chat burst, a roster fanout, or an avatar train all
        // reach. `pending` is in ascending seq order, so the first frame past
        // the limit ends the scan.
        let send_limit = self.peer_ack + RECV_WINDOW;
        for p in self.pending.iter_mut() {
            if p.seq >= send_limit {
                break;
            }
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
        // cumulative ack, plus whatever the selective bitmap covers. It also
        // slides our send window; acks can arrive out of order, so it only
        // ever moves forward.
        self.peer_ack = self.peer_ack.max(pkt.ack);
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

    /// True once a message has been retransmitted past the give-up limit.
    /// The peer is not reachable with anything that has to arrive, so both
    /// cores drop the connection on it: the server reaps the member, the
    /// client reports a timeout. Nothing else reaches this state, because a
    /// peer that has simply vanished is reaped by the member timeout first.
    pub fn is_dead(&self) -> bool {
        self.dead
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
            if destinations.len() > MAX_DESTINATIONS {
                return Err(Error::Malformed);
            }
            let bad_reason = destinations.iter().any(|d| match &d.state {
                DestinationState::Failed { reason } => reason.len() > MAX_STREAM_REASON_LEN,
                _ => false,
            });
            if bad_reason {
                return Err(Error::Malformed);
            }
            Ok(())
        }
        ControlMsg::RecordStatus {
            state: RecordingState::Failed { reason },
            ..
        } if reason.len() > MAX_RECORD_REASON_LEN => Err(Error::Malformed),
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
        assert_eq!(a.pending_len(), 0);
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
        assert_eq!(a.pending_len(), 0);
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
                        repeated_frames: 0,
                    },
                    DestinationStatus {
                        id: DestinationId(2),
                        platform: StreamPlatform::YouTube,
                        state: DestinationState::Failed {
                            reason: "pusher exited: connection refused".into(),
                        },
                        bitrate_kbps: 2_628,
                        dropped_frames: 3,
                        repeated_frames: 41,
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

    #[test]
    fn record_messages_round_trip() {
        let msgs = [
            ControlMsg::RecordCtl {
                op: RecordOp::Start,
            },
            ControlMsg::RecordCtl { op: RecordOp::Stop },
            ControlMsg::RecordStatus {
                state: RecordingState::Idle,
                stems: false,
            },
            ControlMsg::RecordStatus {
                state: RecordingState::Recording,
                stems: true,
            },
            ControlMsg::RecordStatus {
                state: RecordingState::Uploading,
                stems: true,
            },
            ControlMsg::RecordStatus {
                state: RecordingState::Failed {
                    reason: "bucket write refused".into(),
                },
                stems: false,
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

    /// Exact wire bytes, pinned so a reordered field or shifted discriminant
    /// cannot pass by encoding and decoding with the same wrong code.
    #[test]
    fn record_encodings_match_golden_bytes() {
        let cases: &[(ControlMsg, &[u8])] = &[
            (
                ControlMsg::RecordCtl {
                    op: RecordOp::Start,
                },
                &[0x11, 0x00],
            ),
            (ControlMsg::RecordCtl { op: RecordOp::Stop }, &[0x11, 0x01]),
            (
                ControlMsg::RecordStatus {
                    state: RecordingState::Idle,
                    stems: false,
                },
                &[0x12, 0x00, 0x00],
            ),
            (
                ControlMsg::RecordStatus {
                    state: RecordingState::Recording,
                    stems: true,
                },
                &[0x12, 0x01, 0x01],
            ),
            (
                ControlMsg::RecordStatus {
                    state: RecordingState::Uploading,
                    stems: true,
                },
                &[0x12, 0x02, 0x01],
            ),
            (
                ControlMsg::RecordStatus {
                    state: RecordingState::Failed {
                        reason: "dry".into(),
                    },
                    stems: false,
                },
                &[0x12, 0x03, 0x03, b'd', b'r', b'y', 0x00],
            ),
        ];
        for (msg, bytes) in cases {
            assert_eq!(&postcard::to_allocvec(msg).unwrap(), bytes, "{msg:?}");
            assert_eq!(&postcard::from_bytes::<ControlMsg>(bytes).unwrap(), msg);
        }
        // An unknown state discriminant is refused, not misread.
        assert!(postcard::from_bytes::<ControlMsg>(&[0x12, 0x04, 0x00]).is_err());
        // So is a truncated status.
        assert!(postcard::from_bytes::<ControlMsg>(&[0x12, 0x01]).is_err());
    }

    #[test]
    fn rejects_oversized_record_reason() {
        let mut a = ControlLink::new();
        assert!(
            a.send(ControlMsg::RecordStatus {
                state: RecordingState::Failed {
                    reason: "x".repeat(MAX_RECORD_REASON_LEN + 1),
                },
                stems: false,
            })
            .is_err()
        );
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
                    repeated_frames: 0,
                }],
            })
            .is_err()
        );
    }

    /// Exact `StreamStatus` bytes, worked out from the encoding rules rather
    /// than captured from the encoder, so the two have to agree for this to
    /// pass. A field reordered, a state discriminant shifted, or
    /// `repeated_frames` silently dropped cannot survive it, which is what a
    /// round trip through the same wrong code lets through.
    ///
    /// Derivation, postcard: an enum is its variant index as a varint, a
    /// newtype struct is its inner value, `u16`/`u32`/`u64` are LEB128
    /// varints, a `String` and a `Vec` are a varint length then their
    /// contents, and struct fields go in declaration order with no framing.
    /// `ControlMsg::StreamStatus` is variant 16 and `DestinationState` runs
    /// Idle, Connecting, Live, Failed.
    ///
    ///   10              ControlMsg::StreamStatus, variant 16
    ///   02              two destinations
    ///     01            DestinationId(1)
    ///     00            StreamPlatform::Twitch
    ///     02            DestinationState::Live
    ///     c4 14         bitrate 2628: 20 * 128 + 68
    ///     00            dropped_frames: 0
    ///     05            repeated_frames: 5
    ///     ac 02         DestinationId(300): 2 * 128 + 44
    ///     01            StreamPlatform::YouTube
    ///     03            DestinationState::Failed
    ///     04 67 6f 6e 65  reason "gone"
    ///     c4 14         bitrate 2628
    ///     07            dropped_frames: 7
    ///     ac 02         repeated_frames: 300
    #[test]
    fn stream_status_encoding_is_pinned() {
        const GOLDEN: &str = concat!(
            "1002",
            "010002",
            "c414",
            "00",
            "05",
            "ac02",
            "01",
            "0304676f6e65",
            "c414",
            "07",
            "ac02",
        );
        let status = ControlMsg::StreamStatus {
            destinations: vec![
                DestinationStatus {
                    id: DestinationId(1),
                    platform: StreamPlatform::Twitch,
                    state: DestinationState::Live,
                    bitrate_kbps: 2_628,
                    dropped_frames: 0,
                    repeated_frames: 5,
                },
                DestinationStatus {
                    id: DestinationId(300),
                    platform: StreamPlatform::YouTube,
                    state: DestinationState::Failed {
                        reason: "gone".into(),
                    },
                    bitrate_kbps: 2_628,
                    dropped_frames: 7,
                    repeated_frames: 300,
                },
            ],
        };
        let bytes = postcard::to_allocvec(&status).unwrap();
        assert_eq!(data_encoding::HEXLOWER.encode(&bytes), GOLDEN);
        let golden = data_encoding::HEXLOWER.decode(GOLDEN.as_bytes()).unwrap();
        assert_eq!(postcard::from_bytes::<ControlMsg>(&golden).unwrap(), status);
        // One byte short is the second destination's repeat count cut in half:
        // refused, not read as a smaller number.
        assert!(postcard::from_bytes::<ControlMsg>(&golden[..golden.len() - 1]).is_err());
        // An unknown state discriminant is refused rather than misread.
        assert!(postcard::from_bytes::<ControlMsg>(&[0x10, 0x01, 0x01, 0x00, 0x04]).is_err());
    }

    /// The same note as `MemberInfo`, for the field #278 appended: bytes from
    /// before it are short of the new encoding and fail to decode instead of
    /// reading as "no repeats". Breaking pre-release, by decision.
    #[test]
    fn destination_status_trailing_field_changed_the_status_encoding() {
        #[derive(Serialize)]
        struct OldDestinationStatus {
            id: DestinationId,
            platform: StreamPlatform,
            state: DestinationState,
            bitrate_kbps: u32,
            dropped_frames: u64,
        }
        let old = postcard::to_allocvec(&OldDestinationStatus {
            id: DestinationId(1),
            platform: StreamPlatform::Twitch,
            state: DestinationState::Live,
            bitrate_kbps: 2_628,
            dropped_frames: 9,
        })
        .unwrap();
        assert!(postcard::from_bytes::<DestinationStatus>(&old).is_err());

        let now = DestinationStatus {
            id: DestinationId(1),
            platform: StreamPlatform::Twitch,
            state: DestinationState::Live,
            bitrate_kbps: 2_628,
            dropped_frames: 9,
            repeated_frames: 0,
        };
        let bytes = postcard::to_allocvec(&now).unwrap();
        // The repeat count is exactly the byte the old encoding lacked.
        assert_eq!(bytes.len(), old.len() + 1);
        assert_eq!(
            postcard::from_bytes::<DestinationStatus>(&bytes).unwrap(),
            now
        );
    }

    /// A status longer than a session can legally have is refused at decode,
    /// not just at send. `MAX_DATAGRAM_BYTES` left room for about 200 entries,
    /// so the only bound was the datagram's.
    #[test]
    fn rejects_oversized_destination_list() {
        let destinations: Vec<_> = (0..=MAX_DESTINATIONS as u16)
            .map(|i| DestinationStatus {
                id: DestinationId(i),
                platform: StreamPlatform::Twitch,
                state: DestinationState::Live,
                bitrate_kbps: 2_628,
                dropped_frames: 0,
                repeated_frames: 0,
            })
            .collect();
        let mut link = ControlLink::new();
        let legal = ControlMsg::StreamStatus {
            destinations: destinations[..MAX_DESTINATIONS].to_vec(),
        };
        assert!(link.send(legal).is_ok());
        assert!(
            link.send(ControlMsg::StreamStatus { destinations })
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

    /// Same rule for the recording variants: appended after StreamStatus,
    /// leaving every earlier variant's bytes unchanged.
    #[test]
    fn appending_record_variants_left_earlier_encodings_alone() {
        let ctl = postcard::to_allocvec(&ControlMsg::RecordCtl {
            op: RecordOp::Start,
        })
        .unwrap();
        assert_eq!(ctl[0], 17);
        let status = postcard::to_allocvec(&ControlMsg::RecordStatus {
            state: RecordingState::Idle,
            stems: false,
        })
        .unwrap();
        assert_eq!(status[0], 18);
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

    /// Pins what postcard actually does with the trailing fields added to
    /// MemberInfo: old bytes are short of the new ones and fail to decode
    /// instead of misreading. Breaking pre-release, by decision, twice now:
    /// the `avatar_hash` Option and then the `quiet` flag (#285).
    #[test]
    fn member_info_trailing_fields_changed_the_roster_encoding() {
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
            quiet: false,
        };
        let bytes = postcard::to_allocvec(&unset).unwrap();
        // The Option tag and the quiet byte are exactly the two the oldest
        // encoding lacked.
        assert_eq!(bytes.len(), old.len() + 2);
        assert_eq!(postcard::from_bytes::<MemberInfo>(&bytes).unwrap(), unset);

        // And the encoding from between the two changes, which had the Option
        // and not the flag, is also refused rather than read as quiet: false.
        assert!(postcard::from_bytes::<MemberInfo>(&bytes[..bytes.len() - 1]).is_err());

        let set = MemberInfo {
            avatar_hash: Some([9u8; 32]),
            quiet: true,
            ..unset
        };
        let bytes = postcard::to_allocvec(&set).unwrap();
        assert_eq!(postcard::from_bytes::<MemberInfo>(&bytes).unwrap(), set);
    }

    /// Exact roster bytes, worked out from the encoding rules rather than
    /// captured from the encoder above, so the two have to agree for this to
    /// pass. A field reordered, a discriminant shifted, or `quiet` silently
    /// dropped cannot survive it, which is what a round trip through the same
    /// wrong code would let through.
    ///
    /// Derivation, postcard: a newtype struct is its inner value, `u16` is a
    /// LEB128 varint, an enum is its variant index as a varint, a `String` and
    /// a `Vec` are a varint length then their contents, a fixed-size array has
    /// no length prefix at all, `bool` is one byte, and `Option` is a 0x00 tag
    /// or 0x01 followed by the value. Struct fields go in declaration order
    /// with no framing between them.
    ///
    ///   00              ControlMsg::Roster, variant 0
    ///   02              two members
    ///     03            MemberId(3)
    ///     00            Role::Musician
    ///     03 61 6e 61   "ana"
    ///     01            connected
    ///     00            avatar_hash: None
    ///     00            quiet: false
    ///     ac 02         MemberId(300), varint: 300 = 0x2c | 0x80, 0x02
    ///     01            Role::Listener
    ///     02 62 6f      "bo"
    ///     01            connected
    ///     01 09 x32     avatar_hash: Some([9; 32])
    ///     01            quiet: true
    #[test]
    fn roster_encoding_is_pinned() {
        const GOLDEN: &str = concat!(
            "0002",
            "030003616e610100",
            "00",
            "ac0201",
            "02626f",
            "0101",
            "0909090909090909090909090909090909090909090909090909090909090909",
            "01",
        );
        let roster = ControlMsg::Roster(vec![
            MemberInfo {
                id: MemberId(3),
                role: Role::Musician,
                name: "ana".into(),
                connected: true,
                avatar_hash: None,
                quiet: false,
            },
            MemberInfo {
                id: MemberId(300),
                role: Role::Listener,
                name: "bo".into(),
                connected: true,
                avatar_hash: Some([9u8; 32]),
                quiet: true,
            },
        ]);
        let bytes = postcard::to_allocvec(&roster).unwrap();
        assert_eq!(data_encoding::HEXLOWER.encode(&bytes), GOLDEN);
        let golden = data_encoding::HEXLOWER.decode(GOLDEN.as_bytes()).unwrap();
        assert_eq!(postcard::from_bytes::<ControlMsg>(&golden).unwrap(), roster);
        // One byte short is the second member with no quiet flag: refused,
        // not read as present-and-talking.
        assert!(postcard::from_bytes::<ControlMsg>(&golden[..golden.len() - 1]).is_err());
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
            // Strictly inside: the slot at `recv_next` drains on arrival, so
            // the 31 above it are all that can be held.
            assert!(
                b.buffered() < RECV_WINDOW as usize,
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

    /// The receive window was bounded without a matching bound on the send
    /// side, so `poll` put up to MAX_PENDING frames on the wire while the
    /// peer accepted only RECV_WINDOW of them. Everything past the window was
    /// dropped on arrival, retransmitted, dropped again, and abandoned after
    /// MAX_SENDS attempts, all without a packet ever being lost.
    #[test]
    fn poll_holds_frames_the_receiver_would_refuse() {
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        let count = RECV_WINDOW + 8;
        for n in 0..count {
            a.send(chat(n)).unwrap();
        }
        // A window's worth goes out, no more, on a lossless path.
        let first = a.poll(0);
        assert_eq!(first.len(), RECV_WINDOW as usize);
        let got: Vec<_> = first
            .iter()
            .flat_map(|d| b.receive(d).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(got.len(), RECV_WINDOW as usize);
        // Nothing more until the peer's ack slides the window.
        assert!(a.poll(1).is_empty());
        for ack in b.poll(1) {
            a.receive(&ack).unwrap();
        }
        let rest: Vec<_> = a
            .poll(2)
            .iter()
            .flat_map(|d| b.receive(d).unwrap())
            .collect();
        assert_eq!(rest, (RECV_WINDOW..count).map(chat).collect::<Vec<_>>());
        assert!(!a.is_dead());
    }

    /// The same defect end to end, on the case the loss property in
    /// `tests/properties.rs` shrank to while it was live: 38 messages, 50%
    /// loss in both directions, and all 38 have to arrive. Before the send
    /// window, m35 through m37 were abandoned with no packet loss to blame.
    ///
    /// A property that found a case once is not a gate that finds it again.
    /// Replaying that seed no longer fails if the send limit goes, so the
    /// per-poll bound below is what holds this end; without it, deleting the
    /// limit fails one test in this file and nothing else.
    #[test]
    fn every_message_arrives_past_the_window_under_loss() {
        let count = 38u64;
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        for n in 0..count {
            a.send(chat(n)).unwrap();
        }
        let mut state = 4_896_297_390_500_780_748u64 | 1;
        let mut lost = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 61 < 4
        };
        let mut got = Vec::new();
        let mut now = 0u64;
        while (got.len() as u64) < count && now < 120_000 {
            let batch = a.poll(now);
            assert!(
                batch.len() <= RECV_WINDOW as usize,
                "{} frames on the wire at {now} ms, past the window the peer accepts",
                batch.len()
            );
            for d in batch {
                if !lost() {
                    got.extend(b.receive(&d).unwrap());
                }
            }
            for d in b.poll(now) {
                if !lost() {
                    a.receive(&d).unwrap();
                }
            }
            now += 50;
        }
        assert_eq!(got, (0..count).map(chat).collect::<Vec<_>>());
        assert!(!a.is_dead());
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
                    repeated_frames: 0,
                }],
            },
            ControlMsg::StreamStatus {
                destinations: (0..=MAX_DESTINATIONS as u16)
                    .map(|i| DestinationStatus {
                        id: DestinationId(i),
                        platform: StreamPlatform::Twitch,
                        state: DestinationState::Live,
                        bitrate_kbps: 2_628,
                        dropped_frames: 0,
                        repeated_frames: 0,
                    })
                    .collect(),
            },
            ControlMsg::RecordStatus {
                state: RecordingState::Failed {
                    reason: "x".repeat(MAX_RECORD_REASON_LEN + 1),
                },
                stems: false,
            },
            ControlMsg::Roster(vec![MemberInfo {
                id: MemberId(1),
                role: Role::Musician,
                name: "n".repeat(MAX_NAME_LEN + 1),
                connected: true,
                avatar_hash: None,
                quiet: false,
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
