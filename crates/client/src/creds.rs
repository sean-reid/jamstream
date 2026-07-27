//! Credential storage for the host wizard. Secrets live in the platform
//! keychain (service "jamstream", account "<provider>.<field>"); the same
//! environment variables the CLI reads remain a silent fallback so a
//! terminal-configured machine works in the app with no extra setup.
//! Values are never logged and never rendered unmasked without an explicit
//! reveal.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use jamstream_cloud::Provider;
use jamstream_cloud::providers::aws::AwsProvider;
use jamstream_cloud::providers::digitalocean::DigitalOceanProvider;
use jamstream_cloud::providers::gcp::GcpProvider;
use jamstream_cloud::providers::gcp_auth::ServiceAccountTokenSource;
use jamstream_protocol::control::StreamPlatform;

/// Keychain service name; one entry per provider field.
const SERVICE: &str = "jamstream";

/// The complete set of stored fields.
pub const DO_TOKEN: (&str, &str) = ("digitalocean", "token");
pub const AWS_ACCESS_KEY_ID: (&str, &str) = ("aws", "access_key_id");
pub const AWS_SECRET_ACCESS_KEY: (&str, &str) = ("aws", "secret_access_key");
pub const GCP_SERVICE_ACCOUNT_JSON: (&str, &str) = ("gcp", "service_account_json");

/// Where one platform's stream key is kept between sessions, so a host
/// pastes it once per computer rather than once per session. The keychain is
/// the only place it rests on this machine; the session server holds it in
/// memory and never writes it to the VM's disk.
pub fn stream_key_field(platform: StreamPlatform) -> (&'static str, &'static str) {
    (platform.as_str(), "stream_key")
}

