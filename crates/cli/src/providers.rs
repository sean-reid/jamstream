//! The seam where concrete cloud providers plug in. `resolve` is the only
//! place the CLI maps a provider name to an implementation. Tests bypass
//! this and inject a provider directly into the command functions.

use std::path::PathBuf;

use jamstream_cloud::providers::aws::AwsProvider;
use jamstream_cloud::providers::digitalocean::DigitalOceanProvider;
use jamstream_cloud::providers::gcp::GcpProvider;
use jamstream_cloud::providers::local::LocalProvider;
use jamstream_cloud::{DEFAULT_SESSION_PORT, MockProvider, Provider, ProviderKind};

use crate::CliError;
use crate::state;

/// Every provider name this build recognizes. The mock is last and stays
/// out of help text and error messages; it exists for tests and launches
/// nothing real.
pub const KNOWN_PROVIDERS: &[&str] = &["local", "digitalocean", "aws", "gcp", "mock"];

/// Where the local provider keeps its process registry and per-session
/// server configs: the same JAMSTREAM_STATE_DIR override the session state
/// files honor (see state.rs), else the platform data directory.
fn local_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(state::STATE_DIR_ENV) {
        return PathBuf::from(dir);
    }
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("jamstream")
}

pub fn resolve(name: &str) -> Result<Box<dyn Provider>, CliError> {
    resolve_for_port(name, DEFAULT_SESSION_PORT)
}

/// [`resolve`] for a host that picked its own session port. The port is the
/// only one the provider opens in the firewall it creates for the session,
/// so `jamstream host --port` has to reach the provider or the VM comes up
/// behind a firewall for a different port.
pub fn resolve_for_port(name: &str, session_port: u16) -> Result<Box<dyn Provider>, CliError> {
    match name {
        "local" => Ok(Box::new(LocalProvider::new(local_state_dir()))),
        // The mock needs some underlying kind for its instances; Aws is
        // arbitrary and never leaves the process.
        "mock" => Ok(Box::new(
            MockProvider::with_default_regions(ProviderKind::Aws).with_session_port(session_port),
        )),
        "aws" => AwsProvider::from_env()
            .map(|p| boxed(p.with_session_port(session_port)))
            .map_err(|err| creds_error("aws", &err.to_string())),
        "digitalocean" => DigitalOceanProvider::from_env()
            .map(|p| boxed(p.with_session_port(session_port)))
            .map_err(|err| creds_error("digitalocean", &err.to_string())),
        "gcp" => GcpProvider::from_env()
            .map(|p| boxed(p.with_session_port(session_port)))
            .map_err(|err| creds_error("gcp", &err.to_string())),
        other => Err(CliError::Usage(format!(
            "unknown provider {other:?}; known providers are local, digitalocean, aws, gcp"
        ))),
    }
}

/// Every provider whose credentials are present in the environment, plus
/// local and the mock, which need none. Sweep runs across all of them, so
/// stray local servers are found like any cloud stray.
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
        "provider {name}: {detail}. Use --provider local to host on this computer \
         without credentials."
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

    // Local needs no credentials: it must resolve unconditionally, with
    // exactly one region, this computer.
    #[test]
    fn local_resolves_without_credentials() {
        let p = resolve("local").unwrap();
        assert_eq!(p.kind(), ProviderKind::Local);
        let regions = p.regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].id.as_str(), "local");
    }

    #[test]
    fn local_state_dir_honors_the_env_override() {
        // Read-only consistency check against the live environment, like
        // real_providers_track_env_credentials below.
        match std::env::var_os(state::STATE_DIR_ENV) {
            Some(dir) => assert_eq!(local_state_dir(), PathBuf::from(dir)),
            None => assert!(local_state_dir().ends_with("jamstream")),
        }
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
                assert!(err.contains("local"));
            }
        }
    }

    // The error names the user-facing providers and keeps quiet about the
    // test-only mock.
    #[test]
    fn unknown_provider_lists_the_known_ones() {
        let err = resolve("azure")
            .err()
            .expect("must not resolve")
            .to_string();
        assert!(err.contains("azure"));
        assert!(err.contains("local, digitalocean, aws, gcp"));
        assert!(!err.contains("mock"));
    }

    #[test]
    fn resolve_all_always_includes_local_and_the_mock() {
        let all = resolve_all();
        assert!(all.iter().any(|p| p.kind() == ProviderKind::Local));
        assert!(all.len() >= 2, "local and the mock resolve with no creds");
    }
}
