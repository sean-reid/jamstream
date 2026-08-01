//! Interaction tests: what the user does must become the right Command on
//! the runtime. A RecordingRuntime wraps the frozen demo and logs.

use std::sync::Arc;

use egui::accesskit::Role as AkRole;
use egui::{Event, Key, Modifiers, PointerButton, vec2};
use egui_kittest::{Harness, kittest::Queryable};
use jamstream_client::creds::MemStore;
use jamstream_client::demo::{DemoRuntime, FROZEN_FRAME, RecordingRuntime};
use jamstream_client::runtime::{
    Command, DestinationState, MemberId, RecordState, Runtime, Snapshot,
};
use jamstream_client::screens::destinations::DestinationsPanel;
use jamstream_client::screens::session::{SessionScreen, SettingsTab};
use jamstream_client::theme::{self, Theme};

type Recorder = Arc<RecordingRuntime<DemoRuntime>>;

fn session_harness_sized(is_host: bool, size: egui::Vec2) -> (Recorder, Harness<'static>) {
    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        is_host,
    )));
    let rt_ui = rt.clone();
    let mut screen = SessionScreen::default();
    let harness = Harness::builder()
        .with_size(size)
        // kittest runs one queued event per frame; keep frames short so a
        // double click stays inside egui's 0.3 s window.
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            let snap = rt_ui.snapshot();
            screen.ui(ui, &snap, &*rt_ui);
        });
    (rt, harness)
}

fn session_harness(is_host: bool) -> (Recorder, Harness<'static>) {
    session_harness_sized(is_host, vec2(1280.0, 800.0))
}

/// A host with everything a host has: the invite book, the destinations
/// panel, and the broadcast view the runtime gives a host. Every host-only
/// surface hangs off one of those.
fn host_harness_sized(size: egui::Vec2) -> Harness<'static> {
    host_harness_lamps(size, Lamps::Both)
}

/// An invite book with no invites in it, and a key that can mint one.
///
/// The key is not decoration. `Mint invite` and `New link` sign with it, and
/// this fixture used to set it to an empty string, so both buttons errored
/// wherever it was used and nothing noticed (#218).
fn empty_invites() -> jamstream_client::screens::invites::InvitesPanel {
    invite_book(Vec::new(), "empty")
}

/// An invite book over `invites`, with a working issuer key, on a state file of
/// its own so a mint or a revoke here cannot rewrite another test's.
fn invite_book(
    invites: Vec<jamstream_cli::state::InviteRecord>,
    label: &str,
) -> jamstream_client::screens::invites::InvitesPanel {
    let issuer = jamstream_protocol::invite::Issuer::from_bytes(&[7u8; 32]);
    let state = jamstream_cli::state::SessionState {
        session_id_hex: "a3".repeat(16),
        provider: "local".to_owned(),
        region: "local".to_owned(),
        instance_id: "12345".to_owned(),
        address: "203.0.113.10:43210".to_owned(),
        created_unix: 1_784_000_000,
        hourly_microusd: 0,
        issuer_private_key_b64: data_encoding::BASE64.encode(&issuer.to_bytes()),
        server_public_key_b64: data_encoding::BASE64.encode(&[9u8; 32]),
        invites,
        status: jamstream_cli::state::SessionStatus::Running,
        ended_unix: None,
    };
    // A private subdirectory, not temp_dir() itself: the state writer
    // refuses a world-writable parent, and Linux's /tmp is one, unlike the
    // per-user temp dirs macOS and Windows hand out.
    let dir = std::env::temp_dir().join(format!(
        "jamstream-interaction-invites-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("fixture dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("fixture dir mode");
    }
    jamstream_client::screens::invites::InvitesPanel::new(state, dir.join("state.json"))
}

/// The two buttons in the invites panel that sign something, pressed.
///
/// Neither had ever been clicked. Every fixture in the suite set
/// `issuer_private_key_b64` to an empty string, so both would have failed on
/// the state file's key and put a red line under the mint row, and one of those
/// fixtures is the published `session_invites.png` (#218). This drives them
/// through the real app shell and asserts on the seats that come out.
#[test]
fn minting_and_refilling_a_seat_work_in_the_fixtures_state() {
    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        true,
    )));
    let rt_ui = rt.clone();
    let mut app = jamstream_client::app::JamApp::in_memory();
    app.recent = Vec::new();
    app.session.invites = Some(invite_book(Vec::new(), "mint"));
    app.runtime = Some(Box::new(rt_ui));
    app.screen = jamstream_client::app::Screen::Session;
    app.settings_open = true;
    app.settings_tab = SettingsTab::Invites;
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
        });
    harness.run_steps(2);

    // Mint one. A seat appears, and no red line appears under the row.
    harness
        .get_by_role_and_label(AkRole::Button, "Mint invite")
        .click();
    harness.run_steps(2);
    assert!(
        harness.query_by_label_contains("issuer key").is_none(),
        "Mint invite could not sign with the fixture's own key"
    );
    assert!(harness.query_by_label("Copy link").is_some());

    // Revoke it, which frees the seat and puts New link on the row. The mixer
    // strips behind the drawer carry a Revoke each, so this is the rightmost
    // one, which is the seat's.
    let seat_revoke = harness
        .get_all_by_label("Revoke")
        .max_by(|a, b| a.rect().left().total_cmp(&b.rect().left()))
        .expect("the seat's Revoke");
    seat_revoke.click();
    harness.run_steps(2);
    harness
        .get_by_role_and_label(AkRole::Button, "Revoke invite")
        .click();
    harness.run_steps(2);
    assert!(harness.get_all_by_label_contains("free").count() > 0);

    // And New link mints straight back into the same seat.
    harness.get_by_label("New link").click();
    harness.run_steps(2);
    assert!(
        harness.query_by_label("New link").is_none(),
        "New link left the seat free"
    );
    assert!(harness.query_by_label("Copy link").is_some());

    // The seat counts on screen agree that a musician seat is taken.
    assert!(harness.query_by_label_contains("musicians").is_some());
}

/// Every window size the app can be in: its 800x600 minimum, its default,
/// and the widths on either side of the point where a host's status bar
/// stops fitting on one row. The two short ones stand in for the minimum
/// window less the shell these tests do not draw, the top bar and the
/// window margins, so the mixer here gets what it gets in the app.
const SIZES: [egui::Vec2; 8] = [
    vec2(800.0, 600.0),
    vec2(800.0, 540.0),
    vec2(900.0, 470.0),
    vec2(900.0, 600.0),
    vec2(1000.0, 700.0),
    vec2(1100.0, 700.0),
    vec2(1150.0, 700.0),
    vec2(1280.0, 800.0),
];

/// The bar's three zones may never touch. Health on the left, the two lamps
/// centred, Record and Leave on the right: this walks every window size the
/// app can be, with the cluster empty, half lit, and fully lit, and checks
/// that no zone reaches another and nothing leaves the window. The bar
/// overlapped itself at 1280 once (#85), and the centred cluster is the piece
/// most able to do it again, because it grows from the middle outward.
#[test]
fn the_bars_three_zones_never_touch_at_any_size() {
    for size in SIZES {
        for lamps in [Lamps::None, Lamps::OnAir, Lamps::Both] {
            let mut harness = host_harness_lamps(size, lamps);
            harness.run_steps(3);
            let health = harness
                .get_all_by_label_contains("mouth to ear")
                .next()
                .expect("the mouth-to-ear readout")
                .rect();
            let mut cluster: Option<egui::Rect> = None;
            for label in lamps.labels() {
                let rect = harness
                    .get_all_by_label(label)
                    .next()
                    .unwrap_or_else(|| panic!("{label} is not in the bar at {size:?}"))
                    .rect();
                cluster = Some(cluster.map_or(rect, |c| c.union(rect)));
            }
            // The buttons and the readouts beside them: the id and the timer
            // are what the lamps were drawn straight through at 800 while a
            // test that probed only the buttons passed.
            for control in ["Record", "Leave", "a3f29c41", "00:47:32"] {
                let rect = harness
                    .get_all_by_label_contains(control)
                    .next()
                    .unwrap_or_else(|| panic!("{control} is not in the bar at {size:?}"))
                    .rect();
                assert!(
                    rect.left() >= health.right() || rect.top() >= health.bottom(),
                    "{control} at {rect:?} runs into the readouts at {health:?}, \
                     window {size:?}, lamps {lamps:?}"
                );
                if let Some(cluster) = cluster {
                    assert!(
                        rect.left() >= cluster.right() || rect.top() >= cluster.bottom(),
                        "{control} at {rect:?} runs into the lamps at {cluster:?}, \
                         window {size:?}, lamps {lamps:?}"
                    );
                }
                assert!(
                    rect.left() >= 0.0 && rect.right() <= size.x,
                    "{control} at {rect:?} is outside the {size:?} window"
                );
            }
            if let Some(cluster) = cluster {
                assert!(
                    cluster.left() >= health.right() || cluster.top() >= health.bottom(),
                    "the lamps at {cluster:?} run into the readouts at {health:?}, \
                     window {size:?}, lamps {lamps:?}"
                );
                assert!(
                    cluster.right() <= size.x,
                    "the lamps at {cluster:?} leave the {size:?} window"
                );
            }
        }
    }
}

/// Where the bar stops fitting on one row, found by walking widths rather
/// than asserted against a constant, because the threshold moves with the
/// cluster: two lit lamps in the middle take room from both halves at once.
/// The numbers in the doc comment on `status_bar` come from this.
#[test]
fn the_one_row_threshold_is_where_the_zones_stop_fitting() {
    for (lamps, expected) in [
        (Lamps::None, 820_i32),
        (Lamps::OnAir, 880),
        (Lamps::Both, 930),
    ] {
        let mut stacked_at: Option<i32> = None;
        // Walk down in 10 px steps and find the first width that stacks.
        for w in (600..=1400_i32).rev().step_by(10) {
            let size = vec2(w as f32, 700.0);
            let mut harness = host_harness_lamps(size, lamps);
            harness.run_steps(3);
            let health = harness
                .get_all_by_label_contains("mouth to ear")
                .next()
                .expect("the readout")
                .rect();
            let leave = harness.get_by_label("Leave").rect();
            // Two rows put Leave below the readouts instead of beside them.
            if leave.top() >= health.bottom() {
                stacked_at = Some(w);
                break;
            }
        }
        let stacked_at = stacked_at.expect("the bar must stack at some width");
        assert!(
            (stacked_at - expected).abs() <= 20,
            "with lamps {lamps:?} the bar stacks at {stacked_at} px, expected about {expected}"
        );
    }
}

