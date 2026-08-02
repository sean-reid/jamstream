//! Takes: the recordings sessions left behind, and getting them onto this
//! computer.
//!
//! Off Home rather than inside a session, because a take outlives the session
//! that made it: the machine is destroyed, the invites expire, and the take
//! sits in a bucket with a retention clock running.
//!
//! # One row per take, not a list of file names
//!
//! A take is one Record to Stop, and it is a mix plus one file per musician
//! when stems were armed. So a row is a take: when it started, how big the mix
//! is, how big the stems are, where each of those is now, and what pulling it
//! costs. That is what tells two takes of the same song apart. The session
//! above it carries the day, the length, the region, and what it spent.
//!
//! # The key comes from the keychain
//!
//! `jamstream recordings` reads the storage key from the environment, which is
//! right for a terminal and wrong here: a host who set the bucket up in the
//! Recording tab has the key in this computer's keychain already, and sending
//! them to a shell to export it would make the app the place you arm recording
//! and the terminal the only place you can collect. [`KeychainStores`] is that
//! fix, and it is the only thing about downloading that differs between the two
//! surfaces. The bytes move through the CLI's own engine.
//!
//! # What this screen will not do
//!
//! There is no preview player and no drag onto a DAW timeline. Both were
//! considered and dropped: a take you have downloaded is a file in a folder you
//! chose, and every player and every DAW already opens one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use egui::{RichText, Ui};
use jamstream_cli::CliError;
use jamstream_cli::reason::{self, Attempt};
use jamstream_cli::recordings::{self, Action, Take, TakeProgress};
use jamstream_cli::state::{RecordingRecord, SessionState, SessionStatus};
use jamstream_cli::storage::Stores;
use jamstream_cloud::cloudinit::RecordingStorage;
use jamstream_cloud::{ObjectStore, Retention};

use crate::creds::{self, CredStore, EnvReader};
use crate::exec::{Executor, Job};
use crate::reveal;
use crate::theme;

/// How often a download in flight redraws its own figures. A chunk is a few
/// hundred kilobytes, so reporting every one of them would be thousands of
/// repaint requests a second for a number that moves in tenths of a percent.
const PROGRESS_STEP_BYTES: u64 = 4 * 1_000_000;

/// The screen's own column width: a take row carries a size, a price, and a
/// button, which is wider than Home's column of prose.
const COLUMN_W: f32 = 760.0;

/// The download track: as wide as the buttons it stands in for, and a hairline
/// tall, because it is a readout rather than a control.
const TRACK_W: f32 = 180.0;
const TRACK_H: f32 = 6.0;

/// Under this many days left, the countdown is set in the danger ink: a take is
/// deleted permanently when the rule fires, and a weekend is enough to miss it.
const EXPIRY_SOON_DAYS: i64 = 3;

/// Where one session's takes are.
#[derive(Debug, Clone, PartialEq)]
pub enum Place {
    /// A bucket in the host's own cloud account.
    Bucket(RecordingRecord),
    /// This computer's disk, which is where a local session records.
    Disk(PathBuf),
}

/// One file of a take: the mix, or one musician's stem.
#[derive(Debug, Clone, PartialEq)]
pub struct TakeFile {
    /// The name the recorder gave it, which carries the take's time and, for a
    /// stem, the player.
    pub name: String,
    pub bytes: u64,
    /// The object in the bucket, when the take is in one.
    pub object: Option<Take>,
    /// Where the file is on this computer, when it is here.
    pub local: Option<PathBuf>,
    /// Still being written: a local take that has not been closed yet ends in
    /// `.part`, and nothing may offer it as a finished recording.
    pub partial: bool,
}

/// The mix half or the stems half of one take. Kept apart because the decision
/// a host makes is between them: stems are about five times the bytes, and that
/// is the whole reason the egress figure is worth showing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Part {
    pub files: Vec<TakeFile>,
}

impl Part {
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }

    /// Bytes not on this computer yet, which is what a download would move and
    /// therefore what it would cost.
    pub fn missing_bytes(&self) -> u64 {
        self.files
            .iter()
            .filter(|f| f.local.is_none())
            .map(|f| f.bytes)
            .sum()
    }

    /// True while any file of this part is still being written.
    pub fn writing(&self) -> bool {
        self.files.iter().any(|f| f.partial)
    }

    /// The file to show in the file manager: every one of them is in the same
    /// folder, so the first is the one to select.
    pub fn here(&self) -> Option<&Path> {
        self.files
            .iter()
            .find_map(|f| f.local.as_deref())
            .filter(|_| self.missing_bytes() == 0)
    }

    /// The objects a download would fetch, in the CLI engine's own type.
    fn wanted(&self) -> Vec<Take> {
        self.files
            .iter()
            .filter(|f| f.local.is_none())
            .filter_map(|f| f.object.clone())
            .collect()
    }
}

/// One take: everything a single Record to Stop produced.
#[derive(Debug, Clone, PartialEq)]
pub struct TakeRow {
    /// The name every file of the take shares: `jamstream-2026-07-28-1930`.
    pub base: String,
    pub mix: Part,
    pub stems: Part,
}

impl TakeRow {
    /// The time of day the take started, read out of its own name rather than
    /// from a clock: the recorder put it there, in UTC, and it is the one thing
    /// that tells two takes of the same song apart.
    pub fn at(&self) -> String {
        let stamp = self.base.rsplit('-').next().unwrap_or_default();
        if stamp.len() == 4 && stamp.bytes().all(|b| b.is_ascii_digit()) {
            format!("{}:{} UTC", &stamp[..2], &stamp[2..])
        } else {
            self.base.clone()
        }
    }
}

/// One session, and the takes it left behind.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTakes {
    pub session_id: String,
    pub short_id: String,
    /// When the session started, and how long it ran.
    pub started_unix: u64,
    pub secs: u64,
    pub running: bool,
    pub provider: String,
    pub region: String,
    /// What the machine cost, which is what the session itself spent. Not
    /// egress: the prices on the buttons below are a separate charge, and this
    /// figure has to be labelled so the two are not read as one family.
    pub spent_microusd: u64,
    pub place: Place,
    pub takes: Vec<TakeRow>,
    /// Days until the retention rule deletes these takes, and only when a
    /// provider is really enforcing one.
    pub expires_in_days: Option<i64>,
    /// The bucket's own sentence about a retention choice nothing is
    /// enforcing.
    pub unenforced: Option<String>,
    /// Why this session's bucket could not be listed.
    pub error: Option<String>,
    /// True until the bucket has answered.
    pub listing: bool,
    /// Where a download of this session's takes lands.
    pub dir: PathBuf,
}

impl SessionTakes {
    /// The bucket details, for a session that recorded to one.
    pub fn record(&self) -> Option<&RecordingRecord> {
        match &self.place {
            Place::Bucket(record) => Some(record),
            Place::Disk(_) => None,
        }
    }

