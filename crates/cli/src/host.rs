//! `jamstream host`: rank regions, preview cost, launch the session VM,
//! mint invites, verify reachability, and record the session on disk.

use std::io::Write;
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use data_encoding::{BASE64, HEXLOWER};
use jamstream_cloud::{
    BootConfig, CostPreview, InstanceClass, LaunchSpec, Price, ProbeMatrix, ProbeTarget, Provider,
    ProviderKind, Region, RegionId, RegionScore, SelfDestruct, rank, session_tag,
};
use jamstream_protocol::ids::{HOST_MEMBER_ID, MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_session::client::{ClientCore, ClientState};

use crate::CliError;
use crate::cli::HostArgs;
use crate::state::{self, InviteRecord, SessionState, SessionStatus};

const IP_WAIT_CAP: Duration = Duration::from_secs(180);
const IP_POLL_PERIOD: Duration = Duration::from_secs(2);
const HANDSHAKE_CAP: Duration = Duration::from_secs(60);

/// Placeholders the mock provider accepts; a real provider requires the
/// artifact flags because the VM would download and verify this for real.
const PLACEHOLDER_ARTIFACT_URL: &str = "https://artifacts.invalid/jamstreamd";
const PLACEHOLDER_ARTIFACT_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

pub async fn run<W: Write>(
    args: &HostArgs,
    provider: &dyn Provider,
    out: &mut W,
) -> Result<(), CliError> {
    let is_mock = args.provider == "mock";
    let regions = provider.regions();
    if regions.is_empty() {
        return Err(CliError::Failed(format!(
            "provider {} offers no regions",
            args.provider
        )));
    }
    let mut candidates: Vec<(Region, Price)> = Vec::with_capacity(regions.len());
    for region in &regions {
        candidates.push((region.clone(), provider.price(&region.id).await?));
    }

    let (region, price) = choose_region(args, provider, &candidates, is_mock, out).await?;

    let preview = CostPreview::compute(
        &price,
        args.hours,
        args.musicians,
        args.destinations,
        args.listeners,
    );
    if !args.json {
        writeln!(out)?;
        writeln!(
            out,
            "Cost preview for {} {} over {:.1} hours:",
            args.provider, region.id, args.hours
        )?;
        for row in preview.display_table() {
            writeln!(out, "{row}")?;
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

    if !is_mock && (args.artifact_url.is_none() || args.artifact_sha256.is_none()) {
        return Err(CliError::Usage(
            "pass --artifact-url and --artifact-sha256; the VM downloads and verifies \
             jamstreamd at boot"
                .to_owned(),
        ));
    }
    let boot = BootConfig {
        artifact_url: args
            .artifact_url
            .clone()
            .unwrap_or_else(|| PLACEHOLDER_ARTIFACT_URL.to_owned()),
        artifact_sha256: args
            .artifact_sha256
            .clone()
            .unwrap_or_else(|| PLACEHOLDER_ARTIFACT_SHA256.to_owned()),
        server_private_key_b64: BASE64.encode(&server_keys.private),
        issuer_public_key_b64: BASE64.encode(issuer.public_key().as_bytes()),
        session_id_hex: session_hex.clone(),
        port: args.port,
        idle_shutdown_min: args.idle_min,
        max_duration_min: args.max_hours * 60,
        self_destruct: self_destruct_for(provider.kind()),
    };

    let spec = LaunchSpec {
        region: region.clone(),
        instance_class: InstanceClass::Standard,
        user_data: jamstream_cloud::cloudinit::render(&boot),
        tags: vec![session_tag(&session_hex)],
    };
    if !args.json {
        writeln!(out)?;
        writeln!(out, "Launching in {}.", region.id)?;
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
            "invites": session_state.invites,
            "state_file": state_path,
        });
        writeln!(out, "{}", serde_json::to_string_pretty(&value)?)?;
    } else {
        writeln!(out)?;
        writeln!(out, "Session {} is running.", &session_hex[..8])?;
        writeln!(out, "{:<12} {address}", "server")?;
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
    }
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

/// Host invite first (member 0), then musicians 1..=N, then listeners.
fn mint_invites(
    issuer: &Issuer,
    session_id: SessionId,
    address: SocketAddr,
    server_pk: [u8; 32],
    expires_unix: u64,
    musicians: u8,
    listeners: u8,
) -> Vec<(String, Invite)> {
    let mint = |member: u16, role: Role| {
        let token = Token {
            member_id: MemberId(member),
            role,
            name_hint: None,
            expires_unix,
            jti: TokenId::generate(),
        };
        let invite = issuer.mint(session_id, vec![address], server_pk, token);
        (invite_label(role, MemberId(member)), invite)
    };
    let mut invites = vec![mint(HOST_MEMBER_ID.0, Role::Musician)];
    for m in 1..=u16::from(musicians) {
        invites.push(mint(m, Role::Musician));
    }
    for l in 0..u16::from(listeners) {
        invites.push(mint(u16::from(musicians) + 1 + l, Role::Listener));
    }
    invites
}

fn self_destruct_for(kind: ProviderKind) -> SelfDestruct {
    match kind {
        // The mock resolves with an Aws kind, so it lands here too; the
        // shutdown script is a harmless placeholder for it.
        ProviderKind::Aws => SelfDestruct::AwsShutdown,
        ProviderKind::Gcp => SelfDestruct::GcpMaxRunDuration,
        // The droplet-scoped token comes with the real DO wiring; until
        // then this arm is unreachable from resolve().
        ProviderKind::DigitalOcean => SelfDestruct::ApiToken {
            endpoint: "https://api.digitalocean.com/v2/droplets".to_owned(),
            token: "unset".to_owned(),
        },
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
async fn verify_reachable(invite: &Invite, cap: Duration) -> Result<(), CliError> {
    let bind: SocketAddr = if invite.addresses[0].is_ipv4() {
        "0.0.0.0:0".parse().expect("static addr")
    } else {
        "[::]:0".parse().expect("static addr")
    };
    let socket = tokio::net::UdpSocket::bind(bind).await?;
    socket.connect(invite.addresses[0]).await?;

    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let (mut core, init) = ClientCore::connect(invite, now())?;
    socket.send(&init).await?;
    let mut buf = [0u8; 2048];
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
            ClientState::TimedOut => {
                let init = core.reconnect(now())?;
                socket.send(&init).await?;
            }
            ClientState::Connecting => {}
        }
    }
    Err(CliError::Failed(format!(
        "server did not complete a handshake within {} s",
        cap.as_secs()
    )))
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

    #[test]
    fn invite_labels() {
        assert_eq!(invite_label(Role::Musician, MemberId(0)), "host");
        assert_eq!(invite_label(Role::Musician, MemberId(1)), "musician 1");
        assert_eq!(invite_label(Role::Listener, MemberId(5)), "listener 5");
    }

    #[test]
    fn invites_cover_host_musicians_then_listeners() {
        let issuer = Issuer::generate();
        let keys = generate_keypair();
        let invites = mint_invites(
            &issuer,
            SessionId::generate(),
            "10.0.0.1:43210".parse().unwrap(),
            keys.public,
            9_999,
            2,
            2,
        );
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
}
