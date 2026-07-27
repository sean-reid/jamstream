//! The contract between the UI and whatever runs the session. The UI pulls
//! one [`Snapshot`] per frame and pushes [`Command`]s; it never sees sockets,
//! devices, or threads. Pass 2 implements this trait over the real client
//! audio path (rtrb rings, `ClientCore`, triple-buffered stats); pass 1
//! ships [`crate::demo::DemoRuntime`].
//!
//! Per-member level meters need protocol support (the Stats control message
//! follow-up); v1 shows your input and the room output only, which is why
//! [`LevelsView`] has exactly those four values.

pub use jamstream_protocol::ids::{MemberId, Role, TokenId};

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