/// Which of the bar's two states are lit in a fixture.
#[derive(Debug, Clone, Copy)]
enum Lamps {
    None,
    OnAir,
    Both,
}

impl Lamps {
    fn labels(self) -> &'static [&'static str] {
        match self {
            Lamps::None => &[],
            Lamps::OnAir => &["ON AIR"],
            Lamps::Both => &["ON AIR", "REC"],
        }
    }
}

/// A host session with the cluster in a given state, for the layout sweep.
fn host_harness_lamps(size: egui::Vec2, lamps: Lamps) -> Harness<'static> {
    let demo = DemoRuntime::frozen(FROZEN_FRAME, true);
    match lamps {
        Lamps::None => {}
        Lamps::OnAir => demo.set_destinations(&[(
            jamstream_client::runtime::StreamPlatform::Twitch,
            DestinationState::Live,
        )]),
        Lamps::Both => {
            demo.set_destinations(&[(
                jamstream_client::runtime::StreamPlatform::Twitch,
                DestinationState::Live,
            )]);
            demo.set_record(RecordState::Recording, false);
        }
    }
    let rt: Recorder = Arc::new(RecordingRuntime::new(demo));
    let rt_ui = rt.clone();
    let mut screen = SessionScreen {
        invites: Some(empty_invites()),
        destinations: Some(DestinationsPanel::new(Arc::new(MemStore::default()))),
        ..Default::default()
    };
    Harness::builder()
        .with_size(size)
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            let snap = rt_ui.snapshot();
            screen.ui(ui, &snap, &*rt_ui);
        })
}

/// The strip invariant behind #70: a fader's track and handle may never be
/// drawn across the name or the portrait above it. The rows below a fader
/// stack from the bottom edge upward, so a fader handed less room than it
/// asked for used to take the difference out of the header.
#[test]
fn a_fader_never_crosses_the_name_above_it() {
    for size in SIZES {
        for host in [false, true] {
            let mut harness = if host {
                host_harness_sized(size)
            } else {
                session_harness_sized(false, size).1
            };
            harness.run_steps(3);
            for member in ["Ana", "Ben", "Mira"] {
                let fader = harness.get_by_label(&format!("{member} fader")).rect();
                for name in harness.get_all_by_label(member) {
                    let rect = name.rect();
                    assert!(
                        !rect.intersects(fader),
                        "{member}'s name at {rect:?} sits inside its fader at {fader:?},                          window {size:?}, host {host}"
                    );
                }
            }
        }
    }
}

/// The other half of #70, and the whole of #173: reserving the floor is not
/// the same as delivering it. The console counted 22 px for a button row and
/// 16 for the dB readout, egui drew both taller, and the fader absorbed the
/// difference: 17 px tall on a host strip at the smallest window, seven
/// pixels of travel for 66 dB, with Ana at -3.0, Ben at -1.5 and Mira at
/// -6.0 all putting the handle in the same place.
#[test]
fn a_fader_gets_its_floor_and_stays_in_the_window() {
    use jamstream_client::screens::session::MIN_FADER_H;

    for size in SIZES {
        for host in [false, true] {
            let mut harness = if host {
                host_harness_sized(size)
            } else {
                session_harness_sized(false, size).1
            };
            harness.run_steps(3);
            for member in ["Ana", "Ben", "Mira"] {
                let fader = harness.get_by_label(&format!("{member} fader")).rect();
                assert!(
                    fader.height() >= MIN_FADER_H,
                    "{member}'s fader is {:.1} px of the {MIN_FADER_H} the console reserved, \
                     window {size:?}, host {host}",
                    fader.height()
                );
                // And the console does not answer the floor by scrolling the
                // strip half out of the window at the sizes the app opens at.
                assert!(
                    fader.top() >= 0.0 && fader.bottom() <= size.y,
                    "{member}'s fader at {fader:?} runs outside the {size:?} window, host {host}"
                );
            }
        }
    }
}

/// The whole shell with the settings drawer open over a host session, which
/// is the tallest sheet in the app over the shortest status bar it can sit
/// above. `in_memory` for the same reason every fixture uses it: the real
/// keychain would put a system dialog in front of the test run.
fn settings_harness_sized(size: egui::Vec2) -> Harness<'static> {
    session_shell_sized(size, true)
}

/// The same shell with the drawer closed, for the pair of assertions that need
/// to see what the drawer is over.
fn session_shell_sized(size: egui::Vec2, settings_open: bool) -> Harness<'static> {
    use jamstream_client::app::{JamApp, Screen};

    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        true,
    )));
    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.runtime = Some(Box::new(rt));
    app.screen = Screen::Session;
    app.session.invites = Some(empty_invites());
    app.session.destinations = Some(DestinationsPanel::new(Arc::new(MemStore::default())));
    app.settings_open = settings_open;
    Harness::builder()
        .with_size(size)
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
        })
}

/// The drawer is the full height of the window between the two bars and wider
/// than the chat panel, so the panel's message field showed under its bottom
/// edge: a lone text input with a message placeholder and no conversation above
/// it, two surfaces overlapping at the bottom edge the way the two sheets did in
/// #175 (#286). The panel holds its width and draws nothing while it is covered.
///
/// The wide layout only: below 900 px the chat is behind its own toggle and the
/// drawer takes less than half the window, so there is nothing to cover.
#[test]
fn the_drawer_leaves_no_chat_field_poking_out_below_it() {
    let size = vec2(1280.0, 800.0);
    let mut open = session_shell_sized(size, true);
    open.run_steps(4);
    let poking: Vec<egui::Rect> = open
        .query_all_by_role(AkRole::TextInput)
        .map(|n| n.rect())
        .collect();
    assert!(
        poking.is_empty(),
        "the drawer is over the chat panel and these fields are still drawn: {poking:?}"
    );

    // And the field is really there to be covered: the same shell with the
    // drawer closed draws it, so the assertion above is about the drawer and
    // not about a field that never existed.
    let mut closed = session_shell_sized(size, false);
    closed.run_steps(4);
    let field = closed
        .query_all_by_role(AkRole::TextInput)
        .map(|n| n.rect())
        .next()
        .expect("the chat panel's message field");
    assert!(
        field.right() > size.x - 300.0,
        "the field at {field:?} is not the chat panel's"
    );
}

/// The invariant behind #122. Buffer size and input level are adjusted while
/// listening, against the mouth-to-ear readout and the meters in the status
/// bar, so the drawer has to fit the window and stop above them: at 800x600
/// the sheet used to run past the bottom edge with both of them on the part
/// that was gone. Every window size the app can be, both are on screen, and
/// nothing in the drawer is drawn over the readouts.
#[test]
fn the_settings_drawer_fits_the_window_and_clears_the_readouts() {
    for size in SIZES {
        let mut harness = settings_harness_sized(size);
        harness.run_steps(4);
        let readouts = harness
            .get_all_by_label_contains("mouth to ear")
            .next()
            .expect("the mouth-to-ear readout")
            .rect();
        // What a musician reaches for mid session is on screen with nothing
        // scrolled, along with the way out of the drawer and the way between
        // its tabs. The tab row is pinned with Close for exactly this reason.
        for label in [
            "Close",
            "Audio",
            "Broadcast",
            "Invites",
            "You",
            "120 frames (2.5 ms)",
            "480 frames (10.0 ms)",
            "speak or play to check the meter moves",
        ] {
            let rect = harness
                .get_all_by_label_contains(label)
                .next()
                .unwrap_or_else(|| panic!("{label} is not on screen at {size:?}"))
                .rect();
            assert!(
                rect.bottom() <= readouts.top(),
                "{label} at {rect:?} reaches the readouts at {readouts:?}, window {size:?}"
            );
            assert!(
                rect.right() <= size.x && rect.top() >= 0.0,
                "{label} at {rect:?} is outside the {size:?} window"
            );
        }
        // The end of the audio panel, which a short window has no room for,
        // comes into view when the body is scrolled. A sheet with no height of
        // its own has no body to scroll: it grows to its content, past the
        // bottom edge, and this is what that leaves unreachable.
        scroll_drawer(&mut harness, size);
        let last = harness
            .get_all_by_label_contains("Playback")
            .next()
            .expect("the playback picker is the last thing in the audio panel")
            .rect();
        assert!(
            last.top() >= 0.0 && last.bottom() <= readouts.top(),
            "the end of the panel is at {last:?}, out of reach above the readouts at \
             {readouts:?}, window {size:?}"
        );
        // The tab row is still there after the scroll: only the panel moved.
        for label in ["Close", "Audio", "You"] {
            let rect = harness
                .get_all_by_label_contains(label)
                .next()
                .unwrap_or_else(|| panic!("{label} scrolled away at {size:?}"))
                .rect();
            assert!(
                rect.top() >= 0.0 && rect.bottom() <= readouts.top(),
                "{label} at {rect:?} left the drawer's header at {size:?}"
            );
        }
    }
}

/// Each tab shows its own panel and only its own, and the drawer remembers
/// which one while the app runs. Four panels in one scroll was the shape this
/// replaced, and a tab that carried another's scroll offset would be the same
/// mistake one layer down.
#[test]
fn each_settings_tab_shows_its_own_panel_and_the_choice_sticks() {
    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        true,
    )));
    let mut harness = drawer_harness(rt, SettingsTab::Audio, vec2(1280.0, 800.0));
    harness.run_steps(3);

    // Audio, the tab a fresh launch opens on.
    assert!(harness.query_by_label_contains("Buffer size").is_some());
    assert!(harness.query_by_label("Ana stream gain").is_none());
    assert!(
        harness
            .query_by_label_contains("One seat per link")
            .is_none()
    );

    // Broadcast: the mix and the destinations, and nothing from Audio.
    harness
        .get_by_role_and_label(AkRole::Button, "Broadcast")
        .click();
    harness.run_steps(2);
    assert!(harness.query_by_label("Ana stream gain").is_some());
    assert!(
        harness
            .query_by_label_contains("Where this session streams")
            .is_some()
    );
    assert!(harness.query_by_label_contains("Buffer size").is_none());

    // Invites.
    harness
        .get_by_role_and_label(AkRole::Button, "Invites")
        .click();
    harness.run_steps(2);
    assert!(
        harness
            .query_by_label_contains("One seat per link")
            .is_some()
    );
    assert!(harness.query_by_label("Ana stream gain").is_none());

    // You: the avatar row and the theme picker, the two things set once.
    harness.get_by_role_and_label(AkRole::Button, "You").click();
    harness.run_steps(2);
    assert!(harness.query_by_label_contains("Your avatar").is_some());
    // The theme picker's row and its label both carry the word, so this is a
    // count rather than a single match.
    assert!(harness.get_all_by_label_contains("light").next().is_some());
    assert!(
        harness
            .query_by_label_contains("One seat per link")
            .is_none()
    );

    // Close and reopen: the drawer comes back on the tab it was left on.
    harness
        .get_by_role_and_label(AkRole::Button, "Close")
        .click();
    harness.run_steps(2);
    assert!(harness.query_by_label_contains("Your avatar").is_none());
    harness
        .get_by_role_and_label(AkRole::Button, "Settings")
        .click();
    harness.run_steps(2);
    assert!(
        harness.query_by_label_contains("Your avatar").is_some(),
        "the drawer must reopen on the tab it was left on"
    );
}

