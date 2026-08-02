//! `jamstream sweep`: find and destroy every jamstream-tagged instance
//! across resolvable providers. Nothing tagged jamstream may keep billing.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use jamstream_cloud::{Instance, Provider, SweepFilter, SweepReport};

use crate::CliError;
use crate::state::{self, SessionStatus};

pub async fn run<W: Write>(
    providers: &[Box<dyn Provider>],
    dry_run: bool,
    out: &mut W,
) -> Result<(), CliError> {
    let report = jamstream_cloud::sweep(providers, SweepFilter::All, dry_run).await;

    if report.found.is_empty() {
        writeln!(out, "No jamstream-tagged instances found.")?;
    } else {
        writeln!(
            out,
            "{:<14} {:<14} {:<16} RESULT",
            "PROVIDER", "REGION", "INSTANCE"
        )?;
        for inst in &report.found {
            let result = if dry_run {
                "would destroy".to_owned()
            } else if report.destroyed.contains(inst) {
                "destroyed".to_owned()
            } else if let Some((_, err)) = report.failed.iter().find(|(f, _)| f == inst) {
                format!("failed: {err}")
            } else {
                "found".to_owned()
            };
            writeln!(
                out,
                "{:<14} {:<14} {:<16} {}",
                inst.provider.as_str(),
                // RegionId's Display ignores width flags; pad the &str instead.
                inst.region.id.as_str(),
                inst.id,
                result
            )?;
        }
        writeln!(
            out,
            "{} found, {} destroyed, {} failed.",
            report.found.len(),
            report.destroyed.len(),
            report.failed.len()
        )?;
    }
    // A dry run destroyed nothing, so the records it would close are still
    // true; reconciling from one would mark live sessions ended.
    if !dry_run {
        for session in reconcile(&report)? {
            let prefix = &session[..8.min(session.len())];
            writeln!(
                out,
                "Session {prefix}: recorded running, instance gone; marked it ended."
            )?;
        }
    }
    // Only worth a line when there was something to close: firewalls cost
    // nothing, so their absence is not news.
    if !report.firewalls_removed.is_empty() {
        writeln!(
            out,
            "Closed {} leftover session firewall(s).",
            report.firewalls_removed.len()
        )?;
    }
    // A provider that could not be listed was never searched, which is not
    // the same as finding nothing there, and the difference is a bill.
    for (provider, err) in &report.unswept {
        writeln!(out, "{provider}: could not be searched: {err}")?;
    }
    // Same for one region of a provider that answered for the rest: AWS and
    // GCP return what they could reach, so a throttled region reads as empty.
    for search in &report.searches {
        if !search.is_complete() {
            writeln!(
                out,
                "{}: could not search {}; anything there was not looked for.",
                search.provider,
                search
                    .unsearched
                    .iter()
                    .map(|region| region.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
        }
    }
    if !report.is_clean() {
        return Err(CliError::Failed(
            "sweep could not account for everything tagged jamstream; anything above is still \
             billing"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Closes the session records this sweep made false, and returns their ids.
///
/// A record is only closed on positive evidence that its instance is gone:
/// either the sweep destroyed it, or a provider answering to the record's own
/// name searched all of itself and no longer lists it. Anything else leaves
/// the record alone, because closing one blanks the issuer key and hides the
/// session from `jamstream end`, and a machine nobody can end keeps billing.
///
/// Absence of evidence is not evidence of absence, and there are four ways to
/// mistake one for the other here. A provider whose credentials are missing
/// is not in the sweep at all, so no search names it. A provider whose
/// listing failed outright is in `unswept`, again with no search. One that
/// reached some of its regions reports the rest as unsearched, and that is
/// not a complete search. And the mock answers to "mock" alone, however real
/// the kind its instances borrow.
pub fn reconcile(report: &SweepReport) -> Result<Vec<String>, CliError> {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut closed = Vec::new();
    for (path, mut session) in state::list()? {
        if session.status != SessionStatus::Running {
            continue;
        }
        let Some(search) = report.search_for(&session.provider) else {
            continue;
        };
        let ours = |inst: &Instance| inst.id == session.instance_id && inst.provider == search.kind;
        if report.destroyed.iter().any(&ours) {
            session.mark_ended(now_unix);
        } else if search.is_complete() && !report.found.iter().any(ours) {
            // A full listing that does not have it. Weaker evidence than a
            // destroy, so the record closes but keeps its issuer key.
            session.mark_ended_unlisted(now_unix);
        } else {
            continue;
        }
        state::write_to(&path, &session)?;
        closed.push(session.session_id_hex);
    }
    Ok(closed)
}
