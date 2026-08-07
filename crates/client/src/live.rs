//! The production [`Runtime`]: real audio devices through a
//! [`CallbackBridge`], a nonblocking UDP socket, and [`ClientCore`] driven
//! by a dedicated network thread.
//!
//! Thread layout: device callbacks (RT, allocation-free) exchange samples
//! with the network thread over the bridge's SPSC rings; the network thread
//! owns the socket, the core, and the audio stream lifecycle, and publishes
//! UI state into a `Mutex<SharedState>` the paint thread reads once per
//! frame. Loop cadence is ~2.5 ms with sleep-until pacing; precision is
//! forgiving because the raw capture/playout APIs are sample-count driven
//! by the device clock, not the loop clock.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use data_encoding::HEXLOWER;
use jamstream_audio_io::{
    AudioBackend, AudioError, CallbackBridge, DuplexHandler, EngineSide, StreamConfig,
    StreamHandle, WavBackend, WavStream,
};
use jamstream_engine::{JitterBuffer, JitterStats};
use jamstream_protocol::control::{MAX_DATAGRAM_BYTES, MemberInfo, StreamOp};
use jamstream_protocol::control::{RecordOp, RecordingState};
use jamstream_protocol::ids::HOST_MEMBER_ID;
use jamstream_protocol::invite::Invite;
use jamstream_protocol::media::FrameDuration;
use jamstream_session::SessionError;
use jamstream_session::client::{ClientCore, ClientState, ClientStats};

use crate::avatar;
use crate::runtime::{
    AvatarHandle, BroadcastReadiness, BroadcastView, ChatLine, Command, ConnState, CostView,
    DestinationView, FaderView, LevelsView, MemberId, MemberView, MetronomeView, RateOutcomeView,
    RateOutcomesView, RecordState, RecordView, Role, Runtime, Snapshot, StatsView, StreamView,
};
use crate::screens::invites::TokenMap;

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
/// Base backoff between attempts to reopen a lost or misconfigured stream.
/// The first attempt of an episode is immediate; each one after it waits
/// twice as long as the last, to [`REOPEN_BACKOFF_MAX`].
const REOPEN_INTERVAL: Duration = Duration::from_millis(500);
/// Longest the reopen loop waits between attempts.
const REOPEN_BACKOFF_MAX: Duration = Duration::from_secs(4);
/// Attempts one episode gets before the loop stops and says so. Six of them
/// span about twelve seconds, which is long enough for a driver to come back
/// and short enough that a musician is not left watching dead meters.
const REOPEN_ATTEMPTS_MAX: u32 = 6;
/// A stream that has run this long has recovered: the episode ends, so the
/// next loss is retried at once and announced again.
const STREAM_SETTLED_AFTER: Duration = Duration::from_secs(5);
/// Longest offline-pump stall replayed sample-for-sample, in seconds of
/// device time; two seconds is comfortably past the server jitter buffer's
/// 512-frame (1.28 s) stream-restart threshold, so an abandoned backlog
/// always trips it.
const PUMP_REPLAY_MAX_SECS: u64 = 2;
/// Synthetic sender id for system chat lines (device notices). Real member
/// ids are assigned from zero, far below this.
const SYSTEM_MEMBER: MemberId = MemberId(u16::MAX);
/// How long playout may hand out nothing but zeros on a joined session before
/// the log says so. The deepest legitimate refill is the buffer's own
/// `MAX_TARGET` of 24 frames, 60 ms, so a second of it is not the buffer
/// filling: it is a member hearing silence.
const SILENT_PLAYOUT_AFTER: Duration = Duration::from_secs(1);
/// How long every pull may conceal on a joined session before the log calls it
/// a dropout. Two bounds set it. Below [`JitterBuffer::HEAL_TICKS`], 210 ms, the
/// gap may still be the buffer fixing a playout position it cannot reconcile,
/// so a warning there would name the cure as the disease. And a quarter second
/// is the loosest silence the harness lets the media path pass, so nothing this
/// warns about is a gap the product already calls acceptable. Under it sits
/// ordinary jitter, a frame or a few, which is what concealment exists to hide.
const CONCEALED_GAP_AFTER: Duration = Duration::from_millis(250);
/// Window the refused-frame rate is measured over, and the count inside it
/// that means the arriving stream disagrees with playout rather than the
/// network dropping the odd packet. Media arrives one frame per tick, 400 a
/// second, and reordering strands a few percent of them; half of a second's
/// frames refused cannot be that.
const REFUSED_WINDOW: Duration = Duration::from_secs(1);
const REFUSED_WINDOW_LIMIT: u64 = 200;
/// Audio the capture ring holds, which is how long the worker may be held up
/// before captured audio is dropped rather than delayed. Forty milliseconds
/// covers the session's own bring-up and a stalled tick, and a stall that long
/// replays as 16 frames arriving at once, well inside the receiving jitter
/// buffer's 64-frame queue. It costs nothing in latency: see
/// [`capture_capacity`].
const CAPTURE_RING: Duration = Duration::from_millis(40);
/// Wait before the ring counters are reported again, and the ceiling that wait
/// doubles to. A burst at open is then one line, while a ring that keeps
/// dropping says so for as long as it does without filling the file: a single
/// once-per-stream count cannot distinguish a burst from a total that is
/// still climbing.
const RING_REPORT_AGAIN: Duration = Duration::from_secs(1);
const RING_REPORT_MAX: Duration = Duration::from_secs(60);
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

/// Playout ring capacity in samples, which doubles as the playout depth
/// target: the top-up loop keeps the ring full, so the device-side cushion
/// sits at ~2x buffer_frames and every sample of it is latency. Floor of one
/// 2.5 ms frame of slack.
fn playout_capacity(buffer_frames: u32) -> usize {
    2 * buffer_frames.max(FRAME_FRAMES as u32) as usize * usize::from(CHANNELS)
}

