//! Behavioral and statistical tests for the simulation substrate.
//!
//! Statistical assertions use fixed seeds, so tolerances only need to cover
//! the sampling noise of that one deterministic run, not re-roll flakiness.

use jamstream_harness::{
    Delivery, EndpointId, Profile, SimNet, SkewedClock, VirtualClock, profiles,
};

const A: EndpointId = EndpointId(1);
const B: EndpointId = EndpointId(2);

fn custom(
    one_way_ms: f32,
    jitter_ms: f32,
    loss: f32,
    reorder_extra_ms: f32,
    reorder_prob: f32,
    dup_prob: f32,
) -> Profile {
    Profile {
        name: "custom".into(),
        one_way_ms,
        jitter_ms,
        loss,
        reorder_extra_ms,
        reorder_prob,
        dup_prob,
    }
}

/// Runs a fixed bidirectional send schedule over hostile-wifi, polling every
/// 2 ms, and records each delivery with the poll step it surfaced in.
fn run_schedule(seed: u64) -> Vec<(u64, u16, u16, Vec<u8>)> {
    let mut net = SimNet::new(seed);
    net.link(A, B, profiles::profile("hostile-wifi"));
    let mut clock = VirtualClock::new();
    let mut out = Vec::new();
    let mut record = |now_us: u64, deliveries: Vec<Delivery>| {
        for d in deliveries {
            out.push((now_us, d.from.0, d.to.0, d.payload));
        }
    };
    for i in 0u32..500 {
        net.send(clock.now_us(), A, B, i.to_le_bytes().to_vec());
        net.send(clock.now_us(), B, A, (1_000 + i).to_le_bytes().to_vec());
        clock.advance_us(2_000);
        let deliveries = net.poll(clock.now_us());
        record(clock.now_us(), deliveries);
    }
    clock.advance_us(1_000_000);
    let deliveries = net.poll(clock.now_us());
    record(clock.now_us(), deliveries);
    out
}

#[test]
fn same_seed_same_deliveries() {
    assert_eq!(run_schedule(42), run_schedule(42));
}

#[test]
fn different_seed_different_deliveries() {
    assert_ne!(run_schedule(42), run_schedule(43));
}

#[test]
fn regional_fiber_latency_matches_profile() {
    let mut net = SimNet::new(7);
    let p = profiles::profile("regional-fiber");
    net.link(A, B, p);
    for i in 0..10_000u64 {
        net.send(i * 1_000, A, B, vec![0]);
    }
    net.poll(u64::MAX);
    let stats = net.link_stats(A, B).unwrap();
    let base_us = f64::from(p.one_way_ms) * 1_000.0;
    let mean = stats.delay_mean_us().unwrap();
    assert!(
        (mean - base_us).abs() / base_us <= 0.15,
        "mean delay {mean} us not within 15% of {base_us} us"
    );
    assert!(stats.delay_min_us().unwrap() >= base_us as u64);
}

#[test]
fn loss_ratio_tracks_configured() {
    let mut net = SimNet::new(11);
    net.link(A, B, &custom(5.0, 0.0, 0.02, 0.0, 0.0, 0.0));
    for i in 0..20_000u64 {
        net.send(i * 100, A, B, vec![0]);
    }
    net.poll(u64::MAX);
    let stats = net.link_stats(A, B).unwrap();
    let observed = stats.dropped as f64 / stats.sent as f64;
    assert!(
        (observed - 0.02).abs() / 0.02 <= 0.30,
        "observed loss {observed} not within 30% relative of 0.02"
    );
    assert_eq!(stats.delivered + stats.dropped, stats.sent);
}

#[test]
fn duplication_delivers_extra_copies() {
    let mut net = SimNet::new(13);
    net.link(A, B, &custom(5.0, 1.0, 0.0, 0.0, 0.0, 0.1));
    for i in 0..10_000u64 {
        net.send(i * 100, A, B, vec![1]);
    }
    net.poll(u64::MAX);
    let stats = net.link_stats(A, B).unwrap();
    let expected = 10_000.0 * 0.1;
    assert!(
        (stats.duplicated as f64 - expected).abs() / expected <= 0.30,
        "duplicated {} not within 30% relative of {expected}",
        stats.duplicated
    );
    assert_eq!(stats.delivered, stats.sent + stats.duplicated);
}

