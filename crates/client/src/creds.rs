//! Credential storage for the host wizard. Secrets live in the platform
//! keychain (service "jamstream", account "<provider>.<field>"); the same
//! environment variables the CLI reads remain a silent fallback so a
//! terminal-configured machine works in the app with no extra setup.
//! Values are never logged and never rendered unmasked without an explicit
//! reveal.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use jamstream_cloud::cloudinit::StorageCredential;
use jamstream_cloud::providers::aws::AwsProvider;
use jamstream_cloud::providers::digitalocean::DigitalOceanProvider;
use jamstream_cloud::providers::gcp::GcpProvider;
use jamstream_cloud::providers::gcp_auth::ServiceAccountTokenSource;
use jamstream_cloud::{DEFAULT_SESSION_PORT, Provider, ProviderKind};
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

/// Where one provider's object storage key pair lives: two slots of its own,
/// beside that provider's provisioning credential and never the same slot, with
/// no fallback from one to the other.
///
/// AWS is the case that makes the distinction load bearing. `aws.access_key_id`
/// launches instances; `aws.storage_access_key_id` writes recordings, and the
/// second is written into the session machine's user data, where a key that
/// could call `ec2:RunInstances` would be a far larger thing than a bucket
/// prefix.
pub fn storage_key_fields(
    provider: ProviderKind,
) -> ((&'static str, &'static str), (&'static str, &'static str)) {
    (
        (provider.as_str(), "storage_access_key_id"),
        (provider.as_str(), "storage_secret_access_key"),
    )
}

/// The storage key pair for one provider: this computer's keychain first, then
/// the recording variables the CLI reads, so a machine set up in a terminal
/// records from the app with nothing new to paste.
///
/// Both halves have to come from the same place. Half a pair is not a
/// credential, and completing a keychain id with an environment secret is how a
/// host ends up handing a VM a key they never chose. The environment half is the
/// CLI's own reader, so which variables count, and the refusal when none do, are
/// the same on both surfaces.
///
/// The error names what is missing and never the value of anything present.
pub fn storage_credential(
    creds: &dyn CredStore,
    env: &EnvReader,
    provider: ProviderKind,
) -> Result<StorageCredential, String> {
    let (id_field, secret_field) = storage_key_fields(provider);
    if let (Some(access_key_id), Some(secret_access_key)) = (
        creds.get(id_field.0, id_field.1),
        creds.get(secret_field.0, secret_field.1),
    ) {
        return Ok(StorageCredential::KeyPair {
            access_key_id,
            secret_access_key,
        });
    }
    jamstream_cli::storage::credential_from(provider, |key| env(key)).map_err(|err| {
        format!("{err} The Recording tab in Settings takes a key for this computer.")
    })
}

/// Whether recording to a bucket on `provider` has a key at all.
pub fn has_storage_credential(
    creds: &dyn CredStore,
    env: &EnvReader,
    provider: ProviderKind,
) -> bool {
    storage_credential(creds, env, provider).is_ok()
}

/// Writes one provider's storage key pair to the keychain. Called only after a
/// check has proved the pair can write the bucket.
pub fn save_storage_credential(
    creds: &dyn CredStore,
    provider: ProviderKind,
    access_key_id: &str,
    secret_access_key: &str,
) -> Result<(), String> {
    let (id_field, secret_field) = storage_key_fields(provider);
    creds.set(id_field.0, id_field.1, access_key_id.trim())?;
    creds.set(secret_field.0, secret_field.1, secret_access_key.trim())
}

