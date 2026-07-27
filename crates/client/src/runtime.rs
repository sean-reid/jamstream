//! The contract between the UI and whatever runs the session. The UI pulls
//! one [`Snapshot`] per frame and pushes [`Command`]s; it never sees sockets,
//! devices, or threads. Pass 2 implements this trait over the real client
//! audio path (rtrb rings, `ClientCore`, triple-buffered stats); pass 1
//! ships [`crate::demo::DemoRuntime`].
//!
//! Per-member level meters need protocol support (the Stats control message
//! follow-up); v1 shows your input and the room output only, which is why
//! [`LevelsView`] has exactly those four values.

use std::sync::Arc;

pub use jamstream_protocol::control::{DestinationState, StreamKey, StreamPlatform};
pub use jamstream_protocol::ids::{DestinationId, MemberId, Role, TokenId};

/// One member's avatar, decoded. The UI needs pixels, not a file: the
/// runtime decodes each content hash exactly once and hands out clones of
/// this handle, and the UI uploads one egui texture per `hash`.
///
/// `rgba` is straight (non-premultiplied) RGBA, `width * height * 4` bytes.
#[derive(Clone)]
pub struct AvatarHandle {
    /// Lowercase hex of the avatar's Blake2s-256, its identity everywhere:
    /// the decode cache key and the egui texture key.
    pub hash: String,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

/// Content-addressed: equal hashes are the same pixels, so snapshot
/// comparison never walks the buffers.
impl PartialEq for AvatarHandle {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.width == other.width && self.height == other.height
    }
}

impl std::fmt::Debug for AvatarHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AvatarHandle")
            .field("hash", &self.hash)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgba", &format_args!("{} bytes", self.rgba.len()))
            .finish()
    }
}

/// Everything the UI can ask the runtime to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Shapes your personal monitor mix of one member.
    SetFader {
        member: MemberId,
        gain_db: f32,
        pan: f32,
        muted: bool,
    },
    /// Whether you hear the metronome click.
    SetClick(bool),
    /// Host only: the session-wide metronome.
    SetMetronome {
        bpm: u16,
        beats_per_bar: u8,
        enabled: bool,
    },
    SendChat(String),
    Leave,
    /// Host only: invalidates one invite and ejects its member.
    Revoke(TokenId),
    /// Host only: shapes one member's fader in the broadcast mix, the one
    /// listeners and the stream hear. Monitor mixes are unaffected.
    SetBroadcastFader {
        member: MemberId,
        gain_db: f32,
        pan: f32,
        muted: bool,
    },
    /// Host only: while on, the host's monitor carries the exact
    /// post-limiter listener signal, own voice included.
    SetBroadcastAudition(bool),
    /// Your own avatar: raw file bytes as read from disk, or None to drop
    /// it. The runtime hashes, validates, and announces; the UI never sees
    /// a hash. Bytes past the transfer cap are refused with a log line, the
    /// same way the settings sheet refuses them before sending.
    SetOwnAvatar(Option<Vec<u8>>),
    /// Host only: configure one broadcast destination. The id is minted on
    /// this side so add and remove name the same destination with no round
    /// trip. The only command that carries a secret: [`StreamKey`] redacts
    /// its own `Debug` and wipes on drop.
    AddDestination {
        id: DestinationId,
        platform: StreamPlatform,
        key: StreamKey,
    },
    /// Host only: drop one destination. Live or not, the others carry on.
    RemoveDestination(DestinationId),
    /// Host only: bring the encoder up. Destinations configured before or
    /// after both apply.
    StartStream,
    /// Host only: tear the encoder and every pusher down.
    StopStream,
}

