//! The server artifact pinned into this build at compile time.
//!
//! Release builds are compiled with `JAMSTREAM_SERVER_URL` and
//! `JAMSTREAM_SERVER_SHA256` in the environment (release.yml computes both
//! in its server-musl job and exports them into every client and CLI
//! build), so [`pinned`] returns the exact `jamstreamd` download the
//! release published, and cloud hosting works without the user ever seeing
//! an artifact URL or hash. Development builds have neither variable set
//! and get `None`; there the CLI's `--artifact-url`/`--artifact-sha256`
//! overrides are the only way to host on a cloud provider.
//!
//! `option_env!` is read when THIS crate compiles; cargo tracks the env
//! dependency, so a build with the variables set recompiles the crate.
//! That also means an already-compiled test binary cannot exercise the
//! `Some` case: the unit tests here cover the `None` case (dev builds) and
//! the validation helper on a sample pair, while the `Some` case is proven
//! by the release pipeline itself, whose cloud launches would fail without
//! a valid pinned pair.

/// A `jamstreamd` download baked into the binary at compile time: the URL
/// the session VM fetches at boot and the sha256 it must match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedServerArtifact {
    pub url: &'static str,
    pub sha256: &'static str,
}

/// The server artifact pinned into this build, or `None` for development
/// builds. Both environment variables must be present and well formed
/// (non-empty URL, 64 hex character sha256); a half-set or malformed pair
/// yields `None` rather than a panic, with a `debug_assert` to catch a
/// misconfigured pipeline in debug builds.
pub fn pinned() -> Option<PinnedServerArtifact> {
    let url = option_env!("JAMSTREAM_SERVER_URL");
    let sha256 = option_env!("JAMSTREAM_SERVER_SHA256");
    debug_assert!(
        url.is_some() == sha256.is_some(),
        "JAMSTREAM_SERVER_URL and JAMSTREAM_SERVER_SHA256 must be set together"
    );
    match (url, sha256) {
        (Some(url), Some(sha256)) if is_valid_pair(url, sha256) => {
            Some(PinnedServerArtifact { url, sha256 })
        }
        (Some(url), Some(sha256)) => {
            debug_assert!(
                false,
                "pinned server artifact is malformed: url={url:?} sha256={sha256:?}"
            );
            None
        }
        _ => None,
    }
}

/// A usable pair: a non-empty URL and a sha256 of exactly 64 hex digits.
fn is_valid_pair(url: &str, sha256: &str) -> bool {
    !url.is_empty() && sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_builds_have_no_pinned_artifact() {
        // This test binary was compiled without JAMSTREAM_SERVER_URL /
        // JAMSTREAM_SERVER_SHA256; setting them at run time cannot change
        // an already-compiled option_env!, which is exactly the guarantee
        // dev builds rely on.
        assert_eq!(pinned(), None);
    }

    #[test]
    fn a_release_shaped_pair_validates() {
        // The Some case cannot be reached from an already-compiled test
        // binary; this covers the validation the release pair must pass.
        let url = "https://github.com/sean-reid/jamstream/releases/download/v0.1.0/jamstreamd-linux-x86_64-musl";
        let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert!(is_valid_pair(url, sha));
        assert!(is_valid_pair(url, &sha.to_uppercase()));
    }

    #[test]
    fn malformed_pairs_are_rejected() {
        let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert!(!is_valid_pair("", sha));
        assert!(!is_valid_pair("https://example.com/jamstreamd", "abc123"));
        assert!(!is_valid_pair(
            "https://example.com/jamstreamd",
            &"g".repeat(64) // right length, not hex
        ));
        assert!(!is_valid_pair(
            "https://example.com/jamstreamd",
            &sha[..63] // one digit short
        ));
    }
}
