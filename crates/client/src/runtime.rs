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

pub use jamstream_protocol::control::{
    BroadcastReadiness, DestinationState, StreamKey, StreamPlatform,
};
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
    /// Any member: while on, your personal mix includes your own signal
    /// instead of the usual removal.
    SetHearSelf(bool),
    /// Your own avatar: raw file bytes as read from disk, or None to drop
    /// it. The runtime hashes, validates, and announces; the UI never sees
    /// a hash. Bytes past the transfer cap are refused with a log line, the
    /// same way the settings sheet refuses them before sending.
    SetOwnAvatar(Option<Vec<u8>>),
    /// Your own display name, replacing the invite's hint or the member-N
    /// fallback on everyone's roster. Sent at join with whatever the join
    /// screen carried, and validated where the wire's cap lives.
    SetOwnName(String),
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
    /// Host only: begin a take on the session server.
    StartRecord,
    /// Host only: end the take. The upload may drain afterwards; the state
    /// in [`Snapshot::record`] says when it is done.
    StopRecord,
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
    /// The server has heard nothing from this member lately and has not yet
    /// given up on them. Only ever true alongside `connected`: the roster
    /// clears it on disconnect, so gone is never also quiet.
    pub quiet: bool,
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
    /// Every seat for this invite's role is taken. Not a dead end: the
    /// client keeps trying, because a seat frees when somebody leaves, so
    /// this reads as waiting rather than as a failure.
    SessionFull,
    /// No session; also the state after a clean leave.
    Idle,
}

/// How the device stream is talking to the endpoint, for the latency
/// readout's hover. The runtime's copy of `jamstream_audio_io::DeviceMode`,
/// so the UI contract stays free of the audio crate and a fixture can pin
/// either answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceModeView {
    /// WASAPI exclusive: the device is this session's alone, about 10 ms.
    Exclusive,
    /// Shared with the system mixer, 20 to 30 ms.
    Shared,
}

/// What is wrong with this computer's audio stream. A stream that is open is
/// no fault and neither is a reopen for a pick somebody just made, so both
/// read as `None`: only a stream that stopped on its own earns one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFaultView {
    /// The stream stopped and the reopen cadence is still working on it.
    Retrying,
    /// The cadence spent its budget, so nothing reopens without a pick.
    /// `tries` is how many attempts it really made.
    GaveUp { tries: u32 },
}

/// How one direction of the device stream reached the 48 kHz session rate.
/// The runtime's copy of `jamstream_audio_io::RateOutcome`, for the same
/// reason as [`DeviceModeView`]: the UI contract stays free of the audio
/// crate and a fixture can pin any rung.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateOutcomeView {
    /// The device runs at the session rate on its own; not news.
    Native,
    /// This app moved the device clock there, away from `from`.
    ClockSet { from: u32 },
    /// The OS converts between the stream and the device's own rate.
    OsConverted { device: u32 },
    /// The boundary converter carries the difference, at `added_ms`.
    Resampled { device: u32, added_ms: f32 },
}

impl RateOutcomeView {
    /// The sentence this outcome earns on the latency hover and the devices
    /// sheet, or `None` for rung 1: native is not news. `side` is "capture"
    /// or "playback".
    #[must_use]
    pub fn line(&self, side: &str) -> Option<String> {
        let session = khz(jamstream_protocol::SAMPLE_RATE);
        match *self {
            RateOutcomeView::Native => None,
            RateOutcomeView::ClockSet { from } => Some(format!(
                "moved the {side} device to {session} kHz (was {})",
                khz(from)
            )),
            RateOutcomeView::OsConverted { device } => Some(format!(
                "the OS is converting {side} to this device's {} kHz",
                khz(device)
            )),
            RateOutcomeView::Resampled { device, added_ms } => Some(format!(
                "converting {side} {} kHz to {session} kHz (+{added_ms:.1} ms)",
                khz(device)
            )),
        }
    }
}

/// Both directions' rate outcomes, as the stream that is running got them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateOutcomesView {
    pub capture: RateOutcomeView,
    pub playback: RateOutcomeView,
}

impl RateOutcomesView {
    /// What the boundary converter adds to mouth to ear, both directions
    /// summed; zero when nothing resamples.
    #[must_use]
    pub fn added_ms(&self) -> f32 {
        [self.capture, self.playback]
            .iter()
            .map(|o| match o {
                RateOutcomeView::Resampled { added_ms, .. } => *added_ms,
                _ => 0.0,
            })
            .sum()
    }

