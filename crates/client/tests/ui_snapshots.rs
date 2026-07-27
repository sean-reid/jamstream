//! Renders every screen and key state through the full application shell
//! (top bar plus screen content, driven by `JamApp::root_ui`), compares
//! against the committed baselines in tests/snapshots/, and drops a
//! human-reviewable copy of each render under target/ui-previews/.

use std::path::PathBuf;
use std::sync::Arc;

use egui::vec2;
use egui_kittest::Harness;
use jamstream_client::app::{JamApp, Screen};
use jamstream_client::creds::{EnvReader, MemStore};
use jamstream_client::demo::{DemoRuntime, FROZEN_FRAME};
use jamstream_client::exec::Executor;
use jamstream_client::screens::home::RecentSession;
use jamstream_client::screens::host::{HostWizard, ProviderStatus, RegionRow};
use jamstream_client::screens::invites::InvitesPanel;
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

// Wizard states are constructed through the real transitions with a
// MemStore and a pinned (empty) environment, so snapshots stay independent
// of the machine's credentials; region rows are fixture data fed through
// the same pure transition the probe job uses.

fn fixed_wizard() -> HostWizard {
    let env: EnvReader = Arc::new(|_| None);
    HostWizard::new(
        Arc::new(MemStore::default()),
        env,
        Arc::new(Executor::new()),
    )
}

fn fixed_regions() -> Vec<RegionRow> {
    let row = |id: &str, display: &str, hourly: u64, rtt: f32| RegionRow {
        region: Region {
            provider: ProviderKind::DigitalOcean,
            id: RegionId::new(id),
            display: display.to_owned(),
            country: "US".to_owned(),
        },
        price: Price {
            hourly_microusd: hourly,
            egress_microusd_per_gb: 0,
            included_egress_gb: 3000,
        },
        worst_rtt_ms: rtt,
    };
    vec![
        row("nyc3", "New York 3", 26_790, 21.0),
        row("sfo3", "San Francisco 3", 26_790, 74.0),
    ]
}

/// Provider step with all three statuses on screen: local needs no
/// account, DigitalOcean reads ready, the rest need setup.
fn wizard_provider_app(theme: Theme) -> JamApp {
    let mut app = test_app(theme);
    let mut w = fixed_wizard();
    w.providers[1].status = ProviderStatus::Ready;
    w.select_provider(1);
    app.wizard = w;
    app.screen = Screen::HostWizard;
    app
}

fn wizard_region_app(theme: Theme) -> JamApp {
    let mut app = test_app(theme);
    let mut w = fixed_wizard();
    w.providers[1].status = ProviderStatus::Ready;
    w.select_provider(1);
    w.continue_to_region(fixed_regions());
    app.wizard = w;
    app.screen = Screen::HostWizard;
    app
}

/// The DigitalOcean setup pane, open inline with a masked token typed.
fn wizard_setup_app(theme: Theme) -> JamApp {
    let mut app = test_app(theme);
    let mut w = fixed_wizard();
    w.select_provider(1);
    w.setup.do_token = "dop_v1_0000000000000000".to_owned();
    app.wizard = w;
    app.screen = Screen::HostWizard;
    app
}

fn wizard_preview_app(theme: Theme) -> JamApp {
    let mut app = test_app(theme);
    let mut w = fixed_wizard();
    w.providers[1].status = ProviderStatus::Ready;
    w.select_provider(1);
    w.continue_to_region(fixed_regions());
    w.continue_to_preview();
    // The development-build state, pinned explicitly: the advanced
    // artifact fields are only rendered when no server artifact is pinned
    // into the binary. The pinned state renders a version string instead,
    // which would rot the baseline on every release bump, so it is covered
    // by the pure-transition tests rather than a snapshot.
    w.pinned = None;
    w.advanced_open = true;
    app.wizard = w;
    app.screen = Screen::HostWizard;
    app
}

/// Launch progress: the model enters Launching without spawning a job, so
/// the phase readout is frozen on the first phase.
fn wizard_launching_app(theme: Theme) -> JamApp {
    let mut app = test_app(theme);
    let mut w = fixed_wizard();
    w.select_provider(0); // local
    w.advance_from_provider();
    w.step = jamstream_client::screens::host::WizardStep::Launching;
    app.wizard = w;
    app.screen = Screen::HostWizard;
    app
}

