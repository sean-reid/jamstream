//! Scenario gates over the real session cores: the latency budget per
//! network profile, loss resilience, clock drift, chaos, soak, and
//! determinism. Every numeric threshold's derivation lives in a comment next
//! to its assertion, and a failing gate prints the measured numbers.
//!
//! Timing model shared by the derivations below (2.5 ms frames, one master
//! tick per frame): a capture sample waits up to one frame (2.5 ms) to be
//! encoded and sent; each network leg costs its one-way delay rounded up to
//! the next 2.5 ms poll boundary; each jitter buffer holds `target` frames
//! where target = round(3 * jitter_ewma) + 1 (see engine::jitter), and on a
//! clean link a frame arrives and is consumed in the same tick, so a steady
//! buffer contributes ~0 extra; the server mixes in the arrival tick.

use std::time::Instant;

use jamstream_harness::{ScenarioBuilder, Source, profiles};
use jamstream_session::{ClientState, ServerEvent};

fn median(mut v: Vec<f32>) -> f32 {
    assert!(!v.is_empty(), "no samples to take a median of");
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

/// Two musicians, member 0 emitting one impulse per 200 ms (9600 samples).
/// Returns the median mouth-to-ear latency measured at member 1 over 8 s of
/// virtual time after a 2 s settle.
fn median_latency_ms(profile_name: &str, seed: u64) -> f32 {
    let mut s = ScenarioBuilder::new(seed)
        .profile(profiles::profile(profile_name))
        .musicians(2)
        .source(
            0,
            Source::ImpulseTrain {
                period_samples: 9_600,
            },
        )
        .build();
    s.join_all_or_panic(4_000);
    s.run_ms(2_000);
    let mark = s.current_tick();
    s.run_ms(8_000);
    let latencies = s.impulse_latencies(0, 1, mark);
    assert!(
        latencies.len() >= 30,
        "{profile_name}: expected ~40 impulses in 8 s, detected {}",
        latencies.len()
    );
    median(latencies)
}

// lan-fiber: one-way 0.5 ms, jitter 0.05 ms.
// Floor: capture frame 2.5 + uplink ceil(0.5/2.5)*2.5 = 2.5 + server buffer
// ~0 (clean link, target 1, consumed on arrival) + mix in arrival tick +
// downlink 2.5 + client buffer ~0 = ~5 ms expected. Physical floor 2*0.5 =
// 1 ms. Product gate for LAN: 15 ms, leaving ~10 ms of margin for buffer
// adaptation regressions.
#[test]
fn latency_lan_fiber() {
    let m = median_latency_ms("lan-fiber", 0xA1);
    println!("lan-fiber median mouth-to-ear: {m:.2} ms");
    assert!(
        m >= 1.0,
        "lan-fiber median {m:.2} ms is below the 1 ms physical floor; measurement is broken"
    );
    assert!(
        m <= 15.0,
        "lan-fiber median mouth-to-ear {m:.2} ms exceeds the 15 ms gate"
    );
}

// regional-fiber: one-way 6 ms, jitter 0.5 ms.
// Floor: capture 2.5 + uplink ceil(6.75/2.5)*2.5 = 7.5 + server buffer ~0 +
// downlink 7.5 + client buffer ~0 = ~17.5 ms expected. Physical floor 12 ms.
// Gate 30 ms: the product promise (sub-30 ms same-region mouth-to-ear).
#[test]
fn latency_regional_fiber() {
    let m = median_latency_ms("regional-fiber", 0xA2);
    println!("regional-fiber median mouth-to-ear: {m:.2} ms");
    assert!(
        m >= 12.0,
        "regional-fiber median {m:.2} ms is below the 12 ms physical floor; measurement is broken"
    );
    assert!(
        m <= 30.0,
        "regional-fiber median mouth-to-ear {m:.2} ms breaks the 30 ms product promise"
    );
}

// dsl-cross-country: one-way 22.5 ms, jitter 2.5 ms.
// Floor: capture 2.5 + uplink ceil((22.5+j)/2.5)*2.5 = 25..27.5 + server
// buffer ~0..5 (jitter straddles tick boundaries, target 1-2) + downlink
// 25..27.5 + client buffer ~0..5 = ~52.5-65 ms expected. Physical floor
// 45 ms. Gate 65 ms.
#[test]
fn latency_dsl() {
    let m = median_latency_ms("dsl-cross-country", 0xA3);
    println!("dsl-cross-country median mouth-to-ear: {m:.2} ms");
    assert!(
        m >= 45.0,
        "dsl median {m:.2} ms is below the 45 ms physical floor; measurement is broken"
    );
    assert!(
        m <= 65.0,
        "dsl-cross-country median mouth-to-ear {m:.2} ms exceeds the 65 ms gate"
    );
}

// hostile-wifi: 20 ms RTT, 7.5 ms jitter with 20 ms reorder spikes, 2% loss.
// Silence gate 120 ms: the jitter buffer clamp is 24 frames (60 ms) and Opus
// PLC keeps concealment audible for several frames, so any silent gap beyond
// two full buffer refills means recovery is broken, not that the network was
// bad. Sine peak per 2.5 ms tick is ~0.35 (0.5 amp, center pan 0.707);
// threshold 0.02 is well below concealment output and well above numeric
// noise.
#[test]
fn loss_resilience_hostile_wifi() {
    let mut s = ScenarioBuilder::new(0xB1)
        .profile(profiles::profile("hostile-wifi"))
        .musicians(3)
        .source(
            0,
            Source::Sine {
                hz: 440.0,
                amp: 0.5,
            },
        )
        .keep_audio(false)
        .build();
    s.join_all_or_panic(4_000);
    s.run_ms(2_000);
    let mark = s.current_tick();
    s.run_ms(30_000);
    let end = s.current_tick();

    // Musicians 1 and 2 hear member 0's sine; member 0's own mix excludes
    // itself and its peers are silent, so it is not gated.
    for i in [1, 2] {
        let gap = s.longest_silence_ms(i, mark, end, 0.02);
        assert!(
            gap < 120.0,
            "musician {i} longest silence {gap:.1} ms under hostile-wifi (gate 120 ms)"
        );
    }

    // 2% loss sits above the 1% redundancy-on threshold, so uplink frames
    // lost on the wire must be recovered from piggybacked copies.
    let recovered: u64 = s
        .server_member_stats()
        .iter()
        .map(|m| m.jitter.recovered)
        .sum();
    assert!(
        recovered > 0,
        "no frames recovered via app-layer redundancy under 2% loss; stats: {:?}",
        s.server_member_stats()
    );

    assert!(
        !s.server_events().iter().any(|e| matches!(
            e,
            ServerEvent::MemberDisconnected { .. } | ServerEvent::MemberRevoked { .. }
        )),
        "a member dropped during hostile-wifi: {:?}",
        s.server_events()
    );
}

// +-200 ppm is one extra or one missing 2.5 ms frame every 12.5 s. The
// jitter buffer's shrink path (drop one frame after 16 over-target ticks)
// must absorb the fast side and PLC the slow side, so depth stays under the
// 24-frame clamp and playout never gaps longer than 250 ms (= 100 frames,
// four full clamp depths; anything longer means a buffer reset loop).
//
// KNOWN FAILURE (engine finding, not a harness bug): the slow-clock side
// starves the server's jitter buffer ~12.5 s after join and never recovers.
// Once the consumer overruns the producer by one frame, every subsequent
// frame is exactly one tick late: engine::jitter counts lost and late one
// per tick from then on (measured: lost 18_996 / late 18_987 at t=60 s) and
// the stream is silent for the rest of the session. The buffer's only
// re-sync paths are the RESET_JUMP (|jump| > 512) and the growth hold,
// which requires the expected frame to already be buffered, so a persistent
// one-frame consumer lead is unrecoverable. Fix belongs in
// crates/engine/src/jitter.rs; this gate stays red until it lands.
#[test]
fn drift_200ppm_stays_bounded() {
    let mut s = ScenarioBuilder::new(0xC1)
        .profile(profiles::profile("regional-fiber"))
        .musicians(2)
        .skew_ppm(0, 200)
        .skew_ppm(1, -200)
        .source(
            0,
            Source::Sine {
                hz: 440.0,
                amp: 0.5,
            },
        )
        .source(
            1,
            Source::Sine {
                hz: 330.0,
                amp: 0.5,
            },
        )
        .keep_audio(false)
        .build();
    s.join_all_or_panic(4_000);
    s.run_ms(2_000);
    let mark = s.current_tick();

    // 10 virtual minutes, sampling every buffer's depth once per second.
    let mut max_depth = 0usize;
    let mut final_depths: Vec<usize> = Vec::new();
    for _ in 0..600 {
        s.run_ms(1_000);
        final_depths.clear();
        for i in 0..2 {
            final_depths.push(s.client_jitter(i).depth_frames);
        }
        for m in s.server_member_stats() {
            final_depths.push(m.jitter.depth_frames);
        }
        max_depth = max_depth.max(*final_depths.iter().max().expect("depths"));
    }
    let end = s.current_tick();

    assert!(
        max_depth <= 24,
        "jitter depth reached {max_depth} frames under 200 ppm drift (clamp 24); \
         final sampled depths (client 0, client 1, server per member) {final_depths:?}"
    );
    for i in 0..2 {
        let gap = s.longest_silence_ms(i, mark, end, 0.02);
        assert!(
            gap < 250.0,
            "musician {i} longest silence {gap:.1} ms under 200 ppm drift (gate 250 ms); \
             steady-state depths {final_depths:?}, max over run {max_depth}"
        );
    }
    println!(
        "drift 200ppm: max depth {max_depth}, steady-state depths \
         (client 0, client 1, server members) {final_depths:?}"
    );
}

// Six musicians with a seeded schedule of leaves, a garbage-stream client, a
// mid-stream revoke, and rejoins. The invariants: no panic, the roster
// converges to the survivors, and the continuously-present musicians'
// audio never gaps longer than 150 ms (the churn is control-plane; it must
// not stall the media path).
#[test]
fn chaos_join_leave_storm() {
    let seed = 0xD1u64;
    // Seeded schedule: event times get 0..375 ms of seed-derived offset.
    let mut lcg = seed;
    let mut at = move |base_ms: u64| {
        lcg = lcg
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        base_ms + (lcg >> 60) * 25
    };

    let mut s = ScenarioBuilder::new(seed)
        .profile(profiles::profile("regional-fiber"))
        .musicians(6)
        .source(
            0,
            Source::Sine {
                hz: 440.0,
                amp: 0.5,
            },
        )
        .source(
            1,
            Source::Sine {
                hz: 330.0,
                amp: 0.5,
            },
        )
        .keep_audio(false)
        .build();
    s.join_all_or_panic(4_000);

    s.run_until_ms(3_000);
    let mark = s.current_tick();

    // Musician 5 leaves cleanly.
    s.run_until_ms(at(5_000));
    s.leave(5);

    // Musician 4's stream turns to garbage for ~14 s; the server times it
    // out (10 s member timeout) and must not be otherwise disturbed.
    s.run_until_ms(at(10_000));
    s.set_garbage(4, true);
    s.run_until_ms(at(24_000));
    s.set_garbage(4, false);
    s.reconnect(4);

    // Host revokes musician 3 mid-stream: ejected, cannot return.
    s.run_until_ms(at(30_000));
    s.host_revoke(3);

    // Musician 5 rejoins with the same (unrevoked) token.
    s.run_until_ms(at(35_000));
    s.reconnect(5);

    s.run_until_ms(58_000);
    let end = s.current_tick();
    s.run_until_ms(60_000);

    // Roster converged to the survivors: 0, 1, 2, 4, 5. The revoked member
    // is gone entirely, not just disconnected.
    assert_eq!(
        s.musicians_connected(),
        5,
        "server roster did not converge; events: {:?}",
        s.server_events()
    );
    let stats = s.server_member_stats();
    assert!(
        !stats.iter().any(|m| m.id.0 == 3),
        "revoked member 3 still on the server: {stats:?}"
    );
    for id in [0u16, 1, 2, 4, 5] {
        assert!(
            stats.iter().any(|m| m.id.0 == id && m.connected),
            "member {id} should have survived; stats: {stats:?}"
        );
    }
    assert!(
        matches!(s.client_state(3), ClientState::Ejected { .. }),
        "revoked client should be Ejected, was {:?}",
        s.client_state(3)
    );
    assert!(
        s.server_events()
            .iter()
            .any(|e| matches!(e, ServerEvent::MemberRevoked { id } if id.0 == 3)),
        "missing MemberRevoked event: {:?}",
        s.server_events()
    );

    // Musicians 0, 1, 2 were present throughout while 0 and 1 played;
    // 150 ms = 60 frames, over twice the 24-frame buffer clamp, so any
    // longer gap means churn stalled the media path.
    for i in [0usize, 1, 2] {
        let gap = s.longest_silence_ms(i, mark, end, 0.02);
        assert!(
            gap < 150.0,
            "musician {i} longest silence {gap:.1} ms during the join/leave storm (gate 150 ms)"
        );
    }
}

// Ten virtual minutes, 4 musicians + 2 listeners on regional fiber. Bounds:
// every jitter buffer inside the 24-frame clamp, listener audio present in
// every 10 s window, zero protocol violations. The listener rms gate of
// 0.05 is one third of the quietest plausible signal: a single musician at
// 0.3 amplitude lands at ~0.15 rms after constant-power pan; four sines mix
// to ~0.3 under the broadcast limiter.
#[test]
fn soak_ten_minutes_regional() {
    let mut s = ScenarioBuilder::new(0xE1)
        .profile(profiles::profile("regional-fiber"))
        .musicians(4)
        .listeners(2)
        .source(
            0,
            Source::Sine {
                hz: 220.0,
                amp: 0.3,
            },
        )
        .source(
            1,
            Source::Sine {
                hz: 330.0,
                amp: 0.3,
            },
        )
        .source(
            2,
            Source::Sine {
                hz: 440.0,
                amp: 0.3,
            },
        )
        .source(
            3,
            Source::Sine {
                hz: 550.0,
                amp: 0.3,
            },
        )
        .keep_audio(false)
        .build();
    s.join_all_or_panic(4_000);
    s.run_ms(2_000);

    let mut max_depth = 0usize;
    for window in 0..60 {
        let from = s.current_tick();
        s.run_ms(10_000);
        let to = s.current_tick();
        for listener in [4usize, 5] {
            let rms = s.rms_of(listener, from, to);
            assert!(
                rms > 0.05,
                "listener {listener} rms {rms:.4} in 10 s window {window} (gate 0.05)"
            );
        }
        for i in 0..6 {
            max_depth = max_depth.max(s.client_jitter(i).depth_frames);
        }
        for m in s.server_member_stats() {
            max_depth = max_depth.max(m.jitter.depth_frames);
        }
    }

    assert!(
        max_depth <= 24,
        "jitter depth reached {max_depth} frames during the soak (clamp 24)"
    );
    let violations: u64 = s.server_member_stats().iter().map(|m| m.violations).sum();
    assert_eq!(
        violations,
        0,
        "server counted {violations} protocol violations during the soak: {:?}",
        s.server_member_stats()
    );
    // For the record, not asserted: total simulated datagram counts.
    let t = s.traffic();
    println!(
        "soak traffic: sent {} delivered {} dropped {}",
        t.sent, t.delivered, t.dropped
    );
}

fn determinism_run(seed: u64) -> Vec<Vec<f32>> {
    let mut s = ScenarioBuilder::new(seed)
        .profile(profiles::profile("hostile-wifi"))
        .musicians(2)
        .source(
            0,
            Source::Sine {
                hz: 440.0,
                amp: 0.5,
            },
        )
        .source(
            1,
            Source::Sine {
                hz: 330.0,
                amp: 0.5,
            },
        )
        .build();
    // Fixed tick count, no join_all first, so both runs record the exact
    // same number of ticks including the handshake phase.
    s.run_ms(8_000);
    for i in 0..2 {
        assert_eq!(
            s.client_state(i),
            ClientState::Joined,
            "client {i} failed to join within 8 s"
        );
    }
    (0..2).map(|i| s.recording(i).to_vec()).collect()
}

// Handshake bytes differ run to run (fresh Noise keys), but every packet's
// size and send instant is fixed by the tick schedule, so the seeded network
// consumes its RNG identically and draws the same loss/jitter/reorder for
// every packet; after that the media path (Opus in, Opus out) is pure. The
// recordings must therefore be bit-identical, no relaxation needed.
#[test]
fn determinism_same_seed() {
    let a = determinism_run(42);
    let b = determinism_run(42);
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        assert_eq!(x.len(), y.len(), "client {i}: recording lengths differ");
        let first_diff = x
            .iter()
            .zip(y)
            .position(|(p, q)| p.to_bits() != q.to_bits());
        assert_eq!(
            first_diff, None,
            "client {i}: same-seed recordings diverge at sample {first_diff:?}"
        );
    }
    // And the seed matters: on a lossy profile a different seed must produce
    // audibly different packet fates.
    let c = determinism_run(43);
    let differs = a.iter().zip(&c).any(|(x, y)| {
        x.len() != y.len() || x.iter().zip(y).any(|(p, q)| p.to_bits() != q.to_bits())
    });
    assert!(
        differs,
        "different seeds produced identical recordings on a 2% loss profile"
    );
}

