//! Renders every screen and key state through the full application shell
//! (top bar plus screen content, driven by `JamApp::root_ui`), compares
//! against the committed baselines in tests/snapshots/, and drops a
//! human-reviewable copy of each render under target/ui-previews/.

use std::path::PathBuf;
use std::sync::Arc;

use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use jamstream_client::app::{JamApp, Screen};
use jamstream_client::creds::{self, CredStore, EnvReader, MemStore};
use jamstream_client::demo::{DemoRuntime, FROZEN_FRAME};
use jamstream_client::exec::Executor;
use jamstream_client::runtime::{
    AudioFaultView, BroadcastReadiness, DestinationState, RateOutcomeView, RateOutcomesView,
    RecordState, StreamPlatform,
};
use jamstream_client::screens::destinations::DestinationsPanel;
use jamstream_client::screens::home::RecentSession;
use jamstream_client::screens::host::{HostWizard, ProviderStatus, RegionRow, RegionSurvey};
use jamstream_client::screens::invites::InvitesPanel;
use jamstream_client::screens::recording::RecordingChoice;
use jamstream_client::screens::session::SettingsTab;
use jamstream_client::screens::takes::Half;
use jamstream_client::sweep::SweepOutcome;
use jamstream_client::theme::{self, Theme};
use jamstream_cloud::{
    MockProvider, Price, ProbeMatrix, ProviderError, ProviderKind, Region, RegionId, rank,
};

const WIDE: egui::Vec2 = vec2(1280.0, 800.0);
const NARROW: egui::Vec2 = vec2(800.0, 600.0);
/// Kittest reports node rects in physical pixels, so a point-sized window
/// has to be scaled by this before a rect can be compared against it.
const PPP: f32 = 2.0;

fn preview_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"))
        .join("ui-previews")
}

/// Runs a fixed number of steps (animated widgets never settle), writes the
/// preview copy, then asserts against the baseline.
fn snapshot(harness: &mut Harness<'_>, name: &str) {
    unpublish(name);
    render_and_compare(harness, name);
}

/// Drops `name` from the manifest if an earlier run put it there.
///
/// Demoting a fixture out of [`snapshot_for_docs`] is the act of declaring that
/// an image no longer shows the product honestly, so the demotion has to be
/// what removes the line: anything else leaves `site/copy-previews.sh --check`
/// passing against the line the previous run left behind. [`fresh_manifest`]
/// covers the fixture that is deleted outright, which leaves no test behind to
/// demote it.
///
/// Read, filter, rename. The replacement is atomic, so no reader ever sees a
/// partial list. Two processes doing it at once could lose an append that
/// landed between the read and the rename, which leaves the manifest a line
/// short and makes the docs check refuse to publish that image: it fails
/// closed, never open. And it can only happen on the run straight after a
/// demotion, because a name the file does not contain is not written at all.
fn unpublish(name: &str) {
    let manifest = preview_dir().join("publishable.txt");
    let Ok(listed) = std::fs::read_to_string(&manifest) else {
        return;
    };
    let line = format!("{name}.png");
    if !listed.lines().any(|l| l == line) {
        return;
    }
    let kept: String = listed
        .lines()
        .filter(|l| *l != line)
        .map(|l| format!("{l}\n"))
        .collect();
    let staged = manifest.with_extension(format!("txt.{}", std::process::id()));
    if std::fs::write(&staged, kept).is_ok() {
        let _ = std::fs::rename(&staged, &manifest);
    }
}

/// Like [`snapshot`], and additionally declares the baseline fit to publish
/// on the docs site. `site/copy-previews.sh` refuses to copy any image whose
/// name this did not record.
///
/// Use it only when the fixture renders what someone running a RELEASE build
/// would see: nothing stubbed to `None` that a real user has, nothing
/// disabled that would be enabled, no placeholder value that contradicts what
/// the product would really show.
///
/// The gate exists because three published screenshots misrepresented the
/// product in one day and nothing failed. The wizard preview showed the
/// development fallback, with artifact url and sha256 fields and a dead
/// Launch button, directly above prose saying there was nothing to configure;
/// its fixture set `pinned = None` deliberately, to stop the baseline rotting
/// on version bumps, and nobody connected that to the same file being the
/// screenshot on the hosting guide. Every destinations baseline showed a host
/// with no Invites button. The wizard region step showed DigitalOcean egress
/// at $0.00/GB, contradicting the cost guide.
///
/// A snapshot test cannot catch any of that: once a baseline is accepted it
/// passes forever, whatever it depicts. So the judgement has to be made where
/// the author knows what they stubbed, and this is that place.
fn snapshot_for_docs(harness: &mut Harness<'_>, name: &str) {
    let dir = preview_dir();
    std::fs::create_dir_all(&dir).expect("create preview dir");
    let path = dir.join("publishable.txt");
    if fresh_manifest() {
        let _ = std::fs::remove_file(&path);
    }
    let mut manifest = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open publishable manifest");
    use std::io::Write as _;
    writeln!(manifest, "{name}.png").expect("record publishable name");
    render_and_compare(harness, name);
}

/// Whether this write is the first of the run, and so the one that empties
/// the manifest.
///
/// An append-only file leaves a fixture DELETED rather than demoted with its
/// line in place forever, and `site/copy-previews.sh --check` keeps passing
/// against it. Deleting the preview directory in docs-check.yml only fixes CI,
/// so the truncation belongs here, in the only thing that writes the file.
///
/// Under nextest every test is its own process, so no one of them owns the
/// list and each would truncate away the others. There the manifest stays
/// append-only, which can only leave it holding too many names, never too
/// few. Nothing reads it on that path: the docs check renders with libtest,
/// single process, and this returns true exactly once there.
fn fresh_manifest() -> bool {
    static FIRST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
    if std::env::var_os("NEXTEST").is_some() {
        return false;
    }
    FIRST.swap(false, std::sync::atomic::Ordering::SeqCst)
}

