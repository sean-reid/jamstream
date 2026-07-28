//! Interaction tests: what the user does must become the right Command on
//! the runtime. A RecordingRuntime wraps the frozen demo and logs.

use std::sync::Arc;

use egui::accesskit::Role as AkRole;
use egui::{Event, Key, Modifiers, PointerButton, vec2};
use egui_kittest::{Harness, kittest::Queryable};
use jamstream_client::creds::MemStore;
use jamstream_client::demo::{DemoRuntime, FROZEN_FRAME, RecordingRuntime};
use jamstream_client::runtime::{Command, DestinationState, MemberId, Runtime, Snapshot};
use jamstream_client::screens::destinations::DestinationsPanel;
use jamstream_client::screens::session::SessionScreen;
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
/// sheet, and the broadcast view the runtime gives a host. All three status
/// bar toggles come from those, so this is the widest the bar ever gets.
fn host_harness_sized(size: egui::Vec2) -> Harness<'static> {
    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        true,
    )));
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

/// An invite book with no invites in it: enough for the toggle to render,
/// which is all these layout tests need from it.
fn empty_invites() -> jamstream_client::screens::invites::InvitesPanel {
    let state = jamstream_cli::state::SessionState {
        session_id_hex: "a3".repeat(16),
        provider: "local".to_owned(),
        region: "local".to_owned(),
        instance_id: "12345".to_owned(),
        address: "203.0.113.10:43210".to_owned(),
        created_unix: 1_784_000_000,
        hourly_microusd: 0,
        issuer_private_key_b64: String::new(),
        server_public_key_b64: String::new(),
        invites: Vec::new(),
        status: jamstream_cli::state::SessionStatus::Running,
        ended_unix: None,
    };
    let path = std::env::temp_dir().join("jamstream-interaction-invites.json");
    jamstream_client::screens::invites::InvitesPanel::new(state, path)
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

/// The bar may never overlap itself. A host's carries three sheet toggles,
/// the lamp, the session id, the timer, the cost, and Leave beside the
/// readouts, and the readouts drop their meters or move to a row of their
/// own to keep clear of them.
#[test]
fn the_host_status_bar_never_runs_into_its_readouts() {
    for size in SIZES {
        let mut harness = host_harness_sized(size);
        harness.run_steps(3);
        let readout = harness
            .get_all_by_label_contains("loss ")
            .next()
            .expect("the loss readout")
            .rect();
        for control in ["Stream mix", "Destinations", "Invites", "Leave"] {
            let rect = harness.get_by_label(control).rect();
            let clear = rect.left() >= readout.right() || rect.top() >= readout.bottom();
            assert!(
                clear,
                "{control} at {rect:?} runs into the readouts at {readout:?}, window {size:?}"
            );
        }
    }
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

/// The whole shell with the settings drawer open over a host session, which
/// is the tallest sheet in the app over the shortest status bar it can sit
/// above. `in_memory` for the same reason every fixture uses it: the real
/// keychain would put a system dialog in front of the test run.
fn settings_harness_sized(size: egui::Vec2) -> Harness<'static> {
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
    app.settings_open = true;
    Harness::builder()
        .with_size(size)
        .with_step_dt(0.05)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.root_ui(ui);
        })
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
            .get_all_by_label_contains("loss ")
            .next()
            .expect("the loss readout")
            .rect();
        // What a musician reaches for mid session is on screen with nothing
        // scrolled, along with the way out of the sheet.
        for label in [
            "Close",
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
        // And the last thing in the sheet, which a short window has no room
        // for, comes into view when the body is scrolled. A sheet with no
        // height of its own has no body to scroll: it grows to its content,
        // past the bottom edge, and this is what that leaves unreachable.
        harness.event(Event::PointerMoved(egui::pos2(size.x - 100.0, 200.0)));
        harness.run_steps(1);
        harness.event(Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, -1000.0),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::NONE,
        });
        harness.run_steps(3);
        let last = harness
            .get_all_by_label_contains("light")
            .next()
            .expect("the theme picker is the last thing in the sheet")
            .rect();
        assert!(
            last.top() >= 0.0 && last.bottom() <= readouts.top(),
            "the end of the sheet is at {last:?}, out of reach above the readouts at \
             {readouts:?}, window {size:?}"
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
fn stream_mix_panel_opens_and_fader_sends_exact_values() {
    let (rt, mut harness) = session_harness(true);
    harness.run_steps(2);
    assert!(
        harness.query_by_label("Ana stream gain").is_none(),
        "the stream mix sheet must start closed"
    );

    harness
        .get_by_role_and_label(AkRole::Button, "Stream mix")
        .click();
    harness.run_steps(2);
    // Focus the gain control, then one 0.5 dB arrow step from Ana's
    // demo broadcast fader (-2.0 dB, pan -0.3, unmuted).
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

    // The same stationary toggle closes the sheet again.
    harness
        .get_by_role_and_label(AkRole::Button, "Stream mix")
        .click();
    harness.run_steps(2);
    assert!(harness.query_by_label("Ana stream gain").is_none());
}

#[test]
fn audition_round_trip_and_escape_leaves_it_on() {
    let (rt, mut harness) = session_harness(true);
    harness.run_steps(2);
    harness
        .get_by_role_and_label(AkRole::Button, "Stream mix")
        .click();
    harness.run_steps(2);

    harness.get_by_label("audition stream mix").click();
    harness.run_steps(2);
    assert_eq!(audition_commands(&rt), vec![true]);
    assert!(
        harness.query_by_label("hearing stream mix").is_some(),
        "the status bar must show the audition reminder"
    );

    // Escape closes the sheet; audition is a mix state, not navigation,
    // so it stays on and the reminder stays visible.
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
        harness.query_by_label("hearing stream mix").is_some(),
        "closing the sheet must not hide the audition reminder"
    );

    // Reopen and switch it off.
    harness
        .get_by_role_and_label(AkRole::Button, "Stream mix")
        .click();
    harness.run_steps(2);
    harness.get_by_label("audition stream mix").click();
    harness.run_steps(2);
    assert_eq!(audition_commands(&rt), vec![true, false]);
    assert!(harness.query_by_label("hearing stream mix").is_none());
}

