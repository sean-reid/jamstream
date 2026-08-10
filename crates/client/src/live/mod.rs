//! The production [`Runtime`]: real audio devices through a
//! [`CallbackBridge`], a nonblocking UDP socket, and [`ClientCore`] driven
//! by a dedicated network thread.
//!
//! Thread layout: device callbacks (RT, allocation-free) exchange samples
//! with the network thread over the bridge's SPSC rings; the network thread
//! owns the socket, the core, and the audio stream lifecycle, and publishes
//! UI state into a `Mutex<SharedState>` the paint thread reads once per
//! frame. Loop cadence is ~2.5 ms with sleep-until pacing; the sample counts
//! are driven by the device clock rather than the loop clock, so the cadence is
//! forgiving up to the depth of the playout ring, and a wakeup later than that
//! is silence the device padded. The thread asks the platform for the class the
//! device callbacks already run at, and times itself against that ring.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use data_encoding::HEXLOWER;
use jamstream_audio_io::{
    AudioBackend, AudioError, AudioPriority, CallbackBridge, DuplexHandler, EngineSide,
    StreamConfig, StreamHandle, ThreadPriority, WavBackend, WavStream, playout_cushion_samples,
};
use jamstream_engine::JitterBuffer;
use jamstream_protocol::control::{MAX_DATAGRAM_BYTES, MemberInfo, StreamOp};
use jamstream_protocol::control::{RecordOp, RecordingState};
use jamstream_protocol::ids::HOST_MEMBER_ID;
use jamstream_protocol::invite::Invite;
use jamstream_protocol::media::FrameDuration;
use jamstream_session::SessionError;
use jamstream_session::client::{ClientCore, ClientState, ClientStats};

use crate::avatar;
use crate::runtime::{
    AudioFaultView, AvatarHandle, BroadcastReadiness, BroadcastView, ChatLine, Command, ConnState,
    CostView, CushionView, DestinationView, DeviceBuffersView, FaderView, LevelsView, MemberId,
    MemberView, MetronomeView, RateOutcomeView, RateOutcomesView, RecordState, RecordView, Role,
    Runtime, Snapshot, StatsView, StreamView, WakeView, recording_or_on_air,
};
use crate::screens::invites::TokenMap;

mod watch;

use watch::{
    CUTTING_OUT_COUNT, CUTTING_OUT_WINDOW, CushionControl, DownlinkLoss, EpisodeWatch,
    HearSelfOffer, PlayoutWatch, ReopenEpisode, RingWatch, STREAM_SETTLED_AFTER, WakeWatch, as_ms,
};

/// The session rate, from the protocol rather than a second copy of 48000:
/// the device is opened at it, so a protocol that moved would otherwise
/// leave this side opening the wrong rate. The offline pump paces from the
/// device's own rate instead; see [`Driver::pump_one`].
const SAMPLE_RATE: u32 = jamstream_protocol::SAMPLE_RATE;
const CHANNELS: u16 = 2;
/// The pace this side loops at and the frame it sends, both belonging to
/// [`FrameDuration::Ms2_5`] and derived from its const fns, so both always
/// match the wire's own frame instead of a hand-computed copy of it.
const TICK: Duration = Duration::from_micros(FrameDuration::Ms2_5.micros() as u64);
/// One frame: 120 mono capture samples, 240 interleaved playout.
const FRAME_FRAMES: usize = FrameDuration::Ms2_5.samples() as usize;
const CHUNK_STEREO: usize = FRAME_FRAMES * 2;
const CHAT_LIMIT: usize = 500;
/// Meter fall per 2.5 ms tick; roughly a 170 ms half-life so levels look
/// alive at snapshot rate without flickering per packet.
const LEVEL_DECAY: f32 = 0.99;
/// Longest offline-pump stall replayed sample-for-sample, in seconds of
/// device time; two seconds is comfortably past the server jitter buffer's
/// 512-frame (1.28 s) stream-restart threshold, so an abandoned backlog
/// always trips it.
const PUMP_REPLAY_MAX_SECS: u64 = 2;
/// Audio the capture ring holds, which is how long the worker may be held up
/// before captured audio is dropped rather than delayed. Forty milliseconds
/// covers the session's own bring-up and a stalled tick, and a stall that long
/// replays as 16 frames arriving at once, well inside the receiving jitter
/// buffer's 64-frame queue. It costs nothing in latency: see
/// [`capture_capacity`].
const CAPTURE_RING: Duration = Duration::from_millis(40);
/// Deepest cushion the playout ring is cut to hold, which is what makes the
/// cushion a number that moves while the stream stays open: a bigger one needs
/// a bigger ring, and a bigger ring needs the device shut and reopened. The
/// jitter buffer's own ceiling of `MAX_TARGET_FRAMES` sets the figure. Playout
/// holding more audio than the network path ever asks for is latency spent
/// where a bigger device callback is the answer instead.
const PLAYOUT_CUSHION_MAX: Duration = Duration::from_micros(
    FrameDuration::Ms2_5.micros() as u64 * JitterBuffer::MAX_TARGET_FRAMES as u64,
);
/// Floor under [`playout_stall_after`], so a ring only a frame or two deep does
/// not read a scheduling hiccup as a device that has stopped rendering.
const PLAYOUT_STALL_FLOOR: Duration = Duration::from_millis(40);
/// Ceiling under [`playout_stall_after`]: half the arrivals a jitter buffer
/// holds before it gives its playout position up, so the drain always starts
/// while the audio a reopened stream would continue from is still there.
const PLAYOUT_STALL_CEILING: Duration = Duration::from_micros(
    FrameDuration::Ms2_5.micros() as u64 * JitterBuffer::MAX_DEPTH_FRAMES as u64 / 2,
);
/// Audio [`Worker::drain_stalled_playout`] catches up on in one pass. A worker
/// held up longer than this cannot pay the debt back at the frame clock, so it
/// abandons the gap: the jitter buffer treats a hole this size as a restart
/// anyway.
const PLAYOUT_DRAIN_MAX: Duration = Duration::from_millis(160);
/// A reopen slower than this is worth a warning of its own. A device shut and
/// reopened for a settings change costs a few hundred milliseconds of capture on
/// every platform measured; a whole second is something else going on.
const SLOW_REOPEN: Duration = Duration::from_secs(1);

/// Playout ring capacity in samples: the deepest cushion the ring can ever be
/// asked to hold, or the depth target itself where a device period is deeper
/// than that. Capacity costs memory and depth costs latency, so the ring is cut
/// once for the whole range and the cushion moves inside it with the stream
/// open.
fn playout_capacity(buffer_frames: u32) -> usize {
    let deepest = PLAYOUT_CUSHION_MAX.as_millis() as usize * SAMPLE_RATE as usize / 1000;
    playout_target(buffer_frames).max(deepest * usize::from(CHANNELS))
}

/// Playout depth in samples [`Worker::top_up_playout`] fills to, which is the
/// cushion the device plays out of. The depth itself is
/// [`playout_cushion_samples`], in the crate that owns the ring, because the
/// latency harness holds the same depth and its figures are this client's only
/// while the two are one number.
fn playout_target(buffer_frames: u32) -> usize {
    playout_cushion_samples(buffer_frames as usize * usize::from(CHANNELS), CHUNK_STEREO)
}

/// The cushion a stream at `buffer_frames` opens holding, for a caller with no
/// ring of its own: the controller's own opening state, so nothing can draw a
/// depth the app never opens at. Adjusting itself, which is the default the app
/// launches with.
pub(crate) fn opening_cushion(buffer_frames: u32) -> CushionView {
    CushionControl::new(buffer_frames, true).view()
}

/// A depth target as time, which is the audio the device drains while the
/// worker filling it is asleep. Takes the target rather than a device size,
/// because [`CushionControl`] moves the target while the stream stays open and
/// a deadline read off the device size would stand still under it.
fn cushion_time(target: usize) -> Duration {
    let frames = target / usize::from(CHANNELS);
    Duration::from_micros(frames as u64 * 1_000_000 / u64::from(SAMPLE_RATE))
}

/// The two device terms in the latency figure, priced off the cushion the buffer
/// control reports: the callback the device negotiated on the way in, and the
/// depth the top-up loop is filling to as of now on the way out, which is what a
/// sample queues behind. One source for both views, so the depth under the
/// buffer choices and the figure beside them can never be two numbers.
pub(crate) fn device_buffers(cushion: CushionView) -> DeviceBuffersView {
    DeviceBuffersView {
        capture_ms: cushion.callback_ms(),
        playout_ms: cushion.held_ms(),
    }
}

/// Capture ring capacity in samples: the playout cushion, or
/// [`CAPTURE_RING`] of audio, whichever is larger.
///
/// Deeper than the playout cushion because capture depth is not latency: the
/// worker drains this ring to empty every tick, so a sample waits for the next
/// 2.5 ms drain and never for the capacity. Capacity only buys how long the
/// worker may be held up before audio is destroyed, and the session's own
/// bring-up outlasts two callbacks of it.
fn capture_capacity(buffer_frames: u32) -> usize {
    let slack = CAPTURE_RING.as_millis() as usize * SAMPLE_RATE as usize / 1000;
    playout_target(buffer_frames).max(slack * usize::from(CHANNELS))
}

/// Pushes toward `depth` samples banked in the playout ring and no further;
/// returns how many of `samples` fit. The ring is cut for the deepest cushion
/// the app can hold, so what the device plays out of, and pays for in latency,
/// is this depth rather than the capacity.
fn fill_playout_to(engine: &mut EngineSide, samples: &[f32], depth: usize) -> usize {
    let room = depth.saturating_sub(engine.playout_depth());
    if room == 0 {
        return 0;
    }
    engine.push_playout(&samples[..room.min(samples.len())])
}

/// How long the playout ring may accept nothing before the device counts as
/// having stopped rendering: four device callbacks, since the top-up loop holds
/// the ring at its depth target and a rendering device makes room for one
/// callback on every callback. Clamped between [`PLAYOUT_STALL_FLOOR`] and
/// [`PLAYOUT_STALL_CEILING`].
fn playout_stall_after(buffer_frames: u32) -> Duration {
    let period = Duration::from_micros(
        u64::from(buffer_frames.max(FRAME_FRAMES as u32)) * 1_000_000 / u64::from(SAMPLE_RATE),
    );
    (period * 4).clamp(PLAYOUT_STALL_FLOOR, PLAYOUT_STALL_CEILING)
}

/// The device request as [`AudioSettings`] spells it: the session rate and
/// channel layout are the protocol's, the buffer and the exclusive answer are
/// the user's. One function so every open and reopen carries the whole of the
/// settings; a half-plumbed flag here would pass every test the fake sees.
fn stream_config(settings: &AudioSettings) -> StreamConfig {
    StreamConfig {
        sample_rate: SAMPLE_RATE,
        buffer_frames: settings.buffer_frames.max(32),
        channels: CHANNELS,
        allow_exclusive: settings.allow_exclusive,
    }
}

/// Frames the ring is sized from: the request, or the callback size the
/// device negotiated when that is bigger. A device is free to ignore the
/// request (WASAPI shared mode calls back at its period, ~480 frames against
/// the 120 default), and a ring sized from the request alone then underruns
/// on every render callback and drops the tail of every capture.
fn ring_frames(requested: u32, negotiated: Option<u32>) -> u32 {
    negotiated.unwrap_or(0).max(requested)
}

/// Device selection plus buffer size, as picked on the settings screen.
/// `None` device ids select the system default for that direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSettings {
    pub capture_id: Option<String>,
    pub playback_id: Option<String>,
    pub buffer_frames: u32,
    /// Whether the open may take the device exclusively (Windows only);
    /// rides [`StreamConfig::allow_exclusive`] to the backend on every open
    /// and reopen, so the toggle applies mid-session too.
    pub allow_exclusive: bool,
    /// Whether the playout cushion may move itself past what `buffer_frames`
    /// asks for. Off pins it there, which makes the pick the latency.
    pub auto_cushion: bool,
}

