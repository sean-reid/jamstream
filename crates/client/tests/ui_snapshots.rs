//! Renders every screen and key state through the full application shell
//! (top bar plus screen content, driven by `JamApp::root_ui`), compares
//! against the committed baselines in tests/snapshots/, and drops a
//! human-reviewable copy of each render under target/ui-previews/.

use std::path::PathBuf;

use egui::vec2;
use egui_kittest::Harness;
use jamstream_client::app::{JamApp, Screen};
use jamstream_client::demo::{DemoRuntime, FROZEN_FRAME};
use jamstream_client::screens::home::RecentSession;
use jamstream_client::screens::host::{HostWizard, LaunchOutcome, ProviderRow, RegionRow};
use jamstream_client::theme::{self, Theme};
use jamstream_cloud::{Price, ProviderKind, Region, RegionId};

const WIDE: egui::Vec2 = vec2(1280.0, 800.0);
const NARROW: egui::Vec2 = vec2(800.0, 600.0);

fn preview_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"))
        .join("ui-previews")
}

/// Runs a fixed number of steps (animated widgets never settle), writes the
/// preview copy, then asserts against the baseline.
fn snapshot(harness: &mut Harness<'_>, name: &str) {
    harness.run_steps(4);
    let image = harness.render().expect("harness render");
    let dir = preview_dir();
    std::fs::create_dir_all(&dir).expect("create preview dir");
    let path = dir.join(format!("{name}.png"));
    image.save(&path).expect("write preview png");
    println!("ui preview: {}", path.display());
    egui_kittest::image_snapshot(&image, name);
}

/// The real application shell, exactly as `JamApp::ui` runs it: top bar,
/// screen routing, settings sheet, full-bleed surface0 with 10 px margin.
fn app_harness(mut app: JamApp, size: egui::Vec2) -> Harness<'static> {
    let theme = app.theme;
    // 2x: pixel-true to a retina display; layout stays in points.
    Harness::builder()
        .with_size(size)
        .with_pixels_per_point(2.0)
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), theme);
            let fill = theme::palette(theme).surface0;
            egui::CentralPanel::default_margins()
                .frame(
                    egui::Frame::new()
                        .fill(fill)
                        .inner_margin(egui::Margin::same(10)),
                )
                .show(ui, |ui| app.root_ui(ui));
        })
}

/// A JamApp with environment-dependent state pinned for reproducibility.
fn test_app(theme: Theme) -> JamApp {
    let mut app = JamApp::new();
    app.theme = theme;
    app.recent = Vec::new();
    app
}

fn sample_recent() -> Vec<RecentSession> {
    vec![RecentSession {
        short_id: "a3f29c41".to_owned(),
        provider: "mock".to_owned(),
        region: "mock-east".to_owned(),
        status: "running".to_owned(),
    }]
}

fn session_app(rt: DemoRuntime, theme: Theme) -> JamApp {
    let mut app = test_app(theme);
    app.runtime = Some(Box::new(rt));
    app.screen = Screen::Session;
    app
}

#[test]
fn home_empty() {
    let mut harness = app_harness(test_app(Theme::Dark), WIDE);
    snapshot(&mut harness, "home_empty");
}

#[test]
fn home_invalid_invite() {
    // The exact error a failed Join produces; clicking itself is covered
    // by the interaction tests.
    let bad = "jamstream://join/not-a-real-invite";
    let err = jamstream_protocol::invite::Invite::decode(bad)
        .expect_err("invite must not decode")
        .to_string();
    let mut app = test_app(Theme::Dark);
    app.recent = sample_recent();
    app.home.invite_text = bad.to_owned();
    app.home.error = Some(err);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "home_invalid_invite");
}

#[test]
fn home_light() {
    let mut app = test_app(Theme::Light);
    app.recent = sample_recent();
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "home_light");
}

#[test]
fn devices() {
    // The frozen demo feeds the input meter a deterministic mid reading.
    let mut app = test_app(Theme::Dark);
    app.runtime = Some(Box::new(DemoRuntime::frozen(FROZEN_FRAME, false)));
    app.screen = Screen::Devices;
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "devices");
}

#[test]
fn session_demo() {
    let app = session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Dark);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_demo");
}

#[test]
fn session_host() {
    let app = session_app(DemoRuntime::frozen(FROZEN_FRAME, true), Theme::Dark);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_host");
}

#[test]
fn session_light() {
    let app = session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Light);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_light");
}

#[test]
fn session_narrow() {
    // Below 900 px the chat collapses behind a toggle; nothing may overlap.
    let app = session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Dark);
    let mut harness = app_harness(app, NARROW);
    snapshot(&mut harness, "session_narrow");
}

