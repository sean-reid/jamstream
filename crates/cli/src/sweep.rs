//! `jamstream sweep`: find and destroy every jamstream-tagged instance
//! across resolvable providers. Nothing tagged jamstream may keep billing.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use jamstream_cloud::{Provider, ProviderKind, SweepFilter, SweepReport};

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
        for session in reconcile(&report, providers)? {
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
    for (kind, err) in &report.unswept {
        writeln!(out, "{}: could not be searched: {err}", kind.as_str())?;
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

/// Closes the session records this sweep made false: every running record
/// whose instance the sweep destroyed, or whose provider was searched and no
/// longer lists the instance. Returns the ids of the sessions it closed.
///
/// A record on a provider the sweep could not search stays untouched, the
/// same absence-of-evidence rule the report's `unswept` list exists for:
/// nobody looked, so nothing was learned. A destroy that failed leaves the
/// record running too, because the instance is still billing.
pub fn reconcile(
    report: &SweepReport,
    providers: &[Box<dyn Provider>],
) -> Result<Vec<String>, CliError> {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut closed = Vec::new();
    for (path, mut session) in state::list()? {
        if session.status != SessionStatus::Running {
            continue;
        }
        let ours = |inst: &jamstream_cloud::Instance| {
            inst.id == session.instance_id && record_names_kind(&session.provider, inst.provider)
        };
        let destroyed = report.destroyed.iter().any(&ours);
        let listed = report.found.iter().any(ours);
        let searched = providers.iter().any(|p| {
            record_names_kind(&session.provider, p.kind())
                && !report.unswept.iter().any(|(kind, _)| *kind == p.kind())
        });
        if destroyed || (searched && !listed) {
            session.mark_ended(now_unix);
            state::write_to(&path, &session)?;
            closed.push(session.session_id_hex);
        }
    }
    Ok(closed)
}

/// Whether a state record written for provider `name` describes instances of
/// `kind`. Every real provider records its kind's own name; the mock records
/// "mock" while its instances borrow a real kind.
fn record_names_kind(name: &str, kind: ProviderKind) -> bool {
    name == kind.as_str() || name == crate::providers::MOCK_PROVIDER
}