fn render_and_compare(harness: &mut Harness<'_>, name: &str) {
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
        .with_pixels_per_point(PPP)
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
/// `in_memory` is the whole point of the helper existing: it pins the
/// credential store and the environment reader alongside the theme and the
/// recent list, so no fixture can render what one machine has stored, and
/// no fixture can make the test binary ask the developer running it for
/// their real keychain.
fn test_app(theme: Theme) -> JamApp {
    let mut app = JamApp::in_memory();
    app.theme = theme;
    app.recent = Vec::new();
    app
}

/// The guard behind [`test_app`]. Every cloud provider must read "setup
/// needed" in a fixture, on every machine, because the fixture has no
/// credentials and cannot go looking for any. A fixture that reaches the real
/// keychain or the real environment fails on a developer machine with a saved
/// DigitalOcean token, and on macOS puts up a dialog asking to unlock the
/// developer's stored cloud tokens and stream keys.
#[test]
fn fixtures_cannot_see_the_machine_that_runs_them() {
    let app = test_app(Theme::Dark);
    for row in &app.wizard.providers {
        let expected = if row.name == "local" {
            ProviderStatus::NoAccountNeeded
        } else {
            ProviderStatus::SetupNeeded
        };
        assert_eq!(
            row.status, expected,
            "provider {} read as {:?}: the fixture reached real credentials",
            row.name, row.status
        );
    }
}

fn sample_recent() -> Vec<RecentSession> {
    vec![RecentSession {
        short_id: "a3f29c41".to_owned(),
        provider: "mock".to_owned(),
        region: "mock-east".to_owned(),
        running: true,
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
    snapshot_for_docs(&mut harness, "home_empty");
}

#[test]
fn home_invalid_invite() {
    // The exact error a failed Join produces; the press itself is covered
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
fn home_narrow() {
    // The smallest window the app opens, on the first screen anyone sees. The
    // audit rendered this by hand and found it clean; nothing stopped that
    // changing (#191).
    let mut app = test_app(Theme::Dark);
    app.recent = sample_recent();
    let mut harness = app_harness(app, NARROW);
    snapshot(&mut harness, "home_narrow");
}

#[test]
fn home_light() {
    let mut app = test_app(Theme::Light);
    app.recent = sample_recent();
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "home_light");
}

#[test]
fn home_sweep_confirm() {
    // "Stop strays" asks first. It destroys every tagged machine in every
    // account this computer holds a key for, and one of them can be a
    // session a band is playing on, so the dialog says that before the
    // button does it.
    let mut app = test_app(Theme::Dark);
    app.recent = sample_recent();
    app.confirm_sweep = true;
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "home_sweep_confirm");
}

/// Enough sessions to outgrow the window, which is what a computer that has
/// hosted for a few months actually holds. The list is every session ever
/// recorded, so this is the ordinary case rather than an extreme one.
fn many_recent() -> Vec<RecentSession> {
    let places = [
        ("aws", "us-west-1"),
        ("aws", "us-east-2"),
        ("local", "local"),
        ("digitalocean", "sfo3"),
        ("mock", "mock-west"),
    ];
    (0..22)
        .map(|i| {
            let (provider, region) = places[i % places.len()];
            RecentSession {
                short_id: format!("{:08x}", 0xc101_beae_u32.wrapping_add(i as u32 * 0x9e37)),
                provider: provider.to_owned(),
                region: region.to_owned(),
                running: false,
            }
        })
        .collect()
}

/// Running sessions buried in a long history. Each is a machine still being
/// billed, so each has to be on screen whatever the ended ones do.
fn many_recent_with_strays() -> Vec<RecentSession> {
    let mut rows = many_recent();
    for (i, row) in rows.iter_mut().enumerate() {
        row.running = i % 9 == 4;
    }
    rows
}

#[test]
fn home_running_sessions_survive_a_long_history() {
    let mut app = test_app(Theme::Dark);
    app.recent = many_recent_with_strays();
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "home_running_sessions_survive_a_long_history");
}

/// The count of ended rows depends on the window, so the short window is the
/// one that proves the fill is measured rather than a constant.
#[test]
fn home_many_sessions_narrow() {
    let mut app = test_app(Theme::Dark);
    app.recent = many_recent();
    let mut harness = app_harness(app, NARROW);
    snapshot(&mut harness, "home_many_sessions_narrow");
}

/// A running session is a machine still being billed, and Stop strays is how
/// it gets stopped, so no window size may bury one behind the fold.
#[test]
fn a_running_session_is_never_dropped() {
    for size in [WIDE, NARROW] {
        let rows = many_recent_with_strays();
        let running: Vec<String> = rows
            .iter()
            .filter(|row| row.running)
            .map(|row| row.short_id.clone())
            .collect();
        assert!(running.len() >= 2, "the fixture needs strays: {running:?}");
        let mut app = test_app(Theme::Dark);
        app.recent = rows;
        let mut harness = app_harness(app, size);
        harness.run_steps(4);
        for id in &running {
            let node = harness
                .get_all_by_label_contains(id.as_str())
                .next()
                .unwrap_or_else(|| panic!("{id} is not drawn at {size:?}"));
            assert!(
                node.rect().bottom() <= size.y * PPP,
                "{id} is below the window edge at {size:?}: {:?}",
                node.rect()
            );
        }
    }
}

/// Whatever the fill leaves out, the screen says how much. A list that stops
/// without saying so reads as the whole list.
#[test]
fn home_says_how_many_sessions_it_left_out() {
    for size in [WIDE, NARROW] {
        let mut app = test_app(Theme::Dark);
        app.recent = many_recent();
        let mut harness = app_harness(app, size);
        harness.run_steps(4);
        assert!(
            harness
                .get_all_by_label_contains("older ended sessions are not shown")
                .next()
                .is_some(),
            "nothing said what was left out at {size:?}"
        );
    }
}

#[test]
fn home_many_sessions() {
    let mut app = test_app(Theme::Dark);
    app.recent = many_recent();
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "home_many_sessions");
}

#[test]
fn home_sweep_report_over_a_long_list() {
    // The report has to be readable without leaving the window, and the
    // button that produces it is at the top of this card. A report drawn
    // under twenty-two rows is off the bottom of the screen.
    let mut app = test_app(Theme::Dark);
    app.recent = many_recent();
    app.swept = Some(unaccounted_sweep());
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "home_sweep_report_over_a_long_list");
}

/// The report has to be on screen, not merely drawn. Both halves are checked
/// against geometry rather than an image, because an image only catches this
/// when somebody looks at it.
#[test]
fn the_sweep_report_stays_on_screen_above_a_long_list() {
    let mut app = test_app(Theme::Dark);
    app.recent = many_recent();
    app.swept = Some(unaccounted_sweep());
    let mut harness = app_harness(app, WIDE);
    harness.run_steps(4);

    let report = harness
        .get_all_by_label_contains("Found 1 machine")
        .next()
        .expect("the report says what it found")
        .rect();
    let first_row = harness
        .get_all_by_label_contains("aws us-west-1")
        .next()
        .expect("the rows are drawn")
        .rect();
    assert!(
        report.bottom() <= first_row.top(),
        "the report is under the rows: report {report:?}, first row {first_row:?}"
    );
    assert!(
        report.bottom() <= WIDE.y * PPP,
        "the report is off the bottom of a {} point window: {report:?}",
        WIDE.y
    );
}

#[test]
fn home_sweep_report() {
    // What a sweep that could not account for everything says, off a real
    // report: two clouds swept, one of them refusing to list, one machine
    // that would not die, and a provider with no key on this computer. Each
    // of those is a bill, so each is in the danger colour, apart from what
    // the sweep actually did.
    let mut app = test_app(Theme::Dark);
    app.recent = sample_recent();
    app.swept = Some(unaccounted_sweep());
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "home_sweep_report");
}

/// A [`SweepOutcome`] off the real sweeper rather than a hand-built one: the
/// mocks refuse a listing and a destroy, and what the card draws is whatever
/// `jamstream_cloud::sweep` made of that.
fn unaccounted_sweep() -> SweepOutcome {
    let seed = |kind: ProviderKind, session: &str| {
        use jamstream_cloud::Provider as _;
        let p = MockProvider::with_default_regions(kind);
        let region = p.regions()[0].clone();
        p.seed_instance(&region, vec![jamstream_cloud::session_tag(session)]);
        p
    };
    let throttled = seed(ProviderKind::Aws, "a1a1");
    throttled.fail_next_lists(1, ProviderError::RateLimited { retry_after: None });
    let stubborn = seed(ProviderKind::Gcp, "b2b2");
    stubborn.fail_next_destroys(1, ProviderError::Other("instance is locked".to_owned()));
    let providers: Vec<Box<dyn jamstream_cloud::Provider>> =
        vec![Box::new(throttled), Box::new(stubborn)];
    let report = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("fixture runtime")
        .block_on(jamstream_cloud::sweep(
            &providers,
            jamstream_cloud::SweepFilter::All,
            false,
        ));
    SweepOutcome::new(
        &report,
        Ok(jamstream_cli::sweep::Reconciled {
            closed: vec!["c3c3c3c3".repeat(4)],
            unwritten: Vec::new(),
        }),
        vec![ProviderKind::DigitalOcean],
    )
}

/// The Takes screen with everything it can say on it at once.
///
/// The rows are built by the screen's own [`rows_from`] and
/// [`takes_from_objects`] out of session records, bucket sidecars, and a
/// listing, so the grouping into takes, the sizes, the countdowns and the
/// egress figures on the buttons are all the product's own arithmetic. Two
/// things are set here rather than found: the objects a bucket listing would
/// have returned, and, on the first session's older take, that its mix is
/// already on this computer. The second is exactly what a finished download
/// leaves behind, which is the state the primary action is Reveal in.
///
/// The paths are a plausible home directory rather than this machine's,
/// because a baseline that rendered the developer's own would never match
/// twice.
fn takes_app(theme: Theme) -> JamApp {
    takes_app_with_expired(theme, 0)
}