/// The tab row is built from what exists. A plain join has no invite book and
/// no destinations panel, so it has no Broadcast or Invites tab, and no dead
/// slot where they would have been. Audio, Recording and You are settings for
/// this computer, so they are there whatever the window is showing.
#[test]
fn a_musician_and_the_home_screen_get_only_the_tabs_that_mean_anything() {
    use jamstream_client::app::{JamApp, Screen};

    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        false,
    )));
    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.runtime = Some(Box::new(rt));
    app.screen = Screen::Session;
    app.settings_open = true;
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
        });
    harness.run_steps(3);
    assert!(
        harness
            .query_by_role_and_label(AkRole::Button, "Audio")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(AkRole::Button, "You")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(AkRole::Button, "Broadcast")
            .is_none(),
        "a musician has no broadcast to mix"
    );
    assert!(
        harness
            .query_by_role_and_label(AkRole::Button, "Invites")
            .is_none(),
        "a musician has no seats to hand out"
    );
    assert!(
        harness
            .query_by_role_and_label(AkRole::Button, "Recording")
            .is_some(),
        "where takes go is a setting for this computer, not for this session"
    );

    // And outside a session entirely, from home, the machine-local three.
    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.settings_open = true;
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
        });
    harness.run_steps(3);
    assert!(
        harness
            .query_by_role_and_label(AkRole::Button, "Audio")
            .is_some()
    );
    assert!(
        harness
            .query_by_role_and_label(AkRole::Button, "Broadcast")
            .is_none(),
        "there is no session to broadcast from the home screen"
    );
    assert!(
        harness
            .query_by_role_and_label(AkRole::Button, "Recording")
            .is_some(),
        "a bucket is set up before the session that needs it, so the tab is here too"
    );
}

/// The Recording tab, driven the way a host does: open it, paste a pair, and
/// press Check with no bucket named. The refusal is on screen, nothing was
/// spawned, and neither half of the pair is readable anywhere.
#[test]
fn the_recording_tab_refuses_a_check_with_no_bucket_and_never_shows_the_key() {
    use jamstream_client::app::JamApp;
    use jamstream_client::screens::session::SettingsTab;

    const ID: &str = "DO00FAKEFAKEFAKEFAKE";
    const SECRET: &str = "0000000000000000000000000000000000000000fake";

    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.settings_open = true;
    app.settings_tab = SettingsTab::Recording;
    app.recording.type_key(ID, SECRET);
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
        });
    harness.run_steps(3);
    assert!(
        harness.query_by_label_contains("bucket region").is_some(),
        "the tab must be showing"
    );
    harness
        .get_by_role_and_label(AkRole::Button, "Check")
        .click();
    harness.run_steps(3);
    assert!(
        harness
            .query_all_by_label_contains("name the bucket")
            .count()
            > 0,
        "a check with no bucket must say so rather than reaching the network"
    );
    for secret in [ID, SECRET] {
        assert_eq!(
            harness
                .query_all_by(move |node| {
                    node.label().is_some_and(|l| l.contains(secret))
                        || node.value().is_some_and(|v| v.contains(secret))
                })
                .count(),
            0,
            "the storage key is readable on screen"
        );
    }
}

/// Escape closes the drawer before the session screen sees the key, so the
/// innermost thing entered is the first thing left. The drawer is drawn
/// after the screen, which is the order that makes it possible to measure
/// the status bar, and this is the one thing that order could have broken.
#[test]
fn escape_closes_the_settings_drawer_before_a_session_sheet() {
    let mut harness = settings_harness_sized(vec2(1280.0, 800.0));
    harness.run_steps(2);
    assert!(harness.query_by_label_contains("Buffer size").is_some());
    harness.key_press(Key::Escape);
    harness.run_steps(2);
    assert!(
        harness.query_by_label_contains("Buffer size").is_none(),
        "Escape must close the drawer"
    );
}

/// #180, the other end of the same ladder: with the drawer open, Revoke on a
/// strip puts a confirmation on top of it, and Escape used to close the drawer
/// underneath and leave the confirmation standing. The innermost thing entered
/// is the first thing left, and the drawer is not the innermost thing here.
#[test]
fn escape_leaves_a_confirmation_over_the_drawer_before_the_drawer() {
    let mut harness = settings_harness_sized(vec2(1280.0, 800.0));
    harness.run_steps(2);
    // Any strip's Revoke; there is one per member and they behave alike.
    harness
        .get_all_by_role_and_label(AkRole::Button, "Revoke")
        .next()
        .expect("a strip carries Revoke")
        .click();
    harness.run_steps(2);
    assert!(
        harness
            .query_by_label_contains("They will be disconnected")
            .is_some(),
        "the confirmation must be up"
    );

    harness.key_press(Key::Escape);
    harness.run_steps(2);
    assert!(
        harness
            .query_by_label_contains("They will be disconnected")
            .is_none(),
        "the first Escape belongs to the confirmation"
    );
    assert!(
        harness.query_by_label_contains("Buffer size").is_some(),
        "and it must not have taken the drawer with it"
    );

    harness.key_press(Key::Escape);
    harness.run_steps(2);
    assert!(
        harness.query_by_label_contains("Buffer size").is_none(),
        "the second Escape closes the drawer"
    );
}

/// The record sheet and the settings drawer share one right-hand anchor, so
/// opening either closes the other. With both open, 44 px of the wider sheet
/// stuck out to the left of the drawer showing chopped words, and a truncated
/// Stop in that fragment was still clickable, so a stray click there ended the
/// take (#175).
#[test]
fn the_record_sheet_and_the_drawer_are_never_open_at_once() {
    use jamstream_client::app::{JamApp, Screen};

    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        true,
    )));
    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.runtime = Some(Box::new(rt));
    app.screen = Screen::Session;
    app.session.invites = Some(empty_invites());
    app.session.destinations = Some(DestinationsPanel::new(Arc::new(MemStore::default())));
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
        });
    harness.run_steps(2);

    // The sheet first, then the drawer over it.
    harness
        .get_by_role_and_label(AkRole::Button, "Record")
        .click();
    harness.run_steps(2);
    assert!(
        harness
            .query_by_label_contains("kept in the session's storage")
            .is_some()
    );
    harness
        .get_by_role_and_label(AkRole::Button, "Settings")
        .click();
    harness.run_steps(2);
    assert!(
        harness.query_by_label_contains("Buffer size").is_some(),
        "the drawer must have opened"
    );
    assert!(
        harness
            .query_by_label_contains("kept in the session's storage")
            .is_none(),
        "the record sheet must not still be behind the drawer"
    );
    // And the other way round: the sheet takes the anchor back.
    harness
        .get_by_role_and_label(AkRole::Button, "Record")
        .click();
    harness.run_steps(2);
    assert!(
        harness
            .query_by_label_contains("kept in the session's storage")
            .is_some()
    );
    assert!(
        harness.query_by_label_contains("Buffer size").is_none(),
        "the drawer must have closed for the sheet"
    );
}

/// #174: a strip's dot is about that member. The runtime reports one musician
/// away while your own link is fine, so a dot that carried your rtt would read
/// the same on every strip. What is on screen is one dot per musician saying
/// where they are, and the link numbers stay in the bar, where they are yours.
#[test]
fn a_strips_dot_follows_the_member_not_your_own_link() {
    use jamstream_client::widgets::{PRESENCE_AWAY, PRESENCE_HERE};

    let demo = DemoRuntime::frozen(FROZEN_FRAME, false);
    demo.set_away(2, true); // the roster gave up on Ben
    let snap = demo.snapshot();
    assert!(
        snap.stats.rtt_ms.is_some_and(|rtt| rtt < 25.0),
        "your own link has to be reading well for this test to mean anything"
    );
    let musicians = snap
        .members
        .iter()
        .filter(|m| m.role == jamstream_client::runtime::Role::Musician)
        .count();

    let rt: Recorder = Arc::new(RecordingRuntime::new(demo));
    let rt_ui = rt.clone();
    let mut screen = SessionScreen::default();
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            let snap = rt_ui.snapshot();
            screen.ui(ui, &snap, &*rt_ui);
        });
    harness.run_steps(3);

    let here = harness.get_all_by_label(PRESENCE_HERE).count();
    let away = harness.get_all_by_label(PRESENCE_AWAY).count();
    assert_eq!(
        away, 1,
        "exactly the member the roster reports away reads away"
    );
    assert_eq!(
        here + away,
        musicians,
        "one dot per musician, each saying where that member is"
    );
    // The link numbers belong to one place, and it is not a strip.
    assert!(
        harness.query_by_label_contains("mouth to ear").is_some(),
        "your own link stays in the bar"
    );
}

/// #285: the roster carries three presence states now, and every one of them
/// has to reach the console as itself. The middle one is the point: before it
/// existed a member who stopped playing two seconds ago and a member playing
/// looked identical for eight seconds, until the server gave up and the strip
/// greyed out.
///
/// Read by label, not by pixel, because that is also what a screen reader gets
/// off a painted dot.
#[test]
fn a_strip_says_which_of_the_three_presences_a_member_is_in() {
    use jamstream_client::widgets::{PRESENCE_AWAY, PRESENCE_HERE, PRESENCE_QUIET};

    let demo = DemoRuntime::frozen(FROZEN_FRAME, false);
    // Ana has gone quiet, Ben is gone, and the rest are playing.
    demo.set_quiet(1, true);
    demo.set_away(2, true);
    let snap = demo.snapshot();
    let musicians = snap
        .members
        .iter()
        .filter(|m| m.role == jamstream_client::runtime::Role::Musician)
        .count();
    assert!(musicians >= 3, "three states need three musicians");

    let rt: Recorder = Arc::new(RecordingRuntime::new(demo));
    let rt_ui = rt.clone();
    let mut screen = SessionScreen::default();
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            let snap = rt_ui.snapshot();
            screen.ui(ui, &snap, &*rt_ui);
        });
    harness.run_steps(3);

    let quiet = harness.get_all_by_label(PRESENCE_QUIET).count();
    let away = harness.get_all_by_label(PRESENCE_AWAY).count();
    let here = harness.get_all_by_label(PRESENCE_HERE).count();
    assert_eq!(quiet, 1, "exactly the member reported quiet reads quiet");
    assert_eq!(away, 1, "and the member reported gone still reads gone");
    assert_eq!(
        here + quiet + away,
        musicians,
        "one dot per musician, each in exactly one of the three states"
    );

    // Quiet is not gone: the seat is held, so the strip keeps its controls and
    // says nothing about being disconnected.
    let ana = snap
        .members
        .iter()
        .find(|m| m.quiet)
        .expect("the roster reports one member quiet");
    assert!(
        ana.connected,
        "a quiet member is still connected, or this is not the middle state"
    );
    assert_eq!(
        harness.get_all_by_label("disconnected").count(),
        1,
        "only the member the roster gave up on reads disconnected"
    );
}

