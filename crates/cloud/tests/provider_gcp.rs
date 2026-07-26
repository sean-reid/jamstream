//! Wiremock-backed integration tests for the GCP Compute Engine provider.

use std::sync::Arc;

use jamstream_cloud::providers::gcp::{GcpProvider, ServiceAccountTokenSource};
use jamstream_cloud::{
    InstanceClass, LaunchSpec, Provider, ProviderError, ProviderKind, Region, RegionId, session_tag,
};
use serde_json::{Value, json};
use wiremock::matchers::{
    body_string_contains, header, method, path, query_param, query_param_is_missing,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROJECT: &str = "test-project";

fn provider(server: &MockServer) -> GcpProvider {
    GcpProvider::with_access_token(PROJECT.to_owned(), "test-token".to_owned())
        .with_base_url(server.uri())
}

fn region(p: &GcpProvider, id: &str) -> Region {
    p.regions()
        .into_iter()
        .find(|r| r.id.as_str() == id)
        .unwrap_or_else(|| panic!("region {id} not in catalog"))
}

fn zone_path(zone: &str) -> String {
    format!("/compute/v1/projects/{PROJECT}/zones/{zone}/instances")
}

fn spec(p: &GcpProvider) -> LaunchSpec {
    LaunchSpec {
        region: region(p, "us-central1"),
        instance_class: InstanceClass::Small,
        user_data: "#cloud-config\nwrite_files: []\n".to_owned(),
        tags: vec![session_tag("deadbeefcafef00d")],
    }
}

#[tokio::test]
async fn launch_inserts_instance_with_expected_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(zone_path("us-central1-b")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "name": "operation-1", "status": "RUNNING" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server).with_max_run_seconds(7200);
    let instance = p.launch(spec(&p)).await.expect("launch");

    assert_eq!(instance.provider, ProviderKind::Gcp);
    assert!(instance.id.starts_with("jamstream-"), "id is the VM name");
    assert_eq!(instance.public_ip, None, "IP arrives later via refresh");
    assert_eq!(instance.session_id(), Some("deadbeefcafef00d"));

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    let url_path = req.url.path();
    assert!(url_path.contains(PROJECT), "path must contain the project");
    assert!(
        url_path.contains("us-central1-b"),
        "path must contain the zone"
    );

    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["name"], instance.id.as_str());
    assert_eq!(
        body["machineType"],
        "zones/us-central1-b/machineTypes/e2-small"
    );
    assert_eq!(body["disks"][0]["boot"], true);
    assert_eq!(
        body["disks"][0]["initializeParams"]["sourceImage"],
        "projects/debian-cloud/global/images/family/debian-12"
    );
    assert_eq!(
        body["networkInterfaces"][0]["accessConfigs"][0]["type"],
        "ONE_TO_ONE_NAT"
    );
    assert_eq!(body["metadata"]["items"][0]["key"], "user-data");
    assert_eq!(
        body["metadata"]["items"][0]["value"],
        "#cloud-config\nwrite_files: []\n"
    );
    assert_eq!(body["labels"]["jamstream-session"], "deadbeefcafef00d");
    assert_eq!(body["labels"]["jamstream"], "true");
    assert_eq!(body["scheduling"]["maxRunDuration"]["seconds"], "7200");
    assert_eq!(body["scheduling"]["instanceTerminationAction"], "DELETE");
}

