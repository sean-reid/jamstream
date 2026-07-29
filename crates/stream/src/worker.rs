//! The pipeline on its own thread.
//!
//! Feeding ffmpeg means blocking writes: a partial rawvideo frame corrupts
//! the encode, so the writer waits rather than tearing a frame. The session's
//! 2.5 ms mix tick cannot wait for anything, so the two are separated by a
//! bounded channel:
//!
//! - the mix tick does a fixed-size copy and a `try_send`, never blocking;
//! - the worker thread renders, converts, and writes, blocking as needed;
//! - if the worker falls behind and the channel fills, the tick is counted as
//!   a gap and the *next* accepted submission carries silence for it, so the
//!   audio and video clocks stay locked to each other and the loss shows up
//!   as a short gap in the broadcast rather than as drift.
//!
//! Status is published through a mutex the session reads once a second, and
//! an atomic `started` flag lets the mix tick skip the whole path when the
//! session is not streaming.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use jamstream_protocol::control::{DestinationStatus, StreamOp};

use crate::pipeline::{Levels, Pipeline, PipelineEvent, Roster, StreamConfig};
use crate::proc::{ProcessHost, StdProcessHost};

/// Submissions in flight. Half a second of ticks: enough to ride out a
/// scheduler hiccup, short enough that a real stall is noticed rather than
/// hidden behind a growing queue.
const QUEUE_TICKS: usize = 200;

/// One mix tick's worth of broadcast audio plus its meter values.
#[derive(Debug, Clone, Copy)]
pub struct TickPayload {
    pub audio: [f32; crate::TICK_STEREO_SAMPLES],
    pub levels: Levels,
}

impl Default for TickPayload {
    fn default() -> Self {
        TickPayload {
            audio: [0.0; crate::TICK_STEREO_SAMPLES],
            levels: Levels::default(),
        }
    }
}

enum Msg {
    Tick {
        now_ms: u64,
        /// Ticks dropped since the last accepted submission; the worker
        /// replaces them with silence.
        gap: u32,
        payload: Box<TickPayload>,
    },
    Roster(Box<Roster>),
    Op {
        now_ms: u64,
        op: StreamOp,
    },
    /// Clock for supervision while no audio is flowing.
    Beat(u64),
    Shutdown,
}

/// Handle to the pipeline thread. Dropping it stops the thread and every
/// process it owns.
pub struct StreamWorker {
    tx: SyncSender<Msg>,
    status: Arc<Mutex<Vec<DestinationStatus>>>,
    started: Arc<AtomicBool>,
    gap: Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
}

impl StreamWorker {
    /// Starts a worker driving real processes.
    pub fn spawn(cfg: StreamConfig) -> std::io::Result<StreamWorker> {
        Self::spawn_with(cfg, StdProcessHost::new())
    }

    /// Starts a worker over any [`ProcessHost`], for tests that want the fake
    /// with real threading.
    pub fn spawn_with<H>(cfg: StreamConfig, host: H) -> std::io::Result<StreamWorker>
    where
        H: ProcessHost + Send + 'static,
    {
        let (tx, rx) = sync_channel(QUEUE_TICKS);
        let status = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(AtomicBool::new(false));
        let gap = Arc::new(AtomicU64::new(0));
        let thread_status = Arc::clone(&status);
        let thread_started = Arc::clone(&started);
        let join = std::thread::Builder::new()
            .name("jamstream-broadcast".to_owned())
            .spawn(move || run(Pipeline::new(cfg, host), rx, thread_status, thread_started))?;
        Ok(StreamWorker {
            tx,
            status,
            started,
            gap,
            join: Some(join),
        })
    }