/// Manual rather than derived so the flags default on: exclusive is the
/// latency the product exists for, and a derived `false` here would quietly
/// contradict [`StreamConfig::default`]; a cushion nothing adjusts is a
/// dropout on every machine that needs the help.
impl Default for AudioSettings {
    fn default() -> Self {
        AudioSettings {
            capture_id: None,
            playback_id: None,
            buffer_frames: 0,
            allow_exclusive: true,
            auto_cushion: true,
        }
    }
}

impl AudioSettings {
    /// Whether moving from these settings to `next` needs the device shut and
    /// reopened, which costs the band a few hundred milliseconds of capture.
    /// Every field but the cushion answer is part of the device request; that
    /// one is read by the depth controller on the worker's own thread.
    ///
    /// Written as one comparison of the whole struct so a field added later
    /// counts as a device change until somebody says otherwise.
    pub fn reopens_for(&self, next: &AudioSettings) -> bool {
        *self
            != AudioSettings {
                auto_cushion: self.auto_cushion,
                ..next.clone()
            }
    }
}

#[derive(Debug)]
pub enum LiveError {
    Audio(AudioError),
    Session(SessionError),
    Io(std::io::Error),
    /// The invite carries no server address at all.
    NoAddress,
}

impl std::fmt::Display for LiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiveError::Audio(e) => write!(f, "audio: {e}"),
            LiveError::Session(e) => write!(f, "session: {e}"),
            LiveError::Io(e) => write!(f, "network: {e}"),
            LiveError::NoAddress => write!(f, "invite has no server address"),
        }
    }
}

impl std::error::Error for LiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LiveError::Audio(e) => Some(e),
            LiveError::Session(e) => Some(e),
            LiveError::Io(e) => Some(e),
            LiveError::NoAddress => None,
        }
    }
}

/// Everything the worker thread publishes for the paint thread.
struct SharedState {
    /// Every figure the snapshot carries, in the shape the UI reads it: one
    /// value rather than a field per measurement mirroring the view, so a
    /// figure is declared once and written where it is measured.
    ///
    /// [`StatsView::device_mode`] is the exception, read off the backend at
    /// snapshot time and never published here.
    stats: StatsView,
    roster: Vec<MemberInfo>,
    /// Monitor-mix values the UI set, merged over the roster; the server
    /// does not echo MixerSet back.
    faders: HashMap<MemberId, FaderView>,
    /// Broadcast-mix values, from our own optimistic sets and from
    /// BroadcastMixChanged relays; merged over the roster for the host.
    broadcast_faders: HashMap<MemberId, FaderView>,
    /// Client-local optimistic audition state; the server sends no echo.
    audition: bool,
    /// Client-local optimistic hear-self state; the server sends no echo.
    hear_self: bool,
    /// Whether [`HearSelfOffer`] has the offer standing. Rewritten every tick
    /// from the latency figure, so the Audio tab reads it the way it reads a
    /// crackling run.
    offer_hear_self: bool,
    /// Last `StreamStatus` the server sent, verbatim. Unlike the faders
    /// there is no optimistic copy: the pipeline's own view is the only
    /// honest one, and a destination that failed to come up must not read as
    /// live for even one frame.
    stream: Vec<DestinationView>,
    /// Whether the session can broadcast at all, as the server last said.
    /// None until it says, which reads as "assume it works".
    readiness: Option<BroadcastReadiness>,
    /// Last `RecordStatus` the server sent, verbatim, for the same reason:
    /// only the recorder knows whether a take is really being captured.
    record: RecordView,
    chat: VecDeque<ChatLine>,
    levels: LevelsView,
    metronome: MetronomeView,
    /// Decoded avatars by content hash (lowercase hex), the one decode per
    /// hash the UI draws from. A roster hash with no entry yet means the
    /// bytes are still in flight; that member keeps the initials disc.
    avatars: HashMap<String, AvatarHandle>,
    /// You dropped your own avatar. The wire has no way to unset one, so
    /// this hides it locally until the next join; the settings sheet says
    /// so in as many words.
    own_dropped: bool,
    me: Option<MemberId>,
    session_short: String,
    server_addr: String,
    /// Stream reopen attempts after device loss; surfaced in logs.
    reopen_attempts: u64,
    /// Why the audio stream will not open, verbatim, while it will not. Set
    /// on every failed open and cleared by the one that succeeds, so the UI
    /// reads it the way it reads the connection state.
    device_error: Option<String>,
    /// What the reopen cadence has to say about the audio stream, rewritten
    /// every tick from the worker's own state. The UI reads it the way it
    /// reads the connection state.
    audio_fault: Option<AudioFaultView>,
}

impl SharedState {
    fn new(invite: &Invite, server_addr: SocketAddr) -> Self {
        let session_short = HEXLOWER.encode(&invite.session_id.0[..4]);
        SharedState {
            stats: StatsView {
                state: ConnState::Connecting,
                ..StatsView::default()
            },
            roster: Vec::new(),
            faders: HashMap::new(),
            broadcast_faders: HashMap::new(),
            audition: false,
            hear_self: false,
            offer_hear_self: false,
            stream: Vec::new(),
            readiness: None,
            record: RecordView::default(),
            chat: VecDeque::new(),
            levels: LevelsView::default(),
            metronome: MetronomeView {
                bpm: 120,
                beats_per_bar: 4,
                enabled: false,
                you_hear_click: true,
            },
            avatars: HashMap::new(),
            own_dropped: false,
            me: None,
            session_short,
            server_addr: server_addr.to_string(),
            reopen_attempts: 0,
            device_error: None,
            audio_fault: None,
        }
    }

    fn push_chat(&mut self, line: ChatLine) {
        self.chat.push_back(line);
        while self.chat.len() > CHAT_LIMIT {
            self.chat.pop_front();
        }
    }
}

enum ThreadMsg {
    Cmd(Command),
    Reconfigure(AudioSettings),
    /// Whether the playout cushion may move itself. Its own message rather than
    /// a [`ThreadMsg::Reconfigure`], because the depth controller reads it
    /// without the device being touched.
    AutoCushion(bool),
}

/// The audio stream in whichever shape the backend produced it. Real
/// streams run on their own device threads and only need error polling;
/// the offline WAV stream has no clock, so the worker pumps it at the pace
/// wall time dictates.
enum Driver {
    Real {
        backend: Box<dyn AudioBackend>,
        handle: Option<Box<dyn StreamHandle>>,
    },
    Offline {
        backend: WavBackend,
        stream: Option<Box<WavStream>>,
        epoch: Instant,
        pumped_frames: u64,
    },
}

impl Driver {
    /// Opens a fresh bridge and stream for `settings`, replacing nothing:
    /// callers close the previous stream first so real backends never see
    /// two streams on one device.
    ///
    /// Returns the engine side, the frames the rings were sized from, and the
    /// stream's own rate-outcome report. The callback size is only knowable
    /// from an open stream, so the rings are first sized from the request;
    /// when the stream then reports callbacks they cannot absorb, it is
    /// reopened once over rings that can.
    ///
    /// The playout ring is filled with silence to its depth target before the
    /// stream opens, so the device's first callback finds it at its steady-state
    /// depth rather than empty. Refilling it from the core instead would
    /// burst-pull several frames in zero wall time, running the jitter consumer
    /// clock past the sender; the buffer can step back at most one frame, so
    /// every later packet would be dropped as late and playout would stay silent
    /// for the rest of the session.
    fn open(
        &mut self,
        settings: &AudioSettings,
    ) -> Result<(EngineSide, u32, Option<RateOutcomesView>), AudioError> {
        let config = stream_config(settings);
        let requested = config.buffer_frames;
        let mut frames = requested;
        let mut resized = false;
        loop {
            let (device, mut engine) =
                CallbackBridge::new(capture_capacity(frames), playout_capacity(frames));
            engine.push_playout(&vec![0.0; playout_target(frames)]);
            let (negotiated, rate) = self.open_stream(config, device.into_handler(), settings)?;
            let rate = rate.map(rate_view);
            let needed = ring_frames(requested, negotiated);
            if needed <= frames {
                return Ok((engine, frames, rate));
            }
            if resized {
                // Two different answers in a row; chasing it would reopen
                // forever, so keep this ring and let the bridge counters
                // say whether it falls short.
                tracing::warn!(
                    negotiated = needed,
                    ring = frames,
                    "device callback size will not settle; keeping the ring"
                );
                return Ok((engine, frames, rate));
            }
            tracing::info!(
                requested,
                negotiated = needed,
                "device delivers bigger callbacks than requested; resizing the ring"
            );
            self.close();
            frames = needed;
            resized = true;
        }
    }

    /// Opens the stream itself and reports the callback size the device
    /// negotiated and the rate outcomes it landed on, where the backend can
    /// say.
    fn open_stream(
        &mut self,
        config: StreamConfig,
        handler: DuplexHandler,
        settings: &AudioSettings,
    ) -> Result<(Option<u32>, Option<jamstream_audio_io::RateOutcomes>), AudioError> {
        match self {
            Driver::Real { backend, handle } => {
                let new = backend.open_duplex(
                    settings.capture_id.as_deref(),
                    settings.playback_id.as_deref(),
                    config,
                    handler,
                )?;
                let report = (new.buffer_frames(), new.rate_outcomes());
                *handle = Some(new);
                Ok(report)
            }
            Driver::Offline {
                backend,
                stream,
                epoch,
                pumped_frames,
            } => {
                let new = Box::new(backend.open_offline(config, handler)?);
                let report = (new.buffer_frames(), new.rate_outcomes());
                *stream = Some(new);
                *epoch = Instant::now();
                *pumped_frames = 0;
                Ok(report)
            }
        }
    }

    fn close(&mut self) {
        match self {
            Driver::Real { handle, .. } => {
                if let Some(h) = handle.take() {
                    h.close();
                }
            }
            Driver::Offline { stream, .. } => {
                if let Some(s) = stream.take()
                    && let Err(err) = s.finish()
                {
                    tracing::warn!(%err, "offline capture file failed to finalize");
                }
            }
        }
    }

    /// Whether the stream is dead and wants reopening.
    ///
    /// The offline arm answered a flat `false`, so `WavStream::errored` was
    /// never read and the device-gone path in [`Worker::check_stream`] was
    /// unreachable in every test: the only backend a test can drive could not
    /// report a lost device, and the only backend that could report one needs
    /// hardware to unplug.
    fn errored(&self) -> bool {
        match self {
            Driver::Real { handle, .. } => handle.as_ref().is_some_and(|h| h.errored()),
            Driver::Offline { stream, .. } => stream.as_ref().is_some_and(|s| s.errored()),
        }
    }

    /// Offline only: advance the WAV stream by at most one frame-sized bite
    /// (~2.5 ms) when wall time owes it one. Returns whether it pumped, so
    /// the worker can service the rings between bites; the rings are only a
    /// couple of device buffers deep, and pumping a whole catch-up burst
    /// against unserviced rings would play silence and drop capture.
    fn pump_one(&mut self) -> bool {
        let Driver::Offline {
            stream: Some(stream),
            epoch,
            pumped_frames,
            ..
        } = self
        else {
            return false;
        };
        // Pumped frames are device-rate frames, so the debt is counted on
        // the device's clock. Pacing a 44.1 kHz stream at SAMPLE_RATE would
        // run the offline uplink 8.8% fast, far past any compensator.
        let rate = stream.device_rate();
        let due = (epoch.elapsed().as_secs_f64() * f64::from(rate)) as u64;
        let backlog = due.saturating_sub(*pumped_frames);
        if backlog == 0 {
            return false;
        }
        // Catch-up is all or nothing. Replaying only part of a stall (the
        // old one-second cap) compressed the uplink frame clock by the
        // skipped amount, so every later frame reached the server a fixed
        // few hundred frames late: under its jitter buffer's stream-restart
        // threshold, and it can step back at most one frame, so the uplink
        // stayed concealed for the rest of the session. Short stalls replay
        // sample-for-sample; longer ones (a debugger pause) drop the whole
        // backlog, a discontinuity big enough to trip that stream-restart
        // reset and re-anchor cleanly.
        if backlog > PUMP_REPLAY_MAX_SECS * u64::from(rate) {
            *pumped_frames = due;
            return false;
        }
        let chunk = backlog.min(FRAME_FRAMES as u64);
        match stream.pump(chunk as usize) {
            Ok(()) => {
                *pumped_frames += chunk;
                true
            }
            Err(err) => {
                tracing::warn!(%err, "offline pump failed");
                false
            }
        }
    }
}