// Listener gate: broadcast of one 0.5-amp sine mixes to ~0.35 rms after the
// limiter, so 0.05 is a 7x margin over the gate while 100x above silence.
// The second half checks containment: an authenticated listener pushing
// media is counted as a violation and nothing else changes.
#[test]
fn listener_hears_broadcast() {
    let mut s = ScenarioBuilder::new(0xF1)
        .profile(profiles::profile("regional-fiber"))
        .musicians(2)
        .listeners(1)
        .source(
            0,
            Source::Sine {
                hz: 440.0,
                amp: 0.5,
            },
        )
        .keep_audio(false)
        .build();
    s.join_all_or_panic(4_000);
    s.run_ms(2_000);
    let mark = s.current_tick();
    s.run_ms(5_000);
    let mid = s.current_tick();

    let rms = s.rms_of(2, mark, mid);
    assert!(rms > 0.05, "listener broadcast rms {rms:.4} (gate 0.05)");

    // An honest ClientCore listener has no encoder and cannot send media at
    // all; this drives the protocol directly.
    s.raw_listener_media(9);
    s.run_ms(2_000);
    let end = s.current_tick();

    assert!(
        s.server_events().iter().any(|e| matches!(
            e,
            ServerEvent::ProtocolViolation { id, what }
                if id.0 == 9 && *what == "media from listener"
        )),
        "expected a media-from-listener violation, events: {:?}",
        s.server_events()
    );
    let rms_after = s.rms_of(2, mid, end);
    assert!(
        rms_after > 0.05,
        "listener audio disturbed after the media violation: rms {rms_after:.4}"
    );
    let gap = s.longest_silence_ms(1, mark, end, 0.02);
    assert!(
        gap < 120.0,
        "musician 1 gapped {gap:.1} ms around the media violation"
    );
}

