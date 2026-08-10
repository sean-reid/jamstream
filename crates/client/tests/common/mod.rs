//! Shared rig for the client's integration tests: the one runner multiplier
//! the workspace scales its budgets by, and the per-frame statistics the perf
//! gates publish.

#![allow(dead_code)]

use std::time::Duration;

/// What the budgets in this suite are worth on a quiet developer laptop, which
/// is what `JAMSTREAM_PERF_BUDGET_SECS` is measured against in the harness. One
/// variable describes the runner for the whole workspace, and this is the same
/// reference `crates/server/tests/common/mod.rs` and
/// `crates/cli/tests/common/mod.rs` use.
const REFERENCE_LAPTOP_SECS: f64 = 30.0;

/// The multiplier `JAMSTREAM_PERF_BUDGET_SECS` names, never below 1, so an
/// unset or nonsense value can only be generous and never shorten a budget.
pub fn budget_scale(value: Option<&str>) -> f64 {
    value
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map_or(1.0, |v| v / REFERENCE_LAPTOP_SECS)
        .max(1.0)
}

/// A per-frame budget in milliseconds, scaled for the machine running the
/// suite. A machine that takes four times as long over a whole suite takes four
/// times as long over one frame, so a frame budget takes the same multiplier as
/// a wall-clock deadline. CI sets 120, which is 4x, against runners measured
/// 3.7x slower than a quiet laptop.
pub fn frame_budget_ms(laptop_ms: f64) -> f64 {
    laptop_ms * budget_scale(std::env::var("JAMSTREAM_PERF_BUDGET_SECS").ok().as_deref())
}

/// Median, p99 and max of a sorted list of per-frame costs, in milliseconds.
pub fn frame_costs_ms(sorted: &[Duration]) -> (f64, f64, f64) {
    assert!(!sorted.is_empty(), "no frames were timed");
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    (
        ms(sorted[sorted.len() / 2]),
        ms(sorted[sorted.len() * 99 / 100]),
        ms(sorted[sorted.len() - 1]),
    )
}