/// The production runtime. Construct with [`LiveRuntime::join`]; the UI
/// consumes it as a `Box<dyn Runtime>` (an `Arc<LiveRuntime>` implements
/// the trait too, so the app can keep a concrete handle for
/// [`reconfigure_audio`](Self::reconfigure_audio)).
pub struct LiveRuntime {
    shared: Arc<Mutex<SharedState>>,
    tx: Sender<ThreadMsg>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl LiveRuntime {
    /// Opens the duplex stream, connects to the invite's first address, and
    /// spawns the network thread. Returns once the thread is running;
    /// joining continues asynchronously (snapshots show Connecting, then
    /// Joined).
    pub fn join(
        invite: &Invite,
        settings: AudioSettings,
        backend: Box<dyn AudioBackend>,
    ) -> Result<LiveRuntime, LiveError> {
        Self::start(
            invite,
            settings,
            Driver::Real {
                backend,
                handle: None,
            },
        )
    }

    /// Same runtime over the offline WAV backend: the network thread pumps
    /// the stream itself at wall-clock pace. Used by tests and headless
    /// runs; no sound card involved.
    pub fn join_offline(
        invite: &Invite,
        settings: AudioSettings,
        backend: WavBackend,
    ) -> Result<LiveRuntime, LiveError> {
        Self::start(
            invite,
            settings,
            Driver::Offline {
                backend,
                stream: None,
                epoch: Instant::now(),
                pumped_frames: 0,
            },
        )
    }

    fn start(
        invite: &Invite,
        settings: AudioSettings,
        mut driver: Driver,
    ) -> Result<LiveRuntime, LiveError> {
        let addr = *invite.addresses.first().ok_or(LiveError::NoAddress)?;
        // Everything that can be done before the device starts is done before
        // the device starts. Capture flows from the moment the stream opens,
        // into a ring whose only consumer is the worker thread below, so any
        // work between those two points is audio at risk. The join
        // datagram waits for the open to succeed: a failed open leaves no
        // half-joined member on the server.
        let socket = connect_socket(addr).map_err(LiveError::Io)?;
        let (core, init) = ClientCore::connect(invite, 0).map_err(LiveError::Session)?;
        let (engine, device_frames, rate) = driver.open(&settings).map_err(LiveError::Audio)?;
        let _ = socket.send(&init);

        let mut state = SharedState::new(invite, addr);
        // The rung each direction landed on, which the status bar's tag and
        // the Audio tab read for as long as the stream runs. Logged here as
        // well, so the file carries what the session started on.
        state.stats.rate = rate;
        if let Some(rate) = rate {
            for (side, outcome) in [("capture", rate.capture), ("playback", rate.playback)] {
                log_rate_change(None, outcome, side);
            }
        }
        let shared = Arc::new(Mutex::new(state));
        let (tx, rx) = mpsc::channel();
        let worker = Worker {
            core,
            socket,
            addresses: invite.addresses.clone(),
            addr_idx: 0,
            ever_joined: false,
            driver,
            engine: Some(engine),
            device_frames,
            cushion: CushionControl::new(device_frames, settings.auto_cushion),
            rings: RingWatch::new(Instant::now()),
            wake: WakeWatch::new(Instant::now()),
            playout: PlayoutWatch::default(),
            downlink: DownlinkLoss::default(),
            settings,
            shared: Arc::clone(&shared),
            rx,
            rx_buf: vec![0u8; MAX_DATAGRAM_BYTES].into_boxed_slice(),
            epoch: Instant::now(),
            capture_buf: vec![0.0; capture_capacity(device_frames)],
            mono_buf: Vec::new(),
            shut_at: None,
            moved_since_reopen: None,
            sent_since_reopen: None,
            send_errors: 0,
            carry: [0.0; CHUNK_STEREO],
            carry_pos: 0,
            carry_len: 0,
            ring_took: Instant::now(),
            drain_from: None,
            levels: LevelsView::default(),
            avatar_failed: HashSet::new(),
            reopen_attempts: 0,
            last_reopen: None,
            opened_at: Some(Instant::now()),
            episode: ReopenEpisode::default(),
            device_stops: 0,
            cutting_out: EpisodeWatch::new(CUTTING_OUT_COUNT, CUTTING_OUT_WINDOW),
            was_cutting_out: false,
            announced_rate: rate,
            priority: ThreadPriority::Unchanged,
            hear_self_offer: HearSelfOffer::default(),
        };
        let handle = std::thread::Builder::new()
            .name("jamstream-net".into())
            .spawn(move || worker.run())
            .map_err(LiveError::Io)?;
        Ok(LiveRuntime {
            shared,
            tx,
            worker: Mutex::new(Some(handle)),
        })
    }

    /// Closes the audio stream and reopens it with `settings` between two
    /// network-loop iterations; the session itself is untouched. The bridge
    /// is recreated and the ring endpoints swapped atomically from the
    /// worker's point of view (it is the only engine-side consumer).
    pub fn reconfigure_audio(&self, settings: AudioSettings) {
        let _ = self.tx.send(ThreadMsg::Reconfigure(settings));
    }

    /// Frees the playout cushion to move itself, or pins it at what the buffer
    /// size asks for. The device is left alone: this is a depth the top-up loop
    /// fills to, so pinning it drains the ring rather than reopening anything.
    pub fn set_auto_cushion(&self, auto: bool) {
        let _ = self.tx.send(ThreadMsg::AutoCushion(auto));
    }

    /// True once the network thread has exited (after a Leave).
    pub fn finished(&self) -> bool {
        self.worker
            .lock()
            .expect("live worker handle")
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
    }

    /// The connection state without a snapshot behind it. The frame loop
    /// asks every frame only to see whether the session has ended, and
    /// [`Self::snapshot_now`] would copy the roster, the chat buffer, and
    /// the destinations to answer it.
    fn conn_now(&self) -> ConnState {
        self.shared.lock().expect("live state").stats.state.clone()
    }

    fn snapshot_now(&self) -> Snapshot {
        let s = self.shared.lock().expect("live state");
        let members = s
            .roster
            .iter()
            .map(|m| MemberView {
                id: m.id,
                name: m.name.clone(),
                role: m.role,
                connected: m.connected,
                quiet: m.quiet,
                is_you: s.me == Some(m.id),
                fader: s.faders.get(&m.id).copied().unwrap_or(FaderView {
                    gain_db: 0.0,
                    pan: 0.0,
                    muted: false,
                }),
                // The wire roster carries no token ids; the wizard's
                // [`CostedRuntime`] wrapper injects them from the host's
                // invite book. None hides the revoke buttons.
                token: None,
                // Present once the bytes for the roster's hash have arrived
                // and decoded; None until then, and None for your own after
                // you drop it.
                avatar: match m.avatar_hash {
                    Some(_) if s.me == Some(m.id) && s.own_dropped => None,
                    Some(hash) => s.avatars.get(&avatar::hash_hex(&hash)).cloned(),
                    None => None,
                },
            })
            .collect();
        let is_host = s.me == Some(HOST_MEMBER_ID);
        let broadcast = is_host.then(|| BroadcastView {
            faders: s
                .roster
                .iter()
                .filter(|m| m.role == Role::Musician)
                .map(|m| {
                    (
                        m.id,
                        s.broadcast_faders.get(&m.id).copied().unwrap_or(FaderView {
                            gain_db: 0.0,
                            pan: 0.0,
                            muted: false,
                        }),
                    )
                })
                .collect(),
            audition: s.audition,
        });
        Snapshot {
            stats: StatsView {
                // Straight off the backend's own report at read time: there
                // is one device stream per process and it follows the last
                // open, so no worker plumbing could say anything truer.
                device_mode: match jamstream_audio_io::active_device_mode() {
                    Some(jamstream_audio_io::DeviceMode::Exclusive) => {
                        Some(crate::runtime::DeviceModeView::Exclusive)
                    }
                    Some(jamstream_audio_io::DeviceMode::Shared) => {
                        Some(crate::runtime::DeviceModeView::Shared)
                    }
                    None => None,
                },
                ..s.stats.clone()
            },
            members,
            chat: s.chat.iter().cloned().collect(),
            levels: s.levels,
            metronome: s.metronome,
            broadcast,
            stream: StreamView {
                destinations: s.stream.clone(),
                readiness: s.readiness.clone(),
            },
            record: s.record.clone(),
            // The wizard's [`CostedRuntime`] wrapper fills this for
            // sessions this app launched; plain joins have no meter.
            cost: None,
            hear_self: s.hear_self,
            offer_hear_self: s.offer_hear_self,
            session_short: s.session_short.clone(),
            server_addr: s.server_addr.clone(),
            is_host,
            device_error: s.device_error.clone(),
            audio_fault: s.audio_fault,
        }
    }

    fn send_cmd(&self, cmd: Command) {
        // Optimistic local state: faders and the click toggle reflect in
        // the next snapshot instead of after a worker round trip. The
        // server does not echo MixerSet, so this is also the only record.
        {
            let mut s = self.shared.lock().expect("live state");
            match &cmd {
                Command::SetFader {
                    member,
                    gain_db,
                    pan,
                    muted,
                } => {
                    s.faders.insert(
                        *member,
                        FaderView {
                            gain_db: *gain_db,
                            pan: *pan,
                            muted: *muted,
                        },
                    );
                }
                Command::SetClick(on) => s.metronome.you_hear_click = *on,
                Command::SetMetronome {
                    bpm,
                    beats_per_bar,
                    enabled,
                } => {
                    s.metronome.bpm = *bpm;
                    s.metronome.beats_per_bar = *beats_per_bar;
                    s.metronome.enabled = *enabled;
                }
                Command::SetBroadcastFader {
                    member,
                    gain_db,
                    pan,
                    muted,
                } => {
                    s.broadcast_faders.insert(
                        *member,
                        FaderView {
                            gain_db: *gain_db,
                            pan: *pan,
                            muted: *muted,
                        },
                    );
                }
                Command::SetBroadcastAudition(on) => s.audition = *on,
                Command::SetHearSelf(on) => s.hear_self = *on,
                // Your own picture comes back through the roster like
                // anyone else's; dropping it can only be local.
                Command::SetOwnAvatar(bytes) => s.own_dropped = bytes.is_none(),
                // Stream state gets no optimistic echo on purpose: the
                // pipeline is what decides whether a destination is live,
                // and it says so within a second.
                // The name included: the roster fanout is the echo, and it
                // arrives within a tick.
                Command::SendChat(_)
                | Command::Leave
                | Command::Revoke(_)
                | Command::SetOwnName(_)
                | Command::AddDestination { .. }
                | Command::RemoveDestination(_)
                | Command::StartStream
                | Command::StopStream
                | Command::StartRecord
                | Command::StopRecord => {}
            }
        }
        let _ = self.tx.send(ThreadMsg::Cmd(cmd));
    }
}

impl Runtime for LiveRuntime {
    fn snapshot(&self) -> Snapshot {
        self.snapshot_now()
    }

    fn send(&self, cmd: Command) {
        self.send_cmd(cmd);
    }

    fn conn_state(&self) -> ConnState {
        self.conn_now()
    }
}

impl Runtime for Arc<LiveRuntime> {
    fn snapshot(&self) -> Snapshot {
        self.snapshot_now()
    }

    fn send(&self, cmd: Command) {
        self.send_cmd(cmd);
    }

