//! Machine-local preferences: which bucket a provider's takes go to and how
//! long they are kept ([`RecordingPrefs`]), and the audio setup this computer
//! uses ([`AppPrefs`]).
//!
//! Files rather than the keychain, because none of it is secret: the key that
//! writes the bucket is in the keychain and nothing here can hold one. Files
//! rather than session state, because these are set up once per computer,
//! before the session that needs them exists.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jamstream_cloud::Retention;
use jamstream_cloud::private::{create_private_dir, write_private};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// One provider's bucket: its name and the region it lives in. The region is
/// kept because it decides which endpoint gets signed, so a check and a launch
/// have to agree on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    pub name: String,
    pub region: String,
}

impl Bucket {
    /// A bucket needs both halves to be usable at all.
    pub fn is_set(&self) -> bool {
        !self.name.trim().is_empty() && !self.region.trim().is_empty()
    }
}

/// What the Recording tab keeps between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingPrefs {
    /// Bucket per provider, keyed as [`jamstream_cloud::ProviderKind`] spells
    /// it. A host with two clouds keeps one for each rather than retyping when
    /// they switch.
    #[serde(default)]
    pub buckets: BTreeMap<String, Bucket>,
    /// Retention for new sessions, as the same `7d`, `30d`, `90d`, `forever`
    /// token the session record and the CLI flag use. A string so a value this
    /// build does not know reads back verbatim rather than turning into a
    /// silently different rule.
    #[serde(default)]
    pub retention: Option<String>,
}

impl RecordingPrefs {
    /// The bucket for one provider, if both halves of one are set.
    pub fn bucket(&self, provider: &str) -> Option<&Bucket> {
        self.buckets.get(provider).filter(|b| b.is_set())
    }

    pub fn set_bucket(&mut self, provider: &str, name: &str, region: &str) {
        let bucket = Bucket {
            name: name.trim().to_owned(),
            region: region.trim().to_owned(),
        };
        if bucket.name.is_empty() && bucket.region.is_empty() {
            self.buckets.remove(provider);
        } else {
            self.buckets.insert(provider.to_owned(), bucket);
        }
    }

    /// The saved retention, or the shared default when there is none.
    pub fn retention(&self) -> Retention {
        self.retention
            .as_deref()
            .and_then(|r| r.parse().ok())
            .unwrap_or_default()
    }

    pub fn set_retention(&mut self, retention: Retention) {
        self.retention = Some(retention.to_string());
    }

    /// Reads `path`, or the defaults when there is no file there. A file that
    /// will not decode is an error rather than silent defaults: a host who set a
    /// bucket should not find recording quietly off.
    pub fn load_from(path: &Path) -> Result<RecordingPrefs, String> {
        load_json(path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        save_json(path, self)
    }
}

/// The audio setup this computer uses, restored at the next launch: the
/// selected devices by backend id (`None` is the System default entry), and
/// the buffer size. Every launch used to start back on the defaults (#328).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppPrefs {
    #[serde(default)]
    pub capture_id: Option<String>,
    #[serde(default)]
    pub playback_id: Option<String>,
    /// Applied only when it is one of the picker's own choices, so a hand
    /// edit cannot smuggle in a size no picker offers.
    #[serde(default)]
    pub buffer_frames: Option<u32>,
    /// The exclusive-access answer (Windows). `None`, a file from before the
    /// setting existed, reads as the default: allowed.
    #[serde(default)]
    pub allow_exclusive: Option<bool>,
    /// The display name last joined with, pre-filling the join screen so a
    /// band member types who they are once, not once per session (#357).
    #[serde(default)]
    pub display_name: Option<String>,
}

impl AppPrefs {
    /// Reads `path`, or the defaults when there is no file yet. Unlike the
    /// recording prefs, a file that will not decode falls back to the
    /// defaults with a log line: nothing in here is a promise whose silent
    /// loss costs data, and refusing to start the audio setup over it would
    /// punish the wrong thing.
    pub fn load_from(path: &Path) -> AppPrefs {
        load_json(path).unwrap_or_else(|err| {
            tracing::warn!(%err, "audio preferences unreadable; using defaults");
            AppPrefs::default()
        })
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        save_json(path, self)
    }
}

