//! `jamstream recordings`: the takes a session left in a bucket, and getting
//! them onto this machine.
//!
//! # The one cost that arrives late
//!
//! A cloud session prices itself before it launches, and recording into a
//! bucket beside the VM is free. Egress is charged on the way out: pulling
//! 5.5 GB of stems off S3 is about 49 cents, and it happens hours after the
//! session was paid for. So [`get`] prices the transfer from the sizes the
//! bucket reports and asks before spending it, the same shape
//! `jamstream host` uses for a launch, with `--yes` for a script.
//!
//! # Nothing may be silently short
//!
//! Every take is streamed to disk a chunk at a time, never buffered: a mix is
//! gigabytes. What lands is then checked against the size the bucket listed,
//! and a file that came up short is deleted rather than left looking like a
//! recording.
//!
//! # A take outlives the objects it came from
//!
//! A retention rule deletes the objects and leaves behind whatever a download
//! already put on this computer, so a listing is not the only witness to a
//! take. [`list`] and [`get`] both read the session's own download folder
//! ([`crate::downloads`]) before they say a session has nothing, because from
//! that moment the files in it are the only copy.
//!
//! # One downloader, two surfaces
//!
//! The desktop app's Takes screen fetches takes too, and a second
//! implementation of this would be a second place to get the traversal guard,
//! the size check, and the egress quote right. So the pieces are public:
//! [`recorded_sessions`] and [`takes_for`] to list, [`plan_downloads`] to
//! decide what is worth paying for, [`quote_for`] to price it, and
//! [`fetch_takes`] to move the bytes. Progress is the only thing the two
//! surfaces disagree about, and that is a [`TakeProgress`] the caller brings:
//! percentages on a terminal here, a bar in the app.

use std::io::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use jamstream_cloud::{
    ChunkSink, EgressQuote, ObjectMeta, ProviderError, RegionId, format_microusd, session_prefix,
};

use crate::CliError;
use crate::cli::{RecordingsGetArgs, RecordingsListArgs};
use crate::downloads::{self, LocalTake};
use crate::state::{self, RecordingRecord, SessionState};
use crate::storage::{Stores, provider_kind, retention_label};

/// Percentage step progress is reported at.
const PROGRESS_STEP: u64 = 10;

/// How [`get`] talks to whoever ran it.
pub struct Prompt<'a, W> {
    /// True when stdout is a terminal, the only place a progress line redrawn
    /// in place reads better than one line per step.
    pub terminal: bool,
    /// Answers the egress question. A function rather than a read of stdin so
    /// a test can decline without a terminal.
    pub confirm: &'a mut dyn FnMut(&mut W) -> Result<bool, CliError>,
}

impl<'a, W: Write> Prompt<'a, W> {
    /// The interactive prompt: the same y/N question `jamstream host` asks
    /// before it launches.
    pub fn stdin(confirm: &'a mut dyn FnMut(&mut W) -> Result<bool, CliError>) -> Self {
        Prompt {
            terminal: std::io::IsTerminal::is_terminal(&std::io::stdout()),
            confirm,
        }
    }
}

/// Puts the question and reads the answer. Anything but yes is no, including
/// end of input.
pub fn ask<W: Write>(out: &mut W) -> Result<bool, CliError> {
    write!(out, "Download these takes? [y/N] ")?;
    out.flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// One object under a session's prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Take {
    pub key: String,
    /// The key with the session prefix removed: `mix.flac`,
    /// `stems/bass.flac`.
    pub name: String,
    pub size: u64,
    pub last_modified: Option<String>,
}

impl Take {
    /// True for the broadcast mix, which every recorded take has exactly one
    /// of. A stem is anything else, and the two are priced apart because
    /// stems are about five times the bytes.
    pub fn is_mix(&self) -> bool {
        self.name.ends_with("-mix.flac")
    }

    /// The size in the units a bucket bills and reports.
    pub fn size_display(&self) -> String {
        human_size(self.size)
    }
}

/// Every session this machine knows that recorded to a bucket, oldest first.
pub fn recorded_sessions() -> Result<Vec<(SessionState, RecordingRecord)>, CliError> {
    let mut out = Vec::new();
    for (_, session) in state::list()? {
        if let Some(record) = state::load_recording(&session.session_id_hex)? {
            out.push((session, record));
        }
    }
    Ok(out)
}

