//! Renders every screen and key state through the full application shell
//! (top bar plus screen content, driven by `JamApp::root_ui`), compares
//! against the committed baselines in tests/snapshots/, and drops a
//! human-reviewable copy of each render under target/ui-previews/.

use std::path::PathBuf;
use std::sync::Arc;

use egui::vec2;
use egui_kittest::Harness;
use jamstream_client::app::{JamApp, Screen};
use jamstream_client::creds::{self, CredStore, EnvReader, MemStore};
use jamstream_client::demo::{DemoRuntime, FROZEN_FRAME};
use jamstream_client::exec::Executor;
use jamstream_client::runtime::{DestinationState, StreamPlatform};
use jamstream_client::screens::destinations::DestinationsPanel;
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

/// A host as the app actually builds one: the invite book from the launch,
/// the destinations sheet with the keychain behind it, and the broadcast
/// view the runtime hands a host. The three status bar toggles hang off
/// exactly those, so a host fixture missing any of them renders a bar no
/// host ever sees, which is how the crowded bar went unreviewed until it
/// overlapped itself.
fn host_app(rt: DemoRuntime, theme: Theme) -> JamApp {
    {
        use jamstream_client::runtime::Runtime;
        let snap = rt.snapshot();
        assert!(
            snap.is_host && snap.broadcast.is_some(),
            "a host fixture needs a host runtime"
        );
    }
    let mut app = session_app(rt, theme);
    app.session.invites = Some(host_invites());
    app.session.destinations = Some(DestinationsPanel::new(saved_keys(&[])));
    app
}

/// The invite book a launched session carries, for the toggle to hang off.
fn host_invites() -> InvitesPanel {
    let (state, path) = invites_state();
    InvitesPanel::new(state, path)
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
    // The full host bar: three sheet toggles, the lamp, the session id, the
    // timer, the cost, and Leave, beside the readouts on one row.
    let app = host_app(DemoRuntime::frozen(FROZEN_FRAME, true), Theme::Dark);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_host");
}

#[test]
fn session_host_narrow() {
    // The same bar with 480 fewer pixels to put it in: the readouts keep
    // the first row, the controls take the second, and the strips below
    // still show a fader, a name, and a portrait that do not touch.
    let app = host_app(DemoRuntime::frozen(FROZEN_FRAME, true), Theme::Dark);
    let mut harness = app_harness(app, NARROW);
    snapshot(&mut harness, "session_host_narrow");
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
    // the status readout visible. Sam has no picture, so the avatar row
    // shows the initials disc and the empty path field.
    let mut app = session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Dark);
    app.settings_open = true;
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings");
}

#[test]
fn session_settings_avatar() {
    // The other half of the avatar row: a picture chosen this run, shown
    // cover-cropped, with Remove enabled. Wide and short, so the crop is
    // visible rather than assumed.
    let mut app = session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Light);
    app.settings_open = true;
    app.own_avatar = Some(test_avatar());
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings_avatar");
}

/// A deterministic 80x40 picture built through the public contract: warm
/// diagonal bands, no image encoder and no file involved.
fn test_avatar() -> jamstream_client::runtime::AvatarHandle {
    let (w, h) = (80u32, 40u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let band = ((x + y * 2) / 8) % 3;
            let px = match band {
                0 => [0xc0, 0x6a, 0x28, 255],
                1 => [0x8f, 0x4f, 0x1e, 255],
                _ => [0xe0, 0x94, 0x40, 255],
            };
            rgba.extend_from_slice(&px);
        }
    }
    jamstream_client::runtime::AvatarHandle {
        hash: "snapshot-avatar".to_owned(),
        width: w,
        height: h,
        rgba: std::sync::Arc::from(rgba.into_boxed_slice()),
    }
}

// The stream mix sheet over the host session: the frozen demo carries
// distinct broadcast fader values, so rows show gain, pan, and a mute.

fn stream_mix_app(theme: Theme, audition: bool) -> JamApp {
    use jamstream_client::runtime::{Command, Runtime};
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    if audition {
        rt.send(Command::SetBroadcastAudition(true));
    }
    let mut app = host_app(rt, theme);
    app.session.broadcast_open = true;
    app
}

#[test]
fn session_stream_mix() {
    let mut harness = app_harness(stream_mix_app(Theme::Dark, false), WIDE);
    snapshot(&mut harness, "session_stream_mix");
}

#[test]
fn session_stream_mix_light() {
    let mut harness = app_harness(stream_mix_app(Theme::Light, false), WIDE);
    snapshot(&mut harness, "session_stream_mix_light");
}

#[test]
fn session_stream_mix_audition() {
    // Audition on: the lamp toggle lit and the persistent reminder beside
    // the mouth-to-ear readout.
    let mut harness = app_harness(stream_mix_app(Theme::Dark, true), WIDE);
    snapshot(&mut harness, "session_stream_mix_audition");
}

// The destinations sheet, in the states a broadcast actually passes through.
// Every key here is obviously fake and none of them is ever drawn: the entry
// field is masked with no reveal, and the status the server sends back
// carries no key at all.

/// A key nothing could stream with, for the keychain slots a snapshot needs
/// to read as "key saved".
const FAKE_KEY: &str = "0000-0000-0000-0000-fake";

fn saved_keys(platforms: &[StreamPlatform]) -> Arc<MemStore> {
    let store = Arc::new(MemStore::default());
    for platform in platforms {
        let field = creds::stream_key_field(*platform);
        store
            .set(field.0, field.1, FAKE_KEY)
            .expect("store the fake key");
    }
    store
}

