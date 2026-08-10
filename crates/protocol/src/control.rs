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
/// Longest accepted failure reason in a [`DestinationStatus`]. What a
/// receiver tolerates; [`STREAM_REASON_BUDGET`] is what a sender may build.
pub const MAX_STREAM_REASON_LEN: usize = 200;
/// Longest failure reason a sender puts in a [`DestinationStatus`].
///
/// Smaller than [`MAX_STREAM_REASON_LEN`] because the cap that matters is the
/// datagram, not the field: a session may have [`MAX_DESTINATIONS`] of these
/// and they travel in one `StreamStatus`. Eight reasons at the wire cap seal
/// to 1850 bytes, which fragments on a 1500-byte path and so is routinely
/// dropped across the internet; eight at this budget seal to 1274, which does
/// not. `a_full_stream_status_never_fragments` pins it.
///
/// Failing that way would be silent and total. `ControlLink::send` refuses an
/// over-long reason for the whole message, so one destination's long
/// explanation costs every other destination its status line.
pub const STREAM_REASON_BUDGET: usize = 128;
/// Destinations one session may point at, and so the longest `StreamStatus`
/// that decodes. The pipeline in jamstream-stream refuses to add a ninth and
/// re-exports this rather than keeping its own number, so the cap a host hits
/// and the cap the wire enforces cannot drift apart. Each destination is a
/// process on the session VM and another copy of the egress bill.
pub const MAX_DESTINATIONS: usize = 8;
/// Longest accepted failure reason in a [`RecordingState::Failed`].
pub const MAX_RECORD_REASON_LEN: usize = 200;
/// Longest accepted line in a [`ControlMsg::ServerLog`].
///
/// Wide enough for the lines a session actually fails on: an ffmpeg refusal
/// with its timestamp, level, and target is about 180 bytes, and a systemd
/// unit path or a bucket URL pushes a few past 250. One line travels per
/// message, so the sealed datagram is this plus about 40 bytes of framing,
/// well inside the smallest MTU: a log line can never fragment, and it can
/// never cost another field its place, because it shares a message with
/// nothing.
pub const MAX_SERVER_LOG_LINE: usize = 320;
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
/// whether or not it was buffered. Holding it buys nothing and costs about 2 KB
/// of permanently live heap per packet from any peer that simply never sends
/// `recv_next`.
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
/// on (`push failed: Failed to connect to rtmps://<redacted> Connection
/// refused`), never a stream key.
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

/// Whether this session can broadcast at all, which is a different question
/// from what any destination is doing.
///
/// The encoder publishes to a relay on the session machine and every pusher
/// reads from it, so a session whose relay never came up, or whose broadcast
/// tooling never downloaded, cannot stream anywhere no matter what key the
/// host pastes. Nothing else says so: the relay's unit is `Type=simple`, and
/// systemd calls one of those started the moment it forks, so the journal reads
/// `Started mediamtx.service` whether the relay is serving or died a second
/// later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BroadcastReadiness {
    /// The relay answered where the encoder publishes.
    Ready,
    /// It did not, and `reason` is what the host is told instead of being
    /// offered a key field that leads nowhere.
    Unavailable { reason: String },
}

/// Cuts a failure reason down to [`STREAM_REASON_BUDGET`], on a char
/// boundary so the result is still a `str`.
///
/// The cut keeps the head. ffmpeg reports the fault it hit first and then the
/// consequences of it, so the front of a reason is the diagnosis and the back
/// is the fallout.
pub fn fit_stream_reason(reason: &str) -> &str {
    fit_head(reason, STREAM_REASON_BUDGET)
}

/// Cuts one log line down to [`MAX_SERVER_LOG_LINE`], on a char boundary.
///
/// The head again, and for the same reason: a log line names what happened
/// first and elaborates afterwards.
pub fn fit_server_log_line(line: &str) -> &str {
    fit_head(line, MAX_SERVER_LOG_LINE)
}

fn fit_head(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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
    /// Repeats are counted separately, by [`Self::repeated_frames`], because the
    /// two mean opposite things: a repeat says the machine is struggling, a drop
    /// says it has already failed to deliver.
    pub dropped_frames: u64,
    /// Catch-up frames the renderer had no time to draw, cumulative and
    /// pipeline-wide. Delivered, as a repeat of the previous picture: the
    /// frame count is what holds video in step with audio, so a frame with no
    /// time to draw goes out again rather than being skipped. Nothing is
    /// missing and A/V sync stays exact; the cost is a stutter.
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
    /// Blake2s-256 of the member's avatar bytes; None when unset.
    pub avatar_hash: Option<[u8; 32]>,
    /// The server has not heard from this member lately, but has not given up
    /// on them either. `connected` is still true; that is the point.
    ///
    /// With `connected` this is a three-state presence, which is what a
    /// musician mid-song needs: here, gone quiet, gone. Without it, a
    /// client sees the roster before a member vanishes and the roster
    /// after, with nothing in between, so a stall and a healthy player look
    /// the same until the server gives up ten seconds later. The server is
    /// the only party that can know: it is the only one that receives every
    /// member's packets.
    ///
    /// `jamstream_session::MEMBER_QUIET_AFTER_MS` is the threshold and
    /// `DEFAULT_MEMBER_TIMEOUT_MS` is when `connected` goes false, so the
    /// quiet window is the gap between them.
    pub quiet: bool,
}