/// Takes under one session's prefix, smallest key first, with the prefix
/// stripped for display.
pub async fn takes_for(
    session: &SessionState,
    record: &RecordingRecord,
    stores: &dyn Stores,
) -> Result<Vec<Take>, CliError> {
    let store = stores.open(record)?;
    let prefix = session_prefix(&session.session_id_hex);
    let listed = store.list(&record.bucket, &prefix).await?;
    Ok(listed
        .into_iter()
        .map(|meta| take_from(&prefix, meta))
        .collect())
}

fn take_from(prefix: &str, meta: ObjectMeta) -> Take {
    Take {
        name: meta
            .key
            .strip_prefix(prefix)
            .unwrap_or(&meta.key)
            .to_owned(),
        key: meta.key,
        size: meta.size,
        last_modified: meta.last_modified,
    }
}

/// One session's place in the listing: what its bucket answered, and what its
/// download folder on this computer already holds.
struct Row {
    session: SessionState,
    record: RecordingRecord,
    takes: Result<Vec<Take>, CliError>,
    /// Where a download of this session's takes lands, and the ones already
    /// there.
    dir: PathBuf,
    here: Vec<LocalTake>,
}

/// Lists the takes of every session this machine recorded to a bucket, and the
/// ones already on this computer.
///
/// `downloads` is the folder the app downloads into, which is where a take a
/// retention rule has deleted from its bucket still is: see
/// [`crate::downloads`].
///
/// One unreachable bucket does not hide the rest: the reason lands on that
/// session's line, the other sessions still list, and the command still exits
/// nonzero so a script cannot mistake a failure for an empty bucket.
pub async fn list<W: Write>(
    args: &RecordingsListArgs,
    stores: &dyn Stores,
    downloads: &Path,
    out: &mut W,
) -> Result<(), CliError> {
    let mut rows: Vec<Row> = Vec::new();
    for (session, record) in recorded_sessions()? {
        let takes = takes_for(&session, &record, stores).await;
        let dir = downloads::session_dir(downloads, &session.session_id_hex);
        rows.push(Row {
            here: downloads::takes_in(&dir),
            session,
            record,
            takes,
            dir,
        });
    }

    if args.json {
        let value: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                let (session, record, takes) = (&row.session, &row.record, &row.takes);
                serde_json::json!({
                    "session_id": session.session_id_hex,
                    "provider": record.provider,
                    "region": record.region,
                    "bucket": record.bucket,
                    "retention": record.retention,
                    "stems": record.stems,
                    "error": takes.as_ref().err().map(ToString::to_string),
                    "total_bytes": takes.as_ref().map(|t| total_bytes(t)).unwrap_or(0),
                    "takes": takes
                        .as_ref()
                        .map(|takes| takes
                            .iter()
                            .map(|take| serde_json::json!({
                                "name": take.name,
                                "key": take.key,
                                "bytes": take.size,
                                "size": human_size(take.size),
                                "last_modified": take.last_modified,
                            }))
                            .collect::<Vec<_>>())
                        .unwrap_or_default(),
                    // Names and nowhere else a size: a file read off this disk
                    // has nothing left to be measured against.
                    "on_disk": {
                        "dir": row.dir.display().to_string(),
                        "takes": row.here
                            .iter()
                            .map(|take| take.name().to_string())
                            .collect::<Vec<_>>(),
                    },
                })
            })
            .collect();
        writeln!(out, "{}", serde_json::to_string_pretty(&value)?)?;
        return first_error(rows);
    }

    if rows.is_empty() {
        writeln!(
            out,
            "No session on this machine recorded to a bucket. A local session's takes are \
             already on this computer, in the directory jamstream host printed."
        )?;
        return Ok(());
    }

    let any_takes = rows
        .iter()
        .any(|row| row.takes.as_ref().is_ok_and(|takes| !takes.is_empty()));
    if any_takes {
        writeln!(
            out,
            "{:<10} {:<40} {:>10}  MODIFIED",
            "SESSION", "TAKE", "SIZE"
        )?;
    }
    for row in &rows {
        let short = short_id(&row.session);
        let bucket = format!(
            "{} ({}/{})",
            row.record.bucket, row.record.provider, row.record.region
        );
        match &row.takes {
            // An empty table row would read as a take of unknown size.
            Ok(takes) if takes.is_empty() => {
                writeln!(out, "{short} has no takes in {bucket}.")?;
            }
            Ok(takes) => {
                for take in takes {
                    writeln!(
                        out,
                        "{:<10} {:<40} {:>10}  {}",
                        short,
                        take.name,
                        human_size(take.size),
                        short_time(take.last_modified.as_deref()),
                    )?;
                }
            }
            Err(err) => writeln!(out, "{short} could not be listed in {bucket}: {err}")?,
        }
        // Whatever the bucket said, including nothing and including a refusal.
        // No size beside it: these are found, not measured.
        if !row.here.is_empty() {
            writeln!(
                out,
                "{short} has {} on this computer, found in {}.",
                plural(row.here.len(), "take"),
                row.dir.display()
            )?;
        }
    }
    if any_takes {
        writeln!(out)?;
        writeln!(
            out,
            "Fetch a session's takes with: jamstream recordings get <session>"
        )?;
    }
    first_error(rows)
}