#[test]
fn session_narrow_chat() {
    // The toggle stays put and shows its active fill; one click back.
    // Toggling by click is covered by the interaction tests.
    let mut app = session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Dark);
    app.session.chat_open = true;
    let mut harness = app_harness(app, NARROW);
    snapshot(&mut harness, "session_narrow_chat");
}

#[test]
fn session_full() {
    // The design maximum: 10 musicians, 10 listeners. The strip row
    // scrolls sideways like a console; the listener line stays one line.
    let app = session_app(DemoRuntime::full(FROZEN_FRAME, false, true), Theme::Dark);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_full");
}

#[test]
fn session_full_narrow() {
    let app = session_app(DemoRuntime::full(FROZEN_FRAME, false, true), Theme::Dark);
    let mut harness = app_harness(app, NARROW);
    snapshot(&mut harness, "session_full_narrow");
}

#[test]
fn session_long_names() {
    // Names at the 64-char cap truncate inside fixed strips; long chat
    // lines wrap without pushing the input away.
    let app = session_app(DemoRuntime::long_names(FROZEN_FRAME, false), Theme::Dark);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_long_names");
}

#[test]
fn session_settings() {
    // The settings sheet anchors top right and must leave the strips and
    // the status readout visible.
    let mut app = session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Dark);
    app.settings_open = true;
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings");
}

// Wizard states are constructed through the real transitions with fixed
// rows, so snapshots stay independent of environment credentials.

fn fixed_providers() -> Vec<ProviderRow> {
    vec![
        ProviderRow {
            name: "mock".to_owned(),
            available: true,
            detail: "runs locally, no credentials needed".to_owned(),
        },
        ProviderRow {
            name: "aws".to_owned(),
            available: false,
            detail: "provider aws: AWS_ACCESS_KEY_ID is not set".to_owned(),
        },
        ProviderRow {
            name: "digitalocean".to_owned(),
            available: false,
            detail: "provider digitalocean: DIGITALOCEAN_TOKEN is not set".to_owned(),
        },
        ProviderRow {
            name: "gcp".to_owned(),
            available: false,
            detail: "provider gcp: GOOGLE_APPLICATION_CREDENTIALS is not set".to_owned(),
        },
    ]
}

fn fixed_regions() -> Vec<RegionRow> {
    let row = |id: &str, hourly: u64, rtt: f32| RegionRow {
        region: Region {
            provider: ProviderKind::Aws,
            id: RegionId::new(id),
            display: id.to_owned(),
            country: "US".to_owned(),
        },
        price: Price {
            hourly_microusd: hourly,
            egress_microusd_per_gb: 90_000,
            included_egress_gb: 0,
        },
        worst_rtt_ms: rtt,
        fabricated: true,
    };
    vec![
        row("mock-east", 16_800, 21.0),
        row("mock-west", 12_000, 34.0),
    ]
}

fn wizard_at(step: &str) -> HostWizard {
    let mut w = HostWizard::new(fixed_providers());
    if step == "provider" {
        return w;
    }
    w.select_provider(0);
    w.continue_to_region(fixed_regions());
    if step == "region" {
        return w;
    }
    w.continue_to_preview();
    if step == "preview" {
        return w;
    }
    w.begin_launch();
    if step == "launching" {
        return w;
    }
    w.finish_launch(LaunchOutcome {
        session_short: "a3f29c41".to_owned(),
        server_addr: "203.0.113.10:43210".to_owned(),
        invites: vec![
            (
                "host".to_owned(),
                "jamstream://join/AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJ".to_owned(),
            ),
            (
                "musician 1".to_owned(),
                "jamstream://join/KKKKLLLLMMMMNNNNOOOOPPPPQQQQRRRRSSSSTTTT".to_owned(),
            ),
            (
                "listener 4".to_owned(),
                "jamstream://join/UUUUVVVVWWWWXXXXYYYYZZZZ0000111122223333".to_owned(),
            ),
        ],
        state_path: Some("/home/you/.local/share/jamstream/sessions/a3f29c41.json".to_owned()),
        error: None,
    });
    w
}

fn wizard_snapshot(step: &'static str, name: &str) {
    let mut app = test_app(Theme::Dark);
    app.wizard = wizard_at(step);
    app.screen = Screen::HostWizard;
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, name);
}

#[test]
fn wizard_provider() {
    wizard_snapshot("provider", "wizard_provider");
}

#[test]
fn wizard_region() {
    wizard_snapshot("region", "wizard_region");
}

#[test]
fn wizard_preview() {
    wizard_snapshot("preview", "wizard_preview");
}

#[test]
fn wizard_launching() {
    wizard_snapshot("launching", "wizard_launching");
}

#[test]
fn wizard_done() {
    wizard_snapshot("done", "wizard_done");
}