/// The host session with the destinations sheet open: `saved` platforms have
/// a key on this computer, `reported` is what the server says each
/// destination is doing.
fn destinations_app(
    theme: Theme,
    saved: &[StreamPlatform],
    reported: &[(StreamPlatform, DestinationState)],
) -> JamApp {
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    rt.set_destinations(reported);
    let mut app = host_app(rt, theme);
    app.session.destinations = Some(DestinationsPanel::new(saved_keys(saved)));
    app.session.destinations_open = true;
    app
}

fn live(platform: StreamPlatform) -> (StreamPlatform, DestinationState) {
    (platform, DestinationState::Live)
}

#[test]
fn session_destinations() {
    // Nothing configured and no key anywhere: the empty state says what to do
    // next, and the on air lamp is dark.
    let app = destinations_app(Theme::Dark, &[], &[]);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_destinations");
}

#[test]
fn session_destinations_light() {
    let app = destinations_app(Theme::Light, &[], &[]);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_destinations_light");
}

#[test]
fn session_destinations_key() {
    // The one surface where a key exists: masked, with its character count
    // standing in for reading it back, and the platform's own guidance above.
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    let mut app = host_app(rt, Theme::Dark);
    app.session.destinations = Some(DestinationsPanel::with_key_entry(
        Arc::new(MemStore::default()),
        StreamPlatform::Twitch,
        FAKE_KEY,
    ));
    app.session.destinations_open = true;
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_destinations_key");
}

#[test]
fn session_destinations_ready() {
    // One destination configured and waiting for Go live, one platform with a
    // key saved and nothing configured. Both rows land on the same columns.
    let app = destinations_app(
        Theme::Dark,
        &[StreamPlatform::YouTube],
        &[(StreamPlatform::Twitch, DestinationState::Idle)],
    );
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_destinations_ready");
}

#[test]
fn session_destinations_live() {
    // On air to one platform: the row lamp and the status bar lamp both lit,
    // the bitrate and dropped-frame readouts in the monospace.
    let app = destinations_app(Theme::Dark, &[], &[live(StreamPlatform::Twitch)]);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_destinations_live");
}

#[test]
fn session_destinations_live_two() {
    let app = destinations_app(
        Theme::Dark,
        &[],
        &[live(StreamPlatform::Twitch), live(StreamPlatform::YouTube)],
    );
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_destinations_live_two");
}

#[test]
fn session_destinations_failed() {
    // One destination died and the other kept streaming, which is the whole
    // point of a process per destination. The reason is verbatim from the
    // pipeline and the dropped-frame count runs red.
    let app = destinations_app(
        Theme::Dark,
        &[],
        &[
            live(StreamPlatform::Twitch),
            (
                StreamPlatform::YouTube,
                DestinationState::Failed {
                    reason: "pusher exited: rtmp connection refused".to_owned(),
                },
            ),
        ],
    );
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_destinations_failed");
}

#[test]
fn session_destinations_narrow() {
    // At 800 px the status bar carries three toggles, the cost ticker, the
    // lamp, and the live count; nothing may overlap.
    let app = destinations_app(
        Theme::Dark,
        &[],
        &[live(StreamPlatform::Twitch), live(StreamPlatform::YouTube)],
    );
    let mut harness = app_harness(app, NARROW);
    snapshot(&mut harness, "session_destinations_narrow");
}

#[test]
fn session_on_air_musician() {
    // Not a host, no sheet, no controls: a musician still sees that the room
    // is on air and to how many places.
    let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
    rt.set_destinations(&[live(StreamPlatform::Twitch), live(StreamPlatform::YouTube)]);
    let app = session_app(rt, Theme::Dark);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_on_air_musician");
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
        // DigitalOcean's real numbers: $0.01 per GB with 3000 GB included, so
        // the table and the preview in these snapshots say what the docs say.
        price: Price {
            hourly_microusd: hourly,
            egress_microusd_per_gb: 10_000,
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

/// A server artifact that could not exist: a reserved domain and a sha of
/// nothing but zeros. The fixture supplies it rather than the real
/// `option_env!` pin, so the baseline shows what a release build shows
/// without depending on how this build was compiled.
const FAKE_PIN: jamstream_cloud::PinnedServerArtifact = jamstream_cloud::PinnedServerArtifact {
    url: "https://example.invalid/jamstream/jamstreamd-linux-x86_64-musl",
    sha256: "0000000000000000000000000000000000000000000000000000000000000000",
};

/// The cost preview as a release build shows it: the server binary is
/// pinned, so there is one line about it and nothing to configure. This is
/// the path every user of a release is on, and the published screenshot.
fn wizard_preview_app(theme: Theme) -> JamApp {
    let mut app = wizard_preview_unpinned_app(theme);
    app.wizard.pinned = Some(FAKE_PIN);
    app.wizard.advanced_open = false;
    app
}

/// The other half: a build with no artifact pinned into it, which is
/// development only. The advanced fields appear and Launch stays disabled
/// until they are filled in. Not published anywhere; a release build never
/// shows this.
fn wizard_preview_unpinned_app(theme: Theme) -> JamApp {
    let mut app = test_app(theme);
    let mut w = fixed_wizard();
    w.providers[1].status = ProviderStatus::Ready;
    w.select_provider(1);
    w.continue_to_region(fixed_regions());
    w.continue_to_preview();
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
fn wizard_preview_unpinned() {
    wizard_snapshot(
        wizard_preview_unpinned_app(Theme::Dark),
        "wizard_preview_unpinned",
    );
}

#[test]
fn wizard_preview_narrow() {
    // The tallest card in the wizard at the shortest window the app opens
    // at: the card scrolls, so Back and Launch stay reachable instead of
    // sitting past the bottom edge.
    let mut harness = app_harness(wizard_preview_app(Theme::Dark), NARROW);
    snapshot(&mut harness, "wizard_preview_narrow");
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
    let mut app = host_app(DemoRuntime::frozen(FROZEN_FRAME, true), theme);
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
