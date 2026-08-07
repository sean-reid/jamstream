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
/// Consecutive stuck ticks (a concealed pull with nothing playable while
/// packets keep being dropped as late) before the buffer re-anchors.
///
/// Derivation, at 2.5 ms per tick. The lower bound is set by the largest
/// transient that legitimately looks like this and heals itself: playout is
/// covered for at most `MAX_TARGET` = 24 frames of buffered audio, and a
/// reorder spike can strand arrivals for the depth of the reorder window (a
/// hostile-wifi 20 ms spike is 8 frames), so ~32 ticks bounds any honest
/// transient; 60 leaves nearly 2x margin, and it is 60x the one-frame reach
/// of the resurrect path, which must keep owning single-frame overruns. The
/// upper bound is the healing deadline: 60 ticks of detection (150 ms) plus a
/// refill of `target` frames (<= 24 frames = 60 ms) is at most ~210 ms of
/// concealment, well inside a second and inside the 250 ms audio-continuity
/// gate the harness holds the media path to.
const REANCHOR_PATIENCE: u32 = 60;

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

#[derive(Debug, Clone, Copy, Default, PartialEq)]
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
    /// Pulls that consumed a slot (Frame, Recovered, or Missing; Waiting and
    /// growth holds excluded). Denominator for loss deltas over a window.
    pub pulled: u64,
    /// Pulls answered with [`Pull::Waiting`], each one a frame of literal
    /// zeros handed to playout because the buffer has not reached target depth
    /// since it was last anchored. The only branch that plays silence rather
    /// than concealment, so a caller can tell "hearing nothing" from
    /// "hearing a concealed stream" and say so (#451).
    pub waiting: u64,
    /// Times the buffer gave up on a playout position it could not reconcile
    /// with the arriving stream and re-anchored on the newest arrivals, exactly
    /// as it does for a stream restart: either every pull concealed while
    /// packets were dropped (see `REANCHOR_PATIENCE`), or the depth reached
    /// `MAX_BUFFERED` with none of it played. Each one costs a refill; a
    /// healthy session never re-anchors.
    pub reanchors: u64,
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
    pulled: u64,
    waiting: u64,
    reanchors: u64,
    /// Seq the most recent pull concealed (Missing with nothing usable).
    /// While set, `next_seq == concealed + 1`; delivering any frame clears it.
    concealed: Option<u32>,
    /// A packet arrived behind the playout position since the last pull and was
    /// dropped as late, which means the arriving stream and the playout
    /// position disagree.
    dropped_since_pull: bool,
    /// Consecutive stuck ticks: pulls that concealed with nothing playable
    /// while `dropped_since_pull` was set. Any delivery clears it.
    stuck_ticks: u32,
    loss_window: VecDeque<bool>,
}

impl JitterBuffer {
    /// Depth at which the buffer stops believing its playout position and
    /// re-anchors, in frames. Published because a consumer that stops pulling
    /// has this long to notice before the buffer gives the position up.
    pub const MAX_DEPTH_FRAMES: usize = MAX_BUFFERED;

