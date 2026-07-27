//! What a recording costs, worked out before anyone presses record.
//!
//! Same discipline as [`crate::cost`]: integer microdollars, integer bytes,
//! and the only float input (`expected_hours`) converted to whole seconds up
//! front so the arithmetic is deterministic and hand-checkable.
//!
//! # The audio arithmetic
//!
//! Recording captures uncompressed WAV, because a rehearsal recording people
//! might actually mix should not be pre-damaged by a codec. At 48 kHz that
//! makes the size entirely predictable:
//!
//! | Track | Channels | 16-bit | 24-bit |
//! |---|---|---|---|
//! | Broadcast mix | stereo | 691.2 MB/h | 1.037 GB/h |
//! | Per-member stem | mono | 345.6 MB/h | 518.4 MB/h |
//!
//! So the headline number: a two-hour session records a 1.38 GB mix, and each
//! member's stem adds 691 MB on top. A five-piece band recording stems for
//! two hours produces about 4.8 GB. That is the whole reason
//! [`crate::storage`] needs multipart uploads and the reason this estimate
//! exists at all.
//!
//! # What the estimate deliberately does not do
//!
//! * **It does not spend a free allowance twice.** AWS gives 100 GB/month of
//!   free egress and DigitalOcean's Spaces subscription includes 1 TiB, but
//!   [`crate::cost::CostPreview`] already credits the session's own streaming
//!   traffic against the same pool. Crediting it here as well would produce a
//!   total that is too low in exactly the case a host cares about. The
//!   download line is therefore the gross price and the allowance appears as
//!   a note, which makes the figure an upper bound rather than a guess.
//! * **It does not include request charges.** A whole recording is on the
//!   order of a hundred API calls; at S3's $0.005 per 1,000 PUTs that is
//!   under a tenth of a cent. It is stated as excluded rather than silently
//!   dropped.
//! * **It does not include the Spaces subscription in the total.** Anyone
//!   with a Spaces bucket already pays $5/month, and a recording this size
//!   fits inside the included 250 GiB, so the *marginal* cost of recording is
//!   genuinely zero. A host with no bucket yet needs to know about the $5,
//!   so it is carried on [`StoragePrice::base_monthly_microusd`] and stated
//!   in the notes.
//! * **It assumes list prices, 30-day months, and no committed-use or
//!   free-tier discount.** Storage is prorated by day because that is how the
//!   providers bill GB-month.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::cost::{BYTES_PER_GB, LineItem, div_round, gb_display};
use crate::provider::{ProviderError, Result};
use crate::retention::{DAYS_PER_MONTH, Retention};
use crate::types::{ProviderKind, RegionId, format_microusd};

/// Recording sample rate. Fixed: it is the session's own rate.
pub const SAMPLE_RATE_HZ: u64 = 48_000;
/// The broadcast mix is stereo.
pub const MIX_CHANNELS: u64 = 2;
/// A per-member stem is that member's own mono capture.
pub const STEM_CHANNELS: u64 = 1;
/// Canonical 44-byte WAV header, counted so the numbers reconcile with what
/// a host sees in their bucket rather than being off by a hair.
pub const WAV_HEADER_BYTES: u64 = 44;
/// A plain WAV cannot address past 4 GiB: the RIFF size fields are 32-bit.
pub const WAV_MAX_BYTES: u64 = u32::MAX as u64;

const PRICES_JSON: &str = include_str!("../data/storage_prices.json");

/// Bit depth of the recorded WAV.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitDepth {
    /// CD depth. The default: half the bytes, and past the point where a
    /// rehearsal recording is limited by the room rather than the format.
    Sixteen,
    /// Studio depth, for a session someone intends to mix properly.
    TwentyFour,
}

impl BitDepth {
    pub fn bytes_per_sample(&self) -> u64 {
        match self {
            BitDepth::Sixteen => 2,
            BitDepth::TwentyFour => 3,
        }
    }

    pub fn bits(&self) -> u32 {
        match self {
            BitDepth::Sixteen => 16,
            BitDepth::TwentyFour => 24,
        }
    }
}

/// What a session is going to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingPlan {
    pub bit_depth: BitDepth,
    /// Number of per-member stems; 0 records the broadcast mix only.
    pub stems: u8,
    pub retention: Retention,
}