/// The first bucket that could not be listed, so an exit code follows the
/// lines already printed.
fn first_error(rows: Vec<Row>) -> Result<(), CliError> {
    match rows.into_iter().find_map(|row| row.takes.err()) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Downloads one session's takes into `--out` or the current directory.
///
/// `downloads` is the folder the app downloads into, read only to answer a
/// session whose objects are gone: the takes still land where `--out` says.
pub async fn get<W: Write + Send>(
    args: &RecordingsGetArgs,
    stores: &dyn Stores,
    downloads: &Path,
    prompt: &mut Prompt<'_, W>,
    out: &mut W,
) -> Result<(), CliError> {
    let (session, record) = select(&args.session)?;
    let takes = takes_for(&session, &record, stores).await?;
    if takes.is_empty() {
        return Err(nothing_to_fetch(&session, &record, downloads));
    }

    let dir = args.out.clone().unwrap_or_else(|| PathBuf::from("."));
    // Planned before anything is priced: a take already on disk costs no
    // egress, and a name that would be clobbered stops the whole download
    // rather than half of it.
    let plan = plan_downloads(&takes, &dir)?;
    let wanted: Vec<&Take> = plan
        .iter()
        .zip(&takes)
        .filter(|(action, _)| **action == Action::Fetch)
        .map(|(_, take)| take)
        .collect();

    for (action, take) in plan.iter().zip(&takes) {
        if *action == Action::Have {
            writeln!(out, "{} is already here, skipping.", take.name)?;
        }
    }
    if wanted.is_empty() {
        writeln!(out, "Every take is already in {}.", dir.display())?;
        return Ok(());
    }

    let bytes: u64 = wanted.iter().map(|take| take.size).sum();
    let quote = quote_for(&record, bytes)?;
    writeln!(
        out,
        "Session {} recorded {} in {} ({}/{}), {}.",
        short_id(&session),
        plural(wanted.len(), "take"),
        record.bucket,
        record.provider,
        record.region,
        retention_label(&record).to_lowercase()
    )?;
    writeln!(out, "{}", quote.display_row())?;
    // The point of the prompt: this charge is not in the session's own price.
    writeln!(
        out,
        "Egress is billed on the download, not on the recording."
    )?;
    for note in quote.notes() {
        writeln!(out, "{note}")?;
    }
    if !args.yes && !(prompt.confirm)(out)? {
        writeln!(out, "Aborted. Nothing was downloaded.")?;
        return Ok(());
    }

    let store = stores.open(&record)?;
    let fetched = {
        let mut progress = Percentages::new(prompt.terminal, &mut *out);
        fetch_takes(store.as_ref(), &record.bucket, &wanted, &dir, &mut progress).await?
    };
    writeln!(
        out,
        "{} in {}, {}.",
        plural(wanted.len(), "take"),
        dir.display(),
        human_size(fetched)
    )?;
    writeln!(
        out,
        "Egress for this download: {}.",
        format_microusd(quote.microusd)
    )?;
    Ok(())
}

/// What a download costs to pull out of one session's bucket.
///
/// The provider and region come from the session's own record, so the quote
/// prices the bucket the takes are actually in.
pub fn quote_for(record: &RecordingRecord, bytes: u64) -> Result<EgressQuote, CliError> {
    Ok(EgressQuote::compute(
        provider_kind(&record.provider)?,
        &RegionId::new(record.region.clone()),
        bytes,
    )?)
}

/// What holding `bytes` in one session's bucket costs a month.
///
/// The charge nobody agreed to: a session prices its machine and its download,
/// and storage keeps billing after both. A surface saying so needs the figure
/// for the bucket the takes are really in, which is why the provider and the
/// region come off the session's own record.
pub fn monthly_storage_for(record: &RecordingRecord, bytes: u64) -> Result<u64, CliError> {
    let price = jamstream_cloud::storage_price(
        provider_kind(&record.provider)?,
        &RegionId::new(record.region.clone()),
    )?;
    Ok(price.monthly_microusd(bytes))
}

/// Streams `takes` into `dir`, one at a time, and returns the bytes that
/// landed.
///
/// The whole download path both surfaces share: the traversal guard on every
/// name, the parent directories, the size check, and the removal of anything
/// that came up short.
pub async fn fetch_takes(
    store: &dyn jamstream_cloud::ObjectStore,
    bucket: &str,
    takes: &[&Take],
    dir: &Path,
    progress: &mut dyn TakeProgress,
) -> Result<u64, CliError> {
    std::fs::create_dir_all(dir)?;
    let mut fetched = 0u64;
    for take in takes {
        let path = destination(dir, take)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        fetched += fetch_one(store, bucket, take, &path, progress).await?;
    }
    Ok(fetched)
}

/// Streams one take to `path` and proves what landed is the whole object.
pub async fn fetch_one(
    store: &dyn jamstream_cloud::ObjectStore,
    bucket: &str,
    take: &Take,
    path: &Path,
    progress: &mut dyn TakeProgress,
) -> Result<u64, CliError> {
    let file = std::fs::File::create(path)?;
    progress.started(&take.name, take.size)?;
    let mut sink = TakeSink {
        file: std::io::BufWriter::new(file),
        path: path.to_owned(),
        written: 0,
        expected: take.size,
        name: take.name.clone(),
        progress,
    };
    let outcome = store.get(bucket, &take.key, &mut sink).await;
    let written = sink.finish()?;
    match outcome {
        // A take that came up short must not be left looking like a
        // recording, so the partial file goes.
        Ok(_) if written != take.size => {
            let _ = std::fs::remove_file(path);
            Err(CliError::Failed(format!(
                "{} is truncated: the bucket lists {} bytes and {} arrived, so the partial file \
                 was removed",
                take.name, take.size, written
            )))
        }
        Ok(_) => Ok(written),
        Err(err) => {
            let _ = std::fs::remove_file(path);
            Err(err.into())
        }
    }
}

/// What to do about one take that may already be on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Fetch,
    /// Already here at exactly the size the bucket lists.
    Have,
}

