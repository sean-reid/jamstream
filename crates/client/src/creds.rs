//! Credential storage for the host wizard. Secrets live in the platform
//! keychain (service "jamstream", account "<provider>.<field>"); the same
//! environment variables the CLI reads remain a silent fallback so a
//! terminal-configured machine works in the app with no extra setup. A
//! secret the keychain itself refuses as too long lands in a private file
//! instead; see [`KeyringStore`].
//!
//! # No field in this product reveals a secret
//!
//! Every credential input is masked with no way to unmask it, and a character
//! count under the field stands in for reading it back. This is the one place
//! that rule is written down; the three panes that take a credential point
//! here.
//!
//! The reason is the screen a host is on. Hosting means being one keystroke
//! from a broadcast, and a key on a shared screen is worse than a typo: a
//! stream key lets a stranger broadcast as you, a cloud API token lets them
//! launch machines on your card, and a storage key lets them read every take
//! you have ever made. None of those is undone by rotating a password. A typo
//! is undone by pasting again, and the character count catches the paste that
//! took half a token, which is the failure a reveal was there for.
//!
//! The destinations sheet argued this for stream keys and held the line while
//! the host wizard's API token field kept a Show button and the Recording
//! tab's key pair kept another, so two surfaces disagreed about whether the
//! same class of secret was safe to put on a screen (#184). It is not.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use jamstream_cloud::cloudinit::StorageCredential;
use jamstream_cloud::private::{create_private_dir, read_private, write_private};
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

/// The keychain itself, under [`KeyringStore`]. A trait so the fallback
/// logic can be exercised against the refusal the Windows backend makes,
/// with the error still typed: the fallback matches
/// [`keyring::Error::TooLong`] and nothing else.
trait Keychain: Send + Sync {
    fn get(&self, account: &str) -> Option<String>;
    fn set(&self, account: &str, value: &str) -> Result<(), keyring::Error>;
    fn delete(&self, account: &str);
}

/// The operating system's keychain via the keyring crate.
struct SystemKeychain;

impl SystemKeychain {
    fn entry(account: &str) -> Result<keyring::Entry, keyring::Error> {
        keyring::Entry::new(SERVICE, account)
    }
}

impl Keychain for SystemKeychain {
    fn get(&self, account: &str) -> Option<String> {
        Self::entry(account).ok()?.get_password().ok()
    }

    fn set(&self, account: &str, value: &str) -> Result<(), keyring::Error> {
        Self::entry(account)?.set_password(value)
    }

    fn delete(&self, account: &str) {
        if let Ok(entry) = Self::entry(account) {
            let _ = entry.delete_credential();
        }
    }
}

/// Platform keychain via the keyring crate, with a private file as the
/// fallback for the one secret the keychain refuses to hold.
///
/// Windows Credential Manager caps a credential blob at 2560 bytes of
/// UTF-16, about 1280 characters, and a GCP service account key is
/// typically near twice that, so on Windows the host wizard's check would
/// pass and the save would then fail, every time. The keychain stays
/// primary; only a [`keyring::Error::TooLong`] refusal diverts the secret
/// to `<data dir>/credentials/<provider>.<field>`, written through
/// [`jamstream_cloud::private`] so the file is 0600 on unix and on Windows
/// inherits from a directory whose ACL was tightened at creation. Reads
/// ask the keychain first and fall through to the file, so which place
/// holds a secret is never recorded anywhere, and the fallback read vets
/// the directory the write vetted: a key kept somewhere that has stopped
/// being private is not handed back.
pub struct KeyringStore {
    keychain: Box<dyn Keychain>,
    /// Where an oversized secret lands; the CLI's own data directory
    /// resolution, so everything this machine keeps stays under one root.
    /// The `Err` is shown only if a too-long save actually needs the dir.
    fallback_dir: Result<PathBuf, String>,
}

impl KeyringStore {
    /// The production store: the system keychain, with the fallback beside
    /// the CLI's session state.
    pub fn system() -> Self {
        KeyringStore {
            keychain: Box::new(SystemKeychain),
            fallback_dir: jamstream_cli::state::data_dir()
                .map(|dir| dir.join("credentials"))
                .map_err(|e| e.to_string()),
        }
    }

    fn account(provider: &str, field: &str) -> String {
        format!("{provider}.{field}")
    }

    fn fallback_path(&self, provider: &str, field: &str) -> Result<PathBuf, String> {
        self.fallback_dir
            .as_ref()
            .map(|dir| dir.join(Self::account(provider, field)))
            .map_err(Clone::clone)
    }

