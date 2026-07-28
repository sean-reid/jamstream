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

use jamstream_audio_io::{
    AudioBackend, AudioError, CallbackBridge, EngineSide, StreamConfig, StreamHandle, WavBackend,
    WavStream,
};
use jamstream_protocol::control::{MAX_DATAGRAM_BYTES, MemberInfo, StreamOp};
use jamstream_protocol::ids::HOST_MEMBER_ID;
use jamstream_protocol::invite::Invite;
use jamstream_session::SessionError;
use jamstream_session::client::{ClientCore, ClientState, ClientStats};

use crate::avatar;
use crate::runtime::{
    AvatarHandle, BroadcastView, ChatLine, Command, ConnState, CostView, DestinationView,
    FaderView, LevelsView, MemberId, MemberView, MetronomeView, Role, Runtime, Snapshot, StatsView,
    StreamView,
};
use crate::screens::invites::TokenMap;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const TICK: Duration = Duration::from_micros(2_500);
/// One 2.5 ms frame: 120 mono capture samples, 240 interleaved playout.
const FRAME_FRAMES: usize = 120;
const CHUNK_STEREO: usize = FRAME_FRAMES * 2;
const CHAT_LIMIT: usize = 500;
/// Meter fall per 2.5 ms tick; roughly a 170 ms half-life so levels look
/// alive at snapshot rate without flickering per packet.
const LEVEL_DECAY: f32 = 0.99;
/// Backoff between attempts to reopen a lost or misconfigured stream.
const REOPEN_INTERVAL: Duration = Duration::from_millis(500);
/// Longest offline-pump stall replayed sample-for-sample; two seconds is
/// comfortably past the server jitter buffer's 512-frame (1.28 s)
/// stream-restart threshold, so an abandoned backlog always trips it.
const PUMP_REPLAY_MAX: u64 = 2 * SAMPLE_RATE as u64;
/// Synthetic sender id for system chat lines (device notices). Real member
/// ids are assigned from zero, far below this.
const SYSTEM_MEMBER: MemberId = MemberId(u16::MAX);

/// Ring capacity in samples. It doubles as the playout depth target: the
/// top-up loop keeps the ring full, so the device-side cushion sits at
/// ~2x buffer_frames. Floor of one 2.5 ms frame of slack.
fn ring_capacity(buffer_frames: u32) -> usize {
    2 * buffer_frames.max(FRAME_FRAMES as u32) as usize * usize::from(CHANNELS)
}

/// Device selection plus buffer size, as picked on the settings screen.
/// `None` device ids select the system default for that direction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioSettings {
    pub capture_id: Option<String>,
    pub playback_id: Option<String>,
    pub buffer_frames: u32,
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
}