    fn conn_state(&self) -> ConnState {
        self.conn_now()
    }
}

/// The host-session view over a [`LiveRuntime`] the wizard launched:
/// injects the running cost (hourly rate from the state file, elapsed from
/// its creation time) and the seats' token ids into every snapshot. The
/// wire roster carries no token ids, so revocation targeting can only come
/// from the host's own records; this closes that gap without touching the
/// [`Runtime`] contract or the wire protocol.
///
/// The map is the invites panel's own, shared rather than copied: a seat
/// revoked and minted into again carries a new token, and a snapshot
/// handing the mixer the token from launch would revoke the credential that
/// is already dead and leave the person in the seat.
pub struct CostedRuntime {
    inner: Arc<LiveRuntime>,
    hourly_microusd: u64,
    created_unix: u64,
    tokens: TokenMap,
}

impl CostedRuntime {
    pub fn new(
        inner: Arc<LiveRuntime>,
        hourly_microusd: u64,
        created_unix: u64,
        tokens: TokenMap,
    ) -> CostedRuntime {
        CostedRuntime {
            inner,
            hourly_microusd,
            created_unix,
            tokens,
        }
    }
}

impl Runtime for CostedRuntime {
    fn snapshot(&self) -> Snapshot {
        let mut snap = self.inner.snapshot_now();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(self.created_unix);
        let elapsed_secs = now.saturating_sub(self.created_unix);
        snap.cost = Some(CostView {
            hourly_microusd: self.hourly_microusd,
            accrued_microusd: self.hourly_microusd * elapsed_secs / 3600,
            elapsed_secs,
        });
        let tokens = self.tokens.lock().expect("token map");
        for member in &mut snap.members {
            if member.token.is_none() {
                member.token = tokens.get(&member.id).copied();
            }
        }
        drop(tokens);
        snap
    }

    fn send(&self, cmd: Command) {
        self.inner.send_cmd(cmd);
    }

    /// Neither the cost meter nor the token map can change the connection
    /// state, so this is the inner runtime's answer unwrapped.
    fn conn_state(&self) -> ConnState {
        self.inner.conn_now()
    }
}

impl Drop for LiveRuntime {
    fn drop(&mut self) {
        // Harmless if the worker already left; the channel just reports
        // closed. Join keeps the socket and stream teardown ordered.
        let _ = self.tx.send(ThreadMsg::Cmd(Command::Leave));
        if let Some(handle) = self.worker.lock().expect("live worker handle").take() {
            let _ = handle.join();
        }
    }
}

fn connect_socket(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().expect("static addr")
    } else {
        "[::]:0".parse().expect("static addr")
    };
    let socket = UdpSocket::bind(bind)?;
    socket.set_nonblocking(true)?;
    socket.connect(addr)?;
    Ok(socket)
}

struct Worker {
    core: ClientCore,
    socket: UdpSocket,
    addresses: Vec<SocketAddr>,
    addr_idx: usize,
    ever_joined: bool,
    driver: Driver,
    /// None while the stream is lost or being reopened; the network side
    /// keeps running so the session survives an unplugged interface.
    engine: Option<EngineSide>,
    /// Frames the current ring was sized from: the settings' request, or the
    /// device's own callback size when the device negotiated a bigger one.
    device_frames: u32,
    /// The depth [`Worker::top_up_playout`] holds the playout ring at, which is
    /// the cushion the device plays out of and the latency it costs. The ring is
    /// cut for [`PLAYOUT_CUSHION_MAX`] at open, so this moves without the
    /// device.
    cushion: CushionControl,
    /// The bridge counters as the log reports them; nothing else consumes
    /// them, so without this a ring the device outgrows is audible but
    /// invisible.
    rings: RingWatch,
    /// This loop's own pacing against the ring it fills. The device side is
    /// scheduled in real time and this side is not, so without this the margin
    /// between them is unmeasured.
    wake: WakeWatch,
    /// One warn per episode when playout goes silent or media is refused;
    /// nothing else consumes the jitter buffer's counters, so without this
    /// neither would be said.
    playout: PlayoutWatch,
    /// The downlink's own loss rate, which no control message carries: the
    /// server reports the uplink and this side has to measure the other half.
    downlink: DownlinkLoss,
    settings: AudioSettings,
    shared: Arc<Mutex<SharedState>>,
    rx: mpsc::Receiver<ThreadMsg>,
    /// Datagram scratch, allocated once: an avatar chunk is four times a
    /// media packet, and a short buffer would truncate it silently.
    rx_buf: Box<[u8]>,
    epoch: Instant,
    capture_buf: Vec<f32>,
    mono_buf: Vec<f32>,
    /// When a settings change closed the device, so the reopen can say how
    /// long nothing was captured. A real device takes real time here and the
    /// fake takes none, which is the difference no offline test can see.
    shut_at: Option<Instant>,
    /// Capture samples moved since a reopen, counted until the first report.
    /// Zero means the device came back and the microphone did not.
    moved_since_reopen: Option<usize>,
    /// Packets the uplink produced since a reopen. Samples reaching the core
    /// prove capture; only this proves anything left the machine.
    sent_since_reopen: Option<usize>,
    /// Sends the socket refused. Producing a packet and sending one are not
    /// the same event, and the error was thrown away at all four call sites.
    send_errors: u64,
    /// Playout staged toward the ring: pulled from the core but not yet
    /// accepted, so a full ring never discards decoded audio.
    carry: [f32; CHUNK_STEREO],
    carry_pos: usize,
    carry_len: usize,
    /// Last time the playout ring accepted a sample, which a device that is
    /// rendering makes room for on every callback.
    ring_took: Instant,
    /// Frame clock [`Worker::drain_stalled_playout`] is paying from, and None
    /// whenever the device is taking audio and owes it nothing.
    drain_from: Option<Instant>,
    levels: LevelsView,
    /// Hashes whose bytes did not decode; never retried, so one bad avatar
    /// costs one decode attempt per session.
    avatar_failed: HashSet<String>,
    reopen_attempts: u64,
    last_reopen: Option<Instant>,
    /// When the running stream opened, until it has been up long enough to
    /// count as recovered; None while there is no stream, and once the
    /// running one has already ended its episode.
    opened_at: Option<Instant>,
    episode: ReopenEpisode,
    /// Streams that stopped on their own this session. A reopen somebody asked
    /// for never reaches here, which is what keeps a buffer change out of the
    /// cutting-out state and out of the log.
    device_stops: u64,
    /// Whether those stops are bunched up densely enough to call the device
    /// unreliable, over a window the reopen cadence itself is far too short to
    /// see: an episode ends five seconds after a stream that came back.
    cutting_out: EpisodeWatch,
    /// Whether the cutting-out run was open last tick, so the warning is one
    /// line per run rather than one per 2.5 ms tick.
    was_cutting_out: bool,
    /// The rate outcomes last logged, so a reopen on the same rung writes
    /// nothing and a rung change is written exactly once.
    announced_rate: Option<RateOutcomesView>,
    /// What the platform granted this thread, set once it is running. The
    /// pacing warning names it: a loop waking late at a real-time priority and
    /// one waking late at a priority nobody raised are different faults.
    priority: ThreadPriority,
    /// Whether the session has been far enough apart for long enough to be
    /// worth offering the other monitoring arrangement.
    hear_self_offer: HearSelfOffer,
}

