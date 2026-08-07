//! The wizard-to-real-session story, end to end through the production
//! wizard code: provider local in a temp state dir, launch through the
//! executor-backed job (real jamstreamd spawned, real reachability
//! handshake), auto-join with the host invite, the invites panel model
//! listing the right labels with tokens, a revoke the server then enforces
//! against a real join attempt, and end-session destroying the process.
//!
//! The join and the end go through `JamApp::enter_hosted_session` and
//! `JamApp::end_session`, the functions the app itself calls, with the only
//! substitution being the offline WAV backend in place of the sound card.
//! A hand-rolled copy of either body keeps passing while the app stops
//! wrapping the runtime in `CostedRuntime`, which loses the cost meter and
//! leaves the mixer's Revoke pointing at nothing.
//!
//! One test function: the state directory and JAMSTREAMD_PATH overrides
//! are process-global environment.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jamstream_audio_io::WavBackend;
use jamstream_client::app::{JamApp, Screen};
use jamstream_client::creds::{EnvReader, MemStore};
use jamstream_client::exec::Executor;
use jamstream_client::live::{AudioSettings, LiveRuntime};
use jamstream_client::runtime::{Command, ConnState, RecordState, Runtime, Snapshot, TokenId};
use jamstream_client::screens::host::{HostWizard, WizardEvent, WizardStep};
use jamstream_client::screens::invites::{InvitesPanel, Seat};
use jamstream_client::screens::recording::RecordingChoice;
use jamstream_client::screens::session::SettingsTab;
use jamstream_protocol::ids::{HOST_MEMBER_ID, MemberId};
use jamstream_protocol::invite::Invite;

/// The token and the shareable link in the seat with this label. Panics
/// rather than returning an option: every caller here names a seat the
/// session has.
fn link_of(panel: &InvitesPanel, label: &str) -> (TokenId, String) {
    let seat = panel
        .guest_seats()
        .find(|s| s.label() == label)
        .unwrap_or_else(|| panic!("no seat labelled {label}"));
    let invite = seat
        .invite
        .as_ref()
        .unwrap_or_else(|| panic!("seat {label} holds no invite"));
    (invite.token, invite.encoded.clone())
}

#[cfg(windows)]
const BIN_NAME: &str = "jamstreamd.exe";
#[cfg(not(windows))]
const BIN_NAME: &str = "jamstreamd";

/// Builds (if needed) and returns the jamstreamd binary for this profile;
/// same mechanism as the CLI's local_host test, because CARGO_BIN_EXE_
/// only covers binaries of the package under test.
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
    assert!(binary.is_file(), "missing {}", binary.display());
    binary
}

/// Takes the recorder has finished writing: a `.part` is a take still in
/// flight, and it must never be mistaken for one.
fn finished_takes(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "flac"))
        .collect()
}

fn settings() -> AudioSettings {
    AudioSettings {
        capture_id: None,
        playback_id: None,
        buffer_frames: 120,
        ..AudioSettings::default()
    }
}