/// Your monitor-mix settings for one member.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FaderView {
    pub gain_db: f32,
    /// -1 full left, 0 center, 1 full right.
    pub pan: f32,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemberView {
    pub id: MemberId,
    pub name: String,
    pub role: Role,
    pub connected: bool,
    pub is_you: bool,
    pub fader: FaderView,
    /// The invite token admitting this member; what [`Command::Revoke`]
    /// takes. Present only in host snapshots.
    pub token: Option<TokenId>,
    /// Decoded avatar, once its bytes have arrived and decoded. None covers
    /// both "no avatar set" and "the roster announced a hash whose bytes are
    /// still in flight"; the UI shows the initials disc for both, and
    /// swapping in the picture must not move anything.
    pub avatar: Option<AvatarHandle>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatLine {
    pub from_name: String,
    pub from_id: MemberId,
    pub text: String,
    /// Milliseconds since session start.
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnState {
    Connecting,
    Joined,
    Ejected(String),
    TimedOut,
    /// No session; also the state after a clean leave.
    Idle,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatsView {
    pub state: ConnState,
    pub rtt_ms: Option<f32>,
    /// Jitter buffer depth and target, in 2.5 ms frames.
    pub jitter_depth: usize,
    pub jitter_target: usize,
    pub loss_pct: f32,
    /// The headline number: capture to playout, end to end.
    pub mouth_to_ear_ms: Option<f32>,
}

/// Linear levels in 0..1. dB conversion is the meter widget's job.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LevelsView {
    pub input_peak: f32,
    pub input_rms: f32,
    pub output_peak: f32,
    pub output_rms: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetronomeView {
    pub bpm: u16,
    pub beats_per_bar: u8,
    pub enabled: bool,
    pub you_hear_click: bool,
}

/// Host only: the broadcast mix, as the server applies it before the
/// limiter. Fader entries follow roster order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BroadcastView {
    pub faders: Vec<(MemberId, FaderView)>,
    /// Whether the host is auditioning the stream mix in their monitor.
    /// Client-local optimistic state: the server sends no audition echo.
    pub audition: bool,
}

/// One broadcast destination as the server reports it. Key-free by
/// construction: the wire status carries no key, so neither does this, and
/// every member gets the same view.
#[derive(Debug, Clone, PartialEq)]
pub struct DestinationView {
    pub id: DestinationId,
    pub platform: StreamPlatform,
    pub state: DestinationState,
    /// Video plus audio bitrate the encoder is configured for. One encode
    /// feeds every destination, so it is the same number on each.
    pub bitrate_kbps: u32,
    /// Frames the pipeline could not hand the encoder in time, cumulative.
    pub dropped_frames: u64,
}

/// Where the broadcast is going, as everyone in the room sees it. Empty
/// until the server reports a destination, which is also how "nothing
/// configured" reads.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StreamView {
    pub destinations: Vec<DestinationView>,
}

impl StreamView {
    /// On air: at least one destination is actually being watched. Idle and
    /// connecting destinations are not on air, and a failed one is the
    /// opposite of on air.
    pub fn on_air(&self) -> bool {
        self.destinations
            .iter()
            .any(|d| d.state == DestinationState::Live)
    }

    pub fn live_count(&self) -> usize {
        self.destinations
            .iter()
            .filter(|d| d.state == DestinationState::Live)
            .count()
    }

    pub fn failed_count(&self) -> usize {
        self.destinations
            .iter()
            .filter(|d| matches!(d.state, DestinationState::Failed { .. }))
            .count()
    }

    pub fn get(&self, id: DestinationId) -> Option<&DestinationView> {
        self.destinations.iter().find(|d| d.id == id)
    }

    pub fn of_platform(&self, platform: StreamPlatform) -> Option<&DestinationView> {
        self.destinations.iter().find(|d| d.platform == platform)
    }
}

/// Host only: the running cost of the session VM.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostView {
    pub hourly_microusd: u64,
    pub accrued_microusd: u64,
    pub elapsed_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub stats: StatsView,
    pub members: Vec<MemberView>,
    pub chat: Vec<ChatLine>,
    pub levels: LevelsView,
    pub metronome: MetronomeView,
    /// The broadcast mix; None for everyone but the host.
    pub broadcast: Option<BroadcastView>,
    /// Where the broadcast is going. Not host-only: the on-air lamp is for
    /// everyone in the room, because everyone in it is being broadcast.
    pub stream: StreamView,
    pub cost: Option<CostView>,
    /// First 8 hex characters of the session id.
    pub session_short: String,
    pub server_addr: String,
    pub is_host: bool,
}

/// One snapshot pull per frame, commands fire and forget. Implementations
/// must be cheap to call from the paint thread.
pub trait Runtime: Send {
    fn snapshot(&self) -> Snapshot;
    fn send(&self, cmd: Command);
}
