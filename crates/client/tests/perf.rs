//! Frame-time measurement at the design maximum: a full session (10
//! musicians, 10 listeners) with every meter animating. egui repaints the
//! whole screen each frame, so this is the realistic worst case. The debug
//! number is printed honestly; run with `--nocapture` to see it.

use std::sync::Arc;
use std::time::Instant;

use egui::vec2;
use egui_kittest::Harness;
use jamstream_client::demo::DemoRuntime;
use jamstream_client::runtime::Runtime;
use jamstream_client::screens::session::SessionScreen;
use jamstream_client::theme::{self, Theme};

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