/// Polls snapshots until `pred` holds; panics with the last state on timeout.
fn wait_for(
    rt: &dyn Runtime,
    what: &str,
    timeout: Duration,
    mut pred: impl FnMut(&Snapshot) -> bool,
) -> Snapshot {
    let deadline = Instant::now() + timeout;
    loop {
        let snap = rt.snapshot();
        if pred(&snap) {
            return snap;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; state {:?}, {} members",
            snap.stats.state,
            snap.members.len()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wizard_hosts_a_real_local_session() {
    let state_dir = std::env::temp_dir().join(format!(
        "jamstream-client-wizard-local-{}-{}",
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
        std::env::set_var(jamstream_cli::state::STATE_DIR_ENV, &state_dir);
        std::env::set_var("JAMSTREAMD_PATH", &server_binary);
        // Loopback, not every interface. A real session needs the LAN so
        // bandmates can reach it; this test only ever talks to itself, and
        // on macOS binding 0.0.0.0 makes the firewall ask about each newly
        // built binary, parking the run behind a dialog nobody answers.
        std::env::set_var("JAMSTREAM_BIND", "127.0.0.1");
    }

    // Drive the wizard exactly as the UI does: transitions plus job polls.
    let env: EnvReader = Arc::new(|_| None);
    let mut wizard = HostWizard::new(
        Arc::new(MemStore::default()),
        env,
        Arc::new(Executor::new()),
    );
    assert!(wizard.select_provider(0), "local is the first row");
    assert_eq!(wizard.selected_provider_name(), Some("local"));
    assert!(
        wizard.advance_from_provider(),
        "local skips the region step"
    );
    assert_eq!(wizard.step, WizardStep::Preview);
    // Three musician seats, the host's own included, plus one listener: the
    // wizard's counts mean what --musicians and --listeners mean.
    wizard.musicians = 3;
    wizard.listeners = 1;
    // Arm the take. A session on this computer records to this computer's disk
    // and needs no bucket and no key, which is why this launch can prove the
    // whole path: the wizard arms the server it spawns, and a real take lands.
    assert_eq!(
        wizard.recording,
        RecordingChoice::Off,
        "recording is off until a host turns it on"
    );
    assert_eq!(
        wizard.recording_refusal(),
        None,
        "a local session needs no credential to record"
    );
    assert!(wizard.set_recording(RecordingChoice::MixOnly));
    assert!(wizard.can_launch(), "local needs no artifact fields");
    assert!(wizard.begin_launch());
    assert_eq!(wizard.step, WizardStep::Launching);

    // The launch job spawns jamstreamd and proves it answers a handshake.
    let deadline = Instant::now() + Duration::from_secs(60);
    let outcome = loop {
        if let Some(WizardEvent::Launched(outcome)) = wizard.poll() {
            break *outcome;
        }
        assert!(
            wizard.launch_error.is_none(),
            "launch failed: {:?}",
            wizard.launch_error
        );
        assert!(Instant::now() < deadline, "launch never completed");
        std::thread::sleep(Duration::from_millis(25));
    };

    assert_eq!(outcome.state.provider, "local");
    assert_eq!(outcome.state.hourly_microusd, 0);
    // Three musician seats (host + 2 guests) and one listener.
    assert_eq!(outcome.state.invites.len(), 4);
    assert_eq!(outcome.state.invites[0].role, "host");
    assert!(outcome.state_path.is_file(), "state file must exist");

    // Auto-join with the host invite through the app's own entry point, with
    // the offline WAV backend in place of the sound card and nothing else
    // substituted. Everything asserted below the call is something
    // `enter_hosted_session` decided, not something this test arranged.
    let mut app = JamApp::in_memory();
    app.join = Arc::new(|invite, settings| {
        LiveRuntime::join_offline(invite, settings, WavBackend::new(None, None))
    });
    app.enter_hosted_session(outcome.clone());
    assert_eq!(
        app.wizard.launch_error, None,
        "the app failed to join its own session"
    );
    assert_eq!(app.screen, Screen::Session, "the app lands on the session");
    // The drawer opens on the links, because sharing them is the next act.
    assert!(app.settings_open);
    assert_eq!(app.settings_tab, SettingsTab::Invites);
    // Hosting is the only role that can stream, so only a host gets the panel.
    assert!(app.session.destinations.is_some());

    // Borrowed back out of the app for the rest of the story, because the app
    // is the owner: the end below hands it the same runtime and the same book.
    let owned = app.runtime.take().expect("the app holds the runtime");
    let mut panel = app.session.invites.take().expect("the invite book");
    let rt: &dyn Runtime = &*owned;
    let snap = wait_for(rt, "host joined", Duration::from_secs(15), |s| {
        s.stats.state == ConnState::Joined && s.members.iter().any(|m| m.id == HOST_MEMBER_ID)
    });
    assert!(snap.is_host, "member 0 is the host");
    // The CostedRuntime wrapper the app put around the live one: the cost view
    // and the invite book's token ids are both injected by it, and both are
    // things a plain join does not have.
    assert!(snap.cost.is_some(), "wizard sessions carry the cost meter");
    assert_eq!(snap.cost.unwrap().hourly_microusd, 0);
    let me = snap
        .members
        .iter()
        .find(|m| m.id == HOST_MEMBER_ID)
        .expect("host in roster");
    assert_eq!(me.token, panel.token_of(HOST_MEMBER_ID));

    // The take, through the app's own controls: Record, then Stop, then a
    // finished .flac in the recordings directory beside the session state. This
    // is the seam that was missing, so it is proved against a real server
    // rather than against a runtime's own bookkeeping.
    let record_dir = state_dir.join("recordings");
    assert!(
        finished_takes(&record_dir).is_empty(),
        "nothing is captured before Record is pressed"
    );
    rt.send(Command::StartRecord);
    wait_for(rt, "the take started", Duration::from_secs(10), |s| {
        s.record.state == RecordState::Recording
    });
    // Long enough for the recorder to see ticks and write frames.
    std::thread::sleep(Duration::from_millis(500));
    rt.send(Command::StopRecord);
    wait_for(rt, "the take finished", Duration::from_secs(15), |s| {
        s.record.state == RecordState::Idle
    });
    // The rename off .part happens on the recorder's own task, so it is waited
    // for rather than assumed.
    let deadline = Instant::now() + Duration::from_secs(10);
    let takes = loop {
        let takes = finished_takes(&record_dir);
        if !takes.is_empty() {
            break takes;
        }
        assert!(
            Instant::now() < deadline,
            "no finished take appeared in {}",
            record_dir.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        takes.len(),
        1,
        "one Record to one Stop is one take: {takes:?}"
    );
    let take = takes[0].clone();
    let name = take.file_name().expect("a file name").to_string_lossy();
    assert!(
        name.contains("mix"),
        "mix only records the mix and nothing else: {name}"
    );
    assert!(
        std::fs::metadata(&take).expect("the take is on disk").len() > 0,
        "the take is empty"
    );

    // The invites panel model: one seat per non-host invite, labeled, with
    // its token; the host's own seat is never listed.
    let labels: Vec<String> = panel.guest_seats().map(Seat::label).collect();
    assert_eq!(labels, vec!["musician 1", "musician 2", "listener 3"]);
    assert_eq!(panel.tokens().lock().unwrap().len(), 4);

    // Revoke musician 1 the way the panel does, then prove the server
    // refuses that invite on a real join attempt: the handshake is
    // silently dropped, so the joiner times out and never reaches Joined.
    let (revoked_token, revoked_link) = link_of(&panel, "musician 1");
    rt.send(Command::Revoke(revoked_token));
    panel.revoke(revoked_token, Some("Ana".to_owned()));
    assert!(
        panel.seat_of(MemberId(1)).expect("seat 1").is_free(),
        "revoking frees the seat, because the server has already ejected them"
    );
    std::thread::sleep(Duration::from_millis(500));

    let revoked_invite = Invite::decode(&revoked_link).expect("revoked invite decodes");
    let refused =
        LiveRuntime::join_offline(&revoked_invite, settings(), WavBackend::new(None, None))
            .expect("join attempt starts");
    let snap = wait_for(
        &refused,
        "revoked join refused",
        Duration::from_secs(20),
        |s| {
            assert_ne!(
                s.stats.state,
                ConnState::Joined,
                "server admitted a revoked invite"
            );
            s.stats.state == ConnState::TimedOut
        },
    );
    assert_eq!(snap.stats.state, ConnState::TimedOut);
    drop(refused);

    // A musician with an intact invite still gets in.
    let (_, good_link) = link_of(&panel, "musician 2");
    let good_invite = Invite::decode(&good_link).expect("good invite");
    let good = LiveRuntime::join_offline(&good_invite, settings(), WavBackend::new(None, None))
        .expect("musician 2 join");
    wait_for(&good, "musician 2 joined", Duration::from_secs(15), |s| {
        s.stats.state == ConnState::Joined
    });
    good.send(Command::Leave);
    wait_for(&good, "musician 2 left", Duration::from_secs(5), |s| {
        s.stats.state == ConnState::Idle
    });
    drop(good);

    // #81: the freed seat takes a replacement, member id and all. This is
    // the claim the whole seat model rests on, so it is made against a real
    // server rather than against the panel's own arithmetic: revocation
    // keys on the token's jti, so seat 1's new link is admitted while the
    // link revoked out of that same seat stays refused.
    panel.refill(MemberId(1)).expect("refill seat 1");
    let (reissued_token, reissued_link) = link_of(&panel, "musician 1");
    assert_ne!(
        reissued_token, revoked_token,
        "a reused seat must carry a new credential"
    );
    let reissued = LiveRuntime::join_offline(
        &Invite::decode(&reissued_link).expect("reissued invite decodes"),
        settings(),
        WavBackend::new(None, None),
    )
    .expect("reissued join starts");
    wait_for(
        &reissued,
        "reissued seat joined",
        Duration::from_secs(15),
        |s| s.stats.state == ConnState::Joined,
    );
    reissued.send(Command::Leave);
    wait_for(
        &reissued,
        "reissued seat left",
        Duration::from_secs(5),
        |s| s.stats.state == ConnState::Idle,
    );
    drop(reissued);

    let still_dead =
        LiveRuntime::join_offline(&revoked_invite, settings(), WavBackend::new(None, None))
            .expect("second attempt with the revoked link starts");
    wait_for(
        &still_dead,
        "the revoked link stays refused after the seat is reissued",
        Duration::from_secs(20),
        |s| {
            assert_ne!(
                s.stats.state,
                ConnState::Joined,
                "reissuing a seat resurrected the invite revoked out of it"
            );
            s.stats.state == ConnState::TimedOut
        },
    );
    drop(still_dead);

    // End the session for everyone, through the app's own path: it resolves
    // the provider from the state file's provider name, hands the invite book
    // to the teardown, drops the runtime, and lands on home.
    let instance_pid = panel.state.instance_id.clone();
    app.runtime = Some(owned);
    app.session.invites = Some(panel);
    app.end_session();
    assert!(app.runtime.is_none(), "ending drops the runtime");
    assert_eq!(app.screen, Screen::Home);
    assert!(app.ending(), "the teardown runs on the executor");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(result) = app.poll_ending() {
            result.expect("end session");
            break;
        }
        assert!(Instant::now() < deadline, "the teardown never finished");
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(app.home.error, None, "the teardown reported a problem");

    let ended = jamstream_cli::state::load(&outcome.state_path).expect("state reloads");
    assert_eq!(ended.status, jamstream_cli::state::SessionStatus::Ended);
    assert!(ended.ended_unix.is_some());
    // #195, asserted on the file rather than on the status word: the server
    // this key authenticated against is gone, so the key that signs the
    // session's invites has no reason to still be in the state directory.
    // Ending from the terminal has always taken it; ending from the app did
    // not, and the test on this side only ever read the status.
    assert!(
        ended.issuer_private_key_b64.is_empty(),
        "the issuer private key is still on disk after ending from the app"
    );
    assert!(
        take.is_file(),
        "ending the session must never take the recording with it"
    );

    #[cfg(unix)]
    {
        // Zombies count as dead, like production liveness.
        let stat = std::process::Command::new("ps")
            .args(["-p", &instance_pid, "-o", "stat="])
            .output()
            .expect("ps");
        let alive = stat.status.success()
            && !String::from_utf8_lossy(&stat.stdout)
                .trim_start()
                .starts_with('Z');
        assert!(!alive, "jamstreamd pid {instance_pid} survived end-session");
    }
    #[cfg(not(unix))]
    let _ = instance_pid;

    // Nothing tagged remains for a fresh provider on the same state dir.
    let fresh = jamstream_cli::providers::resolve("local").expect("local provider");
    assert!(
        fresh
            .list_tagged(None)
            .await
            .expect("list")
            .instances
            .is_empty()
    );

    std::fs::remove_dir_all(&state_dir).ok();
    unsafe {
        std::env::remove_var(jamstream_cli::state::STATE_DIR_ENV);
        std::env::remove_var("JAMSTREAMD_PATH");
    }
}