/// Where one take is written, refusing a name that would land outside `dir`
/// or that some filesystem would misread.
///
/// The server writes takes under a sanitized prefix, but the bucket belongs to
/// the host and an object store key is an arbitrary string: `..` in one is a
/// valid key and would be a write into somebody's home directory.
///
/// The Win32 name rules are checked on every platform, not just Windows: a
/// bucket written by a Windows host is read on a mac and the other way
/// around, so the namespace rules travel with the data. Without this,
/// `NUL.flac` opens the device and reports success with no file, a colon
/// writes an alternate data stream, and a trailing dot collapses onto
/// another take's name.
pub fn destination(dir: &Path, take: &Take) -> Result<PathBuf, CliError> {
    let relative = Path::new(&take.name);
    let contained = !take.name.is_empty()
        && relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)));
    if !contained {
        return Err(CliError::Failed(format!(
            "refusing to download {:?}: a take has to land inside {}, and that key would not",
            take.key,
            dir.display()
        )));
    }
    // Split on the key's own separator rather than the platform's, so a
    // backslash inside a component is judged, not swallowed as a separator.
    for part in take.name.split('/') {
        if let Some(hazard) = jamstream_cloud::windows_hazard(part) {
            return Err(CliError::Failed(format!(
                "refusing to download {:?}: {part:?} {hazard}, and a take's name has to work on \
                 every platform",
                take.key
            )));
        }
    }
    // One component at a time: take names use forward slashes, and joining
    // the name whole on Windows keeps them verbatim inside a
    // backslash-separated path.
    Ok(relative
        .components()
        .fold(dir.to_owned(), |path, part| path.join(part)))
}

