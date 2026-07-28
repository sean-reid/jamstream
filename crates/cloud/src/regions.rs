//! Building the region table a host picks from.
//!
//! A row in that table carries two facts about a region, its price and the
//! round trip measured to it, and either can be absent. They were handled
//! badly in opposite directions: an unpriced region aborted the whole table,
//! and an unmeasured one was rendered as 0 ms and sorted first. Absent is
//! neither fatal nor zero.
//!
//! The two absences are not the same, so they are not treated the same:
//!
//! - **No price** is per region and permanent for this account. DigitalOcean's
//!   atl1 does not carry `s-2vcpu-2gb`, so there is no session to run there
//!   at all. That region leaves the table; see [`priced_regions`].
//! - **No probe** is transient and says nothing about the region. Those rows
//!   stay in the table and say they were not measured, and [`rank`](crate::rank)
//!   sorts them behind every region that was.

use crate::provider::{Provider, ProviderError, Result};
use crate::types::{Price, Region};

/// The regions a session can actually run in, and the ones ruled out.
#[derive(Debug, Clone, Default)]
pub struct RegionTable {
    /// Regions this account can launch in, with their hourly price. Never
    /// empty: [`priced_regions`] errors rather than returning nothing.
    pub candidates: Vec<(Region, Price)>,
    /// Regions dropped because the provider does not offer this session's
    /// instance size there. Worth naming in the interface so the table is
    /// not silently shorter than the provider's own region list.
    pub unavailable: Vec<Region>,
}

/// Prices every region the provider offers, dropping the ones where the
/// instance size is not available.
///
/// A region that cannot run our instance is not an error, it is a region we
/// cannot use, and it must not take the rest of the table with it: one
/// missing size in `atl1` used to abort the whole DigitalOcean region step.
/// Everything else is a real failure and does stop the table, because a
/// token that cannot read the size catalog at all, or a network that is
/// down, would otherwise render as an empty list of regions, and a
/// convincing empty answer is worse than an error.
pub async fn priced_regions(provider: &dyn Provider) -> Result<RegionTable> {
    let regions = provider.regions();
    if regions.is_empty() {
        return Err(ProviderError::Other(format!(
            "{} offers no regions",
            provider.kind().as_str()
        )));
    }
    let offered = regions.len();
    let mut table = RegionTable::default();
    for region in regions {
        match provider.price(&region.id).await {
            Ok(price) => table.candidates.push((region, price)),
            Err(ProviderError::NotFound(reason)) => {
                tracing::info!(
                    region = %region.id,
                    %reason,
                    "region dropped from the table: this session's instance size is not offered there"
                );
                table.unavailable.push(region);
            }
            Err(err) => return Err(err),
        }
    }
    if table.candidates.is_empty() {
        // A real state, not a theoretical one: an account restricted to
        // regions that do not carry the size lands here. `Other` rather than
        // `NotFound` because this string is shown to the host as written,
        // and "not found: no digitalocean region..." reads like a bug.
        return Err(ProviderError::Other(format!(
            "no {} region can run this session: the machine size is unavailable in all {offered} \
             regions the account offers",
            provider.kind().as_str()
        )));
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::types::{ProviderKind, RegionId};

    const KIND: ProviderKind = ProviderKind::DigitalOcean;

    fn region(id: &str) -> Region {
        Region {
            provider: KIND,
            id: RegionId::new(id),
            display: id.to_owned(),
            country: "US".to_owned(),
        }
    }

    fn price(hourly: u64) -> Price {
        Price {
            hourly_microusd: hourly,
            egress_microusd_per_gb: 10_000,
            included_egress_gb: 3000,
        }
    }

    /// The DigitalOcean defect in miniature: atl1 is a region the account
    /// really offers, `s-2vcpu-2gb` really is not sold there, and the first
    /// such region used to end the region step for every other region too.
    #[tokio::test]
    async fn a_region_without_our_size_leaves_the_table_and_the_rest_stay() {
        let p = MockProvider::new(KIND)
            .with_region(region("nyc3"), price(26_790))
            .with_unpriced_region(region("atl1"))
            .with_region(region("sfo3"), price(26_790));
        let table = priced_regions(&p).await.expect("the rest of the table");
        let ids: Vec<&str> = table
            .candidates
            .iter()
            .map(|(r, _)| r.id.as_str())
            .collect();
        assert_eq!(ids, vec!["nyc3", "sfo3"]);
        let dropped: Vec<&str> = table.unavailable.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(dropped, vec!["atl1"]);
    }

    /// The distinction the fix rests on. An unavailable size is not a
    /// failure; a token that cannot read prices at all is, and it must not
    /// arrive looking like a slightly shorter table.
    #[tokio::test]
    async fn a_real_price_failure_still_stops_the_table() {
        let p = MockProvider::new(KIND)
            .with_region(region("nyc3"), price(26_790))
            .with_region(region("sfo3"), price(26_790));
        p.fail_next_prices(1, ProviderError::Auth("bad token".to_owned()));
        let err = priced_regions(&p).await.unwrap_err();
        assert!(matches!(err, ProviderError::Auth(_)), "got {err:?}");
    }

    /// A restricted account can genuinely have no usable region. That is an
    /// error with a sentence in it, not an empty table.
    #[tokio::test]
    async fn no_usable_region_is_an_error_that_says_so() {
        let p = MockProvider::new(KIND)
            .with_unpriced_region(region("atl1"))
            .with_unpriced_region(region("blr1"));
        let err = priced_regions(&p).await.unwrap_err();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("no digitalocean region can run this session"),
            "the host reads this verbatim: {rendered}"
        );
        assert!(
            rendered.contains("machine size is unavailable"),
            "{rendered}"
        );
        assert!(rendered.contains("2 regions"), "{rendered}");
    }

    #[tokio::test]
    async fn a_provider_with_no_regions_at_all_is_an_error() {
        let p = MockProvider::new(KIND);
        let err = priced_regions(&p).await.unwrap_err();
        assert!(err.to_string().contains("offers no regions"), "{err}");
    }
}