#[test]
fn reordering_inverts_at_least_one_pair() {
    let mut net = SimNet::new(17);
    // Zero jitter isolates the reorder mechanism: only reorder_extra_ms can
    // push a packet past its successors.
    net.link(A, B, &custom(5.0, 0.0, 0.0, 20.0, 0.2, 0.0));
    for i in 0u32..1_000 {
        net.send(u64::from(i) * 1_000, A, B, i.to_le_bytes().to_vec());
    }
    let seqs: Vec<u32> = net
        .poll(u64::MAX)
        .iter()
        .map(|d| u32::from_le_bytes(d.payload[..4].try_into().unwrap()))
        .collect();
    assert_eq!(seqs.len(), 1_000);
    assert!(
        seqs.windows(2).any(|w| w[0] > w[1]),
        "expected at least one inverted pair"
    );
}

#[test]
fn fifo_holds_without_jitter_or_reorder() {
    let mut net = SimNet::new(19);
    net.link(A, B, &custom(5.0, 0.0, 0.0, 0.0, 0.0, 0.0));
    // Four packets per send instant exercises the seq tiebreak too.
    for i in 0u32..1_000 {
        net.send(u64::from(i / 4) * 1_000, A, B, i.to_le_bytes().to_vec());
    }
    let seqs: Vec<u32> = net
        .poll(u64::MAX)
        .iter()
        .map(|d| u32::from_le_bytes(d.payload[..4].try_into().unwrap()))
        .collect();
    assert_eq!(seqs.len(), 1_000);
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "FIFO order violated");
}

#[test]
fn poll_boundary_is_inclusive_at_due_time() {
    let mut net = SimNet::new(1);
    net.link(A, B, &custom(5.0, 0.0, 0.0, 0.0, 0.0, 0.0));
    net.send(1_000, A, B, vec![9]);
    assert!(net.poll(5_999).is_empty());
    assert_eq!(
        net.poll(6_000),
        vec![Delivery {
            to: B,
            from: A,
            payload: vec![9]
        }]
    );
}

#[test]
fn profile_swap_affects_only_later_sends() {
    let mut net = SimNet::new(3);
    net.link(A, B, &custom(5.0, 0.0, 0.0, 0.0, 0.0, 0.0));
    net.send(0, A, B, vec![1]); // due at 5_000 under the old profile
    net.set_profile(A, B, &custom(50.0, 0.0, 0.0, 0.0, 0.0, 0.0));
    net.send(0, A, B, vec![2]); // due at 50_000 under the new profile
    assert!(net.poll(4_999).is_empty());
    let first = net.poll(5_000);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].payload, vec![1]);
    assert!(net.poll(49_999).is_empty());
    let second = net.poll(50_000);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].payload, vec![2]);
}

#[test]
fn skewed_clock_drifts_exactly() {
    let hour_us = 3_600_000_000u64;
    assert_eq!(SkewedClock::new(200).map(hour_us), hour_us + 720_000);
    assert_eq!(SkewedClock::new(0).map(hour_us), hour_us);
    assert_eq!(SkewedClock::new(-200).map(hour_us), hour_us - 720_000);
}

#[test]
fn virtual_clock_has_no_creep_over_ten_million_ticks() {
    let mut clock = VirtualClock::new();
    let mut count = 0u64;
    let mut last = 0u64;
    for start in clock.ticks(2_500, 10_000_000) {
        last = start;
        count += 1;
    }
    assert_eq!(count, 10_000_000);
    assert_eq!(last, 2_500 * 9_999_999);
    assert_eq!(clock.now_us(), 25_000_000_000);
    assert_eq!(clock.now_ms(), 25_000_000);
}