fn set_fader_commands(rt: &Recorder, member: u16) -> Vec<(f32, f32, bool)> {
    rt.commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::SetFader {
                member: m,
                gain_db,
                pan,
                muted,
            } if m == MemberId(member) => Some((gain_db, pan, muted)),
            _ => None,
        })
        .collect()
}

#[test]
fn fader_drag_sends_set_fader() {
    let (rt, mut harness) = session_harness(false);
    harness.run_steps(2);
    let center = harness.get_by_label("Ana fader").rect().center();

    harness.event(Event::PointerMoved(center));
    harness.step();
    harness.event(Event::PointerButton {
        pos: center,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::NONE,
    });
    harness.step();
    let up = center - vec2(0.0, 30.0);
    harness.event(Event::PointerMoved(up));
    harness.step();
    harness.event(Event::PointerButton {
        pos: up,
        button: PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::NONE,
    });
    harness.run_steps(2);

    let faders = set_fader_commands(&rt, 1);
    assert!(!faders.is_empty(), "drag sent no SetFader command");
    let (gain, pan, muted) = *faders.last().unwrap();
    // Ana starts at -3.0 dB; dragging up raises gain and touches nothing else.
    assert!(gain > -3.0, "gain did not rise: {gain}");
    assert_eq!(pan, -0.4);
    assert!(!muted);
}

#[test]
fn fader_double_click_resets_to_zero_db() {
    let (rt, mut harness) = session_harness(false);
    harness.run_steps(2);
    // Two clicks in one frame land inside egui's 0.3 s double-click window.
    let node = harness.get_by_label("Ana fader");
    node.click();
    node.click();
    harness.run_steps(2);

    let faders = set_fader_commands(&rt, 1);
    assert!(!faders.is_empty(), "double-click sent no SetFader command");
    assert_eq!(faders.last().unwrap().0, 0.0, "gain must reset to 0 dB");
}

#[test]
fn chat_enter_sends_send_chat() {
    let (rt, mut harness) = session_harness(false);
    harness.run_steps(2);
    harness.get_by_role(AkRole::TextInput).click();
    harness.run_steps(1);
    harness
        .get_by_role(AkRole::TextInput)
        .type_text("take it from the bridge");
    harness.run_steps(1);
    harness.key_press(Key::Enter);
    harness.run_steps(2);

    assert!(
        rt.commands()
            .contains(&Command::SendChat("take it from the bridge".to_owned())),
        "commands were: {:?}",
        rt.commands()
    );
}

#[test]
fn leave_confirms_before_sending() {
    let (rt, mut harness) = session_harness(false);
    harness.run_steps(2);
    harness.get_by_label("Leave").click();
    harness.run_steps(2);
    // Only the confirm actually leaves.
    assert!(!rt.commands().contains(&Command::Leave));
    harness
        .get_by_role_and_label(AkRole::Button, "Leave session")
        .click();
    harness.run_steps(2);
    assert!(rt.commands().contains(&Command::Leave));
}

#[test]
fn leave_cancel_sends_nothing() {
    let (rt, mut harness) = session_harness(false);
    harness.run_steps(2);
    harness.get_by_label("Leave").click();
    harness.run_steps(2);
    harness
        .get_by_role_and_label(AkRole::Button, "Cancel")
        .click();
    harness.run_steps(2);
    assert!(rt.commands().is_empty());
}

#[test]
fn tab_reaches_a_fader_and_arrows_nudge_gain() {
    let (rt, mut harness) = session_harness(false);
    harness.run_steps(2);
    // Your own fader is disabled, so the first focusable fader is Ana's.
    let mut reached = false;
    for _ in 0..40 {
        harness.key_press(Key::Tab);
        harness.run_steps(1);
        if harness.get_by_label("Ana fader").is_focused() {
            reached = true;
            break;
        }
    }
    assert!(reached, "tab never reached the first fader");
    harness.key_press(Key::ArrowUp);
    harness.run_steps(2);

    let faders = set_fader_commands(&rt, 1);
    assert!(!faders.is_empty(), "arrow key sent no SetFader command");
    // One 0.5 dB step up from -3.0.
    assert_eq!(faders.last().unwrap().0, -2.5);
}

#[test]
fn revoke_needs_confirmation_and_sends_the_token() {
    let (rt, mut harness) = session_harness(true);
    harness.run_steps(2);
    // Every non-you musician strip has a revoke button; take Ana's (first).
    harness.get_all_by_label("Revoke").next().unwrap().click();
    harness.run_steps(2);
    assert!(
        !rt.commands()
            .iter()
            .any(|c| matches!(c, Command::Revoke(_))),
        "revoke must not fire before confirmation"
    );
    harness
        .get_by_role_and_label(AkRole::Button, "Revoke invite")
        .click();
    harness.run_steps(2);
    let revokes: Vec<_> = rt
        .commands()
        .into_iter()
        .filter(|c| matches!(c, Command::Revoke(_)))
        .collect();
    assert_eq!(revokes.len(), 1);
    // Ana is member 1; the demo token is the member id repeated.
    assert_eq!(
        revokes[0],
        Command::Revoke(jamstream_client::runtime::TokenId([1; 16]))
    );
}

#[test]
fn narrow_chat_toggle_is_symmetric_and_escape_closes() {
    // Below 900 px chat replaces the mixer; the same stationary toggle
    // reopens the mixer, Escape does too, and the round trip sends nothing.
    let (rt, mut harness) = session_harness_sized(false, vec2(800.0, 600.0));
    harness.run_steps(2);
    assert!(harness.query_by_label("Ana fader").is_some());

    harness.get_by_label("Chat").click();
    harness.run_steps(2);
    assert!(
        harness.query_by_label("Ana fader").is_none(),
        "chat must replace the mixer in the narrow layout"
    );
    harness.get_by_label("Chat").click();
    harness.run_steps(2);
    assert!(
        harness.query_by_label("Ana fader").is_some(),
        "the same toggle must return to the mixer"
    );

    harness.get_by_label("Chat").click();
    harness.run_steps(2);
    assert!(harness.query_by_label("Ana fader").is_none());
    harness.key_press(Key::Escape);
    harness.run_steps(2);
    assert!(
        harness.query_by_label("Ana fader").is_some(),
        "Escape must close chat and return to the mixer"
    );

    assert!(
        rt.commands().is_empty(),
        "toggling views must not send commands: {:?}",
        rt.commands()
    );
}

fn broadcast_fader_commands(rt: &Recorder, member: u16) -> Vec<(f32, f32, bool)> {
    rt.commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::SetBroadcastFader {
                member: m,
                gain_db,
                pan,
                muted,
            } if m == MemberId(member) => Some((gain_db, pan, muted)),
            _ => None,
        })
        .collect()
}

fn audition_commands(rt: &Recorder) -> Vec<bool> {
    rt.commands()
        .into_iter()
        .filter_map(|c| match c {
            Command::SetBroadcastAudition(on) => Some(on),
            _ => None,
        })
        .collect()
}

#[test]
fn the_broadcast_tab_carries_the_mix_and_a_fader_sends_exact_values() {
    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        true,
    )));
    // The drawer starts on Audio, so the mix is not on screen until the tab
    // is chosen: the tab row is the entry point now.
    let mut harness = drawer_harness(rt.clone(), SettingsTab::Audio, vec2(1280.0, 800.0));
    harness.run_steps(2);
    assert!(harness.query_by_label("Ana stream gain").is_none());

    harness
        .get_by_role_and_label(AkRole::Button, "Broadcast")
        .click();
    harness.run_steps(2);
    // Focus the gain control, then one 0.5 dB arrow step from Ana's demo
    // broadcast fader (-2.0 dB, pan -0.3, unmuted).
    harness.get_by_label("Ana stream gain").click();
    harness.run_steps(2);
    harness.key_press(Key::ArrowUp);
    harness.run_steps(2);

    let faders = broadcast_fader_commands(&rt, 1);
    assert!(!faders.is_empty(), "arrow key sent no SetBroadcastFader");
    assert_eq!(
        *faders.last().unwrap(),
        (-1.5, -0.3, false),
        "exactly one 0.5 dB step up, pan and mute untouched"
    );
    // The monitor mix must be untouched: no SetFader at all.
    assert!(
        !rt.commands()
            .iter()
            .any(|c| matches!(c, Command::SetFader { .. })),
        "broadcast rows must never send monitor SetFader"
    );

    // And back to Audio: one panel at a time, so the mix goes away and the
    // audio controls come back.
    harness
        .get_by_role_and_label(AkRole::Button, "Audio")
        .click();
    harness.run_steps(2);
    assert!(harness.query_by_label("Ana stream gain").is_none());
    assert!(harness.query_by_label_contains("Buffer size").is_some());
}

#[test]
fn audition_round_trip_and_closing_the_drawer_leaves_it_on() {
    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        true,
    )));
    let mut harness = drawer_harness(rt.clone(), SettingsTab::Broadcast, vec2(1280.0, 800.0));
    harness.run_steps(2);

    harness.get_by_label("audition stream mix").click();
    harness.run_steps(2);
    assert_eq!(audition_commands(&rt), vec![true]);
    assert!(
        harness.query_by_label("AUDITION").is_some(),
        "the bar's centre cluster must show the audition lamp"
    );

    // Escape closes the drawer; audition is a mix state, not navigation, so
    // it stays on and the reminder stays visible.
    harness.key_press(Key::Escape);
    harness.run_steps(2);
    assert!(harness.query_by_label("audition stream mix").is_none());
    assert_eq!(audition_commands(&rt), vec![true]);
    assert!(
        rt.snapshot()
            .broadcast
            .expect("host broadcast view")
            .audition
    );
    assert!(
        harness.query_by_label("AUDITION").is_some(),
        "closing the drawer must not take the audition lamp with it"
    );

    // Reopen and switch it off. The drawer remembers the tab it was on, so
    // the mix is back without choosing Broadcast again.
    harness
        .get_by_role_and_label(AkRole::Button, "Settings")
        .click();
    harness.run_steps(2);
    harness.get_by_label("audition stream mix").click();
    harness.run_steps(2);
    assert_eq!(audition_commands(&rt), vec![true, false]);
    assert!(harness.query_by_label("AUDITION").is_none());
}

