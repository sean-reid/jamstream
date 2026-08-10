//! Frame cost at the design maximum: a full session (10 musicians, 10
//! listeners) with every meter animating. egui repaints the whole screen
//! each frame, so this is the realistic worst case. Both numbers below are
//! published on every run through `.config/nextest.toml`.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{budget_scale, frame_budget_ms, frame_costs_ms};
use egui::vec2;
use egui_kittest::Harness;
use jamstream_client::app::{JamApp, Screen};
use jamstream_client::demo::DemoRuntime;
use jamstream_client::runtime::{Command, ConnState, Runtime, Snapshot};
use jamstream_client::screens::session::{SessionScreen, SettingsTab};
use jamstream_client::theme::{self, Theme};

// Allocations made on this thread, counted by the allocator below. Thread
// local because the test binary runs its tests in parallel and a global
// counter would bill one test for another's work.
thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

/// The system allocator with a tally in front of it. A `const`-initialised
/// `Cell` has no destructor to register, so the count itself allocates
/// nothing and cannot recurse.
struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

fn allocations(f: impl FnOnce()) -> u64 {
    let before = ALLOCS.with(Cell::get);
    f();
    ALLOCS.with(Cell::get) - before
}

/// Counts what one frame asks of the runtime. Wraps the demo so the app
/// draws a real session behind the tally, and clones by handle so the test
/// can read the counts off the same runtime the app is holding.
struct CountingRuntime<R: Runtime> {
    inner: Arc<Counts<R>>,
}

struct Counts<R: Runtime> {
    inner: R,
    snapshots: AtomicUsize,
    conn_states: AtomicUsize,
}

impl<R: Runtime> Clone for CountingRuntime<R> {
    fn clone(&self) -> Self {
        CountingRuntime {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<R: Runtime> CountingRuntime<R> {
    fn new(inner: R) -> Self {
        CountingRuntime {
            inner: Arc::new(Counts {
                inner,
                snapshots: AtomicUsize::new(0),
                conn_states: AtomicUsize::new(0),
            }),
        }
    }

    fn snapshots(&self) -> usize {
        self.inner.snapshots.load(Ordering::Relaxed)
    }

    fn conn_states(&self) -> usize {
        self.inner.conn_states.load(Ordering::Relaxed)
    }
}

impl<R: Runtime + Sync> Runtime for CountingRuntime<R> {
    fn snapshot(&self) -> Snapshot {
        self.inner.snapshots.fetch_add(1, Ordering::Relaxed);
        self.inner.inner.snapshot()
    }

    fn send(&self, cmd: Command) {
        self.inner.inner.send(cmd);
    }

    /// Delegated rather than left to the trait's default, which would go
    /// through `snapshot` and hide the very thing this counts.
    fn conn_state(&self) -> ConnState {
        self.inner.conn_states.fetch_add(1, Ordering::Relaxed);
        self.inner.inner.conn_state()
    }
}

/// A host's session with the chat buffer as long as the runtime lets it
/// get, because the chat lines are two `String`s each and the biggest part
/// of what a snapshot copies.
fn busy_session() -> DemoRuntime {
    let rt = DemoRuntime::full(0, true, false);
    for i in 0..500 {
        rt.send(Command::SendChat(format!("bar {i}, from the top")));
    }
    rt
}

/// The app as eframe drives it: the two things `logic` does, then the
/// frame's layout, all on the app's own code.
fn app_harness(rt: CountingRuntime<DemoRuntime>, tab: SettingsTab) -> Harness<'static> {
    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.runtime = Some(Box::new(rt));
    app.screen = Screen::Session;
    app.settings_open = true;
    app.settings_tab = tab;
    Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            app.repaint_while_animating(ui.ctx());
            app.fall_back_when_idle();
            app.root_ui(ui);
        })
}

