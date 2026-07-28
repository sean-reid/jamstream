//! `jamstream host`: rank regions, preview cost, launch the session VM,
//! mint invites, verify reachability, and record the session on disk.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use data_encoding::{BASE64, HEXLOWER};
use jamstream_cloud::{
    BootConfig, CostPreview, InstanceClass, LaunchSpec, Price, ProbeMatrix, ProbeTarget, Provider,
    ProviderKind, RecordingStorage, Region, RegionId, RegionScore, RetentionEnforcement,
    SelfDestruct, rank, session_tag,
};
use jamstream_protocol::control::MAX_DATAGRAM_BYTES;
use jamstream_protocol::ids::{HOST_MEMBER_ID, MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_session::client::{ClientCore, ClientState, ServerCandidates};

use crate::CliError;
use crate::cli::HostArgs;
use crate::state::{self, InviteRecord, SessionState, SessionStatus};

const IP_WAIT_CAP: Duration = Duration::from_secs(180);
const IP_POLL_PERIOD: Duration = Duration::from_secs(2);
const HANDSHAKE_CAP: Duration = Duration::from_secs(60);

/// Placeholders the mock and local providers accept: the mock launches
/// nothing, and the flat config the local provider consumes carries no
/// artifact fields. A cloud provider needs a real url and hash because the
/// VM downloads and verifies this at boot; see [`resolve_artifact`].
const PLACEHOLDER_ARTIFACT_URL: &str = "https://artifacts.invalid/jamstreamd";
const PLACEHOLDER_ARTIFACT_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// The artifact the boot config carries, by precedence: the explicit
/// --artifact-url/--artifact-sha256 overrides first (they apply to
/// whatever architecture this launch runs on), then the download pinned
/// into this build for `arch`, the architecture of the machines the
/// provider launches. A launch whose architecture has no pin is refused
/// here, because the VM would download a binary that cannot execute
/// (#139). Local and mock launches download nothing, so without
/// overrides they get the inert placeholders.
fn resolve_artifact(
    needs_download: bool,
    arch: jamstream_cloud::ServerArch,
    url_flag: Option<&str>,
    sha_flag: Option<&str>,
    pinned: jamstream_cloud::PinnedServerArtifacts,
) -> Result<(String, String), CliError> {
    if let (Some(url), Some(sha)) = (url_flag, sha_flag) {
        // Checked here rather than on the VM, where a bad pair costs a
        // launch, a boot, and a self-destruct before anyone hears about it.
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
             machines, which could only download a binary they cannot run; pass \
             --artifact-url and --artifact-sha256 naming an {arch} jamstreamd build"
        ))),
        None => Err(CliError::Usage(
            "this build has no pinned server artifact because it is not a release build; \
             pass --artifact-url and --artifact-sha256 naming a jamstreamd build the VM \
             can download and verify, or host with a release build, which pins its own"
                .to_owned(),
        )),
    }
}