fn load_json<T: Default + DeserializeOwned>(path: &Path) -> Result<T, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| format!("cannot read {}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    write_private(path, body.as_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}

/// `recording.json` beside the session records this machine already keeps.
///
/// A path rather than a hidden default, for the reason [`crate::app::JamApp`]
/// takes its credential store as a parameter: a test that read this by default
/// would read the developer's own bucket, and a test that wrote it would change
/// it.
pub fn path() -> Result<PathBuf, String> {
    jamstream_cli::state::state_dir()
        .map(|dir| dir.join("recording.json"))
        .map_err(|e| e.to_string())
}

/// `settings.json` beside `recording.json`, holding [`AppPrefs`]. Its own
/// file rather than more fields in recording.json, because that file is the
/// Recording tab's and reads like the session records beside it.
pub fn app_path() -> Result<PathBuf, String> {
    jamstream_cli::state::state_dir()
        .map(|dir| dir.join("settings.json"))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_is_kept_per_provider_and_needs_both_halves() {
        let mut prefs = RecordingPrefs::default();
        assert_eq!(prefs.bucket("aws"), None);
        prefs.set_bucket("aws", " my-jams ", " eu-west-1 ");
        prefs.set_bucket("digitalocean", "our-takes", "nyc3");
        let aws = prefs.bucket("aws").expect("a saved bucket");
        assert_eq!(aws.name, "my-jams");
        assert_eq!(aws.region, "eu-west-1");
        assert_eq!(
            prefs.bucket("digitalocean").map(|b| b.region.as_str()),
            Some("nyc3")
        );
        // A name with no region cannot be signed for, so it is not a bucket.
        prefs.set_bucket("aws", "my-jams", "");
        assert_eq!(prefs.bucket("aws"), None);
        prefs.set_bucket("aws", "", "");
        assert!(!prefs.buckets.contains_key("aws"));
    }

    #[test]
    fn retention_defaults_to_thirty_days_and_round_trips_through_the_file() {
        let mut prefs = RecordingPrefs::default();
        assert_eq!(prefs.retention(), Retention::Days30);
        prefs.set_retention(Retention::Days90);
        prefs.set_bucket("gcp", "takes", "europe-west1");
        let json = serde_json::to_string(&prefs).expect("encode");
        // The token the rest of the product uses, so this file reads like the
        // session record beside it.
        assert!(json.contains("\"90d\""), "{json}");
        let read: RecordingPrefs = serde_json::from_str(&json).expect("decode");
        assert_eq!(read, prefs);
        assert_eq!(read.retention(), Retention::Days90);
    }

    /// Nothing in here may be able to hold a key, so the file cannot become the
    /// second place a secret rests.
    #[test]
    fn the_preferences_file_has_nowhere_to_put_a_key() {
        let mut prefs = RecordingPrefs::default();
        prefs.set_bucket("aws", "my-jams", "eu-west-1");
        prefs.set_retention(Retention::Days7);
        let json = serde_json::to_string(&prefs).expect("encode");
        for word in ["key", "secret", "token", "credential"] {
            assert!(
                !json.contains(word),
                "{word:?} appears in the preferences file: {json}"
            );
        }
    }

    #[test]
    fn a_file_that_is_not_there_yet_reads_as_the_defaults_and_one_that_is_broken_says_so() {
        let dir = std::env::temp_dir().join(format!("jamstream-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("recording.json");
        assert_eq!(
            RecordingPrefs::load_from(&path).expect("no file is not an error"),
            RecordingPrefs::default()
        );
        let mut prefs = RecordingPrefs::default();
        prefs.set_bucket("aws", "my-jams", "eu-west-1");
        prefs.save_to(&path).expect("save");
        assert_eq!(RecordingPrefs::load_from(&path).expect("read back"), prefs);

        std::fs::write(&path, "{not json").expect("write");
        let err = RecordingPrefs::load_from(&path).expect_err("a broken file is not defaults");
        assert!(err.contains("recording.json"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audio_preferences_round_trip_and_a_broken_file_reads_as_defaults() {
        let dir = std::env::temp_dir().join(format!("jamstream-app-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");
        assert_eq!(AppPrefs::load_from(&path), AppPrefs::default());

        let prefs = AppPrefs {
            capture_id: Some("coreaudio:scarlett-in".to_owned()),
            playback_id: None,
            buffer_frames: Some(240),
            allow_exclusive: Some(false),
            display_name: Some("Ana".to_owned()),
        };
        prefs.save_to(&path).expect("save");
        assert_eq!(AppPrefs::load_from(&path), prefs);

        // A file from before the exclusive setting existed reads as None,
        // which the app treats as the default: allowed.
        let old: AppPrefs =
            serde_json::from_str("{\"buffer_frames\":240}").expect("an old file decodes");
        assert_eq!(old.allow_exclusive, None);

        // Unlike the recording prefs, a broken settings file is defaults with
        // a log line, not a refusal: nothing here loses data silently.
        std::fs::write(&path, "{not json").expect("write");
        assert_eq!(AppPrefs::load_from(&path), AppPrefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_retention_falls_back_rather_than_becoming_another_rule() {
        let prefs: RecordingPrefs =
            serde_json::from_str("{\"retention\":\"whenever\"}").expect("decode");
        assert_eq!(prefs.retention(), Retention::Days30);
    }
}
