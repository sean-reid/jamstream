//! The launch orchestrator both surfaces run: resolve the server artifact,
//! arm the self-destruct, wait for the machine's address, and prove it
//! answers a real handshake.
//!
//! `jamstream host` and the desktop wizard bring a session up the same way,
//! and for a while they did it with two copies of these functions. The
//! copies drifted: only one of them checked the artifact pair an operator
//! typed, so the other spent a launch, a boot and a self-destruct before
//! anyone heard that the url was wrong (#384). What differs between the two
//! surfaces is where an operator types the override and how the caller
//! renders an error, and that is all this module takes as parameters.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use jamstream_cloud::{
    IP_POLL_PERIOD, IP_WAIT_CAP, Instance, PinnedServerArtifacts, Provider, ProviderKind,
    SelfDestruct, ServerArch,
};
use jamstream_protocol::control::MAX_DATAGRAM_BYTES;
use jamstream_protocol::invite::Invite;
use jamstream_session::client::{ClientCore, ClientState, ServerCandidates};

use crate::CliError;

/// Placeholders the mock and local providers accept: the mock launches
/// nothing, and the flat config the local provider consumes carries no
/// artifact fields. A cloud provider needs a real url and hash because the
/// VM downloads and verifies this at boot; see [`resolve_artifact`].
const PLACEHOLDER_ARTIFACT_URL: &str = "https://artifacts.invalid/jamstreamd";
const PLACEHOLDER_ARTIFACT_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// The endpoint a droplet deletes itself through. One definition, because
/// the address in the boot script and the address either surface arms are
/// the same address.
const DROPLETS_ENDPOINT: &str = "https://api.digitalocean.com/v2/droplets";

/// Where the operator types an artifact override on the surface asking for
/// one. The only part of a refusal that differs between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactOverride {
    /// `--artifact-url` and `--artifact-sha256`.
    Flags,
    /// The advanced section of the wizard's preview step.
    AdvancedFields,
}

impl ArtifactOverride {
    /// The clause that ends a refusal: what to do, in the words of the
    /// surface being refused.
    fn remedy(self, arch: ServerArch) -> String {
        match self {
            ArtifactOverride::Flags => format!(
                "pass --artifact-url and --artifact-sha256 naming an {arch} jamstreamd build"
            ),
            ArtifactOverride::AdvancedFields => format!(
                "point the advanced fields of the preview step at an {arch} jamstreamd build"
            ),
        }
    }

    /// The same clause for a build that pins nothing at all, where the
    /// architecture is not the problem.
    fn unpinned_remedy(self) -> &'static str {
        match self {
            ArtifactOverride::Flags => {
                "pass --artifact-url and --artifact-sha256 naming a jamstreamd build the VM \
                 can download and verify, or host with a release build, which pins its own"
            }
            ArtifactOverride::AdvancedFields => {
                "open the advanced section of the preview step and name a jamstreamd build \
                 the VM can download and verify"
            }
        }
    }
}

/// The artifact the boot config carries, by precedence: the explicit
/// override first (it applies to whatever architecture this launch runs
/// on), then the download pinned into this build for `arch`, the
/// architecture of the machines the provider launches. A launch whose
/// architecture has no pin is refused here, because the VM would download a
/// binary that cannot execute (#139). Local and mock launches download
/// nothing, so without an override they get the inert placeholders.
///
/// The override is the one part of the VM's root bootstrap a person types,
/// so it is checked here rather than on the VM, where a bad pair costs a
/// launch, a boot and a self-destruct before anyone hears about it.
pub fn resolve_artifact(
    needs_download: bool,
    arch: ServerArch,
    url: Option<&str>,
    sha256: Option<&str>,
    pinned: PinnedServerArtifacts,
    typed_in: ArtifactOverride,
) -> Result<(String, String), CliError> {
    if let (Some(url), Some(sha)) = (url, sha256) {
        jamstream_cloud::validate_pair(url, sha).map_err(CliError::Usage)?;
        return Ok((url.to_owned(), sha.to_owned()));
    }
    if !needs_download {
        return Ok((
            PLACEHOLDER_ARTIFACT_URL.to_owned(),
            PLACEHOLDER_ARTIFACT_SHA256.to_owned(),
        ));
    }
    match pinned.for_arch(arch) {
        Some(p) => Ok((p.url.to_owned(), p.sha256.to_owned())),
        None if pinned.any() => Err(CliError::Failed(format!(
            "this build pins no {arch} server artifact and this provider launches {arch} \
             machines, which could only download a binary they cannot run; {}",
            typed_in.remedy(arch)
        ))),
        None => Err(CliError::Usage(format!(
            "this build has no pinned server artifact because it is not a release build; {}",
            typed_in.unpinned_remedy()
        ))),
    }
}