impl Worker {
    fn run(mut self) {
        // This thread fills a ring a real-time callback drains, so it asks for
        // the same class the callback runs at. Held for the session and released
        // when this returns: on Windows it carries process-wide timer
        // resolution, which the app has no business keeping once its audio has
        // stopped.
        let priority = AudioPriority::raise_current_thread(TICK);
        self.priority = priority.granted();
        let mut next = Instant::now() + TICK;
        loop {
            if !self.step() {
                return;
            }
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            }
            next += TICK;
            // After a long stall, resume the cadence instead of spinning
            // through the backlog; sample counts carry the audio timing.
            if next < Instant::now() {
                next = Instant::now() + TICK;
            }
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// One loop iteration. Returns false when the session is over and the
    /// thread should exit.
    fn step(&mut self) -> bool {
        self.watch_wakeup();
        let now_ms = self.now_ms();

        loop {
            match self.rx.try_recv() {
                Ok(ThreadMsg::Cmd(Command::Leave)) => {
                    self.shutdown(now_ms);
                    return false;
                }
                Ok(ThreadMsg::Cmd(cmd)) => self.apply_command(cmd),
                Ok(ThreadMsg::Reconfigure(settings)) => self.reconfigure(settings),
                Ok(ThreadMsg::AutoCushion(auto)) => {
                    self.settings.auto_cushion = auto;
                    self.cushion.set_auto(auto);
                }
                Err(_) => break,
            }
        }

        self.check_stream();

        // Audio moves in device-callback-sized bites with the rings
        // serviced between them; real streams pump themselves, so their
        // pass runs exactly once. Socket first so fresh media feeds this
        // tick's playout.
        loop {
            self.drain_socket();
            self.top_up_playout();
            let pumped = self.driver.pump_one();
            let now_ms = self.now_ms();
            self.move_capture(now_ms);
            if !pumped {
                break;
            }
        }

        self.drain_stalled_playout();
        self.watch_ring_health();

        let now_ms = self.now_ms();
        for pkt in self.core.poll(now_ms) {
            self.send_datagram(&pkt);
        }
        self.drain_events(now_ms);
        let stats = self.core.stats();
        self.watch_playout(&stats);
        self.publish_stats(&stats);
        self.watch_hear_self_offer();
        self.maybe_fail_over(now_ms);
        true
    }

    fn apply_command(&mut self, cmd: Command) {
        let result = match cmd {
            Command::SetFader {
                member,
                gain_db,
                pan,
                muted,
            } => self.core.set_fader(member, gain_db, pan, muted),
            Command::SetClick(on) => self.core.set_click(on),
            Command::SetMetronome {
                bpm,
                beats_per_bar,
                enabled,
            } => self.core.set_metronome(bpm, beats_per_bar, enabled),
            Command::SetBroadcastFader {
                member,
                gain_db,
                pan,
                muted,
            } => self.core.set_broadcast_fader(member, gain_db, pan, muted),
            Command::SetBroadcastAudition(on) => self.core.set_broadcast_audition(on),
            Command::SetHearSelf(on) => self.core.set_hear_self(on),
            Command::SendChat(text) => self.core.send_chat(&text),
            Command::Revoke(jti) => self.core.revoke(jti),
            // The bytes arrive raw from the settings sheet: hashing,
            // caching, and the announcement are the core's job, and it
            // refuses anything outside the transfer caps.
            Command::SetOwnAvatar(Some(bytes)) => self.core.set_avatar(&bytes).map(|hash| {
                tracing::info!(
                    hash = %avatar::hash_hex(&hash),
                    bytes = bytes.len(),
                    "own avatar announced"
                );
            }),
            // Straight through to the control link. The key is moved into
            // the op and never copied, logged, or kept here; the server
            // holds it in memory for as long as the destination exists.
            Command::AddDestination { id, platform, key } => {
                tracing::info!(
                    destination = id.0,
                    platform = platform.as_str(),
                    "destination configured"
                );
                self.core
                    .stream_ctl(StreamOp::AddDestination { id, platform, key })
            }
            Command::RemoveDestination(id) => {
                self.core.stream_ctl(StreamOp::RemoveDestination { id })
            }
            Command::StartStream => self.core.stream_ctl(StreamOp::Start),
            Command::StopStream => self.core.stream_ctl(StreamOp::Stop),
            Command::StartRecord => self.core.record_ctl(RecordOp::Start),
            Command::StopRecord => self.core.record_ctl(RecordOp::Stop),
            // Validated in the core against the wire's own cap; stored there
            // and re-announced on every join, exactly like the avatar.
            Command::SetOwnName(name) => self.core.set_name(&name),
            Command::SetOwnAvatar(None) => {
                // The control protocol has no way to unset an avatar, so
                // this is local only: your own strip falls back to the
                // initials disc, and members already here keep the last
                // picture you sent until you rejoin without one.
                tracing::info!("own avatar dropped locally; the session keeps the announced hash");
                Ok(())
            }
            Command::Leave => unreachable!("handled by step"),
        };
        if let Err(err) = result {
            // Commands racing a disconnect are expected; not joined yet is
            // the usual cause.
            tracing::debug!(%err, "command not sent");
        }
    }

    fn shutdown(&mut self, now_ms: u64) {
        if let Err(err) = self.core.leave("left the session") {
            tracing::debug!(%err, "leave without a session");
        }
        // One poll flushes the queued Bye; delivery is best effort, the
        // server also ejects on silence.
        for pkt in self.core.poll(now_ms) {
            self.send_datagram(&pkt);
        }
        self.driver.close();
        self.shared.lock().expect("live state").stats.state = ConnState::Idle;
    }

    /// Closes and reopens the audio stream with new settings; the network
    /// side never pauses. On failure the user's selection is kept and the
    /// reopen cadence keeps trying exactly it: rewriting the settings to the
    /// system default here would leave the Audio tab claiming a device the
    /// stream does not run. The refusal itself stays on screen through
    /// `device_error`.
    fn reconfigure(&mut self, settings: AudioSettings) {
        self.shut_at = Some(Instant::now());
        // Drain what the old ring already captured so those samples reach
        // the core before the endpoints are dropped; orphaning them would
        // shift our uplink frame clock behind the server's.
        let now_ms = self.now_ms();
        self.move_capture(now_ms);
        self.driver.close();
        self.engine = None;
        self.opened_at = None;
        self.settings = settings;
        // A device the user just picked is a fresh start: the budget the last
        // one spent, and the fault it was in, do not carry over.
        self.episode = ReopenEpisode::default();
        self.attempt_open();
    }

    /// A dead stream is closed and retried with the same settings on the
    /// episode's widening cadence. What class of failure it was is only
    /// knowable from the reopen attempt: the exclusive path latches on any
    /// read or write hiccup, so nothing here may call a driver stutter an
    /// unplug.
    fn check_stream(&mut self) {
        if self.driver.errored() {
            self.driver.close();
            self.engine = None;
            self.opened_at = None;
            // A dead stream has no rate outcome to show.
            self.shared.lock().expect("live state").stats.rate = None;
            self.episode.faulted = true;
            // One stop, whether or not the reopen that follows heals it before
            // any screen could draw the fault: the gap was audible either way.
            self.device_stops += 1;
        }
        if self
            .opened_at
            .is_some_and(|t| t.elapsed() >= STREAM_SETTLED_AFTER)
        {
            self.opened_at = None;
            self.episode = ReopenEpisode::default();
        }
        if self.engine.is_none() && !self.episode.spent() {
            let backoff = self.episode.backoff();
            if self.last_reopen.is_none_or(|t| t.elapsed() >= backoff) {
                self.attempt_open();
            }
        }
        self.publish_fault();
        self.publish_cutting_out(Instant::now());
    }

    /// What the stream is doing wrong, as the status bar and the Audio tab
    /// read it. A budget that is spent while a stream runs is a stream that
    /// came back on the last attempt, so the engine decides first; and a
    /// reopen for a pick somebody just made is not a fault, which is what
    /// keeps a buffer change from flashing the bar.
    fn audio_fault(&self) -> Option<AudioFaultView> {
        if self.engine.is_some() {
            return None;
        }
        if self.episode.spent() {
            return Some(AudioFaultView::GaveUp {
                tries: self.episode.attempts,
            });
        }
        self.episode.faulted.then_some(AudioFaultView::Retrying)
    }

    /// The fault the UI reads, published every tick and logged only when it
    /// changes: at a tick every 2.5 ms, a line per tick would fill the file
    /// in seconds.
    fn publish_fault(&mut self) {
        let fault = self.audio_fault();
        let mut shared = self.shared.lock().expect("live state");
        if shared.audio_fault == fault {
            return;
        }
        shared.audio_fault = fault;
        drop(shared);
        // Every attempt logs itself, so a run of them needs nothing here. The
        // end of the cadence does, because nothing after it reopens the
        // stream, and so does the recovery that clears a fault.
        match fault {
            Some(AudioFaultView::GaveUp { tries }) => {
                tracing::warn!(tries, "the audio device did not stay open; giving up");
            }
            Some(AudioFaultView::Retrying) => {}
            None => tracing::info!("the audio stream is running again"),
        }
    }

    /// Whether the device is failing rather than down, which is the one thing
    /// the fault itself cannot say: a stop the next tick reopens is over before
    /// any frame draws it, so a device doing that twenty times a minute is
    /// twenty gaps and nothing on screen. The run is warned once, because the
    /// stops each already log a line and this is the conclusion drawn from
    /// them, and a device that never stops writes nothing at all.
    fn publish_cutting_out(&mut self, now: Instant) {
        let open = self.cutting_out.observe(now, self.device_stops);
        if open && !self.was_cutting_out {
            tracing::warn!(
                stops = self.device_stops,
                "the audio device keeps stopping and reopening"
            );
        }
        self.was_cutting_out = open;
        let stops = open.then_some(self.device_stops);
        let mut shared = self.shared.lock().expect("live state");
        shared.stats.cutting_out = stops;
    }

    /// One open attempt against the episode's budget. It sets the cadence
    /// clock whether it succeeds or not, so a device that opens and then dies
    /// before the next tick escalates exactly like one that refuses outright:
    /// an open that does not last is not progress.
    fn attempt_open(&mut self) {
        self.last_reopen = Some(Instant::now());
        self.reopen_attempts += 1;
        self.episode.attempts += 1;
        // A first attempt at a reopen somebody asked for is not a fault, and the
        // log file promises to stay empty on a healthy run. A retry is a fault
        // whatever started the episode, and so is any reopen nobody asked for.
        let asked_for = self.shut_at.is_some() && self.episode.attempts == 1;
        if asked_for {
            tracing::debug!(
                attempt = self.reopen_attempts,
                "reopening the audio stream for a settings change"
            );
        } else {
            tracing::warn!(
                attempt = self.reopen_attempts,
                in_episode = self.episode.attempts,
                "reopening audio stream"
            );
        }
        // What the device said about a refusal reaches the UI through
        // `device_error`, and the cadence itself decides what happens next,
        // so there is nothing to do with the answer here.
        let _ = self.try_open();
    }

    fn try_open(&mut self) -> Result<(), AudioError> {
        match self.driver.open(&self.settings) {
            Ok((engine, device_frames, rate)) => {
                // Sized to the whole ring, so one pull always empties it:
                // a shorter buffer would leave a backlog behind on every
                // tick, which is capture latency that never drains.
                self.capture_buf
                    .resize(capture_capacity(device_frames), 0.0);
                self.engine = Some(engine);
                self.opened_at = Some(Instant::now());
                self.device_frames = device_frames;
                // A fresh ring at a fresh device size: the cushion this one
                // settled on says nothing about the next one, and the first
                // window of a stream holds the open itself.
                self.cushion = CushionControl::new(device_frames, self.settings.auto_cushion);
                self.rings = RingWatch::new(Instant::now());
                self.carry_pos = 0;
                self.carry_len = 0;
                // The ring opens at its target depth of silence, so the first
                // callback owes this stream nothing yet.
                self.ring_took = Instant::now();
                if let Some(shut) = self.shut_at.take() {
                    let shut_ms = shut.elapsed().as_millis() as u64;
                    // Nothing is captured while the device is shut, so the gap is
                    // a hole in what everybody else hears. A few hundred
                    // milliseconds is what a reopen costs; a second is a device
                    // taking far longer than one, and worth saying out loud.
                    if shut_ms >= SLOW_REOPEN.as_millis() as u64 {
                        tracing::warn!(
                            shut_ms,
                            device_frames,
                            "the audio device took a long time to reopen, and captured nothing while it was shut"
                        );
                    } else {
                        tracing::debug!(shut_ms, device_frames, "audio reopened");
                    }
                    self.moved_since_reopen = Some(0);
                    self.sent_since_reopen = Some(0);
                }
                let mut shared = self.shared.lock().expect("live state");
                shared.reopen_attempts = self.reopen_attempts;
                shared.device_error = None;
                shared.stats.rate = rate;
                drop(shared);
                self.log_rate_changes(rate);
                Ok(())
            }
            Err(err) => {
                tracing::warn!(%err, "audio stream open failed");
                // The reason the device gave, kept for the UI. A refused
                // device is the one failure a musician can act on from inside
                // the session, and the log is the one place they will not be
                // looking at mid-song.
                let mut shared = self.shared.lock().expect("live state");
                shared.device_error = Some(err.to_string());
                shared.stats.rate = None;
                Err(err)
            }
        }
    }

    /// One log line per direction whose rung changed at this open. The rung
    /// itself is on screen for as long as the stream runs, in the status
    /// bar's tag and under the pickers on the Audio tab; what the file keeps
    /// is when it changed, which no on-screen state can say.
    fn log_rate_changes(&mut self, rate: Option<RateOutcomesView>) {
        let Some(rate) = rate else { return };
        let old = self.announced_rate;
        for (side, old, new) in [
            ("capture", old.map(|r| r.capture), rate.capture),
            ("playback", old.map(|r| r.playback), rate.playback),
        ] {
            log_rate_change(old, new, side);
        }
        self.announced_rate = Some(rate);
    }

    fn drain_socket(&mut self) {
        // WouldBlock ends the drain; other errors (ICMP refusals surface
        // here on connected sockets) also just wait for the timeout
        // machinery rather than tearing anything down.
        loop {
            let Ok(len) = self.socket.recv(&mut self.rx_buf) else {
                return;
            };
            let now_ms = self.now_ms();
            for pkt in self.core.handle_datagram(now_ms, &self.rx_buf[..len]) {
                self.send_datagram(&pkt);
            }
        }
    }

    /// Device-paced capture: whatever arrived in the ring goes through the
    /// raw path, which emits zero or more sealed frames.
    /// Sends one datagram, counting a refusal instead of discarding it. A
    /// socket that stops accepting looks exactly like a server that stopped
    /// listening, and both were invisible here.
    fn send_datagram(&mut self, pkt: &[u8]) {
        if let Err(err) = self.socket.send(pkt) {
            self.send_errors = self.send_errors.saturating_add(1);
            if self.send_errors.is_power_of_two() {
                tracing::warn!(
                    refused = self.send_errors,
                    bytes = pkt.len(),
                    %err,
                    "the socket would not send a packet"
                );
            }
        }
    }

    fn move_capture(&mut self, now_ms: u64) {
        let mut inst_peak = 0.0f32;
        let mut inst_sq = 0.0f32;
        let mut n = 0usize;
        if let Some(engine) = self.engine.as_mut() {
            let got = engine.pull_captured(&mut self.capture_buf);
            self.mono_buf.clear();
            // The stream is interleaved stereo on both sides; the uplink
            // is mono, so fold the pair down.
            for frame in self.capture_buf[..got].chunks_exact(2) {
                let s = (frame[0] + frame[1]) * 0.5;
                self.mono_buf.push(s);
                inst_peak = inst_peak.max(s.abs());
                inst_sq += s * s;
            }
            n = self.mono_buf.len();
            if let Some(moved) = self.moved_since_reopen.as_mut() {
                *moved += n;
                // Ten seconds of a 48 kHz mono uplink, read once, and only
                // reported if it has something to complain about. The server's
                // loss figure covers the second before it was sent, so a sample
                // taken right after a reopen can report the gap itself and read
                // as permanent. Ten seconds is long enough for the window to
                // have caught up. A settings change is a thing somebody asked
                // for, so a healthy one leaves this file empty, which is what
                // its first line promises the reader.
                if *moved >= SAMPLE_RATE as usize * 10 {
                    let moved = *moved;
                    let sent = self.sent_since_reopen.take().unwrap_or(0);
                    self.moved_since_reopen = None;
                    let st = self.core.stats();
                    let recovered = st.uplink_loss_pct.is_some_and(|pct| pct < 5.0)
                        && moved > 0
                        && self.send_errors == 0;
                    if !recovered {
                        tracing::warn!(
                            moved,
                            sent,
                            server_says_loss_pct = ?st.uplink_loss_pct,
                            server_says_depth = ?st.uplink_jitter_depth,
                            own_late = st.jitter.late,
                            own_reanchors = st.jitter.reanchors,
                            send_errors = self.send_errors,
                            "ten seconds after the reopen the uplink has not come back"
                        );
                    }
                }
            }
            for pkt in self.core.push_capture_raw(now_ms, &self.mono_buf) {
                if let Some(sent) = self.sent_since_reopen.as_mut() {
                    *sent += 1;
                }
                self.send_datagram(&pkt);
            }
        }
        let inst_rms = if n == 0 {
            0.0
        } else {
            (inst_sq / n as f32).sqrt()
        };
        self.levels.input_peak = inst_peak.max(self.levels.input_peak * LEVEL_DECAY);
        self.levels.input_rms = inst_rms.max(self.levels.input_rms * LEVEL_DECAY);
    }

    /// Holds the playout ring at the depth [`CushionControl`] is asking for,
    /// which is the cushion the device plays out of; the ring itself is cut
    /// deeper, so that depth is a number and not a device size. The carry holds
    /// anything the ring refused so no decoded audio is dropped.
    fn top_up_playout(&mut self) {
        let mut inst_peak = 0.0f32;
        let mut inst_sq = 0.0f32;
        let mut n = 0usize;
        let mut took = false;
        let target = self.cushion.target();
        if let Some(engine) = self.engine.as_mut() {
            loop {
                if self.carry_pos < self.carry_len {
                    let pushed = fill_playout_to(
                        engine,
                        &self.carry[self.carry_pos..self.carry_len],
                        target,
                    );
                    took |= pushed > 0;
                    self.carry_pos += pushed;
                    if self.carry_pos < self.carry_len {
                        break; // the ring is at its target depth
                    }
                }
                self.core.pull_playout_raw(&mut self.carry);
                for &s in &self.carry {
                    inst_peak = inst_peak.max(s.abs());
                    inst_sq += s * s;
                }
                n += self.carry.len();
                self.carry_pos = 0;
                self.carry_len = CHUNK_STEREO;
            }
        }
        if took {
            self.ring_took = Instant::now();
        }
        let inst_rms = if n == 0 {
            0.0
        } else {
            (inst_sq / n as f32).sqrt()
        };
        self.levels.output_peak = inst_peak.max(self.levels.output_peak * LEVEL_DECAY);
        self.levels.output_rms = inst_rms.max(self.levels.output_rms * LEVEL_DECAY);
    }

    /// The playout path while the device is not taking audio, which is the
    /// stream being reopened, a reopen that keeps failing, and a ring the device
    /// has stopped emptying. `pull_playout_raw` is the only thing that advances
    /// the jitter buffer's playout position, and it is otherwise reached only
    /// from the device-paced fill above, so the buffer would fill to its depth
    /// cap and give the position up while the frames it was holding were the
    /// only audio the reopened stream could have started from. What comes
    /// out here is dropped: there is no device to play it.
    fn drain_stalled_playout(&mut self) {
        let now = Instant::now();
        if self.engine.is_some()
            && now.duration_since(self.ring_took) < playout_stall_after(self.device_frames)
        {
            self.drain_from = None;
            return;
        }
        let from = *self.drain_from.get_or_insert(now);
        let owed = now.duration_since(from).as_micros() / TICK.as_micros();
        let cap = PLAYOUT_DRAIN_MAX.as_micros() / TICK.as_micros();
        let frames = owed.min(cap);
        for _ in 0..frames {
            self.core.pull_playout_raw(&mut self.carry);
        }
        // Whatever the ring refused belongs to a position this has walked past.
        self.carry_pos = 0;
        self.carry_len = 0;
        self.drain_from = Some(if owed > cap {
            now
        } else {
            from + TICK * frames as u32
        });
    }

    /// This loop's pacing, timed where it wakes up, and the cushion it has to
    /// stay inside. The cushion is only a deadline when something drains the
    /// ring on a clock of its own: the offline driver pumps from this thread,
    /// where a late wakeup delays playout instead of emptying it.
    fn watch_wakeup(&mut self) {
        let cushion = (self.engine.is_some() && matches!(self.driver, Driver::Real { .. }))
            .then(|| cushion_time(self.cushion.target()));
        self.wake.observe(Instant::now(), cushion, self.priority);
    }

    /// The bridge counters, reported by [`RingWatch`]. Movement means a ring
    /// too shallow for what the device delivers or for what the worker is
    /// keeping up with; the log is the one place that class of defect shows
    /// as something other than bad audio somebody else can hear. Whether the
    /// ring is in a crackling run reaches the snapshot too, so the status bar
    /// and the Audio tab read it the way they read connection state: no
    /// stream, no run in progress.
    fn watch_ring_health(&mut self) {
        let now = Instant::now();
        let (crackling, low_at, low) = match self.engine.as_ref() {
            Some(engine) => (
                self.rings.observe(now, engine, self.device_frames),
                self.rings.low_water_at(),
                self.rings.playout_low_frames(),
            ),
            // A ring that is gone is not a ring keeping up, and a stale water
            // mark is what the cushion would act on.
            None => {
                self.rings.forget();
                (false, now, None)
            }
        };
        let mut s = self.shared.lock().expect("live state");
        s.stats.crackling = crackling;
        // The server owns both states, so this is what its last RecordStatus
        // and StreamStatus said.
        let held = recording_or_on_air(&s.record, &s.stream);
        drop(s);
        self.cushion.observe(low_at, low, held);
    }

    fn drain_events(&mut self, now_ms: u64) {
        use jamstream_session::client::ClientEvent;
        let events = self.core.events();
        if events.is_empty() {
            return;
        }
        // Decoding is milliseconds of work; it happens after the lock is
        // released so the paint thread never waits on it.
        let mut ready: Vec<(MemberId, [u8; 32])> = Vec::new();
        let mut s = self.shared.lock().expect("live state");
        for event in events {
            match event {
                ClientEvent::AvatarReady { member, hash } => ready.push((member, hash)),
                ClientEvent::Joined => {
                    self.ever_joined = true;
                    s.me = self.core.member_id();
                }
                ClientEvent::Roster(members) => s.roster = members,
                ClientEvent::Chat { from, text } => {
                    let from_name = s
                        .roster
                        .iter()
                        .find(|m| m.id == from)
                        .map_or_else(|| format!("member {}", from.0), |m| m.name.clone());
                    s.push_chat(ChatLine {
                        from_name,
                        from_id: from,
                        text,
                        at_ms: now_ms,
                    });
                }
                ClientEvent::MetronomeChanged {
                    bpm,
                    beats_per_bar,
                    enabled,
                } => {
                    s.metronome.bpm = bpm;
                    s.metronome.beats_per_bar = beats_per_bar;
                    s.metronome.enabled = enabled;
                }
                ClientEvent::BroadcastMixChanged {
                    target,
                    gain_db,
                    pan,
                    muted,
                } => {
                    s.broadcast_faders.insert(
                        target,
                        FaderView {
                            gain_db,
                            pan,
                            muted,
                        },
                    );
                }
                ClientEvent::StreamStatus(destinations) => {
                    s.stream = destinations
                        .into_iter()
                        .map(|d| DestinationView {
                            id: d.id,
                            platform: d.platform,
                            state: d.state,
                            bitrate_kbps: d.bitrate_kbps,
                            dropped_frames: d.dropped_frames,
                            repeated_frames: d.repeated_frames,
                        })
                        .collect();
                }
                ClientEvent::BroadcastReadiness(state) => s.readiness = Some(state),
                ClientEvent::RecordStatus { state, stems } => {
                    s.record = RecordView {
                        state: match state {
                            RecordingState::Idle => RecordState::Idle,
                            RecordingState::Recording => RecordState::Recording,
                            RecordingState::Uploading => RecordState::Uploading,
                            RecordingState::Failed { reason } => RecordState::Failed { reason },
                        },
                        stems,
                    };
                }
                // rtt_ms_last rides along in stats(); Ejected, Rejected,
                // and TimedOut land through the state mapping below.
                _ => {}
            }
        }
        // A member who left takes their decode with them: keep only what
        // the current roster still points at.
        let live: HashSet<String> = s
            .roster
            .iter()
            .filter_map(|m| m.avatar_hash.as_ref().map(avatar::hash_hex))
            .collect();
        s.avatars.retain(|hash, _| live.contains(hash));
        drop(s);
        for (member, hash) in ready {
            self.decode_avatar(member, hash);
        }
    }

    /// Decodes one ready avatar into shared state, exactly once per content
    /// hash. A decode failure is logged and dropped: the member keeps the
    /// initials disc rather than a broken image, and the bad hash is not
    /// retried.
    fn decode_avatar(&mut self, member: MemberId, hash: [u8; 32]) {
        let hex = avatar::hash_hex(&hash);
        if self.avatar_failed.contains(&hex) {
            return;
        }
        if self
            .shared
            .lock()
            .expect("live state")
            .avatars
            .contains_key(&hex)
        {
            return;
        }
        let Some(bytes) = self.core.avatar_bytes(&hash) else {
            // The cache evicted it between the event and here; a later
            // roster sync re-requests the bytes.
            tracing::debug!(member = member.0, hash = %hex, "avatar bytes are gone");
            return;
        };
        match avatar::decode(hex.clone(), bytes) {
            Ok(handle) => {
                tracing::debug!(
                    member = member.0,
                    hash = %hex,
                    width = handle.width,
                    height = handle.height,
                    "avatar decoded"
                );
                self.shared
                    .lock()
                    .expect("live state")
                    .avatars
                    .insert(hex, handle);
            }
            Err(err) => {
                tracing::warn!(%err, member = member.0, hash = %hex, "avatar did not decode");
                self.avatar_failed.insert(hex);
            }
        }
    }

    /// Hands this tick's jitter counters to [`PlayoutWatch`], with the member
    /// they belong to. Only a joined client is owed media, so the member is
    /// also the gate.
    fn watch_playout(&mut self, stats: &ClientStats) {
        let joined_as = matches!(stats.state, ClientState::Joined)
            .then(|| self.core.member_id())
            .flatten();
        self.playout
            .observe(Instant::now(), joined_as, stats.jitter);
    }

    fn publish_stats(&mut self, stats: &ClientStats) {
        let joined = matches!(stats.state, ClientState::Joined);
        let downlink = self.downlink.observe(Instant::now(), joined, stats.jitter);
        let mut s = self.shared.lock().expect("live state");
        if joined {
            self.ever_joined = true;
        }
        // Idle is terminal (set by shutdown); never overwrite it.
        if s.stats.state != ConnState::Idle {
            s.stats.state = conn_state_with(&stats.state, stats.session_full);
        }
        s.me = self.core.member_id();
        s.stats.rtt_ms = stats.rtt_ms_last;
        s.stats.jitter_depth = stats.jitter.depth_frames;
        s.stats.jitter_target = stats.jitter.target_frames;
        // Two directions, never one figure: the uplink is what the band is not
        // hearing and only the server can see it, the downlink is what this
        // machine is not playing.
        s.stats.uplink_loss_pct = stats.uplink_loss_pct;
        s.stats.downlink_loss_pct = downlink;
        // The server's buffer on our uplink is a term in the latency figure and
        // only the server can see it, so the figure has it or it has nothing.
        s.stats.uplink_jitter_depth = stats.uplink_jitter_depth.map(usize::from);
        // The depth inside the playout term, and what is holding it: a buffer
        // size sets where the depth starts and not where it stays, so the
        // control that picks one has to be able to say which it is showing.
        let cushion = self.cushion.view();
        s.stats.cushion = self.engine.is_some().then_some(cushion);
        s.stats.device_buffers = self.engine.is_some().then(|| device_buffers(cushion));
        s.levels = self.levels;
        s.stats.playout_low_frames = self.rings.playout_low_frames();
        s.stats.wake = self.wake.pacing().map(|pacing| WakeView {
            p99_ms: as_ms(pacing.p99) as f32,
            max_ms: as_ms(pacing.max) as f32,
        });
    }

    /// Hands this tick's latency figure to [`HearSelfOffer`], with whether
    /// there is anybody else playing to be out of time with. Reads the figure
    /// back off the snapshot rather than recomputing it, so the offer is
    /// always made against the number the musician is being shown.
    fn watch_hear_self_offer(&mut self) {
        let (mouth_to_ear_ms, hear_self, playing_with_others) = {
            let s = self.shared.lock().expect("live state");
            let others = s
                .roster
                .iter()
                .any(|m| m.connected && m.role == Role::Musician && Some(m.id) != s.me);
            (s.stats.mouth_to_ear_ms(), s.hear_self, others)
        };
        let offer = self.hear_self_offer.observe(
            Instant::now(),
            mouth_to_ear_ms,
            hear_self,
            playing_with_others,
        );
        self.shared.lock().expect("live state").offer_hear_self = offer;
    }

    /// Initial connect only: a timeout on one invite address moves on to
    /// the next with a fresh handshake. Once joined, timeouts are surfaced
    /// instead; the session lives on the address that admitted us.
    fn maybe_fail_over(&mut self, now_ms: u64) {
        if self.ever_joined
            || !matches!(self.core.state(), ClientState::TimedOut)
            || self.addr_idx + 1 >= self.addresses.len()
        {
            return;
        }
        self.addr_idx += 1;
        let addr = self.addresses[self.addr_idx];
        tracing::info!(%addr, "connect timed out, trying the next invite address");
        match connect_socket(addr) {
            Ok(socket) => {
                self.socket = socket;
                match self.core.reconnect(now_ms) {
                    Ok(init) => {
                        let _ = self.socket.send(&init);
                    }
                    Err(err) => tracing::warn!(%err, "reconnect failed"),
                }
                self.shared.lock().expect("live state").server_addr = addr.to_string();
            }
            Err(err) => tracing::warn!(%err, %addr, "socket for fallback address failed"),
        }
    }
}

/// The runtime's view of the backend's rate outcomes.
fn rate_view(outcomes: jamstream_audio_io::RateOutcomes) -> RateOutcomesView {
    let map = |o: jamstream_audio_io::RateOutcome| match o {
        jamstream_audio_io::RateOutcome::Native => RateOutcomeView::Native,
        jamstream_audio_io::RateOutcome::ClockSet { from } => RateOutcomeView::ClockSet { from },
        jamstream_audio_io::RateOutcome::OsConverted { device } => {
            RateOutcomeView::OsConverted { device }
        }
        jamstream_audio_io::RateOutcome::Resampled { device, added_ms } => {
            RateOutcomeView::Resampled { device, added_ms }
        }
    };
    RateOutcomesView {
        capture: map(outcomes.capture),
        playback: map(outcomes.playback),
    }
}

/// The log line one direction's rung earns at an open, given the rung it was
/// on before, or nothing when there is nothing to say. Rung 1 is not news, the
/// OS converter is a hover detail, and an unchanged rung is silence, so the
/// reopen cadence can never fill the file. A converter that replaces a clock
/// this app had set names the contest: that is the one demotion a musician
/// might otherwise chase into their other software's settings.
fn log_rate_change(old: Option<RateOutcomeView>, new: RateOutcomeView, side: &str) {
    if let Some(line) = rate_change_line(old, new, side) {
        tracing::info!(line, "the audio stream's sample rate path changed");
    }
}

/// The sentence a rung change earns, split out from [`log_rate_change`] so
/// the copy per rung is one thing a test can hold.
fn rate_change_line(
    old: Option<RateOutcomeView>,
    new: RateOutcomeView,
    side: &str,
) -> Option<String> {
    if old == Some(new) {
        return None;
    }
    match new {
        RateOutcomeView::Native | RateOutcomeView::OsConverted { .. } => None,
        RateOutcomeView::ClockSet { .. } => new.line(side),
        RateOutcomeView::Resampled { .. } => {
            let line = new.line(side)?;
            if matches!(old, Some(RateOutcomeView::ClockSet { .. })) {
                Some(format!("another app took the device clock back; {line}"))
            } else {
                Some(line)
            }
        }
    }
}

/// `session_full` rides on [`ClientStats`] rather than on the client state,
/// because the core stays in `Connecting` while it retries a full session.
/// So the stat decides, and only while nothing better has happened.
fn conn_state_with(state: &ClientState, session_full: bool) -> ConnState {
    if session_full && matches!(state, ClientState::Connecting) {
        return ConnState::SessionFull;
    }
    conn_state(state)
}

fn conn_state(state: &ClientState) -> ConnState {
    match state {
        ClientState::Connecting => ConnState::Connecting,
        ClientState::Joined => ConnState::Joined,
        // The Snapshot contract has no Rejected variant; the ejection
        // banner with the mismatch text is the honest nearest fit.
        ClientState::Rejected { ours, theirs } => ConnState::Ejected(format!(
            "protocol version mismatch: this client speaks {ours}, the server speaks {theirs}"
        )),
        ClientState::Ejected { reason } => ConnState::Ejected(reason.clone()),
        ClientState::TimedOut => ConnState::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use jamstream_protocol::control::{DestinationState, StreamPlatform};
    use jamstream_protocol::ids::DestinationId;

    use super::watch::CUSHION_STEP;
    use super::*;

    /// The loop's pace against the audio it moves. Both constants come off
    /// [`FrameDuration::Ms2_5`] now, so asserting each against its own source
    /// would only agree with itself; what is still worth holding is that the
    /// two describe one frame, at the rate the device is opened at.
    ///
    /// Nothing else in the suite would notice this drifting: the offline
    /// backend opens at whatever rate it is handed, so a client pacing at
    /// 2.5 ms while sending 5 ms frames would pass every test in
    /// `live_runtime.rs` and run its uplink clock at half speed against a real
    /// server.
    #[test]
    fn the_loop_paces_at_the_frame_it_moves() {
        let micros_of_a_frame = FRAME_FRAMES as u64 * 1_000_000 / u64::from(SAMPLE_RATE);
        assert_eq!(
            TICK,
            Duration::from_micros(micros_of_a_frame),
            "{FRAME_FRAMES} samples at {SAMPLE_RATE} Hz is not {TICK:?} of audio"
        );
        assert_eq!(CHUNK_STEREO, FRAME_FRAMES * usize::from(CHANNELS));
    }

    /// The figure a musician reads is the depth the loop fills to, so a cushion
    /// that moves moves it, live: the term follows the target and nothing else
    /// in the sum does.
    #[test]
    fn the_latency_figure_follows_the_cushion_that_is_held() {
        const FRAMES: u32 = 120;
        let opening = opening_cushion(FRAMES);
        let base = device_buffers(opening);
        let deeper = device_buffers(CushionView {
            held_frames: opening.held_frames + CUSHION_STEP / usize::from(CHANNELS),
            ..opening
        });
        let step_ms = as_ms(TICK) as f32;
        let figure = |device| {
            StatsView {
                rtt_ms: Some(45.0),
                jitter_depth: 3,
                device_buffers: Some(device),
                ..StatsView::default()
            }
            .mouth_to_ear_ms()
            .expect("the round trip is sampled")
        };

        assert_eq!(
            base.capture_ms, deeper.capture_ms,
            "the capture buffer is the device's and the cushion is not"
        );
        assert_eq!(deeper.playout_ms - base.playout_ms, step_ms);
        assert_eq!(
            figure(deeper) - figure(base),
            step_ms,
            "the headline figure moved by the frame the cushion moved and nothing else"
        );
    }

    /// Which states the cushion may not hand latency back in. A take under the
    /// recorder and a destination somebody is watching, and neither of the two
    /// states that look like them: an upload is a take that already stopped,
    /// and a destination still coming up has nobody to hear a dropout.
    #[test]
    fn only_a_running_take_or_a_watched_destination_holds_the_cushion() {
        let take = |state| RecordView {
            state,
            stems: false,
        };
        let destination = |state| {
            vec![DestinationView {
                id: DestinationId(0),
                platform: StreamPlatform::Twitch,
                state,
                bitrate_kbps: 0,
                dropped_frames: 0,
                repeated_frames: 0,
            }]
        };
        let idle = take(RecordState::Idle);

        assert!(!recording_or_on_air(&idle, &[]));
        assert!(recording_or_on_air(&take(RecordState::Recording), &[]));
        assert!(!recording_or_on_air(&take(RecordState::Uploading), &[]));
        assert!(recording_or_on_air(
            &idle,
            &destination(DestinationState::Live)
        ));
        for state in [
            DestinationState::Idle,
            DestinationState::Connecting,
            DestinationState::Failed {
                reason: "no relay".into(),
            },
        ] {
            assert!(
                !recording_or_on_air(&idle, &destination(state.clone())),
                "{state:?} held the cushion still"
            );
        }
    }

    /// The sizing that matters: the rings must fit the callbacks the
    /// device really delivers, and the request is only a lower bound.
    #[test]
    fn the_ring_is_sized_from_what_the_device_delivers() {
        // WASAPI shared mode: 120 asked for, the ~10 ms device period given.
        assert_eq!(ring_frames(120, Some(480)), 480);
        assert_eq!(
            playout_target(ring_frames(120, Some(480))),
            2 * 480 * usize::from(CHANNELS),
            "the 2x headroom applies to the negotiated size"
        );
        // A device that honours the request keeps the requested cushion.
        assert_eq!(ring_frames(120, Some(120)), 120);
        // A backend that cannot say leaves the request in charge.
        assert_eq!(ring_frames(240, None), 240);
        // A smaller negotiation never shrinks the ring below the request.
        assert_eq!(ring_frames(240, Some(32)), 240);
    }

    /// What the two depths cost, which is why they are separate numbers. The
    /// playout cushion is held, so it is mouth-to-ear and stays at the two
    /// callbacks of headroom the design settles on. The capture ring is drained
    /// to empty, so its capacity is only stall tolerance and buys 40 ms of it.
    #[test]
    fn the_capture_ring_is_deeper_than_the_playout_cushion_and_costs_nothing() {
        let ms =
            |samples: usize| samples as f64 / f64::from(CHANNELS) / f64::from(SAMPLE_RATE) * 1000.0;
        for frames in [32u32, 120, 240] {
            assert_eq!(
                ms(playout_target(frames)),
                2.0 * f64::from(frames.max(FRAME_FRAMES as u32)) / 48.0,
                "the playout cushion is the latency and may not grow"
            );
            assert_eq!(
                ms(capture_capacity(frames)),
                CAPTURE_RING.as_millis() as f64,
                "{frames}-frame callbacks want {CAPTURE_RING:?} of capture ring"
            );
        }
        // A device period past the floor takes the deeper of the two rather
        // than losing the two-callback slack the ring depends on.
        assert_eq!(capture_capacity(2_400), playout_target(2_400));
    }

    /// The ring is cut for the deepest cushion the app can hold, so growing the
    /// cushion never needs the device shut and reopened. Every buffer the Audio
    /// tab offers, and negotiated periods past the largest of them, where the
    /// period itself is deeper than the ceiling and the ring follows it.
    #[test]
    fn the_ring_holds_the_deepest_cushion_without_reopening_the_device() {
        let deepest = PLAYOUT_CUSHION_MAX.as_millis() as usize * SAMPLE_RATE as usize / 1000
            * usize::from(CHANNELS);
        for frames in [0u32, 32, 120, 240, 480, 2_400] {
            let capacity = playout_capacity(frames);
            assert!(
                capacity >= playout_target(frames),
                "{frames}-frame callbacks: a ring of {capacity} cannot hold its own target"
            );
            assert_eq!(capacity, playout_target(frames).max(deepest));
        }
        // Every buffer the Audio tab offers has room above its own cushion.
        for frames in [120u32, 240, 480] {
            assert!(
                playout_capacity(frames) > playout_target(frames),
                "{frames}-frame callbacks leave the cushion nowhere to grow into"
            );
        }
    }

    /// Which settings changes cost a device reopen. Every field the backend is
    /// handed does; the cushion answer does not, because it is a depth the
    /// worker's own loop fills to and a reopen for it would take a few hundred
    /// milliseconds of capture the whole band hears.
    #[test]
    fn only_the_device_half_of_the_settings_reopens_the_stream() {
        let base = AudioSettings {
            capture_id: Some("coreaudio:scarlett-in".to_owned()),
            playback_id: Some("coreaudio:scarlett-out".to_owned()),
            buffer_frames: 120,
            allow_exclusive: true,
            auto_cushion: true,
        };
        assert!(!base.reopens_for(&base), "nothing changed, nothing reopens");
        for pinned in [false, true] {
            assert!(
                !base.reopens_for(&AudioSettings {
                    auto_cushion: pinned,
                    ..base.clone()
                }),
                "pinning the cushion may not reopen the device"
            );
        }
        for changed in [
            AudioSettings {
                capture_id: None,
                ..base.clone()
            },
            AudioSettings {
                playback_id: None,
                ..base.clone()
            },
            AudioSettings {
                buffer_frames: 240,
                ..base.clone()
            },
            AudioSettings {
                allow_exclusive: false,
                ..base.clone()
            },
        ] {
            assert!(
                base.reopens_for(&changed),
                "the backend has to be handed {changed:?}"
            );
            // And the two changes together are still a reopen, so a cushion
            // answer riding along cannot swallow a device pick.
            assert!(base.reopens_for(&AudioSettings {
                auto_cushion: false,
                ..changed
            }));
        }
    }

    /// The stall threshold sits between two things it must not collide with: the
    /// device period, since a device that renders makes room once a period and
    /// must never be drained behind its own back, and the depth the jitter
    /// buffer holds before it gives its playout position up, since a drain that
    /// starts after that has nothing left to keep moving.
    #[test]
    fn the_stall_threshold_outlasts_a_callback_and_beats_the_buffer_cap() {
        let cap = TICK * JitterBuffer::MAX_DEPTH_FRAMES as u32;
        // Every buffer the Audio tab offers, and negotiated periods well past
        // the largest of them.
        for frames in [0u32, 32, 120, 240, 480, 960] {
            let period = TICK * frames.max(FRAME_FRAMES as u32) / FRAME_FRAMES as u32;
            let after = playout_stall_after(frames);
            assert!(
                after >= period * 2,
                "{frames}-frame callbacks: {after:?} is inside two device periods"
            );
            assert!(
                after < cap,
                "{frames}-frame callbacks: {after:?} outlasts the {cap:?} the buffer holds"
            );
        }
    }

    /// The claim the capture ring rests on: its depth is stall tolerance, not
    /// latency. A worker-paced consumer against a device-paced producer leaves
    /// at most one callback waiting whatever the capacity is, because every
    /// drain empties the ring.
    #[test]
    fn a_deeper_capture_ring_does_not_deepen_what_waits_in_it() {
        const FRAMES: u32 = 120;
        let callback = FRAMES as usize * usize::from(CHANNELS);
        let (mut device, mut engine) =
            CallbackBridge::new(capture_capacity(FRAMES), playout_capacity(FRAMES));
        let mut buf = vec![0.0f32; capture_capacity(FRAMES)];
        let mut deepest = 0usize;
        // One device callback per worker tick, the steady state of a device
        // opened at the loop's own frame size.
        for _ in 0..400 {
            device.on_capture(&vec![1.0; callback]);
            deepest = deepest.max(engine.pull_captured(&mut buf));
        }
        assert_eq!(deepest, callback, "one callback waits, not the ring");
        assert_eq!(engine.overruns(), 0);
    }

    /// The starvation shape a real device produces: 120-frame callbacks
    /// arriving before the consumer's first drain exists. A CoreAudio open
    /// has capture running more than 20 ms before the caller holds the
    /// handle, which at 2.5 ms a callback is eight of them.
    ///
    /// Counted rather than timed. The producer is the device's clock in
    /// production, but a test that sleeps for the window measures the runner's
    /// scheduler instead: a loaded macOS runner stretched 20 ms to 145 ms and
    /// delivered 58 callbacks where 8 were meant. Two callbacks of ring, the
    /// old shared capacity, still drops this; the assertion below is what
    /// separates them.
    #[test]
    fn a_capture_ring_absorbs_the_session_coming_up() {
        const FRAMES: u32 = 120;
        const BRING_UP: usize = 8;
        let callback = FRAMES as usize * usize::from(CHANNELS);
        let (mut device, mut engine) =
            CallbackBridge::new(capture_capacity(FRAMES), playout_capacity(FRAMES));

        for _ in 0..BRING_UP {
            device.on_capture(&vec![1.0; callback]);
        }
        let mut buf = vec![0.0f32; capture_capacity(FRAMES)];
        let got = engine.pull_captured(&mut buf);

        assert_eq!(
            engine.overruns(),
            0,
            "{} callbacks of capture were dropped while the consumer came up; \
             the first drain took {got} samples of a {} sample ring",
            engine.overruns(),
            capture_capacity(FRAMES)
        );
        assert_eq!(
            got,
            BRING_UP * callback,
            "the ring held {got} of the {} samples pushed before the first drain",
            BRING_UP * callback
        );
    }

    /// The log copy per rung change, the rate-rung disclosure contract: rung
    /// 2 and rung 3 are written once, an unchanged rung and rung 1 write
    /// nothing (so the reopen cadence cannot fill the file), the OS converter
    /// is a hover detail only, and a converter that replaced a clock this app
    /// set names the contest instead of reading like a random downgrade.
    #[test]
    fn rung_changes_earn_one_honest_log_line() {
        let clock_set = RateOutcomeView::ClockSet { from: 44_100 };
        let resampled = RateOutcomeView::Resampled {
            device: 44_100,
            added_ms: 3.2,
        };
        assert_eq!(
            rate_change_line(None, clock_set, "capture").as_deref(),
            Some("moved the capture device to 48 kHz (was 44.1)")
        );
        assert_eq!(
            rate_change_line(None, resampled, "capture").as_deref(),
            Some("converting capture 44.1 kHz to 48 kHz (+3.2 ms)")
        );
        assert_eq!(
            rate_change_line(Some(clock_set), resampled, "playback").as_deref(),
            Some(
                "another app took the device clock back; \
                 converting playback 44.1 kHz to 48 kHz (+3.2 ms)"
            )
        );
        // Silence: rung 1, the OS converter, and any unchanged rung.
        assert_eq!(
            rate_change_line(None, RateOutcomeView::Native, "capture"),
            None
        );
        assert_eq!(
            rate_change_line(Some(resampled), RateOutcomeView::Native, "capture"),
            None,
            "returning to native is visible in the tag going away"
        );
        assert_eq!(
            rate_change_line(
                None,
                RateOutcomeView::OsConverted { device: 44_100 },
                "playback"
            ),
            None
        );
        assert_eq!(
            rate_change_line(Some(resampled), resampled, "capture"),
            None
        );
        assert_eq!(
            rate_change_line(Some(clock_set), clock_set, "capture"),
            None
        );
    }

    /// The whole of the settings reaches the device request: the exclusive
    /// answer rides every open and reopen, and the floor on the buffer stays.
    #[test]
    fn the_device_request_carries_the_users_exclusive_answer() {
        let mut settings = AudioSettings {
            buffer_frames: 120,
            ..AudioSettings::default()
        };
        assert!(
            stream_config(&settings).allow_exclusive,
            "the default asks for the low-latency path"
        );
        settings.allow_exclusive = false;
        let config = stream_config(&settings);
        assert!(!config.allow_exclusive);
        assert_eq!(config.sample_rate, SAMPLE_RATE);
        assert_eq!(config.buffer_frames, 120);
        settings.buffer_frames = 0;
        assert_eq!(stream_config(&settings).buffer_frames, 32, "the floor");
    }

    /// A real device test: the client's own ring sizes, the client's own
    /// 2.5 ms consumer cadence, and a sound card producing on its own clock,
    /// which no fake in this workspace does. The only backend a test can drive
    /// is pumped by the consumer itself, so the producer could never be early
    /// and this whole class of fault had nowhere to show.
    ///
    /// A device starts delivering the moment its stream opens, which is before
    /// the caller holds the handle and well before a worker thread drains
    /// anything: measured here, a CoreAudio open hands over with 2 to 11
    /// callbacks already captured, up to 27 ms of audio. A two-callback ring,
    /// 5 ms, drops the rest of that. The capture ring holds it and drains it
    /// in one pull.
    ///
    /// The last assertion is what stops the others passing on a machine
    /// producing nothing at all.
    #[test]
    #[ignore = "requires a real capture and playback device"]
    fn a_real_device_loses_no_capture_while_a_session_comes_up() {
        const FRAMES: u32 = 120;
        const RUN: Duration = Duration::from_secs(1);

        let settings = AudioSettings {
            buffer_frames: FRAMES,
            ..AudioSettings::default()
        };
        let config = stream_config(&settings);
        let (device, mut engine) =
            CallbackBridge::new(capture_capacity(FRAMES), playout_capacity(FRAMES));
        engine.push_playout(&vec![0.0; playout_target(FRAMES)]);

        let backend = jamstream_audio_io::backend();
        let stream = backend
            .open_duplex(None, None, config, device.into_handler())
            .expect("the default capture and playback devices open");
        // Read before anything else: whatever is already in the ring was
        // captured while the caller had no way to drain it.
        let mut capture_buf = vec![0.0f32; capture_capacity(FRAMES)];
        let early = engine.pull_captured(&mut capture_buf);
        let negotiated = stream.buffer_frames();
        println!("negotiated callback frames: {negotiated:?}");

        // The worker's own loop: drain the whole capture ring and refill
        // playout to its target once per 2.5 ms tick.
        let silence = vec![0.0f32; playout_target(FRAMES)];
        let mut pulled = early;
        let deadline = Instant::now() + RUN;
        let mut next = Instant::now() + TICK;
        while Instant::now() < deadline {
            pulled += engine.pull_captured(&mut capture_buf);
            top_up(&mut engine, &silence, FRAMES);
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            }
            next += TICK;
        }
        let overruns = engine.overruns();
        let underruns = engine.underruns();
        let errored = stream.errored();
        stream.close();

        let callback = negotiated.unwrap_or(FRAMES) as usize * usize::from(CHANNELS);
        println!(
            "{early} samples were waiting when the open returned ({} callbacks, \
             {:.1} ms); the consumer pulled {pulled} in {RUN:?}; \
             overruns={overruns} underruns={underruns}",
            early / callback.max(1),
            early as f64 / f64::from(CHANNELS) / 48.0,
        );
        assert!(!errored, "the backend reported a fatal stream error");
        assert_eq!(
            overruns,
            0,
            "{overruns} capture callbacks were dropped in {RUN:?} against a ring of \
             {} samples, of which the consumer pulled {pulled}",
            capture_capacity(FRAMES)
        );
        // Half of real time is a wide floor: it separates a device that ran
        // from one delivering nothing, without failing on a slow start.
        let want = RUN.as_secs_f64() * f64::from(SAMPLE_RATE) * f64::from(CHANNELS) / 2.0;
        assert!(
            pulled as f64 > want,
            "only {pulled} samples came through in {RUN:?}, so the assertions above \
             passed on a device that delivered next to nothing"
        );
    }

    /// The cushion the worker's pacing is judged against is the depth the loop
    /// fills to, as time, which starts at two device callbacks of audio. The ring
    /// is cut deeper than that and the device never finds the difference, so a
    /// deadline read off the capacity would be one nothing has to meet. A target
    /// set from what the device negotiated rather than from what was asked for
    /// moves the deadline with it, and the 480-frame WASAPI shared period is the
    /// case that matters.
    #[test]
    fn the_cushion_is_two_device_callbacks_of_audio() {
        assert_eq!(cushion_time(playout_target(120)), Duration::from_millis(5));
        assert_eq!(cushion_time(playout_target(480)), Duration::from_millis(20));
        for frames in [0u32, 32, 120, 240, 480, 960] {
            let period = Duration::from_micros(
                u64::from(frames.max(FRAME_FRAMES as u32)) * 1_000_000 / u64::from(SAMPLE_RATE),
            );
            assert_eq!(
                cushion_time(playout_target(frames)),
                period * 2,
                "{frames}-frame callbacks against a target of {} samples",
                playout_target(frames)
            );
            assert!(
                cushion_time(playout_target(frames))
                    <= playout_capacity(frames) as u32 * TICK
                        / (FRAME_FRAMES * usize::from(CHANNELS)) as u32,
                "{frames}-frame callbacks: the cushion cannot outlast the ring holding it"
            );
        }
    }

    /// The worker's own top-up, against a ring a test drives by hand: fill to
    /// the target and no further.
    pub(super) fn top_up(engine: &mut EngineSide, samples: &[f32], frames: u32) {
        while fill_playout_to(engine, samples, playout_target(frames)) > 0 {}
    }
}