    /// Whose bucket refused, which is who a refusal has to be explained in
    /// terms of. The recording's own provider rather than the session's,
    /// because that is the account the key belongs to.
    pub fn provider_name(&self) -> String {
        self.record()
            .map_or_else(|| self.provider.clone(), |r| r.provider.clone())
    }

    /// `Tue 28 Jul, 47 min, digitalocean sfo3`.
    pub fn header(&self) -> String {
        let place = match &self.place {
            Place::Bucket(record) => format!("{} {}", record.provider, record.region),
            Place::Disk(_) => "this computer".to_owned(),
        };
        format!(
            "{}, {}, {place}",
            day_label(self.started_unix),
            length(self.secs)
        )
    }
}

/// A take file found on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTake {
    pub path: PathBuf,
    pub bytes: u64,
    /// When the file was last written, which for a finished take is when Stop
    /// was pressed.
    pub modified_unix: u64,
}

/// Reads the recordings folder. A folder that is not there is no takes rather
/// than an error: it is created by the first local recording.
pub fn local_takes(dir: &Path) -> Vec<LocalTake> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with("jamstream-") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified_unix = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(LocalTake {
            path,
            bytes: meta.len(),
            modified_unix,
        });
    }
    out
}

/// Builds the screen's rows from what this machine knows: the session records,
/// the bucket sidecars beside them, and the take files on this disk.
///
/// A session with no bucket and no local file left no take, so it is not here.
/// Bucket rows arrive empty and are filled in when the listing job answers;
/// a local session's takes are on this computer already, so they are complete
/// from the start.
///
/// Local takes are matched to the session that was running when they were
/// written, because the recorder puts every local take in one folder and the
/// only thing tying a file to a session is the clock. The file's own name
/// carries the time it started, so the row can be checked by eye.
pub fn rows_from(
    sessions: &[(SessionState, Option<RecordingRecord>)],
    local: &[LocalTake],
    local_dir: &Path,
    downloads_dir: &Path,
    now_unix: u64,
) -> Vec<SessionTakes> {
    let mut rows = Vec::new();
    for (session, record) in sessions {
        let running = session.status == SessionStatus::Running;
        let until = if running {
            now_unix
        } else {
            session.ended_unix.unwrap_or(now_unix)
        };
        let secs = until.saturating_sub(session.created_unix);
        let mine: Vec<&LocalTake> = local
            .iter()
            .filter(|take| {
                take.modified_unix >= session.created_unix
                    && take.modified_unix <= until.max(now_unix)
            })
            .collect();
        let place = match record {
            Some(record) => Place::Bucket(record.clone()),
            None if !mine.is_empty() => Place::Disk(local_dir.to_owned()),
            // A session that recorded nowhere has nothing to show here.
            None => continue,
        };
        let takes = match &place {
            Place::Bucket(_) => Vec::new(),
            Place::Disk(_) => takes_from_disk(&mine),
        };
        let (expires_in_days, unenforced) = expiry(record.as_ref(), session.created_unix, now_unix);
        rows.push(SessionTakes {
            session_id: session.session_id_hex.clone(),
            short_id: session.session_id_hex.chars().take(8).collect(),
            started_unix: session.created_unix,
            secs,
            running,
            provider: session.provider.clone(),
            region: session.region.clone(),
            spent_microusd: spent(session.hourly_microusd, secs),
            place,
            takes,
            expires_in_days,
            unenforced,
            error: None,
            listing: record.is_some(),
            dir: downloads_dir.join(session.session_id_hex.chars().take(8).collect::<String>()),
        });
    }
    // Newest session first: the take a musician wants is nearly always from
    // the last thing they played.
    rows.sort_by_key(|row| std::cmp::Reverse(row.started_unix));
    rows
}

/// Groups a bucket listing into takes. Called when the listing job answers.
pub fn takes_from_objects(objects: &[Take], dir: &Path) -> Result<Vec<TakeRow>, CliError> {
    // The plan is what decides whether a take is already here, and it refuses
    // a key that would land outside the folder before any of this is drawn.
    let plan = recordings::plan_downloads(objects, dir)?;
    let mut files = Vec::new();
    for (object, action) in objects.iter().zip(plan) {
        files.push(TakeFile {
            name: file_name(&object.name),
            bytes: object.size,
            object: Some(object.clone()),
            local: match action {
                Action::Have => Some(recordings::destination(dir, object)?),
                Action::Fetch => None,
            },
            partial: false,
        });
    }
    Ok(group(files))
}

fn takes_from_disk(local: &[&LocalTake]) -> Vec<TakeRow> {
    let files = local
        .iter()
        .map(|take| {
            let name = take.path.file_name().unwrap_or_default().to_string_lossy();
            TakeFile {
                name: name.trim_end_matches(".part").to_owned(),
                bytes: take.bytes,
                object: None,
                local: Some(take.path.clone()),
                partial: name.ends_with(".part"),
            }
        })
        .collect();
    group(files)
}

/// Sorts files into takes by the name they share, mix apart from stems.
fn group(mut files: Vec<TakeFile>) -> Vec<TakeRow> {
    files.sort_by(|a, b| a.name.cmp(&b.name));
    let mut rows: Vec<TakeRow> = Vec::new();
    for file in files {
        let base = base_of(&file.name);
        let row = match rows.iter_mut().find(|row| row.base == base) {
            Some(row) => row,
            None => {
                rows.push(TakeRow {
                    base,
                    mix: Part::default(),
                    stems: Part::default(),
                });
                rows.last_mut().expect("just pushed")
            }
        };
        if file.name.ends_with("-mix.flac") {
            row.mix.files.push(file);
        } else {
            row.stems.files.push(file);
        }
    }
    // Newest take first, which is the order the names sort in.
    rows.sort_by(|a, b| b.base.cmp(&a.base));
    rows
}

/// The name every file of one take shares: everything before the last dash,
/// which is what the recorder builds them from.
fn base_of(name: &str) -> String {
    match name.rsplit_once('-') {
        Some((base, _)) => base.to_owned(),
        None => name.to_owned(),
    }
}

/// The last component of an object name, since a bucket key may carry folders.
fn file_name(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).to_owned()
}

/// How long a take has left, and the reason when nothing is counting.
///
/// A countdown is drawn only when the launch recorded that the provider took
/// the lifecycle rule. Anything else, including a session launched before that
/// answer was written down, gets no countdown: a clock for a deletion nothing
/// will perform is worse than no clock.
///
/// The clock runs from the session's own start rather than from each object's
/// age, which is what the rule actually measures. The two differ by the length
/// of the session, and a rule fires on a day boundary anyway.
fn expiry(
    record: Option<&RecordingRecord>,
    created_unix: u64,
    now_unix: u64,
) -> (Option<i64>, Option<String>) {
    let Some(record) = record else {
        return (None, None);
    };
    match &record.applied {
        Some(jamstream_cli::state::RetentionApplied::Unenforced { note }) => {
            (None, Some(note.clone()))
        }
        Some(jamstream_cli::state::RetentionApplied::ServerSide) => {
            let Ok(retention) = record.retention.parse::<Retention>() else {
                return (None, None);
            };
            let Some(days) = retention.days() else {
                return (None, None);
            };
            let deadline = created_unix + u64::from(days) * 86_400;
            let left = (deadline as i64 - now_unix as i64).div_euclid(86_400);
            (Some(left), None)
        }
        None => (None, None),
    }
}

