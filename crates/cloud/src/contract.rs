//! Generic behavioral suite every Provider implementation must pass.
//! Run it from each implementation's tests; a new provider merges only if
//! this passes unchanged.

use crate::provider::{Provider, ProviderError};
use crate::types::{InstanceClass, LaunchSpec, ProviderKind, RegionId, session_tag};

/// Panics on the first contract violation. Callers supply a provider with
/// at least one advertised region and no pre-existing jamstream instances.
pub async fn assert_provider_contract(p: &dyn Provider) {
    let regions = p.regions();
    assert!(
        !regions.is_empty(),
        "provider must advertise at least one region"
    );
    for r in &regions {
        assert_eq!(r.provider, p.kind(), "region {} has wrong provider", r.id);
        let price = p
            .price(&r.id)
            .await
            .unwrap_or_else(|e| panic!("price lookup failed for region {}: {e}", r.id));
        // Local sessions are free by design; every cloud must cost money.
        assert!(
            price.hourly_microusd > 0 || p.kind() == ProviderKind::Local,
            "region {} advertises a zero hourly price",
            r.id
        );
    }

    let unknown = RegionId::new("jamstream-contract-no-such-region");
    match p.price(&unknown).await {
        Err(ProviderError::NotFound(_)) => {}
        other => panic!("price for unknown region must be NotFound, got {other:?}"),
    }
    match p.destroy(&unknown, "jamstream-contract-no-such-id").await {
        Err(ProviderError::NotFound(_)) => {}
        other => panic!("destroy of unknown instance must be NotFound, got {other:?}"),
    }

    let region = regions[0].clone();
    let spec = |session: &str| LaunchSpec {
        region: region.clone(),
        instance_class: InstanceClass::Small,
        user_data: "#cloud-config\n".to_owned(),
        tags: vec![session_tag(session)],
    };

    let a = p.launch(spec("contract-a")).await.expect("launch a");
    let b = p.launch(spec("contract-b")).await.expect("launch b");
    assert_ne!(a.id, b.id, "instance ids must be unique");
    assert_eq!(a.session_id(), Some("contract-a"));

    let only_a = p.list_tagged(Some("contract-a")).await.expect("list a");
    assert_eq!(only_a.len(), 1, "session filter must return exactly a");
    assert_eq!(only_a[0].id, a.id);

    let all = p.list_tagged(None).await.expect("list all");
    let ids: Vec<&str> = all.iter().map(|i| i.id.as_str()).collect();
    assert!(
        ids.contains(&a.id.as_str()) && ids.contains(&b.id.as_str()),
        "list with None must return all jamstream-tagged instances"
    );

    match p
        .launch(LaunchSpec {
            region: crate::types::Region {
                provider: p.kind(),
                id: unknown.clone(),
                display: String::new(),
                country: String::new(),
            },
            instance_class: InstanceClass::Small,
            user_data: String::new(),
            tags: vec![session_tag("contract-a")],
        })
        .await
    {
        Err(ProviderError::NotFound(_)) => {}
        Ok(inst) => panic!(
            "launch in unknown region must fail, got instance {}",
            inst.id
        ),
        Err(other) => panic!("launch in unknown region must be NotFound, got {other}"),
    }

    p.destroy(&a.region.id, &a.id).await.expect("destroy a");
    let after = p.list_tagged(Some("contract-a")).await.expect("list a");
    assert!(after.is_empty(), "destroyed instance must disappear");
    match p.destroy(&a.region.id, &a.id).await {
        Err(ProviderError::NotFound(_)) => {}
        other => panic!("double destroy must be NotFound, got {other:?}"),
    }

    p.destroy(&b.region.id, &b.id).await.expect("destroy b");
    assert!(
        p.list_tagged(None).await.expect("final list").is_empty(),
        "contract suite must leave nothing behind"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::types::ProviderKind;

    #[tokio::test]
    async fn mock_provider_passes_contract() {
        for kind in [
            ProviderKind::Aws,
            ProviderKind::DigitalOcean,
            ProviderKind::Gcp,
        ] {
            let p = MockProvider::with_default_regions(kind);
            assert_provider_contract(&p).await;
        }
    }
}