/// The same fixture plus `expired` sessions whose retention deadline is a week
/// behind the fixture's clock, none of them downloaded. They are what the
/// screen is entitled to drop.
fn takes_app_with_expired(theme: Theme, expired: usize) -> JamApp {
    use jamstream_cli::recordings::Take;
    use jamstream_cli::state::{RecordingRecord, RetentionApplied, SessionStatus};
    use jamstream_client::screens::takes::{LocalTake, rows_from, takes_from_objects};

    // 2026-07-28 20:20 UTC, ten minutes after the last take of the night.
    const NOW: u64 = 1_785_270_000;
    let music = PathBuf::from("/Users/you/Music/JamStream");
    let disk = PathBuf::from("/Users/you/Library/Application Support/jamstream/recordings");

    let session =
        |id: &str, created: u64, secs: u64, provider: &str| jamstream_cli::state::SessionState {
            session_id_hex: id.to_owned(),
            provider: provider.to_owned(),
            region: if provider == "local" { "local" } else { "sfo3" }.to_owned(),
            instance_id: "droplet-1".to_owned(),
            address: "203.0.113.7:43210".to_owned(),
            created_unix: created,
            hourly_microusd: if provider == "local" { 0 } else { 16_800 },
            issuer_private_key_b64: String::new(),
            server_public_key_b64: "c2VydmVy".to_owned(),
            invites: Vec::new(),
            status: SessionStatus::Ended,
            ended_unix: Some(created + secs),
        };
    let record = |applied: RetentionApplied| RecordingRecord {
        provider: "digitalocean".to_owned(),
        bucket: "our-takes".to_owned(),
        region: "sfo3".to_owned(),
        retention: "30d".to_owned(),
        stems: true,
        applied: Some(applied),
    };
    let unenforced = RetentionApplied::Unenforced {
        note: "This bucket's key cannot read its lifecycle rules, so \"Delete after 30 days\" \
               was not applied: nothing will delete these takes but you."
            .to_owned(),
    };
    // A four piece: the mix, and one stereo stem each.
    let objects = |base: &str, mix: u64, stem: u64| {
        let mut out = vec![Take {
            key: format!("jamstream/recordings/x/{base}-mix.flac"),
            name: format!("{base}-mix.flac"),
            size: mix,
            last_modified: None,
        }];
        for player in ["Ana", "Ben", "Cass", "Dai"] {
            out.push(Take {
                key: format!("jamstream/recordings/x/{base}-{player}.flac"),
                name: format!("{base}-{player}.flac"),
                size: stem,
                last_modified: None,
            });
        }
        out
    };

    let mut sessions = vec![
        (
            session("a3f29c41deadbeef", 1_785_264_000, 2_820, "digitalocean"),
            Some(record(RetentionApplied::ServerSide)),
        ),
        (
            session("77c0de12beefcafe", 1_782_840_000, 5_400, "digitalocean"),
            Some(record(RetentionApplied::ServerSide)),
        ),
        (
            session("b41f0a99cafef00d", 1_785_100_000, 3_600, "digitalocean"),
            Some(record(unenforced)),
        ),
        (
            session("5c8e2b70feedface", 1_785_000_000, 7_800, "local"),
            None,
        ),
    ];
    // 37 days before the fixture's clock against a 30 day rule, so the
    // deadline is a week gone.
    for i in 0..expired {
        sessions.push((
            session(
                &format!("{:016x}", 0xe0e0_0000_0000_0000_u64 + i as u64),
                NOW - 37 * 86_400 - i as u64 * 3_600,
                1_800,
                "digitalocean",
            ),
            Some(record(RetentionApplied::ServerSide)),
        ));
    }
    let local = vec![LocalTake {
        path: disk.join("jamstream-2026-07-25-1730-mix.flac"),
        bytes: 2_400_000_000,
        modified_unix: 1_785_000_600,
    }];
    let mut rows = rows_from(&sessions, &local, &disk, &music, NOW);
    for row in &mut rows {
        if row.record().is_none() {
            continue;
        }
        // The take's own name, as the recorder builds it from the clock when
        // Record is pressed: a few minutes into its session.
        let base = match row.short_id.as_str() {
            "77c0de12" => "jamstream-2026-06-30-1745",
            "b41f0a99" => "jamstream-2026-07-26-2110",
            _ => "jamstream-2026-07-28-1845",
        };
        let mut takes = takes_from_objects(&objects(base, 1_100_000_000, 1_100_000_000), &row.dir)
            .expect("group the listing");
        if row.short_id == "a3f29c41" {
            // Its mix was fetched already, so that half is a Reveal.
            for file in &mut takes[0].mix.files {
                file.local = Some(row.dir.join(&file.name));
            }
        }
        row.takes = takes;
        row.listing = false;
    }

    let mut app = test_app(theme);
    app.screen = Screen::Takes;
    app.takes.rows = rows;
    app
}

#[test]
fn takes() {
    let mut harness = app_harness(takes_app(Theme::Dark), WIDE);
    snapshot_for_docs(&mut harness, "takes");
}

#[test]
fn takes_downloading() {
    // The state a musician spends the longest in: a take is gigabytes, so the
    // wait is the part that would otherwise be a surprise. 1.4 GB of the 4.4 GB
    // of stems is on disk, and the other buttons are out while it runs, because
    // one download at a time is what the screen allows.
    let mut app = takes_app(Theme::Dark);
    app.takes.park_download(
        "a3f29c41deadbeef",
        "jamstream-2026-07-28-1845",
        Half::Stems,
        4_400_000_000,
        1_400_000_000,
    );
    let mut harness = app_harness(app, WIDE);
    snapshot_for_docs(&mut harness, "takes_downloading");
}

#[test]
fn takes_light() {
    let mut harness = app_harness(takes_app(Theme::Light), WIDE);
    snapshot(&mut harness, "takes_light");
}

#[test]
fn takes_narrow() {
    // The smallest window the app opens. The cards scroll; a row's price and
    // its button may not slide off the edge or over each other.
    let mut harness = app_harness(takes_app(Theme::Dark), NARROW);
    snapshot(&mut harness, "takes_narrow");
}

#[test]
fn takes_listing_refused() {
    // A bucket that said no, drawn as the shipped mapping renders it: the
    // row a real S3 AccessDenied produces is a sentence with the remedy,
    // never the XML document, which carries the AWS account number and is
    // exactly what someone would screenshot. The session records to
    // DigitalOcean, so the remedy has to be the Spaces one; this fixture
    // published a Spaces host being told to edit an IAM policy.
    let denied = jamstream_cli::CliError::Provider(jamstream_cloud::ProviderError::Auth(
        "http 403 Forbidden: <?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <Error><Code>AccessDenied</Code><Message>User: \
         arn:aws:iam::887762372032:user/jamstream-recordings is not \
         authorized to perform: s3:ListBucket</Message>\
         <RequestId>Q0YMR4GFKCH1Y688</RequestId></Error>"
            .to_owned(),
    ));
    let mut app = takes_app(Theme::Dark);
    app.takes.rows[0].takes.clear();
    let provider = app.takes.rows[0].provider_name();
    assert_eq!(provider, "digitalocean");
    app.takes.rows[0].error = Some(jamstream_client::screens::takes::error_sentence(
        "listing a3f29c41",
        &provider,
        &denied,
    ));
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "takes_listing_refused");
}

#[test]
fn takes_reveal_error() {
    // A file manager that would not open, which is feedback on a button
    // inside a row. The list fills the rest of the window, so this has to
    // sit above it or it is off the bottom edge with no way to reach it.
    let mut app = takes_app(Theme::Dark);
    app.takes.reveal_error = Some((
        std::path::PathBuf::from("/Users/sean/Music/jamstream/a3f29c41/mix.flac"),
        "no file manager answered".to_owned(),
    ));
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "takes_reveal_error");
}

/// The same rule on the takes screen: the list fills the window, so feedback
/// after it is unreachable.
#[test]
fn a_take_that_would_not_open_says_so_on_screen() {
    let mut app = takes_app(Theme::Dark);
    app.takes.reveal_error = Some((
        std::path::PathBuf::from("/Users/sean/Music/jamstream/a3f29c41/mix.flac"),
        "no file manager answered".to_owned(),
    ));
    let mut harness = app_harness(app, WIDE);
    harness.run_steps(4);

    let reason = harness
        .get_all_by_label_contains("no file manager answered")
        .next()
        .expect("the reason is drawn")
        .rect();
    assert!(
        reason.bottom() <= WIDE.y * PPP,
        "the reason is off the bottom of a {} point window: {reason:?}",
        WIDE.y
    );
}

#[test]
fn takes_with_expired_takes() {
    let mut harness = app_harness(takes_app_with_expired(Theme::Dark, 10), WIDE);
    snapshot(&mut harness, "takes_with_expired_takes");
}

/// A take inside its window, or already on this computer, is one somebody can
/// still play, so no window size may drop one. The expired ones with nothing
/// here are the only rows the screen is allowed to leave out.
#[test]
fn a_take_still_worth_playing_is_never_dropped() {
    for size in [WIDE, NARROW] {
        let app = takes_app_with_expired(Theme::Dark, 10);
        let worth_playing: Vec<String> = app
            .takes
            .rows
            .iter()
            .filter(|row| !row.expired() || row.downloaded())
            .map(|row| row.short_id.clone())
            .collect();
        assert_eq!(
            worth_playing.len(),
            4,
            "the fixture's four keepers: {worth_playing:?}"
        );
        let mut harness = app_harness(app, size);
        harness.run_steps(4);
        for id in &worth_playing {
            assert!(
                harness
                    .get_all_by_label_contains(id.as_str())
                    .next()
                    .is_some(),
                "{id} is not on screen at {size:?}"
            );
        }
        // Whatever it left out, it has to say so: a silent list reads as the
        // whole list.
        assert!(
            harness
                .get_all_by_label_contains("expired takes are not shown")
                .next()
                .is_some(),
            "nothing said what was left out at {size:?}"
        );
    }
}

#[test]
fn takes_empty() {
    // A host who has never recorded. One sentence saying what to do, which is
    // the rule for every empty state in the app.
    let mut app = test_app(Theme::Dark);
    app.screen = Screen::Takes;
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "takes_empty");
}

#[test]
fn session_demo() {
    let app = session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Dark);
    let mut harness = app_harness(app, WIDE);
    snapshot_for_docs(&mut harness, "session_demo");
}

