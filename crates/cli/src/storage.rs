//! Where the storage key comes from, and how a session's bucket is opened.
//!
//! Recording needs a second credential: the object stores speak SigV4 with an
//! access key pair, which is not the DigitalOcean API token that launches
//! droplets and not the GCP service account either. It lives in the
//! environment, like every other credential this CLI uses, so the machine
//! that launched a session can read its takes back with nothing new
//! configured, and a copied state directory grants no bucket access.
//!
//! # Why it has variables of its own
//!
//! This key is written into the session machine's user data, and the process
//! that reads it there parses unauthenticated UDP from the internet. On AWS the
//! obvious names for an S3 key, `AWS_ACCESS_KEY_ID` and
//! `AWS_SECRET_ACCESS_KEY`, are the pair that calls `ec2:RunInstances`, so
//! reading them here would hand a VM the credential that can launch and destroy
//! machines in the whole account, whatever the docs said about scoping it to one
//! prefix. [`RECORDING_VARS`] is therefore the first pair read on every
//! provider, and the AWS launch pair is not read at all: a key that cannot be
//! expressed separately cannot be scoped.

use std::sync::Arc;

use jamstream_cloud::cloudinit::{RecordingStorage, StorageCredential};
use jamstream_cloud::{ObjectStore, ProviderKind, RegionId, Retention};

use crate::CliError;
use crate::state::RecordingRecord;

/// The provider names that can hold a recording bucket.
pub const STORAGE_PROVIDERS: &[&str] = &["aws", "digitalocean", "gcp"];

/// Parses a provider name into the kind that decides which endpoint gets
/// signed.
pub fn provider_kind(name: &str) -> Result<ProviderKind, CliError> {
    match name {
        "aws" => Ok(ProviderKind::Aws),
        "digitalocean" => Ok(ProviderKind::DigitalOcean),
        "gcp" => Ok(ProviderKind::Gcp),
        "local" => Err(CliError::Usage(
            "a local session records to this computer's disk, which needs no bucket".to_owned(),
        )),
        other => Err(CliError::Usage(format!(
            "provider {other:?} has no recording storage; buckets live on {}",
            STORAGE_PROVIDERS.join(", ")
        ))),
    }
}

/// The recording key's own variables, read first on every provider.
///
/// One name for all three, because this credential's job is the same
/// everywhere: write one bucket prefix. See the module docs for why it does not
/// share a provider's launch variables.
pub const RECORDING_VARS: (&str, &str) = (
    "JAMSTREAM_RECORDING_ACCESS_KEY_ID",
    "JAMSTREAM_RECORDING_SECRET_ACCESS_KEY",
);

/// Every variable pair one provider's storage key may come from, in the order
/// they are read: the recording pair, then the S3-style name that provider's own
/// console documents.
///
/// AWS has no second pair. `AWS_ACCESS_KEY_ID` is the key that launches
/// instances, and this key rides on the machine.
pub fn credential_var_pairs(
    provider: ProviderKind,
) -> Result<Vec<(&'static str, &'static str)>, CliError> {
    let own = match provider {
        ProviderKind::Aws => None,
        ProviderKind::DigitalOcean => Some(("SPACES_ACCESS_KEY_ID", "SPACES_SECRET_ACCESS_KEY")),
        ProviderKind::Gcp => Some(("GCS_ACCESS_KEY_ID", "GCS_SECRET_ACCESS_KEY")),
        ProviderKind::Local => {
            return Err(CliError::Usage(
                "a local session records to this computer's disk, which needs no key".to_owned(),
            ));
        }
    };
    Ok(std::iter::once(RECORDING_VARS).chain(own).collect())
}

/// The pair to name when a key is missing: the recording variables, which work
/// on every provider.
pub fn credential_vars(provider: ProviderKind) -> Result<(&'static str, &'static str), CliError> {
    credential_var_pairs(provider).map(|pairs| pairs[0])
}

/// The storage key pair for one provider, from this process's environment.
pub fn credential_from_env(provider: ProviderKind) -> Result<StorageCredential, CliError> {
    credential_from(provider, |key| std::env::var(key).ok())
}