#[test]
fn non_hosts_see_no_stream_mix() {
    let (rt, mut harness) = session_harness(false);
    harness.run_steps(2);
    assert!(harness.query_by_label("Ana stream gain").is_none());
    assert!(harness.query_by_label("AUDITION").is_none());
    assert!(rt.snapshot().broadcast.is_none());
}

// Destinations. The key path is the one that has to be exactly right: what
// the host pastes is what the server gets, once, and nothing else keeps it.

/// A key nothing could stream with.
const FAKE_KEY: &str = "live_000000_fakefakefake";

/// The whole shell with the drawer open on the Broadcast tab, which is where
/// destinations live now. The tab is reached through the real app rather than
/// a screen on its own, because the entry point is part of what these tests
/// are checking.
fn destinations_harness(
    reported: &[(jamstream_client::runtime::StreamPlatform, DestinationState)],
) -> (Recorder, Harness<'static>) {
    let demo = DemoRuntime::frozen(FROZEN_FRAME, true);
    demo.set_destinations(reported);
    let rt: Recorder = Arc::new(RecordingRuntime::new(demo));
    let harness = drawer_harness(rt.clone(), SettingsTab::Broadcast, vec2(1280.0, 800.0));
    (rt, harness)
}

/// Scrolls the drawer's panel to its end, the way a pointer over it does.
fn scroll_drawer(harness: &mut Harness<'_>, size: egui::Vec2) {
    harness.event(Event::PointerMoved(egui::pos2(size.x - 100.0, 300.0)));
    harness.run_steps(1);
    harness.event(Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: vec2(0.0, -1000.0),
        phase: egui::TouchPhase::Move,
        modifiers: Modifiers::NONE,
    });
    harness.run_steps(3);
}

/// The app on a host session with the settings drawer open on `tab`.
fn drawer_harness(rt: Recorder, tab: SettingsTab, size: egui::Vec2) -> Harness<'static> {
    use jamstream_client::app::{JamApp, Screen};

    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.runtime = Some(Box::new(rt));
    app.screen = Screen::Session;
    app.session.invites = Some(empty_invites());
    app.session.destinations = Some(DestinationsPanel::new(Arc::new(MemStore::default())));
    app.settings_open = true;
    app.settings_tab = tab;
    Harness::builder()
        .with_size(size)
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
        })
}

fn destination_commands(rt: &Recorder) -> Vec<Command> {
    rt.commands()
        .into_iter()
        .filter(|c| {
            matches!(
                c,
                Command::AddDestination { .. }
                    | Command::RemoveDestination(_)
                    | Command::StartStream
                    | Command::StopStream
            )
        })
        .collect()
}

#[test]
fn a_pasted_key_reaches_the_server_once_and_then_going_live() {
    let (rt, mut harness) = destinations_harness(&[]);
    harness.run_steps(2);
    // Nothing configured: Go live has nothing to do.
    assert!(destination_commands(&rt).is_empty());

    harness
        .get_all_by_role_and_label(AkRole::Button, "Add key")
        .next()
        .expect("Twitch add key")
        .click();
    harness.run_steps(2);
    // The one masked field on screen. Its accessible role says so, which is
    // itself worth asserting: a stream key is never a plain text input.
    harness.get_by_role(AkRole::PasswordInput).click();
    harness.run_steps(1);
    harness
        .get_by_role(AkRole::PasswordInput)
        .type_text(FAKE_KEY);
    harness.run_steps(2);
    // Still nothing on the wire: typing is not sending.
    assert!(destination_commands(&rt).is_empty());
    // And the key is not in the accessibility tree either, only its mask.
    let shown = harness
        .get_by_role(AkRole::PasswordInput)
        .value()
        .unwrap_or_default();
    assert!(
        !shown.contains(FAKE_KEY) && shown.chars().count() == FAKE_KEY.chars().count(),
        "a password field must expose the mask, not the key: {shown:?}"
    );

    // The key pane makes the Broadcast tab taller than the drawer at this
    // window, so Save key is below the fold: scroll to it, which is what a
    // host does. Everything above it was reachable without scrolling.
    scroll_drawer(&mut harness, vec2(1280.0, 800.0));
    harness
        .get_by_role_and_label(AkRole::Button, "Save key")
        .click();
    harness.run_steps(2);
    let sent = destination_commands(&rt);
    assert_eq!(sent.len(), 1, "commands were {sent:?}");
    match &sent[0] {
        Command::AddDestination { id, platform, key } => {
            assert_eq!(*id, jamstream_client::runtime::DestinationId(0));
            assert_eq!(*platform, jamstream_client::runtime::StreamPlatform::Twitch);
            assert_eq!(key.expose(), FAKE_KEY, "the key must arrive verbatim");
        }
        other => panic!("expected AddDestination, got {other:?}"),
    }
    // The field is gone from the screen, and the row now reads as a
    // destination the server knows about.
    harness.run_steps(2);
    assert!(
        harness.query_by_role(AkRole::PasswordInput).is_none(),
        "the key pane must close once the key is sent"
    );

    harness
        .get_by_role_and_label(AkRole::Button, "Go live")
        .click();
    harness.run_steps(2);
    assert_eq!(
        destination_commands(&rt).last(),
        Some(&Command::StartStream)
    );
    // The demo's stand-in server brings it up, so the whole room is on air
    // and the bar's centre cluster says so.
    assert!(rt.snapshot().stream.on_air());
    assert!(harness.query_by_label("ON AIR").is_some());
}

/// The key pane's two keystrokes, both of which it was missing (#180): Enter
/// saves, the way the join field and the chat field do, and Escape dismisses a
/// half typed key without sending it. Escape reaches the pane before the
/// drawer the pane is in.
#[test]
fn the_key_pane_saves_on_enter_and_leaves_on_escape() {
    let (rt, mut harness) = destinations_harness(&[]);
    harness.run_steps(2);
    harness
        .get_all_by_role_and_label(AkRole::Button, "Add key")
        .next()
        .expect("Twitch add key")
        .click();
    harness.run_steps(2);
    harness.get_by_role(AkRole::PasswordInput).click();
    harness.run_steps(1);
    harness.get_by_role(AkRole::PasswordInput).type_text("half");
    harness.run_steps(2);

    // Escape dismisses the pane, and stops there: the drawer it is in is the
    // next thing out, not the first.
    harness.key_press(Key::Escape);
    harness.run_steps(2);
    assert!(
        harness.query_by_role(AkRole::PasswordInput).is_none(),
        "Escape must dismiss the key pane"
    );
    assert!(
        harness
            .query_by_label_contains("Where this session streams")
            .is_some(),
        "and must not have reached past it to the drawer"
    );
    assert!(
        destination_commands(&rt).is_empty(),
        "a dismissed key is not a sent key"
    );

    // Reopen, type, and press Enter: the same command Save key sends.
    harness
        .get_all_by_role_and_label(AkRole::Button, "Add key")
        .next()
        .expect("Twitch add key")
        .click();
    harness.run_steps(2);
    harness.get_by_role(AkRole::PasswordInput).click();
    harness.run_steps(1);
    harness
        .get_by_role(AkRole::PasswordInput)
        .type_text(FAKE_KEY);
    harness.run_steps(2);
    harness.key_press(Key::Enter);
    harness.run_steps(2);
    let sent = destination_commands(&rt);
    assert_eq!(sent.len(), 1, "commands were {sent:?}");
    match &sent[0] {
        Command::AddDestination { platform, key, .. } => {
            assert_eq!(*platform, jamstream_client::runtime::StreamPlatform::Twitch);
            assert_eq!(key.expose(), FAKE_KEY, "the key must arrive verbatim");
        }
        other => panic!("expected AddDestination, got {other:?}"),
    }
}

#[test]
fn removing_one_live_destination_leaves_the_other_alone() {
    use jamstream_client::runtime::{DestinationId, StreamPlatform};
    let (rt, mut harness) = destinations_harness(&[
        (StreamPlatform::Twitch, DestinationState::Live),
        (StreamPlatform::YouTube, DestinationState::Live),
    ]);
    harness.run_steps(2);
    assert!(harness.query_by_label("ON AIR").is_some());

    // Twitch is the first row, so the first Remove is its own.
    harness
        .get_all_by_role_and_label(AkRole::Button, "Remove")
        .next()
        .expect("Twitch remove")
        .click();
    harness.run_steps(2);
    assert_eq!(
        destination_commands(&rt),
        vec![Command::RemoveDestination(DestinationId(0))]
    );
    // YouTube keeps streaming, which is the whole point of one pusher each.
    let snap = rt.snapshot();
    assert_eq!(snap.stream.live_count(), 1);
    assert_eq!(
        snap.stream
            .of_platform(StreamPlatform::YouTube)
            .map(|d| d.state.clone()),
        Some(DestinationState::Live)
    );
}

#[test]
fn stopping_the_stream_takes_everything_off_air() {
    use jamstream_client::runtime::StreamPlatform;
    let (rt, mut harness) = destinations_harness(&[
        (StreamPlatform::Twitch, DestinationState::Live),
        (StreamPlatform::YouTube, DestinationState::Live),
    ]);
    harness.run_steps(2);
    // Stop streaming is the last control on the Broadcast tab, under both
    // destinations, so it is below the fold at this window.
    scroll_drawer(&mut harness, vec2(1280.0, 800.0));
    harness
        .get_by_role_and_label(AkRole::Button, "Stop streaming")
        .click();
    harness.run_steps(2);
    assert_eq!(destination_commands(&rt), vec![Command::StopStream]);
    assert!(!rt.snapshot().stream.on_air());
}

