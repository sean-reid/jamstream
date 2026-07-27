//! The supervisor: one encode, one pusher per destination, restarts with
//! capped backoff, and per-destination status.
//!
//! Time and processes are both inputs. `Pipeline` never reads a clock and
//! never touches `std::process` directly, so the whole state machine is
//! exercised deterministically against [`crate::proc::fake::FakeProcessHost`];
//! [`crate::worker::StreamWorker`] is the thing that owns a thread and a
//! clock.

use std::collections::BTreeMap;
use std::path::PathBuf;

use jamstream_broadcast::{AvatarImage, MemberVisual, Renderer, Role as VisualRole, SceneConfig};
use jamstream_protocol::control::{
    DestinationState, DestinationStatus, StreamKey, StreamOp, StreamPlatform,
};
use jamstream_protocol::ids::{DestinationId, MemberId};

use crate::cadence::VideoCadence;
use crate::keys::KeyStore;
use crate::platform::PlatformCatalog;
use crate::proc::{Exit, ProcId, ProcSpec, ProcessHost, Stdin};
use crate::yuv;

/// Cards the renderer draws, mirroring `jamstream_broadcast`'s own cap.
pub const MAX_CARDS: usize = 10;
/// A session may not point at more destinations than this. Each one is a
/// process and a copy of the egress bill.
pub const MAX_DESTINATIONS: usize = 8;

/// A process alive this long has proven itself: state goes Live and the
/// backoff resets, so a stream that fails after two hours restarts promptly
/// instead of inheriting an old penalty.
const HEALTHY_MS: u64 = 3_000;
const BACKOFF_BASE_MS: u64 = 500;
const BACKOFF_MAX_MS: u64 = 16_000;

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
    pub session_name: String,
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
    /// Root-only tmpfs directory for one-shot key files.
    pub key_dir: PathBuf,
}

impl StreamConfig {
    /// Encode settings from the bundled catalog, paths from the layout
    /// cloud-init creates.
    pub fn new(session_name: impl Into<String>) -> Self {
        let catalog = PlatformCatalog::bundled();
        let v = catalog.video();
        let a = catalog.audio();
        StreamConfig {
            session_name: session_name.into(),
            width: v.width,
            height: v.height,
            fps: v.fps,
            video_kbps: v.kbps,
            audio_kbps: a.kbps,
            keyframe_secs: v.keyframe_secs,
            ffmpeg: PathBuf::from("/usr/local/bin/ffmpeg"),
            shell: PathBuf::from("/bin/sh"),
            encoder_output: "rtmp://127.0.0.1:1935/jamstream".to_owned(),
            pusher_input: "rtmp://127.0.0.1:1935/jamstream".to_owned(),
            work_dir: PathBuf::from("/run/jamstream"),
            key_dir: PathBuf::from("/run/jamstream/keys"),
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
    spawned_ms: u64,
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
    /// Frames the renderer could not produce in time, delivered as repeats so
    /// the video clock stays exact. Cumulative for the session.
    dropped_frames: u64,
    events: Vec<PipelineEvent>,
}

impl<H: ProcessHost> Pipeline<H> {
    pub fn new(cfg: StreamConfig, host: H) -> Self {
        let scene = SceneConfig {
            width: cfg.width,
            height: cfg.height,
            session_name: cfg.session_name.clone(),
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
                            spawned_ms: 0,
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
                        let reason = format!("spawn failed: {err}");
                        self.encoder_reason = Some(reason.clone());
                        self.events.push(PipelineEvent::EncoderDown { reason });
                    }
                }
            }
        }
    }

    fn poll_destination(&mut self, idx: usize, now_ms: u64, encoding: bool) {
        if let Some(proc) = self.dests[idx].proc {
            match self.host.poll(proc) {
                Exit::Running => {
                    let d = &self.dests[idx];
                    if d.state == DestinationState::Connecting
                        && now_ms.saturating_sub(d.spawned_ms) >= HEALTHY_MS
                    {
                        self.dests[idx].backoff.reset();
                        self.set_state(idx, DestinationState::Live);
                    }
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
                    self.set_state(idx, DestinationState::Failed { reason });
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
                self.dests[idx].spawned_ms = now_ms;
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
    }

    fn set_state(&mut self, idx: usize, state: DestinationState) {
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
                // dropping one would shift the video clock permanently.
                self.dropped_frames += 1;
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
            if let Err(err) = write {
                self.encoder_failed(now_ms, format!("video write failed: {err}"));
                return;
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

    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
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
    /// - AAC-LC at 48 kHz because no platform accepts Opus.
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
        }
    }

    /// Argv for one pusher. The ingest URL, key included, arrives on stdin
    /// from the staged 0600 file and is never one of our arguments; the
    /// launcher reads one line and execs ffmpeg with it.
    fn pusher_spec(
        &self,
        id: DestinationId,
        platform: StreamPlatform,
        key_path: PathBuf,
    ) -> ProcSpec {
        let script = format!(
            "IFS= read -r JS_INGEST || exit 64\n\
             exec {ffmpeg} -nostdin -hide_banner -loglevel error -nostats \
             -i {input} -c copy -f flv \"$JS_INGEST\"\n",
            ffmpeg = shell_quote(&self.cfg.ffmpeg.to_string_lossy()),
            input = shell_quote(&self.cfg.pusher_input),
        );
        ProcSpec {
            program: self.cfg.shell.clone(),
            args: vec!["-c".to_owned(), script],
            stdin: Stdin::SecretFile(key_path),
            fifos: Vec::new(),
            label: format!("pusher:{}:{}", platform.as_str(), id.0),
        }
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
        }
        if let Some(enc) = self.encoder.take() {
            self.host.kill(enc.proc);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::{Call, FakeProcessHost};

    fn tmp_cfg(name: &str) -> StreamConfig {
        let root =
            std::env::temp_dir().join(format!("jamstream-pipeline-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut cfg = StreamConfig::new("Test Session");
        // Small frames keep the render cost off the unit tests.
        cfg.width = 320;
        cfg.height = 180;
        cfg.work_dir = root.clone();
        cfg.key_dir = root.join("keys");
        cfg
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
        // Healthy after the settle window, and only then.
        p.poll(HEALTHY_MS - 1);
        assert_eq!(state(&p, 1), DestinationState::Connecting);
        p.poll(HEALTHY_MS);
        assert_eq!(state(&p, 1), DestinationState::Live);
        assert!(p.on_air());
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
                reason: "exited with status 1".to_owned()
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

        // Once it survives the settle window the penalty is forgotten.
        p.poll(3_000 + HEALTHY_MS);
        assert_eq!(state(&p, 1), DestinationState::Live);
        let third = p.host().find_live("pusher").unwrap();
        p.host_mut().exit(third, "gone again");
        let t = 3_000 + HEALTHY_MS;
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
        assert_eq!(p.dropped_frames(), 15);
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
        assert_eq!(
            spec.stdin,
            Stdin::SecretFile(PathBuf::from("/run/jamstream/keys/dest-2"))
        );
        assert_eq!(spec.label, "pusher:youtube:2");
    }

    #[test]
    fn status_reports_the_shared_bitrate_and_the_drop_count() {
        let mut p = pipeline("status");
        p.apply(0, add(1, StreamPlatform::Twitch, "tw")).unwrap();
        let s = &p.status()[0];
        assert_eq!(s.bitrate_kbps, 2_628);
        assert_eq!(s.dropped_frames, 0);
        assert_eq!(s.platform, StreamPlatform::Twitch);
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
}