    /// The device rate behind the status bar's converting tag: the rate of a
    /// direction on the boundary converter, `None` while nothing resamples.
    #[must_use]
    pub fn resampled_rate(&self) -> Option<u32> {
        [self.capture, self.playback].iter().find_map(|o| match o {
            RateOutcomeView::Resampled { device, .. } => Some(*device),
            _ => None,
        })
    }

    /// Each direction's disclosure line, capture first; empty when both
    /// directions are native.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        [("capture", self.capture), ("playback", self.playback)]
            .iter()
            .filter_map(|(side, outcome)| outcome.line(side))
            .collect()
    }
}

/// A sample rate in kHz for UI copy: 44100 reads "44.1", 48000 reads "48".
#[must_use]
pub fn khz(rate: u32) -> String {
    format!("{}", f64::from(rate) / 1000.0)
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatsView {
    pub state: ConnState,
    pub rtt_ms: Option<f32>,
    /// Jitter buffer depth and target, in 2.5 ms frames.
    pub jitter_depth: usize,
    pub jitter_target: usize,
    /// This client's uplink loss over the last second, as the server measures
    /// it: the audio the band is not hearing, and the direction nothing on
    /// this machine can see. `None` until the first report arrives.
    pub uplink_loss_pct: Option<f32>,
    /// The local jitter buffer's loss over a window of the same length: the
    /// audio this machine is not playing. A rate, so it comes back down once
    /// the bad moment passes; `None` until the first window closes.
    pub downlink_loss_pct: Option<f32>,
    /// The headline number: capture to playout, end to end. Includes what
    /// the boundary converter discloses when a direction resamples.
    pub mouth_to_ear_ms: Option<f32>,
    /// Which sharing mode the device stream got. `None` before a stream
    /// opens and on platforms with no shared/exclusive split, which is why
    /// the readout says nothing rather than inventing an answer.
    pub device_mode: Option<DeviceModeView>,
    /// How each direction reached the session rate: the device's own
    /// clock, a clock this app moved, an OS converter, or the boundary
    /// resampler with its cost. `None` while there is no stream.
    pub rate: Option<RateOutcomesView>,
    /// Whether the playout ring is in a stretch dense enough with
    /// underruns to have been heard, for as long as that stretch holds:
    /// read like connection state, not like a one-shot event that may
    /// already have scrolled past by the time somebody looks up.
    pub crackling: bool,
    /// Closest the playout ring came to empty over the last second, in frames.
    /// The ring's own capacity is the ceiling, and zero means it emptied and
    /// the device played silence. `None` while no stream is rendering.
    pub playout_low_frames: Option<usize>,
    /// How the thread that fills the playout ring is being scheduled, over the
    /// last window. `None` until the first window closes.
    pub wake: Option<WakeView>,
    /// How many times the audio stream has stopped on its own, while those
    /// stops are close enough together to call the device unreliable; `None`
    /// while it is holding. A stop the next tick reopens leaves
    /// [`Snapshot::audio_fault`] clear before any frame draws it, so this is
    /// the only place a device that keeps failing shows at all.
    pub cutting_out: Option<u64>,
}

/// Wakeup-to-wakeup pacing of the thread that fills the playout ring, in
/// milliseconds. The device drains that ring on a clock of its own, so an
/// interval longer than the ring holds is silence the device padded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WakeView {
    /// The 99th percentile interval, taken from tick-wide buckets rather than
    /// from every interval the window held: it is the top of the bucket the
    /// 99th of a hundred fell in, so it reads high by up to one tick and is an
    /// estimate rather than an exact percentile.
    pub p99_ms: f32,
    /// The window's longest interval, which is exact.
    pub max_ms: f32,
}

impl StatsView {
    /// The worse of the two directions, for anything showing a level rather
    /// than a direction. Both are rates over the same window, so the larger of
    /// them is a quantity; `None` while neither direction has a figure yet.
    #[must_use]
    pub fn worst_loss_pct(&self) -> Option<f32> {
        match (self.uplink_loss_pct, self.downlink_loss_pct) {
            (Some(up), Some(down)) => Some(up.max(down)),
            (up, down) => up.or(down),
        }
    }

