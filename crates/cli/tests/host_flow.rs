//! The hosting lifecycle against an injected MockProvider: host writes a
//! state file and launches an instance, end destroys it and flips the
//! state, and sweep dry-run reports a seeded orphan without touching it.
//!
//! One test function: the state directory override is process-global env,
//! so the whole flow runs in sequence.

use jamstream_cli::cli::{EndArgs, HostArgs, StatusArgs};
use jamstream_cli::state::{self, SessionStatus};
use jamstream_cli::{end, host, status, sweep};
use jamstream_cloud::{MockProvider, Provider, ProviderKind, session_tag};

fn host_args() -> HostArgs {
    HostArgs {
        provider: "mock".to_owned(),
        region: None,
        // Seats, host included: three musician seats is the host plus two.
        musicians: 3,
        listeners: 1,
        hours: 2.0,
        destinations: 0,
        port: 43210,
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
    }
}

#[tokio::test]
async fn host_end_and_sweep_flow() {
    let state_dir = std::env::temp_dir().join(format!(
        "jamstream-cli-host-flow-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    // Safety: this test binary is single-test and sets the variable before
    // any state access.
    unsafe {
        std::env::set_var(state::STATE_DIR_ENV, &state_dir);
    }

    // Host with an injected provider the test can inspect afterward.
    let provider = MockProvider::with_default_regions(ProviderKind::Aws);
    let mut out = Vec::new();
    host::run(&host_args(), &provider, &mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("host --json must emit exactly one JSON object ({e}): {text}"));

    // Region, price, and invites are all present in the JSON output.
    let region = json["region"].as_str().unwrap();
    assert!(region.starts_with("mock-"), "region was {region}");
    assert!(json["hourly_microusd"].as_u64().unwrap() > 0);
    assert!(json["estimated_total_microusd"].as_u64().unwrap() > 0);
    assert_eq!(json["reachability"], "skipped");
    let invites = json["invites"].as_array().unwrap();
    // Three musician seats (the host's own plus two) and one listener.
    assert_eq!(invites.len(), 4);
    assert_eq!(invites[0]["role"], "host");
    assert_eq!(invites[1]["role"], "musician 1");
    assert_eq!(invites[3]["role"], "listener 3");
    for invite in invites {
        let encoded = invite["invite"].as_str().unwrap();
        assert!(encoded.starts_with("jamstream://join/"));
        jamstream_protocol::invite::Invite::decode(encoded).unwrap();
    }

    // State file exists in the overridden directory with 0600.
    let session_id = json["session_id"].as_str().unwrap();
    let path = state_dir.join(format!("{session_id}.json"));
    assert!(path.exists(), "missing state file {path:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let session = state::load(&path).unwrap();
    assert_eq!(session.status, SessionStatus::Running);
    assert_eq!(session.provider, "mock");
    assert_eq!(session.invites.len(), 4);

    // The instance really exists in the mock.
    let running = provider.running_instances();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].session_id(), Some(session_id));

    // Status sees the running session, corroborated against a provider that
    // still lists its instance.
    let mut out = Vec::new();
    status::run(
        &StatusArgs {
            hours: 3.0,
            json: false,
        },
        |name| {
            assert_eq!(name, "mock");
            let p = MockProvider::with_default_regions(ProviderKind::Aws);
            let region = p.regions()[0].clone();
            p.seed_instance(&region, vec![session_tag(session_id)]);
            Ok(Box::new(p))
        },
        &mut out,
    )
    .await
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(&session_id[..8]));
    assert!(text.contains("running"));
    assert!(!text.contains("stale"), "status output: {text}");

    // End by prefix destroys the instance and flips the state file.
    let end_args = EndArgs {
        session: Some(session_id[..8].to_owned()),
        last: false,
    };
    let (found_path, found_session) = end::select(&end_args).unwrap();
    assert_eq!(found_path, path);
    let mut out = Vec::new();
    end::run(&found_path, found_session, &provider, &mut out)
        .await
        .unwrap();
    assert!(provider.running_instances().is_empty());
    let ended = state::load(&path).unwrap();
    assert_eq!(ended.status, SessionStatus::Ended);
    assert!(ended.ended_unix.is_some());
    // The issuer key mints and revokes invites to a server that no longer
    // exists, so ending the session takes it off disk. The record stays for
    // status and for the cost history.
    assert!(
        ended.issuer_private_key_b64.is_empty(),
        "the issuer private key outlived the session"
    );
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(!on_disk.contains(&session.issuer_private_key_b64));

    // Nothing running is left to select.
    assert!(
        end::select(&EndArgs {
            session: None,
            last: true,
        })
        .is_err()
    );

    // Sweep dry-run finds a seeded orphan and does not destroy it.
    let orphan_provider = MockProvider::with_default_regions(ProviderKind::Gcp);
    let region = orphan_provider.regions()[0].clone();
    orphan_provider.seed_instance(&region, vec![session_tag("leaked-session")]);
    let providers: Vec<Box<dyn Provider>> = vec![Box::new(orphan_provider)];
    let mut out = Vec::new();
    sweep::run(&providers, true, &mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("would destroy"), "sweep output: {text}");
    assert!(text.contains("1 found, 0 destroyed, 0 failed"));
    assert_eq!(
        providers[0]
            .list_tagged(None)
            .await
            .unwrap()
            .instances
            .len(),
        1
    );

    // A wet sweep then removes it.
    let mut out = Vec::new();
    sweep::run(&providers, false, &mut out).await.unwrap();
    assert!(
        providers[0]
            .list_tagged(None)
            .await
            .unwrap()
            .instances
            .is_empty()
    );

    std::fs::remove_dir_all(&state_dir).unwrap();
}