/// Reads one environment variable; injectable so provider readiness and
/// lookup order are testable without touching the process environment.
pub type EnvReader = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The process environment, as production uses.
pub fn system_env() -> EnvReader {
    Arc::new(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
}

pub trait CredStore: Send + Sync {
    fn get(&self, provider: &str, field: &str) -> Option<String>;
    fn set(&self, provider: &str, field: &str, value: &str) -> Result<(), String>;
    fn delete(&self, provider: &str, field: &str);
}

/// Platform keychain via the keyring crate.
pub struct KeyringStore;

impl KeyringStore {
    fn entry(provider: &str, field: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(SERVICE, &format!("{provider}.{field}"))
            .map_err(|e| format!("keychain: {e}"))
    }
}

impl CredStore for KeyringStore {
    fn get(&self, provider: &str, field: &str) -> Option<String> {
        Self::entry(provider, field)
            .ok()?
            .get_password()
            .ok()
            .filter(|v| !v.is_empty())
    }

    fn set(&self, provider: &str, field: &str, value: &str) -> Result<(), String> {
        Self::entry(provider, field)?
            .set_password(value)
            .map_err(|e| format!("keychain: {e}"))
    }

    fn delete(&self, provider: &str, field: &str) {
        if let Ok(entry) = Self::entry(provider, field) {
            let _ = entry.delete_credential();
        }
    }
}

/// In-memory store for tests and demo runs.
#[derive(Default)]
pub struct MemStore {
    values: Mutex<HashMap<(String, String), String>>,
}

impl CredStore for MemStore {
    fn get(&self, provider: &str, field: &str) -> Option<String> {
        self.values
            .lock()
            .expect("cred store")
            .get(&(provider.to_owned(), field.to_owned()))
            .cloned()
            .filter(|v| !v.is_empty())
    }

    fn set(&self, provider: &str, field: &str, value: &str) -> Result<(), String> {
        self.values
            .lock()
            .expect("cred store")
            .insert((provider.to_owned(), field.to_owned()), value.to_owned());
        Ok(())
    }

    fn delete(&self, provider: &str, field: &str) {
        self.values
            .lock()
            .expect("cred store")
            .remove(&(provider.to_owned(), field.to_owned()));
    }
}

/// The lookup order for one field: the credential store first, then the
/// CLI's environment variable as a silent fallback.
pub fn lookup(
    creds: &dyn CredStore,
    env: &EnvReader,
    field: (&str, &str),
    var: &str,
) -> Option<String> {
    creds.get(field.0, field.1).or_else(|| env(var))
}

/// Builds a provider from stored or environment credentials. The error is
/// what the wizard's "setup needed" state explains away; it never contains
/// a secret.
pub fn build_provider(
    name: &str,
    creds: &dyn CredStore,
    env: &EnvReader,
) -> Result<Box<dyn Provider>, String> {
    match name {
        "local" => jamstream_cli::providers::resolve("local").map_err(|e| e.to_string()),
        "digitalocean" => {
            let token = lookup(creds, env, DO_TOKEN, "DIGITALOCEAN_TOKEN")
                .ok_or("no DigitalOcean token saved and DIGITALOCEAN_TOKEN is not set")?;
            Ok(Box::new(DigitalOceanProvider::new(token)))
        }
        "aws" => {
            let id = lookup(creds, env, AWS_ACCESS_KEY_ID, "AWS_ACCESS_KEY_ID")
                .ok_or("no AWS access key saved and AWS_ACCESS_KEY_ID is not set")?;
            let secret = lookup(creds, env, AWS_SECRET_ACCESS_KEY, "AWS_SECRET_ACCESS_KEY")
                .ok_or("no AWS secret key saved and AWS_SECRET_ACCESS_KEY is not set")?;
            Ok(Box::new(AwsProvider::new(id, secret)))
        }
        "gcp" => build_gcp(creds, env),
        other => Err(format!("unknown provider {other:?}")),
    }
}

/// GCP: a stored service account key first, then the same environment modes
/// the CLI supports (project + access token pair, then a key file path).
fn build_gcp(creds: &dyn CredStore, env: &EnvReader) -> Result<Box<dyn Provider>, String> {
    if let Some(json) = creds.get(GCP_SERVICE_ACCOUNT_JSON.0, GCP_SERVICE_ACCOUNT_JSON.1) {
        return gcp_from_json(&json, env);
    }
    if let (Some(project), Some(token)) = (env("GOOGLE_CLOUD_PROJECT"), env("GCP_ACCESS_TOKEN")) {
        return Ok(Box::new(GcpProvider::with_access_token(project, token)));
    }
    if let Some(path) = env("GOOGLE_APPLICATION_CREDENTIALS") {
        let json = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read service account key {path}: {e}"))?;
        return gcp_from_json(&json, env);
    }
    Err("no GCP service account key saved and no GCP credentials in the environment".to_owned())
}

/// A provider from raw pasted/loaded service account JSON, as the wizard's
/// GCP setup pane holds it.
pub fn gcp_from_json(json: &str, env: &EnvReader) -> Result<Box<dyn Provider>, String> {
    let source = ServiceAccountTokenSource::from_json(json).map_err(|e| e.to_string())?;
    let project = env("GOOGLE_CLOUD_PROJECT")
        .or_else(|| source.project_id().map(str::to_owned))
        .ok_or(
            "the service account key has no project_id field and GOOGLE_CLOUD_PROJECT is not set",
        )?;
    Ok(Box::new(GcpProvider::with_token_source(
        project,
        Arc::new(source),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamstream_cloud::ProviderKind;

    fn env_of(pairs: &[(&str, &str)]) -> EnvReader {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Arc::new(move |key| map.get(key).cloned())
    }

    #[test]
    fn store_wins_over_environment() {
        let store = MemStore::default();
        store
            .set(DO_TOKEN.0, DO_TOKEN.1, "from-keychain")
            .expect("set");
        let env = env_of(&[("DIGITALOCEAN_TOKEN", "from-env")]);
        assert_eq!(
            lookup(&store, &env, DO_TOKEN, "DIGITALOCEAN_TOKEN").as_deref(),
            Some("from-keychain")
        );
    }

    #[test]
    fn environment_is_the_silent_fallback() {
        let store = MemStore::default();
        let env = env_of(&[("DIGITALOCEAN_TOKEN", "from-env")]);
        assert_eq!(
            lookup(&store, &env, DO_TOKEN, "DIGITALOCEAN_TOKEN").as_deref(),
            Some("from-env")
        );
        assert_eq!(lookup(&store, &env_of(&[]), DO_TOKEN, "X"), None);
    }

    #[test]
    fn empty_values_do_not_count() {
        let store = MemStore::default();
        store.set(DO_TOKEN.0, DO_TOKEN.1, "").expect("set");
        assert_eq!(lookup(&store, &env_of(&[]), DO_TOKEN, "X"), None);
        store.set(DO_TOKEN.0, DO_TOKEN.1, "t").expect("set");
        store.delete(DO_TOKEN.0, DO_TOKEN.1);
        assert_eq!(store.get(DO_TOKEN.0, DO_TOKEN.1), None);
    }

    #[test]
    fn local_needs_no_credentials() {
        let p = build_provider("local", &MemStore::default(), &env_of(&[])).expect("local");
        assert_eq!(p.kind(), ProviderKind::Local);
    }

    #[test]
    fn clouds_without_credentials_explain_themselves() {
        let env = env_of(&[]);
        let store = MemStore::default();
        for (name, hint) in [
            ("digitalocean", "DIGITALOCEAN_TOKEN"),
            ("aws", "AWS_ACCESS_KEY_ID"),
            ("gcp", "GCP"),
        ] {
            let err = build_provider(name, &store, &env)
                .err()
                .expect("must not build");
            assert!(err.contains(hint), "error for {name} was {err:?}");
        }
    }

    #[test]
    fn stored_do_token_builds_the_provider() {
        let store = MemStore::default();
        store.set(DO_TOKEN.0, DO_TOKEN.1, "dop_v1_x").expect("set");
        let p = build_provider("digitalocean", &store, &env_of(&[])).expect("do");
        assert_eq!(p.kind(), ProviderKind::DigitalOcean);
    }

    #[test]
    fn aws_needs_both_fields() {
        let store = MemStore::default();
        store
            .set(AWS_ACCESS_KEY_ID.0, AWS_ACCESS_KEY_ID.1, "AKIA")
            .expect("set");
        let err = build_provider("aws", &store, &env_of(&[]))
            .err()
            .expect("half a key");
        assert!(err.contains("AWS_SECRET_ACCESS_KEY"));
        // Mixed sources are fine: id from the store, secret from env.
        let env = env_of(&[("AWS_SECRET_ACCESS_KEY", "s")]);
        assert!(build_provider("aws", &store, &env).is_ok());
    }

    #[test]
    fn each_platform_gets_its_own_stream_key_slot() {
        let store = MemStore::default();
        let twitch = stream_key_field(StreamPlatform::Twitch);
        let youtube = stream_key_field(StreamPlatform::YouTube);
        assert_ne!(twitch, youtube);
        store.set(twitch.0, twitch.1, "live_000_fake").expect("set");
        assert_eq!(store.get(youtube.0, youtube.1), None);
        // And a stream key shares no slot with a provider credential.
        assert_ne!(twitch, DO_TOKEN);
        store.delete(twitch.0, twitch.1);
        assert_eq!(store.get(twitch.0, twitch.1), None);
    }

    #[test]
    fn invalid_gcp_json_is_a_readable_error() {
        let store = MemStore::default();
        store
            .set(
                GCP_SERVICE_ACCOUNT_JSON.0,
                GCP_SERVICE_ACCOUNT_JSON.1,
                "not json",
            )
            .expect("set");
        let err = build_provider("gcp", &store, &env_of(&[]))
            .err()
            .expect("bad json");
        assert!(!err.is_empty());
    }
}
