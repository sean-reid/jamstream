//! The local session mode story, end to end through the real CLI code:
//! `jamstream host --provider local` spawns a real jamstreamd on this
//! machine and answers a real handshake, a headless client joins through
//! the printed invite and plays a sine fixture, and `jamstream end` kills
//! the process and leaves the registry empty.
//!
//! # Server binary location
//!
//! CARGO_BIN_EXE_<name> only covers binaries of the package under test, so
//! the cli tests cannot ask Cargo for jamstreamd directly. Instead the test
//! derives the profile directory from its own executable path
//! (target/<profile>/deps/<test>-<hash>), runs `cargo build -p
//! jamstream-server --bin jamstreamd` against the workspace to guarantee
//! the binary exists and is fresh (a no-op when it already is), and points
//! JAMSTREAMD_PATH at target/<profile>/jamstreamd.
//!
//! One test function: the state directory override is process-global env.

mod common;

use std::net::Ipv4Addr;
use std::path::PathBuf;

use common::fixture;
use jamstream_cli::cli::{EndArgs, HostArgs, JoinArgs};
use jamstream_cli::{end, host, join, providers, state, sweep};

#[cfg(windows)]
const BIN_NAME: &str = "jamstreamd.exe";
#[cfg(not(windows))]
const BIN_NAME: &str = "jamstreamd";

/// Builds (if needed) and returns the jamstreamd binary for this profile.
fn jamstreamd_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let profile_dir = exe
        .parent() // deps/
        .and_then(|d| d.parent()) // target/<profile>/
        .expect("test executable must sit in target/<profile>/deps")
        .to_path_buf();
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = std::process::Command::new(cargo);
    build.args(["build", "-p", "jamstream-server", "--bin", "jamstreamd"]);
    if profile_dir.file_name().is_some_and(|n| n == "release") {
        build.arg("--release");
    }
    let status = build.current_dir(&workspace).status();

    let binary = profile_dir.join(BIN_NAME);
    match status {
        Ok(s) if s.success() => {}
        // A failed or unavailable cargo is tolerable if an earlier build
        // already produced the binary; without one the test cannot run.
        _ if binary.is_file() => eprintln!(
            "warning: cargo build -p jamstream-server failed; using the existing {}",
            binary.display()
        ),
        _ => panic!(
            "cannot build jamstreamd and none exists at {}; \
             run `cargo build -p jamstream-server` first",
            binary.display()
        ),
    }
    assert!(
        binary.is_file(),
        "cargo build succeeded but {} is missing; unusual target layout?",
        binary.display()
    );
    binary
}

/// Bind-then-drop; racy in principle, unique enough in practice.
fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

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

    // Host through the real provider resolution, exactly as main.rs does.
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
        artifact_url: None,
        artifact_sha256: None,
        yes: true,
        json: true,
    };
    let provider = providers::resolve("local").unwrap();
    let mut out = Vec::new();
    host::run(&args, provider.as_ref(), &mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("host --json must emit exactly one JSON object ({e}): {text}"));

    // Zero price, local provider, and a reachability check that actually
    // ran: a real server answered a real encrypted handshake.
    assert_eq!(json["provider"], "local");
    assert_eq!(json["region"], "local");
    assert_eq!(json["hourly_microusd"], 0);
    assert_eq!(json["estimated_total_microusd"], 0);
    assert_eq!(json["reachability"], "ok");
    // One invite per seat, the host's included: --musicians 2 mints exactly
    // two musician invites, never three, so no invite exists that the
    // server's capacity check would refuse.
    let invites = json["invites"].as_array().unwrap();
    assert_eq!(invites.len(), 2, "two musician seats: host + 1 guest");
    assert_eq!(invites[0]["role"], "host");
    assert_eq!(invites[1]["role"], "musician 1");
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