#[test]
fn session_host() {
    // The full host bar: the session id, the timer, the cost, Record, and
    // Leave, beside the readouts on one row. This is also the cluster's empty
    // case, so it reserves nothing and the bar is calm; a fixture for the idle
    // bar on its own would be pixel-identical to this one.
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
fn session_quit_confirm() {
    // The window's close button pressed while this app is the host: one
    // confirmation, three ways out, and the kept-running exits named in it
    // (#322). The flag is set directly because a harness has no window
    // manager; the interaction tests drive the buttons.
    let mut app = host_app(DemoRuntime::frozen(FROZEN_FRAME, true), Theme::Dark);
    app.confirm_quit = true;
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_quit_confirm");
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
    snapshot_for_docs(&mut harness, "session_full");
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

/// The three presences a roster can report, side by side: Ana has gone quiet,
/// Ben is gone, everyone else is playing. Both of the states that are not the
/// resting one had no baseline at all before, and the middle one had no pixels
/// to draw (#285).
fn presence_app(theme: Theme) -> JamApp {
    let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
    rt.set_quiet(1, true);
    rt.set_away(2, true);
    session_app(rt, theme)
}

#[test]
fn session_presence() {
    // What this is for: one amber dot in a row of resting ones, and one strip
    // greyed out beside it, at a glance and without hovering anything.
    let mut harness = app_harness(presence_app(Theme::Dark), WIDE);
    snapshot(&mut harness, "session_presence");
}

#[test]
fn session_presence_light() {
    // The light palette steps the resting dot the other way, so the amber has
    // a different neighbour to stand out against.
    let mut harness = app_harness(presence_app(Theme::Light), WIDE);
    snapshot(&mut harness, "session_presence_light");
}

#[test]
fn session_presence_narrow() {
    // The smallest window the app opens: the strips keep their width, so the
    // dot keeps its place beside the name rather than being the thing that
    // gets squeezed.
    let mut harness = app_harness(presence_app(Theme::Dark), NARROW);
    snapshot(&mut harness, "session_presence_narrow");
}

#[test]
fn session_presence_light_narrow() {
    // The corner where the amber has the least to work with: the light panel
    // and the smallest window at once.
    let mut harness = app_harness(presence_app(Theme::Light), NARROW);
    snapshot(&mut harness, "session_presence_light_narrow");
}

/// A session whose audio device stopped and will not reopen: the reason the
/// device gave, over strips that are all still there. The reason is the one
/// the offline backend really produces when it is handed a rate it does not
/// run at, which is the refusal a musician swapping interfaces mid-song gets.
fn device_refused_app(theme: Theme) -> JamApp {
    let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
    rt.set_device_error(Some(
        "unsupported audio configuration: \
         wav device runs at 44100 Hz and will not open at 48000 Hz",
    ));
    session_app(rt, theme)
}

#[test]
fn session_device_refused() {
    let mut harness = app_harness(device_refused_app(Theme::Dark), WIDE);
    snapshot(&mut harness, "session_device_refused");
}

#[test]
fn session_device_refused_light() {
    let mut harness = app_harness(device_refused_app(Theme::Light), WIDE);
    snapshot(&mut harness, "session_device_refused_light");
}

#[test]
fn session_device_refused_narrow() {
    // The smallest window the app opens: the sentence wraps above the strips
    // and the faders keep their floor under it.
    let mut harness = app_harness(device_refused_app(Theme::Dark), NARROW);
    snapshot(&mut harness, "session_device_refused_narrow");
}

/// A session riding the boundary converter on both directions (#347 rung 3),
/// the shape a 44.1 kHz interface gives a session on a host with no clock to
/// move: the status bar carries the muted device-rate tag beside mouth to
/// ear, and the Audio tab names each conversion and its cost.
fn converting_app(theme: Theme) -> JamApp {
    let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
    rt.set_rate(Some(RateOutcomesView {
        capture: RateOutcomeView::Resampled {
            device: 44_100,
            added_ms: 3.2,
        },
        playback: RateOutcomeView::Resampled {
            device: 44_100,
            added_ms: 3.2,
        },
    }));
    session_app(rt, theme)
}

#[test]
fn session_converting() {
    let mut harness = app_harness(converting_app(Theme::Dark), WIDE);
    snapshot(&mut harness, "session_converting");
}

#[test]
fn session_converting_narrow() {
    // The tag competes with the meters for the bar's tightest layout; it may
    // never push the readout out of the zone.
    let mut harness = app_harness(converting_app(Theme::Dark), NARROW);
    snapshot(&mut harness, "session_converting_narrow");
}

/// A session whose playout ring is in a crackling run: the persistent state
/// the status bar and the Audio tab both read off the snapshot, in place of
/// a chat line that may already have scrolled past by the time somebody
/// looks up.
fn crackling_app(theme: Theme, crackling: bool) -> JamApp {
    let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
    rt.set_crackling(crackling);
    session_app(rt, theme)
}

#[test]
fn session_crackling_narrow() {
    // The narrowest window, where the tag competes hardest with the meters
    // and the two lamps for the bar's own room.
    let mut harness = app_harness(crackling_app(Theme::Dark, true), NARROW);
    snapshot(&mut harness, "session_crackling_narrow");
}

#[test]
fn session_crackling_narrow_clear() {
    // The same fixture with the run over, for a direct look at the tag gone.
    let mut harness = app_harness(crackling_app(Theme::Dark, false), NARROW);
    snapshot(&mut harness, "session_crackling_narrow_clear");
}

/// A session with no audio stream at all: the reopen cadence either working
/// on it or done trying. The same pair of surfaces carries it, the bar for
/// the glance and the Audio tab for the pick, because a musician who has gone
/// silent in both directions finds out from the screen they are looking at.
fn no_audio_app(theme: Theme, fault: AudioFaultView) -> JamApp {
    let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
    rt.set_audio_fault(Some(fault));
    session_app(rt, theme)
}

#[test]
fn session_no_audio_narrow() {
    // The narrowest window, where the tag competes hardest with the meters
    // and the two lamps for the bar's own room. Both faults read the same
    // here: what differs between them is the sentence on the tab.
    let app = no_audio_app(Theme::Dark, AudioFaultView::Retrying);
    let mut harness = app_harness(app, NARROW);
    snapshot(&mut harness, "session_no_audio_narrow");
}

/// The drawer open on one tab. Every settings fixture goes through here, so
/// the tab row in each of them is the one the app really builds for that
/// role and screen.
fn drawer_app(mut app: JamApp, tab: SettingsTab) -> JamApp {
    app.settings_open = true;
    app.settings_tab = tab;
    app
}

/// Scrolls the drawer's panel to the end, for the tabs whose content is
/// taller than the drawer. A fixture that did not scroll would show the top
/// of a panel and call it the panel.
fn scroll_drawer(harness: &mut Harness<'_>, size: egui::Vec2) {
    harness.run_steps(2);
    harness.event(egui::Event::PointerMoved(egui::pos2(
        size.x - 100.0,
        size.y / 2.0,
    )));
    harness.run_steps(1);
    harness.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: vec2(0.0, -2000.0),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::NONE,
    });
    harness.run_steps(3);
    // The pointer leaves before the render: egui paints the cursor, and a
    // published screenshot with an arrow parked in a text field in it is a
    // screenshot of the test rather than of the product.
    harness.event(egui::Event::PointerGone);
    harness.run_steps(2);
}

#[test]
fn session_settings() {
    // The drawer anchors top right and must leave the strips and the status
    // readout visible. A plain musician's tab row is Audio and You: the two
    // session-scoped tabs are not rendered as dead slots, they are absent.
    // Sam has no picture, so the You tab would show the initials disc.
    let app = drawer_app(
        session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Dark),
        SettingsTab::Audio,
    );
    let mut harness = app_harness(app, WIDE);
    snapshot_for_docs(&mut harness, "session_settings");
}

#[test]
fn session_settings_hear_self() {
    // The other state of the same checkbox: on, with the two facts about it
    // still on screen, since a control that reads the same either way is
    // not a control.
    use jamstream_client::runtime::{Command, Runtime};
    let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
    rt.send(Command::SetHearSelf(true));
    let app = drawer_app(session_app(rt, Theme::Dark), SettingsTab::Audio);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings_hear_self");
}

#[test]
fn session_settings_device_refused() {
    // The other place the refusal has to be: with the drawer open it is over
    // the mixer's copy of it, and this tab is where the pick that failed gets
    // changed, so the reason sits under the pickers.
    let app = drawer_app(device_refused_app(Theme::Dark), SettingsTab::Audio);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings_device_refused");
}

#[test]
fn session_settings_converting() {
    // The conversion's cost under the pickers that caused it: the Audio tab
    // is where the pick gets changed, so the disclosure sits with the
    // controls rather than only behind the status bar's hover.
    let app = drawer_app(converting_app(Theme::Dark), SettingsTab::Audio);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings_converting");
}

