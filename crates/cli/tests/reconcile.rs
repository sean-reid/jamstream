//! Session records stop lying after any death: sweep closes the record of
//! every instance it destroyed or found gone, and status corroborates a
//! running record against its provider before repeating it.
//!
//! The other half matters more, because getting it wrong costs a session
//! rather than a line of output. Closing a record blanks the issuer key,
//! hides the session from `jamstream end`, and reports a live machine as
//! ended, so a record is only closed on positive evidence that its instance
//! is gone. Nobody looked is not the same answer as nothing is there.
//!
//! One test function: the state directory override is process-global env,
//! so the phases run in sequence against one directory.

use std::path::Path;

use jamstream_cli::cli::StatusArgs;
use jamstream_cli::state::{self, SessionState, SessionStatus};
use jamstream_cli::{CliError, providers, status, sweep};
use jamstream_cloud::{MockProvider, Provider, ProviderError, ProviderKind, session_tag};

/// Lines of `src` that are exactly `needle` once indentation is stripped.
/// A const fn so the count below is a build error rather than a test.
const fn attribute_lines(src: &str, needle: &str) -> usize {
    let (src, needle) = (src.as_bytes(), needle.as_bytes());
    let (mut found, mut at) = (0, 0);
    while at <= src.len() {
        let mut end = at;
        while end < src.len() && src[end] != b'\n' {
            end += 1;
        }
        let mut from = at;
        while from < end && (src[from] == b' ' || src[from] == b'\t') {
            from += 1;
        }
        let mut to = end;
        while to > from && (src[to - 1] == b' ' || src[to - 1] == b'\t' || src[to - 1] == b'\r') {
            to -= 1;
        }
        if to - from == needle.len() {
            let mut i = 0;
            while i < needle.len() && src[from + i] == needle[i] {
                i += 1;
            }
            if i == needle.len() {
                found += 1;
            }
        }
        at = end + 1;
    }
    found
}

/// The single-test rule this file's `set_var` rests on, enforced at compile
/// time. Two tests in one binary run on two threads under libtest, which the
/// coverage job uses, and the second one's state directory would land on the
/// first one mid-sweep.
const _: () = assert!(
    attribute_lines(include_str!("reconcile.rs"), "#[test]")
        + attribute_lines(include_str!("reconcile.rs"), "#[tokio::test]")
        == 1,
    "reconcile.rs sets JAMSTREAM_STATE_DIR for the whole process, so it holds \
     exactly one test. Add a phase to the one below, or a new file with its \
     own directory."
);

