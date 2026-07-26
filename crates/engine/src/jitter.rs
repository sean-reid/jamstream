//! Adaptive jitter buffer over encoded frames. The consumer clock drives it:
//! one `pull` per mix tick, `push` whenever a packet arrives. Time is
//! measured in ticks, so the buffer is deterministic and testable offline.

use std::collections::{BTreeMap, VecDeque};

const MIN_TARGET: usize = 1;
const MAX_TARGET: usize = 24;
/// Multiplier from mean absolute jitter to target depth, a cheap stand-in
/// for the 95th percentile the protocol doc asks for.
const TARGET_FACTOR: f64 = 3.0;
/// Consecutive over-target ticks before one frame is dropped to shrink.
const SHRINK_PATIENCE: u32 = 16;
/// Sequence discontinuity beyond this is a stream restart, not reordering.
const RESET_JUMP: i64 = 512;
const LOSS_WINDOW: usize = 256;
const MAX_BUFFERED: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPacket {
    pub seq: u32,
    pub timestamp: u64,
    pub payload: Vec<u8>,
    pub redundant: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pull {
    Frame(Vec<u8>),
    /// The frame itself was lost; this is the copy a later packet carried.
    Recovered(Vec<u8>),
    /// Nothing usable; caller runs packet loss concealment.
    Missing,
    /// Buffer still filling at start or after a reset.
    Waiting,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JitterStats {
    pub depth_frames: usize,
    pub target_frames: usize,
    pub jitter_frames_ewma: f32,
    pub lost: u64,
    pub recovered: u64,
    pub late: u64,
    /// Concealed frames whose packet arrived one tick later and was played
    /// after all. Each one already counted in `lost` when it was concealed.
    pub resurrected: u64,
}

#[derive(Debug, Default)]
pub struct JitterBuffer {
    frames: BTreeMap<u32, Vec<u8>>,
    redundant: BTreeMap<u32, Vec<u8>>,
    next_seq: Option<u32>,
    running: bool,
    tick: u64,
    last_arrival: Option<(u64, u32)>,
    jitter_ewma: f64,
    over_ticks: u32,
    held_last: bool,
    drop_pending: bool,
    lost: u64,
    recovered: u64,
    late: u64,
    resurrected: u64,
    /// Seq the most recent pull concealed (Missing with nothing usable).
    /// While set, `next_seq == concealed + 1`; delivering any frame clears it.
    concealed: Option<u32>,
    loss_window: VecDeque<bool>,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, packet: MediaPacket) {
        let anchor = self.next_seq.or(self.last_arrival.map(|(_, s)| s));
        if let Some(anchor) = anchor {
            let jump = i64::from(packet.seq) - i64::from(anchor);
            if jump.abs() > RESET_JUMP {
                self.reset();
            }
        }

        // RFC 3550 interarrival jitter, in frame units: seq doubles as the
        // send timestamp because frames are produced one per tick.
        if let Some((prev_tick, prev_seq)) = self.last_arrival {
            let d = (self.tick as i64 - prev_tick as i64)
                - (i64::from(packet.seq) - i64::from(prev_seq));
            self.jitter_ewma += (d.abs() as f64 - self.jitter_ewma) / 16.0;
        }
        self.last_arrival = Some((self.tick, packet.seq));

        if let Some(next) = self.next_seq
            && packet.seq < next
        {
            // Bounded resurrect: the previous pull concealed exactly this seq
            // and nothing has played since, so the PLC tick already absorbed
            // the gap. Stepping back one frame stretches the timeline by one
            // tick and re-aligns a consumer that overran a slow sender.
            // Anything older, or anything already delivered (Recovered clears
            // the marker), stays late.
            if self.concealed == Some(packet.seq) && packet.seq.wrapping_add(1) == next {
                self.concealed = None;
                self.resurrected += 1;
                self.frames.insert(packet.seq, packet.payload);
                self.next_seq = Some(packet.seq);
                // Its redundant copy covers seq - 1, which already played.
                return;
            }
            self.late += 1;
            return;
        }
        if let Some(red) = packet.redundant {
            let prev = packet.seq.wrapping_sub(1);
            if self.next_seq.is_none_or(|next| prev >= next) {
                self.redundant.entry(prev).or_insert(red);
            }
        }
        self.frames.entry(packet.seq).or_insert(packet.payload);
        while self.frames.len() > MAX_BUFFERED {
            self.frames.pop_first();
            self.late += 1;
        }
    }

    pub fn pull(&mut self) -> Pull {
        self.tick += 1;
        let target = self.target_frames();

        if !self.running {
            if self.frames.len() >= target && !self.frames.is_empty() {
                self.next_seq = self.frames.keys().next().copied();
                self.running = true;
            } else {
                return Pull::Waiting;
            }
        }
        let mut next = self.next_seq.expect("running implies next_seq");

        // Growth: when the target has risen above the current depth, hold a
        // tick (caller conceals) instead of consuming, at most every other
        // tick so audio keeps moving.
        if self.frames.len() < target && self.frames.contains_key(&next) && !self.held_last {
            self.held_last = true;
            return Pull::Missing;
        }
        self.held_last = false;

        // Shrink: a persistent surplus from the previous ticks drops exactly
        // one frame here, trading one frame of audio for one frame of latency.
        if self.drop_pending && self.frames.contains_key(&next) {
            self.drop_pending = false;
            self.frames.remove(&next);
            self.redundant.remove(&next);
            next += 1;
            // The skipped frame supersedes any concealed predecessor.
            self.concealed = None;
        }

        let result = if let Some(payload) = self.frames.remove(&next) {
            self.concealed = None;
            self.note_loss(false);
            Pull::Frame(payload)
        } else if let Some(copy) = self.redundant.remove(&next) {
            self.concealed = None;
            self.recovered += 1;
            self.note_loss(true);
            Pull::Recovered(copy)
        } else {
            self.concealed = Some(next);
            self.lost += 1;
            self.note_loss(true);
            Pull::Missing
        };

        let next = next + 1;
        self.next_seq = Some(next);
        self.frames = self.frames.split_off(&next);
        self.redundant = self.redundant.split_off(&next);

        // One frame of slack over target: app-layer redundancy is only
        // usable when the successor packet is already here, so a stable
        // depth of target + 1 must not trigger the shrink path.
        if self.frames.len() > target + 1 {
            self.over_ticks += 1;
            if self.over_ticks >= SHRINK_PATIENCE {
                self.over_ticks = 0;
                self.drop_pending = true;
            }
        } else {
            self.over_ticks = 0;
        }
        result
    }

    pub fn stats(&self) -> JitterStats {
        JitterStats {
            depth_frames: self.frames.len(),
            target_frames: self.target_frames(),
            jitter_frames_ewma: self.jitter_ewma as f32,
            lost: self.lost,
            recovered: self.recovered,
            late: self.late,
            resurrected: self.resurrected,
        }
    }

    /// Fraction of recent ticks whose frame did not arrive intact, recovered
    /// or not. Feeds the sender's redundancy decision, so wire loss counts
    /// even when the redundant copy papered over it.
    pub fn loss_ratio_recent(&self) -> f32 {
        if self.loss_window.is_empty() {
            return 0.0;
        }
        let losses = self.loss_window.iter().filter(|&&l| l).count();
        losses as f32 / self.loss_window.len() as f32
    }

    fn target_frames(&self) -> usize {
        let derived = (self.jitter_ewma * TARGET_FACTOR).round() as usize + 1;
        derived.clamp(MIN_TARGET, MAX_TARGET)
    }

    fn note_loss(&mut self, lost: bool) {
        self.loss_window.push_back(lost);
        while self.loss_window.len() > LOSS_WINDOW {
            self.loss_window.pop_front();
        }
    }

    fn reset(&mut self) {
        self.frames.clear();
        self.redundant.clear();
        self.next_seq = None;
        self.running = false;
        self.last_arrival = None;
        self.over_ticks = 0;
        self.held_last = false;
        self.drop_pending = false;
        self.concealed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(seq: u32, redundant: Option<u32>) -> MediaPacket {
        MediaPacket {
            seq,
            timestamp: u64::from(seq) * 120,
            payload: payload_for(seq),
            redundant: redundant.map(payload_for),
        }
    }

    fn payload_for(seq: u32) -> Vec<u8> {
        seq.to_le_bytes().to_vec()
    }

    /// Pulls through any growth-hold concealment ticks (late arrivals
    /// register as jitter and may raise the target) to the next real frame.
    fn pull_next_frame(jb: &mut JitterBuffer) -> Vec<u8> {
        for _ in 0..4 {
            match jb.pull() {
                Pull::Frame(p) => return p,
                Pull::Missing => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        panic!("no frame within 4 pulls");
    }

    #[test]
    fn in_order_steady_serves_every_frame() {
        let mut jb = JitterBuffer::new();
        for seq in 0..100u32 {
            jb.push(packet(seq, None));
            assert_eq!(jb.pull(), Pull::Frame(payload_for(seq)));
        }
        let stats = jb.stats();
        assert_eq!(stats.lost, 0);
        assert_eq!(stats.recovered, 0);
        assert_eq!(stats.late, 0);
        assert_eq!(stats.target_frames, 1);
        assert_eq!(jb.loss_ratio_recent(), 0.0);
    }

    #[test]
    fn one_in_ten_loss_recovers_from_redundancy() {
        let mut jb = JitterBuffer::new();
        // Sender runs one packet ahead of the consumer so the redundant
        // copy of a lost frame is on hand when its slot comes up.
        jb.push(packet(0, None));
        jb.push(packet(1, Some(0)));
        let mut recovered = 0;
        for seq in 0..100u32 {
            let arriving = seq + 2;
            if arriving % 10 != 3 {
                jb.push(packet(arriving, Some(arriving - 1)));
            }
            match jb.pull() {
                Pull::Frame(p) => assert_eq!(p, payload_for(seq)),
                Pull::Recovered(p) => {
                    assert_eq!(p, payload_for(seq));
                    recovered += 1;
                }
                other => panic!("seq {seq}: unexpected {other:?}"),
            }
        }
        assert_eq!(recovered, 10);
        let stats = jb.stats();
        assert_eq!(stats.recovered, 10);
        assert_eq!(stats.lost, 0);
        assert!((jb.loss_ratio_recent() - 0.1).abs() < 0.02);
    }

    #[test]
    fn duplicate_flood_is_ignored() {
        let mut jb = JitterBuffer::new();
        for _ in 0..20 {
            jb.push(packet(0, None));
        }
        assert_eq!(jb.stats().depth_frames, 1);
        assert_eq!(jb.pull(), Pull::Frame(payload_for(0)));
        assert_eq!(jb.stats().depth_frames, 0);
        // Replays of an already played seq count as late and are dropped.
        for _ in 0..5 {
            jb.push(packet(0, None));
        }
        assert_eq!(jb.stats().depth_frames, 0);
        assert_eq!(jb.stats().late, 5);
    }

    #[test]
    fn reorder_within_three_plays_in_order() {
        let mut jb = JitterBuffer::new();
        for seq in [0u32, 1, 2, 3] {
            jb.push(packet(seq, None));
        }
        let mut arrivals: Vec<u32> = Vec::new();
        for base in (4..64u32).step_by(3) {
            arrivals.extend([base + 2, base, base + 1]);
        }
        // Reordering registers as jitter, so the buffer may stretch with
        // concealment ticks; the contract is that what plays is contiguous
        // and in order with nothing declared lost or late.
        let mut played = Vec::new();
        for &seq in &arrivals {
            jb.push(packet(seq, None));
            match jb.pull() {
                Pull::Frame(p) => played.push(p),
                Pull::Missing => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(played.len() >= 40);
        for (i, p) in played.iter().enumerate() {
            assert_eq!(*p, payload_for(i as u32));
        }
        assert_eq!(jb.stats().lost, 0);
        assert_eq!(jb.stats().late, 0);
    }

    #[test]
    fn burst_gap_conceals_then_recovers() {
        let mut jb = JitterBuffer::new();
        let mut outcomes = Vec::new();
        for seq in 0..30u32 {
            if !(10..15).contains(&seq) {
                jb.push(packet(seq, None));
            }
            outcomes.push(jb.pull());
        }
        for (seq, outcome) in outcomes.iter().enumerate() {
            let seq = seq as u32;
            if (10..15).contains(&seq) {
                assert_eq!(*outcome, Pull::Missing, "seq {seq}");
            } else {
                assert_eq!(*outcome, Pull::Frame(payload_for(seq)), "seq {seq}");
            }
        }
        assert_eq!(jb.stats().lost, 5);
    }

    #[test]
    fn target_grows_under_jitter_and_shrinks_back() {
        let mut jb = JitterBuffer::new();
        let mut seq = 0u32;
        // Bursty arrivals: two packets every other tick, none in between.
        for tick in 0..200 {
            if tick % 2 == 0 {
                jb.push(packet(seq, None));
                jb.push(packet(seq + 1, None));
                seq += 2;
            }
            jb.pull();
        }
        let grown = jb.stats();
        assert!(
            grown.target_frames >= 3,
            "target should grow, got {}",
            grown.target_frames
        );
        // Steady arrivals decay the estimate and the buffer drains back.
        for _ in 0..400 {
            jb.push(packet(seq, None));
            seq += 1;
            jb.pull();
        }
        let settled = jb.stats();
        assert_eq!(settled.target_frames, 1);
        assert!(
            settled.depth_frames <= 2,
            "depth should shrink, got {}",
            settled.depth_frames
        );
    }

    #[test]
    fn target_respects_clamp_under_extreme_jitter() {
        let mut jb = JitterBuffer::new();
        let mut seq = 0u32;
        let mut max_target = 0;
        let mut check = |jb: &JitterBuffer| {
            let stats = jb.stats();
            assert!(stats.target_frames >= 1);
            assert!(stats.target_frames <= 24);
            assert!(stats.depth_frames <= MAX_BUFFERED);
            max_target = max_target.max(stats.target_frames);
        };
        for tick in 0..600 {
            if tick % 200 == 0 {
                for _ in 0..200 {
                    jb.push(packet(seq, None));
                    seq += 1;
                    check(&jb);
                }
            }
            jb.pull();
            check(&jb);
        }
        // The raw jitter estimate wants far more than 24 frames here.
        assert_eq!(max_target, 24);
    }

    #[test]
    fn big_seq_jump_resets() {
        let mut jb = JitterBuffer::new();
        for seq in 0..10u32 {
            jb.push(packet(seq, None));
            assert_eq!(jb.pull(), Pull::Frame(payload_for(seq)));
        }
        jb.push(packet(1_000_000, None));
        assert_eq!(jb.pull(), Pull::Frame(payload_for(1_000_000)));
        jb.push(packet(1_000_001, None));
        assert_eq!(jb.pull(), Pull::Frame(payload_for(1_000_001)));
        // A jump backwards resets too: a client restarting its stream at
        // zero must not be treated as a million late packets.
        jb.push(packet(3, None));
        assert_eq!(jb.pull(), Pull::Frame(payload_for(3)));
        assert_eq!(jb.stats().lost, 0);
        assert_eq!(jb.stats().late, 0);
    }

    #[test]
    fn waiting_until_first_packet() {
        let mut jb = JitterBuffer::new();
        assert_eq!(jb.pull(), Pull::Waiting);
        assert_eq!(jb.pull(), Pull::Waiting);
        jb.push(packet(5, None));
        assert_eq!(jb.pull(), Pull::Frame(payload_for(5)));
    }

    #[test]
    fn slow_sender_drift_resurrects_and_stays_bounded() {
        // Emulates a -ppm sender the way the harness does: the sender's
        // frame counter crosses one boundary fewer every SLIP ticks, so the
        // consumer periodically overruns it by exactly one frame.
        const SLIP: u64 = 50;
        let mut jb = JitterBuffer::new();
        let mut emitted = 0u32;
        let mut expected = 0u32;
        let mut conceals = 0u64;
        // 999 ticks, not 1000: the run must not end on a conceal tick or the
        // final resurrect has no tick left to land in.
        for tick in 1..=999u64 {
            let due = (tick - tick / SLIP) as u32;
            while emitted < due {
                jb.push(packet(emitted, None));
                emitted += 1;
            }
            match jb.pull() {
                Pull::Frame(p) => {
                    assert_eq!(p, payload_for(expected), "tick {tick}");
                    expected += 1;
                }
                Pull::Missing => conceals += 1,
                other => panic!("tick {tick}: unexpected {other:?}"),
            }
        }
        let stats = jb.stats();
        // Without resurrection this diverges to one loss per tick (~950 by
        // the end); with it, losses stay bounded by the drift rate.
        assert!(
            stats.lost <= 999 / SLIP + 1,
            "lost should stay bounded, got {}",
            stats.lost
        );
        assert!(stats.lost > 0, "the drift must actually cause conceals");
        assert_eq!(stats.resurrected, stats.lost);
        assert_eq!(stats.late, 0);
        // Every emitted frame played: delivery is continuous around each
        // conceal/resurrect pair, nothing is skipped.
        assert_eq!(u64::from(expected) + conceals, 999);
        assert_eq!(expected, emitted);
    }

    #[test]
    fn late_arrival_of_recovered_frame_does_not_resurrect() {
        let mut jb = JitterBuffer::new();
        jb.push(packet(0, None));
        assert_eq!(jb.pull(), Pull::Frame(payload_for(0)));
        // Seq 1 lost on the wire but covered by 2's redundant copy.
        jb.push(packet(2, Some(1)));
        assert_eq!(jb.pull(), Pull::Recovered(payload_for(1)));
        // The direct copy straggles in: 1 was already delivered, so it must
        // count late and not rewind playout.
        jb.push(packet(1, None));
        assert_eq!(pull_next_frame(&mut jb), payload_for(2));
        let stats = jb.stats();
        assert_eq!(stats.late, 1);
        assert_eq!(stats.resurrected, 0);
        assert_eq!(stats.recovered, 1);
    }

    #[test]
    fn only_the_immediately_previous_conceal_resurrects() {
        let mut jb = JitterBuffer::new();
        jb.push(packet(0, None));
        assert_eq!(jb.pull(), Pull::Frame(payload_for(0)));
        // Two consecutive conceals; only the latest (2) is resurrectable.
        assert_eq!(jb.pull(), Pull::Missing);
        assert_eq!(jb.pull(), Pull::Missing);
        jb.push(packet(1, None));
        assert_eq!(jb.stats().late, 1);
        assert_eq!(jb.stats().resurrected, 0);
        jb.push(packet(2, None));
        assert_eq!(pull_next_frame(&mut jb), payload_for(2));
        assert_eq!(jb.stats().resurrected, 1);
        assert_eq!(jb.stats().lost, 2);
    }

    #[test]
    fn delivery_after_conceal_clears_the_resurrect_window() {
        let mut jb = JitterBuffer::new();
        jb.push(packet(0, None));
        assert_eq!(jb.pull(), Pull::Frame(payload_for(0)));
        assert_eq!(jb.pull(), Pull::Missing); // seq 1 concealed
        jb.push(packet(2, None));
        assert_eq!(jb.pull(), Pull::Frame(payload_for(2)));
        // Seq 1 is now two frames behind playout; resurrecting it would
        // replay it out of order, so it must be dropped as late.
        jb.push(packet(1, None));
        assert_eq!(jb.stats().late, 1);
        assert_eq!(jb.stats().resurrected, 0);
    }

    #[test]
    fn stats_counters_track_events() {
        let mut jb = JitterBuffer::new();
        jb.push(packet(0, None));
        assert_eq!(jb.pull(), Pull::Frame(payload_for(0)));
        // seq 1 lost with no cover.
        jb.push(packet(2, None));
        assert_eq!(jb.pull(), Pull::Missing);
        assert_eq!(jb.pull(), Pull::Frame(payload_for(2)));
        // seq 3 lost, covered by 4's redundant copy.
        jb.push(packet(4, Some(3)));
        assert_eq!(jb.pull(), Pull::Recovered(payload_for(3)));
        assert_eq!(jb.pull(), Pull::Frame(payload_for(4)));
        // Late straggler.
        jb.push(packet(1, None));
        let stats = jb.stats();
        assert_eq!(stats.lost, 1);
        assert_eq!(stats.recovered, 1);
        assert_eq!(stats.late, 1);
        assert!(jb.loss_ratio_recent() > 0.0);
    }
}
