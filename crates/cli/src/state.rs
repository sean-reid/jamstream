//! On-disk session records. One JSON file per session under the state
//! directory, written through [`jamstream_cloud::private`] because it holds
//! the issuer private key.

use std::path::{Path, PathBuf};

use jamstream_cloud::private::{create_private_dir, write_private};
use serde::{Deserialize, Serialize};

use crate::CliError;

/// Overrides the state directory; used by integration tests.
pub const STATE_DIR_ENV: &str = "JAMSTREAM_STATE_DIR";

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct InviteRecord {
    /// "host", "musician <id>", or "listener <id>".
    pub role: String,
    pub invite: String,
}

/// Redacts the invite. An invite is a bearer credential: whoever holds one
/// joins the session as the member it names, which is why `jamstream join`
/// warns about passing one on argv. Redacting the issuer key on the record
/// that carries them and then printing the invites themselves would have shut
/// the door that mints new seats and left every existing seat open.
impl std::fmt::Debug for InviteRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InviteRecord")
            .field("role", &self.role)
            .field("invite", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Running,
    Ended,
}

/// The bucket a cloud session recorded to: enough to find its takes again,
/// and no credential.
///
/// The storage key stays in the environment, where the host already keeps the
/// keys they launched with, so a stolen state directory yields no bucket
/// access. `jamstream recordings` rebuilds the client from these fields plus
/// that key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingRecord {
    /// Provider holding the bucket, as [`jamstream_cloud::ProviderKind`]
    /// spells it: aws, digitalocean, or gcp.
    pub provider: String,
    pub bucket: String,
    /// The bucket's region: an AWS region, a Spaces slug, or a GCS location.
    pub region: String,
    /// The retention rule applied to this session's prefix, for display.
    pub retention: String,
    pub stems: bool,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
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

/// Redacts the issuer private key, which mints and revokes every invite to
/// the session. Nothing formats a `SessionState` today; the point is that
/// the first thing that does cannot leak it.
impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionState")
            .field("session_id_hex", &self.session_id_hex)
            .field("provider", &self.provider)
            .field("region", &self.region)
            .field("instance_id", &self.instance_id)
            .field("address", &self.address)
            .field("created_unix", &self.created_unix)
            .field("hourly_microusd", &self.hourly_microusd)
            .field("issuer_private_key_b64", &"<redacted>")
            .field("server_public_key_b64", &self.server_public_key_b64)
            .field("invites", &self.invites)
            .field("status", &self.status)
            .field("ended_unix", &self.ended_unix)
            .finish()
    }
}

impl SessionState {
    /// Drops the issuer private key. Nothing can be minted or revoked for a
    /// session whose server is destroyed, so the key stops being useful at
    /// exactly the moment `end` succeeds, while the record itself is worth
    /// keeping for `status` and for the cost history.
    pub fn forget_issuer_key(&mut self) {
        self.issuer_private_key_b64 = String::new();
    }
}

/// Where session records live: [`STATE_DIR_ENV`] when set, else a
/// `sessions` directory under the platform data directory.
pub fn state_dir() -> Result<PathBuf, CliError> {
    if let Some(dir) = std::env::var_os(STATE_DIR_ENV) {
        return Ok(PathBuf::from(dir));
    }
    Ok(data_dir()?.join("sessions"))
}

/// Where a recorded local session's takes land: a `recordings` directory
/// beside the session state, honoring the same [`STATE_DIR_ENV`] override.
pub fn recordings_dir() -> Result<PathBuf, CliError> {
    if let Some(dir) = std::env::var_os(STATE_DIR_ENV) {
        return Ok(PathBuf::from(dir).join("recordings"));
    }
    Ok(data_dir()?.join("recordings"))
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

/// Where a session's bucket details live: a file of their own under
/// `buckets`, beside the session records.
///
/// A sidecar rather than a field on [`SessionState`]: takes outlive the
/// session, a session that never recorded carries no file at all, and the
/// subdirectory keeps [`list`] from ever having to sort one kind of record
/// from the other.
pub fn recording_path_for(session_id_hex: &str) -> Result<PathBuf, CliError> {
    Ok(state_dir()?
        .join("buckets")
        .join(format!("{session_id_hex}.json")))
}

/// Records where a session's takes are going. Called at launch, once.
pub fn save_recording(
    session_id_hex: &str,
    recording: &RecordingRecord,
) -> Result<PathBuf, CliError> {
    let path = recording_path_for(session_id_hex)?;
    write_recording_to(&path, recording)?;
    Ok(path)
}

pub fn write_recording_to(path: &Path, recording: &RecordingRecord) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    write_private(path, serde_json::to_string_pretty(recording)?.as_bytes())?;
    Ok(())
}