/// Decides each take's fate before any egress is spent. A local file of a
/// different size is a conflict rather than something to overwrite, because
/// it may be the only copy of an edit.
pub fn plan_downloads(takes: &[Take], dir: &Path) -> Result<Vec<Action>, CliError> {
    let mut plan = Vec::with_capacity(takes.len());
    for take in takes {
        let path = destination(dir, take)?;
        plan.push(match std::fs::metadata(&path) {
            Ok(meta) if meta.len() == take.size => Action::Have,
            Ok(meta) => {
                return Err(CliError::Usage(format!(
                    "{} already exists at {} bytes and the bucket lists {}; move it aside or \
                     pass --out to another directory",
                    path.display(),
                    meta.len(),
                    take.size
                )));
            }
            Err(_) => Action::Fetch,
        });
    }
    Ok(plan)
}

/// Picks a recorded session from an id prefix, the way `jamstream end` does.
fn select(prefix: &str) -> Result<(SessionState, RecordingRecord), CliError> {
    let mut matches: Vec<(SessionState, RecordingRecord)> = recorded_sessions()?
        .into_iter()
        .filter(|(session, _)| session.session_id_hex.starts_with(prefix))
        .collect();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(no_match(prefix)),
        n => Err(CliError::Usage(format!(
            "{n} recorded sessions match {prefix:?}; use more of the id"
        ))),
    }
}

/// A session that exists but recorded nowhere is a different mistake from a
/// session id nobody recognises, and a local session's takes are already on
/// this machine.
fn no_match(prefix: &str) -> CliError {
    let known = state::list().unwrap_or_default();
    if let Some((_, session)) = known
        .iter()
        .find(|(_, session)| session.session_id_hex.starts_with(prefix))
    {
        let where_takes_are = match state::recordings_dir() {
            Ok(dir) => format!(
                " Takes from a local session are on this computer, under {}.",
                dir.display()
            ),
            Err(_) => String::new(),
        };
        return CliError::Usage(format!(
            "session {} recorded to no bucket.{where_takes_are}",
            &session.session_id_hex[..8.min(session.session_id_hex.len())]
        ));
    }
    CliError::Usage(format!(
        "no recorded session matches {prefix:?}; run jamstream recordings to list them"
    ))
}

/// There is nothing to download, which is not the same as there being nothing
/// left of the session: a retention rule deletes the objects and leaves the
/// files a download already put on this computer. Still a failure, because
/// nothing was fetched, but one that says where the copies are.
fn nothing_to_fetch(
    session: &SessionState,
    record: &RecordingRecord,
    downloads: &Path,
) -> CliError {
    let dir = downloads::session_dir(downloads, &session.session_id_hex);
    let here = downloads::takes_in(&dir);
    let already = if here.is_empty() {
        String::new()
    } else {
        format!(
            "; {} from it {} on this computer, found in {}",
            plural(here.len(), "take"),
            if here.len() == 1 { "is" } else { "are" },
            dir.display()
        )
    };
    CliError::Failed(format!(
        "session {} recorded to {} but the bucket holds no takes under {}{already}",
        short_id(session),
        record.bucket,
        session_prefix(&session.session_id_hex)
    ))
}

fn short_id(session: &SessionState) -> &str {
    &session.session_id_hex[..8.min(session.session_id_hex.len())]
}

fn total_bytes(takes: &[Take]) -> u64 {
    takes.iter().map(|take| take.size).sum()
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// Decimal units, because that is how a bucket bills and reports.
pub fn human_size(bytes: u64) -> String {
    let round = |num: u64, den: u64| (num + den / 2) / den;
    if bytes >= 1_000_000_000 {
        let centi = round(bytes, 10_000_000);
        format!("{}.{:02} GB", centi / 100, centi % 100)
    } else if bytes >= 1_000_000 {
        let deci = round(bytes, 100_000);
        format!("{}.{} MB", deci / 10, deci % 10)
    } else if bytes >= 1_000 {
        let deci = round(bytes, 100);
        format!("{}.{} KB", deci / 10, deci % 10)
    } else {
        format!("{bytes} B")
    }
}

/// Shortens a provider timestamp to the minute, and leaves anything it does
/// not recognise alone.
///
/// The store hands these back verbatim and opaque (S3 sends ISO 8601 in a
/// listing, GCS RFC 3339), so this trims a known shape for the column rather
/// than parsing a date nobody does arithmetic on.
fn short_time(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return "-".to_owned();
    };
    let bytes = raw.as_bytes();
    if bytes.len() >= 16 && bytes[10] == b'T' && bytes[..10].iter().all(|b| b.is_ascii_graphic()) {
        return format!("{} {}", &raw[..10], &raw[11..16]);
    }
    raw.to_owned()
}

