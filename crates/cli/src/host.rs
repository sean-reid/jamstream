//! `jamstream host`: rank regions, preview cost, launch the session VM,
//! mint invites, verify reachability, and record the session on disk.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

use data_encoding::{BASE64, HEXLOWER};
use jamstream_cloud::{
    BootConfig, CostPreview, HANDSHAKE_CAP, InstanceClass, LaunchSpec, Price, ProbeMatrix,
    ProbeTarget, Provider, ProviderKind, RecordingEstimate, RecordingPlan, RecordingStorage,
    Region, RegionId, RegionScore, RetentionEnforcement, rank, session_tag,
};
use jamstream_protocol::ids::{HOST_MEMBER_ID, MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::generate_keypair;

use crate::CliError;
use crate::cli::HostArgs;
use crate::launch::{self, ArtifactOverride};
use crate::reason::{self, Attempt};
use crate::state::{self, InviteRecord, SessionState, SessionStatus};

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
    let preexisting = provider.list_tagged(None).await?.instances;
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

    let preview = launch_preview(&price, args, recording.as_ref())?;
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
    let (artifact_url, artifact_sha256) = launch::resolve_artifact(
        !is_mock && !is_local,
        provider.server_arch(),
        args.artifact_url.as_deref(),
        args.artifact_sha256.as_deref(),
        jamstream_cloud::pinned(),
        ArtifactOverride::Flags,
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
        self_destruct: launch::self_destruct_for(provider.kind(), digitalocean_token())?,
        // A local session records through the provider's own spawn flags
        // (see LocalProvider::with_record) rather than the boot config, so
        // this is set for a bucket and nothing else.
        recording: recording.clone(),
    };

    // Proved before the machine exists: a take must never fail mid-song
    // because the bucket was wrong, and the retention rule has to be in place
    // before the first byte is uploaded. The lifecycle call is the host's to
    // make, not the VM's, whose key is scoped to writing one prefix.
    let mut retention_applied = None;
    if let Some(storage) = &recording {
        let applied = verify_bucket(storage, &session_hex).await?;
        if !args.json {
            writeln!(out, "{}", applied.describe())?;
        }
        retention_applied = Some(applied);
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
    let instance = launch::wait_for_ip(provider, &session_hex, instance).await?;
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
        launch::verify_reachable(&invites[0].1, HANDSHAKE_CAP).await?;
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
    let recording_record = recording
        .as_ref()
        .zip(retention_applied.as_ref())
        .map(|(storage, applied)| recording_record(storage, applied));
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

/// The launch's cost as the host is shown it: the session, plus the recording
/// when one is armed.
///
/// A recording is not free: `--bucket` buys storage for the retention
/// period and one download's worth of egress, which on a four piece
/// recording stems for three hours is most of the bill, and the preview
/// must account for it. Storage prices in the bucket's own region, which
/// is the region the launch is putting it in.
pub fn launch_preview(
    price: &Price,
    args: &HostArgs,
    recording: Option<&RecordingStorage>,
) -> Result<CostPreview, CliError> {
    let preview = CostPreview::compute(
        price,
        args.hours,
        args.musicians,
        args.destinations,
        args.listeners,
    );
    let Some(storage) = recording else {
        return Ok(preview);
    };
    // One stem per musician, because that is what the recorder writes: the flag
    // is on or off and the count comes from the session.
    let stems = if storage.stems { args.musicians } else { 0 };
    let plan = RecordingPlan {
        stems,
        ..RecordingPlan::default()
    }
    .retention(storage.retention);
    // Cannot fail here: `storage_for_launch` priced this same provider and
    // region before it built the storage config, and refused a region with no
    // bucket service. Propagated rather than dropped anyway, because a total
    // that silently omits the recording is the defect this closes.
    let estimate = RecordingEstimate::compute(
        storage.provider,
        &RegionId::new(storage.region.clone()),
        &plan,
        args.hours,
    )?;
    Ok(preview.with_recording(&estimate))
}

/// The bucket config a cloud launch carries to the VM, from the flags plus
/// the storage key in this machine's environment.
fn recording_storage(
    args: &HostArgs,
    kind: ProviderKind,
    bucket: &str,
    region: &RegionId,
) -> Result<RecordingStorage, CliError> {
    crate::storage::storage_for_launch(
        kind,
        bucket,
        region,
        args.retention,
        || crate::storage::credential_from_env(kind),
        args.record_stems,
    )
}

/// What the session record keeps about the bucket: everything but the key.
///
/// `applied` is what the launch's retention call answered, kept because it is
/// asked once and read for as long as the takes exist: a surface listing them
/// weeks later cannot otherwise tell a countdown that is real from one nothing
/// will perform.
pub fn recording_record(
    storage: &RecordingStorage,
    applied: &RetentionEnforcement,
) -> state::RecordingRecord {
    state::RecordingRecord {
        provider: storage.provider.as_str().to_owned(),
        bucket: storage.bucket.clone(),
        region: storage.region.clone(),
        retention: storage.retention.to_string(),
        stems: storage.stems,
        applied: Some(state::RetentionApplied::from_enforcement(applied)),
    }
}

/// Proves the key can write this session's prefix, and applies the retention
/// rule to it.
///
/// Public because both launch surfaces arm recording through it: this command,
/// and the desktop app's wizard.
pub async fn verify_bucket(
    storage: &RecordingStorage,
    session_hex: &str,
) -> Result<RetentionEnforcement, CliError> {
    let store = storage.object_store()?;
    let prefix = jamstream_cloud::session_prefix(session_hex);
    probe_prefix(store.as_ref(), &storage.bucket, &storage.region, &prefix).await?;
    Ok(store
        .set_retention(&storage.bucket, &prefix, storage.retention)
        .await?)
}

/// [`verify_bucket`] without the lifecycle rule, for a key being saved rather
/// than a session being launched.
///
/// A retention rule belongs to one session's prefix, so a credential check that
/// applied one would leave a rule behind for a session that never happened. The
/// write is what a check is for.
pub async fn probe_bucket(storage: &RecordingStorage, session_hex: &str) -> Result<(), CliError> {
    let store = storage.object_store()?;
    let prefix = jamstream_cloud::session_prefix(session_hex);
    probe_prefix(store.as_ref(), &storage.bucket, &storage.region, &prefix).await
}

/// Writes one probe object under `prefix` and deletes it.
///
/// A bucket that refuses the key fails here, in a configuring frame of mind,
/// rather than at the first take. The failure says what the bucket refused
/// and names the prefix a key has to be able to write; the provider's own
/// response goes to the log, because for S3 it is a document naming the
/// account number and the IAM ARN.
pub async fn probe_prefix(
    store: &dyn jamstream_cloud::ObjectStore,
    bucket: &str,
    region: &str,
    prefix: &str,
) -> Result<(), CliError> {
    let probe = format!("{prefix}.jamstream-probe");
    store
        .put(
            bucket,
            &probe,
            jamstream_cloud::JSON_CONTENT_TYPE,
            b"{\"probe\":true}",
        )
        .await
        .map_err(|err| {
            let refusal = reason::provider_sentence(Attempt::Probe, Some(store.kind()), &err);
            tracing::warn!("writing the probe object to {bucket} in {region}: {err}");
            CliError::Failed(format!(
                "cannot write to {bucket} in {region}. {refusal} Recording needs a key that \
                 may write {prefix} and set the bucket's lifecycle rule."
            ))
        })?;
    store.delete(bucket, &probe).await?;
    Ok(())
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

/// The token a droplet needs to delete itself, from this machine's
/// environment. The app reads its own from the credential store; see
/// [`crate::launch::self_destruct_for`].
fn digitalocean_token() -> Option<String> {
    std::env::var("DIGITALOCEAN_TOKEN").ok()
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
        let applied = RetentionEnforcement::Manual {
            retention: args.retention,
            note: "this target has no lifecycle API".to_owned(),
        };
        let record = recording_record(&storage, &applied);
        // What the bucket did with the rule is on the record, because a
        // surface listing these takes later cannot ask the bucket again.
        assert_eq!(
            record.applied,
            Some(state::RetentionApplied::Unenforced {
                note: applied.describe()
            })
        );
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

    /// A host arming a recording must be shown what it costs. The preview
    /// folds the recording in, so the total a host agrees to is the
    /// total, and stems, which are the expensive choice, move it.
    #[test]
    fn the_preview_prices_the_recording_a_launch_is_arming() {
        let price = Price {
            hourly_microusd: 16_800,
            egress_microusd_per_gb: 90_000,
            included_egress_gb: 0,
        };
        let args = HostArgs {
            bucket: Some("my-jams".to_owned()),
            hours: 3.0,
            musicians: 4,
            record_stems: false,
            ..recording_args()
        };
        let storage = |stems: bool| RecordingStorage {
            provider: ProviderKind::Aws,
            bucket: "my-jams".to_owned(),
            region: "eu-west-1".to_owned(),
            retention: args.retention,
            credential: jamstream_cloud::StorageCredential::KeyPair {
                access_key_id: "AKIDTEST".to_owned(),
                secret_access_key: "hunter2".to_owned(),
            },
            stems,
        };
        let session = launch_preview(&price, &args, None).expect("a session with no bucket");
        let mix = launch_preview(&price, &args, Some(&storage(false))).expect("mix only");
        let stems = launch_preview(&price, &args, Some(&storage(true))).expect("mix and stems");

        // Recording is never free, so the figure has to carry it.
        assert!(
            mix.total_microusd > session.total_microusd,
            "recording the mix has to cost something: {} vs {}",
            mix.total_microusd,
            session.total_microusd
        );
        // The whole point of showing it: stems are the choice with a price.
        assert!(
            stems.total_microusd > mix.total_microusd,
            "four stems must cost more than the mix alone: {} vs {}",
            stems.total_microusd,
            mix.total_microusd
        );
        // The stem count is the session's, not a flag: the recorder writes one
        // per musician, and the estimate has to price that many.
        let rows = stems.display_table().join("\n");
        assert!(rows.contains("mix + 4 stems"), "{rows}");
        assert!(
            mix.display_table().join("\n").contains("mix only"),
            "the mix-only launch must not be priced for stems"
        );
        // Both halves of what a recording costs are named, because storage and
        // the download are billed at different times.
        assert!(rows.contains("Recording"), "{rows}");
        assert!(rows.contains("Download once"), "{rows}");
        // And the session's own lines survive the fold.
        assert_eq!(session.line_items.len() + 2, stems.line_items.len());
        assert_eq!(stems.line_items[0], session.line_items[0]);
        assert_eq!(
            stems.egress_bytes_estimate, session.egress_bytes_estimate,
            "the recording's bytes are storage and a later download, not \
             session egress"
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

    /// A session on this machine is offered on loopback first, so a
    /// same-machine join never leaves the host and the macOS Application
    /// Firewall never gets a vote. The LAN address stays for the bandmate
    /// on the same network.
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