pub async fn run<W: Write>(
    args: &HostArgs,
    provider: &dyn Provider,
    out: &mut W,
) -> Result<(), CliError> {
    let is_mock = args.provider == "mock";
    let is_local = provider.kind() == ProviderKind::Local;
    // A local session records to this machine's disk, because the server is
    // already here; a cloud session records to a bucket, because the take has
    // to outlive a VM that deletes itself. Naming both in one flag pair and
    // resolving it here is what stops a launch that records nowhere.
    if args.bucket.is_some() && is_local {
        return Err(CliError::Usage(
            "a local session records to this computer's disk, so it takes no --bucket; \
             drop the flag and the takes land in the directory this prints"
                .to_owned(),
        ));
    }
    if args.wants_recording() && !is_local && args.bucket.is_none() {
        return Err(CliError::Usage(
            "a cloud session records to a bucket, so --record needs --bucket <name>: the \
             VM is destroyed at the end of the session and a take on its disk goes with it"
                .to_owned(),
        ));
    }
    let record_dir = if args.wants_recording() && is_local {
        Some(state::recordings_dir()?)
    } else {
        None
    };
    let regions = provider.regions();
    if regions.is_empty() {
        return Err(CliError::Failed(format!(
            "provider {} offers no regions",
            args.provider
        )));
    }

    // Pre-flight orphan guard: anything already tagged jamstream on this
    // provider is billing right now, most likely a stray from a session
    // that never got torn down. Warn before adding another instance.
    let preexisting = provider.list_tagged(None).await?;
    if !preexisting.is_empty() && !args.json {
        writeln!(
            out,
            "found {} existing jamstream instances; run jamstream sweep if these are strays",
            preexisting.len()
        )?;
        for inst in &preexisting {
            writeln!(
                out,
                "  {} {} {} (session {})",
                inst.provider.as_str(),
                inst.region.id.as_str(),
                inst.id,
                inst.session_id().unwrap_or("unknown")
            )?;
        }
    }
    // A region that does not sell this session's machine size is not an
    // error, it is a region we cannot use; priced_regions drops it and
    // keeps the rest, and anything worse than that still stops here.
    let table = jamstream_cloud::priced_regions(provider).await?;
    if !table.unavailable.is_empty() && !args.json {
        let names: Vec<&str> = table.unavailable.iter().map(|r| r.id.as_str()).collect();
        writeln!(
            out,
            "not listed: {} ({} does not offer this session's machine size there)",
            names.join(", "),
            args.provider
        )?;
    }
    let candidates = table.candidates;

    let (region, price) = choose_region(args, provider, &candidates, is_mock, out).await?;

    // The bucket is in the session's own region, so this waits for the region
    // to be chosen; the credential is read here rather than after the launch,
    // because a missing key must not cost a machine.
    let recording = match &args.bucket {
        Some(bucket) => Some(recording_storage(
            args,
            provider.kind(),
            bucket,
            &region.id,
        )?),
        None => None,
    };

    let preview = CostPreview::compute(
        &price,
        args.hours,
        args.musicians,
        args.destinations,
        args.listeners,
    );
    if !args.json {
        writeln!(out)?;
        if is_local {
            // One region, this computer, zero price; a cost table would
            // dress that up.
            writeln!(out, "Local sessions cost nothing.")?;
        } else {
            writeln!(
                out,
                "Cost preview for {} {} over {:.1} hours:",
                args.provider, region.id, args.hours
            )?;
            for row in preview.display_table() {
                writeln!(out, "{row}")?;
            }
        }
        if let Some(storage) = &recording {
            writeln!(
                out,
                "Takes go to {} in {} ({}). Downloading them later costs egress, which \
                 jamstream recordings get prices before it starts.",
                storage.bucket,
                storage.region,
                storage.retention.label().to_lowercase()
            )?;
        }
    }

    if !args.yes && !confirm(out)? {
        writeln!(out, "Aborted. Nothing was launched.")?;
        return Ok(());
    }

    // Session identity and key material. The issuer private key stays on
    // this machine; the server gets its own private key via user-data.
    let session_id = SessionId::generate();
    let session_hex = HEXLOWER.encode(&session_id.0);
    let issuer = Issuer::generate();
    let server_keys = generate_keypair();
    let now_unix = unix_now();
    let expires_unix = now_unix + u64::from(args.max_hours) * 3600;

    // Local mode runs a binary already on this machine and the mock
    // launches nothing; only a cloud VM downloads the artifact for real.
    let (artifact_url, artifact_sha256) = resolve_artifact(
        !is_mock && !is_local,
        provider.server_arch(),
        args.artifact_url.as_deref(),
        args.artifact_sha256.as_deref(),
        jamstream_cloud::pinned(),
    )?;
    let boot = BootConfig {
        artifact_url,
        artifact_sha256,
        server_private_key_b64: BASE64.encode(&server_keys.private),
        issuer_public_key_b64: BASE64.encode(issuer.public_key().as_bytes()),
        session_id_hex: session_hex.clone(),
        port: args.port,
        idle_shutdown_min: args.idle_min,
        max_duration_min: args.max_hours * 60,
        self_destruct: self_destruct_for(provider.kind())?,
        // A local session records through the provider's own spawn flags
        // (see LocalProvider::with_record) rather than the boot config, so
        // this is set for a bucket and nothing else.
        recording: recording.clone(),
    };

    // Proved before the machine exists: a take must never fail mid-song
    // because the bucket was wrong, and the retention rule has to be in place
    // before the first byte is uploaded. The lifecycle call is the host's to
    // make, not the VM's, whose key is scoped to writing one prefix.
    if let Some(storage) = &recording {
        let applied = verify_bucket(storage, &session_hex).await?;
        if !args.json {
            writeln!(out, "{}", applied.describe())?;
        }
    }

    // The local provider consumes the flat key=value server config
    // directly; cloud providers get cloud-init YAML that writes the same
    // config on the VM. See the user_data contract in the local provider.
    let user_data = if is_local {
        boot.render_flat_config()
    } else {
        jamstream_cloud::cloudinit::render(&boot)
    };
    let spec = LaunchSpec {
        region: region.clone(),
        instance_class: InstanceClass::Standard,
        user_data,
        tags: vec![session_tag(&session_hex)],
    };
    if !args.json {
        writeln!(out)?;
        if is_local {
            writeln!(out, "Starting the server on this computer.")?;
        } else {
            writeln!(out, "Launching in {}.", region.id)?;
        }
    }
    let instance = provider.launch(spec).await?;
    let instance = wait_for_ip(provider, &session_hex, instance).await?;
    let ip = instance.public_ip.expect("wait_for_ip returned without ip");
    let address = SocketAddr::new(ip, args.port);

    let invites = mint_invites(
        &issuer,
        session_id,
        address,
        server_keys.public,
        expires_unix,
        args.musicians,
        args.listeners,
    );

    let reachability = if is_mock {
        if !args.json {
            writeln!(
                out,
                "Reachability check skipped: the mock provider launches no real server."
            )?;
        }
        "skipped"
    } else {
        verify_reachable(&invites[0].1, HANDSHAKE_CAP).await?;
        "ok"
    };

    let session_state = SessionState {
        session_id_hex: session_hex.clone(),
        provider: args.provider.clone(),
        region: region.id.to_string(),
        instance_id: instance.id.clone(),
        address: address.to_string(),
        created_unix: now_unix,
        hourly_microusd: price.hourly_microusd,
        issuer_private_key_b64: BASE64.encode(&issuer.to_bytes()),
        server_public_key_b64: BASE64.encode(&server_keys.public),
        invites: invites
            .iter()
            .map(|(label, invite)| InviteRecord {
                role: label.clone(),
                invite: invite.encode(),
            })
            .collect(),
        status: SessionStatus::Running,
        ended_unix: None,
    };
    let state_path = state::save(&session_state)?;
    // Written beside the session record, because `jamstream recordings` has
    // no other way to find the bucket once the VM that wrote to it is gone.
    let recording_record = recording.as_ref().map(recording_record);
    if let Some(record) = &recording_record {
        state::save_recording(&session_hex, record)?;
    }

    if args.json {
        let value = serde_json::json!({
            "session_id": session_hex,
            "provider": args.provider,
            "region": region.id.as_str(),
            "instance_id": instance.id,
            "address": address.to_string(),
            "hourly_microusd": price.hourly_microusd,
            "estimated_total_microusd": preview.total_microusd,
            "reachability": reachability,
            "record_dir": record_dir,
            "recording": recording_record,
            "invites": session_state.invites,
            "state_file": state_path,
            "preexisting_instances": preexisting
                .iter()
                .map(|inst| {
                    serde_json::json!({
                        "provider": inst.provider.as_str(),
                        "region": inst.region.id.as_str(),
                        "instance_id": inst.id,
                        "session_id": inst.session_id(),
                    })
                })
                .collect::<Vec<_>>(),
        });
        writeln!(out, "{}", serde_json::to_string_pretty(&value)?)?;
    } else {
        writeln!(out)?;
        writeln!(out, "Session {} is running.", &session_hex[..8])?;
        writeln!(out, "{:<12} {address}", "server")?;
        // The take is the point of --record, so where it lands is printed
        // where the invites are, not buried in a log.
        if let Some(dir) = &record_dir {
            writeln!(out, "{:<12} {}", "record dir", dir.display())?;
        }
        if let Some(record) = &recording_record {
            writeln!(
                out,
                "{:<12} {}/{}",
                "takes",
                record.bucket,
                jamstream_cloud::session_prefix(&session_hex)
            )?;
        }
        for (label, invite) in &invites {
            writeln!(out, "{:<12} {}", label, invite.encode())?;
        }
        writeln!(out)?;
        writeln!(out, "State written to {}.", state_path.display())?;
        writeln!(
            out,
            "End the session with: jamstream end {}",
            &session_hex[..8]
        )?;
        if recording_record.is_some() {
            writeln!(
                out,
                "Fetch the takes with:  jamstream recordings get {}",
                &session_hex[..8]
            )?;
        }
    }
    Ok(())
}

