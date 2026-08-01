//! `jamstream end`: destroy a session's instance, verify it is gone, and
//! rewrite the state file as ended.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use jamstream_cloud::{Provider, ProviderError, RegionId};

use crate::CliError;
use crate::cli::EndArgs;
use crate::providers;
use crate::state::{self, SessionState, SessionStatus};

/// Picks the target session from a prefix or --last. Only running
/// sessions qualify.
pub fn select(args: &EndArgs) -> Result<(PathBuf, SessionState), CliError> {
    let running: Vec<(PathBuf, SessionState)> = state::list()?
        .into_iter()
        .filter(|(_, s)| s.status == SessionStatus::Running)
        .collect();
    if args.last {
        return running
            .into_iter()
            .max_by_key(|(_, s)| s.created_unix)
            .ok_or_else(|| CliError::Usage("no running sessions to end".to_owned()));
    }
    let prefix = args
        .session
        .as_deref()
        .ok_or_else(|| CliError::Usage("pass a session id prefix or --last".to_owned()))?;
    let mut matches: Vec<(PathBuf, SessionState)> = running
        .into_iter()
        .filter(|(_, s)| s.session_id_hex.starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(CliError::Usage(format!(
            "no running session matches {prefix:?}; run jamstream status to list sessions"
        ))),
        1 => Ok(matches.remove(0)),
        n => Err(CliError::Usage(format!(
            "{n} running sessions match {prefix:?}; use more of the id"
        ))),
    }
}

/// Resolves the provider recorded in the state file. An unwired provider
/// cannot destroy anything yet, so the error points at sweep for later.
pub fn resolve_provider(session: &SessionState) -> Result<Box<dyn Provider>, CliError> {
    providers::resolve(&session.provider).map_err(|e| {
        CliError::Usage(format!(
            "{e} Once the provider is wired, run jamstream sweep to clean up anything \
             this session left running."
        ))
    })
}

pub async fn run<W: Write>(
    path: &Path,
    mut session: SessionState,
    provider: &dyn Provider,
    out: &mut W,
) -> Result<(), CliError> {
    let region = RegionId::new(session.region.clone());
    match provider.destroy(&region, &session.instance_id).await {
        Ok(()) => {}
        // Already gone (crashed earlier, self-destructed, or swept); still
        // verify below and mark the session ended.
        Err(ProviderError::NotFound(_)) => {
            writeln!(
                out,
                "Instance {} was already gone; marking the session ended.",
                session.instance_id
            )?;
        }
        Err(e) => return Err(e.into()),
    }

    // The instance is gone, so its firewall has nothing behind it. AWS may
    // still refuse while the network interface detaches, in which case the
    // next sweep collects it, so this never fails an otherwise clean end.
    match provider.destroy_orphan_firewalls().await {
        Ok(names) if !names.is_empty() => {
            writeln!(out, "Closed {} session firewall(s).", names.len())?;
        }
        Ok(_) => {}
        Err(e) => writeln!(
            out,
            "Could not close the session firewall ({e}); jamstream sweep will retry."
        )?,
    }

    let remaining = provider.list_tagged(Some(&session.session_id_hex)).await?;
    if !remaining.is_empty() {
        return Err(CliError::Failed(format!(
            "{} instance(s) still listed for session {} after destroy; run jamstream sweep",
            remaining.len(),
            &session.session_id_hex[..8]
        )));
    }

    session.mark_ended(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    state::write_to(path, &session)?;
    writeln!(
        out,
        "Session {} ended. Instance {} is destroyed.",
        &session.session_id_hex[..8],
        session.instance_id
    )?;
    Ok(())
}