#[test]
fn escape_closes_the_drawer_without_leaving_the_air() {
    use jamstream_client::runtime::StreamPlatform;
    let (rt, mut harness) =
        destinations_harness(&[(StreamPlatform::Twitch, DestinationState::Live)]);
    harness.run_steps(2);
    harness.key_press(Key::Escape);
    harness.run_steps(2);
    assert!(
        harness
            .query_by_role_and_label(AkRole::Button, "Stop streaming")
            .is_none(),
        "Escape must close the drawer"
    );
    // Closing the drawer is navigation: nothing was sent and the room is
    // still on air, with the cluster's lamp to say so.
    assert!(destination_commands(&rt).is_empty());
    assert!(harness.query_by_label("ON AIR").is_some());
}

#[test]
fn a_failed_destination_says_why_where_the_host_will_see_it() {
    use jamstream_client::runtime::StreamPlatform;
    let (_, mut harness) = destinations_harness(&[
        (StreamPlatform::Twitch, DestinationState::Live),
        (
            StreamPlatform::YouTube,
            DestinationState::Failed {
                reason: "pusher exited: rtmp connection refused".to_owned(),
            },
        ),
    ]);
    harness.run_steps(2);
    assert!(
        harness
            .query_by_label("pusher exited: rtmp connection refused")
            .is_some(),
        "the pipeline's reason must be on screen verbatim"
    );
    // And with the drawer closed, the bar still says one died, in the cluster
    // beside the lamp for the one that did not.
    harness.key_press(Key::Escape);
    harness.run_steps(2);
    assert!(harness.query_by_label("STREAM FAILED").is_some());
    assert!(harness.query_by_label("ON AIR").is_some());
}

/// A row's two actions read safe first, destructive second, and both sit under
/// the platform they belong to.
///
/// The pair used to be built inside a right_to_left layout, so it reached the
/// screen as "Forget key" then "Use saved key": destructive first, and the
/// opposite reading order from the same pair in the invites panel two tabs away
/// (#183). An unconfigured platform's lone "Add key" was pinned to the drawer's
/// right edge, a blank line below its own name (#192). Both are questions about
/// where a rect lands, so both are asserted on rects.
#[test]
fn a_destination_rows_actions_read_safe_first_and_sit_under_its_name() {
    use jamstream_client::creds::CredStore as _;
    use jamstream_client::runtime::StreamPlatform;
    let store = Arc::new(MemStore::default());
    let field = jamstream_client::creds::stream_key_field(StreamPlatform::Twitch);
    store.set(field.0, field.1, FAKE_KEY).expect("save a key");
    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        true,
    )));
    let rt_ui = rt.clone();
    let mut app = jamstream_client::app::JamApp::in_memory();
    app.recent = Vec::new();
    app.session.destinations = Some(DestinationsPanel::new(store));
    app.session.invites = Some(empty_invites());
    app.runtime = Some(Box::new(rt_ui));
    app.screen = jamstream_client::app::Screen::Session;
    app.settings_open = true;
    app.settings_tab = SettingsTab::Broadcast;
    let size = vec2(1280.0, 800.0);
    let mut harness = Harness::builder()
        .with_size(size)
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
        });
    harness.run_steps(2);
    scroll_drawer(&mut harness, size);
    harness.run_steps(2);

    let use_saved = harness.get_by_label("Use saved key").rect();
    let forget = harness.get_by_label("Forget key").rect();
    assert!(
        use_saved.left() < forget.left(),
        "Forget key reads first: Use saved key at {:?}, Forget key at {:?}",
        use_saved,
        forget
    );

    // The unconfigured platform's action is under its own name, not pinned to
    // the far edge of the drawer.
    let add_key = harness.get_by_label("Add key").rect();
    let youtube = harness.get_by_label_contains("YouTube Live").rect();
    assert!(
        add_key.left() < youtube.left() + 40.0,
        "Add key at {:?} is not under YouTube Live at {:?}",
        add_key,
        youtube
    );
    assert!(
        add_key.top() - youtube.bottom() < 24.0,
        "Add key at {:?} is a blank line below YouTube Live at {:?}",
        add_key,
        youtube
    );
}

#[test]
fn non_hosts_get_no_destination_controls_but_do_see_the_air() {
    let demo = DemoRuntime::frozen(FROZEN_FRAME, false);
    demo.set_destinations(&[(
        jamstream_client::runtime::StreamPlatform::Twitch,
        DestinationState::Live,
    )]);
    let rt: Recorder = Arc::new(RecordingRuntime::new(demo));
    let rt_ui = rt.clone();
    // A musician's screen has no panel to give it, the way it has no invites.
    let mut screen = SessionScreen::default();
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            let snap = rt_ui.snapshot();
            screen.ui(ui, &snap, &*rt_ui);
        });
    harness.run_steps(2);
    assert!(harness.query_by_label("Go live").is_none());
    assert!(harness.query_by_label("Record").is_none());
    // The lamp is not host-only: a musician is the one being broadcast.
    assert!(harness.query_by_label("ON AIR").is_some());
}

// Recording. One rule from the destinations sheet, applied whole: the
// button sends the command and the lamp follows the snapshot; nothing on
// this surface echoes a press optimistically.

fn record_harness(
    state: RecordState,
    is_host: bool,
    sheet_open: bool,
) -> (Recorder, Harness<'static>) {
    let demo = DemoRuntime::frozen(FROZEN_FRAME, is_host);
    demo.set_record(state, false);
    let rt: Recorder = Arc::new(RecordingRuntime::new(demo));
    let rt_ui = rt.clone();
    let mut screen = SessionScreen {
        record_open: sheet_open,
        ..Default::default()
    };
    let harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            let snap = rt_ui.snapshot();
            screen.ui(ui, &snap, &*rt_ui);
        });
    (rt, harness)
}

/// While the sheet is open and the take is idle, "Record" is on screen
/// twice: the bar toggle at the bottom and the sheet's control at the top.
/// The sheet anchors top right, so the higher one is its.
fn sheet_record_button<'h>(harness: &'h Harness<'_>) -> egui_kittest::Node<'h> {
    harness
        .get_all_by_role_and_label(AkRole::Button, "Record")
        .min_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
        .expect("the sheet's Record control")
}

#[test]
fn record_round_trip_sends_the_commands_and_the_lamp_follows() {
    let (rt, mut harness) = record_harness(RecordState::Idle, true, false);
    harness.run_steps(2);
    assert!(harness.query_by_label("REC").is_none());

    harness
        .get_by_role_and_label(AkRole::Button, "Record")
        .click();
    harness.run_steps(2);
    assert!(
        harness
            .query_by_label_contains("Capturing the mix only")
            .is_some(),
        "the toggle must open the sheet"
    );
    // Opening a sheet is navigation; nothing went to the runtime.
    assert!(rt.commands().is_empty());

    sheet_record_button(&harness).click();
    harness.run_steps(2);
    assert_eq!(rt.commands(), vec![Command::StartRecord]);
    // The lamp follows the snapshot, which the demo flips on the command: the
    // cluster lights in the middle of the bar.
    assert!(harness.query_by_label("REC").is_some());

    // Let the sheet settle on its recording layout and the double-click
    // window pass; Stop sits where Record just was.
    harness.run_steps(8);
    harness
        .get_by_role_and_label(AkRole::Button, "Stop")
        .click();
    harness.run_steps(2);
    assert_eq!(
        rt.commands(),
        vec![Command::StartRecord, Command::StopRecord]
    );
    assert!(harness.query_by_label("REC").is_none());
}

#[test]
fn escape_closes_the_record_sheet_and_the_take_keeps_running() {
    let (rt, mut harness) = record_harness(RecordState::Recording, true, true);
    harness.run_steps(2);
    assert!(
        harness
            .query_by_label_contains("Stop ends the take")
            .is_some()
    );
    harness.key_press(Key::Escape);
    harness.run_steps(2);
    assert!(
        harness
            .query_by_label_contains("Stop ends the take")
            .is_none(),
        "Escape must close the sheet"
    );
    // Closing the sheet is navigation: nothing was sent, and the lamp
    // still says the take is running.
    assert!(rt.commands().is_empty());
    assert!(harness.query_by_label("REC").is_some());
}

#[test]
fn an_uploading_take_holds_record_and_reads_as_in_progress() {
    let (rt, mut harness) = record_harness(RecordState::Uploading, true, true);
    harness.run_steps(2);
    // Its own state, not done and not failed: the bar's cluster carries its
    // own word for it and the sheet's state row spells it out.
    assert!(harness.query_by_label("UPLOADING").is_some());
    assert!(harness.query_by_label("uploading").is_some());
    // The sheet's Record control waits for the upload; a click on the
    // disabled button sends nothing.
    sheet_record_button(&harness).click();
    harness.run_steps(2);
    assert!(
        rt.commands().is_empty(),
        "Record must wait for the upload: {:?}",
        rt.commands()
    );
}

#[test]
fn non_hosts_get_no_record_control_but_do_see_the_lamp() {
    let (rt, mut harness) = record_harness(RecordState::Recording, false, false);
    harness.run_steps(2);
    assert!(harness.query_by_label("Record").is_none());
    assert!(harness.query_by_label("REC").is_some());
    assert!(rt.commands().is_empty());
}

#[test]
fn a_failed_take_says_why_on_the_lamp_everyone_sees() {
    let reason = "multipart upload aborted: connection reset by peer";
    let (_, mut harness) = record_harness(
        RecordState::Failed {
            reason: reason.to_owned(),
        },
        false,
        false,
    );
    harness.run_steps(2);
    let lamp = harness.get_by_label("REC FAILED").rect();
    harness.event(Event::PointerMoved(lamp.center()));
    // Past the tooltip delay, the recorder's reason is on the lamp.
    harness.run_steps(20);
    assert!(
        harness.query_by_label(reason).is_some(),
        "the reason must be on the lamp verbatim"
    );
}

#[test]
fn metronome_changes_send_commands() {
    let (rt, mut harness) = session_harness(true);
    harness.run_steps(2);
    harness.get_by_label("hear the click").click();
    harness.run_steps(2);
    assert!(rt.commands().contains(&Command::SetClick(false)));
}

// Avatars. Two invariants and one round trip: an arriving picture may not
// move anything in a strip, the chat message column may not move with the
// name beside it, and the settings sheet must send the file's own bytes.

/// A runtime that replays one fixed snapshot, so a test can hold two
/// snapshots that differ in exactly one field.
struct StaticRuntime(Snapshot);

impl Runtime for StaticRuntime {
    fn snapshot(&self) -> Snapshot {
        self.0.clone()
    }

    fn send(&self, _cmd: Command) {}
}

fn static_harness(snap: Snapshot) -> Harness<'static> {
    let rt = StaticRuntime(snap);
    let mut screen = SessionScreen::default();
    Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            let snap = rt.snapshot();
            screen.ui(ui, &snap, &rt);
        })
}

