//! Session records stop lying after any death: sweep closes the record of
//! every instance it destroyed or found gone, and status corroborates a
//! running record against its provider before repeating it. A provider that
//! cannot be asked proves nothing, so its records are left alone.
//!
//! One test function: the state directory override is process-global env,
//! so the phases run in sequence against one directory.

use std::path::Path;

use jamstream_cli::cli::StatusArgs;
use jamstream_cli::state::{self, SessionState, SessionStatus};
use jamstream_cli::{CliError, status, sweep};
use jamstream_cloud::{MockProvider, Provider, ProviderError, ProviderKind, session_tag};

fn record(session_hex: &str, provider: &str, instance_id: &str) -> SessionState {
    SessionState {
        session_id_hex: session_hex.to_owned(),
        provider: provider.to_owned(),
        region: "mock-east".to_owned(),
        instance_id: instance_id.to_owned(),
        address: "10.0.0.1:43210".to_owned(),
        created_unix: 1_784_000_000,
        hourly_microusd: 16_800,
        issuer_private_key_b64: "aXNzdWVy".to_owned(),
        server_public_key_b64: "c2VydmVy".to_owned(),
        invites: Vec::new(),
        status: SessionStatus::Running,
        ended_unix: None,
    }
}

fn reset(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

async fn status_text(
    json: bool,
    resolve: impl Fn(&str) -> Result<Box<dyn Provider>, CliError>,
) -> String {
    let mut out = Vec::new();
    status::run(&StatusArgs { hours: 3.0, json }, resolve, &mut out)
        .await
        .unwrap();
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn records_are_reconciled_after_any_death() {
    let state_dir = std::env::temp_dir().join(format!(
        "jamstream-cli-reconcile-{}-{}",
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

    // Phase 1: sweep closes what it destroyed and what it found gone.
    let aws = MockProvider::with_default_regions(ProviderKind::Aws);
    let region = aws.regions()[0].clone();
    let swept = aws.seed_instance(&region, vec![session_tag("aaaa1111aaaa1111")]);
    let path_swept = state::save(&record("aaaa1111aaaa1111", "aws", &swept.id)).unwrap();
    // Crashed or self-destructed earlier: a record with no instance behind it.
    let path_gone = state::save(&record("bbbb2222bbbb2222", "aws", "i-long-gone")).unwrap();
    // A provider this sweep was not given: nobody looked, nothing learned.
    let path_elsewhere = state::save(&record("cccc3333cccc3333", "gcp", "gcp-instance")).unwrap();

    let providers: Vec<Box<dyn Provider>> = vec![Box::new(aws)];

    // A dry run destroys nothing, so it must also close nothing.
    let mut out = Vec::new();
    sweep::run(&providers, true, &mut out).await.unwrap();
    for path in [&path_swept, &path_gone, &path_elsewhere] {
        assert_eq!(state::load(path).unwrap().status, SessionStatus::Running);
    }

    let mut out = Vec::new();
    sweep::run(&providers, false, &mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("Session aaaa1111: recorded running, instance gone; marked it ended."),
        "sweep output: {text}"
    );
    assert!(text.contains("Session bbbb2222:"), "sweep output: {text}");
    assert!(!text.contains("cccc3333"), "sweep output: {text}");

    for path in [&path_swept, &path_gone] {
        let closed = state::load(path).unwrap();
        assert_eq!(closed.status, SessionStatus::Ended);
        assert!(closed.ended_unix.is_some());
        assert!(
            closed.issuer_private_key_b64.is_empty(),
            "the issuer key outlived the session"
        );
    }
    assert_eq!(
        state::load(&path_elsewhere).unwrap().status,
        SessionStatus::Running,
        "a record on a provider the sweep never searched must not be closed"
    );

    // Phase 2: a provider whose listing fails was never searched, so its
    // records stand even though the sweep saw no instances at all.
    reset(&state_dir);
    let path_unlisted = state::save(&record("dddd4444dddd4444", "aws", "i-unknown")).unwrap();
    let broken = MockProvider::with_default_regions(ProviderKind::Aws);
    broken.fail_next_lists(1, ProviderError::Other("network unreachable".to_owned()));
    let providers: Vec<Box<dyn Provider>> = vec![Box::new(broken)];
    let mut out = Vec::new();
    sweep::run(&providers, false, &mut out)
        .await
        .expect_err("an unswept provider is not a clean sweep");
    assert_eq!(
        state::load(&path_unlisted).unwrap().status,
        SessionStatus::Running,
        "absence of evidence must not close a record"
    );

    // Phase 3: status corroborates running records against the provider,
    // read-only. Live stays running, gone prints stale with the way out,
    // and a provider that cannot be asked degrades to the record plus a note.
    reset(&state_dir);
    let live = "1111aaaa1111aaaa";
    state::save(&record(live, "aws", "mock-000001")).unwrap();
    let path_dead = state::save(&record("2222bbbb2222bbbb", "aws", "i-dead")).unwrap();
    state::save(&record("3333cccc3333cccc", "digitalocean", "droplet-9")).unwrap();
    state::save(&record("4444dddd4444dddd", "gcp", "gcp-instance")).unwrap();
    let mut over = record("5555eeee5555eeee", "local", "pid-1");
    over.mark_ended(1_784_003_600);
    state::save(&over).unwrap();

    let resolve = |name: &str| -> Result<Box<dyn Provider>, CliError> {
        match name {
            "aws" => {
                let p = MockProvider::with_default_regions(ProviderKind::Aws);
                let region = p.regions()[0].clone();
                p.seed_instance(&region, vec![session_tag(live)]);
                Ok(Box::new(p))
            }
            "gcp" => {
                let p = MockProvider::with_default_regions(ProviderKind::Gcp);
                p.fail_next_lists(1, ProviderError::Other("network unreachable".to_owned()));
                Ok(Box::new(p))
            }
            "digitalocean" => Err(CliError::Usage(
                "provider digitalocean: DIGITALOCEAN_TOKEN is not set".to_owned(),
            )),
            other => panic!("status asked about {other}, whose record is not running"),
        }
    };

    let text = status_text(false, resolve).await;
    let live_row = text.lines().find(|l| l.starts_with("1111aaaa")).unwrap();
    assert!(live_row.contains("running"), "table: {text}");
    let dead_row = text.lines().find(|l| l.starts_with("2222bbbb")).unwrap();
    assert!(dead_row.contains("stale"), "table: {text}");
    assert!(
        !dead_row.contains(" at 3.0 h"),
        "a gone instance gets no projection: {text}"
    );
    assert!(
        text.contains("Session 2222bbbb: recorded running, instance gone; run jamstream end 2222bbbb to close it."),
        "table: {text}"
    );
    assert!(
        text.contains("Session 3333cccc: recorded running; digitalocean could not be checked"),
        "table: {text}"
    );
    assert!(
        text.contains(
            "Session 4444dddd: recorded running; gcp could not be checked (network unreachable)."
        ),
        "table: {text}"
    );

    let json: Vec<serde_json::Value> =
        serde_json::from_str(&status_text(true, resolve).await).unwrap();
    let row = |prefix: &str| {
        json.iter()
            .find(|r| r["session_id"].as_str().unwrap().starts_with(prefix))
            .unwrap()
    };
    assert_eq!(row("1111aaaa")["status"], "running");
    assert_eq!(row("1111aaaa")["corroborated"], true);
    assert_eq!(row("2222bbbb")["status"], "stale");
    assert_eq!(row("2222bbbb")["corroborated"], false);
    assert_eq!(row("3333cccc")["status"], "running");
    assert_eq!(row("3333cccc")["corroborated"], false);
    assert!(
        row("3333cccc")["note"]
            .as_str()
            .unwrap()
            .contains("DIGITALOCEAN_TOKEN")
    );
    assert_eq!(row("4444dddd")["status"], "running");
    assert!(
        row("4444dddd")["note"]
            .as_str()
            .unwrap()
            .contains("network unreachable")
    );
    assert_eq!(row("5555eeee")["status"], "ended");
    assert!(
        row("5555eeee").get("corroborated").is_none(),
        "an ended record makes no claim to corroborate"
    );

    // Status is read-only: every verdict above changed nothing on disk.
    assert_eq!(
        state::load(&path_dead).unwrap().status,
        SessionStatus::Running
    );

    // Phase 4: the uninstallers' pre-flight matches "status": "running" in
    // this JSON. A machine holding only dead and ended sessions must not
    // block an uninstall, while one whose provider could not be checked
    // still does.
    reset(&state_dir);
    state::save(&record("2222bbbb2222bbbb", "aws", "i-dead")).unwrap();
    state::save(&over).unwrap();
    let text = status_text(true, resolve).await;
    assert!(
        !text.contains("\"status\": \"running\""),
        "a dead session must not read as running: {text}"
    );

    reset(&state_dir);
    state::save(&record("3333cccc3333cccc", "digitalocean", "droplet-9")).unwrap();
    let text = status_text(true, resolve).await;
    assert!(
        text.contains("\"status\": \"running\""),
        "an unverifiable session must keep blocking an uninstall: {text}"
    );

    // Phase 5: a listing that reached only some of a provider proves
    // nothing either. The session below is alive in the region that did not
    // answer, and calling it stale would invite the host to close a jam
    // that is still playing.
    reset(&state_dir);
    let session = "7777aaaa7777aaaa";
    state::save(&record(session, "aws", "mock-000001")).unwrap();
    let partial = |name: &str| -> Result<Box<dyn Provider>, CliError> {
        assert_eq!(name, "aws");
        let p = MockProvider::with_default_regions(ProviderKind::Aws);
        let west = p.regions()[1].clone();
        p.seed_instance(&west, vec![session_tag(session)]);
        p.unsearchable_region(&west.id);
        Ok(Box::new(p))
    };
    let text = status_text(false, partial).await;
    assert!(!text.contains("stale"), "table: {text}");
    assert!(
        text.contains(
            "Session 7777aaaa: recorded running; aws could not be checked \
             (mock-west did not answer)."
        ),
        "table: {text}"
    );
    let json: Vec<serde_json::Value> =
        serde_json::from_str(&status_text(true, partial).await).unwrap();
    assert_eq!(json[0]["status"], "running");
    assert_eq!(json[0]["corroborated"], false);

    reset(&state_dir);
}
