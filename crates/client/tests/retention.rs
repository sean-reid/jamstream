//! What a launch's retention call decided has to reach the host's eyes.
//!
//! The value comes out of the cloud crate's own store rather than being
//! written here, so the words under test are the ones a real bucket produces,
//! and the assertions are made on the rendered screen through
//! `JamApp::root_ui`. The defect (#257) was a caller discarding a return
//! value, and the only thing that can tell you a value arrived is a surface
//! showing it.

use std::sync::Arc;

use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use jamstream_client::app::{JamApp, Screen};
use jamstream_client::creds::MemStore;
use jamstream_client::demo::{DemoRuntime, FROZEN_FRAME};
use jamstream_client::screens::destinations::DestinationsPanel;
use jamstream_client::screens::host::LaunchOutcome;
use jamstream_client::screens::session::SettingsTab;
use jamstream_client::theme::{self, Theme};
use jamstream_cloud::{
    MockStore, ObjectStore, ProviderKind, Retention, RetentionEnforcement, session_prefix,
};

const WIDE: egui::Vec2 = vec2(1280.0, 800.0);

/// The real answer a store gives, from the store itself. No lifecycle support
/// is the target that will not take a rule, which is the shape a key holding
/// `s3:PutLifecycleConfiguration` and not `s3:GetLifecycleConfiguration` comes
/// back as.
async fn applied(retention: Retention, lifecycle: bool) -> RetentionEnforcement {
    let store = MockStore::new(ProviderKind::DigitalOcean);
    let store = if lifecycle {
        store
    } else {
        store.without_lifecycle_support()
    };
    store
        .set_retention("our-jams", &session_prefix(&"a3".repeat(16)), retention)
        .await
        .expect("the mock store answers")
}

/// A launch outcome whose host invite does not decode, so
/// `enter_hosted_session` takes its first early return. The answer has to
/// survive that, and the test reaches no sound card and no socket on the way.
fn outcome_with(retention: Option<RetentionEnforcement>) -> LaunchOutcome {
    let state = jamstream_cli::state::SessionState {
        session_id_hex: "a3".repeat(16),
        provider: "digitalocean".to_owned(),
        region: "nyc3".to_owned(),
        instance_id: "12345".to_owned(),
        address: "203.0.113.10:43210".to_owned(),
        created_unix: 1_784_000_000,
        hourly_microusd: 26_790,
        issuer_private_key_b64: String::new(),
        server_public_key_b64: String::new(),
        invites: vec![jamstream_cli::state::InviteRecord {
            role: "host".to_owned(),
            invite: "jamstream://join/not-a-real-invite".to_owned(),
        }],
        status: jamstream_cli::state::SessionStatus::Running,
        ended_unix: None,
    };
    // A private subdirectory, not temp_dir() itself: the state writer
    // refuses a world-writable parent, and Linux's /tmp is one.
    let dir = std::env::temp_dir().join(format!("jamstream-retention-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("fixture dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("fixture dir mode");
    }
    LaunchOutcome {
        state,
        state_path: dir.join("state.json"),
        retention,
    }
}

/// The application shell as `ui_snapshots.rs` builds it, at one point per
/// pixel: nothing here reads pixels, and a rect in points is comparable to
/// the window these tests asked for.
fn app_harness(mut app: JamApp) -> Harness<'static> {
    let theme = app.theme;
    Harness::builder()
        .with_size(WIDE)
        .with_pixels_per_point(1.0)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), theme);
            egui::CentralPanel::default_margins()
                .frame(
                    egui::Frame::new()
                        .fill(theme::palette(theme).surface0)
                        .inner_margin(egui::Margin::same(10)),
                )
                .show(ui, |ui| app.root_ui(ui));
        })
}

/// Where `text` is drawn, once per node carrying it. Whether that is inside
/// the window is the caller's question: a node scrolled past an edge is still
/// in the accessibility tree and is not something a host can read.
fn drawn_at(harness: &mut Harness<'_>, text: &str) -> Vec<egui::Rect> {
    harness.run_steps(4);
    harness
        .query_all_by_label_contains(text)
        .map(|node| node.rect())
        .collect()
}

/// Asserts `text` is on screen exactly once, inside the window.
fn shown_once(harness: &mut Harness<'_>, surface: &str, text: &str) {
    let rects = drawn_at(harness, text);
    assert_eq!(
        rects.len(),
        1,
        "{surface} says nothing about the rule the bucket refused:\n{text}"
    );
    let window = egui::Rect::from_min_size(egui::Pos2::ZERO, WIDE);
    assert!(
        window.contains_rect(rects[0]),
        "{surface} draws the note at {:?}, outside the {WIDE:?} window",
        rects[0]
    );
}

/// Scrolls the settings drawer to its end, the way `ui_snapshots.rs` does.
/// The Recording tab is taller than the drawer and the retention section is
/// the last thing in it.
fn scroll_drawer(harness: &mut Harness<'_>) {
    harness.run_steps(2);
    harness.event(egui::Event::PointerMoved(egui::pos2(
        WIDE.x - 100.0,
        WIDE.y / 2.0,
    )));
    harness.run_steps(1);
    harness.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: vec2(0.0, -2000.0),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::NONE,
    });
    harness.run_steps(3);
}

