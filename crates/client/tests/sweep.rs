//! "Stop strays" on the Home screen, driven the way the button drives it:
//! the app's own resolver, the executor, `jamstream_cloud::sweep`, and the
//! CLI's `reconcile` writing real session records on disk.
//!
//! Nothing is stubbed between the press and the provider. A test that called
//! the cloud sweeper itself would agree with itself about which providers
//! were searched and which records that closed, and that seam, the app's
//! button reaching the CLI's rule, is the one #371 adds and the one worth
//! holding.
//!
//! One test function, and a state directory this binary owns: `reconcile`
//! reads every record under `JAMSTREAM_STATE_DIR`, so two tests sharing a
//! process would each be reconciling the other's fixtures.

use std::sync::{Arc, Mutex};

use jamstream_cli::state::{self, SessionState, SessionStatus};
use jamstream_client::app::JamApp;
use jamstream_client::sweep::{Resolved, Resolver};
use jamstream_cloud::{MockProvider, Provider, ProviderError, ProviderKind, session_tag};

/// A record of a session that is, as far as this computer knows, still up.
/// `instance_id` is what ties it to a machine the sweep may destroy.
///
/// `provider` is the name the record spells, and `reconcile` will only close
/// it on the word of a search answering to that same name. The mock answers
/// to "mock" alone, so a record naming a real cloud is one no mock can speak
/// for, which is what the second half of this test leans on.
fn running_record(provider: &str, session_id_hex: &str, instance_id: &str) -> SessionState {
    SessionState {
        session_id_hex: session_id_hex.to_owned(),
        provider: provider.to_owned(),
        region: "mock-1".to_owned(),
        instance_id: instance_id.to_owned(),
        address: "203.0.113.7:43210".to_owned(),
        created_unix: 1_785_000_000,
        hourly_microusd: 16_800,
        issuer_private_key_b64: data_encoding::BASE64.encode(&[7u8; 32]),
        server_public_key_b64: data_encoding::BASE64.encode(&[9u8; 32]),
        invites: Vec::new(),
        status: SessionStatus::Running,
        ended_unix: None,
    }
}

/// A provider with one jamstream-tagged machine per session id, and the ids
/// the machines were given.
fn seeded(kind: ProviderKind, sessions: &[&str]) -> (MockProvider, Vec<String>) {
    let provider = MockProvider::with_default_regions(kind);
    let region = provider.regions()[0].clone();
    let ids = sessions
        .iter()
        .map(|s| provider.seed_instance(&region, vec![session_tag(s)]).id)
        .collect();
    (provider, ids)
}

/// A resolver good for exactly one sweep. The sweeper takes the providers by
/// value, so a second press would need a second set, and panicking on it says
/// so rather than sweeping an empty list and calling that clean.
fn once(providers: Vec<Box<dyn Provider>>, unconfigured: Vec<ProviderKind>) -> Resolver {
    let held = Mutex::new(Some(providers));
    Arc::new(move || Resolved {
        providers: held.lock().expect("providers").take().expect("one sweep"),
        unconfigured: unconfigured.clone(),
    })
}

