//! Frame-time gate for the worst scene we ship: 10 carded musicians. The
//! default budget is the 30 fps frame (33 ms) in debug; slow CI runners
//! can widen it with JAMSTREAM_PERF_BUDGET_MS.

mod common;

use std::time::Instant;

use common::{H, W, roster};
use jamstream_broadcast::{Renderer, SceneConfig};

#[test]
fn ten_member_scene_meets_frame_budget() {
    let budget_ms: f64 = std::env::var("JAMSTREAM_PERF_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(33.0);

    let mut r = Renderer::new(SceneConfig::default());
    let mut members = roster(10);
    let mut out = vec![0u8; (W * H * 4) as usize];
    r.render(0, &members, 25, &mut out);

    const FRAMES: u64 = 300;
    let start = Instant::now();
    for f in 1..=FRAMES {
        for (i, m) in members.iter_mut().enumerate() {
            let v = ((f * 17 + i as u64 * 11) % 100) as f32 / 100.0;
            m.level_peak = v;
            m.level_rms = v * 0.55;
        }
        r.render(f, &members, 25, &mut out);
    }
    let per_frame_ms = start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;
    println!("perf: 10-member 1280x720 scene, {per_frame_ms:.3} ms/frame over {FRAMES} frames");
    assert!(
        per_frame_ms < budget_ms,
        "{per_frame_ms:.3} ms/frame over the {budget_ms} ms budget \
         (override with JAMSTREAM_PERF_BUDGET_MS)"
    );
}
