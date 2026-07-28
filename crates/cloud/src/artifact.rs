//! The server artifacts pinned into this build at compile time, one per
//! architecture the providers launch.
//!
//! Release builds are compiled with `JAMSTREAM_SERVER_URL_X86_64` /
//! `JAMSTREAM_SERVER_SHA256_X86_64` and `JAMSTREAM_SERVER_URL_AARCH64` /
//! `JAMSTREAM_SERVER_SHA256_AARCH64` in the environment (release.yml
//! computes all four in its server-musl job and exports them into every
//! client and CLI build), so [`pinned`] returns the exact `jamstreamd`
//! downloads the release published, and cloud hosting works without the
//! user ever seeing an artifact URL or hash. Two pins because the
//! providers do not agree on a CPU: AWS launches Graviton (arm64) while
//! GCP and DigitalOcean launch x86_64, and #139 was a released app
//! booting an arm64 machine with only an x86_64 binary to give it.
//! Development builds have no variables set and get an empty set; there
//! the CLI's `--artifact-url`/`--artifact-sha256` overrides are the only
//! way to host on a cloud provider.
//!
//! `option_env!` is read when THIS crate compiles; cargo tracks the env
//! dependency, so a build with the variables set recompiles the crate.
//! That also means an already-compiled test binary cannot exercise the
//! pinned case: the unit tests here cover the empty case (dev builds),
//! per-architecture selection, and the validation helper on a sample
//! pair, while the pinned case is proven by the release pipeline itself,
//! whose cloud launches would fail without valid pinned pairs.

use std::fmt;

/// CPU architecture of the machines a provider launches, which decides
/// which `jamstreamd` build the session VM must download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerArch {
    X86_64,
    Aarch64,
}

impl ServerArch {
    /// The architecture's name as `uname -m` reports it on Linux.
    pub fn as_str(self) -> &'static str {
        match self {
            ServerArch::X86_64 => "x86_64",
            ServerArch::Aarch64 => "aarch64",
        }
    }
}

impl fmt::Display for ServerArch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `jamstreamd` download baked into the binary at compile time: the URL
/// the session VM fetches at boot and the sha256 it must match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedServerArtifact {
    pub url: &'static str,
    pub sha256: &'static str,
}

/// The per-architecture pins this build carries; release builds carry
/// both, development builds neither.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PinnedServerArtifacts {
    pub x86_64: Option<PinnedServerArtifact>,
    pub aarch64: Option<PinnedServerArtifact>,
}

impl PinnedServerArtifacts {
    /// The pin for the machines `arch` names, if this build carries one.
    pub fn for_arch(self, arch: ServerArch) -> Option<PinnedServerArtifact> {
        match arch {
            ServerArch::X86_64 => self.x86_64,
            ServerArch::Aarch64 => self.aarch64,
        }
    }

    /// True when this build carries any pin at all, which is how callers
    /// tell a release build from a development one.
    pub fn any(self) -> bool {
        self.x86_64.is_some() || self.aarch64.is_some()
    }
}

/// The server artifacts pinned into this build; both fields are `None`
/// for development builds.
pub fn pinned() -> PinnedServerArtifacts {
    PinnedServerArtifacts {
        x86_64: pin_from(
            option_env!("JAMSTREAM_SERVER_URL_X86_64"),
            option_env!("JAMSTREAM_SERVER_SHA256_X86_64"),
        ),
        aarch64: pin_from(
            option_env!("JAMSTREAM_SERVER_URL_AARCH64"),
            option_env!("JAMSTREAM_SERVER_SHA256_AARCH64"),
        ),
    }
}

/// One architecture's pair. Both variables must be present and well
/// formed (non-empty URL, 64 hex character sha256); a half-set or
/// malformed pair yields `None` rather than a panic, with a
/// `debug_assert` to catch a misconfigured pipeline in debug builds.
fn pin_from(
    url: Option<&'static str>,
    sha256: Option<&'static str>,
) -> Option<PinnedServerArtifact> {
    debug_assert!(
        url.is_some() == sha256.is_some(),
        "a pinned artifact's URL and SHA256 variables must be set together"
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
    fn dev_builds_have_no_pinned_artifacts() {
        // This test binary was compiled without the JAMSTREAM_SERVER_URL_* /
        // JAMSTREAM_SERVER_SHA256_* variables; setting them at run time
        // cannot change an already-compiled option_env!, which is exactly
        // the guarantee dev builds rely on.
        assert_eq!(pinned(), PinnedServerArtifacts::default());
        assert!(!pinned().any());
        for arch in [ServerArch::X86_64, ServerArch::Aarch64] {
            assert_eq!(pinned().for_arch(arch), None);
        }
    }

    /// #139: the pin an AWS (arm64) launch selects must never be the
    /// x86_64 build, and vice versa.
    #[test]
    fn each_architecture_selects_its_own_pin() {
        let x86 = PinnedServerArtifact {
            url: "https://example.com/jamstreamd-linux-x86_64-musl",
            sha256: "1111111111111111111111111111111111111111111111111111111111111111",
        };
        let arm = PinnedServerArtifact {
            url: "https://example.com/jamstreamd-linux-aarch64-musl",
            sha256: "2222222222222222222222222222222222222222222222222222222222222222",
        };
        let pins = PinnedServerArtifacts {
            x86_64: Some(x86),
            aarch64: Some(arm),
        };
        assert_eq!(pins.for_arch(ServerArch::X86_64), Some(x86));
        assert_eq!(pins.for_arch(ServerArch::Aarch64), Some(arm));
        assert!(pins.any());

        // A half-pinned build reports the hole instead of substituting the
        // other architecture's binary, which is the exact wrong-binary
        // launch this module exists to prevent.
        let half = PinnedServerArtifacts {
            x86_64: Some(x86),
            aarch64: None,
        };
        assert_eq!(half.for_arch(ServerArch::Aarch64), None);
        assert!(half.any());
    }

    /// The names must match `uname -m` on Linux: they name release assets
    /// and appear in errors a user acts on.
    #[test]
    fn arch_names_match_uname() {
        assert_eq!(ServerArch::X86_64.as_str(), "x86_64");
        assert_eq!(ServerArch::Aarch64.as_str(), "aarch64");
        assert_eq!(format!("{}", ServerArch::Aarch64), "aarch64");
    }

    #[test]
    fn a_release_shaped_pair_validates() {
        // The pinned case cannot be reached from an already-compiled test
        // binary; this covers the validation the release pairs must pass.
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
