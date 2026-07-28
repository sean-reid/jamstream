//! Where the storage key comes from, and how a session's bucket is opened.
//!
//! Recording needs a second credential: the object stores speak SigV4 with an
//! access key pair, which is not the DigitalOcean API token that launches
//! droplets and not the GCP service account either. It lives in the
//! environment, like every other credential this CLI uses, so the machine
//! that launched a session can read its takes back with nothing new
//! configured, and a copied state directory grants no bucket access.

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

/// The two variables one provider's storage key pair lives in.
pub fn credential_vars(provider: ProviderKind) -> Result<(&'static str, &'static str), CliError> {
    match provider {
        ProviderKind::Aws => Ok(("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY")),
        ProviderKind::DigitalOcean => Ok(("SPACES_ACCESS_KEY_ID", "SPACES_SECRET_ACCESS_KEY")),
        ProviderKind::Gcp => Ok(("GCS_ACCESS_KEY_ID", "GCS_SECRET_ACCESS_KEY")),
        ProviderKind::Local => Err(CliError::Usage(
            "a local session records to this computer's disk, which needs no key".to_owned(),
        )),
    }
}

/// The storage key pair for one provider, from this process's environment.
pub fn credential_from_env(provider: ProviderKind) -> Result<StorageCredential, CliError> {
    credential_from(provider, |key| std::env::var(key).ok())
}

/// [`credential_from_env`] with the lookup supplied, so the messages are
/// testable without setting a variable every test in the process would share.
pub fn credential_from(
    provider: ProviderKind,
    get: impl Fn(&str) -> Option<String>,
) -> Result<StorageCredential, CliError> {
    let (id_var, secret_var) = credential_vars(provider)?;
    let read = |key: &str| {
        get(key)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| CliError::Usage(missing_key(provider, key, id_var, secret_var)))
    };
    Ok(StorageCredential::KeyPair {
        access_key_id: read(id_var)?,
        secret_access_key: read(secret_var)?,
    })
}

/// The mistake everybody makes once is pasting the token that launches
/// machines, so each provider's message says what the key is not.
fn missing_key(provider: ProviderKind, missing: &str, id_var: &str, secret_var: &str) -> String {
    let aside = match provider {
        ProviderKind::DigitalOcean => {
            " Spaces does not accept DIGITALOCEAN_TOKEN: generate a Spaces access key under \
             API > Spaces Keys."
        }
        ProviderKind::Gcp => {
            " Cloud Storage takes an HMAC key here, not a service account: create one under \
             Cloud Storage > Settings > Interoperability."
        }
        _ => "",
    };
    format!(
        "{missing} is not set, so this machine has no key for the {} bucket. Set {id_var} and \
         {secret_var}.{aside}",
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
    fn every_storage_provider_names_its_two_variables() {
        for name in STORAGE_PROVIDERS {
            let kind = provider_kind(name).unwrap();
            let (id, secret) = credential_vars(kind).unwrap();
            assert!(id.ends_with("ACCESS_KEY_ID"), "{name}: {id}");
            assert!(secret.ends_with("SECRET_ACCESS_KEY"), "{name}: {secret}");
            let credential = credential_from(kind, |key| Some(format!("{key}-value"))).unwrap();
            let StorageCredential::KeyPair {
                access_key_id,
                secret_access_key,
            } = credential;
            assert_eq!(access_key_id, format!("{id}-value"));
            assert_eq!(secret_access_key, format!("{secret}-value"));
        }
    }

    #[test]
    fn a_missing_key_names_both_variables_and_what_the_key_is_not() {
        let err = credential_from(ProviderKind::DigitalOcean, |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("SPACES_ACCESS_KEY_ID"), "{err}");
        assert!(err.contains("SPACES_SECRET_ACCESS_KEY"), "{err}");
        assert!(err.contains("DIGITALOCEAN_TOKEN"), "{err}");

        let err = credential_from(ProviderKind::Gcp, |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("HMAC"), "{err}");

        // An empty variable is not a credential, and a secret without an id
        // fails on the id.
        assert!(credential_from(ProviderKind::Aws, |_| Some(String::new())).is_err());
        let err = credential_from(ProviderKind::Aws, |key| {
            (key == "AWS_SECRET_ACCESS_KEY").then(|| "s".to_owned())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("AWS_ACCESS_KEY_ID"), "{err}");
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