    fn set_fallback(&self, provider: &str, field: &str, value: &str) -> Result<(), String> {
        let too_long = "this key is longer than the keychain allows";
        let path = self.fallback_path(provider, field).map_err(|e| {
            format!("{too_long}, and there is no private directory to keep it in instead: {e}")
        })?;
        let dir = self.fallback_dir.as_ref().expect("path implies dir");
        create_private_dir(dir)
            .and_then(|()| write_private(&path, value.as_bytes()))
            .map_err(|e| {
                format!(
                    "{too_long}, and keeping it as a private file failed: cannot write {}: {e}",
                    path.display()
                )
            })
    }

    /// The read half of [`KeyringStore::set_fallback`], vetted the way the
    /// write was. The secret that lands here in practice is a GCP service
    /// account key, and a directory that has stopped being private since
    /// the key was written is one somebody else can hand a key back from.
    ///
    /// `Ok(None)` is nothing saved here; `Err` is something saved that we
    /// will not read, which the caller logs. A refusal reads back as
    /// nothing saved either way, a state the wizard already explains.
    ///
    /// Vetting starts at the file, not at the directory: a machine that
    /// never saved a credential has no credentials directory, and vetting
    /// one that is not there turns "nothing saved" into a refusal for
    /// every key name in turn.
    fn read_fallback(path: &Path) -> std::io::Result<Option<String>> {
        if !path.exists() {
            return Ok(None);
        }
        match read_private(path) {
            Ok(bytes) => Ok(String::from_utf8(bytes).ok().filter(|v| !v.is_empty())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
}

impl CredStore for KeyringStore {
    fn get(&self, provider: &str, field: &str) -> Option<String> {
        self.keychain
            .get(&Self::account(provider, field))
            .filter(|v| !v.is_empty())
            .or_else(|| {
                let path = self.fallback_path(provider, field).ok()?;
                match Self::read_fallback(&path) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            path = %path.display(),
                            "refusing to read a saved credential"
                        );
                        None
                    }
                }
            })
    }

    fn set(&self, provider: &str, field: &str, value: &str) -> Result<(), String> {
        match self.keychain.set(&Self::account(provider, field), value) {
            Ok(()) => {
                // A shorter secret saved over an oversized one must not
                // leave the old one on disk under the fallback name, where
                // nothing would ever overwrite or read it again.
                if let Ok(path) = self.fallback_path(provider, field) {
                    let _ = std::fs::remove_file(path);
                }
                Ok(())
            }
            Err(keyring::Error::TooLong(..)) => {
                self.set_fallback(provider, field, value)?;
                // A refused save leaves the keychain's previous value in
                // place, and reads ask the keychain first; without this the
                // stale shorter secret would shadow the file just written.
                self.keychain.delete(&Self::account(provider, field));
                Ok(())
            }
            Err(e) => Err(format!("keychain: {e}")),
        }
    }