/// [`credential_from_env`] with the lookup supplied, so the messages are
/// testable without setting a variable every test in the process would share.
///
/// A pair is taken only when both halves of it are set, so half a recording key
/// never silently completes itself from another pair.
pub fn credential_from(
    provider: ProviderKind,
    get: impl Fn(&str) -> Option<String>,
) -> Result<StorageCredential, CliError> {
    let read = |key: &str| get(key).filter(|value| !value.is_empty());
    for (id_var, secret_var) in credential_var_pairs(provider)? {
        if let (Some(access_key_id), Some(secret_access_key)) = (read(id_var), read(secret_var)) {
            return Ok(StorageCredential::KeyPair {
                access_key_id,
                secret_access_key,
            });
        }
    }
    Err(CliError::Usage(missing_key(provider, &read)))
}

/// Why there is no key, and what to set. Names every pair that would work, and
/// on AWS says plainly that the launch pair is not one of them: the mistake
/// everybody makes once is reaching for the credential they already have.
fn missing_key(provider: ProviderKind, read: &impl Fn(&str) -> Option<String>) -> String {
    let (id_var, secret_var) = RECORDING_VARS;
    let aside = match provider {
        ProviderKind::Aws if read("AWS_ACCESS_KEY_ID").is_some() => {
            " AWS_ACCESS_KEY_ID is set and is deliberately not used here: it is the key that \
             launches instances, and the recording key is written to the session machine, so \
             create a second key scoped to writing the bucket."
        }
        ProviderKind::Aws => {
            " Not your launch key: this one is written to the session machine, so scope it to \
             writing one bucket."
        }
        ProviderKind::DigitalOcean => {
            " Spaces does not accept DIGITALOCEAN_TOKEN: generate a Spaces access key under \
             API > Spaces Keys. SPACES_ACCESS_KEY_ID and SPACES_SECRET_ACCESS_KEY work too."
        }
        ProviderKind::Gcp => {
            " Cloud Storage takes an HMAC key here, not a service account: create one under \
             Cloud Storage > Settings > Interoperability. GCS_ACCESS_KEY_ID and \
             GCS_SECRET_ACCESS_KEY work too."
        }
        ProviderKind::Local => "",
    };
    format!(
        "this machine has no storage key for the {} bucket. Set {id_var} and {secret_var}.{aside}",
        provider.as_str()
    )
}

/// The storage config for one session's bucket, credential included.
pub fn storage_for(record: &RecordingRecord) -> Result<RecordingStorage, CliError> {
    let provider = provider_kind(&record.provider)?;
    Ok(RecordingStorage {
        provider,
        bucket: record.bucket.clone(),
        region: record.region.clone(),
        retention: record.retention.parse().unwrap_or_default(),
        credential: credential_from_env(provider)?,
        stems: record.stems,
    })
}

/// The storage config a launch carries to the VM: the bucket in the session's
/// own region, with the key that writes it.
///
/// Both launch surfaces build one through here, so the empty-bucket refusal and
/// the price check are the same wherever a session is armed.
///
/// The key is read last, and only once the bucket is worth having one for: a
/// region with no bucket service has to say so rather than naming a variable
/// that would not have helped.
pub fn storage_for_launch(
    provider: ProviderKind,
    bucket: &str,
    region: &RegionId,
    retention: Retention,
    credential: impl FnOnce() -> Result<StorageCredential, CliError>,
    stems: bool,
) -> Result<RecordingStorage, CliError> {
    if bucket.trim().is_empty() {
        return Err(CliError::Usage(
            "the bucket name is empty; recording needs a bucket to write to".to_owned(),
        ));
    }
    // Priced here so a region with no bucket service (a DigitalOcean region
    // with no Spaces endpoint) is refused before the launch rather than at the
    // first upload.
    jamstream_cloud::storage_price(provider, region)?;
    Ok(RecordingStorage {
        provider,
        bucket: bucket.trim().to_owned(),
        region: region.to_string(),
        retention,
        credential: credential()?,
        stems,
    })
}

/// What a session's bucket details are turned into to reach the bucket.
///
/// A seam, not a layer: the integration tests hand `recordings` a factory
/// pointing at a wiremock server, so the listing and download run through the
/// real S3 client rather than a stand-in for it.
pub trait Stores {
    fn open(&self, record: &RecordingRecord) -> Result<Arc<dyn ObjectStore>, CliError>;
}

/// The real thing: provider, region, and bucket from the session record, key
/// from the environment.
pub struct EnvStores;

impl Stores for EnvStores {
    fn open(&self, record: &RecordingRecord) -> Result<Arc<dyn ObjectStore>, CliError> {
        Ok(storage_for(record)?.object_store()?)
    }
}

