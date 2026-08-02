//! `jamstream status`: every known session with elapsed time, accrued
//! cost, and a projection at the requested horizon. Records that say
//! running are checked against their provider before being repeated;
//! nothing on disk is rewritten here, that is `end` and `sweep`'s job.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use jamstream_cloud::{Provider, format_microusd};

use crate::CliError;
use crate::cli::StatusArgs;
use crate::state::{self, SessionState, SessionStatus};

/// How a record that says running squares with its provider.
///
/// `Unchecked` is deliberately not `Gone`: a provider that cannot be asked
/// (no credentials in this shell, network down) proved nothing, the same
/// absence-of-evidence rule the sweeper's `unswept` list follows. The
/// recorded state stands, with a note saying it could not be checked.
enum Corroboration {
    /// The provider still lists an instance for the session.
    Confirmed,
    /// The provider answered and lists nothing: the instance is gone and
    /// the record is stale.
    Gone,
    /// The provider could not be asked; the string says why.
    Unchecked(String),
}

pub async fn run<W: Write>(
    args: &StatusArgs,
    resolve: impl Fn(&str) -> Result<Box<dyn Provider>, CliError>,
    out: &mut W,
) -> Result<(), CliError> {
    let sessions = state::list()?;
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Where each session's takes went, from the sidecar written at launch. A
    // session that recorded nothing has no file, and a file that will not
    // decode is an error rather than a session quietly reported as recording
    // nowhere.
    let recordings: Vec<Option<state::RecordingRecord>> = sessions
        .iter()
        .map(|(_, s)| state::load_recording(&s.session_id_hex))
        .collect::<Result<_, _>>()?;

    // One provider question per running record, the same one `end` asks
    // after a destroy. Ended records need no call: they claim nothing that
    // could still be billing.
    let mut checks: Vec<Option<Corroboration>> = Vec::with_capacity(sessions.len());
    for (_, s) in &sessions {
        checks.push(match s.status {
            SessionStatus::Running => Some(corroborate(s, &resolve).await),
            SessionStatus::Ended => None,
        });
    }

    if args.json {
        let rows: Vec<serde_json::Value> = sessions
            .iter()
            .zip(&recordings)
            .zip(&checks)
            .map(|(((_, s), recording), check)| {
                let elapsed = elapsed_secs(s, now_unix);
                let mut row = serde_json::json!({
                    "session_id": s.session_id_hex,
                    "provider": s.provider,
                    "region": s.region,
                    "status": status_label(s, check.as_ref()),
                    "address": s.address,
                    "created_unix": s.created_unix,
                    "elapsed_secs": elapsed,
                    "hourly_microusd": s.hourly_microusd,
                    "accrued_microusd": cost_for(s.hourly_microusd, elapsed),
                    "projected_microusd": projected(s.hourly_microusd, args.hours),
                    "recording": recording,
                });
                // Only a record that says running makes a claim to check, so
                // only those rows carry the verdict.
                match check {
                    Some(Corroboration::Confirmed) => row["corroborated"] = true.into(),
                    Some(Corroboration::Gone) => row["corroborated"] = false.into(),
                    Some(Corroboration::Unchecked(note)) => {
                        row["corroborated"] = false.into();
                        row["note"] = note.as_str().into();
                    }
                    None => {}
                }
                row
            })
            .collect();
        writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
        return Ok(());
    }

    if sessions.is_empty() {
        writeln!(out, "No sessions found.")?;
        return Ok(());
    }
    writeln!(
        out,
        "{:<10} {:<20} {:<8} {:>10} {:>12} {:>14} TAKES",
        "SESSION", "PROVIDER/REGION", "STATUS", "ELAPSED", "ACCRUED", "PROJECTED"
    )?;
    for (((_, s), recording), check) in sessions.iter().zip(&recordings).zip(&checks) {
        let elapsed = elapsed_secs(s, now_unix);
        let status = status_label(s, check.as_ref());
        // A projection is a promise about future billing; only a session
        // whose instance may still exist gets one.
        let projected = if status == "running" {
            format!(
                "{} at {:.1} h",
                format_microusd(projected(s.hourly_microusd, args.hours)),
                args.hours
            )
        } else {
            "-".to_owned()
        };
        writeln!(
            out,
            "{:<10} {:<20} {:<8} {:>10} {:>12} {:>14} {}",
            &s.session_id_hex[..8.min(s.session_id_hex.len())],
            format!("{}/{}", s.provider, s.region),
            status,
            format_elapsed(elapsed),
            format_microusd(cost_for(s.hourly_microusd, elapsed)),
            projected,
            takes(recording.as_ref()),
        )?;
    }
    for ((_, s), check) in sessions.iter().zip(&checks) {
        let prefix = &s.session_id_hex[..8.min(s.session_id_hex.len())];
        match check {
            Some(Corroboration::Gone) => writeln!(
                out,
                "Session {prefix}: recorded running, instance gone; run jamstream end {prefix} \
                 to close it."
            )?,
            Some(Corroboration::Unchecked(note)) => writeln!(
                out,
                "Session {prefix}: recorded running; {} could not be checked ({note}).",
                s.provider
            )?,
            _ => {}
        }
    }
    Ok(())
}