fn wizard_snapshot(app: JamApp, name: &str) {
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, name);
}

#[test]
fn wizard_provider() {
    wizard_snapshot(wizard_provider_app(Theme::Dark), "wizard_provider");
}

#[test]
fn wizard_provider_light() {
    wizard_snapshot(wizard_provider_app(Theme::Light), "wizard_provider_light");
}

#[test]
fn wizard_setup_digitalocean() {
    wizard_snapshot(wizard_setup_app(Theme::Dark), "wizard_setup_digitalocean");
}

#[test]
fn wizard_setup_digitalocean_light() {
    wizard_snapshot(
        wizard_setup_app(Theme::Light),
        "wizard_setup_digitalocean_light",
    );
}

#[test]
fn wizard_region() {
    wizard_snapshot(wizard_region_app(Theme::Dark), "wizard_region");
}

#[test]
fn wizard_region_light() {
    wizard_snapshot(wizard_region_app(Theme::Light), "wizard_region_light");
}

#[test]
fn wizard_preview() {
    wizard_snapshot(wizard_preview_app(Theme::Dark), "wizard_preview");
}

#[test]
fn wizard_launching() {
    wizard_snapshot(wizard_launching_app(Theme::Dark), "wizard_launching");
}

#[test]
fn wizard_launching_light() {
    wizard_snapshot(wizard_launching_app(Theme::Light), "wizard_launching_light");
}

// The invites panel over the host session, fed from a real decodable state
// record so labels and statuses come through the production path.

fn invites_state() -> (jamstream_cli::state::SessionState, PathBuf) {
    use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
    use jamstream_protocol::invite::{Issuer, Token};

    let issuer = Issuer::from_bytes(&[7u8; 32]);
    let server_pk = [9u8; 32];
    let session_id = SessionId([0xa3; 16]);
    let address: std::net::SocketAddr = "203.0.113.10:43210".parse().expect("addr");
    let mint = |member: u16, role: Role, label: &str| {
        let token = Token {
            member_id: MemberId(member),
            role,
            name_hint: None,
            expires_unix: 4_000_000_000,
            jti: TokenId([member as u8 + 1; 16]),
        };
        jamstream_cli::state::InviteRecord {
            role: label.to_owned(),
            invite: issuer
                .mint(session_id, vec![address], server_pk, token)
                .encode(),
        }
    };
    let state = jamstream_cli::state::SessionState {
        session_id_hex: "a3".repeat(16),
        provider: "local".to_owned(),
        region: "local".to_owned(),
        instance_id: "12345".to_owned(),
        address: address.to_string(),
        created_unix: 1_784_000_000,
        hourly_microusd: 0,
        issuer_private_key_b64: String::new(),
        server_public_key_b64: String::new(),
        invites: vec![
            mint(0, Role::Musician, "host"),
            mint(1, Role::Musician, "musician 1"),
            mint(2, Role::Musician, "musician 2"),
            mint(3, Role::Musician, "musician 3"),
            mint(4, Role::Listener, "listener 4"),
            mint(5, Role::Listener, "listener 5"),
        ],
        status: jamstream_cli::state::SessionStatus::Running,
        ended_unix: None,
    };
    let path = std::env::temp_dir().join("jamstream-snapshot-invites.json");
    (state, path)
}

fn session_invites_app(theme: Theme) -> JamApp {
    let mut app = session_app(DemoRuntime::frozen(FROZEN_FRAME, true), theme);
    let (state, path) = invites_state();
    let mut panel = InvitesPanel::new(state, path);
    // One revoked row so all three statuses render: the demo roster has
    // members 1..4 connected and member 5 absent.
    let revoked = panel.token_map()[&jamstream_protocol::ids::MemberId(2)];
    panel.mark_revoked(revoked);
    app.session.invites = Some(panel);
    app.session.invites_open = true;
    app
}

#[test]
fn session_invites() {
    let mut harness = app_harness(session_invites_app(Theme::Dark), WIDE);
    snapshot(&mut harness, "session_invites");
}

#[test]
fn session_invites_light() {
    let mut harness = app_harness(session_invites_app(Theme::Light), WIDE);
    snapshot(&mut harness, "session_invites_light");
}