    fn delete(&self, provider: &str, field: &str) {
        self.keychain.delete(&Self::account(provider, field));
        if let Ok(path) = self.fallback_path(provider, field) {
            let _ = std::fs::remove_file(path);
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
    // The cloud crate's own parser, not a fifth table of the same four
    // spellings: `ProviderKind::as_str` is authoritative and the hand-written
    // matches kept drifting from it, so a fifth provider is a compile error in
    // the arms below rather than a wrong error message here (#233).
    let kind: ProviderKind = name
        .parse()
        .map_err(|e: jamstream_cloud::ProviderError| e.to_string())?;
    match kind {
        // Local spawns a process rather than opening a firewall, and takes its
        // port from the flat config the launch writes.
        ProviderKind::Local => {
            jamstream_cli::providers::resolve(kind.as_str()).map_err(|e| e.to_string())
        }
        ProviderKind::DigitalOcean => {
            let token = lookup(creds, env, DO_TOKEN, "DIGITALOCEAN_TOKEN")
                .ok_or("no DigitalOcean token saved and DIGITALOCEAN_TOKEN is not set")?;
            Ok(Box::new(
                DigitalOceanProvider::new(token).with_session_port(session_port),
            ))
        }
        ProviderKind::Aws => {
            let id = lookup(creds, env, AWS_ACCESS_KEY_ID, "AWS_ACCESS_KEY_ID")
                .ok_or("no AWS access key saved and AWS_ACCESS_KEY_ID is not set")?;
            let secret = lookup(creds, env, AWS_SECRET_ACCESS_KEY, "AWS_SECRET_ACCESS_KEY")
                .ok_or("no AWS secret key saved and AWS_SECRET_ACCESS_KEY is not set")?;
            Ok(Box::new(
                AwsProvider::new(id, secret).with_session_port(session_port),
            ))
        }
        ProviderKind::Gcp => build_gcp(session_port, creds, env),
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

    /// Every name the app builds a provider from is a `ProviderKind` name, and
    /// a name that is not one is refused by the cloud crate's own parser, whose
    /// message lists what it would have taken. The hand-written match this
    /// replaced answered `unknown provider "azure"` and named nothing.
    #[test]
    fn the_names_are_the_provider_kinds_and_nothing_else_parses() {
        let (store, env) = (MemStore::default(), env_of(&[]));
        for kind in ProviderKind::ALL {
            // Every one of them gets past the parse: local builds, the clouds
            // reach their own credential complaint rather than a name error.
            let result = build_provider(kind.as_str(), &store, &env);
            if let Err(err) = &result {
                assert!(
                    !err.contains("unknown provider"),
                    "{kind} did not parse: {err}"
                );
            }
        }
        let err = build_provider("azure", &store, &env)
            .err()
            .expect("azure is not a provider");
        assert!(err.contains("azure"), "error was {err:?}");
        for kind in ProviderKind::ALL {
            assert!(
                err.contains(kind.as_str()),
                "the refusal does not name {kind}: {err:?}"
            );
        }
        // The test-only mock is resolvable in the CLI and is deliberately not
        // one of these, so the app cannot launch a session onto nothing.
        assert!(build_provider("mock", &store, &env).is_err());
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

    /// The Windows Credential Manager blob cap: `CRED_MAX_CREDENTIAL_BLOB_SIZE`
    /// bytes, which the password fills as UTF-16.
    const WINDOWS_BLOB_CAP_BYTES: usize = 2560;

    /// A keychain that refuses what keyring's windows-native backend
    /// refuses: a password whose UTF-16 encoding exceeds the blob cap. The
    /// check and the error are the backend's own, so the fallback is tested
    /// against the refusal production actually makes, including the part
    /// where a refused save leaves the previous value in place.
    #[derive(Default)]
    struct CappedKeychain {
        values: Mutex<HashMap<String, String>>,
    }

    impl Keychain for CappedKeychain {
        fn get(&self, account: &str) -> Option<String> {
            self.values.lock().expect("keychain").get(account).cloned()
        }

        fn set(&self, account: &str, value: &str) -> Result<(), keyring::Error> {
            if value.encode_utf16().count() * 2 > WINDOWS_BLOB_CAP_BYTES {
                return Err(keyring::Error::TooLong(
                    "password encoded as UTF-16".to_owned(),
                    WINDOWS_BLOB_CAP_BYTES as u32,
                ));
            }
            self.values
                .lock()
                .expect("keychain")
                .insert(account.to_owned(), value.to_owned());
            Ok(())
        }

        fn delete(&self, account: &str) {
            self.values.lock().expect("keychain").remove(account);
        }
    }

    /// A store over the capped keychain, with the fallback under a fresh
    /// temp path. The directory is not pre-created: whether it exists is
    /// itself an assertion.
    fn capped_store(label: &str) -> (KeyringStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "jamstream-creds-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = KeyringStore {
            keychain: Box::new(CappedKeychain::default()),
            fallback_dir: Ok(dir.clone()),
        };
        (store, dir)
    }

    #[cfg(unix)]
    fn mode_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777
    }

    /// A GCP service account key sized secret: the keychain refuses it, the
    /// file takes it, and set/get/delete behave as if the keychain had.
    #[test]
    fn a_secret_the_keychain_refuses_round_trips_through_a_private_file() {
        let (store, dir) = capped_store("oversized");
        let (provider, field) = GCP_SERVICE_ACCOUNT_JSON;
        let secret = "x".repeat(2500);
        store.set(provider, field, &secret).expect("set");

        let path = dir.join("gcp.service_account_json");
        assert!(path.is_file(), "the fallback file was not written");
        assert_eq!(
            store.keychain.get("gcp.service_account_json"),
            None,
            "the keychain must hold nothing for a slot the file owns"
        );
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&dir), 0o700);
            assert_eq!(mode_of(&path), 0o600);
        }
        assert_eq!(store.get(provider, field), Some(secret));

