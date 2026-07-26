//! Interaction tests: what the user does must become the right Command on
//! the runtime. A RecordingRuntime wraps the frozen demo and logs.

use std::sync::Arc;

use egui::accesskit::Role as AkRole;
use egui::{Event, Key, Modifiers, PointerButton, vec2};
use egui_kittest::{Harness, kittest::Queryable};
use jamstream_client::demo::{DemoRuntime, FROZEN_FRAME, RecordingRuntime};
use jamstream_client::runtime::{Command, MemberId, Runtime};
use jamstream_client::screens::session::SessionScreen;
use jamstream_client::theme::{self, Theme};

type Recorder = Arc<RecordingRuntime<DemoRuntime>>;

fn session_harness(is_host: bool) -> (Recorder, Harness<'static>) {
    let rt: Recorder = Arc::new(RecordingRuntime::new(DemoRuntime::frozen(
        FROZEN_FRAME,
        is_host,
    )));
    let rt_ui = rt.clone();
    let mut screen = SessionScreen::default();
    let harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
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
fn metronome_changes_send_commands() {
    let (rt, mut harness) = session_harness(true);
    harness.run_steps(2);
    harness.get_by_label("hear the click").click();
    harness.run_steps(2);
    assert!(rt.commands().contains(&Command::SetClick(false)));
}