/// How the record spells one retention choice.
pub fn retention_label(record: &RecordingRecord) -> String {
    record
        .retention
        .parse::<Retention>()
        .map(|r| r.label().to_owned())
        .unwrap_or_else(|_| record.retention.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_storage_provider_reads_the_recording_pair_first() {
        for name in STORAGE_PROVIDERS {
            let kind = provider_kind(name).unwrap();
            let pairs = credential_var_pairs(kind).unwrap();
            assert_eq!(pairs[0], RECORDING_VARS, "{name} must take its own pair");
            for (id, secret) in &pairs {
                assert!(id.ends_with("ACCESS_KEY_ID"), "{name}: {id}");
                assert!(secret.ends_with("SECRET_ACCESS_KEY"), "{name}: {secret}");
            }
            let credential = credential_from(kind, |key| Some(format!("{key}-value"))).unwrap();
            let StorageCredential::KeyPair {
                access_key_id,
                secret_access_key,
            } = credential;
            assert_eq!(access_key_id, format!("{}-value", RECORDING_VARS.0));
            assert_eq!(secret_access_key, format!("{}-value", RECORDING_VARS.1));
        }
    }

    /// The defect this shape exists to prevent: on AWS the obvious names for an
    /// S3 key are the pair that launches instances, and this key is written to
    /// the session machine. It must not be readable from there, and the refusal
    /// has to say why rather than leaving a host to guess.
    #[test]
    fn the_aws_launch_pair_is_never_read_as_a_recording_key() {
        let launch_pair = |key: &str| {
            matches!(key, "AWS_ACCESS_KEY_ID" | "AWS_SECRET_ACCESS_KEY").then(|| "AKIA".to_owned())
        };
        let err = credential_from(ProviderKind::Aws, launch_pair)
            .expect_err("the launch pair must not become a recording key")
            .to_string();
        assert!(err.contains(RECORDING_VARS.0), "{err}");
        assert!(err.contains("launches instances"), "{err}");
        assert!(err.contains("written to the session machine"), "{err}");
        assert!(
            !credential_var_pairs(ProviderKind::Aws)
                .unwrap()
                .iter()
                .any(|(id, _)| *id == "AWS_ACCESS_KEY_ID"),
            "the launch pair must not be in the read order at all"
        );
    }

    #[test]
    fn a_missing_key_names_the_variables_and_what_the_key_is_not() {
        let err = credential_from(ProviderKind::DigitalOcean, |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains(RECORDING_VARS.0), "{err}");
        assert!(err.contains("SPACES_ACCESS_KEY_ID"), "{err}");
        assert!(err.contains("DIGITALOCEAN_TOKEN"), "{err}");

        let err = credential_from(ProviderKind::Gcp, |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("HMAC"), "{err}");

        // A provider's own pair still works, so a host who set it up before the
        // recording pair existed keeps working.
        let spaces = credential_from(ProviderKind::DigitalOcean, |key| {
            key.starts_with("SPACES_").then(|| "s".to_owned())
        })
        .expect("the Spaces pair is a recording key");
        let StorageCredential::KeyPair { access_key_id, .. } = spaces;
        assert_eq!(access_key_id, "s");

        // An empty variable is not a credential, and half a pair is not one
        // either: it must not complete itself from the other pair.
        assert!(credential_from(ProviderKind::Aws, |_| Some(String::new())).is_err());
        let err = credential_from(ProviderKind::Gcp, |key| {
            (key == RECORDING_VARS.1).then(|| "s".to_owned())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains(RECORDING_VARS.0), "{err}");
    }

    #[test]
    fn local_has_no_bucket_and_an_unknown_provider_lists_the_real_ones() {
        let err = provider_kind("local").unwrap_err().to_string();
        assert!(err.contains("this computer's disk"), "{err}");
        let err = provider_kind("azure").unwrap_err().to_string();
        assert!(err.contains("aws, digitalocean, gcp"), "{err}");
        assert!(credential_vars(ProviderKind::Local).is_err());
    }

    #[test]
    fn a_records_retention_reads_back_as_the_choice_it_was() {
        let mut record = RecordingRecord {
            provider: "aws".to_owned(),
            bucket: "b".to_owned(),
            region: "eu-west-1".to_owned(),
            retention: Retention::Days90.to_string(),
            stems: false,
        };
        assert_eq!(retention_label(&record), "Delete after 90 days");
        // Anything unparseable is shown verbatim rather than guessed at.
        record.retention = "whenever".to_owned();
        assert_eq!(retention_label(&record), "whenever");
    }
}
