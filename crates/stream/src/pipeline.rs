//! The supervisor: one encode, one pusher per destination, restarts with
//! capped backoff, and per-destination status.
//!
//! Time and processes are both inputs. `Pipeline` never reads a clock and
//! never touches `std::process` directly, so the whole state machine is
//! exercised deterministically against `crate::proc::fake::FakeProcessHost`;
//! [`crate::worker::StreamWorker`] is the thing that owns a thread and a
//! clock.
//!
//! A pusher's progress file is the third input. What a destination reports is
//! read out of what its ffmpeg says about itself rather than inferred from the
//! process still being there, so a test writes the block a real one writes.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};

use jamstream_broadcast::{AvatarImage, MemberVisual, Renderer, Role as VisualRole, SceneConfig};
use jamstream_cloud::providers::local::{BROADCAST_DIR_ENV, SESSION_VM_ENV};
use jamstream_protocol::control::{
    DestinationState, DestinationStatus, StreamKey, StreamOp, StreamPlatform, fit_stream_reason,
};
use jamstream_protocol::ids::{DestinationId, MemberId};

use crate::cadence::VideoCadence;
use crate::keys::KeyStore;
use crate::platform::PlatformCatalog;
use crate::proc::{Exit, Feed, ProcId, ProcSpec, ProcessHost, Stdin};
use crate::tools;
use crate::yuv;

/// Cards the renderer draws. The renderer's own cap, not a copy of it: this
/// bounds the roster and the level array handed to it, so a second number
/// here would silently truncate or overrun.
pub use jamstream_broadcast::MAX_CARDS;
/// Destinations a session may point at. The wire type's own cap, not a copy of
/// it: `StreamStatus` refuses a longer list at decode, so a second number here
/// would let the pipeline build a status no peer would accept.
pub use jamstream_protocol::control::MAX_DESTINATIONS;

/// An encode alive this long has proven itself and its backoff resets, so one
/// that fails after two hours restarts promptly instead of inheriting an old
/// penalty. Survival is the whole of what there is to go on here: the encoder
/// is fed by us, and a broken feed is reported by the write that failed.
///
/// Deliberately not what promotes a destination. See [`Pipeline::read_report`].
const HEALTHY_MS: u64 = 3_000;
const BACKOFF_BASE_MS: u64 = 500;
const BACKOFF_MAX_MS: u64 = 16_000;

/// How often a pusher's ffmpeg reports on itself, in the seconds
/// `-stats_period` takes.
///
/// The first report is what makes a destination Live, and ffmpeg waits out one
/// period before writing it, so this is what a working push spends waiting to
/// be called one. Every report after that one is read by nothing and costs the
/// session VM's tmpfs, which is what [`PROGRESS_TRIM_MS`] is for.
const PROGRESS_PERIOD_SECS: &str = "1";

/// The line that closes one of ffmpeg's progress blocks while it is still
/// running. `progress=end` closes the report it writes on the way out, which is
/// no evidence of anything.
const PROGRESS_RUNNING: &str = "progress=continue";

/// How often a connecting pusher's progress file is looked at. Off the mix
/// tick, so it is a throttle rather than a syscall every 2.5 ms.
const PROGRESS_PROBE_MS: u64 = 200;

/// How often a live pusher's progress file is emptied. ffmpeg keeps its own
/// offset and writes on past the hole, which bounds what a twelve-hour session
/// of reports nobody reads costs the VM.
const PROGRESS_TRIM_MS: u64 = 60_000;

/// Most of a progress file that is ever read. A block is a couple of hundred
/// bytes and the first one settles the question.
const PROGRESS_HEAD_BYTES: u64 = 4_096;

/// ffmpeg's floor for `-probesize`, and all either raw input needs: both are
/// fully described in argv, so there is nothing to detect. See
/// [`Pipeline::encoder_spec`] for why the default is a live-stream bug.
const PROBESIZE: &str = "32";

/// The tmpfs directory a session VM's units grant write access to, created by
/// the cloud-init bootstrap before jamstreamd starts. `jamstream_cloud`'s
/// `STREAM_KEY_DIR` is the key directory inside it, spelled from the other
/// side; `crates/server/tests/seams.rs` holds the two together.
const VM_RUN_DIR: &str = "/run/jamstream";

/// Where the VM bootstrap installs the ffmpeg it downloads. Absolute rather
/// than resolved, because on the VM this is a fact and PATH is systemd's.
const VM_FFMPEG: &str = "/usr/local/bin/ffmpeg";

/// What the pipeline's own directory is called inside a session's directory on
/// a machine that is not a VM. Its appearance is how
/// `crates/server/tests/local_provider.rs` sees that the layout a launcher named
/// is the layout the server it spawned resolved.
pub const BROADCAST_SUBDIR: &str = "broadcast";

/// Blake2s-256 of an avatar's bytes, as the roster carries it.
pub type AvatarHash = [u8; 32];

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("platform {0} is not in the bundled catalog")]
    UnknownPlatform(&'static str),
    #[error("a session may not exceed {MAX_DESTINATIONS} destinations")]
    TooManyDestinations,
    #[error("no destination with id {0}")]
    NoSuchDestination(u16),
}

/// Everything the pipeline needs that is not session state. Defaults come
/// from the platform catalog, so the encode ladder has one source of truth.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
    pub audio_kbps: u32,
    pub keyframe_secs: u32,
    pub ffmpeg: PathBuf,
    /// Launcher for pushers: it reads the ingest URL from stdin so the key
    /// never becomes one of *our* arguments.
    pub shell: PathBuf,
    /// Where the encoder publishes: the MediaMTX instance on localhost. A
    /// plain file path works too, which is what the real-ffmpeg test uses.
    pub encoder_output: String,
    /// Where each pusher reads from, normally the same relay path.
    pub pusher_input: String,
    /// Holds the video FIFO.
    pub work_dir: PathBuf,
    /// Directory for one-shot key files, 0700 and readable by nobody but the
    /// account running the server. See [`crate::keys`].
    pub key_dir: PathBuf,
}

impl Default for StreamConfig {
    /// Encode settings from the bundled catalog, paths from whatever this
    /// machine is. See `StreamConfig::resolve`.
    fn default() -> Self {
        StreamConfig::resolve(std::env::var_os(BROADCAST_DIR_ENV).as_deref())
    }
}

impl StreamConfig {
    /// The layout cloud-init creates on a session VM.
    pub fn session_vm() -> StreamConfig {
        StreamConfig::layout(PathBuf::from(VM_RUN_DIR), PathBuf::from(VM_FFMPEG))
    }

    /// The layout for a session server running on someone's own machine: the
    /// video FIFO, every pusher's progress file and the staged keys in a
    /// `broadcast` directory inside `dir`, which is the session's own, and
    /// whichever ffmpeg the host installed.
    ///
    /// A directory of its own rather than the session's, because the session's
    /// also holds the config and the log a host is told to read.
    ///
    /// A bare `ffmpeg` when this machine has none, which the OS resolves again
    /// at each spawn, so a host who installs it mid-session gets a working
    /// broadcast without restarting, and a spawn that still fails names the
    /// program rather than an errno.
    ///
    /// Where the platform has no tmpfs, and macOS and Windows have none, this
    /// puts the instant a stream key spends on disk on a real filesystem: one
    /// 0600 file in a 0700 directory, opened and unlinked before the pusher
    /// runs. On the VM that same file is on tmpfs and root-only. See
    /// [`crate::keys`].
    pub fn in_dir(dir: impl Into<PathBuf>) -> StreamConfig {
        let ffmpeg = tools::on_path(tools::FFMPEG).unwrap_or_else(|| PathBuf::from(tools::FFMPEG));
        StreamConfig::layout(dir.into().join(BROADCAST_SUBDIR), ffmpeg)
    }

    /// Which layout this machine gets: the directory a local session's
    /// launcher named, the session VM's when the VM's tmpfs is there, and
    /// failing both a directory of this process's own under the platform's
    /// temp, so a jamstreamd started by hand has somewhere to work.
    ///
    /// Only a session VM may resolve to `/run/jamstream`, and it says so: the
    /// unit cloud-init writes sets `JAMSTREAM_SESSION_VM`. The layout used to
    /// be inferred from that directory existing, which anything can create,
    /// and a jamstreamd with no `--revoked` creates exactly it to hold the
    /// revocation list.
    fn resolve(broadcast_dir: Option<&OsStr>) -> StreamConfig {
        if let Some(dir) = broadcast_dir.filter(|dir| !dir.is_empty()) {
            return StreamConfig::in_dir(dir);
        }
        if std::env::var_os(SESSION_VM_ENV).is_some() {
            return StreamConfig::session_vm();
        }
        StreamConfig::in_dir(
            std::env::temp_dir().join(format!("jamstream-broadcast-{}", std::process::id())),
        )
    }

    /// Encode settings from the bundled catalog, so the ladder has one source
    /// of truth, plus the paths a layout decides.
    fn layout(dir: PathBuf, ffmpeg: PathBuf) -> StreamConfig {
        let catalog = PlatformCatalog::bundled();
        let v = catalog.video();
        let a = catalog.audio();
        StreamConfig {
            width: v.width,
            height: v.height,
            fps: v.fps,
            video_kbps: v.kbps,
            audio_kbps: a.kbps,
            keyframe_secs: v.keyframe_secs,
            ffmpeg,
            shell: PathBuf::from("/bin/sh"),
            encoder_output: "rtmp://127.0.0.1:1935/jamstream".to_owned(),
            pusher_input: "rtmp://127.0.0.1:1935/jamstream".to_owned(),
            key_dir: dir.join("keys"),
            work_dir: dir,
        }
    }

    /// Video plus audio, the number every destination reports.
    pub fn total_kbps(&self) -> u32 {
        self.video_kbps + self.audio_kbps
    }
}

/// One musician as the stream draws them. Sent on roster changes only; levels
/// travel per tick in [`Levels`].
#[derive(Debug, Clone, PartialEq)]
pub struct StreamMember {
    pub id: MemberId,
    pub name: String,
    pub connected: bool,
    /// Avatar hash and bytes; decoded once per hash and cached, because the
    /// renderer's card cache keys on the decoded buffer's address.
    pub avatar: Option<(AvatarHash, Vec<u8>)>,
}