/// The bucket config a cloud launch carries to the VM, from the flags plus
/// the storage key in this machine's environment.
fn recording_storage(
    args: &HostArgs,
    kind: ProviderKind,
    bucket: &str,
    region: &RegionId,
) -> Result<RecordingStorage, CliError> {
    if bucket.trim().is_empty() {
        return Err(CliError::Usage("--bucket is empty".to_owned()));
    }
    // Priced here so a region with no bucket service (a DigitalOcean region
    // with no Spaces endpoint) is refused before the launch rather than at
    // the first upload.
    jamstream_cloud::storage_price(kind, region)?;
    Ok(RecordingStorage {
        provider: kind,
        bucket: bucket.to_owned(),
        region: region.to_string(),
        retention: args.retention,
        credential: crate::storage::credential_from_env(kind)?,
        stems: args.record_stems,
    })
}

/// What the session record keeps about the bucket: everything but the key.
fn recording_record(storage: &RecordingStorage) -> state::RecordingRecord {
    state::RecordingRecord {
        provider: storage.provider.as_str().to_owned(),
        bucket: storage.bucket.clone(),
        region: storage.region.clone(),
        retention: storage.retention.to_string(),
        stems: storage.stems,
    }
}

/// Proves the key can write this session's prefix, and applies the retention
/// rule to it.
///
/// The probe object is written and deleted under the session's own prefix, so
/// a bucket that refuses the launch key fails here, in a configuring frame of
/// mind, rather than at the first take.
async fn verify_bucket(
    storage: &RecordingStorage,
    session_hex: &str,
) -> Result<RetentionEnforcement, CliError> {
    let store = storage.object_store()?;
    let prefix = jamstream_cloud::session_prefix(session_hex);
    let probe = format!("{prefix}.jamstream-probe");
    store
        .put(
            &storage.bucket,
            &probe,
            jamstream_cloud::JSON_CONTENT_TYPE,
            b"{\"probe\":true}",
        )
        .await
        .map_err(|err| {
            CliError::Failed(format!(
                "cannot write to {} in {}: {err}. Recording needs a key that may write \
                 {prefix} and set the bucket's lifecycle rule.",
                storage.bucket, storage.region
            ))
        })?;
    store.delete(&storage.bucket, &probe).await?;
    Ok(store
        .set_retention(&storage.bucket, &prefix, storage.retention)
        .await?)
}

