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

use std::io::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use jamstream_cloud::{
    ChunkSink, EgressQuote, ObjectMeta, ProviderError, RegionId, format_microusd, session_prefix,
};

use crate::CliError;
use crate::cli::{RecordingsGetArgs, RecordingsListArgs};
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
struct Take {
    key: String,
    /// The key with the session prefix removed: `mix.flac`,
    /// `stems/bass.flac`.
    name: String,
    size: u64,
    last_modified: Option<String>,
}

/// Every session this machine knows that recorded to a bucket, oldest first.
fn recorded_sessions() -> Result<Vec<(SessionState, RecordingRecord)>, CliError> {
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
async fn takes_for(
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

/// Lists the takes of every session this machine recorded to a bucket.
///
/// One unreachable bucket does not hide the rest: the reason lands on that
/// session's line, the other sessions still list, and the command still exits
/// nonzero so a script cannot mistake a failure for an empty bucket.
pub async fn list<W: Write>(
    args: &RecordingsListArgs,
    stores: &dyn Stores,
    out: &mut W,
) -> Result<(), CliError> {
    let mut rows: Vec<(SessionState, RecordingRecord, Result<Vec<Take>, CliError>)> = Vec::new();
    for (session, record) in recorded_sessions()? {
        let takes = takes_for(&session, &record, stores).await;
        rows.push((session, record, takes));
    }

    if args.json {
        let value: Vec<serde_json::Value> = rows
            .iter()
            .map(|(session, record, takes)| {
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
        .any(|(_, _, takes)| takes.as_ref().is_ok_and(|takes| !takes.is_empty()));
    if any_takes {
        writeln!(
            out,
            "{:<10} {:<40} {:>10}  MODIFIED",
            "SESSION", "TAKE", "SIZE"
        )?;
    }
    for (session, record, takes) in &rows {
        let short = short_id(session);
        let bucket = format!("{} ({}/{})", record.bucket, record.provider, record.region);
        match takes {
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
fn first_error(
    rows: Vec<(SessionState, RecordingRecord, Result<Vec<Take>, CliError>)>,
) -> Result<(), CliError> {
    match rows.into_iter().find_map(|(_, _, takes)| takes.err()) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Downloads one session's takes into `--out` or the current directory.
pub async fn get<W: Write + Send>(
    args: &RecordingsGetArgs,
    stores: &dyn Stores,
    prompt: &mut Prompt<'_, W>,
    out: &mut W,
) -> Result<(), CliError> {
    let (session, record) = select(&args.session)?;
    let takes = takes_for(&session, &record, stores).await?;
    if takes.is_empty() {
        return Err(CliError::Failed(format!(
            "session {} recorded to {} but the bucket holds no takes under {}",
            short_id(&session),
            record.bucket,
            session_prefix(&session.session_id_hex)
        )));
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
    let quote = EgressQuote::compute(
        provider_kind(&record.provider)?,
        &RegionId::new(record.region.clone()),
        bytes,
    )?;
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
    std::fs::create_dir_all(&dir)?;
    let terminal = prompt.terminal;
    let mut fetched = 0u64;
    for take in &wanted {
        let path = destination(&dir, take)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let written = fetch_one(store.as_ref(), &record.bucket, take, &path, terminal, out).await?;
        fetched += written;
    }
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

/// Streams one take to `path` and proves what landed is the whole object.
async fn fetch_one<W: Write + Send>(
    store: &dyn jamstream_cloud::ObjectStore,
    bucket: &str,
    take: &Take,
    path: &Path,
    terminal: bool,
    out: &mut W,
) -> Result<u64, CliError> {
    let file = std::fs::File::create(path)?;
    let mut sink = TakeSink {
        file: std::io::BufWriter::new(file),
        path: path.to_owned(),
        written: 0,
        expected: take.size,
        name: take.name.clone(),
        next_pct: 0,
        terminal,
        out,
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
enum Action {
    Fetch,
    /// Already here at exactly the size the bucket lists.
    Have,
}

/// Where one take is written, refusing a name that would land outside `dir`.
///
/// The server writes takes under a sanitized prefix, but the bucket belongs to
/// the host and an object store key is an arbitrary string: `..` in one is a
/// valid key and would be a write into somebody's home directory.
fn destination(dir: &Path, take: &Take) -> Result<PathBuf, CliError> {
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
    Ok(dir.join(relative))
}

/// Decides each take's fate before any egress is spent. A local file of a
/// different size is a conflict rather than something to overwrite, because
/// it may be the only copy of an edit.
fn plan_downloads(takes: &[Take], dir: &Path) -> Result<Vec<Action>, CliError> {
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
fn human_size(bytes: u64) -> String {
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

/// Writes one take to disk, reporting progress as it goes.
///
/// Progress is percentages on their own lines rather than a redrawn bar, so a
/// log or a pipe reads the same as a terminal; a terminal gets the same
/// percentages redrawn in place.
struct TakeSink<'a, W: Write> {
    file: std::io::BufWriter<std::fs::File>,
    path: PathBuf,
    written: u64,
    expected: u64,
    name: String,
    next_pct: u64,
    terminal: bool,
    out: &'a mut W,
}

#[async_trait]
impl<W: Write + Send> ChunkSink for TakeSink<'_, W> {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        self.file
            .write_all(chunk)
            .map_err(|e| ProviderError::Other(format!("writing {}: {e}", self.path.display())))?;
        self.written += chunk.len() as u64;
        self.report()
            .map_err(|e| ProviderError::Other(format!("reporting progress: {e}")))
    }
}

impl<W: Write> TakeSink<'_, W> {
    fn percent(&self) -> u64 {
        if self.expected == 0 {
            return 100;
        }
        (self.written.saturating_mul(100) / self.expected).min(100)
    }

    fn report(&mut self) -> std::io::Result<()> {
        let pct = self.percent();
        if pct < self.next_pct {
            return Ok(());
        }
        self.next_pct = pct - pct % PROGRESS_STEP + PROGRESS_STEP;
        let line = format!("  {:<40} {:>3}%", self.name, pct);
        if self.terminal {
            write!(self.out, "\r{line}")?;
            self.out.flush()
        } else {
            writeln!(self.out, "{line}")
        }
    }

    /// Flushes to disk and returns what was written. `sync_all` is the point:
    /// the size check below has to see the bytes, not a buffer.
    fn finish(mut self) -> Result<u64, CliError> {
        if self.terminal {
            writeln!(self.out)?;
        }
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
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
            destination(dir, &take("stems/bass.flac")).unwrap(),
            dir.join("stems/bass.flac")
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

    /// Progress has to be readable in a log, so a pipe gets whole lines at
    /// fixed percentages rather than carriage returns.
    #[tokio::test]
    async fn progress_reports_percentages_on_their_own_lines_when_piped() {
        let dir = std::env::temp_dir().join(format!("jamstream-cli-prog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mix.flac");
        let mut out: Vec<u8> = Vec::new();
        let mut sink = TakeSink {
            file: std::io::BufWriter::new(std::fs::File::create(&path).unwrap()),
            path: path.clone(),
            written: 0,
            expected: 100,
            name: "mix.flac".to_owned(),
            next_pct: 0,
            terminal: false,
            out: &mut out,
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
}