/// The reserve-space invariant: the strip's slot for an avatar is allocated
/// whether or not one has arrived, so every control in the strip sits at the
/// same place either way. The demo gives Ana a picture; stripping it must
/// move nothing.
#[test]
fn a_strip_lays_out_identically_with_and_without_an_avatar() {
    let with = DemoRuntime::frozen(FROZEN_FRAME, true).snapshot();
    assert!(
        with.members.iter().any(|m| m.avatar.is_some()),
        "the demo must provide at least one avatar"
    );
    let mut without = with.clone();
    for member in &mut without.members {
        member.avatar = None;
    }

    // Every labelled control in the mixer, in both states.
    let probes = [
        "Ana fader",
        "Ana pan",
        "Ben fader",
        "Ben pan",
        "Mira fader",
        "Sam fader",
    ];
    let mut with_rects = Vec::new();
    let mut harness = static_harness(with);
    harness.run_steps(3);
    for probe in probes {
        with_rects.push(harness.get_by_label(probe).rect());
    }
    let mut harness = static_harness(without);
    harness.run_steps(3);
    for (probe, expected) in probes.iter().zip(with_rects) {
        assert_eq!(
            harness.get_by_label(probe).rect(),
            expected,
            "{probe} moved when the avatar went away"
        );
    }
}

/// A device that will not run has to be readable from inside the session: the
/// reason the device gave, and one line saying what it costs and where to fix
/// it, over strips that are all still there. Told, not stopped (#263).
///
/// The mixer used to draw nothing at all for this, so the whole of what a
/// silent musician had was a chat line about the fallback and a log they were
/// not reading mid-song.
#[test]
fn a_refused_device_puts_the_reason_over_the_strips() {
    let reason = "unsupported audio configuration: \
                  wav device runs at 44100 Hz and will not open at 48000 Hz";
    let quiet = DemoRuntime::frozen(FROZEN_FRAME, true).snapshot();
    let mut refused = quiet.clone();
    refused.device_error = Some(reason.to_owned());

    let mut harness = static_harness(refused);
    harness.run_steps(3);
    assert!(
        harness.query_by_label(reason).is_some(),
        "the device's own reason must be on the mixer, verbatim and unprefixed"
    );
    assert!(
        harness
            .query_all_by_label_contains("The Audio tab in Settings")
            .next()
            .is_some(),
        "and what it costs and where to fix it"
    );
    // The session carries on around it: nothing here blocks the mixer.
    assert!(harness.query_by_label("Ana fader").is_some());

    // And it is the state that draws it, not the screen always drawing it.
    let mut harness = static_harness(quiet);
    harness.run_steps(3);
    assert!(
        harness.query_by_label(reason).is_none(),
        "a working device must say nothing"
    );
}

/// The chat alignment invariant: message text starts at one x for every
/// line, whatever the name beside it is. The long-names demo carries a
/// 64-character name and the short scripted ones together.
#[test]
fn chat_messages_share_one_left_edge_whatever_the_name() {
    for size in [vec2(1280.0, 800.0), vec2(800.0, 600.0)] {
        let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::long_names(
            FROZEN_FRAME,
            false,
        )));
        let rt_ui = rt.clone();
        // The narrow layout shows chat instead of the mixer.
        let mut screen = SessionScreen {
            chat_open: true,
            ..Default::default()
        };
        let mut harness = Harness::builder().with_size(size).build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            let snap = rt_ui.snapshot();
            screen.ui(ui, &snap, &*rt_ui);
        });
        harness.run_steps(3);
        let short = harness.get_by_label("tuning up, one minute").rect().left();
        let long = harness
            .get_all_by_label_contains("the monitor mix on my end")
            .next()
            .expect("the long-name chat line")
            .rect()
            .left();
        assert_eq!(
            short, long,
            "message column moved with the name at {size:?}"
        );
        // And it is a column, not the panel edge: the clock and the name
        // gutter sit to its left.
        let clock = harness.get_by_label("00:12").rect().left();
        assert!(clock < short, "the clock must precede the message column");
    }
}

/// A picked file becomes a picture on the row and its bytes on the wire;
/// Remove sends the None variant. The dialog itself cannot be driven from a
/// test, so the pick enters through the same function its thread calls.
#[test]
fn settings_avatar_pick_and_remove_send_the_right_commands() {
    use jamstream_client::app::{JamApp, Screen};

    // Already at drawing size, so the fitter leaves it alone and these are
    // the exact bytes that must reach the runtime.
    let png = png_bytes(6, 4);
    let path = avatar_file("small.png", &png);

    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        false,
    )));
    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.runtime = Some(Box::new(rt.clone()));
    app.screen = Screen::Session;
    app.settings_open = true;
    // The avatar row lives on the You tab now.
    app.settings_tab = SettingsTab::You;
    app.load_avatar_from(&path);
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
        });
    harness.run_steps(3);

    let sent: Vec<Command> = rt
        .commands()
        .into_iter()
        .filter(|c| matches!(c, Command::SetOwnAvatar(_)))
        .collect();
    assert_eq!(
        sent,
        vec![Command::SetOwnAvatar(Some(png.clone()))],
        "a picture already at drawing size travels as the file itself"
    );
    // And the row says what was picked, so nothing is sent blind.
    assert!(harness.query_by_label_contains("small.png").is_some());
    assert!(harness.query_by_label_contains("6x4").is_some());

    harness
        .get_by_role_and_label(AkRole::Button, "Remove")
        .click();
    harness.run_steps(3);
    let sent: Vec<Command> = rt
        .commands()
        .into_iter()
        .filter(|c| matches!(c, Command::SetOwnAvatar(_)))
        .collect();
    assert_eq!(
        sent,
        vec![
            Command::SetOwnAvatar(Some(png)),
            Command::SetOwnAvatar(None)
        ],
        "Remove must send the None variant"
    );

    std::fs::remove_file(&path).ok();
}

/// A photograph is fitted before it is announced: what reaches the runtime
/// is inside the transfer layer's caps, and the row says what happened.
#[test]
fn settings_avatar_sends_the_fitted_photo_not_the_file() {
    use jamstream_client::app::{JamApp, Screen};
    use jamstream_client::avatar;

    let photo = jpeg_bytes(1600, 1200);
    assert!(
        photo.len() > avatar::MAX_BYTES,
        "the fixture must be a file the caps would refuse"
    );
    let path = avatar_file("rehearsal.jpg", &photo);

    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        false,
    )));
    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.runtime = Some(Box::new(rt.clone()));
    app.screen = Screen::Session;
    app.settings_open = true;
    // The avatar row lives on the You tab now.
    app.settings_tab = SettingsTab::You;
    app.load_avatar_from(&path);
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
        });
    harness.run_steps(3);

    let sent = rt
        .commands()
        .into_iter()
        .find_map(|c| match c {
            Command::SetOwnAvatar(Some(bytes)) => Some(bytes),
            _ => None,
        })
        .expect("the picture must be announced");
    assert_ne!(sent, photo, "the file itself is over the byte cap");
    assert!(sent.len() <= avatar::MAX_BYTES);
    assert!(
        harness
            .query_by_label_contains("1600x1200 fitted to 256x256")
            .is_some(),
        "the row must say what happened to the photo"
    );

    std::fs::remove_file(&path).ok();
}

/// A file that cannot be read says so, on the spot, and sends nothing.
#[test]
fn settings_avatar_reports_an_unreadable_file_inline() {
    use jamstream_client::app::{JamApp, Screen};

    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        false,
    )));
    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.runtime = Some(Box::new(rt.clone()));
    app.screen = Screen::Session;
    app.settings_open = true;
    // The avatar row lives on the You tab now.
    app.settings_tab = SettingsTab::You;
    app.load_avatar_from("/nonexistent/not-an-avatar.png");
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
        });
    harness.run_steps(3);
    assert!(
        harness
            .query_by_label_contains("not-an-avatar.png could not be read")
            .is_some(),
        "the specific failure must show inline"
    );
    assert!(
        !rt.commands()
            .iter()
            .any(|c| matches!(c, Command::SetOwnAvatar(_))),
        "a refused file must send nothing"
    );
}

/// A file on disk with a name of our choosing, since the row shows the name.
fn avatar_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("jamstream-avatar-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write the avatar file");
    path
}

/// A real PNG through the image encoder the client already depends on:
/// `w`x`h` of warm diagonal bands.
fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let img = image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([((x + y) * 7) as u8, (y * 5) as u8, 40, 255])
    });
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .expect("encode png");
    buf.into_inner()
}

/// A camera-shaped file: `w`x`h` of JPEG with enough detail that it does not
/// compress down to nothing.
fn jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        let ring = ((x * x + y * y) / 97 % 200) as u8;
        image::Rgb([200 - ring / 2, 90 + ring / 3, 40 + ring / 4])
    });
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 95)
        .encode_image(&img)
        .expect("encode jpeg");
    buf.into_inner()
}

// The host wizard's card. The steps are reached through the real transitions
// with an in-memory credential store and a pinned (empty) environment, so a
// fixture never reads what the developer running it has stored.

/// Server artifacts that could not exist, so the preview step is the one a
/// release build shows: nothing to configure and Launch live.
const FAKE_PINS: jamstream_cloud::PinnedServerArtifacts = jamstream_cloud::PinnedServerArtifacts {
    x86_64: Some(jamstream_cloud::PinnedServerArtifact {
        url: "https://example.invalid/jamstream/jamstreamd-linux-x86_64-musl",
        sha256: "0000000000000000000000000000000000000000000000000000000000000000",
    }),
    aarch64: Some(jamstream_cloud::PinnedServerArtifact {
        url: "https://example.invalid/jamstream/jamstreamd-linux-aarch64-musl",
        sha256: "0000000000000000000000000000000000000000000000000000000000000000",
    }),
};

/// The whole shell on the wizard's cost preview, the tallest card it has.
fn wizard_preview_harness(size: egui::Vec2) -> Harness<'static> {
    wizard_harness(size, jamstream_client::screens::host::WizardStep::Preview)
}

