//! On-disk session records. One JSON file per session under the state
//! directory, written through [`jamstream_cloud::private`] because it holds
//! the issuer private key.

use std::path::{Path, PathBuf};

use jamstream_cloud::private::{create_private_dir, write_private};
use serde::{Deserialize, Serialize};

use crate::CliError;

/// Overrides the state directory; used by integration tests.
pub const STATE_DIR_ENV: &str = "JAMSTREAM_STATE_DIR";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InviteRecord {
    /// "host", "musician <id>", or "listener <id>".
    pub role: String,
    pub invite: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Running,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id_hex: String,
    pub provider: String,
    pub region: String,
    pub instance_id: String,
    /// ip:port of the session server.
    pub address: String,
    pub created_unix: u64,
    pub hourly_microusd: u64,
    pub issuer_private_key_b64: String,
    pub server_public_key_b64: String,
    pub invites: Vec<InviteRecord>,
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_unix: Option<u64>,
}

/// Where session records live: [`STATE_DIR_ENV`] when set, else a
/// `sessions` directory under the platform data directory.
pub fn state_dir() -> Result<PathBuf, CliError> {
    if let Some(dir) = std::env::var_os(STATE_DIR_ENV) {
        return Ok(PathBuf::from(dir));
    }
    Ok(data_dir()?.join("sessions"))
}

/// `<platform data dir>/jamstream`, the root of everything this machine
/// keeps about its sessions. The local provider's registry and per-session
/// server configs live here too.
///
/// There is no fallback: these files hold the issuer private key and the
/// server's, and the temp directory is somewhere every account on the
/// machine can write.
pub fn data_dir() -> Result<PathBuf, CliError> {
    resolve_data_dir(dirs::data_local_dir())
}

fn resolve_data_dir(platform: Option<PathBuf>) -> Result<PathBuf, CliError> {
    platform.map(|dir| dir.join("jamstream")).ok_or_else(|| {
        CliError::Usage(format!(
            "no private directory to keep session keys in: this environment has no \
             platform data directory (on unix that means HOME is unset or not absolute, \
             which is common under systemd, cron, and some sudo configurations). \
             Set HOME, or set {STATE_DIR_ENV} to a directory only you can write."
        ))
    })
}

pub fn path_for(session_id_hex: &str) -> Result<PathBuf, CliError> {
    Ok(state_dir()?.join(format!("{session_id_hex}.json")))
}

/// Writes to the canonical location for the session id. Returns the path.
pub fn save(state: &SessionState) -> Result<PathBuf, CliError> {
    let path = path_for(&state.session_id_hex)?;
    write_to(&path, state)?;
    Ok(path)
}

pub fn write_to(path: &Path, state: &SessionState) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let json = serde_json::to_string_pretty(state)?;
    // Replaces the file rather than truncating it: a torn write here loses
    // the issuer key and the session with it, while `list` silently skips
    // what it cannot decode and the VM keeps billing.
    write_private(path, json.as_bytes())?;
    Ok(())
}

pub fn load(path: &Path) -> Result<SessionState, CliError> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// Every readable state file, oldest first. A missing directory is an
/// empty list, not an error.
pub fn list() -> Result<Vec<(PathBuf, SessionState)>, CliError> {
    let dir = state_dir()?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "json")
            && let Ok(state) = load(&path)
        {
            out.push((path, state));
        }
    }
    out.sort_by_key(|entry| entry.1.created_unix);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SessionState {
        SessionState {
            session_id_hex: "deadbeefcafef00ddeadbeefcafef00d".to_owned(),
            provider: "mock".to_owned(),
            region: "mock-east".to_owned(),
            instance_id: "mock-000001".to_owned(),
            address: "10.0.0.1:43210".to_owned(),
            created_unix: 1_784_000_000,
            hourly_microusd: 16_800,
            issuer_private_key_b64: "aXNzdWVy".to_owned(),
            server_public_key_b64: "c2VydmVy".to_owned(),
            invites: vec![
                InviteRecord {
                    role: "host".to_owned(),
                    invite: "jamstream://join/AAAA".to_owned(),
                },
                InviteRecord {
                    role: "musician 1".to_owned(),
                    invite: "jamstream://join/BBBB".to_owned(),
                },
            ],
            status: SessionStatus::Running,
            ended_unix: None,
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join("jamstream-cli-state-tests")
            .join(format!("{}-{name}.json", std::process::id()))
    }

    #[test]
    fn round_trips_and_restricts_permissions() {
        let path = temp_path("round-trip");
        let state = sample();
        write_to(&path, &state).unwrap();
        assert_eq!(load(&path).unwrap(), state);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "state file must be 0600");
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn rewrite_keeps_permissions_and_ended_fields() {
        let path = temp_path("rewrite");
        let mut state = sample();
        write_to(&path, &state).unwrap();
        state.status = SessionStatus::Ended;
        state.ended_unix = Some(1_784_000_500);
        write_to(&path, &state).unwrap();
        let reloaded = load(&path).unwrap();
        assert_eq!(reloaded.status, SessionStatus::Ended);
        assert_eq!(reloaded.ended_unix, Some(1_784_000_500));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_file(&path).unwrap();
    }

    /// The whole point of #49: with no platform data directory the records
    /// used to land in the temp directory, so the issuer private key sat at
    /// a fully predictable path in a place every account can write.
    #[test]
    fn no_data_directory_is_an_error_not_a_detour_through_tmp() {
        let err = resolve_data_dir(None).unwrap_err().to_string();
        assert!(err.contains(STATE_DIR_ENV), "error was: {err}");
        let tmp = std::env::temp_dir();
        assert!(
            !err.contains(&tmp.display().to_string()),
            "the temp directory must not be offered as a fallback: {err}"
        );
        // And with one, it is that directory and nothing invented.
        let home = PathBuf::from("/home/someone/.local/share");
        assert_eq!(
            resolve_data_dir(Some(home.clone())).unwrap(),
            home.join("jamstream")
        );
    }

    #[test]
    fn running_state_omits_ended_unix() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(!json.contains("ended_unix"));
        assert!(json.contains("\"status\":\"running\""));
    }
}