    /// True while the host wants a stream; the session only feeds audio then.
    pub fn wants_audio(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// Hands over one mix tick. Never blocks: a full queue counts a gap the
    /// worker will fill with silence.
    pub fn submit_tick(&self, now_ms: u64, payload: TickPayload) {
        let gap = self.gap.swap(0, Ordering::Relaxed) as u32;
        let msg = Msg::Tick {
            now_ms,
            gap,
            payload: Box::new(payload),
        };
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // Put the gap back, plus this tick.
                self.gap.fetch_add(u64::from(gap) + 1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Roster changes are rare, so this one may block briefly on a full
    /// queue; it is called from the once-a-second path, not the mix tick.
    pub fn submit_roster(&self, roster: Roster) {
        let _ = self.tx.send(Msg::Roster(Box::new(roster)));
    }

    pub fn apply(&self, now_ms: u64, op: StreamOp) {
        let _ = self.tx.send(Msg::Op { now_ms, op });
    }

    /// Keeps supervision running while nothing is streaming.
    pub fn beat(&self, now_ms: u64) {
        let _ = self.tx.send(Msg::Beat(now_ms));
    }

    /// Latest per-destination status; empty until the host configures one.
    pub fn status(&self) -> Vec<DestinationStatus> {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Ticks the queue refused, cumulative, for logs.
    pub fn gap_ticks(&self) -> u64 {
        self.gap.load(Ordering::Relaxed)
    }
}

impl Drop for StreamWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run<H: ProcessHost>(
    mut pipeline: Pipeline<H>,
    rx: Receiver<Msg>,
    status: Arc<Mutex<Vec<DestinationStatus>>>,
    started: Arc<AtomicBool>,
) {
    let silence = [0.0f32; crate::TICK_STEREO_SAMPLES];
    let publish = |p: &Pipeline<H>| {
        if let Ok(mut slot) = status.lock() {
            *slot = p.status();
        }
    };
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Tick {
                now_ms,
                gap,
                payload,
            } => {
                for _ in 0..gap {
                    pipeline.push_tick(now_ms, &silence, &payload.levels);
                }
                pipeline.push_tick(now_ms, &payload.audio, &payload.levels);
                pipeline.poll(now_ms);
            }
            Msg::Roster(roster) => pipeline.set_roster(*roster),
            Msg::Op { now_ms, op } => {
                if let Err(err) = pipeline.apply(now_ms, op) {
                    tracing::warn!(error = %err, "stream control rejected");
                }
                started.store(pipeline.started(), Ordering::Relaxed);
            }
            Msg::Beat(now_ms) => pipeline.poll(now_ms),
            Msg::Shutdown => break,
        }
        for event in pipeline.events() {
            log_event(&event);
        }
        publish(&pipeline);
    }
    started.store(false, Ordering::Relaxed);
}

fn log_event(event: &PipelineEvent) {
    match event {
        PipelineEvent::EncoderUp => tracing::info!("broadcast encoder up"),
        PipelineEvent::EncoderDown { reason } => {
            tracing::warn!(reason = %reason, "broadcast encoder down");
        }
        PipelineEvent::DestinationChanged {
            id,
            platform,
            state,
        } => tracing::info!(
            destination = id.0,
            platform = platform.as_str(),
            // The state's Debug is key-free by construction.
            state = ?state,
            "destination state"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::FakeProcessHost;
    use jamstream_protocol::control::{DestinationState, StreamKey, StreamPlatform};
    use jamstream_protocol::ids::DestinationId;
    use std::time::{Duration, Instant};

    fn cfg(name: &str) -> StreamConfig {
        let root =
            std::env::temp_dir().join(format!("jamstream-worker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        StreamConfig {
            width: 160,
            height: 90,
            work_dir: root.clone(),
            key_dir: root.join("keys"),
            ..StreamConfig::default()
        }
    }

    /// Waits for a predicate on the published status, so the test does not
    /// race the worker thread.
    fn wait_for(worker: &StreamWorker, what: &str, f: impl Fn(&[DestinationStatus]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if f(&worker.status()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for {what}: {:?}", worker.status());
    }

    #[test]
    fn the_worker_runs_the_pipeline_off_the_mix_tick() {
        let worker = StreamWorker::spawn_with(cfg("basic"), FakeProcessHost::new()).unwrap();
        assert!(!worker.wants_audio());
        worker.apply(
            0,
            StreamOp::AddDestination {
                id: DestinationId(1),
                platform: StreamPlatform::Twitch,
                key: StreamKey::new("tw"),
            },
        );
        worker.apply(0, StreamOp::Start);
        wait_for(&worker, "connecting", |s| {
            s.len() == 1 && s[0].state == DestinationState::Connecting
        });
        assert!(worker.wants_audio());

        // Feed a second of audio; nothing blocks and the status keeps flowing.
        for tick in 0..400u64 {
            worker.submit_tick(tick * 2, TickPayload::default());
        }
        worker.beat(4_000);
        wait_for(&worker, "live", |s| s[0].state == DestinationState::Live);

        worker.apply(4_000, StreamOp::Stop);
        wait_for(&worker, "idle", |s| s[0].state == DestinationState::Idle);
        assert!(!worker.wants_audio());
    }
}