impl Default for RecordingPlan {
    fn default() -> Self {
        RecordingPlan {
            bit_depth: BitDepth::Sixteen,
            stems: 0,
            retention: Retention::default(),
        }
    }
}

impl RecordingPlan {
    /// The broadcast mix only, at the default depth and retention.
    pub fn mix_only() -> Self {
        Self::default()
    }

    /// The mix plus one stem per member.
    pub fn with_stems(stems: u8) -> Self {
        RecordingPlan {
            stems,
            ..Self::default()
        }
    }

    pub fn bit_depth(mut self, depth: BitDepth) -> Self {
        self.bit_depth = depth;
        self
    }

    pub fn retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    fn track_bytes(&self, channels: u64, seconds: u64) -> u64 {
        seconds * SAMPLE_RATE_HZ * channels * self.bit_depth.bytes_per_sample() + WAV_HEADER_BYTES
    }

    /// Bytes the broadcast mix accumulates per hour of session.
    pub fn mix_bytes_per_hour(&self) -> u64 {
        3600 * SAMPLE_RATE_HZ * MIX_CHANNELS * self.bit_depth.bytes_per_sample()
    }

    /// Bytes one member's stem accumulates per hour of session.
    pub fn stem_bytes_per_hour(&self) -> u64 {
        3600 * SAMPLE_RATE_HZ * STEM_CHANNELS * self.bit_depth.bytes_per_sample()
    }

    /// Size of the finished mix WAV.
    pub fn mix_bytes(&self, seconds: u64) -> u64 {
        self.track_bytes(MIX_CHANNELS, seconds)
    }

    /// Size of one finished stem WAV.
    pub fn stem_bytes(&self, seconds: u64) -> u64 {
        self.track_bytes(STEM_CHANNELS, seconds)
    }

    /// Every WAV together, mix plus stems. The manifest is a few hundred
    /// bytes and is not counted.
    pub fn total_bytes(&self, seconds: u64) -> u64 {
        self.mix_bytes(seconds) + self.stems as u64 * self.stem_bytes(seconds)
    }

    /// Number of WAV objects uploaded.
    pub fn object_count(&self) -> u32 {
        1 + self.stems as u32
    }

    /// The largest single object, which is always the stereo mix.
    pub fn largest_object_bytes(&self, seconds: u64) -> u64 {
        self.mix_bytes(seconds)
    }

    /// Warns when the mix would outgrow what a plain WAV can address. Not an
    /// upload problem — the object store handles any size — but the writer
    /// on the server side has to switch container (RF64/W64) or split the
    /// file, so the constraint belongs next to the size arithmetic.
    pub fn wav_size_warning(&self, seconds: u64) -> Option<String> {
        let mix = self.largest_object_bytes(seconds);
        (mix > WAV_MAX_BYTES).then(|| {
            let hours = WAV_MAX_BYTES / self.mix_bytes_per_hour();
            format!(
                "a {}-bit stereo mix passes the 4 GiB limit of a plain WAV after about {hours} \
                 hours ({} at this length), so the recorder has to split the file or write RF64",
                self.bit_depth.bits(),
                gb_display(mix)
            )
        })
    }
}

/// Object storage prices for one provider and region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoragePrice {
    pub provider: ProviderKind,
    /// Storage class the price belongs to, for display.
    pub class: String,
    pub storage_microusd_per_gb_month: u64,
    /// Internet download, first tier.
    pub egress_microusd_per_gb: u64,
    /// Storage included before per-GB charges begin.
    pub included_storage_gb: u32,
    /// Egress included per month. Shared with the session's own traffic; see
    /// the module docs on why this is not credited here.
    pub included_egress_gb: u32,
    /// Fixed monthly subscription that owning a bucket implies at all, if
    /// any. Not part of the marginal cost of one recording.
    pub base_monthly_microusd: u64,
    /// Provider-specific caveats from the bundled price file.
    pub note: String,
}

impl StoragePrice {
    pub fn storage_display(&self) -> String {
        format!(
            "{}/GB-month",
            format_microusd(self.storage_microusd_per_gb_month)
        )
    }

    pub fn egress_display(&self) -> String {
        format!("{}/GB", format_microusd(self.egress_microusd_per_gb))
    }
}