    /// Deepest target the adaptive law will ask for, in frames. Published
    /// because it is the deepest a healthy buffer ever sits, so a caller
    /// watching depth can tell a refill from a buffer nobody is draining.
    pub const MAX_TARGET_FRAMES: usize = MAX_TARGET;

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
            self.dropped_since_pull = true;
            return;
        }
        if let Some(red) = packet.redundant {
            let prev = packet.seq.wrapping_sub(1);
            if self.next_seq.is_none_or(|next| prev >= next) {
                self.redundant.entry(prev).or_insert(red);
            }
        }
        self.frames.entry(packet.seq).or_insert(packet.payload);
        // A buffer this deep holds `MAX_BUFFERED` frames of audio the playout
        // position has not reached, against a target that never exceeds
        // `MAX_TARGET`: the two cannot be reconciled, so playout gives up this
        // one and re-anchors on the arrivals still to come. It belongs here
        // rather than in `pull` because a buffer nobody pulls is how a buffer
        // gets this deep, and nothing in `pull` runs then.
        if self.frames.len() > MAX_BUFFERED {
            self.reset();
            self.reanchors += 1;
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
                self.waiting += 1;
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

        // Re-anchor watchdog for the consumer that overran the producer.
        // Between the resurrect path (which steps back exactly one frame) and
        // RESET_JUMP (which treats a discontinuity over 512 frames as a
        // restart) sits a hole: a persistent offset of 2..512 frames with the
        // playout position ahead of the arriving stream, so every packet is
        // dropped as late while every pull conceals, forever. The signature is
        // exactly that: a concealed pull with nothing playable, on a tick that
        // dropped a packet. Held past REANCHOR_PATIENCE it cannot be reordering
        // or a brief stall, so treat it as a stream restart and re-anchor on
        // the newest arrivals. The mirror case, playout behind the stream, is
        // caught in `push` by the depth itself.
        let stuck = matches!(result, Pull::Missing) && self.dropped_since_pull;
        self.dropped_since_pull = false;
        if stuck {
            self.stuck_ticks += 1;
            if self.stuck_ticks >= REANCHOR_PATIENCE {
                // `reset` clears the counter, so this fires once per stuck
                // episode: with no anchor nothing can be late, and the next
                // pulls refill to target before playing again.
                self.reset();
                self.reanchors += 1;
            }
        } else {
            self.stuck_ticks = 0;
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
            pulled: self.pulled,
            waiting: self.waiting,
            reanchors: self.reanchors,
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
        self.pulled += 1;
        self.loss_window.push_back(lost);
        while self.loss_window.len() > LOSS_WINDOW {
            self.loss_window.pop_front();
        }
    }

    /// Drops the stream's anchor and all buffered audio: the next `target`
    /// arrivals re-anchor playout. Idempotent, and the only state it touches
    /// is anchor state - the counters and the jitter estimate carry over, so
    /// `lost`, `late`, and `pulled` keep their meanings across it.
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
        self.dropped_since_pull = false;
        self.stuck_ticks = 0;
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

    fn seq_of(payload: &[u8]) -> u32 {
        u32::from_le_bytes(payload.try_into().expect("4-byte test payload"))
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
        // Nothing was ever played as silence: the first packet was already in
        // hand when the first pull came, so a caller watching `waiting` sees a
        // healthy stream as zero (#451).
        assert_eq!(stats.waiting, 0);
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
        // Each of those pulls handed playout a frame of zeros, and the counter
        // is what lets a caller notice that it is happening.
        assert_eq!(jb.stats().waiting, 2);
        assert_eq!(jb.stats().pulled, 0);
        jb.push(packet(5, None));
        assert_eq!(jb.pull(), Pull::Frame(payload_for(5)));
        // It stops the moment audio plays, so a run of silence is `waiting`
        // moving while `pulled` stands still.
        assert_eq!(jb.stats().waiting, 2);
        assert_eq!(jb.stats().pulled, 1);
    }

    /// A buffer handed nothing at all counts every silent frame it plays: this
    /// is the state a client is in when its user hears nothing for a whole
    /// session, and before #451 no counter named it.
    #[test]
    fn silence_is_counted_for_as_long_as_the_buffer_stays_unfilled() {
        let mut jb = JitterBuffer::new();
        for _ in 0..400 {
            assert_eq!(jb.pull(), Pull::Waiting);
        }
        let stats = jb.stats();
        assert_eq!(stats.waiting, 400);
        assert_eq!(stats.pulled, 0);
        assert_eq!(stats.depth_frames, 0);
        assert_eq!(stats.late, 0);
        assert_eq!(stats.reanchors, 0);
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
        assert_eq!(stats.pulled, 5);
        assert!(jb.loss_ratio_recent() > 0.0);
    }

    /// A buffer whose consumer has overrun its producer by `offset` frames:
    /// 20 ticks in lockstep, then `offset` ticks where the far end's capture
    /// driver produced nothing (the buffer conceals and playout walks on),
    /// leaving the sender permanently `offset` frames behind the playout
    /// position with contiguous sequence numbers. Returns the buffer and the
    /// next seq the sender will use.
    fn overrun_by(offset: u32) -> (JitterBuffer, u32) {
        let mut jb = JitterBuffer::new();
        let mut seq = 0u32;
        for _ in 0..20 {
            jb.push(packet(seq, None));
            assert_eq!(jb.pull(), Pull::Frame(payload_for(seq)));
            seq += 1;
        }
        for _ in 0..offset {
            assert_eq!(jb.pull(), Pull::Missing);
        }
        // The stall alone is honest concealment, not a reason to re-anchor.
        assert_eq!(jb.stats().reanchors, 0);
        (jb, seq)
    }

    /// Playout must never go backwards, and after a re-anchor it must be
    /// essentially gapless: the only frames a healthy re-anchored stream
    /// skips are the shrink path's, one per surplus episode while the jitter
    /// estimate the stall inflated decays back, so `shrinks` is how many
    /// surplus episodes the case's own arrival pattern earns.
    fn assert_playout_forward(case: &str, played: &[u32], shrinks: usize) {
        assert!(!played.is_empty(), "{case}: nothing played");
        for pair in played.windows(2) {
            assert!(pair[1] > pair[0], "{case}: playout went backwards {pair:?}");
        }
        let span = (played[played.len() - 1] - played[0] + 1) as usize;
        let skipped = span - played.len();
        assert!(
            skipped <= shrinks,
            "{case}: {skipped} frames skipped after recovery (shrink allows at most {shrinks})"
        );
    }

    /// Runs a persistent `offset`-frame overrun to its conclusion: one push
    /// and one pull per tick, sequence numbers contiguous. Before the
    /// re-anchor policy this state was permanent - every packet late, `late`
    /// climbing one per tick, depth pinned at 0, playout concealed for the
    /// rest of the session. Now it must heal inside the patience window and
    /// stay healed.
    fn assert_heals_from_overrun(offset: u32) {
        let (mut jb, mut seq) = overrun_by(offset);
        let late_before = jb.stats().late;
        let mut first_frame_tick = None;
        let mut played = Vec::new();
        let mut late_at_first_frame = 0;
        for tick in 1..=600u32 {
            jb.push(packet(seq, None));
            seq += 1;
            match jb.pull() {
                Pull::Frame(p) => {
                    if first_frame_tick.is_none() {
                        first_frame_tick = Some(tick);
                        late_at_first_frame = jb.stats().late;
                    }
                    played.push(seq_of(&p));
                }
                Pull::Missing | Pull::Waiting => {}
                other => panic!("offset {offset}, tick {tick}: unexpected {other:?}"),
            }
        }
        let stats = jb.stats();
        assert_eq!(
            stats.reanchors, 1,
            "offset {offset}: expected exactly one re-anchor, got {stats:?}"
        );
        // Nothing resurrected: the overrun is out of that path's one-frame
        // reach, so the re-anchor is what healed it.
        assert_eq!(stats.resurrected, 0, "offset {offset}: {stats:?}");

        // Healing is bounded: patience ticks of detection plus a refill of at
        // most MAX_TARGET frames, i.e. at most 210 ms, inside a second.
        let at = first_frame_tick.unwrap_or_else(|| panic!("offset {offset}: never recovered"));
        assert!(
            at > REANCHOR_PATIENCE / 2,
            "offset {offset}: re-anchored after only {at} stuck ticks; \
             ordinary reordering would trip it"
        );
        assert!(
            at <= REANCHOR_PATIENCE + MAX_TARGET as u32,
            "offset {offset}: took {at} ticks to recover (patience {REANCHOR_PATIENCE})"
        );

        // Delivery resumed and stayed up for the rest of the run.
        assert!(
            played.len() >= 500,
            "offset {offset}: only {} frames played after recovery",
            played.len()
        );
        assert_playout_forward(&format!("offset {offset}"), &played, 2);

        // `late` stopped climbing once playout re-anchored: the drops are
        // confined to the detection window.
        assert_eq!(
            stats.late, late_at_first_frame,
            "offset {offset}: late kept climbing after the re-anchor"
        );
        assert!(
            stats.late - late_before <= u64::from(REANCHOR_PATIENCE) + 4,
            "offset {offset}: {} late drops during recovery",
            stats.late - late_before
        );
        assert!(stats.depth_frames <= MAX_BUFFERED, "offset {offset}");
    }

    // Five frames: the middle of the hole between the resurrect path and
    // RESET_JUMP, the shape a partial capture-clock catch-up leaves behind.
    #[test]
    fn persistent_five_frame_overrun_reanchors() {
        assert_heals_from_overrun(5);
    }

    // Two frames: one more than the resurrect path can step back.
    #[test]
    fn persistent_two_frame_overrun_reanchors() {
        assert_heals_from_overrun(2);
    }

    // 500 frames: just under RESET_JUMP, so no discontinuity reset saves it.
    #[test]
    fn persistent_500_frame_overrun_reanchors() {
        assert_heals_from_overrun(500);
    }

    /// The mirror image inside the same hole: playout stalls, nothing is
    /// pulled, and arrivals keep coming. `redundant` says whether each packet
    /// carries a copy of its predecessor, which is what the sender attaches
    /// under loss.
    ///
    /// The buffer must give the playout position up while the stall is still
    /// running, because the consumer that would notice is the one that stopped.
    /// It has to hold with the copies as well as without them: a pull that
    /// finds a redundant copy of the frame it wanted is not concealment, so it
    /// resets the watchdog's counter, and a buffer whose every pull is answered
    /// that way stays a stream's length behind for the rest of the session
    /// (#447).
    fn assert_heals_from_a_playout_stall(redundant: bool) {
        let cover = |seq: u32| redundant.then(|| seq.wrapping_sub(1)).filter(|_| seq > 0);
        let mut jb = JitterBuffer::new();
        let mut seq = 0u32;
        for _ in 0..20 {
            jb.push(packet(seq, cover(seq)));
            assert_eq!(jb.pull(), Pull::Frame(payload_for(seq)));
            seq += 1;
        }
        // A frozen consumer: 200 frames arrive with nothing pulled, three times
        // the depth the buffer will hold. 200 stays under RESET_JUMP.
        for _ in 0..200 {
            jb.push(packet(seq, cover(seq)));
            seq += 1;
        }
        let stalled = jb.stats();
        assert!(
            stalled.reanchors > 0,
            "the buffer waited for a pull that was never coming: {stalled:?}"
        );
        assert!(
            stalled.depth_frames <= MAX_BUFFERED,
            "depth pinned at the cap: {stalled:?}"
        );
        // Nothing is refused on the way there: the frames thrown away are the
        // ones the abandoned position wanted, not the arrivals.
        assert_eq!(stalled.late, 0, "{stalled:?}");

        let reanchors_stalled = stalled.reanchors;
        let mut first_frame_tick = None;
        let mut played = Vec::new();
        for tick in 1..=400u32 {
            jb.push(packet(seq, cover(seq)));
            seq += 1;
            match jb.pull() {
                Pull::Frame(p) => {
                    first_frame_tick.get_or_insert(tick);
                    played.push(seq_of(&p));
                }
                Pull::Recovered(p) => {
                    first_frame_tick.get_or_insert(tick);
                    played.push(seq_of(&p));
                }
                Pull::Missing | Pull::Waiting => {}
            }
        }
        let stats = jb.stats();
        // The stall was already resolved, so resuming costs a refill and
        // nothing else.
        assert_eq!(
            stats.reanchors, reanchors_stalled,
            "resuming the pull re-anchored again: {stats:?}"
        );
        let at = first_frame_tick.expect("never recovered from the playout stall");
        assert!(
            at <= MAX_TARGET as u32 + 1,
            "took {at} ticks to play again after a resolved stall"
        );
        assert!(
            played.len() >= 380,
            "only {} frames played after the stall",
            played.len()
        );
        assert_playout_forward("playout stall", &played, 3);
        // And it keeps up from there instead of trailing the stream.
        assert!(
            stats.depth_frames <= stats.target_frames + 1,
            "depth {} still above target {} + 1: {stats:?}",
            stats.depth_frames,
            stats.target_frames
        );
        assert_eq!(
            stats.late, 0,
            "arrivals refused after the stall was over: {stats:?}"
        );
    }

    #[test]
    fn playout_stall_past_max_buffered_reanchors() {
        assert_heals_from_a_playout_stall(false);
    }

    #[test]
    fn playout_stall_covered_by_redundancy_reanchors() {
        assert_heals_from_a_playout_stall(true);
    }

    // Negative control: garden-variety 2% loss with reordering deep enough to
    // strand packets behind playout. Late drops happen, concealment happens,
    // but they never coincide for long, so the buffer must never re-anchor.
    #[test]
    fn ordinary_loss_and_reorder_never_reanchor() {
        let mut jb = JitterBuffer::new();
        let mut lcg = 0x2545_F491_4F6C_DD1Du64;
        let mut draw = move || {
            lcg = lcg
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (lcg >> 33) as u32
        };
        // (arrival tick, seq) for packets held back by the network.
        let mut delayed: Vec<(u32, u32)> = Vec::new();
        let mut delivered = 0u32;
        for tick in 0..5_000u32 {
            let r = draw();
            if r % 50 != 7 {
                // 2% of packets never arrive; 5% arrive two or three ticks
                // late, past their playout slot.
                if r % 20 == 3 {
                    delayed.push((tick + 2 + (r >> 5) % 2, tick));
                } else {
                    jb.push(packet(tick, Some(tick.wrapping_sub(1))));
                }
            }
            delayed.retain(|&(at, seq)| {
                if at > tick {
                    return true;
                }
                jb.push(packet(seq, None));
                false
            });
            if matches!(jb.pull(), Pull::Frame(_) | Pull::Recovered(_)) {
                delivered += 1;
            }
        }
        let stats = jb.stats();
        assert_eq!(
            stats.reanchors, 0,
            "ordinary loss and reordering re-anchored the buffer: {stats:?}"
        );
        // The control is only meaningful if it actually exercised the
        // signature's ingredients and kept a healthy stream running.
        assert!(
            stats.late > 0,
            "no late drops in the control run: {stats:?}"
        );
        assert!(
            stats.lost > 0,
            "no concealment in the control run: {stats:?}"
        );
        assert!(
            delivered >= 4_500,
            "only {delivered} of 5000 ticks played audio; the control is not \
             a healthy stream: {stats:?}"
        );
    }

    // Negative control: a single concealed frame whose packet arrives one
    // tick later is still the resurrect path's business, however long the
    // pattern repeats. Re-anchoring here would throw away a frame of audio
    // and a buffer refill for a one-frame slip.
    #[test]
    fn single_frame_overrun_still_resurrects() {
        let (mut jb, mut seq) = overrun_by(1);
        // The resurrect path steps playout back onto the arriving stream on
        // the very next tick, so every tick from here on plays its own frame.
        for tick in 0..400u32 {
            let sending = seq;
            jb.push(packet(sending, None));
            seq += 1;
            match jb.pull() {
                Pull::Frame(p) => assert_eq!(seq_of(&p), sending, "tick {tick}"),
                other => panic!("tick {tick}: unexpected {other:?}"),
            }
        }
        let stats = jb.stats();
        assert_eq!(stats.resurrected, 1, "{stats:?}");
        assert_eq!(stats.reanchors, 0, "{stats:?}");
        assert_eq!(stats.late, 0, "{stats:?}");
        assert_eq!(stats.lost, 1, "{stats:?}");
    }

    // Negative control: an overrun past RESET_JUMP is a stream restart and
    // must still be handled by the discontinuity reset, which re-anchors on
    // the spot instead of waiting out the patience window.
    #[test]
    fn overrun_past_reset_jump_still_uses_reset_jump() {
        let (mut jb, mut seq) = overrun_by(600);
        jb.push(packet(seq, None));
        seq += 1;
        assert_eq!(jb.pull(), Pull::Frame(payload_for(seq - 1)));
        for _ in 0..100 {
            jb.push(packet(seq, None));
            seq += 1;
            assert_eq!(jb.pull(), Pull::Frame(payload_for(seq - 1)));
        }
        let stats = jb.stats();
        assert_eq!(stats.reanchors, 0, "{stats:?}");
        assert_eq!(stats.late, 0, "{stats:?}");
    }

    // The re-anchor is one event per episode, not a loop: a buffer that has
    // just re-anchored has no anchor to be late against, so a second
    // re-anchor can only follow a fresh stuck episode.
    #[test]
    fn reanchor_is_idempotent_across_two_episodes() {
        let (mut jb, mut seq) = overrun_by(5);
        for _ in 0..200 {
            jb.push(packet(seq, None));
            seq += 1;
            jb.pull();
        }
        assert_eq!(jb.stats().reanchors, 1);
        // A second stall of the same shape, once the stream is healthy again.
        for _ in 0..5 {
            jb.pull();
        }
        for _ in 0..200 {
            jb.push(packet(seq, None));
            seq += 1;
            jb.pull();
        }
        let stats = jb.stats();
        assert_eq!(stats.reanchors, 2, "{stats:?}");
        assert!(stats.depth_frames <= MAX_BUFFERED);
    }
}
