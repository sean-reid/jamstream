//! Session recording: the broadcast mix and, when asked, a stereo stem per
//! musician, encoded to 16-bit FLAC and handed to a [`RecordingSink`].
//!
//! The mix tick never waits for any of it. Like the stream pipeline, the
//! recorder runs on its own thread behind a bounded channel: the tick does a
//! fixed-size copy and a `try_send`, a full queue counts a gap, and the next
//! accepted tick carries that many ticks of silence so the recording keeps
//! true time and the loss is audible rather than silent drift.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use jamstream_engine::{Fader, mix_into};
use jamstream_protocol::ids::MemberId;
use jamstream_session::{MAX_MUSICIANS, TICK_SAMPLES};

use crate::flac::{BLOCK_INTERLEAVED, FlacEncoder};

/// Interleaved stereo samples per 2.5 ms tick.
pub const TICK_STEREO_SAMPLES: usize = TICK_SAMPLES * 2;

/// Ticks in flight to the recorder thread. One second: enough to ride out a
/// slow sink flush, short enough that a stalled sink surfaces as gaps.
const QUEUE_TICKS: usize = 400;

/// Where encoded recordings go, one object per file. Bytes arrive in order
/// while the take runs and exactly one of finish or abort ends every object,
/// so a streaming multipart uploader can map open, write, finish, and abort
/// onto create, upload part, complete, and abort.
pub trait RecordingSink: Send {
    fn open(&mut self, name: &str) -> io::Result<Box<dyn RecordingObject>>;

    /// True when finishing a take means waiting on a network rather than a
    /// local write. It decides whether the room is told the take is
    /// uploading: saying so about a file on the host's own disk, which
    /// finishes instantly, would be a lie.
    fn uploads(&self) -> bool {
        false
    }
}

/// One object being written. Earlier bytes are never rewritten.
pub trait RecordingObject: Send {
    fn write(&mut self, chunk: &[u8]) -> io::Result<()>;
    /// Completes the object; only a finished object may be treated as whole.
    fn finish(self: Box<Self>) -> io::Result<()>;
    /// Discards everything written, best effort on the way out of a failure.
    fn abort(self: Box<Self>) -> io::Result<()>;
}

/// Local-disk sink for self-hosted sessions: objects are `.part` files until
/// finished, so a crash never leaves something that looks like a recording.
pub struct DiskSink {
    dir: PathBuf,
}

impl DiskSink {
    pub fn new(dir: impl Into<PathBuf>) -> DiskSink {
        DiskSink { dir: dir.into() }
    }
}

impl RecordingSink for DiskSink {
    fn open(&mut self, name: &str) -> io::Result<Box<dyn RecordingObject>> {
        std::fs::create_dir_all(&self.dir)?;
        let done = self.dir.join(name);
        let part = self.dir.join(format!("{name}.part"));
        let file = std::fs::File::create(&part)?;
        Ok(Box::new(DiskObject { file, part, done }))
    }
}

struct DiskObject {
    file: std::fs::File,
    part: PathBuf,
    done: PathBuf,
}

impl RecordingObject for DiskObject {
    fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.file.write_all(chunk)
    }

    fn finish(self: Box<Self>) -> io::Result<()> {
        let DiskObject { file, part, done } = *self;
        file.sync_all()?;
        // Closed before the rename, and the rename retried: on Windows our
        // own open handle and an antivirus scan of the fresh .part file each
        // fail the rename with a sharing violation, and either one reported
        // a finished take as failed.
        drop(file);
        jamstream_cloud::private::rename_with_retry(&part, &done)
    }

    fn abort(self: Box<Self>) -> io::Result<()> {
        drop(self.file);
        std::fs::remove_file(&self.part)
    }
}

/// One mix tick as the recorder consumes it: the post-limiter broadcast
/// slice, plus each musician's decoded pre-mix audio when stems are wanted.
#[derive(Debug, Clone, Copy)]
pub struct RecordPayload {
    pub mix: [f32; TICK_STEREO_SAMPLES],
    pub stem_len: usize,
    pub stem_ids: [MemberId; MAX_MUSICIANS],
    pub stem_faders: [Fader; MAX_MUSICIANS],
    pub stem_pcm: [[f32; TICK_SAMPLES]; MAX_MUSICIANS],
}

impl Default for RecordPayload {
    fn default() -> Self {
        RecordPayload {
            mix: [0.0; TICK_STEREO_SAMPLES],
            stem_len: 0,
            stem_ids: [MemberId(0); MAX_MUSICIANS],
            stem_faders: [Fader::default(); MAX_MUSICIANS],
            stem_pcm: [[0.0; TICK_SAMPLES]; MAX_MUSICIANS],
        }
    }
}

impl RecordPayload {
    pub fn push_stem(&mut self, id: MemberId, fader: Fader, pcm: &[f32; TICK_SAMPLES]) {
        if self.stem_len < MAX_MUSICIANS {
            self.stem_ids[self.stem_len] = id;
            self.stem_faders[self.stem_len] = fader;
            self.stem_pcm[self.stem_len] = *pcm;
            self.stem_len += 1;
        }
    }
}