#[test]
fn session_settings_crackling() {
    // The remedy sits with the control it names: a notice beside Buffer size
    // while the ring is running dry, on the tab a musician opens to act on it.
    let app = drawer_app(crackling_app(Theme::Dark, true), SettingsTab::Audio);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings_crackling");
}

#[test]
fn session_settings_crackling_clear() {
    let app = drawer_app(crackling_app(Theme::Dark, false), SettingsTab::Audio);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings_crackling_clear");
}

#[test]
fn session_settings_no_audio() {
    // Under the pickers, because that is where the pick is: the cadence is
    // still working on it, so this one asks for nothing.
    let app = drawer_app(
        no_audio_app(Theme::Dark, AudioFaultView::Retrying),
        SettingsTab::Audio,
    );
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings_no_audio");
}

#[test]
fn session_settings_no_audio_gave_up() {
    // The end of the cadence, which is the one state that needs somebody:
    // nothing reopens the stream now except a pick on this tab.
    let app = drawer_app(
        no_audio_app(Theme::Dark, AudioFaultView::GaveUp { tries: 6 }),
        SettingsTab::Audio,
    );
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings_no_audio_gave_up");
}

#[test]
fn session_settings_host_tabs() {
    // The other tab row: a host this app launched has all four, so this is
    // the fixture that proves the row adapts rather than greys out.
    let app = drawer_app(
        host_app(DemoRuntime::frozen(FROZEN_FRAME, true), Theme::Dark),
        SettingsTab::Audio,
    );
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings_host_tabs");
}

#[test]
fn session_settings_narrow() {
    // The drawer at the smallest window the app opens, over the busiest
    // session there is: buffer size and input level are both on screen with
    // the host bar under them, and the mouth-to-ear readout the buffer is
    // traded against is visible in both places at once.
    let app = drawer_app(
        host_app(DemoRuntime::frozen(FROZEN_FRAME, true), Theme::Dark),
        SettingsTab::Audio,
    );
    let mut harness = app_harness(app, NARROW);
    snapshot(&mut harness, "session_settings_narrow");
}

#[test]
fn home_settings() {
    // Settings from the home screen, outside any session: Audio and You, and
    // no Broadcast or Invites, because there is no broadcast to mix and no
    // seat to invite anyone into.
    //
    // The faint marks at the left edge are egui's, not ours: an anchored
    // window's first pass lays out at the origin and is painted, which every
    // other fixture hides behind session content.
    let app = drawer_app(test_app(Theme::Dark), SettingsTab::Audio);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "home_settings");
}

#[test]
fn home_settings_narrow() {
    // The drawer at the smallest window with no session behind it: three tabs,
    // and the body scrolls rather than running past the bottom edge.
    let app = drawer_app(test_app(Theme::Dark), SettingsTab::Audio);
    let mut harness = app_harness(app, NARROW);
    snapshot(&mut harness, "home_settings_narrow");
}

#[test]
fn session_settings_you() {
    // The You tab: the avatar row above the theme picker, which is where the
    // two things you set once ended up, and under them the build and this
    // run's log. The path is the one main hands over, so a fixture that left
    // it None would render the tab a real launch never shows.
    let mut app = drawer_app(
        session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Dark),
        SettingsTab::You,
    );
    app.log_path = Some(PathBuf::from(
        "/Users/you/Library/Application Support/jamstream/logs/app.log",
    ));
    let mut harness = app_harness(app, WIDE);
    // The whole point of drawing it: a Windows release build has no console,
    // so this label is the only place the path appears.
    harness.run_steps(4);
    assert!(
        harness
            .query_by_label_contains("jamstream/logs/app.log")
            .is_some(),
        "the log path has to be readable in the drawer"
    );
    snapshot(&mut harness, "session_settings_you");
}

#[test]
fn session_settings_no_log() {
    // A machine with no platform data directory: logging::init returned None
    // and there is no file. Saying so beats printing a path nothing wrote.
    let app = drawer_app(
        session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Dark),
        SettingsTab::You,
    );
    assert!(app.log_path.is_none());
    let mut harness = app_harness(app, WIDE);
    harness.run_steps(4);
    assert!(harness.query_by_label("Copy path").is_none());
    snapshot(&mut harness, "session_settings_no_log");
}

#[test]
fn session_settings_avatar() {
    // The other half of the avatar row: a photograph picked this run,
    // through the same read, fit, and decode a picked file goes through, so
    // the disc, the file name, and the two sizes are all the real ones.
    let mut app = drawer_app(
        session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Light),
        SettingsTab::You,
    );
    app.load_avatar_from(avatar_fixture("rehearsal.jpg"));
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings_avatar");
}

#[test]
fn session_settings_avatar_refused() {
    // A file the dialog's image filter offers and the fitter still cannot
    // use: a .png that is really a GIF, which is the shape every refusal
    // takes on screen. The disc stays on the initials and the reason sits
    // under the row in the danger colour.
    let mut app = drawer_app(
        session_app(DemoRuntime::frozen(FROZEN_FRAME, false), Theme::Dark),
        SettingsTab::You,
    );
    app.load_avatar_from(fixture_file(
        "poster.png",
        b"GIF89a\x01\x00\x01\x00\x00\x00\x00;",
    ));
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_settings_avatar_refused");
}

/// A file on disk that is a real photograph in every way that matters: a
/// 1600x1200 JPEG, over the byte cap and over the dimension cap, which the
/// fitter has to bring down to 256x256.
fn avatar_fixture(name: &str) -> PathBuf {
    let (w, h) = (1600u32, 1200u32);
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        // Warm diagonal bands with a soft vertical wash, so the disc has
        // both flat areas and detail through the crop.
        let band = ((x + y * 2) / 90) % 3;
        let wash = (y * 40 / h) as u8;
        match band {
            0 => image::Rgb([0xc0 - wash, 0x6a, 0x28 + wash]),
            1 => image::Rgb([0x8f, 0x4f + wash, 0x1e]),
            _ => image::Rgb([0xe0 - wash, 0x94, 0x40]),
        }
    });
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 95)
        .encode_image(&img)
        .expect("encode the fixture photo");
    fixture_file(name, &buf.into_inner())
}

/// Writes a fixture under a name of our choosing, because the row shows the
/// file's name and the snapshot has to be the same every run.
fn fixture_file(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = private_temp_dir(&format!("jamstream-snapshot-{}", std::process::id()));
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write the fixture");
    path
}

/// A fixture directory only this user can read. Not `temp_dir()` itself: the
/// state writer refuses a world-writable parent, and Linux's /tmp is one.
fn private_temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(name);
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("fixture dir mode");
    }
    dir
}

// The Recording tab: where takes go, and the key that writes them. Always
// present, because a bucket is a setting for this computer rather than session
// state. Every key here is fake and none of them is ever drawn: both fields are
// masked, and the assertion under the typed fixture is that neither half of the
// pair reaches the accessibility tree either.

/// A key pair nothing could write a bucket with.
const FAKE_STORAGE_ID: &str = "DO00FAKEFAKEFAKEFAKE";
const FAKE_STORAGE_SECRET: &str = "0000000000000000000000000000000000000000fake";

/// An app on a computer set up to record: a DigitalOcean bucket in the
/// Recording tab's preferences and, when `saved`, a key in the keychain behind
/// it. One store for the app and the wizard both, so nothing in a fixture can be
/// armed in one half and unconfigured in the other.
///
/// The preferences are held in this process (`None`), so no fixture reads the
/// bucket of whoever runs the suite and none writes theirs.
fn configured_app(theme: Theme, saved: bool) -> (Arc<MemStore>, EnvReader, JamApp) {
    let store = Arc::new(MemStore::default());
    if saved {
        creds::save_storage_credential(
            store.as_ref(),
            ProviderKind::DigitalOcean,
            FAKE_STORAGE_ID,
            FAKE_STORAGE_SECRET,
        )
        .expect("store the fake key");
    }
    let env: EnvReader = Arc::new(|_| None);
    let mut app = JamApp::new(store.clone(), Arc::clone(&env), None);
    app.theme = theme;
    app.recent = Vec::new();
    app.recording
        .remember_bucket(ProviderKind::DigitalOcean, "our-jams", "nyc3");
    (store, env, app)
}

/// The Recording tab over a host session.
fn recording_app(theme: Theme, saved: bool) -> JamApp {
    let (_, _, mut app) = configured_app(theme, saved);
    app.runtime = Some(Box::new(DemoRuntime::frozen(FROZEN_FRAME, true)));
    app.session.invites = Some(host_invites());
    app.session.destinations = Some(DestinationsPanel::new(saved_keys(&[])));
    app.screen = Screen::Session;
    drawer_app(app, SettingsTab::Recording)
}

