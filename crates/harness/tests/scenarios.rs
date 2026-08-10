//! Scenario gates over the real session cores: the latency budget per
//! network profile, loss resilience, clock drift, chaos, soak, determinism,
//! and a session at its shipped capacity. Every numeric threshold's
//! derivation lives in a comment next to its assertion, and a failing gate
//! prints the measured numbers.
//!
//! Timing model shared by the derivations below (2.5 ms frames, one master
//! tick per frame): a capture sample waits up to one frame (2.5 ms) to be
//! encoded and sent; each network leg costs its one-way delay rounded up to
//! the next 2.5 ms poll boundary; each jitter buffer holds `target` frames
//! where target = round(3 * jitter_ewma) + 1 (see engine::jitter), and on a
//! clean link a frame arrives and is consumed in the same tick, so a steady
//! buffer contributes ~0 extra; the server mixes in the arrival tick; and the
//! listener's playout ring holds two device callbacks, 5 ms at the 120-frame
//! default, which every sample queues behind before the device plays it.
//!
//! What the gates below cover is capture to our own last buffer. The playout
//! cushion is the final stage this side can see; whatever the sound card holds
//! after the callback is not knowable from here and is in none of these
//! numbers.

use std::time::Instant;

use jamstream_engine::JitterStats;
use jamstream_harness::scenario::DEVICE_FRAMES;
use jamstream_harness::{ScenarioBuilder, Source, profiles};
use jamstream_session::{ClientState, MAX_LISTENERS, MAX_MUSICIANS, ServerEvent};