/// Recorder status, kept beside the on-air lamp's discipline: a recording
/// that fails says so and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingState {
    Idle,
    Recording {
        stems: bool,
    },
    /// The take has ended and its bytes are still going to storage. Only a
    /// sink that uploads reports this; a disk take goes straight to Idle.
    Uploading,
    Failed {
        reason: String,
    },
}

/// One open file: its encoder and the sink object its bytes go to.
struct Track {
    enc: FlacEncoder,
    object: Box<dyn RecordingObject>,
}

impl Track {
    fn open(sink: &mut dyn RecordingSink, file: &str) -> io::Result<Track> {
        let mut object = sink.open(file)?;
        let enc = FlacEncoder::new();
        object.write(&enc.header()?)?;
        Ok(Track { enc, object })
    }

    /// Opens a track whose stream starts with `head`, already-encoded frames
    /// that the caller counts in `frames`, plus `pending` interleaved samples
    /// of silence the encoder carries into its next block.
    fn open_with_head(
        sink: &mut dyn RecordingSink,
        file: &str,
        head: &[u8],
        frames: usize,
        pending: usize,
    ) -> io::Result<Track> {
        let mut object = sink.open(file)?;
        let enc = FlacEncoder::resume_silent(frames, pending);
        object.write(&enc.header()?)?;
        object.write(head)?;
        Ok(Track { enc, object })
    }

    fn push(&mut self, samples: &[f32], buf: &mut Vec<u8>) -> io::Result<()> {
        buf.clear();
        self.enc.push(samples, buf)?;
        if !buf.is_empty() {
            self.object.write(buf)?;
        }
        Ok(())
    }

    fn finish(mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.clear();
        self.enc.finish(buf)?;
        if !buf.is_empty() {
            self.object.write(buf)?;
        }
        self.object.finish()
    }
}

/// The take's silent head, encoded once as the take runs so a stem that opens
/// late is backfilled by copying bytes instead of encoding minutes of silence
/// on the recorder thread. FLAC frames here carry no state but their number,
/// so frames 0..n of silence are the same bytes in every stem's stream.
/// One tick of silence per tick, about 300 bytes of memory per second of take.
struct SilentHead {
    enc: FlacEncoder,
    /// Every complete frame emitted so far, back to back.
    bytes: Vec<u8>,
    /// Byte offset in `bytes` just past each frame.
    frame_ends: Vec<usize>,
}

impl SilentHead {
    fn new() -> SilentHead {
        SilentHead {
            enc: FlacEncoder::new(),
            bytes: Vec::new(),
            frame_ends: Vec::new(),
        }
    }

    /// Encodes one more tick of silence. A tick is shorter than a block, so
    /// this completes at most one frame.
    fn advance(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.clear();
        self.enc.push(&[0.0; TICK_STEREO_SAMPLES], buf)?;
        if !buf.is_empty() {
            self.bytes.extend_from_slice(buf);
            self.frame_ends.push(self.bytes.len());
        }
        Ok(())
    }

    /// Where a stem opening `ticks` into the take starts: the frames to write
    /// as they are, how many they are, and the leftover silent samples its
    /// encoder carries. None when the head has not been grown that far.
    fn split(&self, ticks: u64) -> Option<(&[u8], usize, usize)> {
        let needed = usize::try_from(ticks).ok()? * TICK_STEREO_SAMPLES;
        let frames = needed / BLOCK_INTERLEAVED;
        let pending = needed % BLOCK_INTERLEAVED;
        let end = match frames.checked_sub(1) {
            None => 0,
            Some(last) => *self.frame_ends.get(last)?,
        };
        Some((&self.bytes[..end], frames, pending))
    }
}

struct Take {
    base: String,
    mix: Track,
    /// Stem tracks by member, opened on first audio. None: stems are off.
    stems: Option<BTreeMap<MemberId, Track>>,
    /// Silence to backfill a late stem with; only kept while stems are on.
    silence: Option<SilentHead>,
    /// Names for stem files, as given at start; unknown members fall back to
    /// their id.
    roster: Vec<(MemberId, String)>,
    /// Stem file names already claimed, so two Alexes get distinct files.
    names_used: Vec<String>,
    ticks: u64,
}

/// The synchronous recorder: feed it ticks, it feeds the sink. [`RecordWorker`]
/// runs one of these on its own thread; tests drive it directly.
pub struct Recorder {
    sink: Box<dyn RecordingSink>,
    take: Option<Take>,
    state: RecordingState,
    buf: Vec<u8>,
    stereo: [f32; TICK_STEREO_SAMPLES],
}

impl Recorder {
    pub fn new(sink: impl RecordingSink + 'static) -> Recorder {
        Recorder {
            sink: Box::new(sink),
            take: None,
            state: RecordingState::Idle,
            buf: Vec::new(),
            stereo: [0.0; TICK_STEREO_SAMPLES],
        }
    }