fn spent(hourly_microusd: u64, secs: u64) -> u64 {
    ((u128::from(hourly_microusd) * u128::from(secs)) / 3600) as u64
}

/// `47 min`, `2 h 10 min`.
fn length(secs: u64) -> String {
    if secs >= 3600 {
        format!("{} h {:02} min", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{} min", secs / 60)
    }
}

/// `Tue 28 Jul`, in UTC, which is the clock the take names are in.
///
/// A weekday and a day of the month, because a musician remembers "Tuesday"
/// and not a date. The year is left off: a take older than a retention rule
/// does not exist, and the day is enough to pick one out of a list.
fn day_label(unix: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = (unix / 86_400) as i64;
    let (_, month, day) = jamstream_cloud::civil_from_days(days);
    let weekday = DAYS[days.rem_euclid(7) as usize];
    let month = MONTHS[(month as usize).clamp(1, 12) - 1];
    format!("{weekday} {day} {month}")
}

/// One sentence for the screen; the whole error for the log.
///
/// The Takes screen's binding of the shared builder: every refusal here is a
/// bucket refusing to be read, so the remedy is about listing and reading,
/// and the closing verb is this screen's Refresh. See
/// [`jamstream_cli::reason`] for why the provider's own response never
/// reaches a row.
///
/// `provider` is the recording's own provider name, because the remedy for a
/// refusal is a different act on each of them; an unparseable name gets the
/// remedy that holds everywhere.
///
/// Public because the snapshot fixture renders the row a refusal really
/// produces, through this same mapping.
pub fn error_sentence(doing: &str, provider: &str, err: &CliError) -> String {
    reason::error_sentence(doing, Attempt::Takes, provider.parse().ok(), err)
}

/// Opens a session's bucket with the key this computer's keychain holds.
///
/// The whole reason the app can fetch a take at all. The CLI's own
/// [`jamstream_cli::storage::EnvStores`] reads the environment, which is right
/// for a terminal and would send a host who configured everything here off to
/// find a key the app already has.
pub struct KeychainStores {
    creds: Arc<dyn CredStore>,
    env: EnvReader,
}

impl KeychainStores {
    pub fn new(creds: Arc<dyn CredStore>, env: EnvReader) -> KeychainStores {
        KeychainStores { creds, env }
    }
}

impl Stores for KeychainStores {
    fn open(&self, record: &RecordingRecord) -> Result<Arc<dyn ObjectStore>, CliError> {
        let provider = jamstream_cli::storage::provider_kind(&record.provider)?;
        // The one place the app reads a storage key for a download. It goes
        // into the signing client and nowhere else: not a log line, not a
        // command line, not a field on the row.
        let credential = creds::storage_credential(self.creds.as_ref(), &self.env, provider)
            .map_err(CliError::Usage)?;
        Ok(RecordingStorage {
            provider,
            bucket: record.bucket.clone(),
            region: record.region.clone(),
            retention: record.retention.parse().unwrap_or_default(),
            credential,
            stems: record.stems,
        }
        .object_store()?)
    }
}

/// What a download reports while it runs. Written from the executor thread and
/// read by the frame loop, so it is atomics rather than a channel: the frame
/// loop must never wait on the network to draw.
#[derive(Default)]
struct Meter {
    /// Bytes on disk so far, across every take in this download.
    done: AtomicU64,
    /// The file being written right now.
    name: Mutex<String>,
    /// Bytes already reported, so a chunk that moves the figure by nothing
    /// does not wake the paint thread.
    reported: AtomicU64,
}

/// The [`TakeProgress`] the app hands the CLI's download engine.
struct MeterProgress {
    meter: Arc<Meter>,
    /// Bytes from takes already finished in this download.
    base: u64,
}

impl TakeProgress for MeterProgress {
    fn started(&mut self, take: &str, _expected: u64) -> Result<(), CliError> {
        *self.meter.name.lock().expect("download name") = take.to_owned();
        Ok(())
    }

    fn advanced(&mut self, _take: &str, written: u64, _expected: u64) -> Result<(), CliError> {
        let done = self.base + written;
        let reported = self.meter.reported.load(Ordering::Relaxed);
        if done < reported + PROGRESS_STEP_BYTES {
            return Ok(());
        }
        self.meter.reported.store(done, Ordering::Relaxed);
        self.meter.done.store(done, Ordering::Relaxed);
        Ok(())
    }

    fn finished(&mut self, _take: &str, written: u64) -> Result<(), CliError> {
        self.base += written;
        self.meter.done.store(self.base, Ordering::Relaxed);
        self.meter.reported.store(self.base, Ordering::Relaxed);
        Ok(())
    }
}

/// Which half of a take a download is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Half {
    Mix,
    Stems,
}

impl Half {
    fn label(self) -> &'static str {
        match self {
            Half::Mix => "mix",
            Half::Stems => "stems",
        }
    }
}

/// A download in flight.
struct Fetching {
    session_id: String,
    base: String,
    half: Half,
    /// Bytes the whole download will move, for the bar.
    total: u64,
    dir: PathBuf,
    meter: Arc<Meter>,
    job: Job<Result<u64, String>>,
}

/// What one round of asking every bucket answered: the session id, and either
/// its takes or the reason it could not be listed. One unreachable bucket does
/// not hide the rest, exactly as in `jamstream recordings`.
type Listing = Vec<(String, Result<Vec<Take>, String>)>;

/// What the last download left behind, shown where it happened.
pub struct Landed {
    pub session_id: String,
    pub base: String,
    pub half: Half,
    pub result: Result<PathBuf, String>,
}

pub struct TakesScreen {
    pub rows: Vec<SessionTakes>,
    /// Why the session records could not be read at all.
    pub error: Option<String>,
    /// The path shown as copyable text when the file manager could not be
    /// opened, with the platform's reason.
    pub reveal_error: Option<(PathBuf, String)>,
    pub landed: Option<Landed>,
    listing: Option<Job<Listing>>,
    fetching: Option<Fetching>,
    stores: Arc<dyn Stores>,
    exec: Arc<Executor>,
}

impl TakesScreen {
    pub fn new(creds: Arc<dyn CredStore>, env: EnvReader, exec: Arc<Executor>) -> TakesScreen {
        Self::with_stores(Arc::new(KeychainStores::new(creds, env)), exec)
    }

