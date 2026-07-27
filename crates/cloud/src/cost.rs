//! Pre-flight cost preview. All arithmetic is integer microdollars and
//! integer bytes; the only float input is expected_hours, converted to whole
//! seconds up front so rounding is deterministic.

use crate::types::{Price, format_microusd};

/// Per-musician downstream mix, kilobits per second. Negligible but shown.
pub const MUSICIAN_DOWNSTREAM_KBPS: u64 = 300;
/// Broadcast video per destination (x264 CBR ~2.5 Mbps).
pub const STREAM_DEST_VIDEO_KBPS: u64 = 2500;
/// Broadcast audio per destination (AAC-LC 128 kbps).
pub const STREAM_DEST_AUDIO_KBPS: u64 = 128;
/// Per-listener Opus stream.
pub const LISTENER_KBPS: u64 = 150;

pub(crate) const BYTES_PER_GB: u128 = 1_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineItem {
    pub label: String,
    /// Negative for credits.
    pub microusd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostPreview {
    pub line_items: Vec<LineItem>,
    pub total_microusd: u64,
    pub egress_bytes_estimate: u64,
}

pub(crate) fn div_round(num: u128, den: u128) -> u64 {
    ((num + den / 2) / den) as u64
}

/// GB with two decimals from bytes, deterministic.
pub(crate) fn gb_display(bytes: u64) -> String {
    let centi_gb = div_round(bytes as u128, 10_000_000);
    format!("{}.{:02} GB", centi_gb / 100, centi_gb % 100)
}

impl CostPreview {
    /// `musicians` is the number of musicians in the session, the host
    /// included: the server sends every one of them a personal mix, so every
    /// one of them costs egress.
    pub fn compute(
        price: &Price,
        expected_hours: f32,
        musicians: u8,
        stream_destinations: u8,
        listeners: u8,
    ) -> CostPreview {
        let secs = (expected_hours.max(0.0) * 3600.0).round() as u64;

        let vm_microusd = div_round(price.hourly_microusd as u128 * secs as u128, 3600);

        // 1 kbps sustained is 125 bytes per second.
        let kbps = musicians as u64 * MUSICIAN_DOWNSTREAM_KBPS
            + stream_destinations as u64 * (STREAM_DEST_VIDEO_KBPS + STREAM_DEST_AUDIO_KBPS)
            + listeners as u64 * LISTENER_KBPS;
        let egress_bytes = kbps * 125 * secs;

        let egress_microusd = div_round(
            egress_bytes as u128 * price.egress_microusd_per_gb as u128,
            BYTES_PER_GB,
        );
        let credited_bytes =
            (egress_bytes as u128).min(price.included_egress_gb as u128 * BYTES_PER_GB);
        let credit_microusd = div_round(
            credited_bytes * price.egress_microusd_per_gb as u128,
            BYTES_PER_GB,
        );

        let mut line_items = vec![
            LineItem {
                label: format!(
                    "VM {} x {:.1} h",
                    price.hourly_display(),
                    secs as f64 / 3600.0
                ),
                microusd: vm_microusd as i64,
            },
            LineItem {
                label: format!(
                    "Egress estimate {} at {}",
                    gb_display(egress_bytes),
                    price.egress_display()
                ),
                microusd: egress_microusd as i64,
            },
        ];
        if credit_microusd > 0 {
            line_items.push(LineItem {
                label: format!(
                    "Included egress credit ({} GB free)",
                    price.included_egress_gb
                ),
                microusd: -(credit_microusd as i64),
            });
        }

        let total_microusd = (vm_microusd + egress_microusd).saturating_sub(credit_microusd);
        CostPreview {
            line_items,
            total_microusd,
            egress_bytes_estimate: egress_bytes,
        }
    }

    /// Folds a recording estimate into this preview: the recording's line
    /// items are appended and its total added, so a host sees one number for
    /// the session whether or not they turned recording on.
    ///
    /// Additive on purpose. [`CostPreview::compute`] keeps its signature, and
    /// a caller that never records never mentions recording.
    pub fn with_recording(mut self, recording: &crate::recording::RecordingEstimate) -> Self {
        self.line_items.extend(recording.line_items.iter().cloned());
        self.total_microusd += recording.total_microusd;
        self
    }

    /// Plain aligned strings, one per line item plus the total row.
    pub fn display_table(&self) -> Vec<String> {
        let mut rows: Vec<String> = self
            .line_items
            .iter()
            .map(|li| {
                let amount = if li.microusd < 0 {
                    format!("-{}", format_microusd(li.microusd.unsigned_abs()))
                } else {
                    format_microusd(li.microusd as u64)
                };
                format!("{:<44} {:>12}", li.label, amount)
            })
            .collect();
        rows.push(format!(
            "{:<44} {:>12}",
            "Total (estimate)",
            format_microusd(self.total_microusd)
        ));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aws_like() -> Price {
        Price {
            hourly_microusd: 16_800,
            egress_microusd_per_gb: 90_000,
            included_egress_gb: 0,
        }
    }

    #[test]
    fn hand_computed_fixture() {
        // 2 h, 4 musicians, 2 destinations, 10 listeners:
        // kbps = 4*300 + 2*2628 + 10*150 = 7956
        // bytes = 7956 * 125 * 7200 = 7_160_400_000
        // egress = 7_160_400_000 * 90_000 / 1e9 = 644_436 microusd
        // vm = 16_800 * 2 = 33_600 microusd
        let p = CostPreview::compute(&aws_like(), 2.0, 4, 2, 10);
        assert_eq!(p.egress_bytes_estimate, 7_160_400_000);
        assert_eq!(p.line_items[0].microusd, 33_600);
        assert_eq!(p.line_items[1].microusd, 644_436);
        assert_eq!(p.line_items.len(), 2);
        assert_eq!(p.total_microusd, 678_036);
    }

    #[test]
    fn free_egress_case() {
        // DO-like: 100 GB included covers the whole estimate, so the credit
        // cancels egress exactly and total is the VM alone.
        let price = Price {
            hourly_microusd: 16_800,
            egress_microusd_per_gb: 10_000,
            included_egress_gb: 100,
        };
        let p = CostPreview::compute(&price, 2.0, 4, 2, 10);
        let egress = p.line_items[1].microusd;
        let credit = p.line_items[2].microusd;
        assert_eq!(egress, 71_604);
        assert_eq!(credit, -71_604);
        assert_eq!(p.total_microusd, 33_600);
    }

    #[test]
    fn partial_egress_credit() {
        let price = Price {
            included_egress_gb: 5,
            ..aws_like()
        };
        // Credit is 5 GB * $0.09 = 450_000 microusd of the 644_436 estimate.
        let p = CostPreview::compute(&price, 2.0, 4, 2, 10);
        assert_eq!(p.line_items[2].microusd, -450_000);
        assert_eq!(p.total_microusd, 33_600 + 644_436 - 450_000);
    }

    #[test]
    fn no_traffic_is_vm_only() {
        let p = CostPreview::compute(&aws_like(), 1.5, 0, 0, 0);
        assert_eq!(p.egress_bytes_estimate, 0);
        assert_eq!(p.line_items[1].microusd, 0);
        assert_eq!(p.total_microusd, 25_200);
    }

    #[test]
    fn fractional_hours_round_to_seconds() {
        // 0.5 h = 1800 s; vm = 16_800 * 1800 / 3600 = 8_400.
        let p = CostPreview::compute(&aws_like(), 0.5, 0, 0, 0);
        assert_eq!(p.total_microusd, 8_400);
    }

    #[test]
    fn display_table_shape() {
        let p = CostPreview::compute(&aws_like(), 2.0, 4, 2, 10);
        let rows = p.display_table();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].contains("VM $0.0168/hr x 2.0 h"));
        assert!(rows[0].ends_with("$0.0336"));
        assert!(rows[1].contains("Egress estimate 7.16 GB at $0.09/GB"));
        assert!(rows[1].ends_with("$0.644436"));
        assert!(rows[2].ends_with("$0.678036"));
    }

    #[test]
    fn credit_row_renders_negative() {
        let price = Price {
            hourly_microusd: 16_800,
            egress_microusd_per_gb: 10_000,
            included_egress_gb: 100,
        };
        let rows = CostPreview::compute(&price, 2.0, 4, 2, 10).display_table();
        assert!(rows[2].ends_with("-$0.071604"), "row was {:?}", rows[2]);
    }

    #[test]
    fn recording_folds_into_the_session_preview() {
        use crate::recording::{RecordingEstimate, RecordingPlan};
        use crate::types::{ProviderKind, RegionId};

        let session = CostPreview::compute(&aws_like(), 2.0, 4, 2, 10);
        let session_total = session.total_microusd;
        let recording = RecordingEstimate::compute(
            ProviderKind::Aws,
            &RegionId::new("us-east-1"),
            &RecordingPlan::with_stems(4),
            2.0,
        )
        .unwrap();

        let combined = session.with_recording(&recording);
        // Two session rows plus the recording's two, and one total.
        assert_eq!(combined.line_items.len(), 4);
        assert_eq!(
            combined.total_microusd,
            session_total + recording.total_microusd
        );
        assert_eq!(combined.total_microusd, 678_036 + 468_634);
        let rows = combined.display_table();
        assert_eq!(rows.len(), 5);
        assert!(rows[2].contains("Recording 4.15 GB (mix + 4 stems, 16-bit) for 30 days"));
        assert!(rows[4].ends_with("$1.14667"), "row was {:?}", rows[4]);
        // Turning recording off must leave the session preview untouched.
        assert_eq!(
            CostPreview::compute(&aws_like(), 2.0, 4, 2, 10).total_microusd,
            session_total
        );
    }
}
