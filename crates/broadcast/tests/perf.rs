//! Frame-time gate for the worst scene we ship: 10 carded musicians. The
//! budget is what one frame may cost on a quiet laptop and takes its
//! multiplier from JAMSTREAM_PERF_BUDGET_SECS, the one variable every workflow
//! sets.

mod common;

use std::time::{Duration, Instant};

use common::{H, W, budget_scale, frame_budget_ms, frame_costs_ms, roster};
use jamstream_broadcast::{Renderer, SceneConfig};

/// What one frame of the fullest scene may cost on a quiet laptop, in
/// milliseconds. The renderer feeds a 30 fps encoder, so 33 ms is where the
/// product breaks; this is where a regression is worth hearing about, which is
/// two orders of magnitude sooner.
///
/// Measured on a 14-core laptop, 15 runs quiet and 10 against 14 busy cores.
/// Quiet: median 0.065 ms every run but one, p99 0.076 to 0.152 ms. Saturated:
/// median 0.105 to 0.115 ms, p99 up to 1.085 ms, max up to 2.730 ms. The median
/// moved 1.7x and the tail moved 36x, so the gate is 9x above the worst median
/// measured on a machine with no idle core, and the 4x runner multiplier is on
/// top of that.
const LAPTOP_FRAME_MS: f64 = 1.0;

#[test]
fn ten_member_scene_meets_frame_budget() {
    let budget_ms = frame_budget_ms(LAPTOP_FRAME_MS);

    let mut r = Renderer::new(SceneConfig::default());
    let mut members = roster(10);
    let mut out = vec![0u8; (W * H * 4) as usize];
    r.render(0, &members, 25, &mut out);

    const FRAMES: u64 = 300;
    let mut costs: Vec<Duration> = Vec::with_capacity(FRAMES as usize);
    for f in 1..=FRAMES {
        for (i, m) in members.iter_mut().enumerate() {
            let v = ((f * 17 + i as u64 * 11) % 100) as f32 / 100.0;
            m.level_peak = v;
            m.level_rms = v * 0.55;
        }
        let at = Instant::now();
        r.render(f, &members, 25, &mut out);
        costs.push(at.elapsed());
    }
    costs.sort_unstable();
    let (median, p99, max) = frame_costs_ms(&costs);
    println!(
        "10-member 1280x720 scene: median {median:.3} ms/frame, p99 {p99:.3} ms, \
         max {max:.3} ms over {FRAMES} frames; the median is {:.0}% of the \
         {budget_ms:.2} ms budget on this machine",
        100.0 * median / budget_ms
    );
    // The median and not the p99, because this test shares the machine with
    // the rest of the suite and a tail measured beside a hundred cpu-bound
    // tests records the scheduler. The tail is published rather than gated, so
    // a drift toward the wall is readable on a passing run.
    assert!(
        median < budget_ms,
        "{median:.3} ms/frame at the median, over the {budget_ms:.2} ms budget \
         (p99 {p99:.3} ms, max {max:.3} ms)"
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

/// The measurement above only reaches a log on a passing run because
/// `.config/nextest.toml` names this test for publishing, and filters there are
/// exact matches: a rename has to land in both places or in neither. Same
/// pairing the harness, session and server suites keep for their measurements.
#[test]
fn the_measured_tests_are_named_in_the_nextest_config() {
    const CONFIG: &str = include_str!("../../../.config/nextest.toml");
    let (name, _) = (
        stringify!(ten_member_scene_meets_frame_budget),
        ten_member_scene_meets_frame_budget as fn(),
    );
    assert!(
        CONFIG.contains(&format!("test(={name})")),
        ".config/nextest.toml no longer names {name}, so the frame cost it \
         measures is being printed into a void"
    );
}