    /// Starts a take, named from the caller's wall clock. `stems` carries the
    /// roster to name stem files by; None records the mix alone. Ignored
    /// while a take is already running.
    pub fn start(&mut self, unix_secs: u64, stems: Option<Vec<(MemberId, String)>>) {
        if self.take.is_some() {
            return;
        }
        let base = take_base(unix_secs);
        let mix = match Track::open(self.sink.as_mut(), &format!("{base}-mix.flac")) {
            Ok(t) => t,
            Err(err) => {
                self.state = RecordingState::Failed {
                    reason: format!("cannot open the mix file: {err}"),
                };
                return;
            }
        };
        self.state = RecordingState::Recording {
            stems: stems.is_some(),
        };
        self.take = Some(Take {
            base,
            mix,
            stems: stems.as_ref().map(|_| BTreeMap::new()),
            silence: stems.as_ref().map(|_| SilentHead::new()),
            roster: stems.unwrap_or_default(),
            names_used: vec!["mix".to_owned()],
            ticks: 0,
        });
    }

    /// Consumes one tick. A tick that fails the sink aborts the take and
    /// parks the reason in [`Recorder::state`].
    pub fn tick(&mut self, payload: &RecordPayload) {
        let Some(take) = self.take.as_mut() else {
            return;
        };
        if let Err(err) = feed_take(
            take,
            payload,
            self.sink.as_mut(),
            &mut self.buf,
            &mut self.stereo,
        ) {
            self.fail(format!("recording failed: {err}"));
        }
    }

    /// Ends the take, flushing every encoder tail and finishing every object.
    pub fn stop(&mut self) {
        let Some(take) = self.take.take() else {
            return;
        };
        let mut result = take.mix.finish(&mut self.buf);
        for (_, track) in take.stems.into_iter().flatten() {
            let finished = track.finish(&mut self.buf);
            result = result.and(finished);
        }
        self.state = match result {
            Ok(()) => RecordingState::Idle,
            Err(err) => RecordingState::Failed {
                reason: format!("recording could not be finished: {err}"),
            },
        };
    }

    pub fn state(&self) -> &RecordingState {
        &self.state
    }

    /// Whether a take is open and its sink uploads, so a caller knows that
    /// the next `stop` will wait on the network and is worth announcing.
    pub fn stop_will_upload(&self) -> bool {
        self.take.is_some() && self.sink.uploads()
    }

    fn fail(&mut self, reason: String) {
        if let Some(take) = self.take.take() {
            let _ = take.mix.object.abort();
            for (_, track) in take.stems.into_iter().flatten() {
                let _ = track.object.abort();
            }
        }
        self.state = RecordingState::Failed { reason };
    }
}

/// One tick into one take: mix always, stems when on. Members absent from the
/// payload get silence so every stem stays aligned with the mix.
fn feed_take(
    take: &mut Take,
    payload: &RecordPayload,
    sink: &mut dyn RecordingSink,
    buf: &mut Vec<u8>,
    stereo: &mut [f32; TICK_STEREO_SAMPLES],
) -> io::Result<()> {
    take.mix.push(&payload.mix, buf)?;
    if take.stems.is_some() {
        for i in 0..payload.stem_len {
            let id = payload.stem_ids[i];
            if !take.stems.as_ref().is_some_and(|t| t.contains_key(&id)) {
                open_stem(take, id, sink, buf)?;
            }
            // The member's contribution to the broadcast mix, pre-limiter:
            // the same fader and pan law the mix ran their audio through.
            mix_into(
                &[(id, &payload.stem_pcm[i][..])],
                |_| payload.stem_faders[i],
                None,
                stereo,
            );
            let track = take.stems.as_mut().and_then(|t| t.get_mut(&id));
            track.expect("stem opened above").push(stereo, buf)?;
        }
        let silent = [0.0f32; TICK_STEREO_SAMPLES];
        for (id, track) in take.stems.as_mut().into_iter().flatten() {
            if !payload.stem_ids[..payload.stem_len].contains(id) {
                track.push(&silent, buf)?;
            }
        }
    }
    take.ticks += 1;
    if let Some(silence) = take.silence.as_mut() {
        silence.advance(buf)?;
    }
    Ok(())
}

/// Opens a stem for a member first heard now, backfilled with silence from the
/// start of the take so it lines up with the mix. The backfill is a copy of the
/// take's silent head: encoding it here would stall the recorder for tens of
/// milliseconds per minute of take, and the mix tick is queued behind it.
fn open_stem(
    take: &mut Take,
    id: MemberId,
    sink: &mut dyn RecordingSink,
    buf: &mut Vec<u8>,
) -> io::Result<()> {
    let name = stem_name(&take.roster, &take.names_used, id);
    let file = format!("{}-{name}.flac", take.base);
    let head = take.silence.as_ref().and_then(|s| s.split(take.ticks));
    let track = match head {
        Some((bytes, frames, pending)) => {
            Track::open_with_head(sink, &file, bytes, frames, pending)?
        }
        // The head does not reach this tick, which the loop above keeps it
        // from happening. Correctness first: encode the silence.
        None => {
            let mut track = Track::open(sink, &file)?;
            let silent = [0.0f32; TICK_STEREO_SAMPLES];
            for _ in 0..take.ticks {
                track.push(&silent, buf)?;
            }
            track
        }
    };
    take.names_used.push(name);
    take.stems
        .as_mut()
        .expect("only called with stems on")
        .insert(id, track);
    Ok(())
}