#[test]
fn session_settings_recording() {
    // A computer that is set up to record: the bucket, its region, and a key
    // already in the keychain, which is why the key fields are empty and the
    // line above them says one is there. This is the state a host is in when
    // they arm a launch, and the five-tab row is a real host's.
    let mut harness = app_harness(recording_app(Theme::Dark, true), WIDE);
    snapshot_for_docs(&mut harness, "session_settings_recording");
}

#[test]
fn session_settings_recording_light() {
    let mut harness = app_harness(recording_app(Theme::Light, true), WIDE);
    snapshot(&mut harness, "session_settings_recording_light");
}

#[test]
fn session_settings_recording_narrow() {
    // The tab at the smallest window the app opens: the drawer keeps its width,
    // every field and both pick lists fit inside it, and the status bar under
    // it stays clear.
    let mut harness = app_harness(recording_app(Theme::Dark, true), NARROW);
    snapshot(&mut harness, "session_settings_recording_narrow");
}

#[test]
fn session_settings_recording_empty() {
    // Nothing set up anywhere, which is every host's first visit: the provider
    // rows read "not set up" and the fields are empty.
    let mut harness = app_harness(
        drawer_app(test_app(Theme::Dark), SettingsTab::Recording),
        WIDE,
    );
    snapshot(&mut harness, "session_settings_recording_empty");
}

/// A launch whose bucket would not take the retention rule.
///
/// Constructed rather than driven, because a snapshot fixture is synchronous
/// and the store's answer is not; `tests/retention.rs` is where the value comes
/// out of the store itself. The note is the store's own text either way.
fn unenforced_retention() -> jamstream_cloud::RetentionEnforcement {
    let retention = jamstream_cloud::Retention::Days30;
    jamstream_cloud::RetentionEnforcement::Manual {
        retention,
        note: jamstream_cloud::retention::manual_note(retention),
    }
}

#[test]
fn session_settings_recording_unenforced() {
    // The bucket took the takes and refused the rule, so nothing is going to
    // delete them. Scrolled to the retention rows, because the note sits under
    // the choice it contradicts and that is the whole point of it.
    let mut app = recording_app(Theme::Dark, true);
    app.recording.applied = Some(unenforced_retention());
    let mut harness = app_harness(app, WIDE);
    scroll_drawer(&mut harness, WIDE);
    snapshot(&mut harness, "session_settings_recording_unenforced");
}

#[test]
fn session_record_unenforced_retention() {
    // The same fact on the sheet a host reads before pressing Record, which is
    // the last moment it is any use to them.
    let mut app = record_app(Theme::Dark, RecordState::Idle, false);
    app.recording.applied = Some(unenforced_retention());
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_record_unenforced_retention");
}

#[test]
fn session_settings_recording_typed() {
    // The one surface where a storage key exists at all: a pair pasted into the
    // fields, both masked, with the reveal off. Scrolled to the fields, which is
    // where a host who just pasted one is looking.
    let mut app = recording_app(Theme::Dark, false);
    app.recording.type_key(FAKE_STORAGE_ID, FAKE_STORAGE_SECRET);
    let mut harness = app_harness(app, WIDE);
    scroll_drawer(&mut harness, WIDE);
    snapshot(&mut harness, "session_settings_recording_typed");
    // The baseline proves the pixels are dots. This proves the same about the
    // accessibility tree, which is the other way a key gets read off a screen.
    for secret in [FAKE_STORAGE_ID, FAKE_STORAGE_SECRET] {
        let leaks = harness
            .query_all_by(move |node| {
                node.label().is_some_and(|l| l.contains(secret))
                    || node.value().is_some_and(|v| v.contains(secret))
            })
            .count();
        assert_eq!(
            leaks, 0,
            "the storage key reached the accessibility tree on {leaks} node(s)"
        );
    }
}

// The Broadcast tab: stream mix above destinations, both at the drawer's
// width. The frozen demo carries distinct broadcast fader values, so the mix
// rows show gain, pan, and a mute rather than four identical rows.

fn stream_mix_app(theme: Theme, audition: bool) -> JamApp {
    use jamstream_client::runtime::{Command, Runtime};
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    if audition {
        rt.send(Command::SetBroadcastAudition(true));
    }
    drawer_app(host_app(rt, theme), SettingsTab::Broadcast)
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
fn session_stream_mix_full_band() {
    // Ten musicians, which is the design maximum and the real test of the
    // mix rows at the drawer's width: ten names, ten gains, ten pans and ten
    // mutes, none of them truncated into uselessness or off the edge.
    let app = drawer_app(
        host_app(DemoRuntime::full(FROZEN_FRAME, true, true), Theme::Dark),
        SettingsTab::Broadcast,
    );
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_stream_mix_full_band");
}

#[test]
fn session_stream_mix_long_names() {
    // Names at the 64-character protocol cap in a cell 84 px wide: they
    // truncate, and the full name is on hover, the same treatment a strip
    // gives one.
    let app = drawer_app(
        host_app(DemoRuntime::long_names(FROZEN_FRAME, true), Theme::Dark),
        SettingsTab::Broadcast,
    );
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_stream_mix_long_names");
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

/// The host session with the Broadcast tab open: `saved` platforms have a key
/// on this computer, `reported` is what the server says each destination is
/// doing. The destinations section sits under the stream mix, so these
/// fixtures are scrolled to it, which is what a host looking at destinations
/// is actually seeing.
fn destinations_app(
    theme: Theme,
    saved: &[StreamPlatform],
    reported: &[(StreamPlatform, DestinationState)],
) -> JamApp {
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    rt.set_destinations(reported);
    let mut app = host_app(rt, theme);
    app.session.destinations = Some(DestinationsPanel::new(saved_keys(saved)));
    drawer_app(app, SettingsTab::Broadcast)
}

fn live(platform: StreamPlatform) -> (StreamPlatform, DestinationState) {
    (platform, DestinationState::Live)
}

#[test]
fn session_destinations() {
    // Nothing configured and no key anywhere: the empty state says what to do
    // next, and the on air lamp is dark.
    //
    // Scrolled, like every other destinations fixture. Unscrolled, this and its
    // light twin were pixel-identical to the two stream mix baselines, so the
    // reviewable empty state of Destinations had no image at all (#191).
    let app = destinations_app(Theme::Dark, &[], &[]);
    let mut harness = app_harness(app, WIDE);
    scroll_drawer(&mut harness, WIDE);
    snapshot(&mut harness, "session_destinations");
}

#[test]
fn session_destinations_light() {
    let app = destinations_app(Theme::Light, &[], &[]);
    let mut harness = app_harness(app, WIDE);
    scroll_drawer(&mut harness, WIDE);
    snapshot(&mut harness, "session_destinations_light");
}

#[test]
fn session_destinations_key() {
    // The one surface where a key exists: masked, with its character count
    // standing in for reading it back, and the platform's own guidance above.
    //
    // Publishable: this is a host who clicked Add key and typed one. The
    // keychain behind it is empty, which is a first-time host's real state,
    // and it is why Twitch still reads "no key" and Go live is still off.
    // Nothing else is stubbed: the tab row carries all four tabs and the bar
    // carries Record, because the fixture is a real host.
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    let mut app = host_app(rt, Theme::Dark);
    app.session.destinations = Some(DestinationsPanel::with_key_entry(
        Arc::new(MemStore::default()),
        StreamPlatform::Twitch,
        FAKE_KEY,
    ));
    let app = drawer_app(app, SettingsTab::Broadcast);
    let mut harness = app_harness(app, WIDE);
    // Scrolled to the key pane, which is where a host who just pressed Add
    // key is looking. Unscrolled, the field would be on screen and the Save
    // that sends it would not, which is not the state anyone is ever in.
    scroll_drawer(&mut harness, WIDE);
    snapshot_for_docs(&mut harness, "session_destinations_key");
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
    // Scrolled to the destinations themselves, so the published image shows
    // both live rows and the control that takes them off air rather than the
    // stream mix above them.
    let app = destinations_app(
        Theme::Dark,
        &[],
        &[live(StreamPlatform::Twitch), live(StreamPlatform::YouTube)],
    );
    let mut harness = app_harness(app, WIDE);
    scroll_drawer(&mut harness, WIDE);
    snapshot_for_docs(&mut harness, "session_destinations_live_two");
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
                    reason: "push failed: [flv @ 0x55d1c0a2f480] Failed to connect to \
                     rtmps://<redacted> Connection refused"
                        .to_owned(),
                },
            ),
        ],
    );
    let mut harness = app_harness(app, WIDE);
    scroll_drawer(&mut harness, WIDE);
    snapshot_for_docs(&mut harness, "session_destinations_failed");
}