/// Who is on stage, and how many are listening.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Roster {
    pub members: Vec<StreamMember>,
    pub listeners: usize,
}

/// This tick's meter values, in roster order. Fixed size so the tick path
/// allocates nothing.
#[derive(Debug, Clone, Copy)]
pub struct Levels {
    pub peak: [f32; MAX_CARDS],
    pub rms: [f32; MAX_CARDS],
    pub len: usize,
}

impl Default for Levels {
    fn default() -> Self {
        Levels {
            peak: [0.0; MAX_CARDS],
            rms: [0.0; MAX_CARDS],
            len: 0,
        }
    }
}

impl Levels {
    pub fn push(&mut self, peak: f32, rms: f32) {
        if self.len < MAX_CARDS {
            self.peak[self.len] = peak;
            self.rms[self.len] = rms;
            self.len += 1;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineEvent {
    EncoderUp,
    EncoderDown {
        reason: String,
    },
    DestinationChanged {
        id: DestinationId,
        platform: StreamPlatform,
        state: DestinationState,
    },
}

/// Capped exponential backoff: 500 ms, doubling to 16 s, then flat.
#[derive(Debug, Clone, Copy, Default)]
pub struct Backoff {
    attempts: u32,
}

impl Backoff {
    /// Records a failure and returns how long to wait before the next try.
    pub fn fail(&mut self) -> u64 {
        self.attempts = self.attempts.saturating_add(1);
        self.delay_ms()
    }

    /// Delay implied by the failures so far; zero before the first one.
    pub fn delay_ms(&self) -> u64 {
        if self.attempts == 0 {
            return 0;
        }
        let shift = (self.attempts - 1).min(u64::BITS - 1);
        BACKOFF_BASE_MS
            .checked_shl(shift)
            .unwrap_or(BACKOFF_MAX_MS)
            .min(BACKOFF_MAX_MS)
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

#[derive(Debug)]
struct Destination {
    id: DestinationId,
    platform: StreamPlatform,
    /// Memory only. Never logged, never in a status message, never on disk
    /// outside the one-shot 0600 spawn file.
    key: StreamKey,
    proc: Option<ProcId>,
    state: DestinationState,
    backoff: Backoff,
    /// When a restart may be attempted; None means "as soon as possible".
    retry_at_ms: Option<u64>,
    /// When to read this pusher's progress file next: a throttle while it is
    /// connecting, a trim once it is live.
    progress_at_ms: u64,
}

#[derive(Debug)]
struct Encoder {
    proc: ProcId,
    spawned_ms: u64,
    healthy: bool,
}

pub struct Pipeline<H: ProcessHost> {
    cfg: StreamConfig,
    catalog: PlatformCatalog,
    host: H,
    keys: KeyStore,
    renderer: Renderer,
    cadence: VideoCadence,
    /// Render target and its yuv420p conversion, both preallocated.
    rgba: Vec<u8>,
    yuv: Vec<u8>,
    /// s16le scratch for one audio submission.
    pcm: Vec<u8>,
    roster: Roster,
    visuals: Vec<MemberVisual>,
    avatars: BTreeMap<AvatarHash, AvatarImage>,
    encoder: Option<Encoder>,
    encoder_backoff: Backoff,
    encoder_retry_at_ms: Option<u64>,
    /// Why the encoder is not running, when it should be. Surfaced on every
    /// destination: a host whose ffmpeg is missing must not stare at "idle".
    encoder_reason: Option<String>,
    /// The host asked for Start and has not asked for Stop.
    started: bool,
    dests: Vec<Destination>,
    /// Frames the encoder's queue refused, cumulative for the session and
    /// reported on every destination. Not delivered at all, so the video
    /// timeline comes up one frame short of the audio where it happened. It is
    /// the bounded alternative to a queue that grows until the VM is out of
    /// memory.
    dropped_frames: u64,
    /// Catch-up frames the renderer had no time to draw, cumulative and
    /// reported the same way. Still *delivered*, as a repeat of the last
    /// picture, so the frame count and with it A/V sync stay exact and the
    /// cost is a stutter.
    ///
    /// Counted apart from `dropped_frames` because the two say different
    /// things to a host: a repeat says the machine is struggling, a drop says
    /// it has already failed to deliver.
    repeated_frames: u64,
    events: Vec<PipelineEvent>,
}

impl<H: ProcessHost> Pipeline<H> {
    pub fn new(cfg: StreamConfig, host: H) -> Self {
        let scene = SceneConfig {
            width: cfg.width,
            height: cfg.height,
            // The catalog's rate, not a constant: the renderer's peak-hold is
            // a duration, so a frame rate it does not know about would hold
            // for the wrong length of time.
            fps: cfg.fps,
            wordmark: true,
        };
        let rgba = vec![0u8; (cfg.width * cfg.height * 4) as usize];
        let yuv = vec![0u8; yuv::i420_len(cfg.width, cfg.height)];
        Pipeline {
            keys: KeyStore::new(cfg.key_dir.clone()),
            renderer: Renderer::new(scene),
            cadence: VideoCadence::new(cfg.fps),
            rgba,
            yuv,
            pcm: Vec::new(),
            catalog: PlatformCatalog::bundled(),
            cfg,
            host,
            roster: Roster::default(),
            visuals: Vec::new(),
            avatars: BTreeMap::new(),
            encoder: None,
            encoder_backoff: Backoff::default(),
            encoder_retry_at_ms: None,
            encoder_reason: None,
            started: false,
            dests: Vec::new(),
            dropped_frames: 0,
            repeated_frames: 0,
            events: Vec::new(),
        }
    }

    /// Applies one host request. Only validation failures are errors; a
    /// process that will not start is a destination in `Failed`, not a
    /// rejected command, because the answer to that is a retry, not an error
    /// message to the host.
    pub fn apply(&mut self, now_ms: u64, op: StreamOp) -> Result<(), StreamError> {
        match op {
            StreamOp::AddDestination { id, platform, key } => {
                if self.catalog.get(platform).is_none() {
                    return Err(StreamError::UnknownPlatform(platform.as_str()));
                }
                match self.dests.iter().position(|d| d.id == id) {
                    // Re-adding an id replaces it: a host correcting a typo'd
                    // key should not have to remove first.
                    Some(idx) => {
                        self.stop_destination(idx);
                        let d = &mut self.dests[idx];
                        d.platform = platform;
                        d.key = key;
                        d.backoff.reset();
                        d.retry_at_ms = None;
                        self.set_state(idx, DestinationState::Idle);
                    }
                    None => {
                        if self.dests.len() >= MAX_DESTINATIONS {
                            return Err(StreamError::TooManyDestinations);
                        }
                        self.dests.push(Destination {
                            id,
                            platform,
                            key,
                            proc: None,
                            state: DestinationState::Idle,
                            backoff: Backoff::default(),
                            retry_at_ms: None,
                            progress_at_ms: 0,
                        });
                        self.events.push(PipelineEvent::DestinationChanged {
                            id,
                            platform,
                            state: DestinationState::Idle,
                        });
                    }
                }
                self.poll(now_ms);
                Ok(())
            }
            StreamOp::RemoveDestination { id } => {
                let Some(idx) = self.dests.iter().position(|d| d.id == id) else {
                    return Err(StreamError::NoSuchDestination(id.0));
                };
                // Kill exactly this pusher. Nothing else is touched: not the
                // encoder, not any other destination's process or state.
                self.stop_destination(idx);
                self.keys.discard(id);
                let gone = self.dests.remove(idx);
                self.events.push(PipelineEvent::DestinationChanged {
                    id: gone.id,
                    platform: gone.platform,
                    state: DestinationState::Idle,
                });
                Ok(())
            }
            StreamOp::Start => {
                self.started = true;
                self.encoder_retry_at_ms = None;
                self.poll(now_ms);
                Ok(())
            }
            StreamOp::Stop => {
                self.started = false;
                for idx in 0..self.dests.len() {
                    self.stop_destination(idx);
                    self.dests[idx].backoff.reset();
                    self.dests[idx].retry_at_ms = None;
                    self.set_state(idx, DestinationState::Idle);
                }
                if let Some(enc) = self.encoder.take() {
                    self.host.kill(enc.proc);
                    self.events.push(PipelineEvent::EncoderDown {
                        reason: "stopped by host".to_owned(),
                    });
                }
                self.encoder_backoff.reset();
                Ok(())
            }
        }
    }

    /// Supervision step: reap the dead, promote the healthy, respawn what is
    /// due. Cheap enough to call every tick and correct to call rarely.
    pub fn poll(&mut self, now_ms: u64) {
        if !self.started {
            return;
        }
        self.poll_encoder(now_ms);
        // Pushers are only useful with an encode behind them; without one
        // they would spin their backoff against a relay with no publisher.
        let encoding = self.encoder.is_some();
        for idx in 0..self.dests.len() {
            self.poll_destination(idx, now_ms, encoding);
        }
    }

    fn poll_encoder(&mut self, now_ms: u64) {
        match self.encoder.as_mut() {
            Some(enc) => {
                let id = enc.proc;
                match self.host.poll(id) {
                    Exit::Running => {
                        if !enc.healthy && now_ms.saturating_sub(enc.spawned_ms) >= HEALTHY_MS {
                            enc.healthy = true;
                            self.encoder_backoff.reset();
                        }
                    }
                    Exit::Exited { reason } => {
                        self.host.kill(id);
                        self.encoder = None;
                        let delay = self.encoder_backoff.fail();
                        self.encoder_retry_at_ms = Some(now_ms + delay);
                        tracing::warn!(reason = %reason, delay_ms = delay, "encoder died");
                        self.encoder_reason = Some(reason.clone());
                        self.events.push(PipelineEvent::EncoderDown { reason });
                    }
                }
            }
            None => {
                if self.encoder_retry_at_ms.is_some_and(|at| now_ms < at) {
                    return;
                }
                let spec = self.encoder_spec();
                match self.host.spawn(&spec) {
                    Ok(proc) => {
                        // A fresh ffmpeg starts its timeline at zero, so the
                        // audio-mastered video clock restarts with it.
                        self.cadence = VideoCadence::new(self.cfg.fps);
                        self.encoder = Some(Encoder {
                            proc,
                            spawned_ms: now_ms,
                            healthy: false,
                        });
                        self.encoder_retry_at_ms = None;
                        self.encoder_reason = None;
                        self.events.push(PipelineEvent::EncoderUp);
                    }
                    Err(err) => {
                        let delay = self.encoder_backoff.fail();
                        self.encoder_retry_at_ms = Some(now_ms + delay);
                        tracing::error!(error = %err, delay_ms = delay, "encoder spawn failed");
                        let reason = self.spawn_reason(&err);
                        self.encoder_reason = Some(reason.clone());
                        self.events.push(PipelineEvent::EncoderDown { reason });
                    }
                }
            }
        }
    }

    /// A failed encoder spawn as a host can act on it. An ffmpeg this machine
    /// does not have is named, with how to install it, because the errno for
    /// it is `No such file or directory`, which names neither the program nor
    /// the fix. Checked against the filesystem rather than assumed from the
    /// kind, so a NotFound from anything else the spawn touches keeps its own
    /// message.
    fn spawn_reason(&self, err: &std::io::Error) -> String {
        if err.kind() == std::io::ErrorKind::NotFound && !tools::installed(&self.cfg.ffmpeg) {
            return tools::missing(&self.cfg.ffmpeg);
        }
        format!("spawn failed: {err}")
    }

    fn poll_destination(&mut self, idx: usize, now_ms: u64, encoding: bool) {
        if let Some(proc) = self.dests[idx].proc {
            match self.host.poll(proc) {
                Exit::Running => {
                    self.read_report(idx, now_ms);
                    return;
                }
                Exit::Exited { reason } => {
                    self.host.kill(proc);
                    self.dests[idx].proc = None;
                    let delay = self.dests[idx].backoff.fail();
                    self.dests[idx].retry_at_ms = Some(now_ms + delay);
                    let id = self.dests[idx].id;
                    tracing::warn!(
                        destination = id.0,
                        platform = self.dests[idx].platform.as_str(),
                        reason = %reason,
                        delay_ms = delay,
                        "pusher died"
                    );
                    // Prefixed, because the encoder's failures reach this
                    // same row as "encoder down: ...". They sit on opposite
                    // sides of the local relay and take different fixes, so
                    // a host must not have to guess which one they are
                    // reading.
                    self.set_state(
                        idx,
                        DestinationState::Failed {
                            reason: format!("push failed: {reason}"),
                        },
                    );
                    return;
                }
            }
        }
        if !encoding {
            // No encode, nothing to push. Say why, rather than leaving the
            // destination sitting in Idle with no explanation.
            if let Some(reason) = self.encoder_reason.clone() {
                self.set_state(
                    idx,
                    DestinationState::Failed {
                        reason: format!("encoder down: {reason}"),
                    },
                );
            }
            return;
        }
        if self.dests[idx].retry_at_ms.is_some_and(|at| now_ms < at) {
            return;
        }
        self.spawn_pusher(idx, now_ms);
    }

    /// Reads what a running pusher has said about itself.
    ///
    /// A destination stays Connecting until its ffmpeg has written a whole
    /// progress block, because that block is the first thing ffmpeg writes once
    /// its output is open, and a pusher's output is the platform: the handshake
    /// went through and the broadcast has bytes in it.
    ///
    /// Surviving a timer cannot stand in for that, and the length of the timer
    /// is not the reason. A pusher is two execs and an ffmpeg startup ahead of
    /// its connect, so on a machine busy with an encode it can outlive any
    /// window worth waiting and still be short of the refusal it is heading
    /// for. What that bought was Live with nothing behind it, for as long as it
    /// took to die, and Live is the one word a host reads as "it is working".
    fn read_report(&mut self, idx: usize, now_ms: u64) {
        if now_ms < self.dests[idx].progress_at_ms {
            return;
        }
        let path = progress_path(&self.cfg.work_dir, self.dests[idx].id);
        match self.dests[idx].state {
            DestinationState::Connecting => {
                if pushed(&path) {
                    self.dests[idx].backoff.reset();
                    self.set_state(idx, DestinationState::Live);
                    self.dests[idx].progress_at_ms = now_ms + PROGRESS_TRIM_MS;
                } else {
                    self.dests[idx].progress_at_ms = now_ms + PROGRESS_PROBE_MS;
                }
            }
            DestinationState::Live => {
                trim_progress(&path);
                self.dests[idx].progress_at_ms = now_ms + PROGRESS_TRIM_MS;
            }
            _ => {}
        }
    }

    fn spawn_pusher(&mut self, idx: usize, now_ms: u64) {
        let (id, platform) = (self.dests[idx].id, self.dests[idx].platform);
        let Some(spec) = self.catalog.get(platform) else {
            self.set_state(
                idx,
                DestinationState::Failed {
                    reason: format!("no catalog entry for {}", platform.as_str()),
                },
            );
            return;
        };
        // Before the key is staged, so a failure here cannot leave one behind.
        // ffmpeg will not start at all if it cannot open its progress file, and
        // a block the last attempt left would be read as this one's report.
        if let Err(err) = clear_progress(&self.cfg.work_dir, id) {
            let delay = self.dests[idx].backoff.fail();
            self.dests[idx].retry_at_ms = Some(now_ms + delay);
            self.set_state(
                idx,
                DestinationState::Failed {
                    reason: format!("cannot clear progress file: {err}"),
                },
            );
            return;
        }
        // The URL with the key in it exists as a String here, is written to a
        // 0600 file, and is dropped. The host opens and unlinks that file
        // before the child runs.
        let url = spec.ingest_url(&self.dests[idx].key);
        let key_path = match self.keys.stage(id, &url) {
            Ok(path) => path,
            Err(err) => {
                let delay = self.dests[idx].backoff.fail();
                self.dests[idx].retry_at_ms = Some(now_ms + delay);
                self.set_state(
                    idx,
                    DestinationState::Failed {
                        reason: format!("cannot stage key file: {err}"),
                    },
                );
                return;
            }
        };
        let proc_spec = self.pusher_spec(id, platform, key_path);
        debug_assert!(
            !proc_spec.mentions(self.dests[idx].key.expose()),
            "stream key leaked into a pusher's argv"
        );
        match self.host.spawn(&proc_spec) {
            Ok(proc) => {
                self.dests[idx].proc = Some(proc);
                self.dests[idx].progress_at_ms = now_ms;
                self.dests[idx].retry_at_ms = None;
                self.set_state(idx, DestinationState::Connecting);
            }
            Err(err) => {
                self.keys.discard(id);
                let delay = self.dests[idx].backoff.fail();
                self.dests[idx].retry_at_ms = Some(now_ms + delay);
                self.set_state(
                    idx,
                    DestinationState::Failed {
                        reason: format!("spawn failed: {err}"),
                    },
                );
            }
        }
    }

    /// Kills one destination's pusher, leaving its configuration in place.
    fn stop_destination(&mut self, idx: usize) {
        if let Some(proc) = self.dests[idx].proc.take() {
            self.host.kill(proc);
        }
        let _ = clear_progress(&self.cfg.work_dir, self.dests[idx].id);
    }

    /// Records a destination's state and, on a change, emits it.
    ///
    /// Every reason is cut to fit here rather than at each of the places one
    /// is built, because the cost of missing one is not this destination's
    /// line: the wire refuses the whole `StreamStatus`, so one over-long
    /// explanation would leave every other destination with no status at all.
    fn set_state(&mut self, idx: usize, state: DestinationState) {
        let state = match state {
            DestinationState::Failed { reason } => DestinationState::Failed {
                reason: fit_stream_reason(&reason).to_owned(),
            },
            other => other,
        };
        if self.dests[idx].state == state {
            return;
        }
        self.dests[idx].state = state.clone();
        self.events.push(PipelineEvent::DestinationChanged {
            id: self.dests[idx].id,
            platform: self.dests[idx].platform,
            state,
        });
    }

    /// Replaces the on-stage roster. Avatars are decoded once per hash: the
    /// renderer caches card pixels by the decoded buffer's address, so the
    /// `MemberVisual`s must be stable between roster changes.
    pub fn set_roster(&mut self, roster: Roster) {
        if roster == self.roster {
            return;
        }
        self.avatars.retain(|hash, _| {
            roster
                .members
                .iter()
                .any(|m| m.avatar.as_ref().is_some_and(|(h, _)| h == hash))
        });
        self.visuals.clear();
        for m in roster.members.iter().take(MAX_CARDS) {
            let avatar = m.avatar.as_ref().and_then(|(hash, bytes)| {
                if !self.avatars.contains_key(hash) {
                    match AvatarImage::from_bytes(bytes) {
                        Ok(img) => {
                            self.avatars.insert(*hash, img);
                        }
                        Err(err) => {
                            tracing::debug!(error = %err, "avatar undecodable, drawing initials");
                        }
                    }
                }
                self.avatars.get(hash).cloned()
            });
            self.visuals.push(MemberVisual {
                name: m.name.clone(),
                avatar,
                level_peak: 0.0,
                level_rms: 0.0,
                connected: m.connected,
                role: VisualRole::Musician,
            });
        }
        self.roster = roster;
    }

    /// Feeds one submission of interleaved stereo broadcast audio (any whole
    /// number of frames) plus this tick's meter values.
    ///
    /// Audio goes out first and unconditionally: it is the master clock, and
    /// [`VideoCadence`] turns the sample count into the frames now due. A
    /// caller that missed ticks submits silence for them, which keeps the two
    /// timelines locked rather than letting video slide.
    ///
    /// Neither submission blocks on the encoder. The host queues each pipe
    /// separately ([`crate::proc::StdProcessHost`]); a video queue at its cap
    /// gives the frame back as [`Feed::Dropped`] and an audio queue at its cap
    /// is a broken feed, so a slow encoder costs frames rather than memory and
    /// a stalled one is restarted rather than waited on.
    pub fn push_tick(&mut self, now_ms: u64, audio: &[f32], levels: &Levels) {
        if self.encoder.is_none() {
            return;
        }
        debug_assert!(audio.len() % 2 == 0, "audio must be interleaved stereo");
        self.pcm.clear();
        self.pcm.reserve(audio.len() * 2);
        for &s in audio {
            let v = (s.clamp(-1.0, 1.0) * 32_767.0) as i16;
            self.pcm.extend_from_slice(&v.to_le_bytes());
        }
        let proc = self.encoder.as_ref().expect("checked above").proc;
        let pcm = std::mem::take(&mut self.pcm);
        let write = self.host.write_stdin(proc, &pcm);
        self.pcm = pcm;
        if let Err(err) = write {
            self.encoder_failed(now_ms, format!("audio write failed: {err}"));
            return;
        }

        let run = self.cadence.advance((audio.len() / 2) as u64);
        let mut rendered = false;
        for frame in run {
            if rendered {
                // Catch-up frames repeat the last picture instead of skipping:
                // the frame *count* is what keeps video aligned with audio, so
                // dropping one would shift the video clock permanently. The
                // frame still goes out, which is why this is not a drop.
                self.repeated_frames += 1;
            } else {
                for (v, i) in self.visuals.iter_mut().zip(0..levels.len) {
                    v.level_peak = levels.peak[i];
                    v.level_rms = levels.rms[i];
                }
                self.renderer
                    .render(frame, &self.visuals, self.roster.listeners, &mut self.rgba);
                yuv::rgba_to_i420(&self.rgba, self.cfg.width, self.cfg.height, &mut self.yuv);
                rendered = true;
            }
            let yuv = std::mem::take(&mut self.yuv);
            let write = self.host.write_fifo(proc, 0, &yuv);
            self.yuv = yuv;
            match write {
                Ok(Feed::Queued) => {}
                // The encoder is behind by more than the queue holds. Shed the
                // frame and count it: the broadcast comes up one frame short
                // where it happened, which is a visible, bounded cost the host
                // can see in the status, unlike a queue that grows until the
                // VM is out of memory.
                Ok(Feed::Dropped) => self.dropped_frames += 1,
                Err(err) => {
                    self.encoder_failed(now_ms, format!("video write failed: {err}"));
                    return;
                }
            }
        }
    }

    fn encoder_failed(&mut self, now_ms: u64, reason: String) {
        if let Some(enc) = self.encoder.take() {
            self.host.kill(enc.proc);
        }
        let delay = self.encoder_backoff.fail();
        self.encoder_retry_at_ms = Some(now_ms + delay);
        tracing::warn!(reason = %reason, delay_ms = delay, "encoder feed failed");
        self.encoder_reason = Some(reason.clone());
        self.events.push(PipelineEvent::EncoderDown { reason });
    }

    /// Key-free per-destination status, safe to send to every member.
    pub fn status(&self) -> Vec<DestinationStatus> {
        let bitrate_kbps = self.cfg.total_kbps();
        self.dests
            .iter()
            .map(|d| DestinationStatus {
                id: d.id,
                platform: d.platform,
                state: d.state.clone(),
                bitrate_kbps,
                dropped_frames: self.dropped_frames,
                repeated_frames: self.repeated_frames,
            })
            .collect()
    }

    pub fn events(&mut self) -> Vec<PipelineEvent> {
        std::mem::take(&mut self.events)
    }

    /// The host asked to stream and has not asked to stop. While true the
    /// session must keep feeding audio.
    pub fn started(&self) -> bool {
        self.started
    }

    /// At least one destination is up.
    pub fn on_air(&self) -> bool {
        self.dests.iter().any(|d| d.state == DestinationState::Live)
    }

    /// Frames the encoder's queue refused, so the broadcast is that many
    /// pictures short. Genuine loss.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    /// Frames delivered as a repeat of the last picture because there was no
    /// time to draw them. Nothing missing, sync exact, visibly stuttery.
    pub fn repeated_frames(&self) -> u64 {
        self.repeated_frames
    }

    pub fn cadence(&self) -> &VideoCadence {
        &self.cadence
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// Argv for the single encode. Notes on the choices, since every one of
    /// them is a platform requirement rather than a preference:
    ///
    /// - video input first so the FIFO open ordering is deterministic (see
    ///   [`crate::proc::StdProcessHost`]),
    /// - `-r`/`-framerate` on a rawvideo input makes it constant frame rate,
    ///   which is what lets us treat "how many frames did we push" as the
    ///   whole of A/V sync,
    /// - `nal-hrd=cbr` with min=max=target is what Twitch means by CBR,
    /// - `keyint=min-keyint=fps*2` with `scenecut=0` pins keyframes to
    ///   exactly 2 s, which both platforms require,
    /// - AAC-LC at 48 kHz because no platform accepts Opus,
    /// - `-probesize`/`-analyzeduration` on both inputs, which is not a
    ///   preference either.
    ///
    /// That last one is the difference between a stream and a stall. ffmpeg
    /// analyses each input before it starts, and its default budget for a
    /// stream with no duration is five seconds of *content*. Off a file that is
    /// a few milliseconds of reading; off a live pipe it is five seconds of
    /// waiting, during which ffmpeg reads one video frame and then nothing but
    /// audio. That read pattern deadlocks a writer feeding both pipes, and
    /// short of a deadlock it costs the first five seconds of every broadcast
    /// as dropped frames. Both inputs are fully described right here in argv,
    /// so there is nothing to analyse: the floor of 32 bytes and no duration.
    fn encoder_spec(&self) -> ProcSpec {
        let fifo = self.work_fifo();
        let gop = self.cfg.fps * self.cfg.keyframe_secs;
        let vb = format!("{}k", self.cfg.video_kbps);
        let args = vec![
            "-hide_banner".to_owned(),
            "-loglevel".to_owned(),
            "error".to_owned(),
            "-nostats".to_owned(),
            "-y".to_owned(),
            // Video: raw yuv420p frames over the FIFO, constant rate.
            "-f".to_owned(),
            "rawvideo".to_owned(),
            "-pixel_format".to_owned(),
            "yuv420p".to_owned(),
            "-video_size".to_owned(),
            format!("{}x{}", self.cfg.width, self.cfg.height),
            "-framerate".to_owned(),
            self.cfg.fps.to_string(),
            "-probesize".to_owned(),
            PROBESIZE.to_owned(),
            "-analyzeduration".to_owned(),
            "0".to_owned(),
            "-i".to_owned(),
            fifo.to_string_lossy().into_owned(),
            // Audio: s16le on stdin. Two raw inputs cannot share one stdin,
            // which is why the video side is a FIFO at all.
            "-f".to_owned(),
            "s16le".to_owned(),
            "-ar".to_owned(),
            crate::SAMPLE_RATE.to_string(),
            "-ac".to_owned(),
            "2".to_owned(),
            "-probesize".to_owned(),
            PROBESIZE.to_owned(),
            "-analyzeduration".to_owned(),
            "0".to_owned(),
            "-i".to_owned(),
            "pipe:0".to_owned(),
            "-map".to_owned(),
            "0:v:0".to_owned(),
            "-map".to_owned(),
            "1:a:0".to_owned(),
            "-c:v".to_owned(),
            "libx264".to_owned(),
            "-preset".to_owned(),
            "veryfast".to_owned(),
            "-tune".to_owned(),
            "zerolatency".to_owned(),
            "-profile:v".to_owned(),
            "main".to_owned(),
            "-pix_fmt".to_owned(),
            "yuv420p".to_owned(),
            "-b:v".to_owned(),
            vb.clone(),
            "-minrate".to_owned(),
            vb.clone(),
            "-maxrate".to_owned(),
            vb.clone(),
            "-bufsize".to_owned(),
            vb,
            "-g".to_owned(),
            gop.to_string(),
            "-keyint_min".to_owned(),
            gop.to_string(),
            "-x264-params".to_owned(),
            format!("nal-hrd=cbr:keyint={gop}:min-keyint={gop}:scenecut=0:force-cfr=1"),
            "-color_primaries".to_owned(),
            "bt709".to_owned(),
            "-color_trc".to_owned(),
            "bt709".to_owned(),
            "-colorspace".to_owned(),
            "bt709".to_owned(),
            "-color_range".to_owned(),
            "tv".to_owned(),
            "-c:a".to_owned(),
            "aac".to_owned(),
            "-profile:a".to_owned(),
            "aac_low".to_owned(),
            "-b:a".to_owned(),
            format!("{}k", self.cfg.audio_kbps),
            "-ar".to_owned(),
            crate::SAMPLE_RATE.to_string(),
            "-ac".to_owned(),
            "2".to_owned(),
            "-f".to_owned(),
            output_format(&self.cfg.encoder_output).to_owned(),
            self.cfg.encoder_output.clone(),
        ];
        ProcSpec {
            program: self.cfg.ffmpeg.clone(),
            args,
            stdin: Stdin::Pipe,
            fifos: vec![fifo],
            label: "encoder".to_owned(),
            relay_url: self.relay_url(&self.cfg.encoder_output),
        }
    }

    /// Argv for one pusher. The ingest URL, key included, arrives on stdin
    /// from the staged 0600 file and is never one of our arguments; the
    /// launcher reads one line and execs ffmpeg with it.
    ///
    /// `-progress` is the destination's state. ffmpeg writes counters there and
    /// nothing else, so it is safe to leave in a directory the session can
    /// read, and it says the one thing no other channel does: the output is
    /// open. See [`Pipeline::read_report`].
    fn pusher_spec(
        &self,
        id: DestinationId,
        platform: StreamPlatform,
        key_path: PathBuf,
    ) -> ProcSpec {
        let script = format!(
            "IFS= read -r JS_INGEST || exit 64\n\
             exec {ffmpeg} -nostdin -hide_banner -loglevel error -nostats \
             -stats_period {period} -progress {progress} \
             -i {input} -c copy -f flv \"$JS_INGEST\"\n",
            ffmpeg = shell_quote(&self.cfg.ffmpeg.to_string_lossy()),
            period = PROGRESS_PERIOD_SECS,
            progress = shell_quote(&progress_path(&self.cfg.work_dir, id).to_string_lossy()),
            input = shell_quote(&self.cfg.pusher_input),
        );
        ProcSpec {
            program: self.cfg.shell.clone(),
            args: vec!["-c".to_owned(), script],
            stdin: Stdin::SecretFile(key_path),
            fifos: Vec::new(),
            label: format!("pusher:{}:{}", platform.as_str(), id.0),
            // The pusher's *input*. Its output is the platform, key and all,
            // and that one stays redacted.
            relay_url: self.relay_url(&self.cfg.pusher_input),
        }
    }

    /// The relay as it will appear in a child's stderr, when it is a URL at
    /// all: `encoder_output` is a plain file path in the real-ffmpeg test,
    /// and a path is not something the redactor would ever match anyway.
    fn relay_url(&self, target: &str) -> Option<String> {
        target.contains("://").then(|| target.to_owned())
    }

    fn work_fifo(&self) -> PathBuf {
        self.cfg.work_dir.join("broadcast-video.raw")
    }
}

impl<H: ProcessHost> Drop for Pipeline<H> {
    fn drop(&mut self) {
        for d in &mut self.dests {
            if let Some(proc) = d.proc.take() {
                self.host.kill(proc);
            }
            self.keys.discard(d.id);
            let _ = clear_progress(&self.cfg.work_dir, d.id);
        }
        if let Some(enc) = self.encoder.take() {
            self.host.kill(enc.proc);
        }
    }
}

/// Where one pusher's ffmpeg reports on itself. Nothing secret is in it: a
/// progress block is counters, and the name is a destination id.
fn progress_path(work_dir: &Path, id: DestinationId) -> PathBuf {
    work_dir.join(format!("pusher-{}.progress", id.0))
}

/// Makes the directory a pusher's report goes in and removes what any earlier
/// attempt left there.
///
/// ffmpeg truncates the file when it opens it, but only if it gets that far: a
/// launcher that never execs, or an exec that fails, leaves the last attempt's
/// block behind, and the next attempt would read it as its own.
fn clear_progress(work_dir: &Path, id: DestinationId) -> std::io::Result<()> {
    std::fs::create_dir_all(work_dir)?;
    match std::fs::remove_file(progress_path(work_dir, id)) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// True once a pusher's ffmpeg has written a whole progress block.
///
/// A pusher that never reached its output writes nothing at all: the file is
/// created when ffmpeg parses its arguments and stays empty, whether the
/// refusal came from the relay it reads or the platform it writes to.
fn pushed(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = Vec::new();
    if file
        .take(PROGRESS_HEAD_BYTES)
        .read_to_end(&mut head)
        .is_err()
    {
        return false;
    }
    String::from_utf8_lossy(&head)
        .lines()
        .any(|line| line.trim_end() == PROGRESS_RUNNING)
}

/// Empties a live pusher's progress file.
///
/// The first block was the whole signal and nothing reads the rest, but ffmpeg
/// writes one a second for as long as it runs, which over a long session is
/// megabytes of the VM's tmpfs. It keeps its own offset, so what this leaves
/// behind is a hole rather than an interrupted child.
fn trim_progress(path: &Path) {
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_len(0));
}

/// FLV for RTMP (what MediaMTX and every platform ingest speaks); anything
/// else is a local file and ffmpeg can pick the muxer from the extension.
fn output_format(target: &str) -> &'static str {
    if target.starts_with("rtmp") || target.ends_with(".flv") {
        "flv"
    } else if target.ends_with(".ts") {
        "mpegts"
    } else {
        "mp4"
    }
}

/// Single-quote for a POSIX shell. Only paths from our own config pass
/// through here, but the launcher script is the one place a shell is
/// involved at all, so it quotes anyway.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// One of ffmpeg's progress blocks, byte for byte as a pusher writes it.
///
/// Captured from a real `ffmpeg -progress` run rather than written from the
/// parser, which would only prove the parser agrees with itself.
/// `tests/going_live.rs` hands the same fixture to a real child through the
/// pipeline's own argv, and `tests/relay_chain.rs` promotes a destination on
/// the block a real ffmpeg writes to a real platform stand-in.
#[cfg(test)]
pub(crate) const PROGRESS_BLOCK: &str = include_str!("../testdata/pusher.progress");

/// Writes what a pusher's ffmpeg writes once its output is open.
#[cfg(test)]
pub(crate) fn report_push(work_dir: &Path, id: DestinationId) {
    std::fs::create_dir_all(work_dir).expect("work dir");
    std::fs::write(progress_path(work_dir, id), PROGRESS_BLOCK).expect("a progress block");
}

#[cfg(test)]
mod tests {

    /// `cargo test` shares one process across these tests while nextest does
    /// not, so anything setting a variable another test reads takes this
    /// first.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    use super::*;
    use crate::proc::fake::{Call, FakeProcessHost};

    fn tmp_cfg(name: &str) -> StreamConfig {
        let root =
            std::env::temp_dir().join(format!("jamstream-pipeline-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        StreamConfig {
            // Small frames keep the render cost off the unit tests.
            width: 320,
            height: 180,
            work_dir: root.clone(),
            key_dir: root.join("keys"),
            ..StreamConfig::default()
        }
    }

    fn pipeline(name: &str) -> Pipeline<FakeProcessHost> {
        Pipeline::new(tmp_cfg(name), FakeProcessHost::new())
    }

    fn add(id: u16, platform: StreamPlatform, key: &str) -> StreamOp {
        StreamOp::AddDestination {
            id: DestinationId(id),
            platform,
            key: StreamKey::new(key),
        }
    }

    fn state(p: &Pipeline<FakeProcessHost>, id: u16) -> DestinationState {
        p.status()
            .into_iter()
            .find(|s| s.id == DestinationId(id))
            .expect("destination present")
            .state
    }

    /// The fake host runs no ffmpeg, so a test that wants a destination live
    /// writes the report a real pusher would have written.
    fn reports_push(p: &Pipeline<FakeProcessHost>, id: u16) {
        report_push(&p.cfg.work_dir, DestinationId(id));
    }

    fn progress_of(p: &Pipeline<FakeProcessHost>, id: u16) -> PathBuf {
        progress_path(&p.cfg.work_dir, DestinationId(id))
    }

    #[test]
    fn backoff_schedule_is_capped_exponential() {
        let mut b = Backoff::default();
        assert_eq!(b.delay_ms(), 0);
        let seen: Vec<u64> = (0..9).map(|_| b.fail()).collect();
        assert_eq!(
            seen,
            vec![
                500, 1_000, 2_000, 4_000, 8_000, 16_000, 16_000, 16_000, 16_000
            ]
        );
        b.reset();
        assert_eq!(b.attempts(), 0);
        assert_eq!(b.fail(), 500);
    }

    #[test]
    fn start_brings_up_the_encoder_then_the_pushers() {
        let mut p = pipeline("start");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw-key"))
            .unwrap();
        assert_eq!(state(&p, 1), DestinationState::Idle);
        p.apply(0, StreamOp::Start).unwrap();
        let labels: Vec<&str> = p
            .host()
            .calls()
            .iter()
            .filter_map(|c| match c {
                Call::Spawn { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["encoder", "pusher:twitch:1"]);
        assert_eq!(state(&p, 1), DestinationState::Connecting);
        // Connecting for as long as it takes, however long the process lives:
        // nothing has been pushed yet, so there is nothing to report.
        p.poll(HEALTHY_MS);
        p.poll(HEALTHY_MS * 10);
        assert_eq!(state(&p, 1), DestinationState::Connecting);
        assert!(!p.on_air());
        // Live on the pusher's own report, and on nothing else.
        reports_push(&p, 1);
        p.poll(HEALTHY_MS * 10 + PROGRESS_PROBE_MS);
        assert_eq!(state(&p, 1), DestinationState::Live);
        assert!(p.on_air());
    }

    /// A pusher that outlives every window a supervisor might have waited
    /// and only then reaches its refused connect must never have been
    /// called Live. It is a race on a loaded machine, so the process here
    /// never dies until the test says so, which is the same case held
    /// still.
    #[test]
    fn a_pusher_that_pushed_nothing_is_never_live_however_long_it_survives() {
        let mut p = pipeline("nothingpushed");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        p.apply(0, StreamOp::Start).unwrap();
        let pusher = p.host().find_live("pusher").unwrap();

        let mut seen = Vec::new();
        for now in (0..30_000).step_by(250) {
            p.poll(now);
            seen.extend(p.events());
            assert_eq!(
                state(&p, 1),
                DestinationState::Connecting,
                "at {now} ms, with nothing pushed"
            );
            assert!(!p.on_air(), "on air at {now} ms with nothing pushed");
        }
        // What it was doing all along, arriving late.
        p.host_mut().exit(
            pusher,
            "[flv @ 0x1] Failed to connect to rtmps://<redacted> Connection refused",
        );
        p.poll(30_000);
        seen.extend(p.events());
        match state(&p, 1) {
            DestinationState::Failed { reason } => {
                assert!(reason.contains("Failed to connect"), "{reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(
            !seen.iter().any(|e| matches!(
                e,
                PipelineEvent::DestinationChanged {
                    state: DestinationState::Live,
                    ..
                }
            )),
            "a destination that never pushed anything was reported Live: {seen:?}"
        );
    }

    /// A report belongs to the pusher that wrote it. The one a dead pusher left
    /// behind must not make its replacement live before it has connected.
    #[test]
    fn a_dead_pushers_report_does_not_carry_over_to_its_replacement() {
        let mut p = pipeline("stalereport");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        p.apply(0, StreamOp::Start).unwrap();
        let first = p.host().find_live("pusher").unwrap();
        reports_push(&p, 1);
        p.poll(1_000);
        assert_eq!(state(&p, 1), DestinationState::Live);

        p.host_mut().exit(first, "connection reset");
        p.poll(1_100);
        p.poll(1_600);
        let second = p.host().find_live("pusher").unwrap();
        assert_ne!(second, first);
        assert!(
            !progress_of(&p, 1).exists(),
            "the last attempt's report outlived it"
        );
        for now in (1_600..12_000).step_by(250) {
            p.poll(now);
            assert_eq!(state(&p, 1), DestinationState::Connecting, "at {now} ms");
        }
        reports_push(&p, 1);
        p.poll(12_000 + PROGRESS_PROBE_MS);
        assert_eq!(state(&p, 1), DestinationState::Live);
    }

    /// Reports pile up for as long as a push runs and nothing reads one after
    /// the first, so the file is emptied rather than left to grow through a
    /// session. The destination is unaffected: it is already live, and ffmpeg
    /// writes on from its own offset.
    #[test]
    fn a_live_destination_stops_collecting_reports_nobody_reads() {
        let mut p = pipeline("trimreports");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        p.apply(0, StreamOp::Start).unwrap();
        reports_push(&p, 1);
        p.poll(1_000);
        assert_eq!(state(&p, 1), DestinationState::Live);

        let path = progress_of(&p, 1);
        std::fs::write(&path, PROGRESS_BLOCK.repeat(500)).expect("an hour of reports");
        p.poll(1_000 + PROGRESS_TRIM_MS - 1);
        assert!(std::fs::metadata(&path).expect("still there").len() > 0);
        p.poll(1_000 + PROGRESS_TRIM_MS);
        assert_eq!(std::fs::metadata(&path).expect("still there").len(), 0);
        assert_eq!(state(&p, 1), DestinationState::Live);
    }

    #[test]
    fn a_dead_pusher_restarts_on_its_own_backoff_and_reports_why() {
        let mut p = pipeline("restart");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw-key"))
            .unwrap();
        p.apply(0, StreamOp::Start).unwrap();
        let first = p.host().find_live("pusher").unwrap();

        p.host_mut().exit(first, "exited with status 1");
        p.poll(1_000);
        assert_eq!(
            state(&p, 1),
            DestinationState::Failed {
                reason: "push failed: exited with status 1".to_owned()
            }
        );
        // 500 ms of backoff: nothing respawns before it elapses.
        p.poll(1_400);
        assert!(p.host().find_live("pusher").is_none());
        p.poll(1_500);
        let second = p.host().find_live("pusher").unwrap();
        assert_ne!(second, first);
        assert_eq!(state(&p, 1), DestinationState::Connecting);

        // Second failure doubles the wait.
        p.host_mut().exit(second, "connection refused");
        p.poll(2_000);
        p.poll(2_900);
        assert!(p.host().find_live("pusher").is_none());
        p.poll(3_000);
        assert!(p.host().find_live("pusher").is_some());

        // Once it reports a push the penalty is forgotten. Survival does not
        // reset the backoff either: a pusher that keeps dying before it
        // connects keeps its place in the schedule.
        p.poll(60_000);
        assert_eq!(state(&p, 1), DestinationState::Connecting);
        reports_push(&p, 1);
        let t = 60_000 + PROGRESS_PROBE_MS;
        p.poll(t);
        assert_eq!(state(&p, 1), DestinationState::Live);
        let third = p.host().find_live("pusher").unwrap();
        p.host_mut().exit(third, "gone again");
        p.poll(t);
        p.poll(t + 500);
        assert!(p.host().find_live("pusher").is_some(), "backoff reset");
    }

    #[test]
    fn one_destinations_failure_leaves_the_others_alone() {
        let mut p = pipeline("isolation");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        p.apply(0, add(2, StreamPlatform::YouTube, "yt")).unwrap();
        p.apply(0, StreamOp::Start).unwrap();
        reports_push(&p, 1);
        reports_push(&p, 2);
        p.poll(HEALTHY_MS);
        let twitch = p.host().find_live("twitch").unwrap();
        let youtube = p.host().find_live("youtube").unwrap();
        let encoder = p.host().find_live("encoder").unwrap();
        assert_eq!(state(&p, 2), DestinationState::Live);

        p.host_mut().clear_calls();
        p.host_mut().exit(twitch, "boom");
        p.poll(HEALTHY_MS + 10);
        p.poll(HEALTHY_MS + 600);

        // Everything that happened touched destination 1 only.
        for call in p.host().calls() {
            match call {
                Call::Spawn { label, .. } => assert!(label.contains("twitch"), "{label}"),
                Call::Kill { id, .. } => assert_eq!(*id, twitch),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(p.host().live().contains(&youtube));
        assert!(p.host().live().contains(&encoder));
        assert_eq!(state(&p, 2), DestinationState::Live);
    }

    #[test]
    fn adding_and_removing_mid_stream_touches_nothing_else() {
        let mut p = pipeline("addremove");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        p.apply(0, StreamOp::Start).unwrap();
        reports_push(&p, 1);
        p.poll(HEALTHY_MS);
        let twitch = p.host().find_live("twitch").unwrap();
        let encoder = p.host().find_live("encoder").unwrap();

        // Add: exactly one spawn, no kills.
        p.host_mut().clear_calls();
        p.apply(HEALTHY_MS, add(2, StreamPlatform::YouTube, "yt"))
            .unwrap();
        let youtube = p.host().find_live("youtube").unwrap();
        assert_eq!(
            p.host().calls(),
            &[Call::Spawn {
                id: youtube,
                label: "pusher:youtube:2".to_owned()
            }]
        );

        // Remove: exactly one kill, of that destination's process.
        p.host_mut().clear_calls();
        p.apply(
            HEALTHY_MS,
            StreamOp::RemoveDestination {
                id: DestinationId(2),
            },
        )
        .unwrap();
        assert_eq!(
            p.host().calls(),
            &[Call::Kill {
                id: youtube,
                label: "pusher:youtube:2".to_owned()
            }]
        );
        assert!(p.host().live().contains(&twitch));
        assert!(p.host().live().contains(&encoder));
        assert_eq!(p.status().len(), 1);
        assert_eq!(state(&p, 1), DestinationState::Live);

        // Removing something that is not there is an error, not a surprise.
        assert!(matches!(
            p.apply(
                HEALTHY_MS,
                StreamOp::RemoveDestination {
                    id: DestinationId(9)
                }
            ),
            Err(StreamError::NoSuchDestination(9))
        ));
    }

    #[test]
    fn the_key_reaches_the_pusher_by_file_and_never_by_argv() {
        const KEY: &str = "live_987654_supersecret";
        let mut p = pipeline("keys");
        let key_dir = p.cfg.key_dir.clone();
        p.apply(0, add(7, StreamPlatform::Twitch, KEY)).unwrap();
        p.apply(0, StreamOp::Start).unwrap();
        let pusher = p.host().find_live("pusher").unwrap();

        // Nothing a local process could read mentions the key.
        for spec in p.host().specs() {
            assert!(!spec.mentions(KEY), "{spec:?}");
            assert!(!format!("{spec:?}").contains(KEY));
        }
        // It arrived through the staged stdin file, wrapped in the ingest URL.
        let secret = p.host().secret(pusher).expect("secret delivered");
        assert_eq!(secret, format!("rtmps://live.twitch.tv:443/app/{KEY}"));
        // And the file is gone: the host opens and unlinks it at spawn.
        assert!(
            std::fs::read_dir(&key_dir)
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
            "key file outlived the spawn"
        );
        // Status is safe to broadcast.
        let status = p.status();
        assert!(!format!("{status:?}").contains(KEY));
        let _ = std::fs::remove_dir_all(&key_dir);
    }

    #[test]
    fn a_spawn_failure_is_a_failed_state_with_a_reason_not_an_error() {
        let mut p = pipeline("spawnfail");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        p.host_mut()
            .fail_next_spawn("pusher:twitch:1", "no such file");
        p.apply(0, StreamOp::Start).unwrap();
        match state(&p, 1) {
            DestinationState::Failed { reason } => {
                assert!(reason.contains("no such file"), "{reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // And it recovers on the next attempt.
        p.poll(600);
        assert_eq!(state(&p, 1), DestinationState::Connecting);
    }

    #[test]
    fn stop_tears_everything_down_and_keeps_the_destination_list() {
        let mut p = pipeline("stop");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        p.apply(0, StreamOp::Start).unwrap();
        reports_push(&p, 1);
        p.poll(HEALTHY_MS);
        assert!(p.on_air());
        p.apply(HEALTHY_MS, StreamOp::Stop).unwrap();
        assert!(p.host().live().is_empty());
        assert!(!p.on_air());
        assert!(!p.started());
        assert_eq!(state(&p, 1), DestinationState::Idle);
        // Restart reuses the configuration.
        p.apply(10_000, StreamOp::Start).unwrap();
        assert_eq!(state(&p, 1), DestinationState::Connecting);
    }

    #[test]
    fn destinations_added_before_start_do_not_spawn_until_the_encoder_is_up() {
        let mut p = pipeline("order");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        assert!(p.host().live().is_empty(), "nothing runs before Start");
        p.host_mut().fail_next_spawn("encoder", "ffmpeg missing");
        p.apply(0, StreamOp::Start).unwrap();
        // No encoder, so no pusher either: it would only fail against a relay
        // with nothing to relay. The destination says why.
        assert!(p.host().find_live("pusher").is_none());
        match state(&p, 1) {
            DestinationState::Failed { reason } => {
                assert!(reason.starts_with("encoder down:"), "{reason}");
                assert!(reason.contains("ffmpeg missing"), "{reason}");
            }
            other => panic!("expected the encoder's reason, got {other:?}"),
        }
        p.poll(600);
        assert!(p.host().find_live("encoder").is_some());
        assert_eq!(state(&p, 1), DestinationState::Connecting);
    }

    #[test]
    fn too_many_destinations_is_refused() {
        let mut p = pipeline("cap");
        for i in 0..MAX_DESTINATIONS as u16 {
            p.apply(0, add(i, StreamPlatform::Twitch, "k")).unwrap();
        }
        assert!(matches!(
            p.apply(0, add(99, StreamPlatform::Twitch, "k")),
            Err(StreamError::TooManyDestinations)
        ));
        // Replacing an existing id is not a new destination.
        p.apply(0, add(0, StreamPlatform::YouTube, "k2")).unwrap();
        assert_eq!(p.status().len(), MAX_DESTINATIONS);
        assert_eq!(p.status()[0].platform, StreamPlatform::YouTube);
    }

    #[test]
    fn audio_is_written_every_tick_and_video_on_the_sample_cadence() {
        let mut p = pipeline("cadence");
        p.apply(0, StreamOp::Start).unwrap();
        let encoder = p.host().find_live("encoder").unwrap();
        p.set_roster(Roster {
            members: vec![StreamMember {
                id: MemberId(1),
                name: "Ana".into(),
                connected: true,
                avatar: None,
            }],
            listeners: 3,
        });
        let mut levels = Levels::default();
        levels.push(0.4, 0.2);
        let audio = [0.0f32; 240];
        for tick in 0..40 {
            p.push_tick(tick, &audio, &levels);
        }
        // 40 ticks of audio: 40 * 480 bytes of s16le stereo.
        assert_eq!(p.host().stdin_bytes(encoder), 40 * 480);
        // 40 ticks is 100 ms, which is three 30 fps frames plus the startup
        // frame at PTS 0.
        let frame_bytes = yuv::i420_len(p.cfg.width, p.cfg.height) as u64;
        assert_eq!(p.host().fifo_bytes(encoder), 4 * frame_bytes);
        assert_eq!(p.cadence().frames_emitted(), 4);
        assert_eq!(p.dropped_frames(), 0);
        assert_eq!(p.repeated_frames(), 0);
    }

    #[test]
    fn a_catch_up_run_repeats_a_frame_rather_than_losing_the_clock() {
        let mut p = pipeline("catchup");
        p.apply(0, StreamOp::Start).unwrap();
        let encoder = p.host().find_live("encoder").unwrap();
        // Half a second of audio in one submission: 15 frames come due, one
        // rendered and fourteen repeated.
        let audio = vec![0.0f32; 24_000 * 2];
        p.push_tick(0, &audio, &Levels::default());
        let frame_bytes = yuv::i420_len(p.cfg.width, p.cfg.height) as u64;
        assert_eq!(p.host().fifo_bytes(encoder), 16 * frame_bytes);
        assert_eq!(p.cadence().frames_emitted(), 16);
        // Repeats, every one of them delivered. Nothing was lost, so the drop
        // count stays at zero: that is the whole point of splitting them.
        assert_eq!(p.repeated_frames(), 15);
        assert_eq!(p.dropped_frames(), 0);
    }

    #[test]
    fn a_frame_the_encoder_cannot_take_is_dropped_and_counted_not_buffered() {
        let mut p = pipeline("backpressure");
        p.apply(0, StreamOp::Start).unwrap();
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        let encoder = p.host().find_live("encoder").unwrap();
        let audio = [0.0f32; 240];
        for tick in 0..40 {
            p.push_tick(tick, &audio, &Levels::default());
        }
        assert_eq!(p.dropped_frames(), 0);

        // The encoder's video queue hits its cap. Frames come back refused.
        p.host_mut().fill_fifo(encoder, true);
        let before = p.host().fifo_bytes(encoder);
        for tick in 40..80 {
            p.push_tick(tick, &audio, &Levels::default());
        }
        assert_eq!(p.dropped_frames(), 3, "three frames were due and refused");
        assert_eq!(p.host().fifo_dropped(encoder), 3);
        assert_eq!(
            p.host().fifo_bytes(encoder),
            before,
            "a refused frame must not reach the pipe"
        );
        // A refused frame is not a broken encoder: audio keeps flowing, the
        // process lives, and the host is told through the status.
        assert_eq!(p.host().stdin_bytes(encoder), 80 * 480);
        assert!(p.host().live().contains(&encoder));
        assert_eq!(p.status()[0].dropped_frames, 3);
        // Refused, never repeated: one submission per tick means no frame here
        // was a catch-up, so the two counts do not bleed into each other.
        assert_eq!(p.repeated_frames(), 0);
        assert_eq!(p.status()[0].repeated_frames, 0);

        // And it recovers on its own once the encoder catches up.
        p.host_mut().fill_fifo(encoder, false);
        for tick in 80..120 {
            p.push_tick(tick, &audio, &Levels::default());
        }
        assert_eq!(p.dropped_frames(), 3, "no new drops once it keeps up");
        assert!(p.host().fifo_bytes(encoder) > before);
    }

    #[test]
    fn a_broken_encoder_pipe_restarts_the_encode() {
        let mut p = pipeline("pipefail");
        p.apply(0, StreamOp::Start).unwrap();
        let first = p.host().find_live("encoder").unwrap();
        p.host_mut().fail_writes(first);
        p.push_tick(0, &[0.0; 240], &Levels::default());
        assert!(p.host().find_live("encoder").is_none());
        assert!(
            p.events()
                .iter()
                .any(|e| matches!(e, PipelineEvent::EncoderDown { .. }))
        );
        p.poll(500);
        let second = p.host().find_live("encoder").unwrap();
        assert_ne!(second, first);
        // The new process starts a fresh timeline.
        assert_eq!(p.cadence().frames_emitted(), 0);
    }

    #[test]
    fn encoder_argv_carries_every_platform_requirement() {
        let p = pipeline("argv");
        let spec = p.encoder_spec();
        let joined = spec.args.join(" ");
        assert!(joined.contains("-c:v libx264"));
        assert!(joined.contains("-preset veryfast"));
        assert!(joined.contains("-tune zerolatency"));
        // CBR, as Twitch requires.
        assert!(joined.contains("-b:v 2500k -minrate 2500k -maxrate 2500k -bufsize 2500k"));
        assert!(joined.contains("nal-hrd=cbr"));
        // Keyframes exactly every 2 s at 30 fps.
        assert!(joined.contains("-g 60 -keyint_min 60"));
        assert!(joined.contains("keyint=60:min-keyint=60:scenecut=0"));
        // AAC-LC 128k 48 kHz stereo; no platform takes Opus.
        assert!(joined.contains("-c:a aac -profile:a aac_low -b:a 128k -ar 48000 -ac 2"));
        // Video first (FIFO), audio on stdin.
        let vi = joined.find("rawvideo").unwrap();
        let ai = joined.find("s16le").unwrap();
        assert!(vi < ai, "video input must come first");
        assert!(joined.contains("pipe:0"));
        // Both inputs opt out of stream analysis. Without this ffmpeg spends
        // the first five seconds of every broadcast waiting to analyse a live
        // pipe, reading one video frame and nothing else while it does.
        assert_eq!(
            joined
                .matches("-probesize 32 -analyzeduration 0 -i")
                .count(),
            2,
            "both inputs need the analysis opt-out: {joined}"
        );
        assert_eq!(spec.stdin, Stdin::Pipe);
        assert_eq!(spec.fifos.len(), 1);
        assert!(joined.contains(&spec.fifos[0].to_string_lossy().into_owned()));
        assert!(joined.ends_with("-f flv rtmp://127.0.0.1:1935/jamstream"));
    }

    #[test]
    fn pusher_argv_is_a_copy_to_the_platform_and_reads_the_url_from_stdin() {
        let p = pipeline("pusherargv");
        let spec = p.pusher_spec(
            DestinationId(2),
            StreamPlatform::YouTube,
            PathBuf::from("/run/jamstream/keys/dest-2"),
        );
        assert_eq!(spec.args.len(), 2);
        let script = &spec.args[1];
        assert!(script.contains("read -r JS_INGEST"));
        assert!(script.contains("-c copy"));
        assert!(script.contains("-f flv \"$JS_INGEST\""));
        assert!(script.contains("rtmp://127.0.0.1:1935/jamstream"));
        // It reports on itself, which is the whole of what makes it Live, and
        // it reports to the file the pipeline reads.
        let progress = progress_path(&p.cfg.work_dir, DestinationId(2));
        assert!(script.contains("-stats_period 1"), "{script}");
        assert!(
            script.contains(&format!("-progress '{}'", progress.display())),
            "{script}"
        );
        assert_eq!(
            spec.stdin,
            Stdin::SecretFile(PathBuf::from("/run/jamstream/keys/dest-2"))
        );
        assert_eq!(spec.label, "pusher:youtube:2");
    }

    /// Which side of the local relay failed. The two faults reach the same
    /// row and take different fixes: an encoder that cannot publish is a
    /// relay problem on the session's own machine, a pusher that cannot
    /// connect is usually the platform.
    #[test]
    fn the_encoder_and_the_push_are_told_apart_in_the_reason() {
        let mut p = pipeline("sides");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        p.apply(0, StreamOp::Start).unwrap();

        let pusher = p.host().find_live("pusher").unwrap();
        p.host_mut().exit(
            pusher,
            "[flv @ 0x1] Failed to connect to rtmps://<redacted> refused",
        );
        p.poll(10);
        match state(&p, 1) {
            DestinationState::Failed { reason } => {
                assert!(reason.starts_with("push failed: "), "{reason}");
                assert!(reason.contains("Failed to connect"), "{reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        let encoder = p.host().find_live("encoder").unwrap();
        p.host_mut().exit(
            encoder,
            "[flv @ 0x1] Failed to connect to <local relay> refused",
        );
        p.poll(1_000);
        match state(&p, 1) {
            DestinationState::Failed { reason } => {
                assert!(reason.starts_with("encoder down: "), "{reason}");
                assert!(reason.contains("<local relay>"), "{reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Both children are told which URL is the loopback relay, and only that
    /// one: a pusher's *output* is the platform's, key and all, and naming it
    /// would put the key in a reason that goes to the whole room.
    #[test]
    fn only_the_relay_url_is_handed_to_a_child_as_safe_to_name() {
        let p = pipeline("relayurl");
        let relay = "rtmp://127.0.0.1:1935/jamstream".to_owned();
        assert_eq!(p.encoder_spec().relay_url, Some(relay.clone()));
        let pusher = p.pusher_spec(
            DestinationId(1),
            StreamPlatform::Twitch,
            PathBuf::from("/tmp/key"),
        );
        assert_eq!(pusher.relay_url, Some(relay));
    }

    /// The reason has to survive the trip, and the trip is what nothing used
    /// to check. `ControlLink::send` refuses a status whose reason is over
    /// the wire cap, and it refuses the whole message: one destination's long
    /// explanation would cost every other destination its status line, and a
    /// full status of failures at the wire cap fragments on the way out.
    #[test]
    fn every_reason_the_pipeline_builds_fits_the_wire_it_leaves_on() {
        use jamstream_protocol::control::{ControlLink, ControlMsg, STREAM_REASON_BUDGET};

        let mut p = pipeline("wire");
        for i in 0..MAX_DESTINATIONS as u16 {
            p.apply(0, add(i, StreamPlatform::Twitch, "tw")).unwrap();
        }
        p.apply(0, StreamOp::Start).unwrap();
        // Every pusher dies with far more to say than the wire will carry.
        let babble = format!(
            "[flv @ 0x55d1c0a2f480] Failed to connect to rtmps://<redacted> Connection \
             refused{}",
            ". and then some".repeat(40)
        );
        for id in p.host().live() {
            p.host_mut().exit(id, &babble);
        }
        p.poll(10);

        let status = p.status();
        assert_eq!(status.len(), MAX_DESTINATIONS);
        for d in &status {
            match &d.state {
                DestinationState::Failed { reason } => {
                    assert!(reason.len() <= STREAM_REASON_BUDGET, "{reason}");
                    // Cut at the back, so the diagnosis is what survives.
                    assert!(reason.starts_with("push failed: [flv"), "{reason}");
                    assert!(reason.contains("Connection refused"), "{reason}");
                }
                other => panic!("expected Failed, got {other:?}"),
            }
        }

        // And the message it travels in both sends and arrives, which is the
        // half a status-shaped unit test cannot see.
        let mut sender = ControlLink::new();
        sender
            .send(ControlMsg::StreamStatus {
                destinations: status.clone(),
            })
            .expect("a full status of failures must be sendable");
        let mut receiver = ControlLink::new();
        let mut arrived = Vec::new();
        for dg in sender.poll(0) {
            arrived.extend(receiver.receive(&dg).expect("and it must decode"));
        }
        assert_eq!(
            arrived,
            vec![ControlMsg::StreamStatus {
                destinations: status
            }]
        );
    }

    #[test]
    fn status_reports_the_shared_bitrate_and_both_frame_counts() {
        let mut p = pipeline("status");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        let s = &p.status()[0];
        assert_eq!(s.bitrate_kbps, 2_628);
        assert_eq!(s.dropped_frames, 0);
        assert_eq!(s.repeated_frames, 0);
        assert_eq!(s.platform, StreamPlatform::Twitch);
    }

    /// Both outcomes in one run, which is the case a single number could not
    /// describe: a catch-up submission while the encoder's queue is full
    /// repeats the picture *and* has it refused, so the same frame lands in
    /// both counts, and a host reading either alone is misled.
    #[test]
    fn a_repeat_the_encoder_also_refuses_lands_in_both_counts() {
        let mut p = pipeline("bothcounts");
        p.apply(0, StreamOp::Start).unwrap();
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        let encoder = p.host().find_live("encoder").unwrap();
        p.host_mut().fill_fifo(encoder, true);
        // Half a second in one submission: 16 frames due, 15 of them repeats,
        // and the queue takes none of them.
        let audio = vec![0.0f32; 24_000 * 2];
        p.push_tick(0, &audio, &Levels::default());
        assert_eq!(p.repeated_frames(), 15, "fifteen were repeats of the first");
        assert_eq!(p.dropped_frames(), 16, "and none of the sixteen went out");
        let s = &p.status()[0];
        assert_eq!(s.repeated_frames, 15);
        assert_eq!(s.dropped_frames, 16);
    }

    #[test]
    fn roster_changes_rebuild_visuals_once_and_reuse_decoded_avatars() {
        let mut p = pipeline("roster");
        let png = tiny_png();
        let roster = Roster {
            members: vec![StreamMember {
                id: MemberId(1),
                name: "Ana".into(),
                connected: true,
                avatar: Some(([1u8; 32], png.clone())),
            }],
            listeners: 0,
        };
        p.set_roster(roster.clone());
        let ptr = p.visuals[0]
            .avatar
            .as_ref()
            .map(|a| (a.width(), a.height()));
        assert_eq!(ptr, Some((8, 8)));
        // An identical roster is a no-op, so the renderer's card cache holds.
        p.set_roster(roster);
        assert_eq!(p.visuals.len(), 1);
        assert_eq!(p.avatars.len(), 1);
        // Dropping the member evicts the decoded avatar.
        p.set_roster(Roster::default());
        assert!(p.visuals.is_empty());
        assert!(p.avatars.is_empty());
    }

    fn tiny_png() -> Vec<u8> {
        // 8x8 solid PNG, produced without an image encoder dependency by
        // leaning on the broadcast crate's decoder accepting this fixture.
        const PNG: &[u8] = include_bytes!("../testdata/avatar8.png");
        PNG.to_vec()
    }

    /// The shipped configuration, resolved on the machine this test is running
    /// on and used without substituting anything for it.
    ///
    /// This is the test that was missing. Every other test here hands the
    /// pipeline a temp directory first, so the layout a session actually gets
    /// was the one layout nothing ran: off Linux it named `/run/jamstream`
    /// under a read-only root, and the encoder's FIFO could not be created.
    #[test]
    fn the_default_layout_is_usable_on_the_machine_that_resolves_it() {
        let cfg = StreamConfig::default();

        // The FIFO, the progress files and the staged keys all live here, so a
        // directory that cannot be created is a session that cannot broadcast.
        std::fs::create_dir_all(&cfg.work_dir).unwrap_or_else(|err| {
            panic!(
                "the default work dir {} cannot be created here: {err}",
                cfg.work_dir.display()
            )
        });
        let probe = cfg.work_dir.join("layout-probe");
        std::fs::write(&probe, b"x").unwrap_or_else(|err| {
            panic!(
                "the default work dir {} is not writable: {err}",
                cfg.work_dir.display()
            )
        });
        std::fs::remove_file(&probe).expect("remove the probe");

        // Keys go under it, through the same staging a pusher spawn uses, which
        // is what enforces 0700 on the directory.
        let staged = KeyStore::new(cfg.key_dir.clone())
            .stage(DestinationId(1), "rtmp://example.invalid/x?key=notakey")
            .expect("a stream key must be stageable under the default layout");
        assert!(staged.starts_with(&cfg.key_dir));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cfg.key_dir)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "key dir is {mode:o}");
        }
        let _ = std::fs::remove_file(&staged);

        // And the encoder is something this machine can spawn: a path that is
        // there, or a bare name the OS resolves at spawn time. An absolute path
        // to a program this machine does not have is the shape of the bug.
        assert!(
            tools::installed(&cfg.ffmpeg) || cfg.ffmpeg == Path::new(tools::FFMPEG),
            "the default names {}, which is not here and is not resolvable",
            cfg.ffmpeg.display()
        );

        // Nothing of this process's is left in temp; a VM's own directory is
        // not this test's to remove.
        if let Some(parent) = cfg
            .work_dir
            .parent()
            .filter(|parent| parent.starts_with(std::env::temp_dir()))
        {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    /// The other half of the local session's contract: the launcher names a
    /// directory, and everything the pipeline writes goes inside it.
    #[test]
    fn a_named_broadcast_dir_holds_the_whole_layout() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("jamstream-layout-{}", std::process::id()));
        let cfg = StreamConfig::resolve(Some(dir.as_os_str()));
        assert_eq!(cfg.work_dir, dir.join(BROADCAST_SUBDIR));
        assert!(cfg.key_dir.starts_with(&cfg.work_dir));
        assert!(progress_path(&cfg.work_dir, DestinationId(1)).starts_with(&dir));

        // An unset or empty variable is no answer, not an empty path: a layout
        // rooted at "" would put the FIFO in whatever the working directory is.
        for empty in [None, Some(OsStr::new(""))] {
            let cfg = StreamConfig::resolve(empty);
            assert_ne!(cfg.work_dir, Path::new(""));
            assert!(cfg.work_dir.is_absolute(), "{}", cfg.work_dir.display());
            // Nothing but the VM's own unit may land on the VM's layout. This
            // held by accident of `/run` being root owned, until a windows
            // runner made C:\run\jamstream and a host adopted a layout
            // naming /usr/local/bin/ffmpeg.
            assert_ne!(
                cfg.work_dir,
                Path::new(VM_RUN_DIR),
                "a host with no session-vm flag resolved to the VM's layout"
            );
        }
    }

    /// The other side of the contract cloud-init writes: the unit sets this
    /// variable and it is what selects the VM's layout. Serialised with the
    /// resolve test above, because both read the process environment.
    #[test]
    fn the_session_vm_flag_is_what_picks_the_vm_layout() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Safety: the guard serialises every test here that touches this
        // variable, and it is removed before the guard is dropped.
        unsafe { std::env::set_var(SESSION_VM_ENV, "1") };
        let flagged = StreamConfig::resolve(None);
        unsafe { std::env::remove_var(SESSION_VM_ENV) };
        let plain = StreamConfig::resolve(None);

        assert_eq!(flagged.work_dir, Path::new(VM_RUN_DIR));
        assert_eq!(flagged.ffmpeg, Path::new(VM_FFMPEG));
        assert_ne!(
            plain.work_dir,
            Path::new(VM_RUN_DIR),
            "without the flag nothing may resolve to the VM's layout"
        );
    }

    /// The VM's layout stays spelled out, because cloud-init creates exactly
    /// these paths and grants exactly this directory.
    #[test]
    fn the_session_vm_layout_is_the_one_cloud_init_creates() {
        let cfg = StreamConfig::session_vm();
        assert_eq!(cfg.work_dir, Path::new("/run/jamstream"));
        assert_eq!(cfg.key_dir, Path::new("/run/jamstream/keys"));
        assert_eq!(cfg.ffmpeg, Path::new("/usr/local/bin/ffmpeg"));
    }

    /// A missing encoder has to reach the host as the program's name and how
    /// to get it. `spawn failed: No such file or directory (os error 2)` names
    /// neither.
    #[test]
    fn an_ffmpeg_this_machine_does_not_have_is_named_in_the_reason() {
        let mut p = pipeline("noffmpeg");
        p.cfg.ffmpeg = p.cfg.work_dir.join("ffmpeg");
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        let reason = p.spawn_reason(&err);
        assert_eq!(reason, tools::missing(Path::new("ffmpeg")));
        assert!(!reason.contains("os error"), "{reason}");

        // A NotFound from anything else the spawn touches keeps its own
        // message: only an ffmpeg that is really absent is blamed on ffmpeg.
        p.cfg.ffmpeg = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(p.spawn_reason(&err).starts_with("spawn failed:"));
    }
}