impl SharedState {
    fn new(invite: &Invite, server_addr: SocketAddr) -> Self {
        let session_short = invite.session_id.0[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
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
    fn open(&mut self, settings: &AudioSettings) -> Result<EngineSide, AudioError> {
        let buffer_frames = settings.buffer_frames.max(32);
        let config = StreamConfig {
            sample_rate: SAMPLE_RATE,
            buffer_frames,
            channels: CHANNELS,
        };
        let (device, engine) = CallbackBridge::new(ring_capacity(buffer_frames));
        match self {
            Driver::Real { backend, handle } => {
                let new = backend.open_duplex(
                    settings.capture_id.as_deref(),
                    settings.playback_id.as_deref(),
                    config,
                    device.into_handler(),
                )?;
                *handle = Some(new);
            }
            Driver::Offline {
                backend,
                stream,
                epoch,
                pumped_frames,
            } => {
                *stream = Some(Box::new(
                    backend.open_offline(config, device.into_handler())?,
                ));
                *epoch = Instant::now();
                *pumped_frames = 0;
            }
        }
        Ok(engine)
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

    fn errored(&self) -> bool {
        match self {
            Driver::Real { handle, .. } => handle.as_ref().is_some_and(|h| h.errored()),
            Driver::Offline { .. } => false,
        }
    }

    /// Offline only: advance the WAV stream by at most one 2.5 ms bite when
    /// wall time owes it one. Returns whether it pumped, so the worker can
    /// service the rings between bites; the rings are only a couple of
    /// device buffers deep, and pumping a whole catch-up burst against
    /// unserviced rings would play silence and drop capture.
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
        let due = (epoch.elapsed().as_secs_f64() * f64::from(SAMPLE_RATE)) as u64;
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
        if backlog > PUMP_REPLAY_MAX {
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
        let engine = driver.open(&settings).map_err(LiveError::Audio)?;
        let socket = connect_socket(addr).map_err(LiveError::Io)?;
        let (core, init) = ClientCore::connect(invite, 0).map_err(LiveError::Session)?;
        let _ = socket.send(&init);

        let shared = Arc::new(Mutex::new(SharedState::new(invite, addr)));
        let (tx, rx) = mpsc::channel();
        let capture_capacity = ring_capacity(settings.buffer_frames.max(32));
        let worker = Worker {
            core,
            socket,
            addresses: invite.addresses.clone(),
            addr_idx: 0,
            ever_joined: false,
            driver,
            engine: Some(engine),
            settings,
            shared: Arc::clone(&shared),
            rx,
            rx_buf: vec![0u8; MAX_DATAGRAM_BYTES].into_boxed_slice(),
            epoch: Instant::now(),
            capture_buf: vec![0.0; capture_capacity],
            mono_buf: Vec::new(),
            carry: [0.0; CHUNK_STEREO],
            carry_pos: 0,
            carry_len: 0,
            levels: LevelsView::default(),
            avatar_failed: HashSet::new(),
            reopen_attempts: 0,
            last_reopen: None,
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
            },
            members,
            chat: s.chat.iter().cloned().collect(),
            levels: s.levels,
            metronome: s.metronome,
            broadcast,
            stream: StreamView {
                destinations: s.stream.clone(),
            },
            // The wizard's [`CostedRuntime`] wrapper fills this for
            // sessions this app launched; plain joins have no meter.
            cost: None,
            session_short: s.session_short.clone(),
            server_addr: s.server_addr.clone(),
            is_host,
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
                Command::SendChat(_)
                | Command::Leave
                | Command::Revoke(_)
                | Command::AddDestination { .. }
                | Command::RemoveDestination(_)
                | Command::StartStream
                | Command::StopStream => {}
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
}

impl Runtime for Arc<LiveRuntime> {
    fn snapshot(&self) -> Snapshot {
        self.snapshot_now()
    }

    fn send(&self, cmd: Command) {
        self.send_cmd(cmd);
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
    levels: LevelsView,
    /// Hashes whose bytes did not decode; never retried, so one bad avatar
    /// costs one decode attempt per session.
    avatar_failed: HashSet<String>,
    reopen_attempts: u64,
    last_reopen: Option<Instant>,
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

        let now_ms = self.now_ms();
        for pkt in self.core.poll(now_ms) {
            let _ = self.socket.send(&pkt);
        }
        self.drain_events(now_ms);
        self.publish_stats();
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
    /// side never pauses. On failure the periodic reopen path takes over
    /// with system defaults.
    fn reconfigure(&mut self, settings: AudioSettings) {
        // Drain what the old ring already captured so those samples reach
        // the core before the endpoints are dropped; orphaning them would
        // shift our uplink frame clock behind the server's.
        let now_ms = self.now_ms();
        self.move_capture(now_ms);
        self.driver.close();
        self.engine = None;
        self.settings = settings;
        if !self.try_open() {
            self.system_line("audio device change failed, falling back to the system default");
            self.settings.capture_id = None;
            self.settings.playback_id = None;
            self.last_reopen = None;
        }
    }

    /// Device-gone handling: a dead stream is closed, announced, and
    /// replaced by the system default on a 500 ms retry cadence.
    fn check_stream(&mut self) {
        if self.driver.errored() {
            self.driver.close();
            self.engine = None;
            self.settings.capture_id = None;
            self.settings.playback_id = None;
            self.last_reopen = None;
            self.system_line("audio device disconnected, switching to the system default");
        }
        if self.engine.is_none()
            && self
                .last_reopen
                .is_none_or(|t| t.elapsed() >= REOPEN_INTERVAL)
        {
            self.last_reopen = Some(Instant::now());
            self.reopen_attempts += 1;
            tracing::warn!(attempt = self.reopen_attempts, "reopening audio stream");
            if self.try_open() {
                self.system_line("audio device reopened");
            }
        }
    }

    fn try_open(&mut self) -> bool {
        match self.driver.open(&self.settings) {
            Ok(mut engine) => {
                let capacity = ring_capacity(self.settings.buffer_frames.max(32));
                self.capture_buf.resize(capacity, 0.0);
                // Prefill the fresh playout ring (its steady-state depth) with
                // silence. Refilling it from the core would burst-pull several
                // frames in zero wall time, running the jitter consumer clock
                // past the sender; the buffer can step back at most one frame,
                // so every later packet would be dropped as late and playout
                // would stay silent for the rest of the session.
                engine.push_playout(&vec![0.0; capacity]);
                self.engine = Some(engine);
                self.carry_pos = 0;
                self.carry_len = 0;
                self.shared.lock().expect("live state").reopen_attempts = self.reopen_attempts;
                true
            }
            Err(err) => {
                tracing::warn!(%err, "audio stream open failed");
                false
            }
        }
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
        if let Some(engine) = self.engine.as_mut() {
            loop {
                if self.carry_pos < self.carry_len {
                    let pushed = engine.push_playout(&self.carry[self.carry_pos..self.carry_len]);
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
        let inst_rms = if n == 0 {
            0.0
        } else {
            (inst_sq / n as f32).sqrt()
        };
        self.levels.output_peak = inst_peak.max(self.levels.output_peak * LEVEL_DECAY);
        self.levels.output_rms = inst_rms.max(self.levels.output_rms * LEVEL_DECAY);
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
                        })
                        .collect();
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

    fn publish_stats(&mut self) {
        let stats = self.core.stats();
        let mut s = self.shared.lock().expect("live state");
        if matches!(stats.state, ClientState::Joined) {
            self.ever_joined = true;
        }
        // Idle is terminal (set by shutdown); never overwrite it.
        if s.conn != ConnState::Idle {
            s.conn = conn_state(&stats.state);
        }
        s.me = self.core.member_id();
        s.rtt_ms = stats.rtt_ms_last;
        s.jitter_depth = stats.jitter.depth_frames;
        s.jitter_target = stats.jitter.target_frames;
        s.loss_pct = loss_pct(&stats);
        // Mouth to ear, capture to playout:
        //   rtt / 2                      the downlink network leg
        // + jitter depth * 2.5 ms        playout buffering ahead of decode
        // + 2.5 ms                       one media frame of encode latency
        // + buffer_frames / 48 ms        the capture device buffer
        s.mouth_to_ear_ms = stats.rtt_ms_last.map(|rtt| {
            rtt / 2.0
                + stats.jitter.depth_frames as f32 * 2.5
                + 2.5
                + self.settings.buffer_frames as f32 / 48.0
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
        tracing::info!(text, "audio notice");
        let at_ms = self.now_ms();
        self.shared.lock().expect("live state").push_chat(ChatLine {
            from_name: "system".to_owned(),
            from_id: SYSTEM_MEMBER,
            text: text.to_owned(),
            at_ms,
        });
    }
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