        store.delete(provider, field);
        assert!(!path.exists(), "delete left the file behind");
        assert_eq!(store.get(provider, field), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The directory is vetted on the way out too. The file it guards is
    /// the GCP service account key on the machines that reach the fallback
    /// at all, and once the directory is writable by everyone the key that
    /// reads back is whoever's key was put there last.
    #[cfg(unix)]
    #[test]
    fn a_fallback_directory_that_stopped_being_private_is_not_read_back() {
        use std::os::unix::fs::PermissionsExt as _;
        let (store, dir) = capped_store("loosened");
        let (provider, field) = GCP_SERVICE_ACCOUNT_JSON;
        let secret = "x".repeat(2500);
        store.set(provider, field, &secret).expect("set");
        assert_eq!(store.get(provider, field).as_deref(), Some(secret.as_str()));

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        assert!(
            dir.join("gcp.service_account_json").is_file(),
            "the file has to still be there for the refusal to mean anything"
        );
        assert_eq!(
            store.get(provider, field),
            None,
            "a key read back out of a world writable directory"
        );
        assert!(
            KeyringStore::read_fallback(&dir.join("gcp.service_account_json")).is_err(),
            "a directory that cannot be trusted is a refusal, not an empty slot"
        );

        // Tightened again, it is the same key it always was.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        assert_eq!(store.get(provider, field).as_deref(), Some(secret.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A joiner has no cloud credentials and never will, so their machine
    /// has no credentials directory either. Every slot has to read back as
    /// nothing saved, silently: reporting a directory that was never
    /// created as one whose permissions we refuse to trust put nine
    /// warnings at the top of a Windows guest's log, in a file that asks
    /// for anything in it to be reported as a bug (#461).
    #[test]
    fn no_credentials_directory_is_nothing_saved_rather_than_a_refusal() {
        let (store, dir) = capped_store("absent");
        assert!(!dir.exists(), "the fixture starts with no directory");
        for (provider, field) in [
            DO_TOKEN,
            AWS_ACCESS_KEY_ID,
            AWS_SECRET_ACCESS_KEY,
            GCP_SERVICE_ACCOUNT_JSON,
        ] {
            let path = dir.join(KeyringStore::account(provider, field));
            assert_eq!(
                KeyringStore::read_fallback(&path).expect("an absent path is not a refusal"),
                None
            );
            assert_eq!(store.get(provider, field), None);
        }
        assert!(
            !dir.exists(),
            "looking for a credential created the directory"
        );
    }

    /// A secret the keychain takes never reaches the disk: not the file,
    /// not even the directory.
    #[test]
    fn a_keychain_sized_secret_never_touches_the_file_path() {
        let (store, dir) = capped_store("small");
        store.set(DO_TOKEN.0, DO_TOKEN.1, "dop_v1_x").expect("set");
        assert!(!dir.exists(), "a fitting secret created the fallback dir");
        assert_eq!(
            store.get(DO_TOKEN.0, DO_TOKEN.1).as_deref(),
            Some("dop_v1_x")
        );
        store.delete(DO_TOKEN.0, DO_TOKEN.1);
        assert_eq!(store.get(DO_TOKEN.0, DO_TOKEN.1), None);
    }

    /// The boundary is the backend's: 1280 ASCII characters is the last
    /// password that fits, one more falls back. And because a refused save
    /// leaves the keychain's old value in place, the fallback has to evict
    /// it or every later read would return the stale shorter secret.
    #[test]
    fn the_fallback_starts_exactly_past_the_blob_cap_and_evicts_the_stale_entry() {
        let (store, dir) = capped_store("boundary");
        let (provider, field) = GCP_SERVICE_ACCOUNT_JSON;
        store.set(provider, field, &"a".repeat(1280)).expect("set");
        assert!(!dir.exists(), "1280 chars fits the cap");

        let grown = "b".repeat(1281);
        store.set(provider, field, &grown).expect("set");
        assert!(dir.join("gcp.service_account_json").is_file());
        assert_eq!(
            store.get(provider, field),
            Some(grown),
            "the keychain's stale value shadowed the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reverse move: a shorter secret saved over an oversized one lands
    /// in the keychain and must take the file with it, or a credential
    /// nothing will ever read again stays on disk.
    #[test]
    fn a_shorter_secret_saved_over_an_oversized_one_removes_the_file() {
        let (store, dir) = capped_store("shrink");
        let (provider, field) = GCP_SERVICE_ACCOUNT_JSON;
        store.set(provider, field, &"x".repeat(2500)).expect("set");
        let path = dir.join("gcp.service_account_json");
        assert!(path.is_file());

        store.set(provider, field, "small").expect("set");
        assert!(!path.exists(), "the oversized copy stayed on disk");
        assert_eq!(store.get(provider, field).as_deref(), Some("small"));
        let _ = std::fs::remove_dir_all(&dir);
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
