//! Region choice: latency and price get equal prominence. Worst-case RTT is
//! bucketed to 5 ms steps and price breaks ties inside a bucket, so a 2 ms
//! latency edge never beats a materially cheaper region.

use std::collections::HashMap;

use crate::types::{Price, Region, RegionId};

/// Local alias; deliberately not a jamstream-protocol dependency.
pub type MemberId = u16;

/// A member whose best probe everywhere exceeds this is on a high-latency
/// link (satellite) and must not veto region choice.
pub const OUTLIER_RTT_MS: f32 = 150.0;

pub const BUCKET_MS: f32 = 5.0;

#[derive(Debug, Clone, Default)]
pub struct ProbeMatrix {
    probes: HashMap<MemberId, HashMap<RegionId, f32>>,
}

impl ProbeMatrix {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, member: MemberId, region: RegionId, rtt_ms: f32) {
        self.probes
            .entry(member)
            .or_default()
            .insert(region, rtt_ms);
    }

    pub fn member_count(&self) -> usize {
        self.probes.len()
    }

    /// Members whose minimum RTT across every probed region exceeds the
    /// outlier threshold. Sorted for determinism.
    pub fn outliers(&self) -> Vec<MemberId> {
        let mut out: Vec<MemberId> = self
            .probes
            .iter()
            .filter(|(_, rtts)| {
                !rtts.is_empty()
                    && rtts.values().fold(f32::INFINITY, |a, &b| a.min(b)) > OUTLIER_RTT_MS
            })
            .map(|(&m, _)| m)
            .collect();
        out.sort_unstable();
        out
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegionScore {
    pub region: Region,
    pub worst_rtt_ms: f32,
    pub mean_rtt_ms: f32,
    pub price: Price,
    /// Fraction of non-outlier members with a probe for this region.
    pub coverage: f32,
    /// Members excluded from worst-case because their best RTT anywhere
    /// exceeds the outlier threshold.
    pub outliers: Vec<MemberId>,
}

fn rtt_bucket(rtt_ms: f32) -> u64 {
    if rtt_ms.is_finite() {
        (rtt_ms / BUCKET_MS) as u64
    } else {
        u64::MAX
    }
}

/// Ranks every candidate region. Sort order: full coverage first, then
/// worst-case RTT in 5 ms buckets, then hourly price ascending, then mean
/// RTT and region id for determinism.
pub fn rank(matrix: &ProbeMatrix, candidates: &[(Region, Price)]) -> Vec<RegionScore> {
    let outliers = matrix.outliers();
    let mut eligible: Vec<MemberId> = matrix
        .probes
        .keys()
        .copied()
        .filter(|m| !outliers.contains(m))
        .collect();
    if eligible.is_empty() {
        // Everyone is an outlier: nobody is excluded, otherwise there would
        // be no signal at all.
        eligible = matrix.probes.keys().copied().collect();
    }

    let mut scores: Vec<RegionScore> = candidates
        .iter()
        .map(|(region, price)| {
            let rtts: Vec<f32> = eligible
                .iter()
                .filter_map(|m| matrix.probes[m].get(&region.id).copied())
                .collect();
            let (worst, mean, coverage) = if eligible.is_empty() {
                // No members at all: rank on price alone.
                (0.0, 0.0, 1.0)
            } else if rtts.is_empty() {
                (f32::INFINITY, f32::INFINITY, 0.0)
            } else {
                let worst = rtts.iter().fold(0.0f32, |a, &b| a.max(b));
                let mean = rtts.iter().sum::<f32>() / rtts.len() as f32;
                (worst, mean, rtts.len() as f32 / eligible.len() as f32)
            };
            RegionScore {
                region: region.clone(),
                worst_rtt_ms: worst,
                mean_rtt_ms: mean,
                price: *price,
                coverage,
                outliers: outliers.clone(),
            }
        })
        .collect();

    scores.sort_by(|a, b| {
        let a_full = a.coverage >= 1.0;
        let b_full = b.coverage >= 1.0;
        b_full
            .cmp(&a_full)
            .then(b.coverage.total_cmp(&a.coverage))
            .then(rtt_bucket(a.worst_rtt_ms).cmp(&rtt_bucket(b.worst_rtt_ms)))
            .then(a.price.hourly_microusd.cmp(&b.price.hourly_microusd))
            .then(a.mean_rtt_ms.total_cmp(&b.mean_rtt_ms))
            .then(a.region.id.cmp(&b.region.id))
    });
    scores
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProviderKind;
    use proptest::prelude::*;

    fn region(id: &str) -> Region {
        Region {
            provider: ProviderKind::Aws,
            id: RegionId::new(id),
            display: id.to_owned(),
            country: "US".to_owned(),
        }
    }

    fn price(hourly: u64) -> Price {
        Price {
            hourly_microusd: hourly,
            egress_microusd_per_gb: 90_000,
            included_egress_gb: 0,
        }
    }

    #[test]
    fn ranks_by_worst_rtt_across_buckets() {
        let mut m = ProbeMatrix::new();
        m.insert(1, "east".into(), 20.0);
        m.insert(1, "west".into(), 80.0);
        m.insert(2, "east".into(), 30.0);
        m.insert(2, "west".into(), 25.0);
        let ranked = rank(
            &m,
            &[
                (region("west"), price(10_000)),
                (region("east"), price(50_000)),
            ],
        );
        // east worst 30, west worst 80: latency wins across buckets even
        // though west is 5x cheaper.
        assert_eq!(ranked[0].region.id.as_str(), "east");
        assert_eq!(ranked[0].worst_rtt_ms, 30.0);
        assert_eq!(ranked[0].coverage, 1.0);
        assert!(ranked[0].outliers.is_empty());
    }

    #[test]
    fn price_breaks_ties_inside_a_bucket() {
        let mut m = ProbeMatrix::new();
        // 42 ms and 44 ms fall in the same 5 ms bucket (40..45).
        m.insert(1, "fast".into(), 42.0);
        m.insert(1, "cheap".into(), 44.0);
        let ranked = rank(
            &m,
            &[
                (region("fast"), price(70_000)),
                (region("cheap"), price(42_000)),
            ],
        );
        assert_eq!(ranked[0].region.id.as_str(), "cheap");

        // 44 ms vs 46 ms cross a bucket boundary: latency wins again.
        let mut m2 = ProbeMatrix::new();
        m2.insert(1, "fast".into(), 44.0);
        m2.insert(1, "cheap".into(), 46.0);
        let ranked2 = rank(
            &m2,
            &[
                (region("fast"), price(70_000)),
                (region("cheap"), price(42_000)),
            ],
        );
        assert_eq!(ranked2[0].region.id.as_str(), "fast");
    }

    #[test]
    fn full_coverage_beats_partial_coverage() {
        let mut m = ProbeMatrix::new();
        m.insert(1, "covered".into(), 90.0);
        m.insert(2, "covered".into(), 95.0);
        m.insert(1, "gappy".into(), 5.0);
        // Member 2 has no probe for gappy.
        let ranked = rank(
            &m,
            &[
                (region("gappy"), price(1_000)),
                (region("covered"), price(90_000)),
            ],
        );
        assert_eq!(ranked[0].region.id.as_str(), "covered");
        assert_eq!(ranked[1].coverage, 0.5);
    }

    #[test]
    fn satellite_member_does_not_veto() {
        let mut m = ProbeMatrix::new();
        m.insert(1, "east".into(), 20.0);
        m.insert(1, "west".into(), 70.0);
        m.insert(2, "east".into(), 25.0);
        m.insert(2, "west".into(), 60.0);
        // Member 3 is on satellite: over 150 ms everywhere, and would flip
        // the ranking to west if counted in worst-case.
        m.insert(3, "east".into(), 620.0);
        m.insert(3, "west".into(), 590.0);
        let ranked = rank(
            &m,
            &[
                (region("east"), price(20_000)),
                (region("west"), price(20_000)),
            ],
        );
        assert_eq!(ranked[0].region.id.as_str(), "east");
        assert_eq!(ranked[0].worst_rtt_ms, 25.0);
        assert_eq!(ranked[0].outliers, vec![3]);
        assert_eq!(ranked[0].coverage, 1.0);
    }

    #[test]
    fn all_outliers_fall_back_to_everyone() {
        let mut m = ProbeMatrix::new();
        m.insert(1, "east".into(), 300.0);
        m.insert(1, "west".into(), 200.0);
        let ranked = rank(
            &m,
            &[
                (region("east"), price(20_000)),
                (region("west"), price(20_000)),
            ],
        );
        assert_eq!(ranked[0].region.id.as_str(), "west");
        assert_eq!(ranked[0].worst_rtt_ms, 200.0);
    }

    #[test]
    fn empty_matrix_ranks_by_price() {
        let m = ProbeMatrix::new();
        let ranked = rank(
            &m,
            &[
                (region("pricey"), price(90_000)),
                (region("cheap"), price(9_000)),
            ],
        );
        assert_eq!(ranked[0].region.id.as_str(), "cheap");
    }

    proptest! {
        #[test]
        fn ranking_is_total_and_deterministic(
            rtts in proptest::collection::vec((0u16..8, 0u8..4, 1.0f32..400.0), 0..40),
            prices in proptest::collection::vec(1_000u64..1_000_000, 4),
        ) {
            let regions = ["r0", "r1", "r2", "r3"];
            let mut m = ProbeMatrix::new();
            for (member, ri, rtt) in &rtts {
                m.insert(*member, regions[*ri as usize].into(), *rtt);
            }
            let candidates: Vec<(Region, Price)> = regions
                .iter()
                .zip(&prices)
                .map(|(r, &p)| (region(r), price(p)))
                .collect();
            let a = rank(&m, &candidates);
            let b = rank(&m, &candidates);
            prop_assert_eq!(a.len(), candidates.len());
            prop_assert_eq!(&a, &b);
            // Full-coverage regions always precede partial-coverage ones.
            let first_partial = a.iter().position(|s| s.coverage < 1.0);
            if let Some(idx) = first_partial {
                prop_assert!(a[idx..].iter().all(|s| s.coverage < 1.0));
            }
        }
    }
}