/// Runs the app's sweep to completion, the way the frame loop does: press,
/// then poll until the job lands.
fn sweep_now(app: &mut JamApp) {
    app.begin_sweep();
    for _ in 0..2000 {
        if app.poll_sweep().is_some() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the sweep never finished");
}

#[test]
fn the_home_sweep_stops_strays_and_says_what_it_could_not_account_for() {
    let dir = std::env::temp_dir().join(format!(
        "jamstream-client-sweep-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    // A private subdirectory, not temp_dir() itself: the state writer refuses
    // a world-writable parent, and Linux's /tmp is one.
    std::fs::create_dir_all(&dir).expect("state dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("state dir mode");
    }
    // Safety: this binary holds one test and sets the variable before any
    // state or provider access, on the only thread that has started.
    unsafe { std::env::set_var(state::STATE_DIR_ENV, &dir) };

    // Three strays, with a record for two of them saying they are still
    // running. One provider, because every mock answers to "mock" and a
    // second would leave which search speaks for these records up to the
    // order they were resolved in.
    let (cloud, ids) = seeded(ProviderKind::Aws, &["a1a1", "b2b2", "c3c3"]);
    for (session, instance) in ["a1", "b2"].iter().zip(&ids) {
        let record = running_record("mock", &session.repeat(16), instance);
        state::write_to(
            &dir.join(format!("{}.json", record.session_id_hex)),
            &record,
        )
        .expect("write the record");
    }

    let mut app = JamApp::in_memory();
    // The keychain holds nothing for DigitalOcean on this computer, so nothing
    // there was searched. Not the same as finding nothing.
    app.providers = once(vec![Box::new(cloud)], vec![ProviderKind::DigitalOcean]);

    sweep_now(&mut app);
    let outcome = app.swept.as_ref().expect("an outcome");
    // Destroyed is only counted after the provider's own destroy returned
    // Ok, so three of three is the machines really being gone.
    assert_eq!(outcome.found, 3, "{outcome:?}");
    assert_eq!(outcome.destroyed, 3, "{outcome:?}");
    assert!(outcome.still_running.is_empty(), "{outcome:?}");
    assert!(outcome.unswept.is_empty(), "{outcome:?}");
    assert_eq!(outcome.summary(), "Stopped 3 machine(s).");

    // The records the sweep made false are closed, so the app and the CLI
    // agree afterwards: `jamstream status` reads these same files.
    assert_eq!(outcome.closed.len(), 2, "{outcome:?}");
    for (_, record) in state::list().expect("list records") {
        assert_eq!(record.status, SessionStatus::Ended, "{record:?}");
        assert!(
            record.issuer_private_key_b64.is_empty(),
            "a closed record keeps no issuer key"
        );
    }

    // DigitalOcean was never searched, so this sweep is not an all clear,
    // however empty the accounts it did search turned out to be.
    assert!(!outcome.accounted_for(), "{outcome:?}");
    let warnings = outcome.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("digitalocean"), "{warnings:?}");
    assert!(warnings[0].contains("Not searched"), "{warnings:?}");

    // A provider whose listing fails reads differently from one that was
    // searched and found nothing, and a destroy that fails names a machine
    // that is still billing. Both against a fresh record that must survive.
    let live = running_record("aws", &"d4".repeat(16), "i-still-here");
    state::write_to(&dir.join(format!("{}.json", live.session_id_hex)), &live)
        .expect("write the record");
    let (broken, _) = seeded(ProviderKind::Aws, &["d4d4"]);
    broken.fail_next_lists(
        1,
        ProviderError::Other("the account is throttled".to_owned()),
    );
    let (stubborn, _) = seeded(ProviderKind::Gcp, &["e5e5"]);
    stubborn.fail_next_destroys(1, ProviderError::RateLimited { retry_after: None });
    app.providers = once(vec![Box::new(broken), Box::new(stubborn)], Vec::new());

    sweep_now(&mut app);
    let outcome = app.swept.as_ref().expect("an outcome");
    assert_eq!(outcome.found, 1, "{outcome:?}");
    assert_eq!(outcome.destroyed, 0, "{outcome:?}");
    assert_eq!(outcome.still_running.len(), 1, "{outcome:?}");
    assert_eq!(outcome.unswept.len(), 1, "{outcome:?}");
    assert!(!outcome.accounted_for(), "{outcome:?}");
    let warnings = outcome.warnings();
    assert!(
        warnings.iter().any(|w| w.contains("could not be searched")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w.contains("Still running")),
        "{warnings:?}"
    );
    // The live record is left alone. Nobody searched the provider it names,
    // and the one thing worse than a stray machine is a record claiming a
    // session someone is playing on is over.
    assert!(outcome.closed.is_empty(), "{outcome:?}");
    let still_running = state::load(&dir.join(format!("{}.json", live.session_id_hex)))
        .expect("the live record is still readable");
    assert_eq!(still_running.status, SessionStatus::Running);

    std::fs::remove_dir_all(&dir).ok();
    // Safety: same as above; the sweeps are finished and nothing else runs.
    unsafe { std::env::remove_var(state::STATE_DIR_ENV) };
}
