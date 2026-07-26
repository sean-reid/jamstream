use jamstream_protocol::control::{ControlLink, ControlMsg};
use jamstream_protocol::ids::MemberId;
use jamstream_protocol::media::{FrameDuration, MediaFrame};
use jamstream_protocol::replay::ReplayWindow;
use proptest::prelude::*;

fn duration_strategy() -> impl Strategy<Value = FrameDuration> {
    prop_oneof![
        Just(FrameDuration::Ms2_5),
        Just(FrameDuration::Ms5),
        Just(FrameDuration::Ms10),
        Just(FrameDuration::Ms20),
    ]
}

proptest! {
    #[test]
    fn media_frame_round_trips(
        seq in any::<u32>(),
        timestamp in any::<u64>(),
        duration in duration_strategy(),
        stereo in any::<bool>(),
        payload in prop::collection::vec(any::<u8>(), 0..400),
        redundant in prop::option::of(prop::collection::vec(any::<u8>(), 0..400)),
    ) {
        let frame = MediaFrame {
            seq,
            timestamp,
            duration,
            stereo,
            payload: &payload,
            redundant: redundant.as_deref(),
        };
        let decoded_bytes = frame.encode();
        let decoded = MediaFrame::decode(&decoded_bytes).unwrap();
        prop_assert_eq!(decoded, frame);
    }

    #[test]
    fn media_decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..600)) {
        let _ = MediaFrame::decode(&bytes);
    }

    #[test]
    fn replay_window_accepts_each_counter_at_most_once(
        counters in prop::collection::vec(0u64..500, 1..300),
    ) {
        let mut window = ReplayWindow::new();
        let mut accepted = std::collections::HashSet::new();
        for &c in &counters {
            if window.accept(c) {
                prop_assert!(accepted.insert(c), "counter {} accepted twice", c);
            }
        }
    }

    #[test]
    fn replay_window_always_accepts_strictly_increasing(
        steps in prop::collection::vec(1u64..1000, 1..200),
    ) {
        let mut window = ReplayWindow::new();
        let mut counter = 0u64;
        for step in steps {
            counter += step;
            prop_assert!(window.accept(counter));
        }
    }

    #[test]
    fn control_link_delivers_everything_in_order_under_loss(
        loss_seed in any::<u64>(),
        // Up to 4-in-8 dropped: the link is required to converge through
        // 50% loss; beyond that its give-up policy is allowed to fire.
        loss_num in 0u64..5,
        count in 1u64..60,
    ) {
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        let msg = |n: u64| ControlMsg::Chat { from: MemberId(1), text: format!("m{n}") };
        for n in 0..count {
            a.send(msg(n)).unwrap();
        }
        let mut got = Vec::new();
        let mut state = loss_seed | 1;
        let mut now = 0u64;
        // Loss rate up to loss_num/8; generous virtual time to converge.
        while got.len() < count as usize && now < 120_000 {
            for dgram in a.poll(now) {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                if state >> 61 < loss_num {
                    continue;
                }
                got.extend(b.receive(&dgram).unwrap());
            }
            for dgram in b.poll(now) {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                if state >> 61 < loss_num {
                    continue;
                }
                a.receive(&dgram).unwrap();
            }
            now += 50;
        }
        let expected: Vec<_> = (0..count).map(msg).collect();
        prop_assert_eq!(got, expected);
    }

    #[test]
    fn control_receive_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        let mut link = ControlLink::new();
        let _ = link.receive(&bytes);
    }
}