/// Honors --region when given; otherwise ranks by latency and price and
/// takes the top of the table.
async fn choose_region<W: Write>(
    args: &HostArgs,
    provider: &dyn Provider,
    candidates: &[(Region, Price)],
    is_mock: bool,
    out: &mut W,
) -> Result<(Region, Price), CliError> {
    if let Some(wanted) = &args.region {
        return candidates
            .iter()
            .find(|(r, _)| r.id.as_str() == wanted)
            .cloned()
            .ok_or_else(|| {
                CliError::Usage(format!(
                    "region {wanted:?} is not offered by provider {}",
                    args.provider
                ))
            });
    }
    // Local offers exactly one region, this computer: nothing to probe,
    // nothing to rank, no table worth printing.
    if provider.kind() == ProviderKind::Local {
        return Ok(candidates[0].clone());
    }
    let regions: Vec<Region> = candidates.iter().map(|(r, _)| r.clone()).collect();
    let matrix = if is_mock {
        if !args.json {
            writeln!(
                out,
                "Latency figures are fabricated: the mock provider has no real endpoints."
            )?;
        }
        mock_matrix(&regions)
    } else {
        let targets = catalog_targets(provider.kind(), &regions);
        let rtts = jamstream_cloud::probe_all(&targets).await;
        let mut matrix = ProbeMatrix::new();
        for (region, rtt_ms) in rtts {
            matrix.insert(0, region, rtt_ms);
        }
        matrix
    };
    let ranked = rank(&matrix, candidates);
    if !args.json {
        for row in render_region_table(&ranked) {
            writeln!(out, "{row}")?;
        }
    }
    let top = &ranked[0];
    Ok((top.region.clone(), top.price))
}

fn confirm<W: Write>(out: &mut W) -> Result<bool, CliError> {
    write!(out, "Launch this session? [y/N] ")?;
    out.flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// Deterministic fake RTT for a mock region: FNV-1a of the region id
/// folded into 5..80 ms. Stable across runs so tests can assert on it.
pub fn fabricated_rtt_ms(region: &RegionId) -> f32 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in region.as_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (5 + hash % 75) as f32
}

/// The host is the only member at create time, so the matrix has exactly
/// one row of fabricated probes.
pub fn mock_matrix(regions: &[Region]) -> ProbeMatrix {
    let mut matrix = ProbeMatrix::new();
    for region in regions {
        matrix.insert(0, region.id.clone(), fabricated_rtt_ms(&region.id));
    }
    matrix
}

/// Probe catalog entries for this provider, restricted to offered regions.
/// Regions absent from the catalog simply produce no probe; the solver
/// falls back to price order when the whole matrix ends up empty.
pub fn catalog_targets(kind: ProviderKind, regions: &[Region]) -> Vec<ProbeTarget> {
    jamstream_cloud::probe_catalog()
        .into_iter()
        .filter(|t| t.provider == kind && regions.iter().any(|r| r.id == t.region))
        .collect()
}

/// One row per region in solver order. Latency and hourly cost share the
/// table with equal column weight.
pub fn render_region_table(scores: &[RegionScore]) -> Vec<String> {
    let mut rows = vec![format!(
        "{:<16} {:>12} {:>14} {:>12}",
        "REGION", "WORST RTT", "HOURLY", "EGRESS"
    )];
    for score in scores {
        let rtt = if score.worst_rtt_ms.is_finite() {
            format!("{:.0} ms", score.worst_rtt_ms)
        } else {
            "no probe".to_owned()
        };
        rows.push(format!(
            "{:<16} {:>12} {:>14} {:>12}",
            // RegionId's Display ignores width flags; pad the &str instead.
            score.region.id.as_str(),
            rtt,
            score.price.hourly_display(),
            score.price.egress_display()
        ));
    }
    rows
}

pub fn invite_label(role: Role, member: MemberId) -> String {
    if member == HOST_MEMBER_ID {
        return "host".to_owned();
    }
    match role {
        Role::Musician => format!("musician {}", member.0),
        Role::Listener => format!("listener {}", member.0),
    }
}

/// Every address an invite should offer for one server address, most
/// direct first.
///
/// A locally hosted session runs on the machine doing the minting, so the
/// address the provider reported is one of this machine's own interfaces.
/// Loopback reaches the same server without leaving the host, and that is
/// worth offering first: the macOS Application Firewall filters incoming
/// connections per binary and does not govern loopback, so a same-machine
/// join over a real interface can simply not arrive, and on a managed Mac
/// the firewall cannot be changed from the command line at all. The
/// symptom is no error, just a handshake that never completes.
///
/// The LAN address stays, second, because a bandmate on the same network
/// joins through the same invite. They spend one connection timeout on
/// loopback first, where either nothing answers or something that is not
/// this session does and cannot complete the Noise handshake against the
/// server key the invite carries.
///
/// A cloud VM's address is nobody's local interface, so a cloud invite is
/// unchanged: one address, exactly as before.
pub fn candidate_addresses(address: SocketAddr) -> Vec<SocketAddr> {
    candidates_for(address, jamstream_cloud::providers::local::primary_lan_ip())
}

/// [`candidate_addresses`] with the local address supplied, so the rule is
/// testable without a network.
fn candidates_for(address: SocketAddr, this_machine: IpAddr) -> Vec<SocketAddr> {
    if address.ip().is_loopback() || address.ip() != this_machine {
        return vec![address];
    }
    let loopback = match address.ip() {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
    };
    vec![SocketAddr::new(loopback, address.port()), address]
}

