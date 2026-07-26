//! On-disk session records. One JSON file per session under the state
//! directory, mode 0600 on unix because it holds the issuer private key.

use std::path::{Path, PathBuf};

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

pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(STATE_DIR_ENV) {
        return PathBuf::from(dir);
    }
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("jamstream")
        .join("sessions")
}

pub fn path_for(session_id_hex: &str) -> PathBuf {
    state_dir().join(format!("{session_id_hex}.json"))
}

/// Writes to the canonical location for the session id. Returns the path.
pub fn save(state: &SessionState) -> Result<PathBuf, CliError> {
    let path = path_for(&state.session_id_hex);
    write_to(&path, state)?;
    Ok(path)
}

pub fn write_to(path: &Path, state: &SessionState) -> Result<(), CliError> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // mode() only applies at create time; enforce on rewrites too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(json.as_bytes())?;
    Ok(())
}

pub fn load(path: &Path) -> Result<SessionState, CliError> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// Every readable state file, oldest first. A missing directory is an
/// empty list, not an error.
pub fn list() -> Result<Vec<(PathBuf, SessionState)>, CliError> {
    let dir = state_dir();
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

    #[test]
    fn running_state_omits_ended_unix() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(!json.contains("ended_unix"));
        assert!(json.contains("\"status\":\"running\""));
    }
}