    /// Each direction's loss, uplink first, in the words that say whose sound
    /// it is. Written once and read by every surface that carries the figures,
    /// because the two directions mean opposite things and a reader who cannot
    /// tell them apart cannot act on either.
    #[must_use]
    pub fn loss_lines(&self) -> [String; 2] {
        [
            loss_line(
                self.uplink_loss_pct,
                "uplink",
                "what the band misses of you",
            ),
            loss_line(self.downlink_loss_pct, "downlink", "what you miss of them"),
        ]
    }
}

fn loss_line(pct: Option<f32>, direction: &str, whose: &str) -> String {
    let figure = pct.map_or("--".to_owned(), |p| format!("{p:.1}"));
    format!("{direction} loss {figure}%, {whose}")
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
    /// Frames the encoder's queue refused, cumulative and pipeline-wide.
    /// Genuine loss: the video timeline is this many pictures short of its
    /// audio.
    pub dropped_frames: u64,
    /// Catch-up frames the renderer had no time to draw, cumulative and
    /// pipeline-wide. Delivered again as the last picture, so nothing is
    /// missing and the audio stays in step; the cost is a stutter.
    pub repeated_frames: u64,
}

/// The recorder as the server last reported it, for the record lamp and
/// sheet. Default is idle with stems unknown-off, which is also what a
/// session that never records shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordView {
    pub state: RecordState,
    pub stems: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum RecordState {
    #[default]
    Idle,
    Recording,
    Uploading,
    Failed {
        reason: String,
    },
}

/// Where the broadcast is going, as everyone in the room sees it. Empty
/// until the server reports a destination, which is also how "nothing
/// configured" reads.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StreamView {
    pub destinations: Vec<DestinationView>,
    /// Whether this session can broadcast at all, as the server's relay probe
    /// answers it. None means it has not answered, which reads as "assume it
    /// works": before the first probe, and on a session that predates the
    /// probe, dimming Go Live would refuse a broadcast the session can serve.
    pub readiness: Option<BroadcastReadiness>,
}

impl StreamView {
    /// Why this session cannot stream, when it cannot. `None` covers both a
    /// working relay and no answer yet, because the tab treats them the same.
    pub fn unavailable_reason(&self) -> Option<&str> {
        match &self.readiness {
            Some(BroadcastReadiness::Unavailable { reason }) => Some(reason),
            _ => None,
        }
    }

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
    /// Whether the session is being recorded. Not host-only for the same
    /// reason as the on-air lamp: everyone in the room is on the take.
    pub record: RecordView,
    pub cost: Option<CostView>,
    /// Whether your personal mix includes your own signal. Client-local
    /// optimistic state: the server sends no echo.
    pub hear_self: bool,
    /// Whether the latency has sat far enough above the band an ensemble holds
    /// together in, for long enough, that the other arrangement is worth
    /// offering. A condition rather than an event, and it stands once it is
    /// out: the person it is for is playing, not reading the screen. Off for
    /// good once the control has been used either way.
    pub offer_hear_self: bool,
    /// First 8 hex characters of the session id.
    pub session_short: String,
    pub server_addr: String,
    pub is_host: bool,
    /// Why this computer has no audio stream, in the device's own words, for
    /// as long as it has none. A device swapped mid-song and refused leaves
    /// the session up and the musician silent, so the reason belongs on
    /// screen rather than only in the log.
    pub device_error: Option<String>,
    /// What the audio stream is doing wrong, for as long as it is: a state
    /// the status bar and the Audio tab read like the connection state, not
    /// an event, because somebody playing an instrument is not looking at the
    /// screen at the moment their device dies.
    pub audio_fault: Option<AudioFaultView>,
}

/// One snapshot pull per frame, commands fire and forget. Implementations
/// must be cheap to call from the paint thread.
pub trait Runtime: Send {
    fn snapshot(&self) -> Snapshot;
    fn send(&self, cmd: Command);

    /// The connection state alone, for the frame loop's "has this session
    /// ended" check. The default answer is the snapshot's, so an
    /// implementation is free to ignore this; one whose snapshot copies a
    /// roster and a chat buffer should not, because this is asked every
    /// frame and the answer is one enum.
    fn conn_state(&self) -> ConnState {
        self.snapshot().stats.state
    }
}