/// What one frame of the fullest session may cost on a quiet laptop, in
/// milliseconds. 16 ms is the 60 fps frame, where the product stops being
/// smooth; this is where a regression is worth hearing about, and the runner
/// multiplier puts a shared runner back at 16.
///
/// Measured on a 14-core laptop, 25 runs quiet and 10 against 14 busy cores.
/// Quiet: median 0.33 to 0.34 ms, p99 up to 0.61 ms. Saturated: median 0.46 to
/// 0.54 ms, p99 up to 5.04 ms, max up to 9.91 ms. The median moved 1.6x and the
/// tail moved 19x, so the gate is 7x above the worst median measured on a
/// machine with no idle core.
const LAPTOP_FRAME_MS: f64 = 4.0;

#[test]
fn full_session_frame_time() {
    let budget_ms = frame_budget_ms(LAPTOP_FRAME_MS);
    // Animating (not frozen): the frame counter advances every snapshot,
    // so meters, cost, and stats all change per frame.
    let rt = Arc::new(DemoRuntime::full(0, true, false));
    let mut screen = SessionScreen::default();
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .build_ui(move |ui| {
            theme::apply(ui.ctx(), Theme::Dark);
            let snap = rt.snapshot();
            screen.ui(ui, &snap, &*rt);
        });

    harness.run_steps(10);
    const FRAMES: u32 = 300;
    let mut costs: Vec<Duration> = Vec::with_capacity(FRAMES as usize);
    for _ in 0..FRAMES {
        let at = Instant::now();
        harness.step();
        costs.push(at.elapsed());
    }
    costs.sort_unstable();
    let (median, p99, max) = frame_costs_ms(&costs);
    println!(
        "session_full: median {median:.2} ms/frame, p99 {p99:.2} ms, max {max:.2} ms \
         over {FRAMES} frames (layout + tessellation, no gpu); the median is \
         {:.0}% of the {budget_ms:.1} ms budget on this machine",
        100.0 * median / budget_ms
    );
    // The median and not the p99, because this test shares the machine with the
    // rest of the suite: on a saturated 14-core laptop the median moves 1.6x
    // while a single frame reaches 19x. The tail is published rather than gated,
    // so a drift toward the wall is readable on a passing run.
    assert!(
        median < budget_ms,
        "frame time {median:.2} ms at the median, over the {budget_ms:.1} ms budget \
         (p99 {p99:.2} ms, max {max:.2} ms)"
    );
}

/// One snapshot per frame, whatever is on screen: a pull copies the roster, the
/// chat buffer, and the destinations out from under the network thread's lock,
/// so the session screen, the drawer's tab list and the drawer's body share the
/// one the frame took.
#[test]
fn the_frame_pulls_one_snapshot() {
    for tab in [
        SettingsTab::Audio,
        SettingsTab::Broadcast,
        SettingsTab::Invites,
        SettingsTab::Recording,
        SettingsTab::You,
    ] {
        let rt = CountingRuntime::new(DemoRuntime::full(0, true, false));
        let mut harness = app_harness(rt.clone(), tab);
        // egui runs a pass more than once for a frame it discards, so the
        // passes are counted rather than assumed from the step count.
        harness.run_steps(4);
        let passes = harness.ctx.cumulative_pass_nr();
        assert_eq!(
            rt.snapshots(),
            passes as usize,
            "{tab:?}: one snapshot per pass, not {} over {passes}",
            rt.snapshots()
        );
        assert_eq!(
            rt.conn_states(),
            passes as usize,
            "{tab:?}: the idle check asks for the state, not a snapshot"
        );
    }
}

