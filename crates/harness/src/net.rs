//! Seeded, deterministic packet network simulation.
//!
//! Time never advances in here; callers pass `now_us` from a `VirtualClock`
//! to `send` and `poll`. All randomness comes from one `StdRng`, so a given
//! seed plus a given call sequence replays exactly, on every OS.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use rand::rngs::StdRng;
// rand 0.10 renamed the `Rng` extension trait to `RngExt`; the name `Rng` is
// now `rand_core`'s old `RngCore`. `random::<T>()` lives on `RngExt`.
use rand::{RngExt, SeedableRng};
use serde::{Deserialize, Serialize};

/// Small integer id for a simulated endpoint (a client or the server).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointId(pub u16);

/// One-way link behavior. `loss`, `reorder_prob`, and `dup_prob` are
/// probabilities in `0..1`; the `_ms` fields are one-way times. Clock drift
/// is deliberately not a profile knob; compose a `SkewedClock` for that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub one_way_ms: f32,
    pub jitter_ms: f32,
    pub loss: f32,
    #[serde(default)]
    pub reorder_extra_ms: f32,
    #[serde(default)]
    pub reorder_prob: f32,
    #[serde(default)]
    pub dup_prob: f32,
}

/// A packet handed back by `SimNet::poll`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub to: EndpointId,
    pub from: EndpointId,
    pub payload: Vec<u8>,
}

/// Per-direction link counters. `delivered` counts every packet handed out
/// by `poll`, duplicate copies included; `duplicated` counts just the extra
/// copies, so with zero loss `delivered == sent + duplicated`.
#[derive(Debug, Clone, Default)]
pub struct LinkStats {
    pub sent: u64,
    pub delivered: u64,
    pub dropped: u64,
    pub duplicated: u64,
    delay_min_us: u64,
    delay_max_us: u64,
    delay_sum_us: u128,
    delay_samples: u64,
}

impl LinkStats {
    fn record_delay(&mut self, delay_us: u64) {
        if self.delay_samples == 0 {
            self.delay_min_us = delay_us;
            self.delay_max_us = delay_us;
        } else {
            self.delay_min_us = self.delay_min_us.min(delay_us);
            self.delay_max_us = self.delay_max_us.max(delay_us);
        }
        self.delay_sum_us += u128::from(delay_us);
        self.delay_samples += 1;
    }

    pub fn delay_min_us(&self) -> Option<u64> {
        (self.delay_samples > 0).then_some(self.delay_min_us)
    }

    pub fn delay_max_us(&self) -> Option<u64> {
        (self.delay_samples > 0).then_some(self.delay_max_us)
    }

    pub fn delay_mean_us(&self) -> Option<f64> {
        (self.delay_samples > 0).then(|| self.delay_sum_us as f64 / self.delay_samples as f64)
    }
}

/// A scheduled in-flight packet. Ordered by `(due_us, seq)`; `seq` is the
/// scheduling sequence number and is the deterministic tiebreak for packets
/// due at the same instant.
struct Scheduled {
    due_us: u64,
    seq: u64,
    sent_us: u64,
    from: EndpointId,
    to: EndpointId,
    payload: Vec<u8>,
}

impl PartialEq for Scheduled {
    fn eq(&self, other: &Self) -> bool {
        self.due_us == other.due_us && self.seq == other.seq
    }
}

impl Eq for Scheduled {}

impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.due_us, self.seq).cmp(&(other.due_us, other.seq))
    }
}

/// The simulated network: directional links between endpoint pairs, a seeded
/// RNG, and a delivery queue.
pub struct SimNet {
    rng: StdRng,
    links: HashMap<(EndpointId, EndpointId), Profile>,
    queue: BinaryHeap<Reverse<Scheduled>>,
    next_seq: u64,
    stats: HashMap<(EndpointId, EndpointId), LinkStats>,
    unknown_link_drops: u64,
}

