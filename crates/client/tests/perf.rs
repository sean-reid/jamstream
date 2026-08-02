//! Frame cost at the design maximum: a full session (10 musicians, 10
//! listeners) with every meter animating. egui repaints the whole screen
//! each frame, so this is the realistic worst case. The debug numbers are
//! printed honestly; run with `--nocapture` to see them.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

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

#[test]
fn full_session_frame_time() {
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
    let start = Instant::now();
    for _ in 0..FRAMES {
        harness.step();
    }
    let elapsed = start.elapsed();
    let per_frame_ms = elapsed.as_secs_f64() * 1000.0 / f64::from(FRAMES);
    println!(
        "session_full: {per_frame_ms:.2} ms/frame over {FRAMES} frames (debug build, layout + tessellation, no gpu)"
    );
    // 60 fps equivalent with headroom, in an unoptimized debug build.
    assert!(
        per_frame_ms < 16.0,
        "frame time {per_frame_ms:.2} ms exceeds the 16 ms budget in debug"
    );
}

/// One snapshot per frame, whatever is on screen. A pull copies the roster,
/// the chat buffer, and the destinations out from under the network
/// thread's lock, and the session screen, the drawer's tab list and the
/// drawer's body used to take one each (#382).
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

/// The settings drawer is not by itself a reason to run at display rate.
/// Its one moving part is the Audio tab's input meter, which draws off a
/// snapshot, so an open drawer on Home used to pin the app at display rate
/// redrawing permanent zeros (#383).
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

/// What a frame of the busiest screen costs in allocations. The number is
/// printed on every run, because a budget nobody can see the distance to
/// cannot be calibrated.
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
    println!(
        "session_drawer: {per_frame} allocations/frame over {FRAMES} frames (host, 20 in the room, 506 chat lines, drawer on Broadcast)"
    );
    // 3933 on the machine this was calibrated on, against 7187 for the four
    // pulls a frame this used to take. A pull of this session is worth about
    // 1100 allocations, so the budget is set where one coming back fails.
    assert!(
        per_frame < 5000,
        "{per_frame} allocations/frame is over the 5000 budget"
    );
}