/// The settings drawer is not by itself a reason to run at display rate. Its
/// one moving part is the Audio tab's input meter, which draws off a snapshot,
/// so an open drawer on Home would pin the app at display rate redrawing
/// permanent zeros.
///
/// Observed where the frame loop observes it: the callback egui calls to
/// wake a sleeping integration, on a context of this test's own.
#[test]
fn an_open_drawer_repaints_only_with_a_session_behind_it() {
    fn wakes(app: &JamApp) -> usize {
        let ctx = egui::Context::default();
        let woken = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&woken);
        ctx.set_request_repaint_callback(move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        app.repaint_while_animating(&ctx);
        woken.load(Ordering::Relaxed)
    }

    let mut app = JamApp::in_memory();
    app.recent = Vec::new();
    app.settings_open = true;
    assert_eq!(wakes(&app), 0, "the drawer on Home animates nothing");

    // The meter's source, and the drawer starts moving with it.
    app.runtime = Some(Box::new(CountingRuntime::new(DemoRuntime::full(
        0, true, false,
    ))));
    assert_eq!(
        wakes(&app),
        1,
        "levels are arriving; the meter has to be redrawn"
    );

    // And the session screen, which animates whatever the drawer is doing.
    app.screen = Screen::Session;
    app.settings_open = false;
    assert_eq!(
        wakes(&app),
        1,
        "a session animates with or without the drawer"
    );
}

/// What a frame of the busiest screen costs in allocations. A count and not a
/// timing, so it is the same number on any machine and takes no runner
/// multiplier: 3961 on ten consecutive runs here, against a load average
/// spanning 2 to 50. It is also the tightest gate in the workspace, at 79
/// percent of its budget, which is why `.config/nextest.toml` publishes the
/// number on every run: a budget nobody can see the distance to cannot be
/// calibrated.
#[test]
fn full_session_frame_allocations() {
    let rt = CountingRuntime::new(busy_session());
    let mut harness = app_harness(rt.clone(), SettingsTab::Broadcast);
    // Fonts, textures, and the layout cache are first-frame costs; they are
    // warmed out of the measurement.
    harness.run_steps(10);
    const FRAMES: u32 = 60;
    let allocs = allocations(|| {
        for _ in 0..FRAMES {
            harness.step();
        }
    });
    let per_frame = allocs / u64::from(FRAMES);
    // A pull of this session is worth about 1100 allocations, so the budget is
    // set where a second one fails.
    const BUDGET: u64 = 5_000;
    println!(
        "session_drawer: {per_frame} allocations/frame over {FRAMES} frames \
         (host, 20 in the room, 506 chat lines, drawer on Broadcast); \
         {:.0}% of the {BUDGET} budget",
        100.0 * per_frame as f64 / BUDGET as f64
    );
    assert!(
        per_frame < BUDGET,
        "{per_frame} allocations/frame is over the {BUDGET} budget"
    );
}

/// The runner is described once, by the variable every workflow sets, and a
/// budget can only ever get longer from it. A missing or nonsense value has to
/// leave the laptop budget alone rather than collapse to zero.
#[test]
fn a_frame_budget_scales_with_the_runner_and_never_shrinks() {
    assert_eq!(budget_scale(None), 1.0, "unset is the laptop budget");
    // What CI sets: 120 s against the harness's 30 s reference run.
    assert_eq!(budget_scale(Some("120")), 4.0);
    assert_eq!(budget_scale(Some("45")), 1.5);
    for nonsense in ["0", "-30", "", "soon", "NaN", "inf"] {
        assert_eq!(
            budget_scale(Some(nonsense)),
            1.0,
            "{nonsense:?} must not shorten a budget"
        );
    }
    assert!(frame_budget_ms(LAPTOP_FRAME_MS) >= LAPTOP_FRAME_MS);
}

/// Both measurements above only reach a log on a passing run because
/// `.config/nextest.toml` names these tests for publishing, and filters there
/// are exact matches: a rename has to land in both places or in neither. Same
/// pairing the harness, session, server and broadcast suites keep.
#[test]
fn the_measured_tests_are_named_in_the_nextest_config() {
    const CONFIG: &str = include_str!("../../../.config/nextest.toml");
    for (name, _) in [
        (
            stringify!(full_session_frame_time),
            full_session_frame_time as fn(),
        ),
        (
            stringify!(full_session_frame_allocations),
            full_session_frame_allocations as fn(),
        ),
    ] {
        assert!(
            CONFIG.contains(&format!("test(={name})")),
            ".config/nextest.toml no longer names {name}, so what it measures is \
             being printed into a void"
        );
    }
}