#[tokio::test]
async fn launch_in_unknown_region_is_not_found_without_network() {
    let server = MockServer::start().await;
    let p = provider(&server);
    let mut bad = spec(&p);
    bad.region = Region {
        provider: ProviderKind::Gcp,
        id: RegionId::new("mars-north1"),
        display: String::new(),
        country: String::new(),
    };
    let err = p.launch(bad).await.unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn refresh_parses_nat_ip_status_and_labels() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{}/jamstream-abc",
            zone_path("europe-west3-b")
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "jamstream-abc",
            "status": "RUNNING",
            "labels": {
                "jamstream": "true",
                "jamstream-session": "deadbeefcafef00d",
                // "Owner" -> "Sean Reid" through the hex label escape.
                "x--4f776e6572": "x--5365616e2052656964"
            },
            "networkInterfaces": [{
                "accessConfigs": [{ "type": "ONE_TO_ONE_NAT", "natIP": "203.0.113.7" }]
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let refreshed = p
        .refresh(&RegionId::new("europe-west3"), "jamstream-abc")
        .await
        .expect("refresh");

    assert_eq!(refreshed.status, "RUNNING");
    let inst = refreshed.instance;
    assert_eq!(inst.id, "jamstream-abc");
    assert_eq!(inst.public_ip, Some("203.0.113.7".parse().unwrap()));
    assert_eq!(inst.session_id(), Some("deadbeefcafef00d"));
    // Escaped labels decode back to canonical tags; the marker is dropped.
    assert!(
        inst.tags
            .contains(&("Owner".to_owned(), "Sean Reid".to_owned()))
    );
    assert!(!inst.tags.iter().any(|(k, _)| k == "jamstream"));
}

#[tokio::test]
async fn destroy_missing_instance_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!("{}/jamstream-gone", zone_path("us-east1-b"))))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let err = p
        .destroy(&RegionId::new("us-east1"), "jamstream-gone")
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)));
}

#[tokio::test]
async fn destroy_retries_transient_500_then_succeeds() {
    let server = MockServer::start().await;
    let instance_path = format!("{}/jamstream-flaky", zone_path("us-west1-b"));
    Mock::given(method("DELETE"))
        .and(path(instance_path.clone()))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(instance_path))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "name": "operation-del" })))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    p.destroy(&RegionId::new("us-west1"), "jamstream-flaky")
        .await
        .expect("destroy after retry");
}

#[tokio::test]
async fn unauthorized_maps_to_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!("{}/jamstream-x", zone_path("us-central1-b"))))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let err = p
        .destroy(&RegionId::new("us-central1"), "jamstream-x")
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Auth(_)));
}

#[tokio::test]
async fn list_tagged_session_filter_is_sent_and_decoded() {
    let server = MockServer::start().await;
    // Only us-central1-b answers; the other eight catalog zones fall
    // through to wiremock's default 404 and are tolerated.
    Mock::given(method("GET"))
        .and(path(zone_path("us-central1-b")))
        .and(query_param(
            "filter",
            "labels.jamstream-session=deadbeefcafef00d",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "name": "jamstream-one",
                "status": "RUNNING",
                "labels": { "jamstream": "true", "jamstream-session": "deadbeefcafef00d" },
                "networkInterfaces": [{
                    "accessConfigs": [{ "natIP": "198.51.100.4" }]
                }]
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let found = p.list_tagged(Some("deadbeefcafef00d")).await.expect("list");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "jamstream-one");
    assert_eq!(found[0].region.id.as_str(), "us-central1");
    assert_eq!(found[0].session_id(), Some("deadbeefcafef00d"));
    assert_eq!(found[0].public_ip, Some("198.51.100.4".parse().unwrap()));
}

#[tokio::test]
async fn list_tagged_aggregates_zones_and_skips_failing_zone() {
    let server = MockServer::start().await;
    let marker_filter = || query_param("filter", "labels.jamstream=true");

    Mock::given(method("GET"))
        .and(path(zone_path("us-central1-b")))
        .and(marker_filter())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "name": "jamstream-a",
                "labels": { "jamstream": "true", "jamstream-session": "aaaa" }
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(zone_path("europe-west2-b")))
        .and(marker_filter())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "name": "jamstream-b",
                "labels": { "jamstream": "true", "jamstream-session": "bbbb" }
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    // This zone fails hard every time; the aggregate must still succeed.
    Mock::given(method("GET"))
        .and(path(zone_path("us-east1-b")))
        .and(marker_filter())
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let p = provider(&server);
    let mut found = p.list_tagged(None).await.expect("list all");
    found.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(found.len(), 2, "one instance per healthy zone");
    assert_eq!(found[0].id, "jamstream-a");
    assert_eq!(found[0].region.id.as_str(), "us-central1");
    assert_eq!(found[1].id, "jamstream-b");
    assert_eq!(found[1].region.id.as_str(), "europe-west2");
}