#[test]
fn session_destinations_unavailable() {
    // The session has no broadcast relay, so there is nothing to stream
    // through: the reason is the server's own sentence, Add key and Go live
    // are both off, and one platform has a key on this computer, which is the
    // state where "Forget key" still has to work.
    //
    // Publishable: nothing here is stubbed that a real host would have. This
    // is a VM whose broadcast tooling never downloaded, which is what #440 is
    // about, and it is the only image that shows what that looks like.
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    rt.set_broadcast_readiness(Some(BroadcastReadiness::Unavailable {
        reason: "the broadcast tooling could not be downloaded".to_owned(),
    }));
    let mut app = host_app(rt, Theme::Dark);
    app.session.destinations = Some(DestinationsPanel::new(saved_keys(&[
        StreamPlatform::Twitch,
    ])));
    let app = drawer_app(app, SettingsTab::Broadcast);
    let mut harness = app_harness(app, WIDE);
    scroll_drawer(&mut harness, WIDE);
    snapshot_for_docs(&mut harness, "session_destinations_unavailable");
}

#[test]
fn session_destinations_narrow() {
    // The Broadcast tab in the smallest window the app opens, over a bar
    // that is on air: the drawer keeps its width, the sections stack inside
    // it, and nothing may overlap the readouts underneath.
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
    // Not a host, no drawer, no controls: a musician still sees the ON AIR
    // lamp in the centre of the bar, because a musician is the one being
    // broadcast.
    let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
    rt.set_destinations(&[live(StreamPlatform::Twitch), live(StreamPlatform::YouTube)]);
    let app = session_app(rt, Theme::Dark);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_on_air_musician");
}

// The bar's centre cluster, in the four combinations that matter. The lamps
// are the loudest thing in the bar when lit and absent when not, so these
// four are the fixtures the restructure lives or dies on.

#[test]
fn session_bar_on_air() {
    // On air and not recording: one lamp, centred.
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    rt.set_destinations(&[live(StreamPlatform::Twitch)]);
    let app = host_app(rt, Theme::Dark);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_bar_on_air");
}

#[test]
fn session_bar_both_lamps() {
    // Both: being broadcast and being recorded, side by side in the middle of
    // the bar, in their own colours. This is the state the cluster exists for.
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    rt.set_destinations(&[live(StreamPlatform::Twitch), live(StreamPlatform::YouTube)]);
    rt.set_record(RecordState::Recording, false);
    let app = host_app(rt, Theme::Dark);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_bar_both_lamps");
}

#[test]
fn session_bar_both_lamps_narrow() {
    // The same pair at the smallest window the app opens, which is where the
    // bar overflowed itself in #85.
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    rt.set_destinations(&[live(StreamPlatform::Twitch), live(StreamPlatform::YouTube)]);
    rt.set_record(RecordState::Recording, false);
    let app = host_app(rt, Theme::Dark);
    let mut harness = app_harness(app, NARROW);
    snapshot(&mut harness, "session_bar_both_lamps_narrow");
}

#[test]
fn session_bar_stream_failed() {
    // A destination stopped while a take runs: the failure is in the cluster
    // with the take, in the danger colour, and it says where the reason is.
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    rt.set_destinations(&[
        live(StreamPlatform::Twitch),
        (
            StreamPlatform::YouTube,
            DestinationState::Failed {
                reason: "push failed: [flv @ 0x55d1c0a2f480] Failed to connect to \
                     rtmps://<redacted> Connection refused"
                    .to_owned(),
            },
        ),
    ]);
    rt.set_record(RecordState::Uploading, false);
    let app = host_app(rt, Theme::Dark);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_bar_stream_failed");
}

// The record sheet and its lamp, in the states a take passes through. The
// demo recorder flips on command, so the in-between states are pinned the
// way destinations are; nothing here is a state the runtime cannot report.

/// The host session with the record sheet open and the recorder pinned to
/// `state`. `stems` is the launch-time choice the sheet reads back.
fn record_app(theme: Theme, state: RecordState, stems: bool) -> JamApp {
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    rt.set_record(state, stems);
    let mut app = host_app(rt, theme);
    app.session.record_open = true;
    app
}

#[test]
fn session_record() {
    // The sheet as a host first opens it: idle, mix only, Record enabled,
    // and the bar's lamp dark because nothing is being captured.
    let app = record_app(Theme::Dark, RecordState::Idle, false);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_record");
}

#[test]
fn session_record_light() {
    // Mid-take in the light palette: the lamp and the state word have to
    // read against the light surfaces too.
    let app = record_app(Theme::Light, RecordState::Recording, false);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_record_light");
}

#[test]
fn session_record_uploading() {
    // Stop has been pressed: the take is safe-in-progress, its own state,
    // neither done nor failed, and Record waits for the upload to finish.
    let app = record_app(Theme::Dark, RecordState::Uploading, true);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_record_uploading");
}

#[test]
fn session_record_failed() {
    // The recorder died and the sheet says why, verbatim, the discipline
    // the destinations sheet set for a dropped stream.
    let app = record_app(
        Theme::Dark,
        RecordState::Failed {
            reason: "multipart upload aborted: connection reset by peer".to_owned(),
        },
        false,
    );
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_record_failed");
}

#[test]
fn session_recording_musician() {
    // Not a host, no sheet, no button: a musician still sees the lit lamp
    // beside the readouts, because everyone in the room is on the take.
    let rt = DemoRuntime::frozen(FROZEN_FRAME, false);
    rt.set_record(RecordState::Recording, false);
    let app = session_app(rt, Theme::Dark);
    let mut harness = app_harness(app, WIDE);
    snapshot(&mut harness, "session_recording_musician");
}

#[test]
fn session_recording_narrow() {
    // The bar that once overlapped itself at 1280 (#85), now with the
    // record lamp lit and 480 fewer pixels: the readouts keep the first
    // row with the lamp, the controls take the second, nothing overlaps.
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    rt.set_record(RecordState::Recording, false);
    let app = host_app(rt, Theme::Dark);
    let mut harness = app_harness(app, NARROW);
    snapshot(&mut harness, "session_recording_narrow");
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
    region_step(theme, fixed_regions().into())
}

fn region_step(theme: Theme, survey: RegionSurvey) -> JamApp {
    let mut app = test_app(theme);
    let mut w = fixed_wizard();
    w.providers[1].status = ProviderStatus::Ready;
    w.select_provider(1);
    w.continue_to_region(survey);
    app.wizard = w;
    app.screen = Screen::HostWizard;
    app
}