/// Asks the record's provider whether the session still has an instance,
/// with the same call `end` uses to verify a destroy.
async fn corroborate(
    session: &SessionState,
    resolve: &impl Fn(&str) -> Result<Box<dyn Provider>, CliError>,
) -> Corroboration {
    let provider = match resolve(&session.provider) {
        Ok(p) => p,
        Err(e) => return Corroboration::Unchecked(e.to_string()),
    };
    match provider.list_tagged(Some(&session.session_id_hex)).await {
        Ok(listing) if !listing.instances.is_empty() => Corroboration::Confirmed,
        // Nothing here, but not everywhere was looked at: AWS and GCP list
        // what they could reach, so a throttled region reads as an empty
        // account and would print a live session as stale.
        Ok(listing) if !listing.is_complete() => {
            Corroboration::Unchecked(format!("{} did not answer", listing.unsearched_display()))
        }
        Ok(_) => Corroboration::Gone,
        Err(e) => Corroboration::Unchecked(e.to_string()),
    }
}

/// The one word the table, the JSON, and the uninstallers' pre-flight all
/// read. "running" is only printed when the provider confirmed it or could
/// not be asked; a session the provider disowned prints "stale", so a dead
/// session stops blocking an uninstall while an unverifiable one still does.
fn status_label(s: &SessionState, check: Option<&Corroboration>) -> &'static str {
    match (s.status, check) {
        (SessionStatus::Ended, _) => "ended",
        (SessionStatus::Running, Some(Corroboration::Gone)) => "stale",
        (SessionStatus::Running, _) => "running",
    }
}

/// What a session recorded, in one cell: the bucket it went to, with a mark for
/// stems, or a dash for a session that recorded to no bucket.
///
/// A take on a bucket is the one this can speak for. A local session writes to
/// this computer's disk through the server's own flags and leaves no bucket
/// record, so it reads as a dash here and the directory is printed at launch.
fn takes(recording: Option<&state::RecordingRecord>) -> String {
    match recording {
        Some(record) if record.stems => format!("{} +stems", record.bucket),
        Some(record) => record.bucket.clone(),
        None => "-".to_owned(),
    }
}

/// Ended sessions stop accruing at ended_unix.
fn elapsed_secs(s: &SessionState, now_unix: u64) -> u64 {
    let until = match s.status {
        SessionStatus::Running => now_unix,
        SessionStatus::Ended => s.ended_unix.unwrap_or(now_unix),
    };
    until.saturating_sub(s.created_unix)
}

fn cost_for(hourly_microusd: u64, secs: u64) -> u64 {
    ((u128::from(hourly_microusd) * u128::from(secs)) / 3600) as u64
}

fn projected(hourly_microusd: u64, hours: f32) -> u64 {
    cost_for(hourly_microusd, (hours.max(0.0) * 3600.0).round() as u64)
}

fn format_elapsed(secs: u64) -> String {
    if secs >= 3600 {
        format!("{} h {:02} min", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{} min", secs / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_math() {
        assert_eq!(cost_for(16_800, 3600), 16_800);
        assert_eq!(cost_for(16_800, 1800), 8_400);
        assert_eq!(projected(16_800, 3.0), 50_400);
        assert_eq!(projected(16_800, 0.0), 0);
    }

    #[test]
    fn a_session_says_which_bucket_it_recorded_to_or_nothing_at_all() {
        assert_eq!(takes(None), "-");
        let mut record = state::RecordingRecord {
            provider: "aws".to_owned(),
            bucket: "my-jams".to_owned(),
            region: "eu-west-1".to_owned(),
            retention: "30d".to_owned(),
            stems: false,
            applied: Some(state::RetentionApplied::ServerSide),
        };
        assert_eq!(takes(Some(&record)), "my-jams");
        record.stems = true;
        assert_eq!(takes(Some(&record)), "my-jams +stems");
        // Nothing in the cell could be a key: the record has none to print.
        let json = serde_json::to_string(&record).expect("encode");
        assert!(!json.contains("secret"), "{json}");
    }

    #[test]
    fn elapsed_formatting() {
        assert_eq!(format_elapsed(59), "0 min");
        assert_eq!(format_elapsed(120), "2 min");
        assert_eq!(format_elapsed(3_660), "1 h 01 min");
        assert_eq!(format_elapsed(45_000), "12 h 30 min");
    }
}