/// Credentials that would put a real cloud provider in `resolve_all`, so the
/// phase that runs a real sweep runs the one a host without them gets.
const CLOUD_CREDENTIALS: [&str; 6] = [
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "DIGITALOCEAN_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
    "GCP_ACCESS_TOKEN",
];

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
    // Safety: the const assertion above holds this binary to one test, and
    // the variable is set before the first state access below.
    unsafe {
        std::env::set_var(state::STATE_DIR_ENV, &state_dir);
    }

    // Phase 1: sweep closes what it destroyed and what it found gone. The
    // records name the provider they were launched against, and the mock is
    // its own provider, not a stand-in for the cloud its instances borrow.
    let mock = MockProvider::with_default_regions(ProviderKind::Aws);
    let region = mock.regions()[0].clone();
    let swept = mock.seed_instance(&region, vec![session_tag("aaaa1111aaaa1111")]);
    let path_swept = state::save(&record("aaaa1111aaaa1111", "mock", &swept.id)).unwrap();
    // Crashed or self-destructed earlier: a record with no instance behind it.
    let path_gone = state::save(&record("bbbb2222bbbb2222", "mock", "i-long-gone")).unwrap();
    // A provider this sweep was not given: nobody looked, nothing learned.
    let path_elsewhere = state::save(&record("cccc3333cccc3333", "gcp", "gcp-instance")).unwrap();
    // A real cloud whose kind the mock happens to borrow. The double proves
    // nothing about the account it is imitating.
    let path_real_cloud = state::save(&record("eeee5555eeee5555", "aws", "i-live")).unwrap();

    let providers: Vec<Box<dyn Provider>> = vec![Box::new(mock)];

    // A dry run destroys nothing, so it must also close nothing.
    let mut out = Vec::new();
    sweep::run(&providers, true, &mut out).await.unwrap();
    for path in [&path_swept, &path_gone, &path_elsewhere, &path_real_cloud] {
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
    assert!(!text.contains("eeee5555"), "sweep output: {text}");

    // Destroyed by this sweep, so the instance is certainly gone and the
    // issuer key goes with it: nothing to mint or revoke against a machine
    // this process took down.
    let destroyed = state::load(&path_swept).unwrap();
    assert_eq!(destroyed.status, SessionStatus::Ended);
    assert!(destroyed.ended_unix.is_some());
    assert!(
        destroyed.issuer_private_key_b64.is_empty(),
        "the issuer key outlived an instance we destroyed ourselves"
    );

    // Closed on a listing instead. Weaker evidence, so the key survives: a
    // listing filters by instance state, and a host whose machine turns out
    // to be alive needs the key to revoke every invite to it.
    let unlisted = state::load(&path_gone).unwrap();
    assert_eq!(unlisted.status, SessionStatus::Ended);
    assert!(unlisted.ended_unix.is_some());
    assert_eq!(
        unlisted.issuer_private_key_b64, "aXNzdWVy",
        "a record closed on a listing must keep the key that revokes its invites"
    );

    for (path, why) in [
        (
            &path_elsewhere,
            "a record on a provider the sweep never searched must not be closed",
        ),
        (
            &path_real_cloud,
            "the mock must not answer for the cloud whose kind its instances borrow",
        ),
    ] {
        assert_eq!(
            state::load(path).unwrap().status,
            SessionStatus::Running,
            "{why}"
        );
    }

    // Phase 2: a provider whose listing fails was never searched, so its
    // records stand even though the sweep saw no instances at all.
    reset(&state_dir);
    let path_unlisted = state::save(&record("dddd4444dddd4444", "mock", "i-unknown")).unwrap();
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

    // Phase 2a: a provider that reached only some of its regions searched
    // none of them as far as a record is concerned. The instance below is
    // alive in the region that did not answer.
    reset(&state_dir);
    let path_partial = state::save(&record("ffff6666ffff6666", "mock", "mock-000001")).unwrap();
    let partial = MockProvider::with_default_regions(ProviderKind::Aws);
    let west = partial.regions()[1].clone();
    partial.seed_instance(&west, vec![session_tag("ffff6666ffff6666")]);
    partial.unsearchable_region(&west.id);
    let providers: Vec<Box<dyn Provider>> = vec![Box::new(partial)];
    let mut out = Vec::new();
    let err = sweep::run(&providers, false, &mut out)
        .await
        .expect_err("a region nobody could list is not a clean sweep");
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("mock: could not search mock-west"),
        "sweep output: {text}"
    );
    assert!(err.to_string().contains("still billing"), "error: {err}");
    let survived = state::load(&path_partial).unwrap();
    assert_eq!(
        survived.status,
        SessionStatus::Running,
        "a session in the region nobody listed must not be closed"
    );
    assert_eq!(survived.issuer_private_key_b64, "aXNzdWVy");

    // Phase 2b: the whole thing end to end through the provider set the
    // binary actually sweeps with. A shell with no cloud credentials
    // resolves no cloud provider at all, so a running AWS record has nobody
    // to speak for it and must come through untouched. This is the shape of
    // the bug that closed live sessions: the test double resolved without
    // credentials, reported an empty account, and passed for AWS.
    reset(&state_dir);
    // Safety: single-test binary, as above.
    unsafe {
        for key in CLOUD_CREDENTIALS {
            std::env::remove_var(key);
        }
    }
    let path_live = state::save(&record("9999cccc9999cccc", "aws", "i-still-running")).unwrap();
    let resolved = providers::resolve_all();
    assert!(
        !resolved.iter().any(|p| p.name() == "aws"),
        "no credentials in this shell, so no aws provider"
    );
    let mut out = Vec::new();
    sweep::run(&resolved, false, &mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains("9999cccc"), "sweep output: {text}");
    let live = state::load(&path_live).unwrap();
    assert_eq!(
        live.status,
        SessionStatus::Running,
        "a sweep from a shell without credentials must not end an aws session"
    );
    assert_eq!(
        live.issuer_private_key_b64, "aXNzdWVy",
        "and it must not blank the key that revokes the session's invites"
    );

    // Phase 2c: a record that cannot be rewritten must not abandon the rest.
    // The instances are already destroyed by the time reconciliation runs, so
    // every record it skips is one that will go on claiming a live session.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        reset(&state_dir);
        let stuck = MockProvider::with_default_regions(ProviderKind::Aws);
        let region = stuck.regions()[0].clone();
        let mut paths = Vec::new();
        for session in ["1a1a1a1a1a1a1a1a", "2b2b2b2b2b2b2b2b"] {
            let inst = stuck.seed_instance(&region, vec![session_tag(session)]);
            paths.push(state::save(&record(session, "mock", &inst.id)).unwrap());
        }
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        // Root writes wherever it likes, so the phase only means something
        // where the directory is genuinely unwritable.
        let probe = state_dir.join("probe");
        let unwritable = std::fs::write(&probe, b"x").is_err();
        let _ = std::fs::remove_file(&probe);

        if unwritable {
            let providers: Vec<Box<dyn Provider>> = vec![Box::new(stuck)];
            let mut out = Vec::new();
            let err = sweep::run(&providers, false, &mut out)
                .await
                .expect_err("a record left claiming a destroyed instance is a failed sweep");
            let text = String::from_utf8(out).unwrap();
            assert!(err.to_string().contains("2 session record(s)"), "{err}");
            for path in &paths {
                assert!(
                    text.contains(&path.display().to_string()),
                    "every record that could not be rewritten is named: {text}"
                );
                assert_eq!(
                    state::load(path).unwrap().status,
                    SessionStatus::Running,
                    "nothing was written, so nothing changed on disk"
                );
            }
        }
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

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