/// Everything the control plane carries. The declaration is the wire: postcard
/// writes a variant index as a varint and struct fields in order with no
/// framing between them, so a new variant belongs at the end, where it leaves
/// every existing variant's bytes alone, and a new field changes the encoding
/// of the message that gains it.
///
/// Either way a peer at the older version refuses the bytes rather than reading
/// a message that means something else: an unknown variant index does not
/// decode, and [`ControlLink::receive`] refuses a datagram with bytes left
/// over. So an addition here moves `PROTOCOL_VERSION`.
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
    Stats {
        uplink_loss_pct: f32,
        uplink_jitter_depth: u16,
        uplink_recovered_pct: f32,
    },
    /// Host shapes one member's fader in the broadcast mix (host to server);
    /// the server relays accepted changes to every connected member so UIs
    /// can mirror the state.
    BroadcastMixSet {
        target: MemberId,
        gain_db: f32,
        pan: f32,
        muted: bool,
    },
    /// Host to server: while enabled, the host hears the broadcast mix
    /// instead of their personal mix.
    BroadcastAudition {
        enabled: bool,
    },
    /// Client to server: announce or replace this member's avatar by
    /// content hash. The bytes follow only if the server asks for them.
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
    StreamCtl {
        op: StreamOp,
    },
    /// Server to all: the on-air state, once a second while streaming and
    /// immediately on any transition. Every member sees it, not just the
    /// host, because everyone in the room deserves to know they are live.
    StreamStatus {
        destinations: Vec<DestinationStatus>,
    },
    /// Host to server: start or stop the session recording. Host-only; the
    /// server counts a violation against any other sender.
    RecordCtl {
        op: RecordOp,
    },
    /// Server to all: the recorder's state, immediately on any transition
    /// and to a member who joins mid-take. A full snapshot, so the latest
    /// one is always sufficient.
    RecordStatus {
        state: RecordingState,
        /// Whether per-member stems are captured alongside the mix, so
        /// surfaces can show what a take holds. Fixed for the session.
        stems: bool,
    },
    /// Client to server: the sender's own display name, replacing the
    /// invite's `name_hint` or the member-N fallback on the roster. Self
    /// only: the sender is the target, exactly as `Chat` forces `from`.
    /// Names past [`MAX_NAME_LEN`] are refused at the link, the same cap the
    /// roster enforces, so a name the roster could not carry never leaves
    /// the client.
    SetName {
        name: String,
    },
    /// Server to all: whether this session can broadcast at all, on change and
    /// to a member as they join. Separate from [`Self::StreamStatus`] because
    /// it is true of the session rather than of a destination, and because the
    /// host who most needs it has configured none yet.
    BroadcastReadiness {
        state: BroadcastReadiness,
    },
    /// Server to host: one line of the server's own log, as it is written.
    ///
    /// The host is the only recipient. A cloud session's machine deletes
    /// itself when the session ends, so its journal is the one copy of why a
    /// broadcast or a take failed and it goes with the machine; this is how
    /// that copy reaches somebody who can read it. A local session already
    /// hands the same text to `sessions/<id>/server.log` on the way past.
    ///
    /// One line per message, sent as the line is written rather than gathered
    /// at the end: a session ending because the VM is being destroyed gets one
    /// flight with no retransmit, which is the worst moment to be starting.
    ServerLog {
        line: String,
    },
    /// Client to server: a musician asking their own personal mix to include
    /// their own signal, rather than the usual removal. Per member rather
    /// than host-gated, unlike [`Self::BroadcastAudition`].
    HearSelf {
        enabled: bool,
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
    ///
    /// A datagram accounts for every byte in it: anything after the packet is
    /// refused rather than ignored, so a message from a peer whose encoding has
    /// grown a field is refused rather than read as the message without it.
    pub fn receive(&mut self, buf: &[u8]) -> Result<Vec<ControlMsg>, Error> {
        if buf.first() != Some(&CHANNEL_CONTROL) {
            return Err(Error::Malformed);
        }
        let (pkt, rest): (CtlPacket, &[u8]) = postcard::take_from_bytes(&buf[1..])?;
        if !rest.is_empty() {
            return Err(Error::Malformed);
        }

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
        // One reason per message here, not eight, so the field cap is the
        // only one that applies.
        ControlMsg::BroadcastReadiness {
            state: BroadcastReadiness::Unavailable { reason },
        } if reason.len() > MAX_STREAM_REASON_LEN => Err(Error::Malformed),
        ControlMsg::ServerLog { line } if line.len() > MAX_SERVER_LOG_LINE => Err(Error::Malformed),
        ControlMsg::Roster(members) if members.iter().any(|m| m.name.len() > MAX_NAME_LEN) => {
            Err(Error::Malformed)
        }
        // The same cap on the way in: a SetName the roster could not relay
        // would otherwise be accepted here and break fanout there.
        ControlMsg::SetName { name } if name.len() > MAX_NAME_LEN => Err(Error::Malformed),
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
    use blake2::{Blake2s256, Digest};
    use std::collections::BTreeSet;

    /// The reason `STREAM_REASON_BUDGET` is what it is. A session may fail on
    /// every destination at once, and all eight reasons travel in one
    /// `StreamStatus`; at the wire cap that datagram fragments, and a
    /// fragmented datagram is routinely dropped on the paths this has to
    /// work across. The property is pinned rather than the constant, so a
    /// wider `DestinationStatus` fails here instead of in the field.
    #[test]
    fn a_full_stream_status_never_fragments() {
        let destinations: Vec<_> = (0..MAX_DESTINATIONS as u16)
            .map(|i| DestinationStatus {
                id: DestinationId(i),
                platform: StreamPlatform::Twitch,
                state: DestinationState::Failed {
                    reason: "x".repeat(STREAM_REASON_BUDGET),
                },
                bitrate_kbps: 2_628,
                // The widest these counters ever encode to, so a long
                // session cannot be the thing that tips it over.
                dropped_frames: u64::MAX,
                repeated_frames: u64::MAX,
            })
            .collect();
        let mut link = ControlLink::new();
        link.send(ControlMsg::StreamStatus { destinations })
            .expect("a status full of failures must be sendable");
        for plain in link.poll(0) {
            // The transport adds an 11-byte header and a 16-byte aead tag.
            let sealed = plain.len() + 11 + 16;
            assert!(
                sealed <= 1_472,
                "sealed stream status is {sealed} bytes, which fragments on a 1500 byte mtu"
            );
        }
    }

    /// A log line at the cap travels alone in its datagram and fits it with
    /// room to spare, which is why nothing else has to be budgeted against it.
    #[test]
    fn a_full_server_log_line_never_fragments() {
        let mut link = ControlLink::new();
        link.send(ControlMsg::ServerLog {
            line: "x".repeat(MAX_SERVER_LOG_LINE),
        })
        .expect("a line at the cap must be sendable");
        for plain in link.poll(0) {
            let sealed = plain.len() + 11 + 16;
            assert!(
                sealed <= 1_472,
                "sealed server log line is {sealed} bytes, which fragments on a 1500 byte mtu"
            );
        }
    }

    /// The cut is what a sender applies, and the cap is what either side
    /// refuses. A line past the cap must not be sendable at all: the link
    /// refuses the whole message, so a sender that skipped the cut would lose
    /// the line rather than shorten it.
    #[test]
    fn fitting_a_log_line_cuts_on_a_char_boundary() {
        let short = "session server up";
        assert_eq!(fit_server_log_line(short), short);

        let wide = "☃".repeat(MAX_SERVER_LOG_LINE);
        let cut = fit_server_log_line(&wide);
        assert!(cut.len() <= MAX_SERVER_LOG_LINE);
        assert!(cut.len() > MAX_SERVER_LOG_LINE - 3);
        assert!(wide.starts_with(cut));

        let mut link = ControlLink::new();
        assert!(
            link.send(ControlMsg::ServerLog {
                line: cut.to_owned()
            })
            .is_ok()
        );
        assert!(
            link.send(ControlMsg::ServerLog {
                line: "x".repeat(MAX_SERVER_LOG_LINE + 1)
            })
            .is_err()
        );
    }

    /// The budget is a byte count and a reason is text, so the cut has to
    /// land on a char boundary or the result is not a string at all.
    #[test]
    fn fitting_a_reason_cuts_on_a_char_boundary() {
        let short = "connection refused";
        assert_eq!(fit_stream_reason(short), short);

        // Three bytes per char, so the budget falls mid-character.
        let wide = "☃".repeat(STREAM_REASON_BUDGET);
        let cut = fit_stream_reason(&wide);
        assert!(cut.len() <= STREAM_REASON_BUDGET);
        assert!(cut.len() > STREAM_REASON_BUDGET - 3);
        assert!(wide.starts_with(cut));

        // And what comes out is always sendable, which is the whole point.
        let mut link = ControlLink::new();
        assert!(
            link.send(ControlMsg::StreamStatus {
                destinations: vec![DestinationStatus {
                    id: DestinationId(1),
                    platform: StreamPlatform::Twitch,
                    state: DestinationState::Failed {
                        reason: cut.to_owned()
                    },
                    bitrate_kbps: 2_628,
                    dropped_frames: 0,
                    repeated_frames: 0,
                }],
            })
            .is_ok()
        );
    }

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

    /// Every `ControlMsg` variant with the exact bytes it must encode to,
    /// written out from the postcard rules rather than captured from the
    /// encoder, so the two have to agree for the golden test to pass. A round
    /// trip through the same wrong code cannot tell a reordered field from a
    /// correct one; these bytes can.
    ///
    /// Derivation, postcard: an enum is its variant index as a varint, a
    /// newtype struct is its inner value, `u16`/`u32`/`u64` are LEB128
    /// varints, `u8` and `bool` are one byte, `f32` is four bytes
    /// little-endian, a `String` and a `Vec` are a varint length then their
    /// contents, a fixed-size array has no length prefix at all, `Option` is a
    /// 0x00 tag or 0x01 followed by the value, and struct fields go in
    /// declaration order with no framing between them.
    fn golden_control_wire() -> Vec<(ControlMsg, &'static str)> {
        vec![
            (
                ControlMsg::Roster(vec![
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
                ]),
                concat!(
                    "00",       // Roster, variant 0
                    "02",       // two members
                    "03",       // MemberId(3)
                    "00",       // Role::Musician
                    "03616e61", // "ana"
                    "01",       // connected
                    "00",       // avatar_hash: None
                    "00",       // quiet: false
                    "ac02",     // MemberId(300): 300 = 0x2c | 0x80, then 0x02
                    "01",       // Role::Listener
                    "02626f",   // "bo"
                    "01",       // connected
                    "01",       // avatar_hash: Some
                    "0909090909090909090909090909090909090909090909090909090909090909",
                    "01", // quiet: true
                ),
            ),
            (
                ControlMsg::Chat {
                    from: MemberId(300),
                    text: "hey".into(),
                },
                concat!(
                    "01",       // Chat, variant 1
                    "ac02",     // MemberId(300)
                    "03686579", // "hey"
                ),
            ),
            (
                ControlMsg::MixerSet {
                    target: MemberId(7),
                    gain_db: -6.0,
                    pan: -1.0,
                    muted: true,
                },
                concat!(
                    "02",       // MixerSet, variant 2
                    "07",       // MemberId(7)
                    "0000c0c0", // gain_db -6.0: 0xc0c00000 little-endian
                    "000080bf", // pan -1.0: 0xbf800000 little-endian
                    "01",       // muted
                ),
            ),
            (
                ControlMsg::MetronomeSet {
                    bpm: 128,
                    beats_per_bar: 4,
                    enabled: true,
                },
                concat!(
                    "03",   // MetronomeSet, variant 3
                    "8001", // bpm 128, the first value the varint spends two bytes on
                    "04",   // beats_per_bar, a u8 and so never a varint
                    "01",   // enabled
                ),
            ),
            (
                ControlMsg::ClickEnable { enabled: true },
                concat!(
                    "04", // ClickEnable, variant 4
                    "01", // enabled
                ),
            ),
            (
                ControlMsg::Ping {
                    nonce: 70_000,
                    sent_ms: 1_000_000,
                },
                concat!(
                    "05",     // Ping, variant 5
                    "f0a204", // nonce 70000: 112 + 34 * 128 + 4 * 16384
                    "c0843d", // sent_ms 1000000: 64 + 4 * 128 + 61 * 16384
                ),
            ),
            (
                // Same fields as Ping, so the discriminant is all that tells
                // the two apart and swapping the pair fails here.
                ControlMsg::Pong {
                    nonce: 70_000,
                    sent_ms: 1_000_000,
                },
                concat!(
                    "06",     // Pong, variant 6
                    "f0a204", // nonce 70000
                    "c0843d", // sent_ms 1000000
                ),
            ),
            (
                ControlMsg::Revoke {
                    jti: TokenId([0x5a; 16]),
                },
                concat!(
                    "07",                               // Revoke, variant 7
                    "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a", // jti, 16 bytes and no length
                ),
            ),
            (
                ControlMsg::Bye {
                    reason: "kicked".into(),
                },
                concat!(
                    "08",           // Bye, variant 8
                    "06",           // six bytes of reason
                    "6b69636b6564", // "kicked"
                ),
            ),
            (
                ControlMsg::Stats {
                    uplink_loss_pct: 2.5,
                    uplink_jitter_depth: 300,
                    uplink_recovered_pct: 12.5,
                },
                concat!(
                    "09",       // Stats, variant 9
                    "00002040", // uplink_loss_pct 2.5: 0x40200000 little-endian
                    "ac02",     // uplink_jitter_depth 300
                    "00004841", // uplink_recovered_pct 12.5: 0x41480000 little-endian
                ),
            ),
            (
                ControlMsg::BroadcastMixSet {
                    target: MemberId(300),
                    gain_db: -3.5,
                    pan: 0.5,
                    muted: false,
                },
                concat!(
                    "0a",       // BroadcastMixSet, variant 10
                    "ac02",     // MemberId(300)
                    "000060c0", // gain_db -3.5: 0xc0600000 little-endian
                    "0000003f", // pan 0.5: 0x3f000000 little-endian
                    "00",       // muted: false
                ),
            ),
            (
                ControlMsg::BroadcastAudition { enabled: true },
                concat!(
                    "0b", // BroadcastAudition, variant 11
                    "01", // enabled
                ),
            ),
            (
                ControlMsg::SetAvatar {
                    hash: [0xab; 32],
                    len: 200_000,
                },
                concat!(
                    "0c", // SetAvatar, variant 12
                    "abababababababababababababababababababababababababababababababab",
                    "c09a0c", // len 200000: 64 + 26 * 128 + 12 * 16384
                ),
            ),
            (
                ControlMsg::AvatarChunk {
                    hash: [0xab; 32],
                    index: 255,
                    total: 256,
                    data: vec![0x01, 0x02, 0x03],
                },
                concat!(
                    "0d", // AvatarChunk, variant 13
                    "abababababababababababababababababababababababababababababababab",
                    "ff01",   // index 255
                    "8002",   // total 256
                    "03",     // three bytes of data
                    "010203", // data
                ),
            ),
            (
                ControlMsg::AvatarRequest { hash: [0xab; 32] },
                concat!(
                    "0e", // AvatarRequest, variant 14
                    "abababababababababababababababababababababababababababababababab",
                ),
            ),
            (
                ControlMsg::StreamCtl {
                    op: StreamOp::AddDestination {
                        id: DestinationId(2),
                        platform: StreamPlatform::YouTube,
                        key: StreamKey::new("live_key"),
                    },
                },
                concat!(
                    "0f",               // StreamCtl, variant 15
                    "00",               // StreamOp::AddDestination
                    "02",               // DestinationId(2)
                    "01",               // StreamPlatform::YouTube
                    "08",               // eight bytes of key
                    "6c6976655f6b6579", // "live_key", transparent newtype
                ),
            ),
            (
                ControlMsg::StreamCtl {
                    op: StreamOp::RemoveDestination {
                        id: DestinationId(300),
                    },
                },
                concat!(
                    "0f",   // StreamCtl
                    "01",   // StreamOp::RemoveDestination
                    "ac02", // DestinationId(300)
                ),
            ),
            (
                ControlMsg::StreamCtl {
                    op: StreamOp::Start,
                },
                concat!(
                    "0f", // StreamCtl
                    "02", // StreamOp::Start
                ),
            ),
            (
                ControlMsg::StreamCtl { op: StreamOp::Stop },
                concat!(
                    "0f", // StreamCtl
                    "03", // StreamOp::Stop
                ),
            ),
            (
                ControlMsg::StreamStatus {
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
                },
                concat!(
                    "10",         // StreamStatus, variant 16
                    "02",         // two destinations
                    "01",         // DestinationId(1)
                    "00",         // StreamPlatform::Twitch
                    "02",         // DestinationState::Live
                    "c414",       // bitrate_kbps 2628: 68 + 20 * 128
                    "00",         // dropped_frames
                    "05",         // repeated_frames
                    "ac02",       // DestinationId(300)
                    "01",         // StreamPlatform::YouTube
                    "03",         // DestinationState::Failed
                    "04676f6e65", // reason "gone"
                    "c414",       // bitrate_kbps 2628
                    "07",         // dropped_frames
                    "ac02",       // repeated_frames 300
                ),
            ),
            (
                ControlMsg::RecordCtl {
                    op: RecordOp::Start,
                },
                concat!(
                    "11", // RecordCtl, variant 17
                    "00", // RecordOp::Start
                ),
            ),
            (
                ControlMsg::RecordCtl { op: RecordOp::Stop },
                concat!(
                    "11", // RecordCtl
                    "01", // RecordOp::Stop
                ),
            ),
            (
                ControlMsg::RecordStatus {
                    state: RecordingState::Idle,
                    stems: false,
                },
                concat!(
                    "12", // RecordStatus, variant 18
                    "00", // RecordingState::Idle
                    "00", // stems: false
                ),
            ),
            (
                ControlMsg::RecordStatus {
                    state: RecordingState::Recording,
                    stems: true,
                },
                concat!(
                    "12", // RecordStatus
                    "01", // RecordingState::Recording
                    "01", // stems
                ),
            ),
            (
                ControlMsg::RecordStatus {
                    state: RecordingState::Uploading,
                    stems: true,
                },
                concat!(
                    "12", // RecordStatus
                    "02", // RecordingState::Uploading
                    "01", // stems
                ),
            ),
            (
                ControlMsg::RecordStatus {
                    state: RecordingState::Failed {
                        reason: "dry".into(),
                    },
                    stems: false,
                },
                concat!(
                    "12",       // RecordStatus
                    "03",       // RecordingState::Failed
                    "03647279", // reason "dry"
                    "00",       // stems: false
                ),
            ),
            (
                ControlMsg::SetName { name: "ana".into() },
                concat!(
                    "13",       // SetName, variant 19
                    "03616e61", // "ana"
                ),
            ),
            (
                ControlMsg::BroadcastReadiness {
                    state: BroadcastReadiness::Ready,
                },
                concat!(
                    "14", // BroadcastReadiness, variant 20
                    "00", // BroadcastReadiness::Ready
                ),
            ),
            (
                ControlMsg::BroadcastReadiness {
                    state: BroadcastReadiness::Unavailable {
                        reason: "no relay".into(),
                    },
                },
                concat!(
                    "14",               // BroadcastReadiness
                    "01",               // BroadcastReadiness::Unavailable
                    "08",               // eight bytes of reason
                    "6e6f2072656c6179", // "no relay"
                ),
            ),
            (
                ControlMsg::ServerLog {
                    line: "ffmpeg exited 1".into(),
                },
                concat!(
                    "15",           // ServerLog, variant 21
                    "0f",           // fifteen bytes of line
                    "66666d706567", // "ffmpeg"
                    "20",           // " "
                    "657869746564", // "exited"
                    "20",           // " "
                    "31",           // "1"
                ),
            ),
            (
                ControlMsg::HearSelf { enabled: true },
                concat!(
                    "16", // HearSelf, variant 22
                    "01", // enabled
                ),
            ),
        ]
    }

    /// The golden bytes of the first message `pick` matches, so a test about
    /// what a decoder refuses works from the same bytes the table pins.
    fn golden_bytes(pick: impl Fn(&ControlMsg) -> bool) -> Vec<u8> {
        let (_, golden) = golden_control_wire()
            .into_iter()
            .find(|(msg, _)| pick(msg))
            .expect("no golden message matched");
        data_encoding::HEXLOWER.decode(golden.as_bytes()).unwrap()
    }

    /// Exact wire bytes for every variant, so a reordered field or a shifted
    /// discriminant cannot pass by encoding and decoding with the same wrong
    /// code.
    #[test]
    fn every_control_encoding_matches_golden_bytes() {
        for (msg, golden) in golden_control_wire() {
            let bytes = postcard::to_allocvec(&msg).unwrap();
            assert_eq!(data_encoding::HEXLOWER.encode(&bytes), golden, "{msg:?}");
            let decoded = data_encoding::HEXLOWER.decode(golden.as_bytes()).unwrap();
            assert_eq!(postcard::from_bytes::<ControlMsg>(&decoded).unwrap(), msg);
        }
    }

    /// The table above is the whole wire, not a sample of it. postcard refuses
    /// a discriminant `ControlMsg` does not have, so the first index that
    /// fails to decode counts the variants, and a new message with no golden
    /// bytes fails here.
    #[test]
    fn every_control_variant_has_golden_bytes() {
        let mut variants = 0u8;
        loop {
            // Zero satisfies every field postcard can read: an empty string, an
            // empty vec, the first variant of a nested enum. Only an unknown
            // discriminant fails, unless a future variant needs more padding
            // than this, which fails here too rather than quietly.
            let mut probe = vec![variants];
            probe.extend(std::iter::repeat_n(0u8, 256));
            if postcard::from_bytes::<ControlMsg>(&probe).is_err() {
                break;
            }
            variants += 1;
        }
        assert!(
            variants < 0x80,
            "past 127 variants the discriminant is a multi-byte varint, so the \
             first golden byte holds only part of it"
        );
        let covered: BTreeSet<u8> = golden_control_wire()
            .iter()
            .map(|(_, golden)| data_encoding::HEXLOWER.decode(golden.as_bytes()).unwrap()[0])
            .collect();
        assert_eq!(
            covered,
            (0..variants).collect::<BTreeSet<u8>>(),
            "every variant index needs a row in golden_control_wire"
        );
    }

    /// One digest over every golden encoding above, taken together with
    /// `PROTOCOL_VERSION`. One row per version, and a row is a record of what
    /// that version speaks, so an existing row is never edited.
    ///
    /// A server admits exactly its own version, which means a released client
    /// and a new build agree on the wire only for as long as the encodings
    /// hold. Nothing else in the tree compares the two.
    const CONTROL_WIRE_DIGESTS: &[(u16, &str)] = &[(
        2,
        "47d7b8fa104415ffddec7325917e62c0085265dd0b9c9f4fd3f622136c0e6943",
    )];

    /// Ties the table to the version that advertises it. The digest is taken
    /// over the golden bytes themselves, not over the encoder, because
    /// `every_control_encoding_matches_golden_bytes` is what holds the encoder
    /// to the table: what this catches is the wrong answer to that failure,
    /// editing the bytes to match a changed encoder and leaving the version
    /// where it was.
    ///
    /// Two honest ways out of a failure here, and pasting the printed digest
    /// into the row for the current version is neither: undo the encoding
    /// change, or move `PROTOCOL_VERSION` and add a row for the new version,
    /// leaving the old row as it is. Appending a variant is the one change
    /// that moves the digest while every existing variant's bytes hold.
    #[test]
    fn control_wire_digest_is_tied_to_the_protocol_version() {
        let mut h = Blake2s256::new();
        h.update(crate::PROTOCOL_VERSION.to_le_bytes());
        for (_, golden) in golden_control_wire() {
            let bytes = data_encoding::HEXLOWER.decode(golden.as_bytes()).unwrap();
            // Length framed, so no two tables of goldens can hash the same by
            // shifting a byte from one message to the next.
            h.update((bytes.len() as u32).to_le_bytes());
            h.update(&bytes);
        }
        let digest = data_encoding::HEXLOWER.encode(&h.finalize());
        let expected = CONTROL_WIRE_DIGESTS
            .iter()
            .find(|(version, _)| *version == crate::PROTOCOL_VERSION)
            .map(|(_, digest)| *digest)
            .expect("the version this build speaks has a row");
        assert_eq!(
            digest,
            expected,
            "the control encodings do not match protocol version {}. Undo the \
             encoding change, or move PROTOCOL_VERSION and add a row for the new \
             version. Overwriting the row for {} makes a released client read the \
             wrong fields onto the wrong member, which is the failure this test \
             exists to stop",
            crate::PROTOCOL_VERSION,
            crate::PROTOCOL_VERSION
        );
    }

    #[test]
    fn record_status_decode_refuses_unknown_state_and_truncation() {
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

    #[test]
    fn stream_status_decode_refuses_truncation_and_unknown_state() {
        let golden = golden_bytes(|msg| matches!(msg, ControlMsg::StreamStatus { .. }));
        // One byte short is the second destination's repeat count cut in half:
        // refused, not read as a smaller number.
        assert!(postcard::from_bytes::<ControlMsg>(&golden[..golden.len() - 1]).is_err());
        // An unknown state discriminant is refused rather than misread.
        assert!(postcard::from_bytes::<ControlMsg>(&[0x10, 0x01, 0x01, 0x00, 0x04]).is_err());
    }

    /// The other half of the rule on `ControlMsg`, for a field: a status one
    /// field short does not decode, rather than reading as no repeats. The
    /// struct below is what a peer that lacks the repeat count encodes.
    #[test]
    fn a_status_without_the_repeat_count_does_not_decode() {
        #[derive(Serialize)]
        struct StatusWithoutRepeats {
            id: DestinationId,
            platform: StreamPlatform,
            state: DestinationState,
            bitrate_kbps: u32,
            dropped_frames: u64,
        }
        let short = postcard::to_allocvec(&StatusWithoutRepeats {
            id: DestinationId(1),
            platform: StreamPlatform::Twitch,
            state: DestinationState::Live,
            bitrate_kbps: 2_628,
            dropped_frames: 9,
        })
        .unwrap();
        assert!(postcard::from_bytes::<DestinationStatus>(&short).is_err());

        let full = DestinationStatus {
            id: DestinationId(1),
            platform: StreamPlatform::Twitch,
            state: DestinationState::Live,
            bitrate_kbps: 2_628,
            dropped_frames: 9,
            repeated_frames: 0,
        };
        let bytes = postcard::to_allocvec(&full).unwrap();
        // The repeat count is exactly the byte the shorter encoding lacks.
        assert_eq!(bytes.len(), short.len() + 1);
        assert_eq!(
            postcard::from_bytes::<DestinationStatus>(&bytes).unwrap(),
            full
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

    /// The half of the rule on `ControlMsg` that lets it grow: a variant index
    /// is a varint, so a message at the end of the declaration only widens the
    /// tag space and every earlier variant keeps the bytes it has. A variant
    /// inserted rather than appended shifts every index after it and fails
    /// here.
    #[test]
    fn the_stream_variants_leave_earlier_encodings_alone() {
        let earlier = ControlMsg::Chat {
            from: MemberId(1),
            text: "hi".into(),
        };
        // Chat is variant index 1: tag byte 1, then the payload.
        let bytes = postcard::to_allocvec(&earlier).unwrap();
        assert_eq!(bytes[0], 1);
        // The stream pair sits after AvatarRequest.
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

    /// The same rule, for the recording pair after `StreamStatus`.
    #[test]
    fn the_record_variants_leave_earlier_encodings_alone() {
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

    /// The same rule, for the readiness variant after `SetName`.
    #[test]
    fn the_readiness_variant_leaves_earlier_encodings_alone() {
        let name = postcard::to_allocvec(&ControlMsg::SetName { name: "a".into() }).unwrap();
        assert_eq!(name[0], 19);
        let ready = postcard::to_allocvec(&ControlMsg::BroadcastReadiness {
            state: BroadcastReadiness::Ready,
        })
        .unwrap();
        assert_eq!(ready[0], 20);
        // Ready is the whole message: one variant byte for the message and one
        // for the state, which is what lets it be sent once a second forever
        // if it ever needs to be.
        assert_eq!(ready.len(), 2);
    }

    /// The same rule, for `HearSelf` after `ServerLog`.
    #[test]
    fn the_hear_self_variant_leaves_earlier_encodings_alone() {
        let log = postcard::to_allocvec(&ControlMsg::ServerLog { line: "x".into() }).unwrap();
        assert_eq!(log[0], 21);
        let hear_self = postcard::to_allocvec(&ControlMsg::HearSelf { enabled: true }).unwrap();
        assert_eq!(hear_self[0], 22);
        assert_eq!(hear_self.len(), 2);
    }

    #[test]
    fn hear_self_round_trips() {
        let msgs = [
            ControlMsg::HearSelf { enabled: true },
            ControlMsg::HearSelf { enabled: false },
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

    /// A readiness message is one reason rather than eight, so it cannot
    /// fragment the way a full `StreamStatus` can. Pinned anyway, because the
    /// cost of finding out otherwise is a host who is never told.
    #[test]
    fn an_unavailable_reason_at_the_wire_cap_still_fits_one_datagram() {
        let mut link = ControlLink::new();
        link.send(ControlMsg::BroadcastReadiness {
            state: BroadcastReadiness::Unavailable {
                reason: "x".repeat(MAX_STREAM_REASON_LEN),
            },
        })
        .expect("a reason at the cap must be sendable");
        for plain in link.poll(0) {
            let sealed = plain.len() + 11 + 16;
            assert!(
                sealed <= 1_472,
                "sealed readiness is {sealed} bytes, which fragments on a 1500 byte mtu"
            );
        }
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

    /// The same for the roster's two trailing fields: a member short of either
    /// does not decode, rather than reading as an avatarless member who is
    /// talking. The struct below is what a peer that lacks both encodes.
    #[test]
    fn a_member_without_the_trailing_fields_does_not_decode() {
        #[derive(Serialize)]
        struct MemberWithoutAvatarOrQuiet {
            id: MemberId,
            role: Role,
            name: String,
            connected: bool,
        }
        let short = postcard::to_allocvec(&MemberWithoutAvatarOrQuiet {
            id: MemberId(3),
            role: Role::Musician,
            name: "ana".into(),
            connected: true,
        })
        .unwrap();
        assert!(postcard::from_bytes::<MemberInfo>(&short).is_err());

        let unset = MemberInfo {
            id: MemberId(3),
            role: Role::Musician,
            name: "ana".into(),
            connected: true,
            avatar_hash: None,
            quiet: false,
        };
        let bytes = postcard::to_allocvec(&unset).unwrap();
        // The Option tag and the quiet byte are exactly the two the shorter
        // encoding lacks.
        assert_eq!(bytes.len(), short.len() + 2);
        assert_eq!(postcard::from_bytes::<MemberInfo>(&bytes).unwrap(), unset);

        // One field short is also refused, rather than read as quiet: false.
        assert!(postcard::from_bytes::<MemberInfo>(&bytes[..bytes.len() - 1]).is_err());

        let set = MemberInfo {
            avatar_hash: Some([9u8; 32]),
            quiet: true,
            ..unset
        };
        let bytes = postcard::to_allocvec(&set).unwrap();
        assert_eq!(postcard::from_bytes::<MemberInfo>(&bytes).unwrap(), set);
    }

    #[test]
    fn roster_decode_refuses_a_member_short_of_quiet() {
        let golden = golden_bytes(|msg| matches!(msg, ControlMsg::Roster(_)));
        // One byte short is the second member with no quiet flag: refused,
        // not read as present-and-talking.
        assert!(postcard::from_bytes::<ControlMsg>(&golden[..golden.len() - 1]).is_err());
    }

    /// An unbounded reassembly buffer grows for any peer that simply never sends
    /// the sequence number the receiver is waiting for. Each frame can carry a
    /// 1 KB avatar chunk, so 2,000 packets, one home connection's worth at
    /// 2,000 pps, pins megabytes per second.
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
            ControlMsg::SetName {
                name: "n".repeat(MAX_NAME_LEN + 1),
            },
            ControlMsg::BroadcastReadiness {
                state: BroadcastReadiness::Unavailable {
                    reason: "x".repeat(MAX_STREAM_REASON_LEN + 1),
                },
            },
            ControlMsg::ServerLog {
                line: "x".repeat(MAX_SERVER_LOG_LINE + 1),
            },
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

    /// A datagram has to account for every byte in it. postcard's decoder stops
    /// at the end of the type it was asked for and says nothing about what
    /// follows, so without this the tail is dropped on the floor.
    #[test]
    fn receive_refuses_a_datagram_with_bytes_left_over() {
        let exact = encode(&CtlPacket {
            ack: 0,
            ack_bits: 0,
            frame: Some((0, chat(0))),
        });
        // A bare ack has a tail too: the frame Option's None tag ends it.
        let bare_ack = encode(&CtlPacket {
            ack: 1,
            ack_bits: 0,
            frame: None,
        });
        for good in [exact, bare_ack] {
            assert!(ControlLink::new().receive(&good).is_ok());
            for tail in [vec![0x00], vec![0x00; 96], vec![0xAB, 0xCD]] {
                let mut padded = good.clone();
                padded.extend_from_slice(&tail);
                let mut link = ControlLink::new();
                assert!(
                    link.receive(&padded).is_err(),
                    "receive accepted {} bytes past the packet",
                    tail.len()
                );
                assert_eq!(link.buffered(), 0);
                assert_eq!(link.pending_len(), 0);
            }
        }
    }

    /// The reverse of `a_member_without_the_trailing_fields_does_not_decode`: a
    /// peer whose encoding has grown a field sends bytes this build cannot
    /// account for, and refusing them is what keeps the addition from being
    /// read as the message without it. The bytes are built by appending the new
    /// field to a real datagram, which is exactly what that peer puts on the
    /// wire, because the message a frame carries is the last thing a
    /// `CtlPacket` encodes.
    #[test]
    fn receive_refuses_a_frame_whose_encoding_grew_a_field() {
        let grown_field = postcard::to_allocvec(&0.5f32).unwrap();
        let mut dgram = encode(&CtlPacket {
            ack: 0,
            ack_bits: 0,
            frame: Some((0, ControlMsg::HearSelf { enabled: true })),
        });
        dgram.extend_from_slice(&grown_field);
        let mut link = ControlLink::new();
        assert!(link.receive(&dgram).is_err());
        assert_eq!(link.buffered(), 0);
        // And the same datagram without the addition is the message it always
        // was, so the refusal is the added field and nothing else.
        let exact = &dgram[..dgram.len() - grown_field.len()];
        assert_eq!(
            ControlLink::new().receive(exact).unwrap(),
            vec![ControlMsg::HearSelf { enabled: true }]
        );
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