/// Where a download says how far it has got.
///
/// A take is hundreds of megabytes, so something has to move while it lands.
/// What that something is differs by surface and nothing else does: this
/// command prints percentages, the app moves a bar, so the download engine
/// takes one of these rather than a writer.
pub trait TakeProgress: Send {
    /// A take is about to be written, of `expected` bytes. Called once, before
    /// any of it lands.
    fn started(&mut self, _take: &str, _expected: u64) -> Result<(), CliError> {
        Ok(())
    }

    /// `written` bytes of `take` are on disk, of `expected`. Called per chunk,
    /// so an implementation that draws has to rate limit itself.
    fn advanced(&mut self, take: &str, written: u64, expected: u64) -> Result<(), CliError>;

    /// The take is closed and flushed. Reached even when it came up short, so
    /// this must not claim the take arrived.
    fn finished(&mut self, _take: &str, _written: u64) -> Result<(), CliError> {
        Ok(())
    }
}

/// Progress as percentages, which is what a terminal and a log both read.
///
/// Percentages on their own lines rather than a redrawn bar, so a log or a
/// pipe reads the same as a terminal; a terminal gets the same percentages
/// redrawn in place.
pub struct Percentages<'a, W: Write> {
    terminal: bool,
    next_pct: u64,
    out: &'a mut W,
}

impl<'a, W: Write> Percentages<'a, W> {
    pub fn new(terminal: bool, out: &'a mut W) -> Percentages<'a, W> {
        Percentages {
            terminal,
            next_pct: 0,
            out,
        }
    }
}

impl<W: Write + Send> TakeProgress for Percentages<'_, W> {
    fn started(&mut self, _take: &str, _expected: u64) -> Result<(), CliError> {
        self.next_pct = 0;
        Ok(())
    }

    fn advanced(&mut self, take: &str, written: u64, expected: u64) -> Result<(), CliError> {
        let pct = written
            .saturating_mul(100)
            .checked_div(expected)
            .map_or(100, |pct| pct.min(100));
        if pct < self.next_pct {
            return Ok(());
        }
        self.next_pct = pct - pct % PROGRESS_STEP + PROGRESS_STEP;
        let line = format!("  {take:<40} {pct:>3}%");
        if self.terminal {
            write!(self.out, "\r{line}")?;
            self.out.flush()?;
        } else {
            writeln!(self.out, "{line}")?;
        }
        Ok(())
    }

    fn finished(&mut self, _take: &str, _written: u64) -> Result<(), CliError> {
        if self.terminal {
            writeln!(self.out)?;
        }
        Ok(())
    }
}

/// Writes one take to disk, reporting progress as it goes.
struct TakeSink<'a> {
    file: std::io::BufWriter<std::fs::File>,
    path: PathBuf,
    written: u64,
    expected: u64,
    name: String,
    progress: &'a mut dyn TakeProgress,
}

#[async_trait]
impl ChunkSink for TakeSink<'_> {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        self.file
            .write_all(chunk)
            .map_err(|e| ProviderError::Other(format!("writing {}: {e}", self.path.display())))?;
        self.written += chunk.len() as u64;
        self.progress
            .advanced(&self.name, self.written, self.expected)
            .map_err(|e| ProviderError::Other(format!("reporting progress: {e}")))
    }
}

