//! `jamstream end`: destroy a session's instance, verify it is gone, and
//! rewrite the state file as ended.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use jamstream_cloud::{Provider, ProviderError, RegionId};

use crate::CliError;
use crate::cli::EndArgs;
use crate::inflight::{self, InFlight};
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

/// Why ending is refused for what the probe learned, or None to go ahead.
/// `--force` overrides every refusal; an unreachable server is never one,
/// because the machine that does not answer is usually already gone.
pub fn refusal(inflight: &InFlight, force: bool) -> Option<String> {
    if force {
        return None;
    }
    match inflight {
        InFlight::Recording => Some(
            "a take is recording, and ending now would lose it. Press Stop in the app and wait \
             for UPLOADING to clear, then run this again; or pass --force to end anyway and \
             lose the take"
                .to_owned(),
        ),
        InFlight::Uploading => Some(
            "the last take is still uploading, and ending now would lose it. Wait for \
             UPLOADING to clear in the app, then run this again; or pass --force to end anyway \
             and lose the take"
                .to_owned(),
        ),
        InFlight::HostConnected => Some(
            "the host is connected from the app, so this terminal cannot tell whether a take is \
             in flight. End the session from the app, which checks; or pass --force"
                .to_owned(),
        ),
        InFlight::Nothing | InFlight::Unknown(_) => None,
    }
}

/// [`run`], after asking the server whether a take is in flight. This is
/// what `jamstream end` calls; `run` alone is the destroy for callers that
/// have already decided.
pub async fn guarded_run<W: Write>(
    args: &EndArgs,
    path: &Path,
    session: SessionState,
    provider: &dyn Provider,
    out: &mut W,
) -> Result<(), CliError> {
    let inflight = inflight::probe(&session).await;
    if let Some(why) = refusal(&inflight, args.force) {
        return Err(CliError::Usage(why));
    }
    match &inflight {
        InFlight::Recording | InFlight::Uploading => {
            writeln!(out, "Ending anyway; the take in flight is lost.")?;
        }
        InFlight::HostConnected => {
            writeln!(
                out,
                "Ending anyway; the host's app may have a take in flight."
            )?;
        }
        InFlight::Unknown(why) => {
            writeln!(
                out,
                "Could not check for a take in flight ({why}); ending anyway."
            )?;
        }
        InFlight::Nothing => {}
    }
    run(path, session, provider, out).await
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

    let remaining = provider
        .list_tagged(Some(&session.session_id_hex))
        .await?
        .instances;
    if !remaining.is_empty() {
        return Err(CliError::Failed(format!(
            "{} instance(s) still listed for session {} after destroy; run jamstream sweep",
            remaining.len(),
            &session.session_id_hex[..8.min(session.session_id_hex.len())]
        )));
    }

    // Only once the instance is really gone, and then retried rather than
    // attempted once: AWS refuses to delete a security group until the
    // terminated instance's network interface has detached, which is always
    // later than this call. A firewall whose instance is still listed is one
    // the provider is right to keep. A sweep still collects it if the budget
    // runs out, so this never fails an otherwise clean end.
    match jamstream_cloud::sweeper::close_session_firewall(
        provider,
        &session.session_id_hex,
        &jamstream_cloud::TokioSleeper,
        jamstream_cloud::sweeper::FIREWALL_WAIT,
    )
    .await
    {
        Ok(names) if !names.is_empty() => {
            writeln!(out, "Closed {} session firewall(s).", names.len())?;
        }
        Ok(_) => {}
        Err(e) => writeln!(
            out,
            "Could not close the session firewall ({e}); jamstream sweep will retry."
        )?,
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
        &session.session_id_hex[..8.min(session.session_id_hex.len())],
        session.instance_id
    )?;
    // The machine is gone and so is its journal, so this is the only copy of
    // why anything on it failed. Named here because this is the moment a host
    // goes looking, and nothing else on this side ever mentions the file.
    if let Ok(log) = state::server_log_path_for(&session.session_id_hex)
        && log.exists()
    {
        writeln!(out, "The session server's log is at {}.", log.display())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_take_in_flight_refuses_and_names_the_override() {
        for inflight in [InFlight::Recording, InFlight::Uploading] {
            let why = refusal(&inflight, false).expect("a take in flight refuses");
            assert!(why.contains("lose"), "{why}");
            assert!(why.contains("--force"), "{why}");
            assert_eq!(refusal(&inflight, true), None, "--force overrides");
        }
    }

    #[test]
    fn an_occupied_host_seat_refuses_and_points_at_the_app() {
        let why = refusal(&InFlight::HostConnected, false).unwrap();
        assert!(why.contains("from the app"), "{why}");
        assert!(why.contains("--force"), "{why}");
        assert_eq!(refusal(&InFlight::HostConnected, true), None);
    }

    /// A server that does not answer is usually a machine that is already
    /// gone, and `end` exists to close those out; refusing here would strand
    /// every crashed session behind --force.
    #[test]
    fn nothing_in_flight_and_no_answer_both_go_ahead() {
        assert_eq!(refusal(&InFlight::Nothing, false), None);
        assert_eq!(
            refusal(&InFlight::Unknown("no answer".to_owned()), false),
            None
        );
    }
}