    /// The screen over a given way of opening a bucket. A seam, not a layer:
    /// the tests point it at a store that holds real objects and drive the same
    /// listing and download the app runs.
    pub fn with_stores(stores: Arc<dyn Stores>, exec: Arc<Executor>) -> TakesScreen {
        TakesScreen {
            rows: Vec::new(),
            error: None,
            reveal_error: None,
            landed: None,
            listing: None,
            fetching: None,
            stores,
            exec,
        }
    }

    /// Reads this machine's session records and starts listing every bucket.
    /// Called on the way in, and by Refresh.
    pub fn reload(&mut self) {
        self.error = None;
        match read_sessions() {
            Ok(sessions) => {
                let local_dir = jamstream_cli::state::recordings_dir().unwrap_or_default();
                self.rows = rows_from(
                    &sessions,
                    &local_takes(&local_dir),
                    &local_dir,
                    &downloads_dir(),
                    now_unix(),
                );
            }
            Err(err) => {
                self.rows = Vec::new();
                self.error = Some(err);
            }
        }
        let rows = std::mem::take(&mut self.rows);
        self.load(rows);
    }

    /// Shows `rows` and starts asking every bucket among them what it holds.
    ///
    /// The way in for a test: [`reload`](TakesScreen::reload) reads the session
    /// records of whoever is running it, which is not something a test may do,
    /// and everything after that point is the real thing.
    pub fn load(&mut self, rows: Vec<SessionTakes>) {
        self.rows = rows;
        self.landed = None;
        self.begin_listing();
    }

    /// Lists every bucket this machine recorded to, in one job.
    ///
    /// One job rather than one per session, because the executor runs them one
    /// at a time anyway and a single job is a single thing to poll and a single
    /// thing to cancel by dropping.
    fn begin_listing(&mut self) {
        let wanted: Vec<(SessionState, RecordingRecord)> = self
            .rows
            .iter()
            .filter_map(|row| row.record().map(|r| (bare_session(row), r.clone())))
            .collect();
        if wanted.is_empty() {
            self.listing = None;
            return;
        }
        let stores = Arc::clone(&self.stores);
        self.listing = Some(self.exec.run(async move {
            let mut out = Vec::new();
            for (session, record) in &wanted {
                let listed = recordings::takes_for(session, record, stores.as_ref())
                    .await
                    .map_err(|e| {
                        error_sentence(
                            &format!("listing {}", session.session_id_hex),
                            &record.provider,
                            &e,
                        )
                    });
                out.push((session.session_id_hex.clone(), listed));
            }
            out
        }));
    }

    /// True while a bucket is being listed or a take is being fetched.
    pub fn busy(&self) -> bool {
        self.listing.is_some() || self.fetching.is_some()
    }

    /// Applies whatever a job has finished. Called once per frame.
    pub fn poll(&mut self) {
        if let Some(job) = &mut self.listing
            && let Some(results) = job.poll()
        {
            self.listing = None;
            for (session_id, listed) in results {
                let Some(row) = self.rows.iter_mut().find(|r| r.session_id == session_id) else {
                    continue;
                };
                row.listing = false;
                let provider = row.provider_name();
                match listed {
                    Ok(objects) => match takes_from_objects(&objects, &row.dir) {
                        Ok(takes) => row.takes = takes,
                        Err(err) => {
                            row.error =
                                Some(error_sentence("planning the downloads", &provider, &err));
                        }
                    },
                    Err(err) => row.error = Some(err),
                }
            }
        }
        if let Some(fetching) = &mut self.fetching
            && let Some(result) = fetching.job.poll()
        {
            let fetching = self.fetching.take().expect("just polled");
            self.landed = Some(Landed {
                session_id: fetching.session_id.clone(),
                base: fetching.base.clone(),
                half: fetching.half,
                result: result.map(|_| fetching.dir.clone()),
            });
            // What is on disk has changed, so what the rows offer has changed
            // with it: the part that landed reads Reveal from here on.
            self.refresh_local();
        }
    }

    /// Re-decides which files are already on this computer, without going back
    /// to the bucket. The plan is the same one that priced the download.
    fn refresh_local(&mut self) {
        for row in &mut self.rows {
            if row.record().is_none() {
                continue;
            }
            let provider = row.provider_name();
            let objects: Vec<Take> = row
                .takes
                .iter()
                .flat_map(|take| take.mix.files.iter().chain(take.stems.files.iter()))
                .filter_map(|file| file.object.clone())
                .collect();
            if objects.is_empty() {
                continue;
            }
            match takes_from_objects(&objects, &row.dir) {
                Ok(takes) => row.takes = takes,
                Err(err) => {
                    row.error = Some(error_sentence("re-planning the downloads", &provider, &err));
                }
            }
        }
    }

    /// Starts a download of one half of one take.
    ///
    /// Public so a test drives the real thing: the plan, the traversal guard,
    /// the CLI's own engine, and the size check are all in here, and a test
    /// that reimplemented the body would agree with itself about all of them.
    pub fn begin_download(&mut self, session_id: &str, base: &str, half: Half) -> bool {
        if self.fetching.is_some() {
            return false;
        }
        let Some(row) = self.rows.iter().find(|r| r.session_id == session_id) else {
            return false;
        };
        let Some(record) = row.record().cloned() else {
            return false;
        };
        let Some(take) = row.takes.iter().find(|t| t.base == base) else {
            return false;
        };
        let part = match half {
            Half::Mix => &take.mix,
            Half::Stems => &take.stems,
        };
        let wanted = part.wanted();
        if wanted.is_empty() {
            return false;
        }
        let total: u64 = wanted.iter().map(|t| t.size).sum();
        let dir = row.dir.clone();
        let meter = Arc::new(Meter::default());
        let stores = Arc::clone(&self.stores);
        let (job_dir, job_meter) = (dir.clone(), Arc::clone(&meter));
        self.landed = None;
        self.fetching = Some(Fetching {
            session_id: session_id.to_owned(),
            base: base.to_owned(),
            half,
            total,
            dir,
            meter,
            job: self.exec.run(async move {
                let provider = record.provider.clone();
                let store = stores
                    .open(&record)
                    .map_err(|e| error_sentence("opening the bucket", &provider, &e))?;
                let mut progress = MeterProgress {
                    meter: job_meter,
                    base: 0,
                };
                let refs: Vec<&Take> = wanted.iter().collect();
                recordings::fetch_takes(
                    store.as_ref(),
                    &record.bucket,
                    &refs,
                    &job_dir,
                    &mut progress,
                )
                .await
                .map_err(|e| error_sentence("downloading takes", &provider, &e))
            }),
        });
        true
    }

