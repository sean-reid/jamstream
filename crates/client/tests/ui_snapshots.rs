//! Renders every screen and key state, compares against the committed
//! baselines in tests/snapshots/, and drops a human-reviewable copy of each
//! render under target/ui-previews/.

use std::path::PathBuf;
use std::sync::Arc;

use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use jamstream_client::demo::{DemoRuntime, FROZEN_FRAME};
use jamstream_client::runtime::{LevelsView, Runtime};
use jamstream_client::screens::devices::{DeviceCatalog, DevicesScreen};
use jamstream_client::screens::home::{HomeScreen, RecentSession};
use jamstream_client::screens::host::{HostWizard, LaunchOutcome, ProviderRow, RegionRow};
use jamstream_client::screens::session::SessionScreen;
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

/// Mirrors `JamApp::ui`: full-bleed surface0 background, 10 px margin.
fn themed_ui<'a>(
    size: egui::Vec2,
    theme: Theme,
    mut body: impl FnMut(&mut egui::Ui) + 'a,
) -> Harness<'a> {
    Harness::builder().with_size(size).build_ui(move |ui| {
        theme::apply(ui.ctx(), theme);
        let fill = theme::palette(theme).surface0;
        egui::CentralPanel::default_margins()
            .frame(
                egui::Frame::new()
                    .fill(fill)
                    .inner_margin(egui::Margin::same(10)),
            )
            .show(ui, |ui| body(ui));
    })
}

fn sample_recent() -> Vec<RecentSession> {
    vec![RecentSession {
        short_id: "deadbeef".to_owned(),
        provider: "mock".to_owned(),
        region: "mock-east".to_owned(),
        status: "running".to_owned(),
    }]
}

#[test]
fn home_empty() {
    let mut screen = HomeScreen::default();
    let mut harness = themed_ui(WIDE, Theme::Dark, move |ui| {
        screen.ui(ui, &[]);
    });
    snapshot(&mut harness, "home_empty");
}

#[test]
fn home_invalid_invite() {
    let mut screen = HomeScreen {
        invite_text: "jamstream://join/not-a-real-invite".to_owned(),
        error: None,
    };
    let recent = sample_recent();
    let mut harness = themed_ui(WIDE, Theme::Dark, move |ui| {
        screen.ui(ui, &recent);
    });
    harness.run_steps(2);
    harness.get_by_label("Join").click();
    snapshot(&mut harness, "home_invalid_invite");
}

#[test]
fn home_light() {
    let mut screen = HomeScreen::default();
    let recent = sample_recent();
    let mut harness = themed_ui(WIDE, Theme::Light, move |ui| {
        screen.ui(ui, &recent);
    });
    snapshot(&mut harness, "home_light");
}

#[test]
fn devices() {
    let mut screen = DevicesScreen::default();
    let catalog = DeviceCatalog::demo();
    // Fixed levels so the input meter shows a realistic reading.
    let levels = LevelsView {
        input_peak: 0.4,
        input_rms: 0.22,
        output_peak: 0.0,
        output_rms: 0.0,
    };
    let mut harness = themed_ui(WIDE, Theme::Dark, move |ui| {
        screen.ui(ui, &catalog, &levels);
    });
    snapshot(&mut harness, "devices");
}

fn session_harness(size: egui::Vec2, theme: Theme, is_host: bool) -> Harness<'static> {
    let rt = Arc::new(DemoRuntime::frozen(FROZEN_FRAME, is_host));
    let mut screen = SessionScreen::default();
    themed_ui(size, theme, move |ui| {
        let snap = rt.snapshot();
        screen.ui(ui, &snap, &*rt);
    })
}

#[test]
fn session_demo() {
    let mut harness = session_harness(WIDE, Theme::Dark, false);
    snapshot(&mut harness, "session_demo");
}

#[test]
fn session_host() {
    let mut harness = session_harness(WIDE, Theme::Dark, true);
    snapshot(&mut harness, "session_host");
}

#[test]
fn session_light() {
    let mut harness = session_harness(WIDE, Theme::Light, false);
    snapshot(&mut harness, "session_light");
}

#[test]
fn session_narrow() {
    // Below 900 px the chat collapses behind a toggle; nothing may overlap.
    let mut harness = session_harness(NARROW, Theme::Dark, false);
    snapshot(&mut harness, "session_narrow");
}

#[test]
fn session_narrow_chat() {
    let mut harness = session_harness(NARROW, Theme::Dark, false);
    harness.run_steps(2);
    harness.get_by_label("Show chat").click();
    snapshot(&mut harness, "session_narrow_chat");
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
        session_short: "deadbeef".to_owned(),
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
        state_path: Some("/home/you/.local/share/jamstream/sessions/deadbeef.json".to_owned()),
        error: None,
    });
    w
}

fn wizard_snapshot(step: &'static str, name: &str) {
    let mut wizard = wizard_at(step);
    let mut harness = themed_ui(WIDE, Theme::Dark, move |ui| {
        wizard.ui(ui);
    });
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
