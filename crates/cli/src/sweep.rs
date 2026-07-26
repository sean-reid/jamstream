//! `jamstream sweep`: find and destroy every jamstream-tagged instance
//! across resolvable providers. Nothing tagged jamstream may keep billing.

use std::io::Write;

use jamstream_cloud::{Provider, SweepFilter};

use crate::CliError;

pub async fn run<W: Write>(
    providers: &[Box<dyn Provider>],
    dry_run: bool,
    out: &mut W,
) -> Result<(), CliError> {
    let report = jamstream_cloud::sweep(providers, SweepFilter::All, dry_run).await;

    if report.found.is_empty() {
        writeln!(out, "No jamstream-tagged instances found.")?;
        return Ok(());
    }
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
    if !report.is_clean() {
        return Err(CliError::Failed(
            "sweep could not destroy every instance; the failures above are still billing"
                .to_owned(),
        ));
    }
    Ok(())
}