/// The Recording tab of the settings drawer, scrolled to the retention rows.
fn recording_tab(mut app: JamApp) -> Harness<'static> {
    app.settings_open = true;
    app.settings_tab = SettingsTab::Recording;
    let mut harness = app_harness(app);
    scroll_drawer(&mut harness);
    harness
}

/// A host session with the record sheet open: the surface a host reads
/// immediately before pressing Record. `DemoRuntime` is the stage here rather
/// than the subject, and nothing asserted below comes out of it.
fn record_sheet_app(applied: Option<RetentionEnforcement>) -> JamApp {
    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.recording.applied = applied;
    app.runtime = Some(Box::new(DemoRuntime::frozen(FROZEN_FRAME, true)));
    app.session.destinations = Some(DestinationsPanel::new(Arc::new(MemStore::default())));
    app.session.record_open = true;
    app.screen = Screen::Session;
    app
}

fn launched_app(retention: Option<RetentionEnforcement>) -> JamApp {
    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.enter_hosted_session(outcome_with(retention));
    app
}

/// The defect itself: the launch's answer went on the floor, so a host whose
/// bucket refused the rule saw a session identical to one where it was
/// applied, and found out when the storage bill did not stop.
#[tokio::test]
async fn an_unenforced_retention_choice_reaches_both_surfaces() {
    let applied = applied(Retention::Days30, false).await;
    assert!(
        !applied.is_server_side(),
        "the fixture must be a target that refused the rule"
    );
    let expected = applied.describe();

    // Through the app's own entry point. The invite does not decode, so the
    // join never runs, and the answer lands anyway: what the bucket agreed to
    // does not depend on this host getting into the session.
    let app = launched_app(Some(applied.clone()));
    assert!(
        app.wizard.launch_error.is_some(),
        "this fixture must not reach a real join"
    );
    assert_eq!(app.recording.applied.as_ref(), Some(&applied));

    shown_once(&mut recording_tab(app), "the Recording tab", &expected);
    // The record sheet gets it through the handover in root_ui, so this is
    // also the proof that the session screen holds no stale copy of its own.
    shown_once(
        &mut app_harness(record_sheet_app(Some(applied))),
        "the record sheet",
        &expected,
    );
}

/// The other half: a bucket that took the rule is keeping the promise, so
/// neither surface says a word. A warning that is always on is not a warning,
/// and the note's absence is what makes its presence mean anything.
#[tokio::test]
async fn a_rule_the_bucket_took_puts_nothing_on_either_surface() {
    let server_side = applied(Retention::Days30, true).await;
    assert!(server_side.is_server_side());
    let refused = applied(Retention::Days30, false).await.describe();

    let app = launched_app(Some(server_side.clone()));
    assert_eq!(app.recording.applied.as_ref(), Some(&server_side));
    assert_eq!(app.recording.retention_note(), None);

    assert!(drawn_at(&mut recording_tab(app), &refused).is_empty());
    assert!(
        drawn_at(
            &mut app_harness(record_sheet_app(Some(server_side))),
            &refused,
        )
        .is_empty()
    );
}

/// "Keep forever" asked for nothing, so a target with no rule is a fact and
/// not a broken promise. It is still said, because an absent lifecycle rule
/// also stops abandoned uploads being cleaned up, but it is not set in the
/// colour reserved for real problems.
#[tokio::test]
async fn keep_forever_is_stated_without_being_a_problem() {
    let mut app = JamApp::in_memory();
    let forever = applied(Retention::KeepForever, false).await;
    app.recording.applied = Some(forever.clone());
    let note = app.recording.retention_note().expect("a note either way");
    assert_eq!(note.text, forever.describe());
    assert!(!note.broken_promise);
    let dark = theme::palette(Theme::Dark);
    assert_eq!(
        note.color(dark),
        dark.text_muted,
        "nothing was promised, so nothing is in the danger colour"
    );

    // And a promise that was broken is: the restrained red, stepped until it
    // reads as a paragraph on the sheet, in both palettes. Straight off the
    // palette it measures 4.22:1 on surface1 in dark, which is under AA for a
    // block of text nobody would otherwise read.
    app.recording.applied = Some(applied(Retention::Days30, false).await);
    let note = app.recording.retention_note().expect("a note");
    assert!(note.broken_promise);
    for theme in [Theme::Dark, Theme::Light] {
        let p = theme::palette(theme);
        let color = note.color(p);
        let ratio = theme::contrast_ratio(color, p.surface1);
        assert!(
            ratio >= theme::AA_TEXT,
            "{theme:?} draws the note at {ratio:.2} on the sheet"
        );
        assert!(
            theme::contrast_ratio(color, p.danger) < 2.0,
            "{theme:?} draws the note in something that is no longer the danger colour"
        );
    }
}

/// A session that records nowhere has nothing to say about retention, and a
/// local session records to this computer's disk with no bucket and no rule.
#[tokio::test]
async fn a_session_with_no_bucket_says_nothing() {
    let app = launched_app(None);
    assert_eq!(app.recording.applied, None);
    assert_eq!(app.recording.retention_note(), None);
}
