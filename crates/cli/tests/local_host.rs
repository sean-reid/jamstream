//! The local session mode story, end to end through the real CLI code:
//! `jamstream host --provider local` spawns a real jamstreamd on this
//! machine and answers a real handshake, a headless client joins through
//! the printed invite and plays a sine fixture, and `jamstream end` kills
//! the process and leaves the registry empty.
//!
//! # Server binary location
//!
//! See `common::jamstreamd_binary`, which builds jamstreamd for this
//! profile and returns the path JAMSTREAMD_PATH should name.
//!
//! One test function: the state directory override is process-global env.

mod common;

use std::net::{IpAddr, Ipv4Addr};

use common::{ServerGuard, fixture, free_udp_port, jamstreamd_binary};
use jamstream_cli::cli::{EndArgs, HostArgs, JoinArgs};
use jamstream_cli::{end, host, join, providers, state, sweep};
use jamstream_cloud::providers::local::LocalProvider;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_host_join_and_end_story() {
    let state_dir = std::env::temp_dir().join(format!(
        "jamstream-cli-local-host-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let server_binary = jamstreamd_binary();
    // Safety: this test binary is single-test and sets both variables
    // before any state or provider access.
    unsafe {
        std::env::set_var(state::STATE_DIR_ENV, &state_dir);
        std::env::set_var("JAMSTREAMD_PATH", &server_binary);
    }

    let guard = ServerGuard::new();

    let args = HostArgs {
        provider: "local".to_owned(),
        region: None,
        // Two musician seats: this host plus one guest. --musicians counts
        // the host, so this is the smallest session with someone to jam
        // with, and the server admits exactly these two.
        musicians: 2,
        listeners: 0,
        hours: 1.0,
        destinations: 0,
        port: free_udp_port(),
        idle_min: 10,
        max_hours: 12,
        record: false,
        record_stems: false,
        bucket: None,
        retention: jamstream_cloud::Retention::Days30,
        artifact_url: None,
        artifact_sha256: None,
        yes: true,
        json: true,
    };
    // Confined to loopback. The provider is built here rather than through
    // providers::resolve so the bind can be set: the macOS Application
    // Firewall filters incoming connections per binary and every rebuild of
    // jamstreamd is a new binary, so a server bound to every interface
    // raises a dialog on a developer's machine and drops this test's
    // datagrams until somebody answers it. Loopback is the one path it does
    // not govern. Resolution itself is still covered, by the fresh provider
    // at the end of this test.
    let provider = LocalProvider::new(state_dir.clone()).with_bind(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let mut out = Vec::new();
    let hosted = host::run(&args, &provider, &mut out).await;
    let text = String::from_utf8(out).unwrap();
    // The pid is in the output whether or not host::run succeeded past it,
    // so the guard learns about the server before anything below can panic.
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(pid) = json["instance_id"].as_str()
    {
        guard.watch(pid);
    }
    hosted.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("host --json must emit exactly one JSON object ({e}): {text}"));

    // The session is on loopback end to end: the server listens there and
    // the invites say so, so nothing this test does crosses an interface a
    // firewall can filter.
    assert_eq!(json["address"], format!("127.0.0.1:{}", args.port));

    // Zero price, local provider, and a reachability check that actually
    // ran: a real server answered a real encrypted handshake.
    //
    // On a machine with the macOS Application Firewall on, that handshake
    // is only possible because the invite offers loopback: the LAN path to
    // a freshly built jamstreamd is filtered per binary and nothing comes
    // back until somebody answers a dialog. Loopback is not filtered, so
    // this assertion is what makes the reachability check below meaningful
    // rather than luck.
    assert_eq!(json["provider"], "local");
    assert_eq!(json["region"], "local");
    assert_eq!(json["hourly_microusd"], 0);
    assert_eq!(json["estimated_total_microusd"], 0);
    assert_eq!(json["reachability"], "ok");
    // Recording was not asked for, so there is no directory to go looking
    // in; the recording story itself lives in local_record.rs.
    assert!(json["record_dir"].is_null(), "output: {json}");
    // One invite per seat, the host's included: --musicians 2 mints exactly
    // two musician invites, never three, so no invite exists that the
    // server's capacity check would refuse.
    let invites = json["invites"].as_array().unwrap();
    assert_eq!(invites.len(), 2, "two musician seats: host + 1 guest");
    assert_eq!(invites[0]["role"], "host");
    assert_eq!(invites[1]["role"], "musician 1");
    // Every invite offers loopback, and only loopback, because this session
    // is bound to loopback: that is the one place the server is listening,
    // so it is the only address an invite can honestly carry. A default
    // session binds every interface and does offer the LAN address second,
    // for the bandmate on the same network; `candidates_for` in host.rs
    // asserts that rule directly, over v4 and v6, with no socket for a
    // firewall to filter.
    for i in invites {
        let encoded = i["invite"].as_str().unwrap();
        let invite = jamstream_protocol::invite::Invite::decode(encoded).unwrap();
        assert_eq!(
            invite.addresses.first().map(ToString::to_string),
            Some(format!("127.0.0.1:{}", args.port)),
            "{} was not offered loopback first: {:?}",
            i["role"],
            invite.addresses
        );
        assert_eq!(
            invite.addresses.len(),
            1,
            "{} was offered somewhere nothing is listening: {:?}",
            i["role"],
            invite.addresses
        );
    }
    let musician_seats = invites
        .iter()
        .filter(|i| {
            let encoded = i["invite"].as_str().unwrap();
            let invite = jamstream_protocol::invite::Invite::decode(encoded).unwrap();
            invite.token.role == jamstream_protocol::ids::Role::Musician
        })
        .count();
    assert_eq!(musician_seats, usize::from(args.musicians));
    assert!(musician_seats <= jamstream_session::MAX_MUSICIANS);

    // The state file records provider "local".
    let session_id = json["session_id"].as_str().unwrap();
    let session = state::load(&state_dir.join(format!("{session_id}.json"))).unwrap();
    assert_eq!(session.provider, "local");

    // A headless musician joins through the printed invite and plays a
    // sine fixture for 2 s. Alone in the session the personal mix excludes
    // self, so silence in the output is correct; Joined plus a clean exit
    // is the assertion.
    let output_wav = state_dir.join("local-mix.wav");
    let join_args = JoinArgs {
        invite: invites[0]["invite"].as_str().unwrap().to_owned(),
        headless: true,
        input: fixture("sine-440-48k.wav"),
        output: output_wav.clone(),
        duration_secs: 2,
        chat: None,
        name: None,
        revoke_invite: None,
        revoke_after_secs: None,
    };
    let mut out = Vec::new();
    join::run(&join_args, &mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("joined"), "join output: {text}");
    assert!(text.contains("left after 2 s"), "join output: {text}");

    // End destroys the spawned process through the recorded provider.
    let (path, session) = end::select(&EndArgs {
        session: Some(session_id[..8].to_owned()),
        last: false,
    })
    .unwrap();
    let end_provider = end::resolve_provider(&session).unwrap();
    let instance_pid = session.instance_id.clone();
    let mut out = Vec::new();
    end::run(&path, session, end_provider.as_ref(), &mut out)
        .await
        .unwrap();
    assert!(String::from_utf8(out).unwrap().contains("ended"));
    guard.disarm();

    // The process is gone, not just deregistered. The host-time provider
    // in this very process still holds the unreaped child handle, so the
    // pid can linger as a zombie; like production liveness, a zombie
    // counts as dead.
    #[cfg(unix)]
    {
        let stat = std::process::Command::new("ps")
            .args(["-p", &instance_pid, "-o", "stat="])
            .output()
            .expect("ps");
        let alive = stat.status.success()
            && !String::from_utf8_lossy(&stat.stdout)
                .trim_start()
                .starts_with('Z');
        assert!(!alive, "jamstreamd pid {instance_pid} survived end");
    }
    #[cfg(not(unix))]
    let _ = instance_pid;

    // A fresh provider on the same state dir sees an empty registry, and a
    // sweep dry-run confirms nothing local is left.
    let fresh = providers::resolve("local").unwrap();
    assert!(fresh.list_tagged(None).await.unwrap().is_empty());
    let mut out = Vec::new();
    sweep::run(&[fresh], true, &mut out).await.unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("No jamstream-tagged instances found.")
    );

    std::fs::remove_dir_all(&state_dir).unwrap();
}