/// Per-provider self-destruct. DigitalOcean is the one that needs a
/// credential on the box, because powered-off droplets still bill, so the
/// VM must hold a token able to delete itself. DigitalOcean cannot mint
/// narrower per-droplet tokens, which is why the docs tell DO users to
/// scope theirs to droplet and tag operations only.
///
/// `do_token` comes from this machine: the environment for the CLI, the
/// credential store for the app.
pub fn self_destruct_for(
    kind: ProviderKind,
    do_token: Option<String>,
) -> Result<SelfDestruct, CliError> {
    match kind {
        // The mock resolves with an Aws kind, so it lands here too; the
        // shutdown script is a harmless placeholder for it.
        ProviderKind::Aws => Ok(SelfDestruct::AwsShutdown),
        ProviderKind::Gcp => Ok(SelfDestruct::GcpMaxRunDuration),
        ProviderKind::DigitalOcean => {
            let token = do_token.filter(|t| !t.is_empty()).ok_or_else(|| {
                CliError::Usage(
                    "a DigitalOcean token is required to arm the droplet's self-destruct; \
                     refusing to launch a machine that cannot delete itself"
                        .to_owned(),
                )
            })?;
            Ok(SelfDestruct::ApiToken {
                endpoint: DROPLETS_ENDPOINT.to_owned(),
                token,
            })
        }
        // Local sessions never render cloud-init: the flat config carries
        // no self-destruct and the spawned server self-limits through its
        // own --idle-exit-min. Any variant satisfies the struct; this one
        // is inert here.
        ProviderKind::Local => Ok(SelfDestruct::AwsShutdown),
    }
}

/// Polls list_tagged until the instance reports a public IP. The mock and
/// the local provider return one at launch, so this is a single pass there.
pub async fn wait_for_ip(
    provider: &dyn Provider,
    session_hex: &str,
    mut instance: Instance,
) -> Result<Instance, CliError> {
    let deadline = Instant::now() + IP_WAIT_CAP;
    while instance.public_ip.is_none() {
        if Instant::now() >= deadline {
            return Err(CliError::Failed(format!(
                "instance {} did not report an ip within {} s",
                instance.id,
                IP_WAIT_CAP.as_secs()
            )));
        }
        tokio::time::sleep(IP_POLL_PERIOD).await;
        let listed = provider.list_tagged(Some(session_hex)).await?;
        if let Some(found) = listed.instances.into_iter().find(|i| i.id == instance.id) {
            instance = found;
        }
    }
    Ok(instance)
}