/// A stem file's name part: the member's name made filesystem-safe, their id
/// appended only when two members would otherwise share a file.
fn stem_name(roster: &[(MemberId, String)], used: &[String], id: MemberId) -> String {
    let given = roster.iter().find(|(rid, _)| *rid == id).map(|(_, n)| n);
    let mut name: String = given
        .map(|n| sanitize(n))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("member-{}", id.0));
    if used.contains(&name) {
        name = format!("{name}-{}", id.0);
    }
    name
}

/// Keeps letters and digits of any script plus `-` and `_`; whitespace
/// becomes `-`; anything else is dropped. Member names are attacker-supplied
/// and this becomes a file name, so nothing `jamstream_cloud::windows_hazard`
/// refuses can survive: the allowlist admits no reserved character or dot,
/// and whitespace never trails. But a musician named in another script keeps
/// their name on the take.
fn sanitize(name: &str) -> String {
    name.chars()
        .filter_map(|c| match c {
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            c if c.is_whitespace() => Some('-'),
            _ => None,
        })
        .collect()
}

/// `jamstream-YYYY-MM-DD-HHMM` in UTC, files named for people.
fn take_base(unix_secs: u64) -> String {
    let (y, m, d) = civil_from_days((unix_secs / 86_400) as i64);
    let secs = unix_secs % 86_400;
    format!(
        "jamstream-{y:04}-{m:02}-{d:02}-{:02}{:02}",
        secs / 3_600,
        (secs % 3_600) / 60
    )
}

/// Days since 1970-01-01 to (year, month, day), Howard Hinnant's civil
/// calendar algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

enum Msg {
    Start {
        unix_secs: u64,
        stems: Option<Vec<(MemberId, String)>>,
    },
    Tick {
        /// Ticks dropped since the last accepted submission; the recorder
        /// replaces them with silence.
        gap: u32,
        payload: Box<RecordPayload>,
    },
    Stop,
    Shutdown,
}

/// Handle to the recorder thread. Dropping it finishes any open take and
/// stops the thread.
pub struct RecordWorker {
    tx: SyncSender<Msg>,
    state: Arc<Mutex<RecordingState>>,
    recording: Arc<AtomicBool>,
    gap: Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
}

impl RecordWorker {
    pub fn spawn(sink: impl RecordingSink + 'static) -> io::Result<RecordWorker> {
        let (tx, rx) = sync_channel(QUEUE_TICKS);
        let state = Arc::new(Mutex::new(RecordingState::Idle));
        let recording = Arc::new(AtomicBool::new(false));
        let gap = Arc::new(AtomicU64::new(0));
        let thread_state = Arc::clone(&state);
        let thread_recording = Arc::clone(&recording);
        let join = std::thread::Builder::new()
            .name("jamstream-record".to_owned())
            .spawn(move || run(Recorder::new(sink), rx, thread_state, thread_recording))?;
        Ok(RecordWorker {
            tx,
            state,
            recording,
            gap,
            join: Some(join),
        })
    }