/// The wizard on `step`, reached through the real transitions as far as the
/// preview. Launching is set rather than launched: a fixture that pressed
/// Launch would ask a real provider for a real machine.
fn wizard_harness(
    size: egui::Vec2,
    step: jamstream_client::screens::host::WizardStep,
) -> Harness<'static> {
    use jamstream_client::app::{JamApp, Screen};
    use jamstream_client::screens::host::{RegionRow, WizardStep};
    use jamstream_cloud::{Price, ProviderKind, Region, RegionId};

    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.wizard.providers[1].status = jamstream_client::screens::host::ProviderStatus::Ready;
    app.wizard.select_provider(1);
    app.wizard.continue_to_region(vec![RegionRow {
        region: Region {
            provider: ProviderKind::DigitalOcean,
            id: RegionId::new("nyc3"),
            display: "New York 3".to_owned(),
            country: "US".to_owned(),
        },
        price: Price {
            hourly_microusd: 26_790,
            egress_microusd_per_gb: 10_000,
            included_egress_gb: 3000,
        },
        worst_rtt_ms: 21.0,
    }]);
    app.wizard.continue_to_preview();
    app.wizard.pinned = FAKE_PINS;
    assert_eq!(app.wizard.step, WizardStep::Preview);
    assert!(app.wizard.can_launch(), "the fixture must be launchable");
    app.wizard.step = step;
    app.screen = Screen::HostWizard;
    Harness::builder()
        .with_size(size)
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            let fill = theme::palette(Theme::Dark).surface0;
            egui::CentralPanel::default_margins()
                .frame(
                    egui::Frame::new()
                        .fill(fill)
                        .inner_margin(egui::Margin::same(10)),
                )
                .show(ui, |ui| app.root_ui(ui));
        })
}

/// #179: the step's actions belong to the card's bottom edge, not to the end
/// of its body. At 800x600 the cost preview is taller than the window, and
/// Back and Launch sat past the bottom edge with a 6 px scrollbar as the only
/// cue that the step had a primary action at all.
#[test]
fn the_wizards_actions_are_on_screen_at_every_window_size() {
    for size in SIZES {
        let mut harness = wizard_preview_harness(size);
        harness.run_steps(4);
        for label in ["Back", "Launch"] {
            let node = harness
                .query_by_role_and_label(AkRole::Button, label)
                .unwrap_or_else(|| panic!("{label} is missing at {size:?}"));
            let rect = node.rect();
            assert!(
                rect.top() >= 0.0 && rect.bottom() <= size.y,
                "{label} at {rect:?} is outside the {size:?} window"
            );
            assert!(
                rect.left() >= 0.0 && rect.right() <= size.x,
                "{label} at {rect:?} is outside the {size:?} window"
            );
        }
        // And the step's own top is on screen, rather than pushed down by a
        // lead the window cannot spare.
        let title = harness
            .query_by_label_contains("Step 3 of 4")
            .expect("the step counter")
            .rect();
        assert!(
            title.top() >= 0.0 && title.bottom() <= size.y,
            "the step counter at {title:?} is outside the {size:?} window"
        );
        // Reachable is not the whole claim: this step's body is taller than
        // every one of these windows, so the card has to spend the window on
        // it. A body that collapsed to its floor would satisfy every assertion
        // above and show two rows of a six row step.
        let launch = harness
            .query_by_role_and_label(AkRole::Button, "Launch")
            .expect("Launch")
            .rect();
        let card = launch.bottom() - title.top();
        assert!(
            card >= size.y * 0.6,
            "the card is {card:.0} px of a {size:?} window, so the step is showing a \
             fraction of itself with the rest behind a scrollbar"
        );
    }
}

/// #177: the launching step used to draw a spinner and nothing else until an
/// error arrived, so a reachability check that never passed left quitting the
/// app as the only way out. Stop waiting has to come back to the preview with
/// the note about what may be running out there.
///
/// The step is entered without a job behind it, which is the state a launch
/// nobody can interrupt leaves the wizard in; a fixture that pressed Launch
/// would be asking a real provider for a real machine.
#[test]
fn the_launching_step_has_a_way_back_while_it_is_still_waiting() {
    let mut harness = wizard_harness(
        vec2(1280.0, 800.0),
        jamstream_client::screens::host::WizardStep::Launching,
    );
    harness.run_steps(3);
    assert!(
        harness.query_by_label_contains("Step 4 of 4").is_some(),
        "the fixture must be on the launching step"
    );
    harness
        .get_by_role_and_label(AkRole::Button, "Stop waiting")
        .click();
    harness.run_steps(3);
    assert!(
        harness.query_by_label_contains("Step 3 of 4").is_some(),
        "Stop waiting must come back to the preview"
    );
    assert!(
        harness
            .query_by_label_contains("You stopped waiting")
            .is_some(),
        "and the preview must say a machine may be running"
    );
}

/// The only way in to the takes, and the state the button is not in.
///
/// Home is three cards and stays three cards: Takes hangs off the Recent
/// sessions card, because a take belongs to a session. With no sessions there
/// are no takes, so there is nothing there to press. Driven through
/// `JamApp::root_ui`, so the screen it lands on is the one the app routes to
/// rather than one a test constructed.
#[test]
fn takes_is_reached_from_the_recent_sessions_card_and_only_when_there_are_any() {
    use jamstream_client::app::JamApp;
    use jamstream_client::screens::home::RecentSession;

    let recent = vec![RecentSession {
        short_id: "a3f29c41".to_owned(),
        provider: "digitalocean".to_owned(),
        region: "sfo3".to_owned(),
        status: "ended".to_owned(),
    }];
    for rows in [Vec::new(), recent] {
        let any = !rows.is_empty();
        let mut app = JamApp::in_memory();
        app.recent = rows;
        // The assertions are about the way in and the screen it lands on, not
        // about the rows: entering reads this machine's own session records,
        // and its bucket listings go nowhere, because an in-memory app has no
        // storage key and cannot reach the developer's keychain for one.
        app.takes.rows = Vec::new();
        let mut harness = Harness::builder()
            .with_size(vec2(1280.0, 800.0))
            .with_step_dt(0.05)
            .build_ui(move |ui| {
                theme::apply(ui.ctx(), Theme::Dark);
                app.root_ui(ui);
            });
        harness.run_steps(3);
        assert_eq!(
            harness
                .query_by_role_and_label(AkRole::Button, "Takes")
                .is_some(),
            any,
            "with {} recent sessions the way in must{} be there",
            usize::from(any),
            if any { "" } else { " not" }
        );
        if !any {
            continue;
        }
        harness
            .get_by_role_and_label(AkRole::Button, "Takes")
            .click();
        harness.run_steps(3);
        // The screen's own line, which is on no other screen.
        assert!(
            harness
                .query_by_label_contains("What your sessions recorded")
                .is_some(),
            "Takes must land on the takes screen"
        );
        // And back, through the top bar's Home, which exists on every screen
        // that is not Home or a session.
        harness
            .get_by_role_and_label(AkRole::Button, "Home")
            .click();
        harness.run_steps(3);
        assert!(
            harness.query_by_label_contains("Join a session").is_some(),
            "Home must come back to Home"
        );
    }
}

/// #278: one frame counter could not say which of two opposite things a host
/// was looking at. A repeat is a stutter with nothing missing; a loss is video
/// the broadcast will never have. Both are on screen, both are named, and each
/// says what it means on hover.
///
/// Over a pipeline that has both, which is the shape a struggling machine
/// really has: it runs out of time to draw long before the encoder's queue
/// starts refusing frames.
#[test]
fn the_two_frame_counts_are_named_apart() {
    let demo = DemoRuntime::frozen(FROZEN_FRAME, true);
    demo.set_destinations(&[
        (
            jamstream_client::runtime::StreamPlatform::Twitch,
            DestinationState::Live,
        ),
        (
            jamstream_client::runtime::StreamPlatform::YouTube,
            DestinationState::Failed {
                reason: "pusher exited: rtmp connection refused".to_owned(),
            },
        ),
    ]);
    let rt: Recorder = Arc::new(RecordingRuntime::new(demo));
    let mut harness = drawer_harness(rt, SettingsTab::Broadcast, vec2(1280.0, 800.0));
    harness.run_steps(3);

    // Two readouts, each with its own count: 41 draws missed, 3 frames the
    // encoder would not take.
    assert!(
        harness.query_by_label("41 repeated").is_some(),
        "the repeats have to be on screen under the encode line"
    );
    assert!(
        harness.query_by_label("3 dropped").is_some(),
        "and the losses have to be a separate figure"
    );
    // Neither may still be the merged number: 44 would be the old single
    // count, and either word carrying the other total is the defect.
    for wrong in ["44 repeated", "44 dropped", "3 repeated", "41 dropped"] {
        assert!(
            harness.query_by_label(wrong).is_none(),
            "{wrong} is on screen, so the two counts are crossed"
        );
    }
}

/// Rescan keeps the selection by device id and falls back visibly. The
/// enumerator is injected, because a test has no interface to unplug; what is
/// under test is everything from the button to the note: the click reaches
/// the app, the selection survives by id, and a device the new catalog no
/// longer holds lands on System default with a sentence saying so (#325).
#[test]
fn rescan_keeps_the_selection_by_id_and_a_lost_device_falls_back_visibly() {
    use jamstream_client::app::{JamApp, Screen};
    use jamstream_client::screens::devices::{DeviceCatalog, DeviceInfo};

    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.runtime = Some(Box::new(DemoRuntime::frozen(FROZEN_FRAME, false)));
    app.screen = Screen::Session;
    app.settings_open = true;
    // The user picked the Scarlett explicitly; index 0 is System default.
    app.devices.capture_idx = 1;
    app.devices.playback_idx = 2;
    // The next scan finds the speakers but not the Scarlett.
    app.enumerate = Arc::new(|| {
        let mut catalog = DeviceCatalog::demo();
        catalog.capture.retain(|d| !d.name.contains("Scarlett"));
        catalog.playback.push(DeviceInfo {
            name: "USB DAC".to_owned(),
            id: Some("demo:USB DAC".to_owned()),
        });
        Ok(catalog)
    });
    // The picker indexes, read back out of the app after each frame: the
    // combo's selected text is painted, not a queryable node.
    let picks = Arc::new(std::sync::Mutex::new((0usize, 0usize)));
    let picks_ui = Arc::clone(&picks);
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
            *picks_ui.lock().unwrap() = (app.devices.capture_idx, app.devices.playback_idx);
        });
    harness.run_steps(3);

    harness
        .get_by_role_and_label(AkRole::Button, "Rescan")
        .click();
    harness.run_steps(3);

    // The lost capture device fell back to System default and said so; the
    // playback selection survived the reorder because it is matched by id.
    assert!(
        harness
            .query_by_label_contains("Scarlett 2i2 input is no longer present")
            .is_some(),
        "a rescan that loses the selected device has to say so under the pickers"
    );
    let (capture_idx, playback_idx) = *picks.lock().unwrap();
    assert_eq!(
        capture_idx, 0,
        "the lost capture pick lands on System default"
    );
    assert_eq!(
        playback_idx, 2,
        "the playback pick has to survive a rescan that kept its device"
    );
}