impl TakeSink<'_> {
    /// Flushes to disk and returns what was written. `sync_all` is the point:
    /// the size check above has to see the bytes, not a buffer.
    fn finish(mut self) -> Result<u64, CliError> {
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        self.progress.finished(&self.name, self.written)?;
        Ok(self.written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_the_units_a_bucket_bills_in() {
        // The figures from the design: a two hour mix and one stem.
        assert_eq!(human_size(1_382_400_044), "1.38 GB");
        assert_eq!(human_size(691_200_044), "691.2 MB");
        assert_eq!(human_size(5_500_000_000), "5.50 GB");
        assert_eq!(human_size(4_096), "4.1 KB");
        assert_eq!(human_size(44), "44 B");
        assert_eq!(human_size(0), "0 B");
    }

    #[test]
    fn a_take_name_drops_the_prefix_it_shares_with_every_other_take() {
        let prefix = session_prefix("abcd");
        let take = take_from(
            &prefix,
            ObjectMeta::new(format!("{prefix}stems/bass.flac"), 7),
        );
        assert_eq!(take.name, "stems/bass.flac");
        assert_eq!(take.key, format!("{prefix}stems/bass.flac"));
        // A key from somewhere else is shown as it is rather than mangled.
        let odd = take_from(&prefix, ObjectMeta::new("elsewhere/mix.flac", 1));
        assert_eq!(odd.name, "elsewhere/mix.flac");
    }

    #[test]
    fn modified_times_shorten_to_the_minute_or_stay_as_they_came() {
        assert_eq!(
            short_time(Some("2026-07-25T10:00:00.000Z")),
            "2026-07-25 10:00"
        );
        // RFC 1123, which is what a HEAD sends, has no shape to trim.
        assert_eq!(
            short_time(Some("Sat, 25 Jul 2026 10:00:00 GMT")),
            "Sat, 25 Jul 2026 10:00:00 GMT"
        );
        assert_eq!(short_time(None), "-");
        assert_eq!(short_time(Some("")), "");
    }

    #[test]
    fn plurals() {
        assert_eq!(plural(1, "take"), "1 take");
        assert_eq!(plural(3, "take"), "3 takes");
        assert_eq!(plural(0, "take"), "0 takes");
    }

    /// A take already on disk at the listed size costs no egress; one at a
    /// different size stops the download instead of being overwritten.
    #[test]
    fn the_plan_skips_what_is_here_and_refuses_to_clobber() {
        let dir = std::env::temp_dir().join(format!("jamstream-cli-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let takes = vec![
            Take {
                key: "k/mix.flac".to_owned(),
                name: "mix.flac".to_owned(),
                size: 4,
                last_modified: None,
            },
            Take {
                key: "k/stems/bass.flac".to_owned(),
                name: "stems/bass.flac".to_owned(),
                size: 9,
                last_modified: None,
            },
        ];
        assert_eq!(
            plan_downloads(&takes, &dir).unwrap(),
            vec![Action::Fetch, Action::Fetch]
        );

        std::fs::write(dir.join("mix.flac"), b"abcd").unwrap();
        assert_eq!(
            plan_downloads(&takes, &dir).unwrap(),
            vec![Action::Have, Action::Fetch]
        );

        std::fs::write(dir.join("mix.flac"), b"ab").unwrap();
        let err = plan_downloads(&takes, &dir).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        assert!(err.contains("--out"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A key is an arbitrary string and the bucket is the host's, so a take
    /// that would land outside the output directory is refused rather than
    /// written.
    #[test]
    fn a_key_that_climbs_out_of_the_output_directory_is_refused() {
        let dir = Path::new("/tmp/takes");
        let take = |name: &str| Take {
            key: format!("jamstream/recordings/s1/{name}"),
            name: name.to_owned(),
            size: 4,
            last_modified: None,
        };
        assert_eq!(
            destination(dir, &take("mix.flac")).unwrap(),
            dir.join("mix.flac")
        );
        // Joined per component, so the platform's own separator lands
        // between stems and the file on every platform.
        assert_eq!(
            destination(dir, &take("stems/bass.flac")).unwrap(),
            dir.join("stems").join("bass.flac")
        );
        for hostile in [
            "../../../etc/passwd",
            "..",
            "stems/../../out.flac",
            "/etc/passwd",
            "",
        ] {
            let err = destination(dir, &take(hostile)).unwrap_err().to_string();
            assert!(err.contains("has to land inside"), "{hostile:?}: {err}");
        }
        // And the plan refuses before anything is priced or downloaded.
        assert!(plan_downloads(&[take("../escape.flac")], dir).is_err());
    }

    /// The Win32 name rules travel with the bucket, so a key only some
    /// filesystems can hold is refused on every platform: on Windows it would
    /// open a device, write an alternate data stream, or collapse onto
    /// another name, and a mac downloading it would upload it right back.
    #[test]
    fn a_key_only_some_filesystems_can_hold_is_refused() {
        let dir = Path::new("/tmp/takes");
        let take = |name: &str| Take {
            key: format!("jamstream/recordings/s1/{name}"),
            name: name.to_owned(),
            size: 4,
            last_modified: None,
        };
        for hostile in [
            // DOS device names, bare, cased, with an extension, and nested.
            "NUL",
            "nul.flac",
            "CON",
            "com1.flac",
            "LPT9",
            "stems/NUL.flac",
            // A colon writes an alternate data stream.
            "mix.flac:hidden",
            // The other reserved characters.
            "a<b.flac",
            "a>b.flac",
            "quote\".flac",
            "pipe|.flac",
            "what?.flac",
            "star*.flac",
            "back\\slash.flac",
            // Win32 strips these, collapsing the file onto another name.
            "mix.flac.",
            "mix.flac ",
            "stems /bass.flac",
        ] {
            let err = destination(dir, &take(hostile)).unwrap_err().to_string();
            assert!(
                err.contains("has to work on every platform"),
                "{hostile:?}: {err}"
            );
        }
        // The names the recorder produces, and their lookalikes, keep working.
        for fine in [
            "jamstream-2026-08-01-1930-mix.flac",
            "stems/bass.flac",
            "jamstream-2026-08-01-1930-Sørén.flac",
            "NULL.flac",
            "COM10.flac",
            "concert.flac",
        ] {
            let name = fine.split('/').next_back().unwrap();
            assert_eq!(
                destination(dir, &take(fine)).unwrap().file_name().unwrap(),
                std::ffi::OsStr::new(name),
                "{fine:?} must keep working"
            );
        }
        // And the plan refuses a hazard before anything is priced.
        assert!(plan_downloads(&[take("NUL.flac")], dir).is_err());
    }

    /// Progress has to be readable in a log, so a pipe gets whole lines at
    /// fixed percentages rather than carriage returns.
    #[tokio::test]
    async fn progress_reports_percentages_on_their_own_lines_when_piped() {
        let dir = std::env::temp_dir().join(format!("jamstream-cli-prog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mix.flac");
        let mut out: Vec<u8> = Vec::new();
        let mut progress = Percentages::new(false, &mut out);
        let mut sink = TakeSink {
            file: std::io::BufWriter::new(std::fs::File::create(&path).unwrap()),
            path: path.clone(),
            written: 0,
            expected: 100,
            name: "mix.flac".to_owned(),
            progress: &mut progress,
        };
        for _ in 0..10 {
            sink.write_chunk(&[7u8; 10]).await.unwrap();
        }
        assert_eq!(sink.finish().unwrap(), 100);
        assert_eq!(std::fs::read(&path).unwrap(), vec![7u8; 100]);
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains('\r'), "a pipe must get no carriage returns");
        for pct in ["10%", "50%", "100%"] {
            assert!(text.contains(pct), "missing {pct}: {text}");
        }
        assert_eq!(
            text.lines().count(),
            10,
            "one line per step, not one per chunk: {text}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The counter restarts per take, so the second take of a download reports
    /// its own percentages rather than picking up where the first stopped.
    #[tokio::test]
    async fn each_take_reports_from_zero() {
        let mut out: Vec<u8> = Vec::new();
        let mut progress = Percentages::new(false, &mut out);
        for name in ["mix.flac", "stems/bass.flac"] {
            progress.started(name, 100).unwrap();
            for step in 1..=10u64 {
                progress.advanced(name, step * 10, 100).unwrap();
            }
        }
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text.matches("100%").count(),
            2,
            "both takes must reach 100 percent: {text}"
        );
        assert_eq!(text.lines().count(), 20, "{text}");
    }

    /// A mix is one object per take and the rest are stems, and the two are
    /// priced apart, so the row has to be able to tell them apart by name.
    #[test]
    fn the_mix_is_named_apart_from_the_stems() {
        let take = |name: &str| Take {
            key: format!("jamstream/recordings/s1/{name}"),
            name: name.to_owned(),
            size: 4,
            last_modified: None,
        };
        assert!(take("jamstream-2026-07-28-1930-mix.flac").is_mix());
        assert!(!take("jamstream-2026-07-28-1930-Ana.flac").is_mix());
        assert!(!take("stems/bass.flac").is_mix());
        assert_eq!(
            Take {
                size: 1_382_400_044,
                ..take("mix.flac")
            }
            .size_display(),
            "1.38 GB"
        );
    }
}