    /// Starts a take. `unix_secs` names the files; `stems` carries the roster
    /// to name stem files by, None for the mix alone.
    pub fn start(&self, unix_secs: u64, stems: Option<Vec<(MemberId, String)>>) {
        self.gap.store(0, Ordering::Relaxed);
        let _ = self.tx.send(Msg::Start { unix_secs, stems });
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Msg::Stop);
    }

    /// True while a take is running; the mix tick only copies audio then.
    pub fn recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }

    /// Hands over one mix tick. Never blocks: a full queue counts a gap the
    /// recorder will fill with silence.
    pub fn submit_tick(&self, payload: Box<RecordPayload>) {
        let gap = self.gap.swap(0, Ordering::Relaxed) as u32;
        match self.tx.try_send(Msg::Tick { gap, payload }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // Put the gap back, plus this tick.
                self.gap.fetch_add(u64::from(gap) + 1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Ticks the queue refused, cumulative, for status and logs.
    pub fn gap_ticks(&self) -> u64 {
        self.gap.load(Ordering::Relaxed)
    }

    pub fn state(&self) -> RecordingState {
        self.state
            .lock()
            .map(|s| s.clone())
            .unwrap_or(RecordingState::Idle)
    }
}

impl Drop for RecordWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run(
    mut rec: Recorder,
    rx: Receiver<Msg>,
    state: Arc<Mutex<RecordingState>>,
    recording: Arc<AtomicBool>,
) {
    let silence = RecordPayload::default();
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Start { unix_secs, stems } => rec.start(unix_secs, stems),
            Msg::Tick { gap, payload } => {
                for _ in 0..gap {
                    rec.tick(&silence);
                }
                rec.tick(&payload);
            }
            Msg::Stop => {
                // stop() blocks until every object is finished, which for a
                // bucket means the tail is still going over the network.
                // Publish that first: the state is only true while the call
                // nobody can observe is running.
                if rec.stop_will_upload()
                    && let Ok(mut slot) = state.lock()
                {
                    *slot = RecordingState::Uploading;
                }
                rec.stop();
            }
            // The process is going away; the tail written so far is worth
            // more finished than aborted.
            Msg::Shutdown => break,
        }
        recording.store(
            matches!(rec.state(), RecordingState::Recording { .. }),
            Ordering::Relaxed,
        );
        if let Ok(mut slot) = state.lock() {
            *slot = rec.state().clone();
        }
    }
    rec.stop();
    recording.store(false, Ordering::Relaxed);
    if let Ok(mut slot) = state.lock() {
        *slot = rec.state().clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// What one object has seen, for asserting the sink contract.
    #[derive(Default)]
    struct ObjectLog {
        bytes: Vec<u8>,
        finished: bool,
        aborted: bool,
    }

    /// In-memory sink shared with the test, so the recorder's writes are
    /// inspected rather than trusted.
    #[derive(Clone, Default)]
    struct MemSink {
        objects: Arc<Mutex<BTreeMap<String, ObjectLog>>>,
        /// Writes fail once this many bytes have been accepted in total.
        fail_after: Option<usize>,
        written: Arc<AtomicU64>,
    }

    struct MemObject {
        name: String,
        sink: MemSink,
    }

    impl RecordingSink for MemSink {
        fn open(&mut self, name: &str) -> io::Result<Box<dyn RecordingObject>> {
            self.objects
                .lock()
                .unwrap()
                .insert(name.to_owned(), ObjectLog::default());
            Ok(Box::new(MemObject {
                name: name.to_owned(),
                sink: self.clone(),
            }))
        }
    }

    impl RecordingObject for MemObject {
        fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
            let total = self
                .sink
                .written
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            if self
                .sink
                .fail_after
                .is_some_and(|cap| total as usize >= cap)
            {
                return Err(io::Error::other("bucket went away"));
            }
            let mut objects = self.sink.objects.lock().unwrap();
            objects.get_mut(&self.name).unwrap().bytes.extend(chunk);
            Ok(())
        }

        fn finish(self: Box<Self>) -> io::Result<()> {
            self.sink
                .objects
                .lock()
                .unwrap()
                .get_mut(&self.name)
                .unwrap()
                .finished = true;
            Ok(())
        }

        fn abort(self: Box<Self>) -> io::Result<()> {
            self.sink
                .objects
                .lock()
                .unwrap()
                .get_mut(&self.name)
                .unwrap()
                .aborted = true;
            Ok(())
        }
    }

    fn decode(bytes: &[u8]) -> Vec<i32> {
        let mut reader = claxon::FlacReader::new(io::Cursor::new(bytes)).unwrap();
        reader.samples().map(|s| s.unwrap()).collect()
    }

    /// A payload with a distinct, non-constant mix and one stem.
    fn payload(tick: u64, stem: Option<(MemberId, Fader)>) -> RecordPayload {
        let mut p = RecordPayload::default();
        for (i, s) in p.mix.iter_mut().enumerate() {
            let n = (tick as usize * TICK_STEREO_SAMPLES + i) as f32;
            *s = (std::f32::consts::TAU * 300.0 * n / 96_000.0).sin() * 0.6;
        }
        if let Some((id, fader)) = stem {
            let mut pcm = [0.0f32; TICK_SAMPLES];
            for (i, s) in pcm.iter_mut().enumerate() {
                let n = (tick as usize * TICK_SAMPLES + i) as f32;
                *s = (std::f32::consts::TAU * 220.0 * n / 48_000.0).sin() * 0.4;
            }
            p.push_stem(id, fader, &pcm);
        }
        p
    }

    // 2026-07-28 19:30:05 UTC.
    const STAMP: u64 = 1_785_267_005;

    #[test]
    fn files_are_named_for_people() {
        assert_eq!(take_base(STAMP), "jamstream-2026-07-28-1930");
        assert_eq!(take_base(0), "jamstream-1970-01-01-0000");
        // A midnight boundary in a leap-adjacent February.
        assert_eq!(take_base(1_772_236_800), "jamstream-2026-02-28-0000");
    }

    #[test]
    fn stem_names_survive_hostile_and_duplicate_members() {
        let roster = vec![
            (MemberId(0), "Ana Q".to_owned()),
            (MemberId(1), "../../../etc/passwd".to_owned()),
            (MemberId(2), "Ana Q".to_owned()),
            (MemberId(3), "mix".to_owned()),
            (MemberId(4), "\u{1F3B8}".to_owned()),
        ];
        let mut used = vec!["mix".to_owned()];
        for id in 0..6u16 {
            let name = stem_name(&roster, &used, MemberId(id));
            used.push(name);
        }
        assert_eq!(
            &used[1..],
            &[
                "Ana-Q",
                "etcpasswd",
                "Ana-Q-2",
                "mix-3",
                "member-4",
                "member-5"
            ]
        );
    }

    /// A musician named in another script keeps their name on the take; only
    /// a name with no letters at all falls back to member-N.
    #[test]
    fn stem_names_keep_every_script() {
        assert_eq!(sanitize("Sørén"), "Sørén");
        assert_eq!(sanitize("日本語"), "日本語");
        assert_eq!(sanitize("Мария Петрова"), "Мария-Петрова");
        // ASCII behaves exactly as before.
        assert_eq!(sanitize("Ana Q"), "Ana-Q");
        assert_eq!(sanitize("../../../etc/passwd"), "etcpasswd");
        // All emoji reduces to nothing, so the id names the take.
        let roster = vec![(MemberId(7), "\u{1F3B8}\u{1F3B6}".to_owned())];
        assert_eq!(stem_name(&roster, &[], MemberId(7)), "member-7");
    }

    /// The names this recorder writes are the names the CLI later refuses or
    /// accepts, so every stem file has to clear the same hazard check, with
    /// no fake in the middle.
    #[test]
    fn stem_file_names_clear_the_windows_hazards() {
        let hostile = [
            "Sørén",
            "日本語",
            "NUL",
            "mix.flac:hidden",
            "a<b>c|d?e*f\"g\\h",
            "trailing. ",
            "\u{1F3B8}",
        ];
        let roster: Vec<(MemberId, String)> = hostile
            .iter()
            .enumerate()
            .map(|(i, name)| (MemberId(i as u16), (*name).to_owned()))
            .collect();
        let mut used = Vec::new();
        for (id, _) in &roster {
            let name = stem_name(&roster, &used, *id);
            let file = format!("{}-{name}.flac", take_base(STAMP));
            assert_eq!(
                jamstream_cloud::windows_hazard(&file),
                None,
                "{file:?} would not survive every filesystem"
            );
            used.push(name);
        }
    }

    #[test]
    fn the_mix_file_holds_exactly_what_was_fed() {
        let sink = MemSink::default();
        let mut rec = Recorder::new(sink.clone());
        rec.start(STAMP, None);
        assert_eq!(rec.state(), &RecordingState::Recording { stems: false });
        let mut expected = Vec::new();
        for tick in 0..100 {
            let p = payload(tick, None);
            expected.extend(p.mix.iter().map(|&s| crate::flac::to_i16(s)));
            rec.tick(&p);
        }
        rec.stop();
        assert_eq!(rec.state(), &RecordingState::Idle);
        let objects = sink.objects.lock().unwrap();
        assert_eq!(objects.len(), 1, "mix only: stems were off");
        let mix = &objects["jamstream-2026-07-28-1930-mix.flac"];
        assert!(mix.finished && !mix.aborted);
        assert_eq!(decode(&mix.bytes), expected);
    }

    #[test]
    fn a_stem_carries_the_faded_member_and_stays_aligned_with_the_mix() {
        let sink = MemSink::default();
        let mut rec = Recorder::new(sink.clone());
        let fader = Fader {
            gain_db: -6.0,
            pan: 0.5,
            muted: false,
        };
        rec.start(STAMP, Some(vec![(MemberId(3), "Ana".to_owned())]));
        // 40 ticks of mix alone; Ana's uplink starts decoding at tick 40.
        for tick in 0..80u64 {
            let p = payload(tick, (tick >= 40).then_some((MemberId(3), fader)));
            rec.tick(&p);
        }
        rec.stop();
        let objects = sink.objects.lock().unwrap();
        let stem = decode(&objects["jamstream-2026-07-28-1930-Ana.flac"].bytes);
        let mix = decode(&objects["jamstream-2026-07-28-1930-mix.flac"].bytes);
        assert_eq!(stem.len(), mix.len(), "stems must stay remixable");
        // Backfilled silence while she was not decodable.
        assert!(stem[..40 * TICK_STEREO_SAMPLES].iter().all(|&s| s == 0));
        // Then her audio, through the exact fader the mix used: recompute it
        // with the mixer and expect sample equality after quantization.
        let mut expected = Vec::new();
        let mut stereo = [0.0f32; TICK_STEREO_SAMPLES];
        for tick in 40..80u64 {
            let p = payload(tick, Some((MemberId(3), fader)));
            mix_into(
                &[(MemberId(3), &p.stem_pcm[0][..])],
                |_| fader,
                None,
                &mut stereo,
            );
            expected.extend(stereo.iter().map(|&s| crate::flac::to_i16(s)));
        }
        assert_eq!(&stem[40 * TICK_STEREO_SAMPLES..], &expected[..]);
        // The pan actually did something: left louder than right at -0.5...
        // pan 0.5 leans right, so right outweighs left.
        let energy = |ch: usize| -> i64 {
            stem[40 * TICK_STEREO_SAMPLES..]
                .iter()
                .skip(ch)
                .step_by(2)
                .map(|&s| i64::from(s) * i64::from(s))
                .sum()
        };
        assert!(energy(1) > energy(0) * 2);
    }

    #[test]
    fn a_failing_sink_aborts_the_take_and_says_why() {
        let sink = MemSink {
            fail_after: Some(20_000),
            ..MemSink::default()
        };
        let mut rec = Recorder::new(sink.clone());
        rec.start(STAMP, Some(vec![(MemberId(0), "Ana".to_owned())]));
        for tick in 0..2_000u64 {
            rec.tick(&payload(tick, Some((MemberId(0), Fader::default()))));
            if matches!(rec.state(), RecordingState::Failed { .. }) {
                break;
            }
        }
        let RecordingState::Failed { reason } = rec.state() else {
            panic!("a dead sink must fail the take, got {:?}", rec.state());
        };
        assert!(reason.contains("bucket went away"), "{reason}");
        let objects = sink.objects.lock().unwrap();
        assert!(objects.values().all(|o| o.aborted && !o.finished));
        // And a later take starts clean.
        drop(objects);
    }

    #[test]
    fn start_while_recording_and_stop_while_idle_are_no_ops() {
        let sink = MemSink::default();
        let mut rec = Recorder::new(sink.clone());
        rec.stop();
        assert_eq!(rec.state(), &RecordingState::Idle);
        rec.start(STAMP, None);
        rec.start(STAMP + 3_600, None);
        rec.tick(&payload(0, None));
        rec.stop();
        let objects = sink.objects.lock().unwrap();
        assert_eq!(objects.len(), 1, "the second start must not open files");
    }

    /// The backfilled head of a late stem has to be exactly the bytes the
    /// encoder would have written for that silence, or the file is a lie about
    /// where the audio sits. Checked twice: byte for byte against a fresh
    /// encoder, and by decoding with claxon rather than flacenc.
    #[test]
    fn a_late_stem_is_byte_identical_to_encoding_its_own_silence() {
        // Not a multiple of the FLAC block, so the copied head stops
        // mid-block and the stem's encoder has to carry the remainder.
        let joined_at = 1_000u64;
        let sink = MemSink::default();
        let mut rec = Recorder::new(sink.clone());
        let fader = Fader {
            gain_db: -3.0,
            pan: -0.25,
            muted: false,
        };
        rec.start(STAMP, Some(vec![(MemberId(7), "Kai".to_owned())]));
        for tick in 0..joined_at + 300 {
            let p = payload(tick, (tick >= joined_at).then_some((MemberId(7), fader)));
            rec.tick(&p);
        }
        rec.stop();

        // What a single encoder writing silence and then Kai would produce.
        let mut enc = FlacEncoder::new();
        let mut expected_bytes = enc.header().unwrap();
        let mut expected_samples = Vec::new();
        let silent = [0.0f32; TICK_STEREO_SAMPLES];
        for _ in 0..joined_at {
            enc.push(&silent, &mut expected_bytes).unwrap();
            expected_samples.extend(silent.iter().map(|&s| crate::flac::to_i16(s)));
        }
        let mut stereo = [0.0f32; TICK_STEREO_SAMPLES];
        for tick in joined_at..joined_at + 300 {
            let p = payload(tick, Some((MemberId(7), fader)));
            mix_into(
                &[(MemberId(7), &p.stem_pcm[0][..])],
                |_| fader,
                None,
                &mut stereo,
            );
            enc.push(&stereo, &mut expected_bytes).unwrap();
            expected_samples.extend(stereo.iter().map(|&s| crate::flac::to_i16(s)));
        }
        enc.finish(&mut expected_bytes).unwrap();

        let objects = sink.objects.lock().unwrap();
        let stem = &objects["jamstream-2026-07-28-1930-Kai.flac"];
        assert!(stem.finished && !stem.aborted);
        let first_diff = stem
            .bytes
            .iter()
            .zip(&expected_bytes)
            .position(|(a, b)| a != b);
        assert_eq!(
            (stem.bytes.len(), first_diff),
            (expected_bytes.len(), None),
            "the copied head is not the silence an encoder would have written"
        );
        assert_eq!(decode(&stem.bytes), expected_samples);
    }

    /// Wall budgets scale the way the harness scales them, off one variable.
    fn perf_budget(laptop: Duration) -> Duration {
        let scale = std::env::var("JAMSTREAM_PERF_BUDGET_SECS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map_or(1.0, |v| v / 30.0);
        Duration::from_secs_f64(laptop.as_secs_f64() * scale)
    }

    /// The one that punched a hole in the mix: open_stem used to encode the
    /// whole take's silence inline, about 38 ms per minute of take, so past
    /// roughly 26 minutes a musician walking in cost more than the queue's one
    /// second and the mix recording went to gap-filled silence. Measured on
    /// this test's take before the fix: 840 ms. After: under a millisecond.
    #[test]
    fn a_join_deep_into_a_take_does_not_stall_the_mix() {
        let minutes = 45u64;
        let ticks = minutes * 60 * 400;
        let sink = MemSink::default();
        let mut rec = Recorder::new(sink.clone());
        rec.start(STAMP, Some(vec![(MemberId(0), "Ana".to_owned())]));
        // A silent take: the backfill's cost is the same either way, and this
        // keeps 45 minutes of ticks affordable in a test.
        let mut quiet = RecordPayload::default();
        quiet.push_stem(MemberId(0), Fader::default(), &[0.0; TICK_SAMPLES]);
        // The queue the mix tick submits into, modelled: the recorder owes the
        // mix clock a tick every 2.5 ms, and QUEUE_TICKS in flight is all the
        // slack there is before submit_tick starts counting gaps.
        let mut backlog = 0.0f64;
        let mut peak_backlog = 0.0f64;
        for _ in 0..ticks {
            let started = Instant::now();
            rec.tick(&quiet);
            backlog = (backlog + started.elapsed().as_secs_f64() / 0.0025 - 1.0).max(0.0);
            peak_backlog = peak_backlog.max(backlog);
        }
        let mut join = quiet;
        join.push_stem(MemberId(1), Fader::default(), &[0.0; TICK_SAMPLES]);
        let started = Instant::now();
        rec.tick(&join);
        let stall = started.elapsed();
        rec.stop();
        assert_eq!(rec.state(), &RecordingState::Idle);

        let budget = perf_budget(Duration::from_millis(25));
        assert!(
            stall < budget,
            "a join {minutes} minutes into a take stalled the recorder for {stall:?}, \
             budget {budget:?}"
        );
        let peak = peak_backlog + stall.as_secs_f64() / 0.0025;
        assert!(
            peak < QUEUE_TICKS as f64,
            "the recorder fell {peak:.0} ticks behind the mix clock; \
             the queue holds {QUEUE_TICKS} and the rest is a hole in the mix"
        );
        let objects = sink.objects.lock().unwrap();
        assert_eq!(objects.len(), 3, "mix and two stems");
        assert!(objects.values().all(|o| o.finished && !o.aborted));
    }

    /// A sink whose first write parks until the test releases it: the worker
    /// thread wedges exactly the way a stalled upload would.
    struct WedgedSink {
        hold: Arc<Mutex<mpsc::Receiver<()>>>,
    }

    struct WedgedObject {
        hold: Arc<Mutex<mpsc::Receiver<()>>>,
    }

    impl RecordingSink for WedgedSink {
        fn open(&mut self, _name: &str) -> io::Result<Box<dyn RecordingObject>> {
            Ok(Box::new(WedgedObject {
                hold: Arc::clone(&self.hold),
            }))
        }
    }

    impl RecordingObject for WedgedObject {
        fn write(&mut self, _chunk: &[u8]) -> io::Result<()> {
            // Blocks until the test sends; hung up means proceed freely. The
            // timeout is a backstop: a failing assertion drops the worker,
            // which joins this thread, and waiting forever would hang the
            // suite instead of failing it.
            let _ = self
                .hold
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(20));
            Ok(())
        }

        fn finish(self: Box<Self>) -> io::Result<()> {
            Ok(())
        }

        fn abort(self: Box<Self>) -> io::Result<()> {
            Ok(())
        }
    }

    /// The property the tick task lives by: however wedged the sink, and
    /// however full the queue, submit_tick returns immediately and the loss
    /// is counted instead of waited out.
    #[test]
    fn a_starved_recorder_never_blocks_the_submitting_task() {
        let (release, hold) = mpsc::channel::<()>();
        let worker = RecordWorker::spawn(WedgedSink {
            hold: Arc::new(Mutex::new(hold)),
        })
        .unwrap();
        worker.start(STAMP, None);
        // Enough ticks to fill the queue several times over while the worker
        // is stuck inside its first write.
        let submissions = 4 * QUEUE_TICKS as u64;
        let started = Instant::now();
        for tick in 0..submissions {
            worker.submit_tick(Box::new(payload(tick, None)));
        }
        let elapsed = started.elapsed();
        // Generous even for a loaded CI runner; a single blocking write here
        // would hold this for as long as the test's release channel exists.
        assert!(
            elapsed < Duration::from_secs(2),
            "submitting took {elapsed:?}; the tick task was made to wait"
        );
        assert!(
            worker.gap_ticks() > 0,
            "a wedged sink with a bounded queue must be dropping ticks"
        );
        drop(release); // Un-wedge so the worker can finish and join.
        worker.stop();
    }

    /// The disk contract in one object: finish closes the handle and renames
    /// the .part into place, so the done name holds the exact bytes and
    /// nothing that looks like a recording is left half-made. Closing before
    /// the rename is unobservable on unix, so the behavior is what is pinned.
    #[test]
    fn a_disk_object_finishes_by_renaming_its_part_file_into_place() {
        let dir =
            std::env::temp_dir().join(format!("jamstream-record-finish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut sink = DiskSink::new(&dir);
        let mut object = sink.open("take.flac").unwrap();
        object.write(b"flac bytes").unwrap();
        assert!(dir.join("take.flac.part").exists());
        object.finish().unwrap();
        assert_eq!(std::fs::read(dir.join("take.flac")).unwrap(), b"flac bytes");
        assert!(
            !dir.join("take.flac.part").exists(),
            "the part file must not survive finish"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_worker_records_through_its_own_thread() {
        let dir = std::env::temp_dir().join(format!("jamstream-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let worker = RecordWorker::spawn(DiskSink::new(&dir)).unwrap();
        assert!(!worker.recording());
        worker.start(STAMP, Some(vec![(MemberId(2), "Bo".to_owned())]));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.recording() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(worker.recording());
        let mut expected = Vec::new();
        for tick in 0..QUEUE_TICKS as u64 / 2 {
            let p = payload(tick, Some((MemberId(2), Fader::default())));
            expected.extend(p.mix.iter().map(|&s| crate::flac::to_i16(s)));
            worker.submit_tick(Box::new(p));
        }
        worker.stop();
        drop(worker); // Joins the thread, so both files are finished.
        let mix = std::fs::read(dir.join("jamstream-2026-07-28-1930-mix.flac")).unwrap();
        assert_eq!(decode(&mix), expected);
        let stem = dir.join("jamstream-2026-07-28-1930-Bo.flac");
        assert_eq!(decode(&std::fs::read(&stem).unwrap()).len(), expected.len());
        // Nothing half-written left behind.
        let parts: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "part")
            })
            .collect();
        assert!(parts.is_empty(), "part files survived a finished take");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