/// Proves the VM is actually serving: a genuine ClientCore handshake with
/// the host invite over UDP, driven until Joined. Skipped for the mock,
/// which launches no server.
///
/// Every address in the invite gets a turn, so this reports what a joining
/// musician will actually experience. Trying the first entry alone gives a host
/// whose invite lists a LAN address first a false "not reachable" on a session
/// the CLI would have connected to.
pub async fn verify_reachable(invite: &Invite, cap: Duration) -> Result<(), CliError> {
    let mut candidates = ServerCandidates::new(invite)?;
    let mut socket = connected_socket(candidates.current()).await?;

    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let (mut core, init) = ClientCore::connect(invite, now())?;
    socket.send(&init).await?;
    let mut buf = [0u8; MAX_DATAGRAM_BYTES];
    while start.elapsed() < cap {
        for pkt in core.poll(now()) {
            socket.send(&pkt).await?;
        }
        if let Ok(Ok(len)) =
            tokio::time::timeout(Duration::from_millis(50), socket.recv(&mut buf)).await
        {
            for pkt in core.handle_datagram(now(), &buf[..len]) {
                socket.send(&pkt).await?;
            }
        }
        match core.state().clone() {
            ClientState::Joined => {
                let _ = core.leave("host reachability check");
                for pkt in core.poll(now()) {
                    socket.send(&pkt).await?;
                }
                return Ok(());
            }
            ClientState::Rejected { ours, theirs } => {
                return Err(CliError::Failed(format!(
                    "server rejected the handshake: this client speaks protocol {ours}, \
                     the server speaks {theirs}"
                )));
            }
            ClientState::Ejected { reason } => {
                return Err(CliError::Failed(format!(
                    "server ejected the reachability check: {reason}"
                )));
            }
            // The core gives up after its own 10 s window; keep trying
            // fresh handshakes until our cap, the VM may still be booting.
            // Each retry moves to the next address the invite offers and
            // comes back round, so a slow boot and a filtered interface
            // both get answered by the time the cap runs out.
            ClientState::TimedOut => {
                if candidates.has_alternatives() {
                    let next = candidates.advance();
                    socket = connected_socket(next).await?;
                }
                let init = core.reconnect(now())?;
                socket.send(&init).await?;
            }
            ClientState::Connecting => {}
        }
    }
    Err(CliError::Failed(format!(
        "server did not complete a handshake within {} s on {}",
        cap.as_secs(),
        invite
            .addresses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// A UDP socket bound to the right family and connected to one candidate.
pub async fn connected_socket(server: SocketAddr) -> Result<tokio::net::UdpSocket, CliError> {
    let bind: SocketAddr = if server.is_ipv4() {
        "0.0.0.0:0".parse().expect("static addr")
    } else {
        "[::]:0".parse().expect("static addr")
    };
    let socket = tokio::net::UdpSocket::bind(bind).await?;
    socket.connect(server).await?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamstream_cloud::{PinnedServerArtifact, providers};

    /// A release-shaped pin set with two distinct downloads, so a test that
    /// selects the wrong one cannot pass by coincidence.
    fn both_pins() -> PinnedServerArtifacts {
        PinnedServerArtifacts {
            x86_64: Some(PinnedServerArtifact {
                url: "https://github.com/sean-reid/jamstream/releases/download/v1/jamstreamd-linux-x86_64-musl",
                sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            }),
            aarch64: Some(PinnedServerArtifact {
                url: "https://github.com/sean-reid/jamstream/releases/download/v1/jamstreamd-linux-aarch64-musl",
                sha256: "4444444444444444444444444444444444444444444444444444444444444444",
            }),
        }
    }

    fn resolve(
        needs_download: bool,
        arch: ServerArch,
        url: Option<&str>,
        sha: Option<&str>,
        pinned: PinnedServerArtifacts,
    ) -> Result<(String, String), CliError> {
        resolve_artifact(
            needs_download,
            arch,
            url,
            sha,
            pinned,
            ArtifactOverride::Flags,
        )
    }

    #[test]
    fn artifact_precedence_is_the_override_then_pinned_then_error() {
        let pinned = both_pins();
        let sha = "1111111111111111111111111111111111111111111111111111111111111111";
        // An explicit pair outranks the pin, on either architecture.
        for arch in [ServerArch::X86_64, ServerArch::Aarch64] {
            let (url, out) = resolve(
                true,
                arch,
                Some("https://own.example/jamstreamd"),
                Some(sha),
                pinned,
            )
            .unwrap();
            assert_eq!(url, "https://own.example/jamstreamd");
            assert_eq!(out, sha);
        }
        // Nothing typed: the pin for the launch's architecture fills in.
        let (url, sha) = resolve(true, ServerArch::X86_64, None, None, pinned).unwrap();
        assert_eq!(url, pinned.x86_64.unwrap().url);
        assert_eq!(sha, pinned.x86_64.unwrap().sha256);
        // Nothing typed, no pins, cloud launch: the error explains why and
        // both ways out.
        let text = resolve(
            true,
            ServerArch::X86_64,
            None,
            None,
            PinnedServerArtifacts::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(text.contains("not a release build"), "error was: {text}");
        assert!(text.contains("--artifact-url"), "error was: {text}");
    }

    /// #139 root cause: the pin followed the build, not the machine. The
    /// architecture must come from the provider doing the launching, and
    /// the two real cloud providers with fixed instance families must pull
    /// opposite pins from the same build.
    #[test]
    fn aws_selects_the_arm64_pin_and_digitalocean_the_x86_64_one() {
        use providers::{aws::AwsProvider, digitalocean::DigitalOceanProvider};
        let pinned = both_pins();
        let aws = AwsProvider::new("AKIATEST".to_owned(), "secret".to_owned());
        let (url, _) = resolve(true, aws.server_arch(), None, None, pinned).unwrap();
        assert!(url.ends_with("jamstreamd-linux-aarch64-musl"), "{url}");
        let digitalocean = DigitalOceanProvider::new("t".to_owned());
        let (url, _) = resolve(true, digitalocean.server_arch(), None, None, pinned).unwrap();
        assert!(url.ends_with("jamstreamd-linux-x86_64-musl"), "{url}");
    }

    /// A launch whose architecture has no pin must refuse before a machine
    /// is paid for, and the error must name the architecture, because
    /// launching would produce exactly the dead VM of #139.
    #[test]
    fn a_missing_arch_pin_refuses_to_launch_naming_the_architecture() {
        let x86_only = PinnedServerArtifacts {
            aarch64: None,
            ..both_pins()
        };
        let err = resolve(true, ServerArch::Aarch64, None, None, x86_only)
            .unwrap_err()
            .to_string();
        assert!(err.contains("aarch64"), "error was: {err}");
        assert!(err.contains("--artifact-url"), "error was: {err}");
        // The override remains the escape hatch and applies to the
        // architecture being launched.
        let (url, _) = resolve(
            true,
            ServerArch::Aarch64,
            Some("https://own.example/jamstreamd-arm64"),
            Some("3333333333333333333333333333333333333333333333333333333333333333"),
            x86_only,
        )
        .unwrap();
        assert_eq!(url, "https://own.example/jamstreamd-arm64");
    }

    /// The refusal has to name the control the surface being refused
    /// actually has: a flag in a terminal, a field in the app.
    #[test]
    fn each_surface_is_refused_in_its_own_words() {
        for (typed_in, expected) in [
            (ArtifactOverride::Flags, "--artifact-url"),
            (ArtifactOverride::AdvancedFields, "advanced fields"),
        ] {
            let arch_missing = resolve_artifact(
                true,
                ServerArch::Aarch64,
                None,
                None,
                PinnedServerArtifacts {
                    aarch64: None,
                    ..both_pins()
                },
                typed_in,
            )
            .unwrap_err()
            .to_string();
            assert!(arch_missing.contains(expected), "{arch_missing}");
            let unpinned = resolve_artifact(
                true,
                ServerArch::X86_64,
                None,
                None,
                PinnedServerArtifacts::default(),
                typed_in,
            )
            .unwrap_err()
            .to_string();
            let unpinned_expected = match typed_in {
                ArtifactOverride::Flags => "--artifact-url",
                ArtifactOverride::AdvancedFields => "advanced section",
            };
            assert!(unpinned.contains(unpinned_expected), "{unpinned}");
        }
    }

    /// #384: the wizard took the same operator-typed pair the CLI takes and
    /// did not check it, so a typo cost a launch, a boot and a self-destruct
    /// before anyone heard about it. Both surfaces resolve through this
    /// function, so both refuse the same pairs for the same reasons.
    #[test]
    fn a_typed_artifact_pair_is_validated_before_anything_launches() {
        let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        for (url, sha, what) in [
            ("http://own.example/jamstreamd", sha, "https"),
            ("https://own.example/a\";id;\"", sha, "url"),
            ("https://own.example/jamstreamd", "abcd", "64 hex digits"),
        ] {
            for typed_in in [ArtifactOverride::Flags, ArtifactOverride::AdvancedFields] {
                let err = resolve_artifact(
                    true,
                    ServerArch::X86_64,
                    Some(url),
                    Some(sha),
                    PinnedServerArtifacts::default(),
                    typed_in,
                )
                .unwrap_err()
                .to_string();
                assert!(err.contains(what), "{url} {sha} was rejected as {err:?}");
            }
        }
        // A valid pair still passes through untouched, on a local launch as
        // well as a cloud one.
        for needs_download in [true, false] {
            let (url, out_sha) = resolve(
                needs_download,
                ServerArch::X86_64,
                Some("https://own.example/d"),
                Some(sha),
                PinnedServerArtifacts::default(),
            )
            .unwrap();
            assert_eq!(url, "https://own.example/d");
            assert_eq!(out_sha, sha);
        }
    }

    #[test]
    fn local_and_mock_launches_use_placeholders_unless_overridden() {
        // No download happens, so nothing typed and no pin is fine.
        let (url, sha) = resolve(
            false,
            ServerArch::X86_64,
            None,
            None,
            PinnedServerArtifacts::default(),
        )
        .unwrap();
        assert_eq!(url, PLACEHOLDER_ARTIFACT_URL);
        assert_eq!(sha, PLACEHOLDER_ARTIFACT_SHA256);
        // An explicit pair still wins everywhere.
        let (url, _) = resolve(
            false,
            ServerArch::X86_64,
            Some("https://own.example/jamstreamd"),
            Some("2222222222222222222222222222222222222222222222222222222222222222"),
            PinnedServerArtifacts::default(),
        )
        .unwrap();
        assert_eq!(url, "https://own.example/jamstreamd");
    }

    /// A powered-off droplet still bills, so a droplet that cannot delete
    /// itself must not be launched at all.
    #[test]
    fn only_digitalocean_needs_a_token_to_arm_its_self_destruct() {
        assert!(matches!(
            self_destruct_for(ProviderKind::Aws, None),
            Ok(SelfDestruct::AwsShutdown)
        ));
        assert!(matches!(
            self_destruct_for(ProviderKind::Gcp, None),
            Ok(SelfDestruct::GcpMaxRunDuration)
        ));
        assert!(matches!(
            self_destruct_for(ProviderKind::Local, None),
            Ok(SelfDestruct::AwsShutdown)
        ));
        let err = self_destruct_for(ProviderKind::DigitalOcean, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("self-destruct"), "{err}");
        // An empty token is no token.
        assert!(self_destruct_for(ProviderKind::DigitalOcean, Some(String::new())).is_err());
        let armed = self_destruct_for(ProviderKind::DigitalOcean, Some("dop_v1_x".to_owned()))
            .expect("a token arms it");
        let SelfDestruct::ApiToken { endpoint, token } = armed else {
            panic!("a droplet deletes itself through the API");
        };
        assert_eq!(endpoint, DROPLETS_ENDPOINT);
        assert_eq!(token, "dop_v1_x");
    }
}