/// Mints the session's invite book: the host (member 0) first, then the
/// remaining musician seats, then the listeners.
///
/// `musicians` is the total number of musician seats *including the host's
/// own*, matching `--musicians` and [`jamstream_session::MAX_MUSICIANS`], so
/// a value of N yields one host invite plus N-1 musician invites and the
/// server admits exactly the seats that were minted. `musicians` of 0 is
/// treated as 1 (the host alone); the flag's range makes that unreachable.
///
/// The desktop app calls this too, so both surfaces mint identically.
pub fn mint_invites(
    issuer: &Issuer,
    session_id: SessionId,
    address: SocketAddr,
    server_pk: [u8; 32],
    expires_unix: u64,
    musicians: u8,
    listeners: u8,
) -> Vec<(String, Invite)> {
    // Once, not once per seat: discovering the machine's own address opens
    // a socket, and every invite to one session offers the same places.
    let addresses = candidate_addresses(address);
    let mint = |member: u16, role: Role| {
        let token = Token {
            member_id: MemberId(member),
            role,
            name_hint: None,
            expires_unix,
            jti: TokenId::generate(),
        };
        let invite = issuer.mint(session_id, addresses.clone(), server_pk, token);
        (invite_label(role, MemberId(member)), invite)
    };
    let seats = u16::from(musicians).max(1);
    let mut invites = vec![mint(HOST_MEMBER_ID.0, Role::Musician)];
    for m in 1..seats {
        invites.push(mint(m, Role::Musician));
    }
    for l in 0..u16::from(listeners) {
        invites.push(mint(seats + l, Role::Listener));
    }
    invites
}