/// Rows as the wizard really receives them: candidates and whatever probes
/// came back, put through the shared solver rather than hand-ordered. A
/// fixture that ordered the table itself could not show that an unmeasured
/// region sorts last, which is the whole point of these two.
fn ranked_regions(probed: &[(&str, u64, Option<f32>)]) -> Vec<RegionRow> {
    let candidates: Vec<(Region, Price)> = probed
        .iter()
        .map(|(id, hourly, _)| {
            (
                Region {
                    provider: ProviderKind::DigitalOcean,
                    id: RegionId::new(*id),
                    display: (*id).to_owned(),
                    country: "US".to_owned(),
                },
                Price {
                    hourly_microusd: *hourly,
                    egress_microusd_per_gb: 10_000,
                    included_egress_gb: 3000,
                },
            )
        })
        .collect();
    let mut matrix = ProbeMatrix::new();
    for (id, _, rtt) in probed {
        if let Some(rtt) = rtt {
            matrix.insert(0, RegionId::new(*id), *rtt);
        }
    }
    rank(&matrix, &candidates)
        .into_iter()
        .map(|score| RegionRow {
            region: score.region,
            price: score.price,
            worst_rtt_ms: score.worst_rtt_ms,
        })
        .collect()
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

/// Server artifacts that could not exist: a reserved domain and shas of
/// nothing but zeros. The fixture supplies them rather than the real
/// `option_env!` pins, so the baseline shows what a release build shows
/// without depending on how this build was compiled.
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

/// The cost preview as a release build shows it: the server binary is
/// pinned, so there is one line about it and nothing to configure. This is
/// the path every user of a release is on, and the published screenshot.
fn wizard_preview_app(theme: Theme) -> JamApp {
    let mut app = wizard_preview_unpinned_app(theme);
    app.wizard.pinned = FAKE_PINS;
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
    w.pinned = jamstream_cloud::PinnedServerArtifacts::default();
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

/// [`wizard_snapshot`] for a baseline the docs site publishes. See
/// [`snapshot_for_docs`] for what that claims.
fn wizard_snapshot_for_docs(app: JamApp, name: &str) {
    let mut harness = app_harness(app, WIDE);
    snapshot_for_docs(&mut harness, name);
}

#[test]
fn wizard_provider() {
    wizard_snapshot_for_docs(wizard_provider_app(Theme::Dark), "wizard_provider");
}

#[test]
fn wizard_provider_light() {
    wizard_snapshot(wizard_provider_app(Theme::Light), "wizard_provider_light");
}

#[test]
fn wizard_setup_digitalocean() {
    wizard_snapshot_for_docs(wizard_setup_app(Theme::Dark), "wizard_setup_digitalocean");
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
    wizard_snapshot_for_docs(wizard_region_app(Theme::Dark), "wizard_region");
}

/// One region the probe never reached, and one the account cannot run this
/// machine size in. Both facts are absent for different reasons and the
/// table says so differently: atl1 stays, reads `no probe`, and sits at the
/// bottom despite being the cheapest row; blr1 is gone with a line naming it.
/// An unmeasured region must never read `0 ms` and sort first.
#[test]
fn wizard_region_unmeasured() {
    let survey = RegionSurvey {
        rows: ranked_regions(&[
            ("nyc3", 26_790, Some(21.0)),
            ("sfo3", 26_790, Some(74.0)),
            ("atl1", 9_000, None),
        ]),
        unavailable: vec!["blr1".to_owned()],
    };
    let app = region_step(Theme::Dark, survey);
    assert!(
        !app.wizard.regions.last().expect("rows").measured(),
        "the unmeasured region must be the last row for this fixture to mean anything"
    );
    wizard_snapshot(app, "wizard_region_unmeasured");
}

/// Nothing answered at all, which is what the host actually saw. Every row
/// reads `no probe`, the order is price and says so, and no region is
/// preselected: the previous behaviour rendered eight `0 ms` rows and
/// preselected one of them.
#[test]
fn wizard_region_no_probes() {
    let survey: RegionSurvey = ranked_regions(&[
        ("nyc3", 26_790, None),
        ("sfo3", 26_790, None),
        ("atl1", 9_000, None),
    ])
    .into();
    let app = region_step(Theme::Dark, survey);
    assert!(app.wizard.nothing_measured());
    assert_eq!(app.wizard.selected_region, None);
    wizard_snapshot(app, "wizard_region_no_probes");
}

#[test]
fn wizard_region_light() {
    wizard_snapshot(wizard_region_app(Theme::Light), "wizard_region_light");
}

#[test]
fn wizard_preview() {
    wizard_snapshot_for_docs(wizard_preview_app(Theme::Dark), "wizard_preview");
}

/// The preview step with recording armed on a computer that has a bucket: the
/// mix and stems row selected, the bucket named under it, and the take's two
/// cost lines folded into the session's own. This is the fixture that shows the
/// five times difference stems make, because both sizes are on screen beside the
/// rows and the total moved when the row was picked.
fn wizard_preview_recording_app(theme: Theme) -> JamApp {
    // Configured the way the app configures itself: the bucket in the Recording
    // tab's preferences and the key in the keychain behind it. The wizard reads
    // that through the app every frame, so a fixture that set the wizard's own
    // copy would be overwritten before it drew.
    let (store, env, mut app) = configured_app(theme, true);
    let mut w = HostWizard::new(store, env, Arc::new(Executor::new()));
    w.providers[1].status = ProviderStatus::Ready;
    w.select_provider(1);
    w.continue_to_region(fixed_regions());
    w.continue_to_preview();
    w.pinned = FAKE_PINS;
    app.wizard = w;
    app.screen = Screen::HostWizard;
    let setup = app.recording.setup(Some(ProviderKind::DigitalOcean));
    assert_eq!(setup.refusal(), None, "the fixture must be able to record");
    app.wizard.set_recording_setup(setup);
    assert!(
        app.wizard.set_recording(RecordingChoice::MixAndStems),
        "a configured computer must be able to arm a take"
    );
    app
}

#[test]
fn wizard_preview_recording() {
    wizard_snapshot_for_docs(
        wizard_preview_recording_app(Theme::Dark),
        "wizard_preview_recording",
    );
}

#[test]
fn wizard_preview_recording_light() {
    wizard_snapshot(
        wizard_preview_recording_app(Theme::Light),
        "wizard_preview_recording_light",
    );
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

// The wizard at the smallest window the app opens. The preview step already
// had one, because it is the tallest card; these are the other three, and the
// question each asks is the same one: does the actions row keep the card's
// bottom edge instead of sitting past the window's (#191).

#[test]
fn wizard_provider_narrow() {
    let mut harness = app_harness(wizard_provider_app(Theme::Dark), NARROW);
    snapshot(&mut harness, "wizard_provider_narrow");
}

#[test]
fn wizard_setup_digitalocean_narrow() {
    let mut harness = app_harness(wizard_setup_app(Theme::Dark), NARROW);
    snapshot(&mut harness, "wizard_setup_digitalocean_narrow");
}

#[test]
fn wizard_region_narrow() {
    let mut harness = app_harness(wizard_region_app(Theme::Dark), NARROW);
    snapshot(&mut harness, "wizard_region_narrow");
}

#[test]
fn wizard_launching_narrow() {
    let mut harness = app_harness(wizard_launching_app(Theme::Dark), NARROW);
    snapshot(&mut harness, "wizard_launching_narrow");
}

#[test]
fn wizard_launching() {
    wizard_snapshot(wizard_launching_app(Theme::Dark), "wizard_launching");
}

/// A launch that failed, which is the state a real one reaches often enough:
/// the reason, what to do about a machine that may already be up, and one way
/// out. Cloud rather than local, because the stray-machine line only applies
/// there and it is the half that costs money.
#[test]
fn wizard_launch_failed() {
    let mut app = test_app(Theme::Dark);
    let mut w = fixed_wizard();
    w.select_provider(1); // digitalocean, so the stray-machine note applies
    w.advance_from_provider();
    w.regions = fixed_regions();
    w.selected_region = Some(0);
    w.step = jamstream_client::screens::host::WizardStep::Launching;
    w.launch_error =
        Some("server did not complete a handshake within 60 s on 203.0.113.7:43210".to_owned());
    app.wizard = w;
    app.screen = Screen::HostWizard;
    wizard_snapshot(app, "wizard_launch_failed");
}

#[test]
fn wizard_launching_light() {
    wizard_snapshot(wizard_launching_app(Theme::Light), "wizard_launching_light");
}

// The invites panel over the host session, fed from a real decodable state
// record so labels and statuses come through the production path.

/// A state record with a working issuer key.
///
/// The key matters. `Mint invite` and `New link` sign with
/// `state.issuer_private_key_b64`, so an empty one leaves both buttons erroring
/// and `session_invites.png` a published screenshot of two broken controls. It
/// is the fixture's own issuer, the same one that minted the seats, and
/// `interactions.rs::minting_and_refilling_a_seat_work_in_the_fixtures_state`
/// presses both.
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
            // The demo roster's own token scheme, so a revoke sent to the
            // panel and to the runtime lands on the same person.
            jti: TokenId([member as u8; 16]),
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
        issuer_private_key_b64: data_encoding::BASE64.encode(&issuer.to_bytes()),
        server_public_key_b64: data_encoding::BASE64.encode(&server_pk),
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
    // Per call, not per process. Revoking rewrites this file, and under
    // libtest, which is what docs-check.yml runs, the three session_invites
    // fixtures are threads in one process: sharing a pid-scoped path meant
    // three tests revoking into the same record, so a committed docs
    // screenshot could render from a record another test had just rewritten,
    // with the job green. The counter makes every panel its own file.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = private_temp_dir(&format!(
        "jamstream-snapshot-invites-{}",
        std::process::id()
    ));
    (state, dir.join(format!("{n}.json")))
}

/// The host who has just revoked one musician, so every row state is on
/// screen at once: connected, free, and not joined.
///
/// The revoke goes to the runtime as well as to the panel, which is what a
/// click does. Freeing the seat in the panel alone would leave the mixer
/// showing a strip for someone the panel says is gone, and a screenshot of
/// two halves disagreeing is exactly what the docs gate exists to stop.
fn session_invites_app(theme: Theme) -> JamApp {
    use jamstream_client::runtime::Runtime;
    let rt = DemoRuntime::frozen(FROZEN_FRAME, true);
    let (state, path) = invites_state();
    let mut panel = InvitesPanel::new(state, path);
    let member = jamstream_protocol::ids::MemberId(2);
    let revoked = panel.token_of(member).expect("musician 2 holds an invite");
    rt.send(jamstream_client::runtime::Command::Revoke(revoked));
    panel.revoke(revoked, Some("Ben".to_owned()));
    let mut app = host_app(rt, theme);
    app.session.invites = Some(panel);
    drawer_app(app, SettingsTab::Invites)
}

#[test]
fn session_invites() {
    let mut harness = app_harness(session_invites_app(Theme::Dark), WIDE);
    snapshot_for_docs(&mut harness, "session_invites");
}

#[test]
fn session_invites_light() {
    let mut harness = app_harness(session_invites_app(Theme::Light), WIDE);
    snapshot(&mut harness, "session_invites_light");
}

#[test]
fn session_invites_narrow() {
    // The Invites tab at the smallest window: every seat's two lines and its
    // own actions inside the drawer's width, with the bar clear underneath.
    let mut harness = app_harness(session_invites_app(Theme::Dark), NARROW);
    snapshot(&mut harness, "session_invites_narrow");
}
