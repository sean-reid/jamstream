//! The seam where concrete cloud providers plug in. `resolve` is the only
//! place the CLI maps a provider name to an implementation. Tests bypass
//! this and inject a provider directly into the command functions.

use jamstream_cloud::providers::aws::AwsProvider;
use jamstream_cloud::providers::digitalocean::DigitalOceanProvider;
use jamstream_cloud::providers::gcp::GcpProvider;
use jamstream_cloud::{MockProvider, Provider, ProviderKind};

use crate::CliError;

/// Every provider name this build recognizes.
pub const KNOWN_PROVIDERS: &[&str] = &["mock", "aws", "digitalocean", "gcp"];

pub fn resolve(name: &str) -> Result<Box<dyn Provider>, CliError> {
    match name {
        // The mock needs some underlying kind for its instances; Aws is
        // arbitrary and never leaves the process.
        "mock" => Ok(Box::new(MockProvider::with_default_regions(
            ProviderKind::Aws,
        ))),
        "aws" => AwsProvider::from_env()
            .map(boxed)
            .map_err(|err| creds_error("aws", &err.to_string())),
        "digitalocean" => DigitalOceanProvider::from_env()
            .map(boxed)
            .map_err(|err| creds_error("digitalocean", &err.to_string())),
        "gcp" => GcpProvider::from_env()
            .map(boxed)
            .map_err(|err| creds_error("gcp", &err.to_string())),
        other => Err(CliError::Usage(format!(
            "unknown provider {other:?}; known providers are mock, aws, digitalocean, gcp"
        ))),
    }
}

/// Every provider whose credentials are present in the environment, plus
/// the mock. Sweep runs across all of them.
pub fn resolve_all() -> Vec<Box<dyn Provider>> {
    KNOWN_PROVIDERS
        .iter()
        .filter_map(|name| resolve(name).ok())
        .collect()
}

fn boxed<P: Provider + 'static>(p: P) -> Box<dyn Provider> {
    Box::new(p)
}

fn creds_error(name: &str, detail: &str) -> CliError {
    CliError::Usage(format!(
        "provider {name}: {detail}. Use --provider mock to try the flow without credentials."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_resolves() {
        let p = resolve("mock").unwrap();
        assert_eq!(p.regions().len(), 2);
    }

    // These read the real process environment, so they assert consistency
    // with it rather than a fixed outcome.
    #[test]
    fn real_providers_track_env_credentials() {
        for (name, var) in [
            ("aws", "AWS_ACCESS_KEY_ID"),
            ("digitalocean", "DIGITALOCEAN_TOKEN"),
        ] {
            let resolved = resolve(name);
            if std::env::var(var).is_ok_and(|v| !v.is_empty()) {
                assert!(resolved.is_ok(), "{name} creds present but resolve failed");
            } else {
                let err = resolved.err().expect("must not resolve").to_string();
                assert!(err.contains(var), "error for {name} was {err:?}");
                assert!(err.contains("mock"));
            }
        }
    }

    #[test]
    fn unknown_provider_lists_the_known_ones() {
        let err = resolve("azure")
            .err()
            .expect("must not resolve")
            .to_string();
        assert!(err.contains("azure"));
        assert!(err.contains("mock, aws, digitalocean, gcp"));
    }

    #[test]
    fn resolve_all_always_includes_the_mock() {
        assert!(!resolve_all().is_empty());
    }
}