fn median(mut v: Vec<f32>) -> f32 {
    assert!(!v.is_empty(), "no samples to take a median of");
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

fn median_latency_ms(profile_name: &str, seed: u64) -> f32 {
    median_latency_with_device(profile_name, seed, DEVICE_FRAMES).0
}

/// Two musicians, member 0 emitting one impulse per 200 ms (9600 samples) and
/// both playing out through a device that calls back for `device_frames` frames
/// at a time. Returns the median mouth-to-ear latency measured at member 1 over
/// 8 s of virtual time after a 2 s settle, and the depth its jitter buffer
/// settled on, which is the other term that moves with the callback size.
fn median_latency_with_device(profile_name: &str, seed: u64, device_frames: u32) -> (f32, usize) {
    let mut s = ScenarioBuilder::new(seed)
        .profile(profiles::profile(profile_name))
        .musicians(2)
        .device_frames(device_frames)
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
    // Silence a starved device invents is latency the figures below would
    // carry without the cushion being the reason for it.
    assert_eq!(
        s.playout_underruns(1),
        0,
        "{profile_name}: listener's playout ring ran dry"
    );
    (median(latencies), s.client_jitter(1).target_frames)
}

// lan-fiber: one-way 0.5 ms, jitter 0.05 ms.
// Floor: capture frame 2.5 + uplink ceil(0.5/2.5)*2.5 = 2.5 + server buffer
// ~0 (clean link, target 1, consumed on arrival) + mix in arrival tick +
// downlink 2.5 + client buffer ~0 + playout cushion 5 = ~10 ms expected,
// measured 14.67. Physical floor 2*0.5 = 1 ms. Product gate for LAN: 16 ms,
// against 15 for the same path measured to the engine boundary, which left the
// cushion out. Covers capture to our own last buffer; whatever the sound card
// holds after the callback returns is outside our sight and outside this gate.
#[test]
fn latency_lan_fiber() {
    let m = median_latency_ms("lan-fiber", 0xA1);
    println!("lan-fiber median mouth-to-ear: {m:.2} ms");
    assert!(
        m >= 1.0,
        "lan-fiber median {m:.2} ms is below the 1 ms physical floor; measurement is broken"
    );
    assert!(
        m <= 16.0,
        "lan-fiber median mouth-to-ear {m:.2} ms exceeds the 16 ms gate"
    );
}

// regional-fiber: one-way 6 ms, jitter 0.5 ms.
// Floor: capture 2.5 + uplink ceil(6.75/2.5)*2.5 = 7.5 + server buffer ~0 +
// downlink 7.5 + client buffer ~0 + playout cushion 5 = ~22.5 ms expected,
// measured 24.31. Physical floor 12 ms. Gate 30 ms, unchanged: this is the
// product promise (sub-30 ms same-region mouth-to-ear) and same-region meets it
// with the device on the end. Covers capture to our own last buffer; whatever
// the sound card holds after the callback returns is outside our sight and
// outside this gate.
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
// 25..27.5 + client buffer ~0..5 + playout cushion 5 = ~57.5-70 ms expected,
// measured 69.75. Physical floor 45 ms. Gate 72 ms, against 65 for the same
// path measured to the engine boundary, which was already 64.75 ms of it: this
// profile has no room for a device inside 65. Covers capture to our own last
// buffer; whatever the sound card holds after the callback returns is outside
// our sight and outside this gate.
#[test]
fn latency_dsl() {
    let m = median_latency_ms("dsl-cross-country", 0xA3);
    println!("dsl-cross-country median mouth-to-ear: {m:.2} ms");
    assert!(
        m >= 45.0,
        "dsl median {m:.2} ms is below the 45 ms physical floor; measurement is broken"
    );
    assert!(
        m <= 72.0,
        "dsl-cross-country median mouth-to-ear {m:.2} ms exceeds the 72 ms gate"
    );
}

// The device stage is in the three numbers above.
//
// Playout runs through the client's own ring, and the depth the top-up loop
// holds is two device callbacks, so the callback size sets the cushion: 5 ms at
// the 120-frame default, 10 ms at 240, 20 ms at 480. A measurement that stopped
// at the engine boundary would read one number for all of them, so a bigger
// callback costing at least its extra cushion is what says the device stage is
// in there.
//
// It costs more than the cushion once a callback outlasts the 2.5 ms master
// tick, and the second assertion is where that cost is accounted for rather
// than waved at. The worker tops the ring up on the tick and the device drains
// it on its own clock, so a 240-frame callback takes two frames out of the
// engine at once and a 480-frame one takes four; pulled in bursts, the
// listener's jitter buffer settles deeper, and every frame of that depth is
// latency too.
//
// Measured on lan-fiber, seed 0xA7: 14.67 ms at 120 frames with a 1-frame
// buffer, 27.17 at 240 with 4, 39.67 at 480 with 6. Both tolerances are one
// frame, since the buffer depth is read at the end of the run and the median is
// over the whole of it.
#[test]
fn playout_cushion_is_in_the_latency() {
    const FRAME_MS: f32 = 2.5;
    let (base, base_depth) = median_latency_with_device("lan-fiber", 0xA7, DEVICE_FRAMES);
    println!("lan-fiber at {DEVICE_FRAMES}-frame callbacks: {base:.2} ms, buffer {base_depth}");
    for frames in [2 * DEVICE_FRAMES, 4 * DEVICE_FRAMES] {
        let (m, depth) = median_latency_with_device("lan-fiber", 0xA7, frames);
        let cushion_ms = 2.0 * (frames - DEVICE_FRAMES) as f32 / 48.0;
        let buffer_ms = (depth - base_depth) as f32 * FRAME_MS;
        println!(
            "lan-fiber at {frames}-frame callbacks: {m:.2} ms, buffer {depth}, against \
             {base:.2} ms and {base_depth} at {DEVICE_FRAMES}"
        );
        assert!(
            m - base >= cushion_ms - FRAME_MS,
            "a {frames}-frame callback holds {cushion_ms:.2} ms more cushion than a \
             {DEVICE_FRAMES}-frame one and mouth-to-ear moved by {:.2} ms ({m:.2} against \
             {base:.2}); the device stage is not in the measurement",
            m - base
        );
        assert!(
            m - base <= cushion_ms + buffer_ms + FRAME_MS,
            "a {frames}-frame callback moved mouth-to-ear by {:.2} ms ({m:.2} against \
             {base:.2}), more than its {cushion_ms:.2} ms of extra cushion and the \
             {buffer_ms:.2} ms the listener's buffer grew by",
            m - base
        );
    }
}

// hostile-wifi: 20 ms RTT, 7.5 ms jitter with 20 ms reorder spikes, 2% loss.
//
// Silence gate 120 ms: the jitter buffer clamp is 24 frames (60 ms), so a
// silent gap beyond two full buffer refills means the buffer is resetting in
// a loop. It says nothing about redundancy, because Opus PLC conceals a
// missing frame with sound and satisfies this on its own; it is here for the
// collapse, not for the recovery. Sine peak per 2.5 ms tick is ~0.35 (0.5
// amp, center pan 0.707); threshold 0.02 is well below concealment output and
// well above numeric noise.
//
// Redundancy is gated separately and per member, on the share of gaps the
// piggybacked copies close. Measured over 30 s: about 430 gaps per member, of
// which 390 to 394 are recovered, so 90% to 93%. The floor is 75%: turning
// redundancy off takes it to 0, and any real degradation lands well below 75
// while a run of bad luck cannot. Per member and not summed, because summed,
// one healthy member covers for two broken ones.
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
    let stats = s.server_member_stats();
    assert_eq!(stats.len(), 3, "expected three members: {stats:?}");
    for m in &stats {
        let gaps = m.jitter.lost + m.jitter.recovered;
        // The profile has to have hurt, or the share below means nothing.
        assert!(
            gaps >= 150,
            "member {:?} saw only {gaps} gaps in 30 s of 2% loss, so the profile \
             is no longer testing redundancy: {m:?}",
            m.id
        );
        let share = 100.0 * m.jitter.recovered as f64 / gaps as f64;
        assert!(
            share >= 75.0,
            "member {:?} recovered {:.1}% of {gaps} gaps from piggybacked copies \
             (floor 75%, measured 90 to 93): {m:?}",
            m.id,
            share
        );
    }

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
// History: this gate found a real engine bug (a slow-clock sender starved
// the server's jitter buffer ~12.5 s after join and never recovered; one
// loss per tick forever). The jitter buffer's resurrect path fixed it: the
// timeline stretches one frame per drift period, roughly every 12.5 s at
// -200 ppm. This test pins that fallback behavior via the exact-frame
// APIs; drift_200ppm_with_resampler covers the steered raw-audio path,
// which keeps resurrects at zero entirely.
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

// The structural fix for the same drift: clients driven through the raw
// device-paced APIs, whose capture compensator is steered from the server's
// once-per-second uplink depth reports (and playout from the local buffer).
// Every server-side resurrect is one stretched frame papering over a
// consumer overrun, so once the steering loop has converged they must stop:
// after a 5 minute startup allowance (the PI loop converges in tens of
// seconds; 5 minutes is deliberate slack for depth-report noise), at most 5
// resurrects may land across the last 5 of 10 virtual minutes, against ~24
// per member without steering (one per 12.5 s at 200 ppm). Depth and
// audio-continuity bounds stay exactly as in the fallback gate above.
#[test]
fn drift_200ppm_with_resampler() {
    let mut s = ScenarioBuilder::new(0xC2)
        .profile(profiles::profile("regional-fiber"))
        .musicians(2)
        .skew_ppm(0, 200)
        .skew_ppm(1, -200)
        .raw_audio(true)
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

    let server_resurrects = |s: &jamstream_harness::Scenario| -> u64 {
        s.server_member_stats()
            .iter()
            .map(|m| m.jitter.resurrected)
            .sum()
    };

    // 10 virtual minutes, sampling every buffer's depth once per second.
    let mut max_depth = 0usize;
    let sample_depths = |s: &jamstream_harness::Scenario, max_depth: &mut usize| {
        for i in 0..2 {
            *max_depth = (*max_depth).max(s.client_jitter(i).depth_frames);
        }
        for m in s.server_member_stats() {
            *max_depth = (*max_depth).max(m.jitter.depth_frames);
        }
    };
    for _ in 0..300 {
        s.run_ms(1_000);
        sample_depths(&s, &mut max_depth);
    }
    let resurrects_at_5m = server_resurrects(&s);
    for _ in 0..300 {
        s.run_ms(1_000);
        sample_depths(&s, &mut max_depth);
    }
    let end = s.current_tick();
    let resurrects_at_10m = server_resurrects(&s);
    let settled_delta = resurrects_at_10m - resurrects_at_5m;

    println!(
        "drift 200ppm with resampler: max depth {max_depth}, server resurrects \
         {resurrects_at_5m} in the first 5 min (startup allowance), \
         {settled_delta} in the last 5 min (gate 5)"
    );
    assert!(
        max_depth <= 24,
        "jitter depth reached {max_depth} frames under steered 200 ppm drift (clamp 24)"
    );
    assert!(
        settled_delta <= 5,
        "steering did not converge: {settled_delta} server resurrects in the last \
         5 minutes (gate 5; startup 5 minutes had {resurrects_at_5m})"
    );
    for i in 0..2 {
        let gap = s.longest_silence_ms(i, mark, end, 0.02);
        assert!(
            gap < 250.0,
            "musician {i} longest silence {gap:.1} ms under steered 200 ppm drift (gate 250 ms)"
        );
    }
}

/// The server's jitter stats for one member id.
fn member_jitter(s: &jamstream_harness::Scenario, id: u16) -> JitterStats {
    s.server_member_stats()
        .into_iter()
        .find(|m| m.id.0 == id)
        .unwrap_or_else(|| panic!("member {id} not on the server roster"))
        .jitter
}

// The re-anchor gate: the real-world trigger for the jitter buffer's
// re-anchor policy, end to end over the real cores.
//
// A client's audio driver freezes mid-session (a multi-second process stall
// under load). While frozen it captures nothing - and those frames are gone,
// not replayed - and asks for no playout, so two buffers come out of the
// stall out of phase with the streams feeding them, at an offset of 2..512
// frames, past the resurrect path's one-frame reach and short of the 512-frame
// restart threshold:
//
//   * the server's buffer for that member's uplink is `stall` frames ahead of
//     the sequence numbers now arriving, which carried on contiguously across
//     the gap, so every packet is late and the member is silent to everyone;
//   * the client's own downlink buffer fills with a stream nobody is pulling,
//     so it reaches its depth cap and gives its playout position up while the
//     stall is still running.
//
// Before the policy the first state was permanent: `late` climbing one per tick
// (~400/s), depth pinned, playout concealed for the rest of the session. Each
// buffer must now come out of the stall inside a bounded window and stay out of
// it. The second, longer stall checks the division of labour at the other edge:
// past 512 frames the discontinuity reset owns the uplink's recovery and no
// re-anchor is needed there.
#[test]
fn driver_stall_reanchors_and_audio_returns() {
    // 400 ticks = 1 s of frozen driver, mid-hole. 1200 ticks = 3 s, past the
    // 512-frame restart threshold.
    const HOLE_STALL_TICKS: u64 = 400;
    const BIG_STALL_TICKS: u64 = 1_200;
    // 160 ticks = 400 ms. The engine's promise is 60 stuck ticks of detection
    // (150 ms) plus a refill of at most 24 frames (60 ms), so ~210 ms; 400 ms
    // is that with margin for the extra network legs, and nothing like
    // "eventually".
    const RECOVER_TICKS: u64 = 160;
    // Each musician hears only the other one (the personal mix excludes its
    // own signal): one 0.5-amp sine at ~0.707 center-pan gain is ~0.25 rms,
    // so 0.1 is a 2.5x margin under the signal and 5x over concealment tails.
    const AUDIO_GATE: f32 = 0.1;

    let mut s = ScenarioBuilder::new(0xC3)
        .profile(profiles::profile("regional-fiber"))
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
        .keep_audio(false)
        .build();
    s.join_all_or_panic(4_000);
    s.run_ms(2_000);

    let base_from = s.current_tick();
    s.run_ms(1_000);
    let base_to = s.current_tick();
    let baseline: Vec<f32> = (0..2).map(|i| s.rms_of(i, base_from, base_to)).collect();
    for (i, &base) in baseline.iter().enumerate() {
        assert!(
            base > AUDIO_GATE,
            "musician {i} rms {base:.4} before the stall (gate {AUDIO_GATE}); \
             the scenario is not producing audio"
        );
    }

    // --- Stall inside the hole.
    s.set_driver_stalled(1, true);
    s.run_ticks(HOLE_STALL_TICKS);
    s.set_driver_stalled(1, false);
    let resume = s.current_tick();

    // The stall really did take member 1 off the air for member 0: the
    // server's buffer for its uplink runs dry and conceals.
    let during = s.rms_of(0, resume - HOLE_STALL_TICKS / 2, resume);
    assert!(
        during < AUDIO_GATE / 2.0,
        "musician 0 rms {during:.4} during member 1's driver stall; \
         the stall is not reproducing the trigger"
    );

    s.run_ticks(RECOVER_TICKS);
    let healed_at = s.current_tick();
    let uplink = member_jitter(&s, 1);
    let downlink = s.client_jitter(1);
    s.run_ms(1_000);
    let after_to = s.current_tick();

    println!(
        "driver stall (1 s, mid-hole): server member 1 uplink {uplink:?}, \
         client 1 downlink {downlink:?}"
    );
    // The uplink's stuck position went exactly once, on the watchdog inside its
    // pull. The client's own buffer had no pull to be rescued from, so its cap
    // is what gave the position up, once per cap's worth of the stall.
    assert_eq!(
        uplink.reanchors, 1,
        "server's buffer for member 1 did not re-anchor within \
         {RECOVER_TICKS} ticks of the stall ending: {uplink:?}"
    );
    assert!(
        downlink.reanchors >= 1,
        "client 1's own buffer sat at its cap waiting for the pull that stalled: {downlink:?}"
    );
    // And nothing else in the session was disturbed.
    assert_eq!(member_jitter(&s, 0).reanchors, 0);
    assert_eq!(s.client_jitter(0).reanchors, 0);

    // Audio is back on both sides of the link, not merely trending up.
    for (i, &base) in baseline.iter().enumerate() {
        let rms = s.rms_of(i, healed_at, after_to);
        assert!(
            rms > AUDIO_GATE && rms > base / 2.0,
            "musician {i} rms {rms:.4} in the second after recovery \
             (gate {AUDIO_GATE}, baseline {base:.4})"
        );
    }

    // The `late` counter stopped climbing: while stuck it grew one per tick,
    // i.e. ~400 in the second just measured. regional-fiber has no
    // reordering, so a handful of stragglers is all that is allowed.
    let late_delta = member_jitter(&s, 1).late - uplink.late;
    assert!(
        late_delta <= 5,
        "server's late count for member 1 climbed {late_delta} in the second \
         after recovery (stuck would be ~400)"
    );
    // And both stay out of it, which is the half a bounded window does not
    // cover: a buffer whose consumer is back has no position to give up.
    let settled = (member_jitter(&s, 1), s.client_jitter(1));
    assert_eq!(
        (settled.0.reanchors, settled.1.reanchors),
        (uplink.reanchors, downlink.reanchors),
        "a re-anchor in the second after recovery: {settled:?}"
    );

    // --- Stall past the restart threshold: RESET_JUMP's job on the uplink,
    // where the position is ahead of the stream. The client's own buffer is
    // filling rather than trailing, so its depth cap owns that side whatever
    // the offset.
    let reanchors_before = (uplink.reanchors, downlink.reanchors);
    s.set_driver_stalled(1, true);
    s.run_ticks(BIG_STALL_TICKS);
    s.set_driver_stalled(1, false);
    s.run_ticks(RECOVER_TICKS);
    let healed_at = s.current_tick();
    let healed = s.client_jitter(1);
    s.run_ms(1_000);
    let after_to = s.current_tick();

    let uplink = member_jitter(&s, 1);
    let downlink = s.client_jitter(1);
    println!(
        "driver stall (3 s, past RESET_JUMP): server member 1 uplink {uplink:?}, \
         client 1 downlink {downlink:?}"
    );
    assert_eq!(
        uplink.reanchors, reanchors_before.0,
        "a 3 s stall (offset past the {} frame restart threshold) should be \
         healed by the discontinuity reset, not by a re-anchor: {uplink:?}",
        512
    );
    assert!(
        downlink.reanchors > reanchors_before.1,
        "client 1's buffer filled for 3 s with nothing pulling it and kept its \
         playout position: {downlink:?}"
    );
    assert_eq!(
        downlink.reanchors, healed.reanchors,
        "client 1's buffer kept re-anchoring after the stall ended: {downlink:?}"
    );
    for (i, &base) in baseline.iter().enumerate() {
        let rms = s.rms_of(i, healed_at, after_to);
        assert!(
            rms > AUDIO_GATE && rms > base / 2.0,
            "musician {i} rms {rms:.4} after the 3 s stall \
             (gate {AUDIO_GATE}, baseline {base:.4})"
        );
    }
    assert!(
        !s.server_events().iter().any(|e| matches!(
            e,
            ServerEvent::MemberDisconnected { .. } | ServerEvent::MemberRevoked { .. }
        )),
        "a member dropped across the driver stalls: {:?}",
        s.server_events()
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

/// Wall budget for the reference run below on a developer machine.
const REFERENCE_LAPTOP_SECS: f64 = 30.0;

/// A wall-clock budget in seconds, scaled for the machine running the suite.
///
/// `JAMSTREAM_PERF_BUDGET_SECS` states what the reference run below is
/// allowed here: 30 s on a laptop, 120 s on a shared ci runner that is
/// several times slower. Every wall-clock gate names its own laptop budget
/// and takes the same multiplier from that one variable, so the runner is
/// described once instead of per gate.
fn perf_budget_secs(laptop_secs: f64) -> f64 {
    laptop_secs * perf_scale()
}

/// How much slower than the reference laptop the machine running this suite is
/// declared to be. 1.0 unless `JAMSTREAM_PERF_BUDGET_SECS` says otherwise, and
/// never below it: a value under the reference, or an unparseable one, has to
/// leave the laptop budget alone rather than shorten a deadline. The server and
/// cli suites clamp the same multiplier the same way.
///
/// It describes throughput and nothing else. What it cannot describe is a
/// machine with no idle core, where a timed region loses the cpu in the middle
/// and the sample records the scheduler instead of the work. Isolation is the
/// answer to that, not a bigger number: see
/// [`the_measured_tests_are_named_in_the_nextest_config`].
fn perf_scale() -> f64 {
    scale_of(std::env::var("JAMSTREAM_PERF_BUDGET_SECS").ok().as_deref())
}

fn scale_of(value: Option<&str>) -> f64 {
    value
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map_or(1.0, |v| v / REFERENCE_LAPTOP_SECS)
        .max(1.0)
}

/// The runner is described once and a deadline can only ever get longer from
/// it. A missing or nonsense value has to leave the laptop budget alone rather
/// than collapse to zero.
#[test]
fn a_deadline_scales_with_the_runner_and_never_shrinks() {
    assert_eq!(scale_of(None), 1.0, "unset is the laptop budget");
    // What CI sets: 120 s against the 30 s reference run.
    assert_eq!(scale_of(Some("120")), 4.0);
    assert_eq!(scale_of(Some("45")), 1.5);
    for nonsense in ["0", "-30", "", "soon", "NaN", "inf"] {
        assert_eq!(
            scale_of(Some(nonsense)),
            1.0,
            "{nonsense:?} must not shorten a deadline"
        );
    }
    assert!(perf_budget_secs(30.0) >= 30.0);
}

/// Pairs a test name with the function that carries it, so the two cannot
/// disagree: `stringify!` and the `as fn()` coercion read the same identifier,
/// and a rename that leaves the config behind fails to compile here rather than
/// dropping an override quietly.
macro_rules! named {
    ($($f:ident),+ $(,)?) => { [$((stringify!($f), $f as fn())),+] };
}

/// Two tests here time a wall clock, and `.config/nextest.toml` runs them with
/// the machine to themselves for a reason worth writing down.
///
/// Every other test in this suite is a cpu-bound simulation and nextest runs
/// as many at once as the machine has cores, so a gate measured alongside them
/// is measured on a machine with nothing left over. A p99 across a 600 us
/// region does not survive that. windows-latest reported the broadcast tick at
/// p99 19251 us against a 1213 us median and a 10000 us budget, on a
/// comments-only change (#276), and the same shape reproduces on demand: on a
/// 14-core laptop `tick_budget_at_capacity` measures p99 684 us against a
/// median of 601 us with the machine idle, and p99 9725 us against a median of
/// 946 us with 14 busy cores beside it. The median moved 1.6x and the tail
/// moved 14x, which is what preemption looks like and not what tick cost looks
/// like.
///
/// The rest of the list is the tests that print a measurement of the simulation
/// rather than of the machine. Those need no isolation, only publishing: the
/// default profile discards a passing test's stdout, which is why one
/// measurement line exists in 935 harness jobs of history (#283).
///
/// Either way it pairs a name in a toml file with a name in this one, which is
/// the kind of pair that comes apart without anyone noticing. This is the half
/// that notices. The session suite has its own copy for the two names that live
/// over there.
#[test]
fn the_measured_tests_are_named_in_the_nextest_config() {
    const CONFIG: &str = include_str!("../../../.config/nextest.toml");
    for (name, _) in named![
        tick_budget_at_capacity,
        perf_sanity_sixty_seconds_regional,
        latency_lan_fiber,
        latency_regional_fiber,
        latency_dsl,
        latency_at_capacity,
        drift_200ppm_stays_bounded,
        drift_200ppm_with_resampler,
        driver_stall_reanchors_and_audio_returns,
        soak_ten_minutes_regional,
    ] {
        assert!(
            CONFIG.contains(&format!("test(={name})")),
            ".config/nextest.toml no longer names {name}, so whatever it measures is \
             either being timed on a busy machine or being printed into a void. \
             Filters there are exact matches, so a rename has to be made in both \
             places or in neither."
        );
    }
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
    let budget = perf_budget_secs(REFERENCE_LAPTOP_SECS);
    assert!(
        wall < budget,
        "60 s regional-fiber scenario took {wall:.2} s wall (budget {budget:.0} s)"
    );
}

// --- A session at capacity.
//
// Every gate above runs 2 to 6 members, and that is exactly where a cost
// paid per member hides: multiplied by 4 it is noise, multiplied by
// MAX_MUSICIANS + MAX_LISTENERS it is the tick budget. The room here is
// built from limits.rs, so raising a cap raises what is gated.

/// Every seat taken, musician 0 emitting one impulse per 200 ms and the other
/// nine silent (a second signal in the mix would trip impulse detection).
/// Returns the median mouth-to-ear latency at musician 1 over 8 s of virtual
/// time after a 2 s settle.
fn capacity_latency_ms(profile_name: &str, seed: u64) -> f32 {
    let mut s = ScenarioBuilder::new(seed)
        .profile(profiles::profile(profile_name))
        .musicians(MAX_MUSICIANS)
        .listeners(MAX_LISTENERS)
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
        "{profile_name} at capacity: expected ~40 impulses in 8 s, detected {}",
        latencies.len()
    );
    median(latencies)
}

// Same three profiles and the same product gates as the two-musician runs
// above, on a full room, plus the one those gates cannot be. Latency is set
// by the tick schedule, the wire and the playout cushion, not by how many
// people are in the session, so the capacity median and the two-musician
// median are the same number: 14.67 / 24.31 / 69.75 ms, and the same on either
// seed. The gate that this test alone can hold is the difference between the
// two rooms rather than the band.
#[test]
fn latency_at_capacity() {
    // A tenth of a tick. The two rooms come out bit-identical today and the
    // slack is for a sample or two of platform difference in the impulse
    // detector, not for a cost that arrives with the tenth musician.
    const ROOM_SIZE_MS: f32 = 0.25;
    // Profile, seed, physical floor (2 x one-way), gate. Derivations sit with
    // the two-musician gates above.
    for (profile, seed, floor, gate) in [
        ("lan-fiber", 0xA4u64, 1.0f32, 16.0f32),
        ("regional-fiber", 0xA5, 12.0, 30.0),
        ("dsl-cross-country", 0xA6, 45.0, 72.0),
    ] {
        let m = capacity_latency_ms(profile, seed);
        let two = median_latency_ms(profile, seed);
        println!(
            "{profile} at capacity ({MAX_MUSICIANS} musicians, {MAX_LISTENERS} listeners) \
             median mouth-to-ear: {m:.2} ms, against {two:.2} ms with two musicians"
        );
        assert!(
            m >= floor,
            "{profile} at capacity: median {m:.2} ms is below the {floor:.0} ms physical \
             floor; measurement is broken"
        );
        assert!(
            m <= gate,
            "{profile} at capacity: median mouth-to-ear {m:.2} ms exceeds the {gate:.0} ms gate"
        );
        assert!(
            (m - two).abs() <= ROOM_SIZE_MS,
            "{profile}: median {m:.2} ms at capacity against {two:.2} ms with two \
             musicians. Room size is not supposed to be in this number, so this is \
             per-member cost on the latency path"
        );
    }
}

// The server keeps up at capacity.
//
// A tick that overruns its 2.5 ms does not error. `MissedTickBehavior::Burst`
// in the server binary means the runtime fires the ticks it missed back to
// back, which reaches clients as arrival jitter their buffers absorb by
// growing, so the symptom is latency creeping up over a session and no test
// anywhere going red. That is how a 20x broadcast encode shipped.
//
// Three things are asserted, because no one of them covers the others.
//
// 1. The deadline itself: the p99 of the broadcast tick against 2500 us,
//    scaled by the one runner multiplier every wall-clock gate here takes.
//    p99 and not a mean, because the deadline is per tick and a mean over all
//    ticks divides the expensive one into the seven that are not. This is the
//    only assertion that fails when the whole tick gets slower, which the two
//    below cannot see. It is also the one that needs the machine to itself,
//    which is why ci.yml runs this test alone; see
//    `the_timed_gates_are_named_in_the_workflow`.
// 2. The fanout ratio, broadcast median over ordinary median, at a limit of 5.
//    Dimensionless, so it is tight without a knob, and it answers exactly one
//    question: has per-listener work come back into the broadcast path. It is
//    blind to everything else and worse than blind to shared cost, because
//    the ratio is 1 + fanout/ordinary, so doubling the work both ticks share
//    moves it from 1.95 down to 1.47. That is why 1 exists.
// 3. One broadcast encode per eight ticks, from `ServerCore::broadcast_encodes`.
//    A count, not a timing, so it is deterministic on any machine.
//
// Measured on an M4 Max, 10 musicians and 20 listeners, 20 s of virtual time:
// broadcast median 607 us, p99 704 us, ordinary 285 us, ratio 1.95, amortized
// 339 us. With the pre-#78 per-listener broadcast encode restored: 4357 us and
// 300 us, ratio 14.53. The ratio gate at 5 sits 2.5x above what the fanout
// costs today and 2.9x below what one extra per-listener encode costs.
//
// And on the ci runners, which is where the budget is actually checked. These
// are the first numbers this gate ever published: a passing test's output went
// nowhere until #276, which is how #239 came to calibrate against an estimate.
// Runner multiplier 4, so a 10000 us budget. Three runs per platform, so the
// spread below is run-to-run variation on an idle runner and not a
// distribution:
//
//   ubuntu-latest    median  675 to 825   p99  714 to  875    7 to  9% of budget
//   macos-latest     median  681 to 744   p99  897 to 1663    9 to 17% of budget
//   windows-latest   median  705 to 895   p99 1042 to 1092   10 to 11% of budget
//
// macos is the loose one, up to 2.4x its own median even with the runner to
// itself, and windows is now the steadiest of the three. If this gate ever
// needs a per-platform allowance it will be for macos, which is not where
// anybody was looking.
//
// The same Windows runner, measured while the rest of the suite was running:
// median 1213 us, p99 19251 us, max 33057 us, 193% of budget. The median
// barely moved and the tail moved seventeen fold. That is why the two timed
// gates get the runner to themselves and why the budget did not change.
#[test]
fn tick_budget_at_capacity() {
    // 20 ms broadcast frame accumulated over 2.5 ms master ticks.
    const TICKS_PER_BROADCAST: usize = 8;
    const FANOUT_GATE: f64 = 5.0;
    // The mix tick's slot in the latency budget in protocol.md, in
    // microseconds. Not a number to tune: it is the frame duration, and a tick
    // that takes longer than the audio it produces falls behind for good.
    const TICK_BUDGET_US: f64 = 2_500.0;
    // Measured 5.6 s here; the same generosity the 60 s reference run takes.
    const WALL_BUDGET_SECS: f64 = 35.0;

    let start = Instant::now();
    let mut b = ScenarioBuilder::new(0x92)
        .profile(profiles::profile("regional-fiber"))
        .musicians(MAX_MUSICIANS)
        .listeners(MAX_LISTENERS)
        .keep_audio(false)
        .measure_tick_cost(true);
    // Every musician playing: the mixer sums ten live sources and each
    // personal mix is a distinct signal, so no encoder gets the easy job.
    for i in 0..MAX_MUSICIANS {
        b = b.source(
            i,
            Source::Sine {
                hz: 110.0 * (i + 1) as f32,
                amp: 0.2,
            },
        );
    }
    let mut s = b.build();
    s.join_all_or_panic(4_000);
    s.run_ms(2_000);
    // Handshake and settle ticks do work no steady-state tick does.
    s.reset_tick_cost();
    let mark = s.current_tick();
    s.run_ms(20_000);
    let end = s.current_tick();
    let wall = start.elapsed().as_secs_f64();
    let cost = s.tick_cost();

    let budget_us = TICK_BUDGET_US * perf_scale();
    println!(
        "capacity tick cost ({MAX_MUSICIANS} musicians, {MAX_LISTENERS} listeners): \
         broadcast median {:.0} us, p99 {:.0} us, max {:.0} us over {} ticks; \
         ordinary median {:.0} us over {} ticks; ratio {:.2}; amortized {:.0} us; \
         p99 is {:.0}% of the {budget_us:.0} us budget on this machine; {wall:.2} s wall",
        cost.broadcast_median_us,
        cost.broadcast_p99_us,
        cost.broadcast_max_us,
        cost.broadcast_ticks,
        cost.ordinary_median_us,
        cost.ordinary_ticks,
        cost.fanout_ratio(),
        cost.amortized_mean_us,
        100.0 * cost.broadcast_p99_us / budget_us,
    );

    // The room really was full for the whole measurement, and the listeners
    // really were being fed: a silent broadcast path would make the tick
    // being timed cheap and the ratio meaningless. Ten sines at 0.2 amp mix
    // to ~0.3 rms under the broadcast limiter, so 0.05 is a wide margin.
    assert_eq!(s.musicians_connected(), MAX_MUSICIANS);
    let connected = s
        .server_member_stats()
        .iter()
        .filter(|m| m.connected)
        .count();
    assert_eq!(connected, MAX_MUSICIANS + MAX_LISTENERS);
    for listener in [MAX_MUSICIANS, MAX_MUSICIANS + MAX_LISTENERS - 1] {
        let rms = s.rms_of(listener, mark, end);
        assert!(
            rms > 0.05,
            "listener {listener} rms {rms:.4} at capacity (gate 0.05); \
             the broadcast path being timed is not carrying audio"
        );
    }

    // One broadcast per 20 ms and no more, give or take where the measurement
    // window opened in the accumulator's cycle. Fanning out every tick would
    // multiply both the encode work and the host's egress bill by eight.
    //
    // Counted twice on purpose. `broadcast_ticks` is read off the datagrams
    // that left the server, so it also catches a fanout that stopped; the
    // encode count comes from `ServerCore` itself, so it catches one encode
    // per listener without depending on how long that takes. Neither is a
    // timing, so both hold on any machine.
    let total = cost.broadcast_ticks + cost.ordinary_ticks;
    let expected = total / TICKS_PER_BROADCAST;
    assert!(
        cost.broadcast_ticks.abs_diff(expected) <= 1,
        "listeners were fed on {} of {total} ticks, expected one in {TICKS_PER_BROADCAST}",
        cost.broadcast_ticks
    );
    assert_eq!(
        cost.broadcast_encodes as usize, cost.broadcast_ticks,
        "{} broadcast frames encoded over {} fanout ticks at {MAX_LISTENERS} listeners; \
         one encode shared by every listener is the whole point of #78",
        cost.broadcast_encodes, cost.broadcast_ticks
    );

    // The deadline. This is the assertion the ratio below cannot make: a
    // slowdown spread evenly over every tick leaves the ratio alone or lowers
    // it, and lands here.
    assert!(
        cost.broadcast_p99_us < budget_us,
        "the broadcast tick's p99 is {:.0} us against a {budget_us:.0} us budget \
         ({:.0} us of deadline x {:.2} for this machine). Median {:.0} us, max {:.0} us, \
         ordinary {:.0} us. The mix tick has to produce 2.5 ms of audio in under 2.5 ms.",
        cost.broadcast_p99_us,
        TICK_BUDGET_US,
        perf_scale(),
        cost.broadcast_median_us,
        cost.broadcast_max_us,
        cost.ordinary_median_us,
    );

    assert!(
        cost.fanout_ratio() <= FANOUT_GATE,
        "a broadcast tick costs {:.2}x an ordinary tick at capacity (gate {FANOUT_GATE:.0}x): \
         {:.0} us against {:.0} us. Work priced per listener has come back into the \
         broadcast path.",
        cost.fanout_ratio(),
        cost.broadcast_median_us,
        cost.ordinary_median_us,
    );

    // Whole-run backstop, which also covers the work outside the tick: the
    // simulated network, the client cores, and the harness itself. Coarse on
    // purpose, at ci generosity it catches an order of magnitude.
    let budget = perf_budget_secs(WALL_BUDGET_SECS);
    assert!(
        wall < budget,
        "22 s of virtual time at capacity took {wall:.2} s wall (budget {budget:.0} s)"
    );
}