fn self_destruct_for(kind: ProviderKind) -> Result<SelfDestruct, CliError> {
    match kind {
        // The mock resolves with an Aws kind, so it lands here too; the
        // shutdown script is a harmless placeholder for it.
        ProviderKind::Aws => Ok(SelfDestruct::AwsShutdown),
        ProviderKind::Gcp => Ok(SelfDestruct::GcpMaxRunDuration),
        // Powered-off droplets still bill, so the VM must hold a token able
        // to delete itself. DigitalOcean cannot mint narrower per-droplet
        // tokens, which is why the docs tell DO users to scope theirs to
        // droplet and tag operations only.
        ProviderKind::DigitalOcean => {
            let token = std::env::var("DIGITALOCEAN_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
                .ok_or_else(|| {
                    CliError::Usage(
                        "DIGITALOCEAN_TOKEN is required to arm the droplet's self-destruct; \
                         refusing to launch a machine that cannot delete itself"
                            .to_owned(),
                    )
                })?;
            Ok(SelfDestruct::ApiToken {
                endpoint: "https://api.digitalocean.com/v2/droplets".to_owned(),
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

/// Polls list_tagged until the instance reports a public IP. The mock
/// returns one at launch, so this is a single pass there.
async fn wait_for_ip(
    provider: &dyn Provider,
    session_hex: &str,
    mut instance: jamstream_cloud::Instance,
) -> Result<jamstream_cloud::Instance, CliError> {
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
        if let Some(found) = listed.into_iter().find(|i| i.id == instance.id) {
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
/// musician will actually experience rather than what the first entry
/// alone would.
async fn verify_reachable(invite: &Invite, cap: Duration) -> Result<(), CliError> {
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamstream_cloud::MockProvider;
    use jamstream_session::MAX_MUSICIANS;

    fn recording_args() -> HostArgs {
        HostArgs {
            provider: "mock".to_owned(),
            region: None,
            musicians: 2,
            listeners: 0,
            hours: 1.0,
            destinations: 0,
            port: 43210,
            idle_min: 10,
            max_hours: 12,
            record: false,
            record_stems: true,
            bucket: None,
            retention: jamstream_cloud::Retention::Days30,
            artifact_url: None,
            artifact_sha256: None,
            yes: true,
            json: true,
        }
    }

    /// A cloud session's VM deletes itself, so a take on its disk is a take
    /// nobody gets. Asking to record without a bucket is refused before
    /// anything launches rather than billed for a session that records
    /// nowhere, and stems alone must trip it too, since they imply recording.
    #[tokio::test]
    async fn a_cloud_host_recording_to_no_bucket_is_refused_before_launch() {
        let provider = MockProvider::with_default_regions(ProviderKind::Aws);
        let mut out = Vec::new();
        let err = run(&recording_args(), &provider, &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("--bucket"), "error was: {err}");
        assert!(out.is_empty(), "the refusal must come before any output");
    }

    /// The other way round: a local session's takes land on this machine's
    /// own disk, so a bucket for one is a flag with nowhere to apply.
    #[tokio::test]
    async fn a_local_host_with_a_bucket_is_refused_before_launch() {
        let provider = MockProvider::with_default_regions(ProviderKind::Local);
        let args = HostArgs {
            provider: "local".to_owned(),
            bucket: Some("my-jams".to_owned()),
            ..recording_args()
        };
        let mut out = Vec::new();
        let err = run(&args, &provider, &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no --bucket"), "error was: {err}");
        assert!(out.is_empty(), "the refusal must come before any output");
    }

    /// The record kept beside the session is where `jamstream recordings`
    /// looks, and the storage key must not be in it.
    #[test]
    fn the_session_record_carries_the_bucket_and_never_the_key() {
        let args = HostArgs {
            bucket: Some("my-jams".to_owned()),
            retention: jamstream_cloud::Retention::Days90,
            ..recording_args()
        };
        // Built directly rather than through recording_storage, which reads
        // the key out of this process's environment.
        let storage = RecordingStorage {
            provider: ProviderKind::Aws,
            bucket: "my-jams".to_owned(),
            region: "eu-west-1".to_owned(),
            retention: args.retention,
            credential: jamstream_cloud::StorageCredential::KeyPair {
                access_key_id: "AKIDTEST".to_owned(),
                secret_access_key: "hunter2".to_owned(),
            },
            stems: args.record_stems,
        };
        let record = recording_record(&storage);
        assert_eq!(record.provider, "aws");
        assert_eq!(record.bucket, "my-jams");
        assert_eq!(record.region, "eu-west-1");
        assert_eq!(record.retention, "90d");
        assert!(record.stems);
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("hunter2"), "{json}");
        assert!(!json.contains("AKID"), "{json}");
        // And it reads back as the choice it was, which is what the download
        // prompt prints.
        assert_eq!(
            crate::storage::retention_label(&record),
            "Delete after 90 days"
        );
    }

    /// A DigitalOcean region with no Spaces endpoint has no bucket to record
    /// to, and that is cheaper to learn before the launch than at the first
    /// upload.
    #[test]
    fn a_region_with_no_bucket_service_is_refused() {
        let args = HostArgs {
            bucket: Some("my-jams".to_owned()),
            ..recording_args()
        };
        let err = recording_storage(
            &args,
            ProviderKind::DigitalOcean,
            "my-jams",
            &RegionId::new("nyc1"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not available in nyc1"), "{err}");
        // An empty bucket name is a typo, not a bucket.
        assert!(
            recording_storage(&args, ProviderKind::Aws, "  ", &RegionId::new("eu-west-1")).is_err()
        );
    }

    #[test]
    fn fabricated_rtts_are_deterministic_and_in_range() {
        for id in ["mock-east", "mock-west", "eu-central-1", "x"] {
            let region = RegionId::new(id);
            let first = fabricated_rtt_ms(&region);
            let second = fabricated_rtt_ms(&region);
            assert_eq!(first, second, "rtt for {id} must be stable");
            assert!(
                (5.0..80.0).contains(&first),
                "rtt for {id} was {first} ms, outside 5..80"
            );
        }
        // Distinct regions should not all collapse to one value.
        assert_ne!(
            fabricated_rtt_ms(&RegionId::new("mock-east")),
            fabricated_rtt_ms(&RegionId::new("mock-west"))
        );
    }

    #[tokio::test]
    async fn region_table_gives_latency_and_price_equal_weight() {
        let provider = MockProvider::with_default_regions(ProviderKind::Aws);
        let regions = provider.regions();
        let mut candidates = Vec::new();
        for r in &regions {
            candidates.push((r.clone(), provider.price(&r.id).await.unwrap()));
        }
        let ranked = rank(&mock_matrix(&regions), &candidates);
        let rows = render_region_table(&ranked);
        assert_eq!(rows.len(), regions.len() + 1);
        assert!(rows[0].contains("WORST RTT") && rows[0].contains("HOURLY"));
        for row in &rows[1..] {
            assert!(row.contains("ms"), "row missing latency: {row}");
            assert!(row.contains('$'), "row missing price: {row}");
            assert!(row.contains("/hr"), "row missing hourly unit: {row}");
        }
    }

    #[test]
    fn mock_regions_are_absent_from_the_probe_catalog() {
        let provider = MockProvider::with_default_regions(ProviderKind::Aws);
        let regions = provider.regions();
        // No catalog entry matches a mock region, so the real-provider code
        // path would build an empty matrix here.
        assert!(catalog_targets(ProviderKind::Aws, &regions).is_empty());
        // An empty matrix still ranks (by price) and still renders.
        let mut candidates = Vec::new();
        for r in &regions {
            candidates.push((
                r.clone(),
                Price {
                    hourly_microusd: 10_000,
                    egress_microusd_per_gb: 90_000,
                    included_egress_gb: 0,
                },
            ));
        }
        let ranked = rank(&ProbeMatrix::new(), &candidates);
        let rows = render_region_table(&ranked);
        assert_eq!(rows.len(), candidates.len() + 1);
        for row in &rows[1..] {
            assert!(row.contains('$'));
        }
    }

    /// A release-shaped pin set with two distinct downloads, so a test that
    /// selects the wrong one cannot pass by coincidence.
    fn both_pins() -> jamstream_cloud::PinnedServerArtifacts {
        jamstream_cloud::PinnedServerArtifacts {
            x86_64: Some(jamstream_cloud::PinnedServerArtifact {
                url: "https://github.com/sean-reid/jamstream/releases/download/v1/jamstreamd-linux-x86_64-musl",
                sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            }),
            aarch64: Some(jamstream_cloud::PinnedServerArtifact {
                url: "https://github.com/sean-reid/jamstream/releases/download/v1/jamstreamd-linux-aarch64-musl",
                sha256: "4444444444444444444444444444444444444444444444444444444444444444",
            }),
        }
    }

    #[test]
    fn artifact_precedence_is_flags_then_pinned_then_error() {
        let pinned = both_pins();
        // Explicit flags outrank the pin, on either architecture.
        for arch in [
            jamstream_cloud::ServerArch::X86_64,
            jamstream_cloud::ServerArch::Aarch64,
        ] {
            let (url, sha) = resolve_artifact(
                true,
                arch,
                Some("https://own.example/jamstreamd"),
                Some("1111111111111111111111111111111111111111111111111111111111111111"),
                pinned,
            )
            .unwrap();
            assert_eq!(url, "https://own.example/jamstreamd");
            assert_eq!(sha, "1".repeat(64));
        }
        // No flags: the pin for the launch's architecture fills in.
        let (url, sha) = resolve_artifact(
            true,
            jamstream_cloud::ServerArch::X86_64,
            None,
            None,
            pinned,
        )
        .unwrap();
        assert_eq!(url, pinned.x86_64.unwrap().url);
        assert_eq!(sha, pinned.x86_64.unwrap().sha256);
        // No flags, no pins, cloud launch: the error explains why and both
        // ways out.
        let err = resolve_artifact(
            true,
            jamstream_cloud::ServerArch::X86_64,
            None,
            None,
            jamstream_cloud::PinnedServerArtifacts::default(),
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("not a release build"), "error was: {text}");
        assert!(text.contains("--artifact-url"), "error was: {text}");
    }

    /// #139 root cause: the pin followed the build, not the machine. The
    /// architecture must come from the provider doing the launching, and
    /// the two real cloud providers with fixed instance families must pull
    /// opposite pins from the same build.
    #[test]
    fn aws_selects_the_arm64_pin_and_digitalocean_the_x86_64_one() {
        use jamstream_cloud::providers::{aws::AwsProvider, digitalocean::DigitalOceanProvider};
        let pinned = both_pins();
        let aws = AwsProvider::new("AKIATEST".to_owned(), "secret".to_owned());
        let (url, _) = resolve_artifact(true, aws.server_arch(), None, None, pinned).unwrap();
        assert!(url.ends_with("jamstreamd-linux-aarch64-musl"), "{url}");
        let digitalocean = DigitalOceanProvider::new("t".to_owned());
        let (url, _) =
            resolve_artifact(true, digitalocean.server_arch(), None, None, pinned).unwrap();
        assert!(url.ends_with("jamstreamd-linux-x86_64-musl"), "{url}");
    }

    /// A launch whose architecture has no pin must refuse before a machine
    /// is paid for, and the error must name the architecture, because
    /// launching would produce exactly the dead VM of #139.
    #[test]
    fn a_missing_arch_pin_refuses_to_launch_naming_the_architecture() {
        let x86_only = jamstream_cloud::PinnedServerArtifacts {
            aarch64: None,
            ..both_pins()
        };
        let err = resolve_artifact(
            true,
            jamstream_cloud::ServerArch::Aarch64,
            None,
            None,
            x86_only,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("aarch64"), "error was: {err}");
        assert!(err.contains("--artifact-url"), "error was: {err}");
        // The override remains the escape hatch and applies to the
        // architecture being launched.
        let (url, _) = resolve_artifact(
            true,
            jamstream_cloud::ServerArch::Aarch64,
            Some("https://own.example/jamstreamd-arm64"),
            Some("3333333333333333333333333333333333333333333333333333333333333333"),
            x86_only,
        )
        .unwrap();
        assert_eq!(url, "https://own.example/jamstreamd-arm64");
    }

    /// The overrides are the one part of the VM's root bootstrap a user
    /// types, so they are checked before a machine is paid for.
    #[test]
    fn artifact_overrides_are_validated_before_anything_launches() {
        let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        for (url, sha, what) in [
            ("http://own.example/jamstreamd", sha, "https"),
            ("https://own.example/a\";id;\"", sha, "url"),
            ("https://own.example/jamstreamd", "abcd", "64 hex digits"),
        ] {
            let err = resolve_artifact(
                true,
                jamstream_cloud::ServerArch::X86_64,
                Some(url),
                Some(sha),
                jamstream_cloud::PinnedServerArtifacts::default(),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains(what), "{url} {sha} was rejected as {err:?}");
        }
        // A valid pair still passes through untouched, on a local launch
        // as well as a cloud one.
        for needs_download in [true, false] {
            let (url, out_sha) = resolve_artifact(
                needs_download,
                jamstream_cloud::ServerArch::X86_64,
                Some("https://own.example/d"),
                Some(sha),
                jamstream_cloud::PinnedServerArtifacts::default(),
            )
            .unwrap();
            assert_eq!(url, "https://own.example/d");
            assert_eq!(out_sha, sha);
        }
    }

    #[test]
    fn local_and_mock_launches_use_placeholders_unless_overridden() {
        // No download happens, so no flags and no pin is fine.
        let (url, sha) = resolve_artifact(
            false,
            jamstream_cloud::ServerArch::X86_64,
            None,
            None,
            jamstream_cloud::PinnedServerArtifacts::default(),
        )
        .unwrap();
        assert_eq!(url, PLACEHOLDER_ARTIFACT_URL);
        assert_eq!(sha, PLACEHOLDER_ARTIFACT_SHA256);
        // Explicit flags still win everywhere.
        let (url, _) = resolve_artifact(
            false,
            jamstream_cloud::ServerArch::X86_64,
            Some("https://own.example/jamstreamd"),
            Some("2222222222222222222222222222222222222222222222222222222222222222"),
            jamstream_cloud::PinnedServerArtifacts::default(),
        )
        .unwrap();
        assert_eq!(url, "https://own.example/jamstreamd");
    }

    /// The point of #121: a session on this machine is offered on loopback
    /// first, so a same-machine join never leaves the host and the macOS
    /// Application Firewall never gets a vote. The LAN address stays for
    /// the bandmate on the same network.
    #[test]
    fn a_session_on_this_machine_offers_loopback_first() {
        let lan: IpAddr = "192.168.1.12".parse().unwrap();
        assert_eq!(
            candidates_for(SocketAddr::new(lan, 43210), lan),
            vec![
                "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
                "192.168.1.12:43210".parse().unwrap(),
            ]
        );
        // Same for a v6 host address: ::1, not 127.0.0.1.
        let lan6: IpAddr = "fd00::5".parse().unwrap();
        assert_eq!(
            candidates_for(SocketAddr::new(lan6, 43210), lan6),
            vec![
                "[::1]:43210".parse::<SocketAddr>().unwrap(),
                "[fd00::5]:43210".parse().unwrap(),
            ]
        );
    }

    /// A cloud VM is not this machine, so a cloud invite is exactly what it
    /// was: one address. Offering loopback there would cost every guest a
    /// connection timeout against their own machine for nothing.
    #[test]
    fn a_server_elsewhere_is_offered_once() {
        let this_machine: IpAddr = "192.168.1.12".parse().unwrap();
        let vm: SocketAddr = "203.0.113.7:43210".parse().unwrap();
        assert_eq!(candidates_for(vm, this_machine), vec![vm]);
        // And a host with no network at all already carries loopback; it
        // must not be listed twice.
        let loopback: SocketAddr = "127.0.0.1:43210".parse().unwrap();
        assert_eq!(
            candidates_for(loopback, IpAddr::V4(Ipv4Addr::LOCALHOST)),
            vec![loopback]
        );
    }

    /// Minting spends one discovery of the machine's own address for the
    /// whole book, and every seat is offered the same places.
    #[test]
    fn every_seat_in_a_session_is_offered_the_same_addresses() {
        let invites = mint_seats(3, 1);
        let first = invites[0].1.addresses.clone();
        assert!(!first.is_empty());
        for (label, invite) in &invites {
            assert_eq!(
                invite.addresses, first,
                "{label} was offered {:?}",
                invite.addresses
            );
        }
    }

    #[test]
    fn invite_labels() {
        assert_eq!(invite_label(Role::Musician, MemberId(0)), "host");
        assert_eq!(invite_label(Role::Musician, MemberId(1)), "musician 1");
        assert_eq!(invite_label(Role::Listener, MemberId(5)), "listener 5");
    }

    fn mint_seats(musicians: u8, listeners: u8) -> Vec<(String, Invite)> {
        let issuer = Issuer::generate();
        let keys = generate_keypair();
        mint_invites(
            &issuer,
            SessionId::generate(),
            "10.0.0.1:43210".parse().unwrap(),
            keys.public,
            9_999,
            musicians,
            listeners,
        )
    }

    #[test]
    fn invites_cover_host_musicians_then_listeners() {
        // Three musician seats: the host's own plus two guests.
        let invites = mint_seats(3, 2);
        let labels: Vec<&str> = invites.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "host",
                "musician 1",
                "musician 2",
                "listener 3",
                "listener 4"
            ]
        );
        assert!(invites.iter().all(|(_, i)| i.token.expires_unix == 9_999));
        assert_eq!(invites[0].1.token.member_id, MemberId(0));
        assert_eq!(invites[0].1.token.role, Role::Musician);
        assert_eq!(invites[4].1.token.role, Role::Listener);
    }

    // The seat count includes the host, so what gets minted is exactly what
    // the server admits: at the cap, every minted musician invite has a seat,
    // and one more would be refused (see the session crate's
    // musician_capacity_enforced).
    #[test]
    fn minted_musician_seats_match_what_the_server_admits() {
        let at_cap = mint_seats(MAX_MUSICIANS as u8, 0);
        let musicians: Vec<&Invite> = at_cap
            .iter()
            .map(|(_, i)| i)
            .filter(|i| i.token.role == Role::Musician)
            .collect();
        assert_eq!(
            musicians.len(),
            MAX_MUSICIANS,
            "a session of MAX_MUSICIANS seats mints MAX_MUSICIANS musician invites, host included"
        );
        assert!(
            musicians
                .iter()
                .any(|i| i.token.member_id == HOST_MEMBER_ID),
            "the host holds one of those seats"
        );
        let mut ids: Vec<u16> = musicians.iter().map(|i| i.token.member_id.0).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), MAX_MUSICIANS, "seats are distinct members");

        // Listener member ids start after the musician seats, so no listener
        // ever collides with a musician seat.
        let with_listeners = mint_seats(MAX_MUSICIANS as u8, 2);
        assert_eq!(with_listeners.len(), MAX_MUSICIANS + 2);
        assert_eq!(
            with_listeners[MAX_MUSICIANS].1.token.member_id,
            MemberId(MAX_MUSICIANS as u16)
        );
    }

    // The floor: one seat is the host alone, and one invite is minted.
    #[test]
    fn a_solo_session_mints_only_the_host_invite() {
        let solo = mint_seats(1, 0);
        assert_eq!(solo.len(), 1);
        assert_eq!(solo[0].0, "host");
        assert_eq!(solo[0].1.token.member_id, HOST_MEMBER_ID);
    }
}