/// A recording's storage bill, itemized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingEstimate {
    pub plan: RecordingPlan,
    pub price: StoragePrice,
    /// Session length the estimate was computed for.
    pub seconds: u64,
    pub mix_bytes: u64,
    /// Every stem together.
    pub stem_bytes: u64,
    pub total_bytes: u64,
    pub object_count: u32,
    /// Bytes actually charged for, after any included storage.
    pub billable_storage_bytes: u64,
    /// Storage for the retained period (one month when kept forever).
    pub storage_microusd: u64,
    /// Downloading the whole recording once, at list price.
    pub download_egress_microusd: u64,
    pub total_microusd: u64,
    pub line_items: Vec<LineItem>,
    /// Everything the number excludes or assumes, in plain words.
    pub notes: Vec<String>,
}

impl RecordingEstimate {
    /// Prices `plan` for a session of `expected_hours` in one provider region.
    ///
    /// The region is the *bucket's* region, which is normally the session
    /// region: recording to a bucket next to the VM avoids paying egress to
    /// upload it.
    pub fn compute(
        provider: ProviderKind,
        region: &RegionId,
        plan: &RecordingPlan,
        expected_hours: f32,
    ) -> Result<RecordingEstimate> {
        let price = storage_price(provider, region)?;
        Ok(Self::with_price(price, plan, expected_hours))
    }

    /// [`RecordingEstimate::compute`] against a price the caller already has,
    /// so a UI can price several retention choices without re-reading the
    /// price table.
    pub fn with_price(
        price: StoragePrice,
        plan: &RecordingPlan,
        expected_hours: f32,
    ) -> RecordingEstimate {
        let seconds = (expected_hours.max(0.0) * 3600.0).round() as u64;
        let mix_bytes = plan.mix_bytes(seconds);
        let stem_bytes = plan.stems as u64 * plan.stem_bytes(seconds);
        let total_bytes = mix_bytes + stem_bytes;

        let included = price.included_storage_gb as u128 * BYTES_PER_GB;
        let billable_storage_bytes = (total_bytes as u128).saturating_sub(included) as u64;

        let days = plan.retention.billed_days() as u128;
        // bytes/GB * price-per-GB-month * days/30, one division so the
        // rounding happens once.
        let storage_microusd = div_round(
            billable_storage_bytes as u128 * price.storage_microusd_per_gb_month as u128 * days,
            BYTES_PER_GB * DAYS_PER_MONTH as u128,
        );
        let download_egress_microusd = div_round(
            total_bytes as u128 * price.egress_microusd_per_gb as u128,
            BYTES_PER_GB,
        );

        let contents = match plan.stems {
            0 => "mix only".to_owned(),
            1 => "mix + 1 stem".to_owned(),
            n => format!("mix + {n} stems"),
        };
        let period = match plan.retention.days() {
            Some(days) => format!("{days} days"),
            None => "1 month, recurring".to_owned(),
        };
        let line_items = vec![
            LineItem {
                label: format!(
                    "Recording {} ({contents}, {}-bit) for {period}",
                    gb_display(total_bytes),
                    plan.bit_depth.bits()
                ),
                microusd: storage_microusd as i64,
            },
            LineItem {
                label: format!(
                    "Download once {} at {}",
                    gb_display(total_bytes),
                    price.egress_display()
                ),
                microusd: download_egress_microusd as i64,
            },
        ];

        let mut notes = vec![format!(
            "{} at {}; {}.",
            price.class,
            price.storage_display(),
            plan.retention.label().to_lowercase()
        )];
        if plan.retention.is_recurring() {
            notes.push(format!(
                "Kept forever means the {} storage charge repeats every month until you delete \
                 the recording, not once.",
                format_microusd(storage_microusd)
            ));
        }
        if price.included_storage_gb > 0 && billable_storage_bytes == 0 {
            notes.push(format!(
                "Storage is $0.00 because this fits inside the {} GB your plan already includes.",
                price.included_storage_gb
            ));
        }
        if price.base_monthly_microusd > 0 {
            notes.push(format!(
                "Owning a bucket at all costs {}/month on this provider, whether or not you \
                 record. That is not counted above, which shows only what recording adds.",
                format_microusd(price.base_monthly_microusd)
            ));
        }
        if price.included_egress_gb > 0 {
            notes.push(format!(
                "Your plan includes {} GB/month of free download, so the download line is an \
                 upper bound. It is not discounted here because the session's own streaming \
                 traffic draws on the same allowance and the cost preview already counts that.",
                price.included_egress_gb
            ));
        }
        notes.push(format!(
            "Excludes request charges: {} uploads plus a manifest is about a hundred API calls, \
             under $0.001 on every provider.",
            plan.object_count()
        ));
        notes.push(
            "Assumes public list prices, 30-day months, and no free-tier or committed-use \
             discount. Billed to your own cloud account; JamStream never sees it."
                .to_owned(),
        );
        notes.push(price.note.clone());
        if let Some(warning) = plan.wav_size_warning(seconds) {
            notes.push(format!("Heads up: {warning}."));
        }

        RecordingEstimate {
            plan: *plan,
            price,
            seconds,
            mix_bytes,
            stem_bytes,
            total_bytes,
            object_count: plan.object_count(),
            billable_storage_bytes,
            storage_microusd,
            download_egress_microusd,
            total_microusd: storage_microusd + download_egress_microusd,
            line_items,
            notes,
        }
    }