    /// Parks a download partway through, with `done` of `total` bytes on disk.
    ///
    /// The app reaches this state only by pressing a Download button, and a
    /// real one is at a different point in the transfer every run. So this is
    /// how a fixture holds the state a musician spends the longest looking at:
    /// the same fields the running download writes, and a job that never
    /// answers, so the bar stays where it was put.
    #[doc(hidden)]
    pub fn park_download(
        &mut self,
        session_id: &str,
        base: &str,
        half: Half,
        total: u64,
        done: u64,
    ) {
        let Some(row) = self.rows.iter().find(|r| r.session_id == session_id) else {
            return;
        };
        let meter = Arc::new(Meter::default());
        meter.done.store(done, Ordering::Relaxed);
        *meter.name.lock().expect("download name") = format!("{base}-mix.flac");
        self.landed = None;
        self.fetching = Some(Fetching {
            session_id: session_id.to_owned(),
            base: base.to_owned(),
            half,
            total,
            dir: row.dir.clone(),
            meter,
            job: self.exec.run(std::future::pending()),
        });
    }

    /// Shows one file in the platform's file manager, keeping the path on
    /// screen when there is no window to open it in.
    pub fn reveal(&mut self, path: &Path) {
        self.reveal_error = match reveal::show(path) {
            Ok(()) => None,
            Err(err) => Some((path.to_owned(), err)),
        };
    }
}

/// The session record, as the CLI's listing wants it. Only the id is read out
/// of it, and rebuilding one here keeps the screen from carrying a copy of the
/// issuer private key for every session on the machine.
fn bare_session(row: &SessionTakes) -> SessionState {
    SessionState {
        session_id_hex: row.session_id.clone(),
        provider: row.provider.clone(),
        region: row.region.clone(),
        instance_id: String::new(),
        address: String::new(),
        created_unix: row.started_unix,
        hourly_microusd: 0,
        issuer_private_key_b64: String::new(),
        server_public_key_b64: String::new(),
        invites: Vec::new(),
        status: SessionStatus::Ended,
        ended_unix: None,
    }
}

/// Every session on this machine with the bucket it recorded to, oldest first.
fn read_sessions() -> Result<Vec<(SessionState, Option<RecordingRecord>)>, String> {
    let sessions = jamstream_cli::state::list().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (_, session) in sessions {
        let record = jamstream_cli::state::load_recording(&session.session_id_hex)
            .map_err(|e| e.to_string())?;
        out.push((session, record));
    }
    Ok(out)
}

/// Where a downloaded take lands: a JamStream folder in the platform's music
/// directory, which is where music belongs and somewhere a DAW already looks.
/// Each session gets its own folder under it, because two sessions can record
/// takes a minute apart and the recorder names by clock time.
pub fn downloads_dir() -> PathBuf {
    resolve_downloads_dir(
        dirs::audio_dir()
            .or_else(dirs::download_dir)
            .or_else(dirs::home_dir)
            // The CLI's state dir refuses outright with no platform
            // directory, because it holds keys. This is where downloads land
            // and what the reveal button opens, so refusal would kill the
            // feature; the current directory keeps the path absolute, where
            // the bare name resolved against whatever the process started in
            // (System32, from the Windows Start menu).
            .or_else(|| std::env::current_dir().ok()),
    )
}

/// The chosen base with our folder inside it; no base at all leaves the bare
/// relative name, the honest floor when even the current directory is
/// unknowable.
fn resolve_downloads_dir(base: Option<PathBuf>) -> PathBuf {
    base.map(|dir| dir.join("JamStream"))
        .unwrap_or_else(|| PathBuf::from("JamStream"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// Rendering.

impl TakesScreen {
    pub fn ui(&mut self, ui: &mut Ui) {
        self.poll();
        let room = ui.available_height();
        let mut refresh = false;
        let mut reveal = None;
        let mut download = None;
        // A column of its own width, pinned to the top rather than pushed into
        // the upper third the way Home and the wizard are: this is a list that
        // scrolls, and the space above a list is space the list could have had.
        let width = ui.available_width().min(COLUMN_W);
        let pad = ((ui.available_width() - width) / 2.0).max(0.0);
        ui.horizontal(|ui| {
            ui.add_space(pad);
            // An explicit size, because a child of a horizontal layout cannot
            // answer how tall it is allowed to be, and the scroll area below
            // has to fill what is left of the window.
            ui.allocate_ui_with_layout(
                egui::vec2(width, room),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(width);
                    ui.add_space(theme::SPACE_MD);
                    ui.horizontal(|ui| {
                        ui.label(theme::title(ui, "Takes"));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            refresh = ui
                                .add_enabled(!self.busy(), egui::Button::new("Refresh"))
                                .clicked();
                        });
                    });
                    ui.label(theme::muted(
                        ui,
                        "What your sessions recorded. A take in a bucket costs egress to \
                 download, and the price is on the button.",
                    ));
                    ui.add_space(theme::SPACE_MD);
                    if let Some(err) = self.error.clone() {
                        theme::reason(ui, err);
                    }
                    if self.rows.is_empty() && self.error.is_none() {
                        theme::panel(ui).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(theme::muted(
                                ui,
                                "No takes yet. Arm recording when you host a session and they \
                         show up here.",
                            ));
                        });
                    }
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for row in &self.rows {
                                theme::panel(ui).show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    if let Some(action) =
                                        session_card(ui, row, self.fetching.as_ref())
                                    {
                                        match action {
                                            CardAction::Reveal(path) => reveal = Some(path),
                                            CardAction::Download(base, half) => {
                                                download =
                                                    Some((row.session_id.clone(), base, half));
                                            }
                                        }
                                    }
                                    if let Some(landed) = &self.landed
                                        && landed.session_id == row.session_id
                                    {
                                        landing(ui, landed);
                                    }
                                });
                                ui.add_space(theme::SPACE_MD);
                            }
                        });
                    if let Some((path, err)) = &self.reveal_error {
                        ui.add_space(theme::SPACE_SM);
                        theme::reason(ui, format!("{err}. The take is here:"));
                        let mut shown = path.display().to_string();
                        ui.add(
                            egui::TextEdit::singleline(&mut shown)
                                .desired_width(ui.available_width())
                                .font(egui::TextStyle::Monospace),
                        );
                    }
                },
            );
        });
        if refresh {
            self.reload();
        }
        if let Some(path) = reveal {
            self.reveal(&path);
        }
        if let Some((session_id, base, half)) = download {
            self.begin_download(&session_id, &base, half);
        }
    }
}

/// What a click on a card asked for.
enum CardAction {
    Reveal(PathBuf),
    Download(String, Half),
}

