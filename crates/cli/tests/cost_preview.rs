//! User story: the host sees a cost preview before launching, and the number
//! shown is the number the pricing library computes for those exact inputs.
//! Any drift between display and computation is a lie to the person paying.
//!
//! One test function: the state directory override is process-global env.

use jamstream_cli::cli::HostArgs;
use jamstream_cli::{host, state};
use jamstream_cloud::{CostPreview, MockProvider, Provider, ProviderKind, RegionId};

#[tokio::test]
async fn host_json_preview_matches_the_library_computation() {
    let state_dir = std::env::temp_dir().join(format!(
        "jamstream-cli-cost-preview-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    // Safety: this test binary is single-test and sets the variable before
    // any state access.
    unsafe {
        std::env::set_var(state::STATE_DIR_ENV, &state_dir);
    }

    // Fixed flags with every cost dial turned: fractional hours, musicians,
    // listeners, and stream destinations.
    let args = HostArgs {
        provider: "mock".to_owned(),
        region: None,
        musicians: 3,
        listeners: 2,
        hours: 2.5,
        destinations: 2,
        port: 43210,
        idle_min: 10,
        max_hours: 12,
        record: false,
        record_stems: false,
        bucket: None,
        retention: jamstream_cloud::Retention::Days30,
        artifact_url: None,
        artifact_sha256: None,
        yes: true,
        json: true,
    };
    let provider = MockProvider::with_default_regions(ProviderKind::Aws);
    let mut out = Vec::new();
    host::run(&args, &provider, &mut out).await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();

    // Recompute the preview from the provider's own price for the region
    // the host actually chose.
    let region = RegionId::new(json["region"].as_str().unwrap());
    let price = provider.price(&region).await.unwrap();
    let preview = CostPreview::compute(
        &price,
        args.hours,
        args.musicians,
        args.destinations,
        args.listeners,
    );

    assert_eq!(
        json["hourly_microusd"].as_u64(),
        Some(price.hourly_microusd),
        "shown hourly rate drifted from the provider's price"
    );
    assert_eq!(
        json["estimated_total_microusd"].as_u64(),
        Some(preview.total_microusd),
        "shown total drifted from CostPreview::compute for the same inputs"
    );

    std::fs::remove_dir_all(&state_dir).unwrap();
}