#[tokio::test]
async fn list_tagged_follows_next_page_token_within_a_zone() {
    let server = MockServer::start().await;
    let marker_filter = || query_param("filter", "labels.jamstream=true");

    // Page one carries a nextPageToken; page two ends the listing. Only
    // us-central1-b answers, the other zones 404 and are tolerated.
    Mock::given(method("GET"))
        .and(path(zone_path("us-central1-b")))
        .and(marker_filter())
        .and(query_param_is_missing("pageToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "name": "jamstream-page1",
                "labels": { "jamstream": "true", "jamstream-session": "aaaa" }
            }],
            "nextPageToken": "tok-page-2"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(zone_path("us-central1-b")))
        .and(marker_filter())
        .and(query_param("pageToken", "tok-page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "name": "jamstream-page2",
                "labels": { "jamstream": "true", "jamstream-session": "bbbb" }
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let mut found = p.list_tagged(None).await.expect("list across pages");
    found.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(found.len(), 2, "both pages must be fetched");
    assert_eq!(found[0].id, "jamstream-page1");
    assert_eq!(found[1].id, "jamstream-page2");
    assert_eq!(found[1].session_id(), Some("bbbb"));
    server.verify().await;
}

/// End to end through the native service-account flow: the throwaway
/// committed test key signs a JWT, the mock token endpoint exchanges it,
/// and the resulting bearer token authenticates a `list_tagged` call.
#[tokio::test]
async fn list_tagged_authenticates_via_native_service_account_source() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer",
        ))
        .and(body_string_contains("assertion="))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "native-minted-token",
            "expires_in": 3600,
            "token_type": "Bearer",
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Only requests carrying the freshly minted token are answered; every
    // zone list must therefore have authenticated through the source.
    Mock::given(method("GET"))
        .and(path(zone_path("us-central1-b")))
        .and(header("authorization", "Bearer native-minted-token"))
        .and(query_param(
            "filter",
            "labels.jamstream-session=deadbeefcafef00d",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "name": "jamstream-native",
                "status": "RUNNING",
                "labels": { "jamstream": "true", "jamstream-session": "deadbeefcafef00d" }
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let key_path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/gcp_test_key.json");
    let source = ServiceAccountTokenSource::from_file(key_path)
        .expect("committed test fixture must parse")
        .with_token_endpoint(format!("{}/token", server.uri()));
    let p = GcpProvider::with_token_source(PROJECT.to_owned(), Arc::new(source))
        .with_base_url(server.uri());

    let found = p
        .list_tagged(Some("deadbeefcafef00d"))
        .await
        .expect("list via native auth");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "jamstream-native");
    server.verify().await;
}

#[tokio::test]
async fn every_catalog_region_has_a_positive_price() {
    let p = GcpProvider::with_access_token(PROJECT.to_owned(), "t".to_owned());
    let regions = p.regions();
    assert_eq!(regions.len(), 9);
    for r in &regions {
        assert_eq!(r.provider, ProviderKind::Gcp);
        let small = p
            .price_for(&r.id, InstanceClass::Small)
            .unwrap_or_else(|e| panic!("small price for {}: {e}", r.id));
        let standard = p
            .price_for(&r.id, InstanceClass::Standard)
            .unwrap_or_else(|e| panic!("standard price for {}: {e}", r.id));
        assert!(small.hourly_microusd > 0, "zero small price for {}", r.id);
        assert!(
            standard.hourly_microusd > small.hourly_microusd,
            "standard must cost more than small in {}",
            r.id
        );
        // The trait-level price() reports the Standard session size.
        let price = p
            .price(&r.id)
            .await
            .unwrap_or_else(|e| panic!("price for {}: {e}", r.id));
        assert_eq!(price, standard);
        assert_eq!(price.egress_microusd_per_gb, 120_000);
        assert_eq!(price.included_egress_gb, 0);
    }
    let err = p.price(&RegionId::new("mars-north1")).await.unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)));
    let err = p
        .price_for(&RegionId::new("mars-north1"), InstanceClass::Small)
        .unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)));
}