/// Forgets one provider's storage key pair.
pub fn forget_storage_credential(creds: &dyn CredStore, provider: ProviderKind) {
    let (id_field, secret_field) = storage_key_fields(provider);
    creds.delete(id_field.0, id_field.1);
    creds.delete(secret_field.0, secret_field.1);
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

/// Builds a provider from stored or environment credentials for everything
/// that is not a launch: readiness, the credential check, the region survey,
/// the teardown. The error is what the wizard's "setup needed" state explains
/// away; it never contains a secret.
pub fn build_provider(
    name: &str,
    creds: &dyn CredStore,
    env: &EnvReader,
) -> Result<Box<dyn Provider>, String> {
    build_provider_for_port(name, DEFAULT_SESSION_PORT, creds, env)
}

/// [`build_provider`] for a launch, which has to name the port the session
/// will listen on: that is the one port the provider opens in the firewall it
/// creates, so a provider built without it can leave the machine behind a
/// firewall for a different port. `jamstream host` threads it through
/// `providers::resolve_for_port` and the app dropped it (#227).
pub fn build_provider_for_port(
    name: &str,
    session_port: u16,
    creds: &dyn CredStore,
    env: &EnvReader,
) -> Result<Box<dyn Provider>, String> {
    match name {
        // Local spawns a process rather than opening a firewall, and takes its
        // port from the flat config the launch writes.
        "local" => jamstream_cli::providers::resolve("local").map_err(|e| e.to_string()),
        "digitalocean" => {
            let token = lookup(creds, env, DO_TOKEN, "DIGITALOCEAN_TOKEN")
                .ok_or("no DigitalOcean token saved and DIGITALOCEAN_TOKEN is not set")?;
            Ok(Box::new(
                DigitalOceanProvider::new(token).with_session_port(session_port),
            ))
        }
        "aws" => {
            let id = lookup(creds, env, AWS_ACCESS_KEY_ID, "AWS_ACCESS_KEY_ID")
                .ok_or("no AWS access key saved and AWS_ACCESS_KEY_ID is not set")?;
            let secret = lookup(creds, env, AWS_SECRET_ACCESS_KEY, "AWS_SECRET_ACCESS_KEY")
                .ok_or("no AWS secret key saved and AWS_SECRET_ACCESS_KEY is not set")?;
            Ok(Box::new(
                AwsProvider::new(id, secret).with_session_port(session_port),
            ))
        }
        "gcp" => build_gcp(session_port, creds, env),
        other => Err(format!("unknown provider {other:?}")),
    }
}

/// GCP: a stored service account key first, then the same environment modes
/// the CLI supports (project + access token pair, then a key file path).
fn build_gcp(
    session_port: u16,
    creds: &dyn CredStore,
    env: &EnvReader,
) -> Result<Box<dyn Provider>, String> {
    if let Some(json) = creds.get(GCP_SERVICE_ACCOUNT_JSON.0, GCP_SERVICE_ACCOUNT_JSON.1) {
        return gcp_for_port(&json, session_port, env);
    }
    if let (Some(project), Some(token)) = (env("GOOGLE_CLOUD_PROJECT"), env("GCP_ACCESS_TOKEN")) {
        return Ok(Box::new(
            GcpProvider::with_access_token(project, token).with_session_port(session_port),
        ));
    }
    if let Some(path) = env("GOOGLE_APPLICATION_CREDENTIALS") {
        let json = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read service account key {path}: {e}"))?;
        return gcp_for_port(&json, session_port, env);
    }
    Err("no GCP service account key saved and no GCP credentials in the environment".to_owned())
}

/// A provider from raw pasted/loaded service account JSON, as the wizard's
/// GCP setup pane holds it. The pane checks credentials and launches nothing,
/// so it gets the default session port.
pub fn gcp_from_json(json: &str, env: &EnvReader) -> Result<Box<dyn Provider>, String> {
    gcp_for_port(json, DEFAULT_SESSION_PORT, env)
}

fn gcp_for_port(
    json: &str,
    session_port: u16,
    env: &EnvReader,
) -> Result<Box<dyn Provider>, String> {
    let source = ServiceAccountTokenSource::from_json(json).map_err(|e| e.to_string())?;
    let project = env("GOOGLE_CLOUD_PROJECT")
        .or_else(|| source.project_id().map(str::to_owned))
        .ok_or(
            "the service account key has no project_id field and GOOGLE_CLOUD_PROJECT is not set",
        )?;
    Ok(Box::new(
        GcpProvider::with_token_source(project, Arc::new(source)).with_session_port(session_port),
    ))
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

    /// A storage key is a second credential, so it must never land in the slot
    /// the provisioning one uses. AWS is where the two would collide.
    #[test]
    fn a_storage_key_shares_no_slot_with_the_credential_that_launches_machines() {
        let store = MemStore::default();
        let (id, secret) = storage_key_fields(ProviderKind::Aws);
        assert_ne!(id, AWS_ACCESS_KEY_ID);
        assert_ne!(secret, AWS_SECRET_ACCESS_KEY);
        store
            .set(AWS_ACCESS_KEY_ID.0, AWS_ACCESS_KEY_ID.1, "AKIA-launch")
            .expect("set");
        assert_eq!(
            store.get(id.0, id.1),
            None,
            "the launch key must not read back as a storage key"
        );
        // Each provider gets its own pair, and no two providers share one.
        let mut slots = Vec::new();
        for provider in [
            ProviderKind::Aws,
            ProviderKind::DigitalOcean,
            ProviderKind::Gcp,
        ] {
            let (id, secret) = storage_key_fields(provider);
            slots.push(id);
            slots.push(secret);
        }
        let unique: std::collections::BTreeSet<_> = slots.iter().collect();
        assert_eq!(unique.len(), slots.len(), "{slots:?} has a shared slot");
    }

    #[test]
    fn a_saved_storage_key_reads_back_and_the_environment_is_the_fallback() {
        let store = MemStore::default();
        assert!(!has_storage_credential(
            &store,
            &env_of(&[]),
            ProviderKind::DigitalOcean
        ));
        save_storage_credential(&store, ProviderKind::DigitalOcean, " DO00ID ", " secret ")
            .expect("save");
        let credential = storage_credential(&store, &env_of(&[]), ProviderKind::DigitalOcean)
            .expect("a saved pair");
        let StorageCredential::KeyPair {
            access_key_id,
            secret_access_key,
        } = credential;
        // Trimmed on the way in, so a pasted key with a stray newline works.
        assert_eq!(access_key_id, "DO00ID");
        assert_eq!(secret_access_key, "secret");

        forget_storage_credential(&store, ProviderKind::DigitalOcean);
        assert!(!has_storage_credential(
            &store,
            &env_of(&[]),
            ProviderKind::DigitalOcean
        ));
        // The CLI's own two variables still work, so a machine configured in a
        // terminal needs nothing pasted here.
        let env = env_of(&[
            ("SPACES_ACCESS_KEY_ID", "DO00ENV"),
            ("SPACES_SECRET_ACCESS_KEY", "s"),
        ]);
        assert!(has_storage_credential(
            &store,
            &env,
            ProviderKind::DigitalOcean
        ));
    }

    /// Half a pair is not a credential, and it must not complete itself from
    /// somewhere else: a keychain id with an environment secret is a key nobody
    /// chose. The refusal names the variables that would work and quotes
    /// neither half of what is present.
    #[test]
    fn half_a_storage_key_is_refused_and_no_secret_is_in_the_reason() {
        let store = MemStore::default();
        let (id, _) = storage_key_fields(ProviderKind::Aws);
        store.set(id.0, id.1, "AKIDSTORAGE").expect("set");
        let err = storage_credential(&store, &env_of(&[]), ProviderKind::Aws)
            .expect_err("half a pair cannot write a bucket");
        assert!(
            err.contains(jamstream_cli::storage::RECORDING_VARS.1),
            "{err}"
        );
        assert!(err.contains("Recording tab"), "{err}");
        assert!(
            !err.contains("AKIDSTORAGE"),
            "the reason quoted a key: {err}"
        );
        // Local has no bucket, so it has no key slot either.
        assert!(storage_credential(&store, &env_of(&[]), ProviderKind::Local).is_err());

        // And the key that launches instances is not a storage key, however it
        // is set: this one is written into the machine's user data.
        let launch_pair = env_of(&[
            ("AWS_ACCESS_KEY_ID", "AKIALAUNCH"),
            ("AWS_SECRET_ACCESS_KEY", "launch-secret"),
        ]);
        assert!(
            !has_storage_credential(&store, &launch_pair, ProviderKind::Aws),
            "the launch pair must never be read as a recording key"
        );
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