#[test]
fn non_hosts_see_no_stream_mix() {
    let (rt, mut harness) = session_harness(false);
    harness.run_steps(2);
    assert!(harness.query_by_label("Stream mix").is_none());
    assert!(harness.query_by_label("hearing stream mix").is_none());
    assert!(rt.snapshot().broadcast.is_none());
}

// Destinations. The key path is the one that has to be exactly right: what
// the host pastes is what the server gets, once, and nothing else keeps it.

/// A key nothing could stream with.
const FAKE_KEY: &str = "live_000000_fakefakefake";

fn destinations_harness(
    reported: &[(jamstream_client::runtime::StreamPlatform, DestinationState)],
) -> (Recorder, Harness<'static>) {
    let demo = DemoRuntime::frozen(FROZEN_FRAME, true);
    demo.set_destinations(reported);
    let rt: Recorder = Arc::new(RecordingRuntime::new(demo));
    let rt_ui = rt.clone();
    let mut screen = SessionScreen {
        destinations: Some(DestinationsPanel::new(Arc::new(MemStore::default()))),
        destinations_open: true,
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
    // The demo's stand-in server brings it up, so the whole room is on air.
    assert!(rt.snapshot().stream.on_air());
    assert!(harness.query_by_label("2 live").is_none());
    assert!(harness.query_by_label("1 live").is_some());
}

#[test]
fn removing_one_live_destination_leaves_the_other_alone() {
    use jamstream_client::runtime::{DestinationId, StreamPlatform};
    let (rt, mut harness) = destinations_harness(&[
        (StreamPlatform::Twitch, DestinationState::Live),
        (StreamPlatform::YouTube, DestinationState::Live),
    ]);
    harness.run_steps(2);
    assert!(harness.query_by_label("2 live").is_some());

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
    harness
        .get_by_role_and_label(AkRole::Button, "Stop streaming")
        .click();
    harness.run_steps(2);
    assert_eq!(destination_commands(&rt), vec![Command::StopStream]);
    assert!(!rt.snapshot().stream.on_air());
}

#[test]
fn escape_closes_the_destinations_sheet_without_leaving_the_air() {
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
        "Escape must close the sheet"
    );
    // Closing a sheet is navigation: nothing was sent and the room is still
    // on air, with the lamp and the count to say so.
    assert!(destination_commands(&rt).is_empty());
    assert!(harness.query_by_label("1 live").is_some());
    assert!(harness.query_by_label("on air").is_some());
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
    // And with the sheet closed, the status bar still says one died.
    harness.key_press(Key::Escape);
    harness.run_steps(2);
    assert!(harness.query_by_label("1 failed").is_some());
    assert!(harness.query_by_label("1 live").is_some());
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
    assert!(harness.query_by_label("Destinations").is_none());
    assert!(harness.query_by_label("Go live").is_none());
    assert!(harness.query_by_label("1 live").is_some());
    assert!(harness.query_by_label("on air").is_some());
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
