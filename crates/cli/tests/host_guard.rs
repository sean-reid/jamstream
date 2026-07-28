//! User story: a stray instance from a session that never got torn down is
//! still billing, and hosting a new session warns about it up front instead
//! of quietly stacking another VM on top. The sweep flow itself is covered
//! in host_flow.rs; this gates the pre-flight warning.
//!
//! One test function: the state directory override is process-global env.

use jamstream_cli::cli::HostArgs;
use jamstream_cli::{host, state};
use jamstream_cloud::{MockProvider, Provider, ProviderKind, session_tag};

fn host_args(json: bool) -> HostArgs {
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
        record_stems: false,
        artifact_url: None,
        artifact_sha256: None,
        yes: true,
        json,
    }
}

#[tokio::test]
async fn host_warns_about_preexisting_tagged_instances() {
    let state_dir = std::env::temp_dir().join(format!(
        "jamstream-cli-host-guard-{}-{}",
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

    // A leaked instance from an earlier session is already running.
    let provider = MockProvider::with_default_regions(ProviderKind::Aws);
    let region = provider.regions()[0].clone();
    let orphan = provider.seed_instance(&region, vec![session_tag("leaked-session")]);

    // JSON mode carries the strays in the output object.
    let mut out = Vec::new();
    host::run(&host_args(true), &provider, &mut out)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
    let strays = json["preexisting_instances"].as_array().unwrap();
    assert_eq!(strays.len(), 1, "exactly the seeded orphan: {strays:?}");
    assert_eq!(strays[0]["instance_id"], orphan.id);
    assert_eq!(strays[0]["session_id"], "leaked-session");
    assert_eq!(strays[0]["region"], region.id.as_str());

    // Human mode warns before provisioning, pointing at the sweeper. The
    // fresh provider is seeded the same way so the first launch above does
    // not muddy the count.
    let provider = MockProvider::with_default_regions(ProviderKind::Aws);
    let orphan = provider.seed_instance(&region, vec![session_tag("leaked-session")]);
    let mut out = Vec::new();
    host::run(&host_args(false), &provider, &mut out)
        .await
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    let warning = "found 1 existing jamstream instances; run jamstream sweep if these are strays";
    assert!(text.contains(warning), "missing warning in: {text}");
    assert!(
        text.contains(&orphan.id),
        "warning must name the instance: {text}"
    );
    let warn_at = text.find(warning).unwrap();
    let launch_at = text.find("Launching in").expect("launch line");
    assert!(
        warn_at < launch_at,
        "the warning must come before provisioning: {text}"
    );

    // A clean provider produces no warning and an empty JSON array.
    let provider = MockProvider::with_default_regions(ProviderKind::Aws);
    let mut out = Vec::new();
    host::run(&host_args(true), &provider, &mut out)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
    assert_eq!(json["preexisting_instances"].as_array().unwrap().len(), 0);

    std::fs::remove_dir_all(&state_dir).unwrap();
}