    /// Plain aligned strings, one per line item plus the total row, matching
    /// [`crate::cost::CostPreview::display_table`].
    pub fn display_table(&self) -> Vec<String> {
        let mut rows: Vec<String> = self
            .line_items
            .iter()
            .map(|li| {
                format!(
                    "{:<44} {:>12}",
                    li.label,
                    format_microusd(li.microusd.unsigned_abs())
                )
            })
            .collect();
        rows.push(format!(
            "{:<44} {:>12}",
            "Recording total (estimate)",
            format_microusd(self.total_microusd)
        ));
        rows
    }
}

// ---- Bundled price table ----

#[derive(Debug, Deserialize)]
struct PriceFile {
    providers: BTreeMap<String, ProviderPrices>,
}

#[derive(Debug, Deserialize)]
struct ProviderPrices {
    class: String,
    base_monthly_microusd: u64,
    included_storage_gb: u32,
    included_egress_gb: u32,
    default_storage_microusd_per_gb_month: u64,
    default_egress_microusd_per_gb: u64,
    note: String,
    #[serde(default)]
    regions: BTreeMap<String, RegionPrices>,
    /// DigitalOcean only: the regions where Spaces actually exists.
    #[serde(default)]
    spaces_regions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RegionPrices {
    storage_microusd_per_gb_month: u64,
    egress_microusd_per_gb: u64,
}

fn price_file() -> &'static PriceFile {
    static FILE: OnceLock<PriceFile> = OnceLock::new();
    FILE.get_or_init(|| {
        serde_json::from_str(PRICES_JSON).expect("bundled data/storage_prices.json must parse")
    })
}