impl SimNet {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: StdRng::seed_from_u64(seed),
            links: HashMap::new(),
            queue: BinaryHeap::new(),
            next_seq: 0,
            stats: HashMap::new(),
            unknown_link_drops: 0,
        }
    }

    /// Configures both directions of a link with the same profile.
    pub fn link(&mut self, a: EndpointId, b: EndpointId, profile: &Profile) {
        self.links.insert((a, b), profile.clone());
        self.links.insert((b, a), profile.clone());
    }

    /// Configures one direction only, for asymmetric links.
    pub fn link_oneway(&mut self, from: EndpointId, to: EndpointId, profile: &Profile) {
        self.links.insert((from, to), profile.clone());
    }

    /// Swaps the profile on both directions mid-run (jitter-spike scenarios).
    /// Packets already in flight keep the delivery times they were scheduled
    /// with; only packets sent after the swap see the new profile.
    pub fn set_profile(&mut self, a: EndpointId, b: EndpointId, profile: &Profile) {
        self.link(a, b, profile);
    }

    /// Schedules `payload` per the `from -> to` link profile. An unconfigured
    /// link drops the packet and counts it.
    pub fn send(&mut self, now_us: u64, from: EndpointId, to: EndpointId, payload: Vec<u8>) {
        let key = (from, to);
        self.stats.entry(key).or_default().sent += 1;

        let Some(profile) = self.links.get(&key).cloned() else {
            let stats = self.stats.get_mut(&key).expect("stats entry just created");
            stats.dropped += 1;
            self.unknown_link_drops += 1;
            return;
        };

        // Fixed draw order per send keeps the RNG stream reproducible:
        // loss, then arrival (jitter + reorder), then dup, then dup arrival.
        if self.rng.random::<f32>() < profile.loss {
            self.stats
                .get_mut(&key)
                .expect("stats entry just created")
                .dropped += 1;
            return;
        }
        let due_us = Self::arrival_us(&mut self.rng, now_us, &profile);
        let dup_due_us = (self.rng.random::<f32>() < profile.dup_prob)
            .then(|| Self::arrival_us(&mut self.rng, now_us, &profile));

        if let Some(dup_due_us) = dup_due_us {
            self.stats
                .get_mut(&key)
                .expect("stats entry just created")
                .duplicated += 1;
            self.schedule(due_us, now_us, from, to, payload.clone());
            self.schedule(dup_due_us, now_us, from, to, payload);
        } else {
            self.schedule(due_us, now_us, from, to, payload);
        }
    }

    /// Returns everything due at or before `now_us`, ordered by scheduled
    /// time with the scheduling sequence number as the deterministic tiebreak.
    pub fn poll(&mut self, now_us: u64) -> Vec<Delivery> {
        let mut out = Vec::new();
        while let Some(Reverse(head)) = self.queue.peek() {
            if head.due_us > now_us {
                break;
            }
            let Reverse(p) = self.queue.pop().expect("peeked entry exists");
            let stats = self.stats.entry((p.from, p.to)).or_default();
            stats.delivered += 1;
            stats.record_delay(p.due_us - p.sent_us);
            out.push(Delivery {
                to: p.to,
                from: p.from,
                payload: p.payload,
            });
        }
        out
    }

    /// Counters for the `from -> to` direction; present after the first send.
    pub fn link_stats(&self, from: EndpointId, to: EndpointId) -> Option<&LinkStats> {
        self.stats.get(&(from, to))
    }

    /// Packets dropped because no link profile was configured.
    pub fn unknown_link_drops(&self) -> u64 {
        self.unknown_link_drops
    }

    /// Draws one arrival time. Jitter is the absolute value of a normal-ish
    /// sample (sum of three uniforms, centered) scaled to `jitter_ms`, so it
    /// is nonnegative and its mean is about 0.4 * jitter_ms. A reorder event
    /// adds `reorder_extra_ms` to this packet only. Delivery is never
    /// scheduled before `now_us`.
    fn arrival_us(rng: &mut StdRng, now_us: u64, profile: &Profile) -> u64 {
        let centered = rng.random::<f64>() + rng.random::<f64>() + rng.random::<f64>() - 1.5;
        let mut delay_ms =
            f64::from(profile.one_way_ms) + centered.abs() * f64::from(profile.jitter_ms);
        if rng.random::<f32>() < profile.reorder_prob {
            delay_ms += f64::from(profile.reorder_extra_ms);
        }
        now_us + (delay_ms * 1_000.0).max(0.0).round() as u64
    }

    fn schedule(
        &mut self,
        due_us: u64,
        sent_us: u64,
        from: EndpointId,
        to: EndpointId,
        payload: Vec<u8>,
    ) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.queue.push(Reverse(Scheduled {
            due_us,
            seq,
            sent_us,
            from,
            to,
            payload,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_profile(one_way_ms: f32) -> Profile {
        Profile {
            name: "clean".into(),
            one_way_ms,
            jitter_ms: 0.0,
            loss: 0.0,
            reorder_extra_ms: 0.0,
            reorder_prob: 0.0,
            dup_prob: 0.0,
        }
    }

    #[test]
    fn unknown_link_drops_and_counts() {
        let mut net = SimNet::new(0);
        net.send(0, EndpointId(9), EndpointId(10), vec![0]);
        assert_eq!(net.unknown_link_drops(), 1);
        let stats = net.link_stats(EndpointId(9), EndpointId(10)).unwrap();
        assert_eq!(stats.sent, 1);
        assert_eq!(stats.dropped, 1);
        assert!(net.poll(u64::MAX).is_empty());
    }

    #[test]
    fn link_configures_both_directions() {
        let mut net = SimNet::new(0);
        let (a, b) = (EndpointId(1), EndpointId(2));
        net.link(a, b, &clean_profile(5.0));
        net.send(0, a, b, vec![1]);
        net.send(0, b, a, vec![2]);
        let deliveries = net.poll(5_000);
        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[0].to, b);
        assert_eq!(deliveries[1].to, a);
    }

    #[test]
    fn link_oneway_leaves_reverse_unconfigured() {
        let mut net = SimNet::new(0);
        let (a, b) = (EndpointId(1), EndpointId(2));
        net.link_oneway(a, b, &clean_profile(5.0));
        net.send(0, a, b, vec![1]);
        net.send(0, b, a, vec![2]);
        assert_eq!(net.unknown_link_drops(), 1);
        assert_eq!(net.poll(u64::MAX).len(), 1);
    }
}