fn session_card(
    ui: &mut Ui,
    row: &SessionTakes,
    fetching: Option<&Fetching>,
) -> Option<CardAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        ui.label(row.header());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if row.spent_microusd > 0 {
                ui.label(theme::mono_muted(
                    ui,
                    // Named, because it is the only past figure on a screen
                    // whose other prices are all prospective, and an unlabelled
                    // one a few pixels above an egress price reads as part of
                    // it.
                    format!("machine {}", theme::microusd(row.spent_microusd)),
                ));
            }
        });
    });
    ui.horizontal(|ui| {
        ui.label(theme::mono_muted(ui, row.short_id.clone()));
        if let Some(record) = row.record() {
            ui.label(theme::muted(ui, record.bucket.clone()));
        }
        if let Some(days) = row.expires_in_days {
            let p = theme::palette_of(ui);
            let text = if days <= 0 {
                "expires today".to_owned()
            } else if days == 1 {
                "expires in 1 day".to_owned()
            } else {
                format!("expires in {days} days")
            };
            let color = if days <= EXPIRY_SOON_DAYS {
                theme::danger_ink(p)
            } else {
                p.text_muted
            };
            ui.label(RichText::new(text).color(color));
        }
    });
    if let Some(note) = &row.unenforced {
        ui.add(egui::Label::new(theme::muted(ui, note.clone()).small()).wrap());
    }
    if let Some(err) = &row.error {
        // Capped: what a row shows never sizes the screen, whatever a
        // provider answered with.
        theme::reason_capped(ui, (&row.session_id, "listing"), err.clone());
    }
    if row.listing {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().color(theme::palette_of(ui).text_muted));
            ui.label(theme::muted(ui, "asking the bucket"));
        });
    }
    if row.takes.is_empty() && !row.listing && row.error.is_none() {
        let word = if row.running {
            "This session is still running. A take shows up here once its upload finishes."
        } else {
            "No takes in this session."
        };
        ui.label(theme::muted(ui, word));
    }
    for take in &row.takes {
        ui.add_space(theme::SPACE_SM);
        ui.label(theme::mono_muted(ui, take.at()));
        for (half, part) in [(Half::Mix, &take.mix), (Half::Stems, &take.stems)] {
            if part.files.is_empty() {
                continue;
            }
            if let Some(clicked) = part_row(ui, row, take, half, part, fetching) {
                action = Some(clicked);
            }
        }
    }
    action
}

/// How far a download has got: an engraved track with the accent filling it,
/// drawn rather than assembled out of default widget chrome, like every other
/// moving thing in the app.
fn track(ui: &mut Ui, done: u64, total: u64) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(TRACK_W, TRACK_H), egui::Sense::hover());
    let p = theme::palette_of(ui);
    let radius = egui::CornerRadius::same(2);
    ui.painter().rect_filled(rect, radius, p.well);
    let fraction = (done as f32 / total.max(1) as f32).clamp(0.0, 1.0);
    if fraction > 0.0 {
        let mut filled = rect;
        filled.set_width(rect.width() * fraction);
        ui.painter().rect_filled(filled, radius, p.accent);
    }
}

/// One line: what this half of the take is, how big, where it is, and the one
/// thing to do about it.
fn part_row(
    ui: &mut Ui,
    row: &SessionTakes,
    take: &TakeRow,
    half: Half,
    part: &Part,
    fetching: Option<&Fetching>,
) -> Option<CardAction> {
    let mut action = None;
    let running = fetching
        .filter(|f| f.session_id == row.session_id && f.base == take.base && f.half == half);
    ui.horizontal(|ui| {
        let count = part.files.len();
        let what = match half {
            Half::Mix => "mix".to_owned(),
            Half::Stems if count == 1 => "1 stem".to_owned(),
            Half::Stems => format!("{count} stems"),
        };
        ui.label(what);
        ui.label(theme::mono_muted(ui, recordings::human_size(part.bytes())));
        if let Some(fetching) = running {
            let done = fetching.meter.done.load(Ordering::Relaxed);
            // In the button's place, so the row does not move when the download
            // starts: what was the one thing to press is now the one thing to
            // watch.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                track(ui, done, fetching.total);
                ui.label(theme::mono_muted(
                    ui,
                    format!(
                        "{} of {}",
                        recordings::human_size(done),
                        recordings::human_size(fetching.total)
                    ),
                ));
            });
            return;
        }
        if part.writing() {
            ui.label(theme::muted(ui, "still being written"));
            return;
        }
        match (part.here(), row.record()) {
            (Some(path), _) => {
                let path = path.to_owned();
                let where_it_is = match &row.place {
                    Place::Disk(_) => "on this computer".to_owned(),
                    Place::Bucket(_) => format!("saved to {}", row.dir.display()),
                };
                // Truncated, not wrapped: a long folder must not push the one
                // button on the row off the edge of the card.
                ui.add(egui::Label::new(theme::muted(ui, where_it_is)).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(reveal::LABEL).clicked() {
                        action = Some(CardAction::Reveal(path));
                    }
                });
            }
            (None, Some(record)) => {
                let bytes = part.missing_bytes();
                let price = recordings::quote_for(record, bytes)
                    .map(|quote| theme::microusd(quote.microusd));
                let label = match &price {
                    Ok(price) => format!(
                        "Download {} · {} · about {price}",
                        half.label(),
                        recordings::human_size(bytes)
                    ),
                    Err(_) => format!("Download {}", half.label()),
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let enabled = fetching.is_none();
                    if ui
                        .add_enabled(enabled, egui::Button::new(label))
                        .on_hover_text(
                            "Egress is billed on the download by your own cloud account, \
                             not on the recording.",
                        )
                        .clicked()
                    {
                        action = Some(CardAction::Download(take.base.clone(), half));
                    }
                });
            }
            // A local take with no file: the row would not exist.
            (None, None) => {}
        }
    });
    action
}