// Not a benchmark: a coarse guard that the simulation stays usable for
// iteration. 60 virtual seconds with 4 musicians must finish inside 30 s of
// wall time in a debug build (libopus itself is compiled optimized by its
// build script regardless of profile).
#[test]
fn perf_sanity_sixty_seconds_regional() {
    let start = Instant::now();
    let mut s = ScenarioBuilder::new(0x91)
        .profile(profiles::profile("regional-fiber"))
        .musicians(4)
        .source(
            0,
            Source::Sine {
                hz: 220.0,
                amp: 0.3,
            },
        )
        .source(
            1,
            Source::Sine {
                hz: 330.0,
                amp: 0.3,
            },
        )
        .source(
            2,
            Source::Sine {
                hz: 440.0,
                amp: 0.3,
            },
        )
        .source(
            3,
            Source::Sine {
                hz: 550.0,
                amp: 0.3,
            },
        )
        .keep_audio(false)
        .build();
    s.run_ms(60_000);
    let wall = start.elapsed().as_secs_f64();
    println!("perf sanity: 60 s virtual, 4 musicians, regional-fiber: {wall:.2} s wall");
    assert_eq!(s.musicians_connected(), 4, "scenario did not even join");
    assert!(
        wall < 30.0,
        "60 s regional-fiber scenario took {wall:.2} s wall (budget 30 s in debug)"
    );
}