/// The bucket a session recorded to, or None when it recorded nowhere.
pub fn load_recording(session_id_hex: &str) -> Result<Option<RecordingRecord>, CliError> {
    read_recording_at(&recording_path_for(session_id_hex)?)
}

/// A file that cannot be decoded is an error, not a session with no takes:
/// silently reporting no recording for a session that made one is how a band
/// loses a take.
pub fn read_recording_at(path: &Path) -> Result<Option<RecordingRecord>, CliError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
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

    /// The printed record dir has to sit beside the session state so takes
    /// survive `jamstream end`, which removes the per-session server
    /// directory but must never touch a recording. Read-only against the
    /// live environment, like the provider state dir test.
    #[test]
    fn recordings_sit_beside_the_session_state() {
        match std::env::var_os(STATE_DIR_ENV) {
            Some(dir) => assert_eq!(
                recordings_dir().unwrap(),
                PathBuf::from(dir).join("recordings")
            ),
            None => match recordings_dir() {
                Ok(dir) => {
                    assert!(dir.ends_with("jamstream/recordings"));
                    assert_eq!(dir.parent(), state_dir().unwrap().parent());
                }
                Err(err) => assert!(err.to_string().contains(STATE_DIR_ENV)),
            },
        }
    }

    /// The issuer key mints and revokes every invite to the session, so it
    /// is not something a stray `{:?}` may print.
    #[test]
    fn debug_never_reveals_the_issuer_key() {
        let rendered = format!("{:?}", sample());
        assert!(!rendered.contains("aXNzdWVy"));
        assert!(rendered.contains("<redacted>"));
        // Nor the invites the record carries, which are bearer credentials in
        // their own right: each one is a seat in the session for whoever holds
        // it, and the roles are what a log wants anyway.
        assert!(!rendered.contains("jamstream://join/AAAA"), "{rendered}");
        assert!(!rendered.contains("jamstream://join/BBBB"), "{rendered}");
        assert!(!format!("{:?}", sample().invites).contains("join/AAAA"));
        assert!(rendered.contains("musician 1"), "{rendered}");
        // The public half and the identity stay visible.
        assert!(rendered.contains("c2VydmVy"));
        assert!(rendered.contains("deadbeefcafef00d"));
    }

    #[test]
    fn ending_a_session_forgets_the_issuer_key() {
        let path = temp_path("forget");
        let mut state = sample();
        write_to(&path, &state).unwrap();
        state.status = SessionStatus::Ended;
        state.forget_issuer_key();
        write_to(&path, &state).unwrap();

        let reloaded = load(&path).unwrap();
        assert!(reloaded.issuer_private_key_b64.is_empty());
        assert!(!std::fs::read_to_string(&path).unwrap().contains("aXNzdWVy"));
        // Everything the record is kept for survives.
        assert_eq!(reloaded.session_id_hex, state.session_id_hex);
        assert_eq!(reloaded.hourly_microusd, state.hourly_microusd);
        assert_eq!(reloaded.invites.len(), 2);
        std::fs::remove_file(&path).unwrap();
    }

    /// The sidecar round trips, holds no credential, and lives one directory
    /// below the session records so the session listing never sees it.
    #[test]
    fn the_bucket_sidecar_round_trips_and_carries_no_key() {
        let path = temp_path("bucket");
        let record = RecordingRecord {
            provider: "aws".to_owned(),
            bucket: "my-jams".to_owned(),
            region: "eu-west-1".to_owned(),
            retention: "30d".to_owned(),
            stems: true,
        };
        assert_eq!(read_recording_at(&path).unwrap(), None);
        write_recording_to(&path, &record).unwrap();
        assert_eq!(read_recording_at(&path).unwrap(), Some(record));
        let text = std::fs::read_to_string(&path).unwrap();
        for secret in ["access", "secret", "key"] {
            assert!(
                !text.contains(secret),
                "the sidecar must carry no credential: {text}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_file(&path).unwrap();

        // Read-only against the live environment, like the state dir tests
        // above: the sidecar is a directory below the session records, which
        // is what keeps it out of the listing.
        match recording_path_for("deadbeef") {
            Ok(resolved) => {
                assert!(resolved.ends_with("buckets/deadbeef.json"));
                assert_eq!(
                    resolved.parent().unwrap().parent(),
                    Some(&*state_dir().unwrap())
                );
            }
            Err(err) => assert!(err.to_string().contains(STATE_DIR_ENV)),
        }
    }

    /// Corrupt bucket details must not read as a session with no takes.
    #[test]
    fn an_undecodable_sidecar_is_an_error() {
        let path = temp_path("bucket-corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(read_recording_at(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn running_state_omits_ended_unix() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(!json.contains("ended_unix"));
        assert!(json.contains("\"status\":\"running\""));
    }
}