/// Where the last download landed, or why it did not.
fn landing(ui: &mut Ui, landed: &Landed) {
    ui.add_space(theme::SPACE_SM);
    match &landed.result {
        Ok(dir) => {
            let p = theme::palette_of(ui);
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "The {} landed in {}.",
                        landed.half.label(),
                        dir.display()
                    ))
                    .color(p.meter_green),
                )
                .wrap(),
            );
        }
        Err(err) => {
            theme::reason_capped(ui, (&landed.session_id, "landed"), err.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::MemStore;
    use jamstream_cli::state::RetentionApplied;

    fn session(id: &str, provider: &str, created: u64, ended: Option<u64>) -> SessionState {
        SessionState {
            session_id_hex: id.to_owned(),
            provider: provider.to_owned(),
            region: "sfo3".to_owned(),
            instance_id: "i-1".to_owned(),
            address: "203.0.113.7:43210".to_owned(),
            created_unix: created,
            hourly_microusd: 16_800,
            issuer_private_key_b64: "aXNzdWVy".to_owned(),
            server_public_key_b64: "c2VydmVy".to_owned(),
            invites: Vec::new(),
            status: if ended.is_some() {
                SessionStatus::Ended
            } else {
                SessionStatus::Running
            },
            ended_unix: ended,
        }
    }

    fn record(applied: Option<RetentionApplied>) -> RecordingRecord {
        RecordingRecord {
            provider: "digitalocean".to_owned(),
            bucket: "our-takes".to_owned(),
            region: "sfo3".to_owned(),
            retention: "30d".to_owned(),
            stems: true,
            applied,
        }
    }

    fn object(name: &str, size: u64) -> Take {
        Take {
            key: format!("jamstream/recordings/s1/{name}"),
            name: name.to_owned(),
            size,
            last_modified: None,
        }
    }

    /// A take is one Record to Stop, so the mix and every stem of it are one
    /// row: a musician picking a take picks a moment, not a file name.
    #[test]
    fn a_take_is_the_mix_and_its_stems_together() {
        let dir = std::env::temp_dir().join("jamstream-takes-group");
        let _ = std::fs::remove_dir_all(&dir);
        let objects = [
            object("jamstream-2026-07-28-1930-mix.flac", 1_100_000_000),
            object("jamstream-2026-07-28-1930-Ana.flac", 1_100_000_000),
            object("jamstream-2026-07-28-1930-Ben.flac", 1_100_000_000),
            object("jamstream-2026-07-28-2015-mix.flac", 400_000_000),
        ];
        let takes = takes_from_objects(&objects, &dir).expect("group");
        assert_eq!(takes.len(), 2, "two Record to Stops, two rows");
        // Newest first: the take somebody wants is nearly always the last one.
        assert_eq!(takes[0].base, "jamstream-2026-07-28-2015");
        assert_eq!(takes[0].at(), "20:15 UTC");
        assert_eq!(takes[0].mix.files.len(), 1);
        assert!(takes[0].stems.files.is_empty());
        assert_eq!(takes[1].mix.bytes(), 1_100_000_000);
        assert_eq!(takes[1].stems.files.len(), 2, "one file per musician");
        assert_eq!(takes[1].stems.bytes(), 2_200_000_000);
        // Nothing is on this computer, so every byte would have to be paid for.
        assert_eq!(takes[1].stems.missing_bytes(), 2_200_000_000);
        assert_eq!(takes[1].stems.here(), None);
    }

    /// A take already on disk is a Reveal, not a Download: it costs nothing to
    /// show a file that is here, and egress to fetch one that is not.
    #[test]
    fn a_part_already_on_disk_offers_no_download() {
        let dir = std::env::temp_dir().join(format!("jamstream-takes-here-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create");
        let mix = object("jamstream-2026-07-28-1930-mix.flac", 4);
        let stem = object("jamstream-2026-07-28-1930-Ana.flac", 9);
        std::fs::write(dir.join(&mix.name), b"abcd").expect("write");
        let takes = takes_from_objects(&[mix, stem], &dir).expect("group");
        assert_eq!(takes.len(), 1);
        assert_eq!(takes[0].mix.missing_bytes(), 0);
        assert!(takes[0].mix.here().is_some(), "the mix is here");
        // The stems half is untouched by the mix being here.
        assert_eq!(takes[0].stems.missing_bytes(), 9);
        assert!(takes[0].stems.here().is_none());
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    /// A key that would climb out of the folder is refused before it is drawn,
    /// let alone downloaded. The guard is the CLI's; this proves the screen
    /// goes through it rather than joining paths itself.
    #[test]
    fn a_take_whose_key_escapes_the_folder_is_refused() {
        let dir = std::env::temp_dir().join("jamstream-takes-escape");
        let err = takes_from_objects(&[object("../../etc/passwd", 1)], &dir)
            .expect_err("a traversal must not become a row")
            .to_string();
        assert!(err.contains("has to land inside"), "{err}");
    }

    /// The countdown is drawn only for a rule a provider is really enforcing.
    /// Everything else, including a session from before the answer was written
    /// down, gets no clock: one for a deletion nothing will perform is worse
    /// than none.
    #[test]
    fn a_countdown_needs_a_rule_that_exists() {
        let created = 1_784_000_000;
        let now = created + 18 * 86_400;
        let (days, note) = expiry(
            Some(&record(Some(RetentionApplied::ServerSide))),
            created,
            now,
        );
        assert_eq!(days, Some(12));
        assert_eq!(note, None);

        let unenforced = RetentionApplied::Unenforced {
            note: "this bucket has no lifecycle API".to_owned(),
        };
        let (days, note) = expiry(Some(&record(Some(unenforced))), created, now);
        assert_eq!(days, None, "nothing is deleting these takes");
        assert!(note.expect("the bucket's reason").contains("lifecycle"));

        // A record written before the answer was kept.
        assert_eq!(expiry(Some(&record(None)), created, now), (None, None));
        // Kept forever expires never.
        let forever = RecordingRecord {
            retention: "forever".to_owned(),
            ..record(Some(RetentionApplied::ServerSide))
        };
        assert_eq!(expiry(Some(&forever), created, now), (None, None));
        // A local session has no bucket and no rule.
        assert_eq!(expiry(None, created, now), (None, None));
    }

    /// The rows are built from what this machine knows: a bucket sidecar, or a
    /// take file written while the session was running. A session that recorded
    /// nowhere is not a row at all.
    #[test]
    fn rows_come_from_the_sidecar_or_the_disk_and_nowhere_else() {
        let start = 1_784_000_000;
        let dir = PathBuf::from("/takes");
        let sessions = vec![
            (
                session("aaaa1111", "digitalocean", start, Some(start + 2_820)),
                Some(record(Some(RetentionApplied::ServerSide))),
            ),
            (
                session("bbbb2222", "local", start + 10_000, Some(start + 17_800)),
                None,
            ),
            (
                session("cccc3333", "aws", start + 30_000, Some(start + 31_000)),
                None,
            ),
        ];
        let local = vec![
            LocalTake {
                path: dir.join("jamstream-2026-07-25-1200-mix.flac"),
                bytes: 2_400_000_000,
                modified_unix: start + 12_000,
            },
            // Written long after every session: not one of theirs.
            LocalTake {
                path: dir.join("jamstream-2026-08-01-0900-mix.flac"),
                bytes: 100,
                modified_unix: start + 900_000,
            },
        ];
        let rows = rows_from(&sessions, &local, &dir, Path::new("/music"), start + 40_000);
        assert_eq!(rows.len(), 2, "the session that recorded nowhere is absent");
        // Newest first.
        assert_eq!(rows[0].short_id, "bbbb2222");
        assert_eq!(rows[0].place, Place::Disk(dir.clone()));
        assert_eq!(rows[0].takes.len(), 1);
        assert_eq!(rows[0].takes[0].mix.bytes(), 2_400_000_000);
        assert!(
            rows[0].takes[0].mix.here().is_some(),
            "a local take is already on this computer"
        );
        let cloud = &rows[1];
        assert_eq!(cloud.short_id, "aaaa1111");
        assert!(cloud.listing, "a bucket has to be asked");
        assert!(cloud.takes.is_empty());
        // 30 days of retention, most of a day into it.
        assert_eq!(cloud.expires_in_days, Some(29));
        assert_eq!(cloud.secs, 2_820);
        // 47 minutes of a $0.0168/h droplet.
        assert_eq!(cloud.spent_microusd, 13_160);
        assert_eq!(cloud.header(), "Tue 14 Jul, 47 min, digitalocean sfo3");
        assert!(cloud.dir.ends_with("aaaa1111"));
    }

    /// A local take still being written must not read as a finished recording.
    #[test]
    fn a_part_file_says_it_is_still_being_written() {
        let dir = PathBuf::from("/takes");
        let sessions = vec![(session("dddd4444", "local", 100, None), None)];
        let local = vec![LocalTake {
            path: dir.join("jamstream-2026-07-28-1930-mix.flac.part"),
            bytes: 512,
            modified_unix: 200,
        }];
        let rows = rows_from(&sessions, &local, &dir, Path::new("/music"), 300);
        let take = &rows[0].takes[0];
        assert!(take.mix.writing(), "a .part file is not a take yet");
        assert_eq!(take.mix.files[0].name, "jamstream-2026-07-28-1930-mix.flac");
    }

    /// The whole point of the screen: the key comes from this computer's
    /// keychain, and the refusal when there is none points at the tab that
    /// takes one rather than at a shell variable.
    #[test]
    fn the_bucket_is_opened_with_the_key_in_the_keychain() {
        let store = Arc::new(MemStore::default());
        let env: EnvReader = Arc::new(|_| None);
        let stores = KeychainStores::new(store.clone(), env);
        let record = record(None);
        let err = match stores.open(&record) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("a bucket must not open without a key"),
        };
        assert!(err.contains("Recording tab"), "{err}");

        creds::save_storage_credential(
            &*store,
            jamstream_cloud::ProviderKind::DigitalOcean,
            "DO00ID",
            "0000-fake-storage-secret",
        )
        .expect("save");
        let Ok(opened) = stores.open(&record) else {
            panic!("the saved pair has to open the bucket");
        };
        assert_eq!(opened.kind(), jamstream_cloud::ProviderKind::DigitalOcean);
    }

    /// A day and a length, in the words the design asks for.
    #[test]
    fn days_and_lengths_read_the_way_a_musician_says_them() {
        // 2026-07-28 19:30 UTC, the stamp the recorder's own tests use.
        assert_eq!(day_label(1_785_267_000), "Tue 28 Jul");
        assert_eq!(day_label(1_784_000_000), "Tue 14 Jul");
        assert_eq!(day_label(0), "Thu 1 Jan");
        assert_eq!(length(2_820), "47 min");
        assert_eq!(length(7_800), "2 h 10 min");
        assert_eq!(length(30), "0 min");
    }

    /// The base name is what the mix and its stems share, and it survives a
    /// name with no dash in it at all.
    #[test]
    fn the_base_name_is_what_one_take_shares() {
        assert_eq!(
            base_of("jamstream-2026-07-28-1930-mix.flac"),
            "jamstream-2026-07-28-1930"
        );
        assert_eq!(
            base_of("jamstream-2026-07-28-1930-Ana.flac"),
            "jamstream-2026-07-28-1930"
        );
        assert_eq!(base_of("mix.flac"), "mix.flac");
        assert_eq!(file_name("stems/bass.flac"), "bass.flac");
    }

    /// Progress is read by the frame loop, so it moves in steps a bar can see
    /// rather than once per chunk, and it never goes backwards between takes.
    #[test]
    fn progress_accumulates_across_the_takes_of_one_download() {
        let meter = Arc::new(Meter::default());
        let mut progress = MeterProgress {
            meter: Arc::clone(&meter),
            base: 0,
        };
        progress.started("mix.flac", 8_000_000).expect("start");
        progress
            .advanced("mix.flac", 1_000, 8_000_000)
            .expect("chunk");
        assert_eq!(
            meter.done.load(Ordering::Relaxed),
            0,
            "a kilobyte of a gigabyte must not wake the paint thread"
        );
        progress
            .advanced("mix.flac", 8_000_000, 8_000_000)
            .expect("chunk");
        assert_eq!(meter.done.load(Ordering::Relaxed), 8_000_000);
        progress.finished("mix.flac", 8_000_000).expect("finish");
        progress.started("Ana.flac", 8_000_000).expect("start");
        assert_eq!(*meter.name.lock().expect("name"), "Ana.flac");
        progress
            .advanced("Ana.flac", 8_000_000, 8_000_000)
            .expect("chunk");
        assert_eq!(
            meter.done.load(Ordering::Relaxed),
            16_000_000,
            "the second take continues the total rather than restarting it"
        );
    }

    /// The screen's binding of the shared mapping: a real 403 reads as the
    /// listing remedy, and none of the four identifiers the document carries
    /// reaches the row. What the mapping does with every other class of
    /// failure is pinned in `jamstream_cli::reason`, where it lives.
    #[test]
    fn a_real_denial_reads_as_the_listing_remedy() {
        let denied = CliError::Provider(jamstream_cloud::ProviderError::Auth(
            "http 403 Forbidden: <?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>AccessDenied</Code><Message>User: \
             arn:aws:iam::887762372032:user/jamstream-recordings is not \
             authorized to perform: s3:ListBucket</Message>\
             <RequestId>Q0YMR4GFKCH1Y688</RequestId>\
             <HostId>EE3WMENDEauoc0QS4v1XCZK1RcDA4A</HostId></Error>"
                .to_owned(),
        ));
        let shown = error_sentence("listing a3f29c41", "aws", &denied);
        assert_eq!(
            shown,
            "The storage key cannot list this bucket. Add s3:ListBucket and s3:GetObject \
             for the bucket to the key's policy, then refresh."
        );
        for identifier in ["887762372032", "arn:", "Q0YMR4GFKCH1Y688", "EE3WMEND", "<"] {
            assert!(!shown.contains(identifier), "{identifier} leaked: {shown}");
        }
    }

    /// The folder is ours by name wherever it lands, and a machine that can
    /// name any directory at all gets an absolute path: a relative one
    /// resolves against whatever the process started in, which for a Start
    /// menu launch on Windows is System32, and both the reveal button and
    /// the "landed in" line would point there.
    #[test]
    fn the_downloads_dir_is_absolute_whenever_any_base_exists() {
        let base = std::env::temp_dir();
        assert_eq!(
            resolve_downloads_dir(Some(base.clone())),
            base.join("JamStream")
        );
        assert_eq!(resolve_downloads_dir(None), PathBuf::from("JamStream"));
        // The live chain ends at current_dir, so on any machine where this
        // test can run the real answer is absolute.
        let dir = downloads_dir();
        assert!(dir.is_absolute(), "{}", dir.display());
        assert!(dir.ends_with("JamStream"), "{}", dir.display());
    }
}