/// Object storage prices for one provider and region, from the bundled
/// snapshot.
///
/// A region missing from a provider's table falls back to that provider's
/// default rates, which are pinned to its most expensive region so an
/// estimate is never too low. DigitalOcean is the exception: Spaces exists in
/// only some droplet regions, so an unsupported region is a
/// [`ProviderError::NotFound`] naming the ones that work rather than a price
/// for a bucket the host cannot create.
pub fn storage_price(provider: ProviderKind, region: &RegionId) -> Result<StoragePrice> {
    if provider == ProviderKind::Local {
        return Err(ProviderError::NotFound(
            "a local session records to your own disk, which JamStream does not price".to_owned(),
        ));
    }
    let key = provider.as_str();
    let p = price_file().providers.get(key).ok_or_else(|| {
        ProviderError::NotFound(format!("no storage prices bundled for provider {key}"))
    })?;

    if provider == ProviderKind::DigitalOcean
        && !p.spaces_regions.iter().any(|r| r == region.as_str())
    {
        return Err(ProviderError::NotFound(format!(
            "DigitalOcean Spaces is not available in {region}; recordings need a bucket in one \
             of: {}",
            p.spaces_regions.join(", ")
        )));
    }

    let (storage, egress) = match p.regions.get(region.as_str()) {
        Some(r) => (r.storage_microusd_per_gb_month, r.egress_microusd_per_gb),
        None => (
            p.default_storage_microusd_per_gb_month,
            p.default_egress_microusd_per_gb,
        ),
    };
    Ok(StoragePrice {
        provider,
        class: p.class.clone(),
        storage_microusd_per_gb_month: storage,
        egress_microusd_per_gb: egress,
        included_storage_gb: p.included_storage_gb,
        included_egress_gb: p.included_egress_gb,
        base_monthly_microusd: p.base_monthly_microusd,
        note: p.note.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;

    /// Two hours.
    const TWO_HOURS_SECS: u64 = 7200;

    fn aws() -> StoragePrice {
        storage_price(ProviderKind::Aws, &RegionId::new("us-east-1")).unwrap()
    }

    // ---- Audio arithmetic ----

    #[test]
    fn per_hour_rates_are_the_documented_figures() {
        let plan = RecordingPlan::mix_only();
        // 3600 * 48000 * 2ch * 2 bytes.
        assert_eq!(plan.mix_bytes_per_hour(), 691_200_000);
        assert_eq!(plan.stem_bytes_per_hour(), 345_600_000);
        let hi = RecordingPlan::mix_only().bit_depth(BitDepth::TwentyFour);
        assert_eq!(hi.mix_bytes_per_hour(), 1_036_800_000);
        assert_eq!(hi.stem_bytes_per_hour(), 518_400_000);
    }

    #[test]
    fn a_two_hour_stereo_mix_is_about_one_point_four_gigabytes() {
        // 7200 s * 48 kHz * 2 ch * 2 B + 44 B header. The product brief's
        // "about 1.3 GB" is this number in GiB.
        let plan = RecordingPlan::mix_only();
        assert_eq!(plan.mix_bytes(TWO_HOURS_SECS), 1_382_400_044);
        assert_eq!(gb_display(plan.mix_bytes(TWO_HOURS_SECS)), "1.38 GB");
        assert_eq!(plan.stem_bytes(TWO_HOURS_SECS), 691_200_044);
        assert_eq!(plan.total_bytes(TWO_HOURS_SECS), 1_382_400_044);
        assert_eq!(plan.object_count(), 1);
    }

    #[test]
    fn stems_multiply_the_total() {
        let plan = RecordingPlan::with_stems(4);
        // mix + 4 * stem.
        assert_eq!(
            plan.total_bytes(TWO_HOURS_SECS),
            1_382_400_044 + 4 * 691_200_044
        );
        assert_eq!(plan.total_bytes(TWO_HOURS_SECS), 4_147_200_220);
        assert_eq!(gb_display(plan.total_bytes(TWO_HOURS_SECS)), "4.15 GB");
        assert_eq!(plan.object_count(), 5);
        // The largest single object is still the mix, which is what bounds
        // the WAV container.
        assert_eq!(plan.largest_object_bytes(TWO_HOURS_SECS), 1_382_400_044);
    }

    #[test]
    fn wav_size_warning_fires_only_past_four_gibibytes() {
        let plan = RecordingPlan::mix_only();
        assert!(plan.wav_size_warning(TWO_HOURS_SECS).is_none());
        // 16-bit stereo hits 4 GiB after ~6.2 hours.
        assert!(plan.wav_size_warning(6 * 3600).is_none());
        let warning = plan.wav_size_warning(7 * 3600).expect("7 h must warn");
        assert!(warning.contains("4 GiB"), "{warning}");
        assert!(warning.contains("RF64"), "{warning}");
        // 24-bit runs out sooner.
        let hi = RecordingPlan::mix_only().bit_depth(BitDepth::TwentyFour);
        assert!(hi.wav_size_warning(5 * 3600).is_some());
    }

    // ---- Price table ----

    #[test]
    fn bundled_prices_cover_both_region_catalogs() {
        for region in crate::providers::aws::AwsProvider::new("i".into(), "s".into())
            .regions()
            .iter()
            .map(|r| r.id.clone())
        {
            let p = storage_price(ProviderKind::Aws, &region).unwrap();
            assert!(p.storage_microusd_per_gb_month > 0, "{region}");
            assert!(p.egress_microusd_per_gb > 0, "{region}");
            assert_eq!(p.class, "S3 Standard");
            assert_eq!(p.included_egress_gb, 100);
        }
        for region in crate::providers::gcp::GcpProvider::with_access_token("p".into(), "t".into())
            .regions()
            .iter()
            .map(|r| r.id.clone())
        {
            let p = storage_price(ProviderKind::Gcp, &region).unwrap();
            assert!(p.storage_microusd_per_gb_month > 0, "{region}");
            assert_eq!(p.egress_microusd_per_gb, 120_000, "{region}");
        }
    }

    #[test]
    fn headline_prices_match_the_published_list_rates() {
        assert_eq!(aws().storage_microusd_per_gb_month, 23_000);
        assert_eq!(aws().egress_microusd_per_gb, 90_000);
        assert_eq!(aws().included_storage_gb, 0);
        assert_eq!(aws().base_monthly_microusd, 0);
        assert_eq!(aws().storage_display(), "$0.023/GB-month");

        // Sao Paulo is dearer on both axes.
        let sa = storage_price(ProviderKind::Aws, &RegionId::new("sa-east-1")).unwrap();
        assert_eq!(sa.storage_microusd_per_gb_month, 40_500);
        assert_eq!(sa.egress_microusd_per_gb, 150_000);

        let gcp = storage_price(ProviderKind::Gcp, &RegionId::new("us-central1")).unwrap();
        assert_eq!(gcp.storage_microusd_per_gb_month, 20_000);

        let dop = storage_price(ProviderKind::DigitalOcean, &RegionId::new("nyc3")).unwrap();
        assert_eq!(dop.storage_microusd_per_gb_month, 20_000);
        assert_eq!(dop.egress_microusd_per_gb, 10_000);
        assert_eq!(dop.included_storage_gb, 250);
        assert_eq!(dop.included_egress_gb, 1024);
        assert_eq!(dop.base_monthly_microusd, 5_000_000);
    }

    #[test]
    fn an_unlisted_region_falls_back_to_the_dearest_rate() {
        // Never understate: the fallback is the most expensive listed region.
        let unknown = storage_price(ProviderKind::Aws, &RegionId::new("ap-south-1")).unwrap();
        assert_eq!(unknown.storage_microusd_per_gb_month, 40_500);
        assert_eq!(unknown.egress_microusd_per_gb, 150_000);
    }

    #[test]
    fn digitalocean_regions_without_spaces_are_refused_helpfully() {
        // nyc1 and atl1 are droplet regions with no Spaces endpoint, so
        // there is no bucket to price.
        for region in ["nyc1", "atl1"] {
            let err =
                storage_price(ProviderKind::DigitalOcean, &RegionId::new(region)).unwrap_err();
            match err {
                ProviderError::NotFound(msg) => {
                    assert!(msg.contains("not available in"), "{msg}");
                    assert!(msg.contains("nyc3"), "must name a region that works: {msg}");
                }
                other => panic!("expected NotFound, got {other:?}"),
            }
        }
        for region in ["nyc3", "fra1", "sfo3", "lon1", "blr1"] {
            assert!(storage_price(ProviderKind::DigitalOcean, &RegionId::new(region)).is_ok());
        }
    }

    #[test]
    fn local_sessions_are_not_priced() {
        let err = storage_price(ProviderKind::Local, &RegionId::new("local")).unwrap_err();
        assert!(err.to_string().contains("your own disk"));
    }

    // ---- Cost fixtures, hand-computed ----

    #[test]
    fn aws_two_hours_mix_only_thirty_days() {
        // bytes   = 1_382_400_044
        // storage = 1_382_400_044 * 23_000 * 30 / (1e9 * 30) = 31_795.20 -> 31_795
        // egress  = 1_382_400_044 * 90_000 / 1e9            = 124_416.00 -> 124_416
        let est = RecordingEstimate::compute(
            ProviderKind::Aws,
            &RegionId::new("us-east-1"),
            &RecordingPlan::mix_only(),
            2.0,
        )
        .unwrap();
        assert_eq!(est.total_bytes, 1_382_400_044);
        assert_eq!(est.billable_storage_bytes, 1_382_400_044);
        assert_eq!(est.storage_microusd, 31_795);
        assert_eq!(est.download_egress_microusd, 124_416);
        assert_eq!(est.total_microusd, 156_211);
        assert_eq!(est.object_count, 1);
    }

    #[test]
    fn aws_two_hours_with_four_stems_thirty_days() {
        // bytes   = 4_147_200_220
        // storage = 4_147_200_220 * 23_000 / 1e9 = 95_385.61 -> 95_386
        // egress  = 4_147_200_220 * 90_000 / 1e9 = 373_248.02 -> 373_248
        let est = RecordingEstimate::compute(
            ProviderKind::Aws,
            &RegionId::new("us-east-1"),
            &RecordingPlan::with_stems(4),
            2.0,
        )
        .unwrap();
        assert_eq!(est.total_bytes, 4_147_200_220);
        assert_eq!(est.mix_bytes, 1_382_400_044);
        assert_eq!(est.stem_bytes, 2_764_800_176);
        assert_eq!(est.storage_microusd, 95_386);
        assert_eq!(est.download_egress_microusd, 373_248);
        assert_eq!(est.total_microusd, 468_634);
        assert_eq!(est.object_count, 5);
    }

    #[test]
    fn gcp_two_hours_with_and_without_stems() {
        // us-central1: storage 20_000/GB-month, egress 120_000/GB.
        // mix only: storage 1_382_400_044*20_000/1e9 = 27_648.00 -> 27_648
        //           egress  1_382_400_044*120_000/1e9 = 165_888.01 -> 165_888
        let mix = RecordingEstimate::compute(
            ProviderKind::Gcp,
            &RegionId::new("us-central1"),
            &RecordingPlan::mix_only(),
            2.0,
        )
        .unwrap();
        assert_eq!(mix.storage_microusd, 27_648);
        assert_eq!(mix.download_egress_microusd, 165_888);
        assert_eq!(mix.total_microusd, 193_536);

        // stems: storage 4_147_200_220*20_000/1e9 = 82_944.00 -> 82_944
        //        egress  4_147_200_220*120_000/1e9 = 497_664.03 -> 497_664
        let stems = RecordingEstimate::compute(
            ProviderKind::Gcp,
            &RegionId::new("us-central1"),
            &RecordingPlan::with_stems(4),
            2.0,
        )
        .unwrap();
        assert_eq!(stems.storage_microusd, 82_944);
        assert_eq!(stems.download_egress_microusd, 497_664);
        assert_eq!(stems.total_microusd, 580_608);
    }

    #[test]
    fn digitalocean_two_hours_is_free_storage_inside_the_included_allowance() {
        // 250 GB included swallows both cases, so storage is genuinely $0
        // and only the download has a marginal price at $0.01/GB.
        let mix = RecordingEstimate::compute(
            ProviderKind::DigitalOcean,
            &RegionId::new("nyc3"),
            &RecordingPlan::mix_only(),
            2.0,
        )
        .unwrap();
        assert_eq!(mix.billable_storage_bytes, 0);
        assert_eq!(mix.storage_microusd, 0);
        assert_eq!(mix.download_egress_microusd, 13_824);
        assert_eq!(mix.total_microusd, 13_824);
        assert!(
            mix.notes.iter().any(|n| n.contains("already includes")),
            "the free-allowance reason must be stated: {:?}",
            mix.notes
        );
        assert!(
            mix.notes
                .iter()
                .any(|n| n.contains("$5.00/month") && n.contains("not counted")),
            "the subscription must be disclosed but excluded: {:?}",
            mix.notes
        );

        let stems = RecordingEstimate::compute(
            ProviderKind::DigitalOcean,
            &RegionId::new("nyc3"),
            &RecordingPlan::with_stems(4),
            2.0,
        )
        .unwrap();
        assert_eq!(stems.storage_microusd, 0);
        assert_eq!(stems.download_egress_microusd, 41_472);
        assert_eq!(stems.total_microusd, 41_472);
    }

    #[test]
    fn retention_scales_storage_and_nothing_else() {
        let mut totals = Vec::new();
        for retention in [
            Retention::Days7,
            Retention::Days30,
            Retention::Days90,
            Retention::KeepForever,
        ] {
            let est = RecordingEstimate::with_price(
                aws(),
                &RecordingPlan::mix_only().retention(retention),
                2.0,
            );
            // The download price cannot depend on how long we keep it.
            assert_eq!(est.download_egress_microusd, 124_416, "{retention}");
            totals.push((retention, est.storage_microusd));
        }
        // 7d:  1_382_400_044 * 23_000 * 7  / 30e9 = 7_418.88  -> 7_419
        // 30d: ... * 30 / 30e9                    = 31_795.20 -> 31_795
        // 90d: ... * 90 / 30e9                    = 95_385.60 -> 95_386
        // forever: quoted as one month, and flagged recurring.
        assert_eq!(
            totals,
            vec![
                (Retention::Days7, 7_419),
                (Retention::Days30, 31_795),
                (Retention::Days90, 95_386),
                (Retention::KeepForever, 31_795),
            ]
        );
    }

    #[test]
    fn keep_forever_says_the_charge_repeats() {
        let est = RecordingEstimate::with_price(
            aws(),
            &RecordingPlan::mix_only().retention(Retention::KeepForever),
            2.0,
        );
        assert!(
            est.notes.iter().any(|n| n.contains("repeats every month")),
            "a recurring charge must not be quoted as a one-off: {:?}",
            est.notes
        );
        assert!(est.line_items[0].label.contains("1 month, recurring"));
    }

    #[test]
    fn twenty_four_bit_costs_half_again_as_much() {
        let sixteen = RecordingEstimate::with_price(aws(), &RecordingPlan::mix_only(), 2.0);
        let twenty_four = RecordingEstimate::with_price(
            aws(),
            &RecordingPlan::mix_only().bit_depth(BitDepth::TwentyFour),
            2.0,
        );
        assert_eq!(twenty_four.total_bytes, 2_073_600_044);
        // 1.5x the samples, so 1.5x the bill to within the header.
        assert!(twenty_four.total_microusd > sixteen.total_microusd * 149 / 100);
        assert!(twenty_four.total_microusd < sixteen.total_microusd * 151 / 100);
    }

    #[test]
    fn zero_and_fractional_lengths_behave() {
        let empty = RecordingEstimate::with_price(aws(), &RecordingPlan::mix_only(), 0.0);
        // Only the WAV header exists, which rounds to nothing billable.
        assert_eq!(empty.total_bytes, WAV_HEADER_BYTES);
        assert_eq!(empty.storage_microusd, 0);
        assert_eq!(empty.total_microusd, 0);

        // A negative duration cannot produce a negative bill.
        let negative = RecordingEstimate::with_price(aws(), &RecordingPlan::mix_only(), -3.0);
        assert_eq!(negative.seconds, 0);

        // Half an hour is exactly a quarter of the two-hour mix.
        let half = RecordingEstimate::with_price(aws(), &RecordingPlan::mix_only(), 0.5);
        assert_eq!(half.total_bytes, 1800 * 48_000 * 2 * 2 + 44);
    }

    #[test]
    fn the_estimate_states_what_it_excludes() {
        let est = RecordingEstimate::with_price(aws(), &RecordingPlan::with_stems(2), 2.0);
        let all = est.notes.join(" ");
        assert!(all.contains("Excludes request charges"), "{all}");
        assert!(all.contains("under $0.001"), "{all}");
        assert!(all.contains("list prices"), "{all}");
        assert!(all.contains("30-day months"), "{all}");
        assert!(all.contains("JamStream never sees it"), "{all}");
        // AWS's 100 GB free egress has to be mentioned and explicitly not
        // double-counted.
        assert!(all.contains("100 GB/month of free download"), "{all}");
        assert!(all.contains("upper bound"), "{all}");
    }

    #[test]
    fn display_table_shape() {
        let est = RecordingEstimate::with_price(aws(), &RecordingPlan::with_stems(4), 2.0);
        let rows = est.display_table();
        assert_eq!(rows.len(), 3);
        assert!(
            rows[0].contains("Recording 4.15 GB (mix + 4 stems, 16-bit) for 30 days"),
            "{}",
            rows[0]
        );
        assert!(rows[0].ends_with("$0.095386"), "{}", rows[0]);
        assert!(
            rows[1].contains("Download once 4.15 GB at $0.09/GB"),
            "{}",
            rows[1]
        );
        assert!(rows[1].ends_with("$0.373248"), "{}", rows[1]);
        assert!(rows[2].contains("Recording total (estimate)"));
        assert!(rows[2].ends_with("$0.468634"), "{}", rows[2]);
    }

    #[test]
    fn a_single_stem_reads_as_singular() {
        let est = RecordingEstimate::with_price(aws(), &RecordingPlan::with_stems(1), 1.0);
        assert!(est.line_items[0].label.contains("mix + 1 stem,"));
        let none = RecordingEstimate::with_price(aws(), &RecordingPlan::mix_only(), 1.0);
        assert!(none.line_items[0].label.contains("mix only"));
    }
}
