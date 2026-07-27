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

fn is_valid_pair(url: &str, sha256: &str) -> bool {
    validate_pair(url, sha256).is_ok()
}

/// Checks an artifact pair, whether it came from the compile-time pin or
/// from a caller. The error names the problem, because the CLI hands it
/// straight to the user.
///
/// A sha256 is exactly 64 hex digits: anything else fails on the VM at
/// `sha256sum -c`, an hour of nobody's time after the launch was paid for.
///
/// The URL must be https and must be made of URL characters. That rules
/// out `"`, `$`, a backtick, and a backslash, which are the four a shell
/// still reads inside a double-quoted string, and it leaves the query
/// syntax a presigned URL needs. The bootstrap script no longer
/// interpolates either value, but refusing the characters costs nothing
/// and does not depend on that staying true. Plain http is refused as
/// well: the hash would catch a rewritten download, but it catches it by
/// destroying the VM, and nothing is gained by fetching the server over a
/// channel anyone on the path can rewrite.
pub fn validate_pair(url: &str, sha256: &str) -> Result<(), String> {
    if !url.starts_with("https://") || url.len() <= "https://".len() {
        return Err(format!(
            "artifact url {url:?} must be an https:// address of a jamstreamd build"
        ));
    }
    if let Some(bad) = url.chars().find(|c| !is_url_safe(*c)) {
        return Err(format!(
            "artifact url {url:?} contains {bad:?}, which is not valid in a url"
        ));
    }
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "artifact sha256 {sha256:?} must be 64 hex digits, and this one is {}",
            sha256.len()
        ));
    }
    Ok(())
}

/// The unreserved and reserved characters a URL is made of (RFC 3986),
/// which between them exclude quotes, backslashes, backticks, `$`, and
/// every space and control character.
fn is_url_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!&'()*+,;=%".contains(c)
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

    /// The pair is the one part of a root bootstrap script a caller
    /// supplies, so it is checked before it goes anywhere near the VM. The
    /// message is what the user sees.
    #[test]
    fn a_url_must_be_https_and_made_of_url_characters() {
        let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        for bad in [
            "http://example.com/jamstreamd",
            "https://",
            "file:///etc/passwd",
            "example.com/jamstreamd",
            // The shell characters that survive a double-quoted string.
            "https://example.com/a\";id;\"",
            "https://example.com/$(id)",
            "https://example.com/`id`",
            "https://example.com/a\\b",
            "https://example.com/two words",
            "https://example.com/a\nb",
        ] {
            let err = validate_pair(bad, sha).unwrap_err();
            assert!(err.contains("url"), "{bad:?} was rejected as {err:?}");
        }
        // A presigned download is a normal thing to point this at, and its
        // query syntax must survive.
        validate_pair(
            "https://bucket.s3.amazonaws.com/jamstreamd?X-Amz-Signature=ab%2Fcd&X-Amz-Expires=60",
            sha,
        )
        .unwrap();
        // The hash message says what was wrong with it.
        let err = validate_pair("https://example.com/jamstreamd", "abc").unwrap_err();
        assert!(err.contains("64 hex digits") && err.contains('3'), "{err}");
    }
}