/// Capture ring capacity in samples: the playout cushion, or
/// [`CAPTURE_RING`] of audio, whichever is larger.
///
/// Deeper than playout because capture depth is not latency: the worker drains
/// this ring to empty every tick, so a sample waits for the next 2.5 ms drain
/// and never for the capacity. Capacity only buys how long the worker may be
/// held up before audio is destroyed, and the session's own bring-up outlasts
/// two callbacks of it.
fn capture_capacity(buffer_frames: u32) -> usize {
    let slack = CAPTURE_RING.as_millis() as usize * SAMPLE_RATE as usize / 1000;
    playout_capacity(buffer_frames).max(slack * usize::from(CHANNELS))
}

/// How long the playout ring may accept nothing before the device counts as
/// having stopped rendering: four device callbacks, since the top-up loop keeps
/// the ring full and a rendering device makes room for one callback on every
/// callback. Clamped between [`PLAYOUT_STALL_FLOOR`] and
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
}

/// Manual rather than derived so the flag defaults on: exclusive is the
/// latency the product exists for, and a derived `false` here would quietly
/// contradict [`StreamConfig::default`].
impl Default for AudioSettings {
    fn default() -> Self {
        AudioSettings {
            capture_id: None,
            playback_id: None,
            buffer_frames: 0,
            allow_exclusive: true,
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
    conn: ConnState,
    rtt_ms: Option<f32>,
    jitter_depth: usize,
    jitter_target: usize,
    loss_pct: f32,
    mouth_to_ear_ms: Option<f32>,
    roster: Vec<MemberInfo>,
    /// Monitor-mix values the UI set, merged over the roster; the server
    /// does not echo MixerSet back.
    faders: HashMap<MemberId, FaderView>,
    /// Broadcast-mix values, from our own optimistic sets and from
    /// BroadcastMixChanged relays; merged over the roster for the host.
    broadcast_faders: HashMap<MemberId, FaderView>,
    /// Client-local optimistic audition state; the server sends no echo.
    audition: bool,
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
    /// reads it the way it reads the connection state rather than being told
    /// once in a chat line it may already have scrolled past.
    device_error: Option<String>,
    /// How each direction of the running stream reaches the session rate,
    /// from the backend's report at open; None while there is no
    /// stream, so the UI never shows a dead stream's outcome.
    rate: Option<RateOutcomesView>,
}

impl SharedState {
    fn new(invite: &Invite, server_addr: SocketAddr) -> Self {
        let session_short = HEXLOWER.encode(&invite.session_id.0[..4]);
        SharedState {
            conn: ConnState::Connecting,
            rtt_ms: None,
            jitter_depth: 0,
            jitter_target: 0,
            loss_pct: 0.0,
            mouth_to_ear_ms: None,
            roster: Vec::new(),
            faders: HashMap::new(),
            broadcast_faders: HashMap::new(),
            audition: false,
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
            rate: None,
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
    /// The playout ring is filled with silence before the stream opens, so the
    /// device's first callback finds it at its steady-state depth rather than
    /// empty. Refilling it from the core instead would burst-pull several
    /// frames in zero wall time, running the jitter consumer clock past the
    /// sender; the buffer can step back at most one frame, so every later
    /// packet would be dropped as late and playout would stay silent for the
    /// rest of the session.
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
            engine.push_playout(&vec![0.0; playout_capacity(frames)]);
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

/// One run of the reopen loop, from the first loss to the stream that stays
/// up for [`STREAM_SETTLED_AFTER`].
///
/// The first attempt of an episode is immediate, so a genuine unplug is
/// reopened on the next tick. Each attempt after it waits twice as long, and
/// the budget stops the loop entirely. Without both, a device that opens and
/// then latches before the next tick was closed and reopened every 2.5 ms:
/// two chat lines a tick emptied the 500-line scrollback in about a second,
/// and a real open costs 10-100 ms, so the rings went unserviced for the
/// whole episode.
#[derive(Debug, Default)]
struct ReopenEpisode {
    attempts: u32,
    /// Said once each per episode. A device that dies on every open would
    /// otherwise alternate two distinct lines forever, which no per-line
    /// dedupe can catch.
    said_stopped: bool,
    said_reopened: bool,
    said_given_up: bool,
}

impl ReopenEpisode {
    /// The wait owed before the next attempt.
    fn backoff(&self) -> Duration {
        match self.attempts.checked_sub(1) {
            None => Duration::ZERO,
            Some(n) => REOPEN_INTERVAL
                .saturating_mul(1u32 << n.min(16))
                .min(REOPEN_BACKOFF_MAX),
        }
    }

    /// Whether the budget is spent and the loop should stop trying.
    fn spent(&self) -> bool {
        self.attempts >= REOPEN_ATTEMPTS_MAX
    }
}

/// Watches the local jitter buffer for the three faults that leave a connected
/// session sounding broken and show up nowhere else: playout handing out zeros
/// because the buffer never filled, playout concealing a gap long enough to
/// hear, and frames arriving only to be refused.
///
/// All three are warnings because the log file promises that an empty file is a
/// healthy run, and a member who heard nobody for a whole session found nothing
/// in it. All three are one line per episode, like the ring counters: at
/// 2.5 ms a tick, warning per tick would put hundreds of lines a second in a
/// file people mail us.
///
/// It reads counters rather than pull outcomes, so it needs no seam through the
/// core. `waiting` moving while `pulled` stands still is exactly a run of
/// `Pull::Waiting`, the one branch that writes literal zeros. `lost` moving
/// with `pulled` frame for frame is a run where every pull concealed, the
/// branch that writes invented audio. And `late` is the frames the buffer
/// refused, which no other surface carries at all.
#[derive(Default)]
struct PlayoutWatch {
    /// Last tick's counters; the deltas are what they mean here.
    prev: Option<JitterStats>,
    /// When the current run of silence began, and whether it has been said.
    silent_since: Option<Instant>,
    silent_said: bool,
    /// The open run of concealed pulls, and whether it has been said.
    gap: Option<Gap>,
    gap_said: bool,
    /// The open refusal window: when it started and `late` as it stood then.
    refused_window: Option<(Instant, u64)>,
    refused_said: bool,
}

/// A run of ticks whose every pull concealed, from the first one seen and the
/// counters as they stood before it, so the line carries the run's own numbers
/// rather than the session's totals.
#[derive(Clone, Copy)]
struct Gap {
    since: Instant,
    lost: u64,
    late: u64,
}

impl PlayoutWatch {
    /// One tick's worth of observation. `joined_as` is the member this client
    /// is joined as, and None whenever it is not joined: before the session is
    /// up nothing is arriving yet, and silence then is the connection's story
    /// to tell.
    fn observe(&mut self, now: Instant, joined_as: Option<MemberId>, stats: JitterStats) {
        let prev = self.prev.replace(stats);
        let Some(member) = joined_as else {
            self.forget();
            return;
        };
        let Some(prev) = prev else { return };
        // A reconnect builds a fresh buffer, so a counter that went backwards
        // is a new stream and not an event.
        if stats.pulled < prev.pulled
            || stats.late < prev.late
            || stats.lost < prev.lost
            || stats.waiting < prev.waiting
        {
            self.forget();
            return;
        }

        // Zeros went out and nothing playable did: the buffer has not filled.
        if stats.waiting > prev.waiting && stats.pulled == prev.pulled {
            let since = *self.silent_since.get_or_insert(now);
            if !self.silent_said && now.duration_since(since) >= SILENT_PLAYOUT_AFTER {
                self.silent_said = true;
                tracing::warn!(
                    member = member.0,
                    depth_frames = stats.depth_frames,
                    target_frames = stats.target_frames,
                    late = stats.late,
                    reanchors = stats.reanchors,
                    silent_ms = now.duration_since(since).as_millis(),
                    "playout is silence: the jitter buffer has not filled"
                );
            }
        } else {
            self.silent_since = None;
            self.silent_said = false;
        }

        // Every pull since the last tick concealed, so what went out was the
        // decoder inventing audio the stream did not carry. A tick that pulled
        // nothing holds the run open rather than ending it: a growth hold
        // conceals too, and a re-anchored buffer plays nothing while it refills.
        let pulled = stats.pulled - prev.pulled;
        if pulled > 0 {
            if stats.lost - prev.lost == pulled {
                self.gap.get_or_insert(Gap {
                    since: now,
                    lost: prev.lost,
                    late: prev.late,
                });
            } else {
                self.gap = None;
                self.gap_said = false;
            }
        }
        if let Some(gap) = self.gap {
            let held = now.duration_since(gap.since);
            if !self.gap_said && held >= CONCEALED_GAP_AFTER {
                self.gap_said = true;
                tracing::warn!(
                    member = member.0,
                    gap_ms = held.as_millis(),
                    concealed = stats.lost - gap.lost,
                    refused = stats.late - gap.late,
                    reanchors = stats.reanchors,
                    depth_frames = stats.depth_frames,
                    target_frames = stats.target_frames,
                    "playout is concealing a gap: nothing arrived in time to play"
                );
            }
        }

        match self.refused_window {
            None => self.refused_window = Some((now, stats.late)),
            Some((from, late_then)) if now.duration_since(from) >= REFUSED_WINDOW => {
                let refused = stats.late - late_then;
                if refused < REFUSED_WINDOW_LIMIT {
                    self.refused_said = false;
                } else if !self.refused_said {
                    self.refused_said = true;
                    tracing::warn!(
                        member = member.0,
                        refused,
                        late = stats.late,
                        depth_frames = stats.depth_frames,
                        target_frames = stats.target_frames,
                        reanchors = stats.reanchors,
                        "media is arriving and being refused: its timing and playout disagree"
                    );
                }
                self.refused_window = Some((now, stats.late));
            }
            Some(_) => {}
        }
    }

    /// Drops every episode without saying anything: the stream this was
    /// watching is gone, and the next one starts its own.
    fn forget(&mut self) {
        self.silent_since = None;
        self.silent_said = false;
        self.gap = None;
        self.gap_said = false;
        self.refused_window = None;
        self.refused_said = false;
    }
}

/// Reports the bridge's dropped-capture and padded-playout counters as the log
/// sees them: the first movement at once, then again on a doubling wait for as
/// long as the count keeps climbing.
///
/// The cadence is the point: a single total, like `overruns=33` on a stream
/// that has been up for a second, cannot say whether that is a burst while
/// the session came up or the first second of a drip that runs for the
/// whole song. Those want different fixes, and the person who can hear the
/// damage is at the other end of the session, so the log is where it has to be
/// answerable. Each line carries the count since the last one and how long the
/// stream has been up, so the shape reads off the timestamps.
struct RingWatch {
    /// When the stream this watches opened; every line is dated from it.
    opened: Instant,
    overruns: CounterWatch,
    underruns: CounterWatch,
}

/// One counter's reporting state.
#[derive(Default)]
struct CounterWatch {
    /// The total as the last line reported it, and when that line went out.
    said: Option<(u64, Instant)>,
    /// The wait owed before this counter is reported again.
    wait: Duration,
}

impl RingWatch {
    fn new(opened: Instant) -> RingWatch {
        RingWatch {
            opened,
            overruns: CounterWatch::default(),
            underruns: CounterWatch::default(),
        }
    }

    /// One tick's worth of observation, against the ring the counters belong to.
    fn observe(&mut self, now: Instant, engine: &EngineSide, ring_frames: u32) {
        let up_ms = now.duration_since(self.opened).as_millis();
        let overruns = engine.overruns();
        if let Some(dropped) = self.overruns.due(now, overruns) {
            tracing::warn!(
                dropped,
                overruns,
                ring_frames,
                up_ms,
                "capture ring overflowed; captured audio was dropped"
            );
        }
        let underruns = engine.underruns();
        if let Some(padded) = self.underruns.due(now, underruns) {
            tracing::warn!(
                padded,
                underruns,
                ring_frames,
                up_ms,
                "playout ring ran dry; the device padded silence"
            );
        }
    }
}

impl CounterWatch {
    /// Whether `total` earns a line now, and the count that line carries.
    fn due(&mut self, now: Instant, total: u64) -> Option<u64> {
        if total == 0 {
            return None;
        }
        match self.said {
            None => {
                self.said = Some((total, now));
                self.wait = RING_REPORT_AGAIN;
                Some(total)
            }
            Some((said, at)) if total > said && now.duration_since(at) >= self.wait => {
                self.said = Some((total, now));
                self.wait = (self.wait * 2).min(RING_REPORT_MAX);
                Some(total - said)
            }
            Some(_) => None,
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
        // The join-time rung disclosure: the worker announces changes on
        // every later open, so the first open's outcome is told here or
        // never.
        state.rate = rate;
        if let Some(rate) = rate {
            for (side, outcome) in [("capture", rate.capture), ("playback", rate.playback)] {
                if let Some(line) = rate_change_line(None, outcome, side) {
                    push_system_line(&mut state, 0, &line);
                }
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
            rings: RingWatch::new(Instant::now()),
            playout: PlayoutWatch::default(),
            settings,
            shared: Arc::clone(&shared),
            rx,
            rx_buf: vec![0u8; MAX_DATAGRAM_BYTES].into_boxed_slice(),
            epoch: Instant::now(),
            capture_buf: vec![0.0; capture_capacity(device_frames)],
            mono_buf: Vec::new(),
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
            announced_failure: None,
            announced_rate: rate,
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
        self.shared.lock().expect("live state").conn.clone()
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
                state: s.conn.clone(),
                rtt_ms: s.rtt_ms,
                jitter_depth: s.jitter_depth,
                jitter_target: s.jitter_target,
                loss_pct: s.loss_pct,
                mouth_to_ear_ms: s.mouth_to_ear_ms,
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
                rate: s.rate,
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
            session_short: s.session_short.clone(),
            server_addr: s.server_addr.clone(),
            is_host,
            device_error: s.device_error.clone(),
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
    /// The bridge counters as the log reports them; nothing else consumes
    /// them, so without this a ring the device outgrows is audible but
    /// invisible.
    rings: RingWatch,
    /// One warn per episode when playout goes silent or media is refused;
    /// nothing else consumes the jitter buffer's counters, so without this
    /// neither would be said.
    playout: PlayoutWatch,
    settings: AudioSettings,
    shared: Arc<Mutex<SharedState>>,
    rx: mpsc::Receiver<ThreadMsg>,
    /// Datagram scratch, allocated once: an avatar chunk is four times a
    /// media packet, and a short buffer would truncate it silently.
    rx_buf: Box<[u8]>,
    epoch: Instant,
    capture_buf: Vec<f32>,
    mono_buf: Vec<f32>,
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
    /// The failure line last put in chat, so the retry cadence announces each
    /// distinct reason once instead of once per attempt.
    announced_failure: Option<String>,
    /// The rate outcomes last announced in chat, so a reopen on the same
    /// rung says nothing and a rung change is said exactly once.
    announced_rate: Option<RateOutcomesView>,
}

impl Worker {
    fn run(mut self) {
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
        let now_ms = self.now_ms();

        loop {
            match self.rx.try_recv() {
                Ok(ThreadMsg::Cmd(Command::Leave)) => {
                    self.shutdown(now_ms);
                    return false;
                }
                Ok(ThreadMsg::Cmd(cmd)) => self.apply_command(cmd),
                Ok(ThreadMsg::Reconfigure(settings)) => self.reconfigure(settings),
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
            let _ = self.socket.send(&pkt);
        }
        self.drain_events(now_ms);
        let stats = self.core.stats();
        self.watch_playout(&stats);
        self.publish_stats(&stats);
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
            let _ = self.socket.send(&pkt);
        }
        self.driver.close();
        self.shared.lock().expect("live state").conn = ConnState::Idle;
    }

    /// Closes and reopens the audio stream with new settings; the network
    /// side never pauses. On failure the user's selection is kept and the
    /// reopen cadence keeps trying exactly it: rewriting the settings to the
    /// system default here would leave the Audio tab claiming a device the
    /// stream does not run, with only a chat line to say otherwise. The
    /// refusal itself stays on screen through `device_error`.
    fn reconfigure(&mut self, settings: AudioSettings) {
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
        // one spent, and anything already said about it, do not carry over.
        self.episode = ReopenEpisode::default();
        self.announced_failure = None;
        self.attempt_open();
    }

    /// A dead stream is closed, announced for what is known about it, and
    /// retried with the same settings on the episode's widening cadence. The
    /// old line claimed "audio device disconnected" for every latched error,
    /// but the exclusive path latches on any read or write hiccup, so a
    /// driver stutter was reported as an unplug; the class is only knowable
    /// from the reopen attempt, and the announcement waits for it.
    fn check_stream(&mut self) {
        if self.driver.errored() {
            self.driver.close();
            self.engine = None;
            self.opened_at = None;
            // A dead stream has no rate outcome to show.
            self.shared.lock().expect("live state").rate = None;
            if !self.episode.said_stopped {
                self.episode.said_stopped = true;
                self.system_line("the audio stream stopped; retrying");
            }
        }
        if self
            .opened_at
            .is_some_and(|t| t.elapsed() >= STREAM_SETTLED_AFTER)
        {
            self.opened_at = None;
            self.episode = ReopenEpisode::default();
        }
        if self.engine.is_some() {
            return;
        }
        if self.episode.spent() {
            if !self.episode.said_given_up {
                self.episode.said_given_up = true;
                self.system_line(&format!(
                    "the audio device did not stay open after {REOPEN_ATTEMPTS_MAX} tries; \
                     pick a device on the Audio tab to try again"
                ));
            }
            return;
        }
        let backoff = self.episode.backoff();
        if self.last_reopen.is_none_or(|t| t.elapsed() >= backoff) {
            self.attempt_open();
        }
    }

    /// One open attempt against the episode's budget. It sets the cadence
    /// clock whether it succeeds or not, so a device that opens and then dies
    /// before the next tick escalates exactly like one that refuses outright:
    /// an open that does not last is not progress.
    fn attempt_open(&mut self) {
        self.last_reopen = Some(Instant::now());
        self.reopen_attempts += 1;
        self.episode.attempts += 1;
        tracing::warn!(
            attempt = self.reopen_attempts,
            in_episode = self.episode.attempts,
            "reopening audio stream"
        );
        match self.try_open() {
            Ok(()) => {
                if self.episode.said_stopped && !self.episode.said_reopened {
                    self.episode.said_reopened = true;
                    self.system_line("audio device reopened");
                }
            }
            Err(err) => self.announce_open_failure(&err),
        }
    }

    /// One chat line per distinct failure, in the device's own words, so the
    /// retry cadence does not flood the conversation. Disconnection is
    /// claimed only when the error class says the device is gone.
    fn announce_open_failure(&mut self, err: &AudioError) {
        let line = match err {
            AudioError::DeviceGone => "audio device disconnected; retrying".to_owned(),
            refused => format!("audio device refused: {}", refused.detail()),
        };
        if self.announced_failure.as_deref() != Some(line.as_str()) {
            self.system_line(&line);
            self.announced_failure = Some(line);
        }
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
                self.rings = RingWatch::new(Instant::now());
                self.carry_pos = 0;
                self.carry_len = 0;
                // The ring opens full of silence, so the first callback owes
                // this stream nothing yet.
                self.ring_took = Instant::now();
                let mut shared = self.shared.lock().expect("live state");
                shared.reopen_attempts = self.reopen_attempts;
                shared.device_error = None;
                shared.rate = rate;
                drop(shared);
                self.announced_failure = None;
                self.announce_rate(rate);
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
                shared.rate = None;
                Err(err)
            }
        }
    }

    /// One chat line per direction whose rung changed at this open: rungs 2
    /// and 3 are said once, never per reopen; rung 1 and the OS converter add
    /// nothing here (the latency hover carries them).
    fn announce_rate(&mut self, rate: Option<RateOutcomesView>) {
        let Some(rate) = rate else { return };
        let old = self.announced_rate;
        for (side, old, new) in [
            ("capture", old.map(|r| r.capture), rate.capture),
            ("playback", old.map(|r| r.playback), rate.playback),
        ] {
            if let Some(line) = rate_change_line(old, new, side) {
                self.system_line(&line);
            }
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
                let _ = self.socket.send(&pkt);
            }
        }
    }

    /// Device-paced capture: whatever arrived in the ring goes through the
    /// raw path, which emits zero or more sealed frames.
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
            for pkt in self.core.push_capture_raw(now_ms, &self.mono_buf) {
                let _ = self.socket.send(&pkt);
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

    /// Keeps the playout ring full. Capacity is 2x the device buffer, so a
    /// full ring is the target depth; the carry holds anything the ring
    /// refused so no decoded audio is dropped.
    fn top_up_playout(&mut self) {
        let mut inst_peak = 0.0f32;
        let mut inst_sq = 0.0f32;
        let mut n = 0usize;
        let mut took = false;
        if let Some(engine) = self.engine.as_mut() {
            loop {
                if self.carry_pos < self.carry_len {
                    let pushed = engine.push_playout(&self.carry[self.carry_pos..self.carry_len]);
                    took |= pushed > 0;
                    self.carry_pos += pushed;
                    if self.carry_pos < self.carry_len {
                        break; // ring is full
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

    /// The bridge counters, reported by [`RingWatch`]. Movement means a ring
    /// too shallow for what the device delivers or for what the worker is
    /// keeping up with; the log is the one place that class of defect shows
    /// as something other than bad audio somebody else can hear.
    fn watch_ring_health(&mut self) {
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        self.rings
            .observe(Instant::now(), engine, self.device_frames);
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
        let mut s = self.shared.lock().expect("live state");
        if matches!(stats.state, ClientState::Joined) {
            self.ever_joined = true;
        }
        // Idle is terminal (set by shutdown); never overwrite it.
        if s.conn != ConnState::Idle {
            s.conn = conn_state_with(&stats.state, stats.session_full);
        }
        s.me = self.core.member_id();
        s.rtt_ms = stats.rtt_ms_last;
        s.jitter_depth = stats.jitter.depth_frames;
        s.jitter_target = stats.jitter.target_frames;
        s.loss_pct = loss_pct(stats);
        // Mouth to ear, capture to playout:
        //   rtt / 2                      the downlink network leg
        // + jitter depth * 2.5 ms        playout buffering ahead of decode
        // + 2.5 ms                       one media frame of encode latency
        // + device_frames / 48 ms        the capture device buffer, as the
        //                                device negotiated it, not as asked
        // + converter added ms           the boundary resampler's disclosed
        //                                figure, per converted direction
        let convert_ms = s.rate.map_or(0.0, |r| r.added_ms());
        s.mouth_to_ear_ms = stats.rtt_ms_last.map(|rtt| {
            rtt / 2.0
                + stats.jitter.depth_frames as f32 * 2.5
                + 2.5
                + self.device_frames as f32 / 48.0
                + convert_ms
        });
        s.levels = self.levels;
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

    fn system_line(&self, text: &str) {
        let at_ms = self.now_ms();
        push_system_line(&mut self.shared.lock().expect("live state"), at_ms, text);
    }
}

/// A system chat line, logged and pushed: [`Worker::system_line`] plus the
/// join-time rate disclosure in [`LiveRuntime::start`], which runs before a
/// worker exists.
fn push_system_line(state: &mut SharedState, at_ms: u64, text: &str) {
    tracing::info!(text, "audio notice");
    state.push_chat(ChatLine {
        from_name: "system".to_owned(),
        from_id: SYSTEM_MEMBER,
        text: text.to_owned(),
        at_ms,
    });
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

/// The chat notice one direction's rung earns at an open, given the rung it
/// was on before. Rung 1 is not news, the OS converter is hover-only, and an
/// unchanged rung is silence, so the reopen cadence can never flood
/// the room. A converter that replaces a clock this app had set names the
/// contest: that is the one demotion a musician might otherwise chase into
/// their other software's settings.
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

/// Loss for the status bar: the worse of the downlink (local jitter buffer,
/// cumulative) and the server's view of our uplink.
fn loss_pct(stats: &ClientStats) -> f32 {
    let down = if stats.jitter.pulled == 0 {
        0.0
    } else {
        stats.jitter.lost as f32 * 100.0 / stats.jitter.pulled as f32
    };
    down.max(stats.uplink_loss_pct.unwrap_or(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamstream_engine::MediaPacket;

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

    /// The sizing that matters: the rings must fit the callbacks the
    /// device really delivers, and the request is only a lower bound.
    #[test]
    fn the_ring_is_sized_from_what_the_device_delivers() {
        // WASAPI shared mode: 120 asked for, the ~10 ms device period given.
        assert_eq!(ring_frames(120, Some(480)), 480);
        assert_eq!(
            playout_capacity(ring_frames(120, Some(480))),
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

    /// What the two capacities cost, which is why they are two. The
    /// playout ring is held full, so its capacity is mouth-to-ear and stays at
    /// the two callbacks of headroom the design settles on. The capture ring
    /// is drained to empty, so its capacity is only stall tolerance and buys
    /// 40 ms of it.
    #[test]
    fn the_capture_ring_is_deeper_than_the_playout_cushion_and_costs_nothing() {
        let ms =
            |samples: usize| samples as f64 / f64::from(CHANNELS) / f64::from(SAMPLE_RATE) * 1000.0;
        for frames in [32u32, 120, 240] {
            assert_eq!(
                ms(playout_capacity(frames)),
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
        assert_eq!(capture_capacity(2_400), playout_capacity(2_400));
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

    /// The chat copy per rung change, the rate-rung disclosure contract: rung
    /// 2 and rung 3 are announced once, an unchanged rung and rung 1 say
    /// nothing (so the reopen cadence cannot flood chat), the OS converter
    /// stays hover-only, and a converter that replaced a clock this app set
    /// names the contest instead of reading like a random downgrade.
    #[test]
    fn rung_changes_earn_one_honest_chat_line() {
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
            "returning to native is visible in the tag, not the chat"
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

    /// The cadence a device that keeps dying is retried on: the first loss at
    /// once, so a genuine unplug comes back on the next tick, then a doubling
    /// wait to the ceiling, then a stop. Before this the wait was cleared on
    /// every loss, so a device that latched between ticks was closed and
    /// reopened ~400 times a second for the rest of the session.
    #[test]
    fn the_reopen_cadence_widens_and_then_gives_up() {
        let mut episode = ReopenEpisode::default();
        let mut waits = Vec::new();
        while !episode.spent() {
            waits.push(episode.backoff());
            episode.attempts += 1;
        }
        assert_eq!(waits.len(), REOPEN_ATTEMPTS_MAX as usize, "{waits:?}");
        assert_eq!(waits[0], Duration::ZERO, "a one-shot loss reopens at once");
        assert_eq!(waits[1], REOPEN_INTERVAL);
        for pair in waits[1..].windows(2) {
            assert_eq!(
                pair[1],
                (pair[0] * 2).min(REOPEN_BACKOFF_MAX),
                "the wait must double to the ceiling and stay there: {waits:?}"
            );
        }
        // Long enough that a driver settling has a chance, short enough that
        // nobody watches dead meters for a minute.
        let span: Duration = waits.iter().sum();
        assert!(
            span > Duration::from_secs(5) && span < Duration::from_secs(30),
            "the whole budget spans {span:?}"
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

    /// The member the watched buffer plays for.
    const ME: MemberId = MemberId(7);

    /// Formatted log lines, behind the app's own default filter, so a test
    /// says both what the log file would hold and that these events are
    /// warnings rather than something the file never sees.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn lines(&self) -> Vec<String> {
            String::from_utf8(self.0.lock().expect("captured log").clone())
                .expect("log is utf8")
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("captured log").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    /// Runs `body` against a capturing subscriber carrying the CLI's default
    /// filter, which is the one the log file is written through.
    fn captured(body: impl FnOnce()) -> Vec<String> {
        use tracing_subscriber::layer::SubscriberExt as _;
        let cap = Captured::default();
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(cap.clone()),
            )
            .with(jamstream_cli::logging::filter(None));
        tracing::subscriber::with_default(subscriber, body);
        cap.lines()
    }

    /// A real jitter buffer, the watch over it, and a synthetic mix clock: one
    /// tick is one 2.5 ms pull, so the watch sees exactly what it sees in the
    /// worker, produced by the buffer itself rather than by a stand-in.
    struct Ticker {
        jitter: JitterBuffer,
        watch: PlayoutWatch,
        start: Instant,
        tick: u32,
    }

    impl Ticker {
        fn new() -> Ticker {
            Ticker {
                jitter: JitterBuffer::new(),
                watch: PlayoutWatch::default(),
                start: Instant::now(),
                tick: 0,
            }
        }

        /// `ticks` mix ticks, with `feed` handed the buffer and the tick number
        /// before each pull.
        fn run(
            &mut self,
            ticks: u32,
            joined_as: Option<MemberId>,
            mut feed: impl FnMut(&mut JitterBuffer, u32),
        ) {
            for _ in 0..ticks {
                feed(&mut self.jitter, self.tick);
                self.jitter.pull();
                let at = self.start + TICK * self.tick;
                self.watch.observe(at, joined_as, self.jitter.stats());
                self.tick += 1;
            }
        }
    }

    fn media(seq: u32) -> MediaPacket {
        MediaPacket {
            seq,
            timestamp: u64::from(seq) * FRAME_FRAMES as u64,
            payload: vec![0u8; 8],
            redundant: None,
        }
    }

    /// This tick's frame, in time, every tick.
    fn healthy(jitter: &mut JitterBuffer, tick: u32) {
        jitter.push(media(tick));
    }

    /// Nothing arrives at all, so every pull has nothing to play.
    fn nothing(_: &mut JitterBuffer, _: u32) {}

    /// This tick's frame every tick, save one that never arrives: the single
    /// concealed frame ordinary jitter produces, and the whole reason the
    /// decoder conceals.
    fn one_frame_short(jitter: &mut JitterBuffer, tick: u32) {
        if tick != 200 {
            jitter.push(media(tick));
        }
    }

    /// This tick's frame plus a copy of the one from 100 ms back, which is
    /// behind playout by more than the buffer is deep and is refused for it.
    fn stale_copies(jitter: &mut JitterBuffer, tick: u32) {
        jitter.push(media(tick));
        if let Some(old) = tick.checked_sub(40) {
            jitter.push(media(old));
        }
    }

    /// A joined client handed no media at all hears silence for the whole
    /// session. The log names it in one line, giving the numbers that
    /// separate "nothing is arriving" from "arriving and being refused", and
    /// one line only: three seconds of it at 2.5 ms a tick would otherwise be
    /// 1200 of them.
    #[test]
    fn a_client_handed_no_media_says_so_once() {
        let lines = captured(|| Ticker::new().run(1_200, Some(ME), |_, _| {}));
        assert_eq!(lines.len(), 1, "{lines:#?}");
        let line = &lines[0];
        assert!(
            line.contains("WARN"),
            "not a warning, so the file never sees it: {line}"
        );
        assert!(line.contains("playout is silence"), "{line}");
        for field in [
            "member=7",
            "depth_frames=0",
            "target_frames=1",
            "late=0",
            "reanchors=0",
        ] {
            assert!(line.contains(field), "no {field} in {line}");
        }
    }

    /// Media that arrives and is refused: the counterpart to a client
    /// hearing nothing at all. Every tick carries this tick's frame and a
    /// copy of one from 100 ms back, which is behind playout and dropped, so
    /// `late` climbs while depth stays at target and audio keeps playing.
    /// The reader has to be able to tell this from hearing nothing, because
    /// the causes are nothing alike.
    #[test]
    fn a_client_whose_media_is_refused_says_something_else() {
        let lines = captured(|| Ticker::new().run(1_200, Some(ME), stale_copies));
        assert_eq!(lines.len(), 1, "{lines:#?}");
        let line = &lines[0];
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains("being refused"), "{line}");
        assert!(
            !line.contains("playout is silence"),
            "a refused stream must not read as a silent one: {line}"
        );
        for field in [
            "member=7",
            "late=",
            "refused=",
            "depth_frames=",
            "reanchors=0",
        ] {
            assert!(line.contains(field), "no {field} in {line}");
        }
    }

    /// The direction that matters more: an ordinary session says nothing at
    /// all. The banner promises an empty file means a healthy run, so a watch
    /// that fires on the couple of ticks every start spends filling would cost
    /// the file its only claim.
    #[test]
    fn an_ordinary_stream_says_nothing() {
        let lines = captured(|| Ticker::new().run(1_200, Some(ME), healthy));
        assert!(lines.is_empty(), "{lines:#?}");
    }

    /// A stream that stops for a second and comes back. The buffer anchored
    /// long ago, so every pull conceals rather than waiting, and concealment has
    /// energy: no surface but this one can say the musician heard nothing. One
    /// line, and it names the gap's own length.
    #[test]
    fn a_dropout_says_how_long_it_lasted() {
        let lines = captured(|| {
            let mut t = Ticker::new();
            t.run(400, Some(ME), healthy);
            t.run(400, Some(ME), nothing);
            t.run(400, Some(ME), healthy);
        });
        assert_eq!(lines.len(), 1, "{lines:#?}");
        let line = &lines[0];
        assert!(
            line.contains("WARN"),
            "not a warning, so the file never sees it: {line}"
        );
        assert!(line.contains("concealing a gap"), "{line}");
        assert!(
            !line.contains("playout is silence"),
            "a buffer that anchored and ran dry is not one that never filled: {line}"
        );
        // The synthetic clock advances exactly one tick per pull, so the
        // reported length is the threshold to the millisecond.
        for field in [
            "member=7",
            "gap_ms=250",
            "concealed=101",
            "refused=0",
            "reanchors=0",
            "depth_frames=0",
        ] {
            assert!(line.contains(field), "no {field} in {line}");
        }
    }

    /// The frame the decoder exists to hide says nothing. A watch that fired on
    /// one concealed pull would warn on every session that ever loses a packet.
    #[test]
    fn a_single_concealed_frame_says_nothing() {
        let lines = captured(|| Ticker::new().run(1_200, Some(ME), one_frame_short));
        assert!(lines.is_empty(), "{lines:#?}");
    }

    /// 200 ms of concealment says nothing either: it is inside the longest gap
    /// the buffer closes on its own, and inside the silence the harness lets the
    /// media path pass, so it cannot yet be called a fault.
    #[test]
    fn a_gap_the_buffer_could_still_heal_says_nothing() {
        let lines = captured(|| {
            let mut t = Ticker::new();
            t.run(400, Some(ME), healthy);
            t.run(80, Some(ME), nothing);
            t.run(400, Some(ME), healthy);
        });
        assert!(lines.is_empty(), "{lines:#?}");
    }

    /// Recovery ends the episode, so a session that drops out twice says so
    /// twice. Otherwise the second half of a bad session reads as clean.
    #[test]
    fn a_second_dropout_is_its_own_episode() {
        let lines = captured(|| {
            let mut t = Ticker::new();
            for _ in 0..2 {
                t.run(400, Some(ME), healthy);
                t.run(400, Some(ME), nothing);
            }
            t.run(400, Some(ME), healthy);
        });
        assert_eq!(lines.len(), 2, "{lines:#?}");
        for line in &lines {
            assert!(line.contains("concealing a gap"), "{line}");
            assert!(line.contains("gap_ms=250"), "{line}");
        }
    }

    /// The threshold's floor, held against the buffer that sets it: a gap
    /// shorter than the buffer's own healing bound may be the buffer working.
    #[test]
    fn the_dropout_threshold_clears_the_buffer_healing_itself() {
        let heal = TICK * JitterBuffer::HEAL_TICKS;
        assert!(
            CONCEALED_GAP_AFTER > heal,
            "{CONCEALED_GAP_AFTER:?} would warn while the buffer is still \
             recovering, which takes up to {heal:?}"
        );
    }

    /// Silence before the session is up belongs to the connection, which
    /// reports itself. Warning here would put a line in every run that starts
    /// with a server slow to answer.
    #[test]
    fn silence_before_joining_says_nothing() {
        let lines = captured(|| Ticker::new().run(1_200, None, |_, _| {}));
        assert!(lines.is_empty(), "{lines:#?}");
    }

    /// A reconnect hands the watch a fresh buffer whose counters restart at
    /// zero. That is a new stream and not an event: counters that ran up and
    /// then dropped must read as a restart, or the refusal window subtracts a
    /// spent count from an empty one.
    #[test]
    fn a_reconnected_stream_starts_its_own_episode() {
        let lines = captured(|| {
            let mut t = Ticker::new();
            t.run(1_200, Some(ME), stale_copies);
            t.jitter = JitterBuffer::new();
            t.run(1_200, Some(ME), healthy);
        });
        assert_eq!(
            lines.len(),
            1,
            "the healthy stream after it said nothing new"
        );
        assert!(lines[0].contains("being refused"), "{:?}", lines[0]);
    }

    /// A bridge whose capture ring is full, so every push overruns and every
    /// playback callback underruns: one event per call, on demand.
    fn full_ring() -> (jamstream_audio_io::DeviceSide, EngineSide) {
        let (mut device, engine) = CallbackBridge::new(4, 4);
        device.on_capture(&[1.0; 4]);
        (device, engine)
    }

    /// The shape a single total misses: drops that happen in a burst and
    /// then stop say so once. The count, the ring, and how long the
    /// stream had been up all ride the line, because those are what separate a
    /// burst at open from a drip.
    #[test]
    fn a_burst_of_dropped_capture_says_so_once() {
        let start = Instant::now();
        let lines = captured(|| {
            let (mut device, engine) = full_ring();
            let mut watch = RingWatch::new(start);
            for _ in 0..8 {
                device.on_capture(&[1.0; 4]);
            }
            // A minute of ticks after the burst, at the loop's own cadence.
            for tick in 0..24_000u32 {
                watch.observe(start + TICK * tick, &engine, 120);
            }
        });
        assert_eq!(lines.len(), 1, "{lines:#?}");
        let line = &lines[0];
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains("capture ring overflowed"), "{line}");
        for field in ["dropped=8", "overruns=8", "ring_frames=120", "up_ms=0"] {
            assert!(line.contains(field), "no {field} in {line}");
        }
    }

    /// The other shape, and the one that matters: a ring that keeps dropping
    /// keeps saying so, on a widening cadence, each line carrying what was lost
    /// since the last. One line per stream would have said 33 and then nothing
    /// for the rest of the song.
    #[test]
    fn capture_that_keeps_dropping_keeps_saying_so() {
        let start = Instant::now();
        let lines = captured(|| {
            let (mut device, engine) = full_ring();
            let mut watch = RingWatch::new(start);
            // Ten seconds, dropping one callback every 100 ms.
            for tick in 0..4_000u32 {
                if tick % 40 == 0 {
                    device.on_capture(&[1.0; 4]);
                }
                watch.observe(start + TICK * tick, &engine, 120);
            }
        });
        assert!(lines.len() >= 4, "{lines:#?}");
        for line in &lines {
            assert!(line.contains("capture ring overflowed"), "{line}");
        }
        // The first line is the first drop; each one after it waits twice as
        // long, so ten seconds of dropping costs four lines and not four
        // hundred.
        let up_ms: Vec<u64> = lines
            .iter()
            .map(|line| {
                let at = line.split("up_ms=").nth(1).expect("up_ms");
                at.split_whitespace()
                    .next()
                    .expect("a value")
                    .parse()
                    .expect("a number")
            })
            .collect();
        assert_eq!(up_ms[0], 0, "{up_ms:?}");
        for pair in up_ms[1..].windows(2) {
            let widened = (pair[1] - pair[0]) as f64 / (pair[0].max(1)) as f64;
            assert!(widened > 0.5, "the wait did not widen: {up_ms:?}");
        }
        // Every drop is accounted for across the lines, none counted twice.
        let dropped: u64 = lines
            .iter()
            .map(|line| {
                let at = line.split("dropped=").nth(1).expect("dropped");
                at.split_whitespace()
                    .next()
                    .expect("a value")
                    .parse::<u64>()
                    .expect("a number")
            })
            .sum();
        let last: u64 = lines
            .last()
            .and_then(|line| line.split("overruns=").nth(1))
            .and_then(|at| at.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .expect("a total on the last line");
        assert_eq!(dropped, last, "the deltas must add up to the total");
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
        engine.push_playout(&vec![0.0; playout_capacity(FRAMES)]);

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
        // playout once per 2.5 ms tick.
        let silence = vec![0.0f32; playout_capacity(FRAMES)];
        let mut pulled = early;
        let deadline = Instant::now() + RUN;
        let mut next = Instant::now() + TICK;
        while Instant::now() < deadline {
            pulled += engine.pull_captured(&mut capture_buf);
            while engine.push_playout(&silence) > 0 {}
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

    /// A ring nothing has gone wrong with says nothing at all, which is what
    /// lets an empty log file mean a healthy run.
    #[test]
    fn a_ring_with_room_says_nothing() {
        let start = Instant::now();
        let lines = captured(|| {
            let (mut device, mut engine) = CallbackBridge::new(64, 64);
            let mut watch = RingWatch::new(start);
            for tick in 0..4_000u32 {
                device.on_capture(&[1.0; 8]);
                let mut buf = [0.0f32; 64];
                engine.pull_captured(&mut buf);
                engine.push_playout(&[0.5; 8]);
                let mut out = [0.0f32; 8];
                device.on_playback(&mut out);
                watch.observe(start + TICK * tick, &engine, 120);
            }
        });
        assert!(lines.is_empty(), "{lines:#?}");
    }

    /// The two counters are reported apart: a stream that pads playout and
    /// never drops capture says so about playout only, in the sentence that
    /// names the device as the one padding.
    #[test]
    fn a_dry_playout_ring_says_that_instead() {
        let start = Instant::now();
        let lines = captured(|| {
            let (mut device, engine) = CallbackBridge::new(64, 64);
            let mut watch = RingWatch::new(start);
            let mut out = [0.0f32; 8];
            device.on_playback(&mut out);
            for tick in 0..400u32 {
                watch.observe(start + TICK * tick, &engine, 120);
            }
        });
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert!(lines[0].contains("padded silence"), "{:?}", lines[0]);
        assert!(lines[0].contains("padded=1"), "{:?}", lines[0]);
        assert!(
            !lines[0].contains("capture ring"),
            "a dry playout ring is not a dropped capture: {:?}",
            lines[0]
        );
    }
}
