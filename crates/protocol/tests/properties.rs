use jamstream_protocol::control::{ControlLink, ControlMsg, RECV_WINDOW};
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

    /// Growth, not panics: for any sequence of arriving frames, including the
    /// ones an attacker picks so the gap never closes, the reassembly buffer
    /// stays inside the window `ack_bits` can advertise.
    #[test]
    fn control_reassembly_stays_inside_the_window(
        seqs in prop::collection::vec(0u64..SEQ_COUNT, 1..500),
    ) {
        let dgrams = one_datagram_per_seq();
        let mut link = ControlLink::new();
        for &seq in &seqs {
            let _ = link.receive(&dgrams[seq as usize]);
            // The tight bound, not RECV_WINDOW itself: the frame at
            // `recv_next` drains on arrival, so the slots that can hold a
            // frame are the 31 above it, which is exactly what `ack_bits`
            // advertises.
            prop_assert!(
                link.buffered() < RECV_WINDOW as usize,
                "buffered {} frames after seq {seq}",
                link.buffered()
            );
        }
    }
}

/// Three windows' worth, so most draws are frames the receiver must refuse.
const SEQ_COUNT: u64 = 100;

/// One legal datagram per sequence number in `0..SEQ_COUNT`, built by a real
/// sender so that only the ordering is adversarial.
///
/// Draining the sender takes a peer that acks, because `poll` holds anything
/// at or past the peer's window: one call returns RECV_WINDOW datagrams and
/// no more. Collecting only those left 68 of every 100 draws with no datagram
/// to present, and the 32 that were left are all inside the window from
/// `recv_next` 0, so the bound above held for every input the property could
/// generate whether or not the code enforced it.
fn one_datagram_per_seq() -> Vec<Vec<u8>> {
    let mut sender = ControlLink::new();
    let mut acker = ControlLink::new();
    for n in 0..SEQ_COUNT {
        sender
            .send(ControlMsg::Chat {
                from: MemberId(1),
                text: format!("m{n}"),
            })
            .expect("inside the queue cap");
    }
    let mut out = Vec::with_capacity(SEQ_COUNT as usize);
    while (out.len() as u64) < SEQ_COUNT {
        let batch = sender.poll(0);
        assert!(!batch.is_empty(), "sender stalled at {} frames", out.len());
        for dgram in batch {
            acker.receive(&dgram).expect("a real sender's own datagram");
            out.push(dgram);
        }
        for ack in acker.poll(0) {
            sender.receive(&ack).expect("a real receiver's own ack");
        }
    }
    out
}
