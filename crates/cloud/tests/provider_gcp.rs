//! Wiremock-backed integration tests for the GCP Compute Engine provider.

// Every test here is about GCP, which sits behind the `gcp` feature so
// jamstreamd can build without aws-lc. With the feature off this file is
// empty rather than broken.
#![cfg(feature = "gcp")]

use std::sync::Arc;

use jamstream_cloud::providers::gcp::{GcpProvider, ServiceAccountTokenSource};
use jamstream_cloud::{
    BootConfig, InstanceClass, LaunchSpec, Provider, ProviderError, ProviderKind, Region, RegionId,
    SelfDestruct, session_tag,
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

fn firewalls_path() -> String {
    format!("/compute/v1/projects/{PROJECT}/global/firewalls")
}

/// Answers the two firewall inserts a launch makes before the instance.
async fn mount_firewalls(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(firewalls_path()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "name": "operation-fw", "status": "RUNNING" })),
        )
        .mount(server)
        .await;
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

    mount_firewalls(&server).await;

    let p = provider(&server).with_max_run_seconds(7200);
    let instance = p.launch(spec(&p)).await.expect("launch");

    assert_eq!(instance.provider, ProviderKind::Gcp);
    assert!(instance.id.starts_with("jamstream-"), "id is the VM name");
    assert_eq!(instance.public_ip, None, "IP arrives later via refresh");
    assert_eq!(instance.session_id(), Some("deadbeefcafef00d"));

    let requests = server.received_requests().await.unwrap();
    // Two firewall rules, then the instance.
    assert_eq!(requests.len(), 3);
    let firewalls: Vec<Value> = requests
        .iter()
        .filter(|r| r.url.path() == firewalls_path())
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(firewalls.len(), 2, "one allow rule and one deny rule");
    let allow = &firewalls[0];
    assert_eq!(allow["name"], "jamstream-deadbeefcafef00d-allow");
    assert_eq!(allow["network"], "global/networks/default");
    assert_eq!(allow["direction"], "INGRESS");
    assert_eq!(allow["priority"], 900);
    assert_eq!(allow["sourceRanges"][0], "0.0.0.0/0");
    assert_eq!(allow["targetTags"][0], "jamstream-deadbeefcafef00d");
    assert_eq!(allow["allowed"][0]["IPProtocol"], "udp");
    assert_eq!(allow["allowed"][0]["ports"][0], "43210");
    // The deny rule outranks default-allow-ssh (priority 65534) for this
    // instance without touching the project's own rules.
    let deny = &firewalls[1];
    assert_eq!(deny["name"], "jamstream-deadbeefcafef00d-deny");
    assert_eq!(deny["priority"], 1000);
    assert_eq!(deny["denied"][0]["IPProtocol"], "all");
    assert!(deny["allowed"].is_null());

    let req = requests
        .iter()
        .find(|r| r.url.path() != firewalls_path())
        .expect("instance insert");
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
    assert_eq!(
        body["networkInterfaces"][0]["network"],
        "global/networks/default"
    );
    assert_eq!(body["tags"]["items"][0], "jamstream-deadbeefcafef00d");
    assert_eq!(body["metadata"]["items"][0]["key"], "user-data");
    assert_eq!(
        body["metadata"]["items"][0]["value"],
        "#cloud-config\nwrite_files: []\n"
    );
    assert_eq!(body["labels"]["jamstream-session"], "deadbeefcafef00d");
    assert_eq!(body["labels"]["jamstream"], "true");
    assert_eq!(body["scheduling"]["maxRunDuration"]["seconds"], "7200");
    assert_eq!(body["scheduling"]["instanceTerminationAction"], "DELETE");
    // Project-wide SSH keys must not reach a session VM.
    assert_eq!(
        body["metadata"]["items"][1]["key"],
        "block-project-ssh-keys"
    );
    assert_eq!(body["metadata"]["items"][1]["value"], "TRUE");
    // Nothing on the instance may hold a credential.
    assert_eq!(body["serviceAccounts"], json!([]));
}

/// #51 end to end: `--max-hours` used to reach the API nowhere, so every
/// GCP session was capped at the 12 h default however long the host asked
/// for. The cap travels in the cloud-init the VM boots from, which is the
/// only channel a `LaunchSpec` has for it.
#[tokio::test]
async fn the_run_cap_on_the_wire_is_the_session_cap() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(zone_path("us-central1-b")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "name": "operation-1", "status": "RUNNING" })),
        )
        .mount(&server)
        .await;
    mount_firewalls(&server).await;

    let p = provider(&server);
    let boot = BootConfig {
        artifact_url: "https://example.invalid/jamstreamd".to_owned(),
        artifact_sha256: "0".repeat(64),
        server_private_key_b64: "c2s=".to_owned(),
        issuer_public_key_b64: "aXA=".to_owned(),
        session_id_hex: "deadbeefcafef00d".to_owned(),
        port: 43210,
        idle_shutdown_min: 10,
        // The host asked for two hours.
        max_duration_min: 120,
        self_destruct: SelfDestruct::GcpMaxRunDuration,
        recording: None,
    };
    let mut spec = spec(&p);
    spec.user_data = jamstream_cloud::cloudinit::render(&boot);
    p.launch(spec).await.expect("launch");

    let requests = server.received_requests().await.unwrap();
    let insert = requests
        .iter()
        .find(|r| r.url.path() != firewalls_path())
        .expect("instance insert");
    let body: Value = serde_json::from_slice(&insert.body).unwrap();
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

// ---- Firewall rules ----

#[tokio::test]
async fn session_ingress_reads_the_allow_rule() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(firewalls_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                // The project's own rule, which we must not report as ours.
                { "name": "default-allow-ssh", "sourceRanges": ["0.0.0.0/0"],
                  "allowed": [{ "IPProtocol": "tcp", "ports": ["22"] }] },
                { "name": "jamstream-deadbeefcafef00d-allow",
                  "sourceRanges": ["0.0.0.0/0"],
                  "targetTags": ["jamstream-deadbeefcafef00d"],
                  "allowed": [{ "IPProtocol": "udp", "ports": ["43210"] }] },
                { "name": "jamstream-deadbeefcafef00d-deny",
                  "sourceRanges": ["0.0.0.0/0"],
                  "targetTags": ["jamstream-deadbeefcafef00d"],
                  "denied": [{ "IPProtocol": "all" }] },
            ],
        })))
        .mount(&server)
        .await;

    let p = provider(&server);
    let rules = p
        .session_ingress("deadbeefcafef00d")
        .await
        .expect("ingress");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].protocol, "udp");
    assert!(rules[0].is_only_port(43210));
    assert!(rules[0].is_open_to_the_internet());
    assert!(
        p.session_ingress("nosuchsession").await.unwrap().is_empty(),
        "a session with no rule reports no ingress"
    );
}

#[tokio::test]
async fn launch_survives_rules_a_previous_attempt_created() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(firewalls_path()))
        // GCP answers a repeated insert with 409 alreadyExists.
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": { "code": 409, "message": "already exists" },
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(zone_path("us-central1-b")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "name": "operation-1", "status": "RUNNING" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    p.launch(spec(&p)).await.expect("launch");
    server.verify().await;
}

#[tokio::test]
async fn orphan_cleanup_spares_the_rules_of_a_live_session() {
    let server = MockServer::start().await;
    // One instance is still up, in one zone; every other zone is empty.
    Mock::given(method("GET"))
        .and(path(zone_path("us-central1-b")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "name": "jamstream-live",
                "status": "RUNNING",
                "labels": { "jamstream-session": "live", "jamstream": "true" },
            }],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(firewalls_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                { "name": "jamstream-live-allow", "targetTags": ["jamstream-live"] },
                { "name": "jamstream-live-deny", "targetTags": ["jamstream-live"] },
                { "name": "jamstream-gone-allow", "targetTags": ["jamstream-gone"] },
                { "name": "jamstream-gone-deny", "targetTags": ["jamstream-gone"] },
                // Not ours: no prefix, so it is never a candidate.
                { "name": "default-allow-ssh", "targetTags": [] },
            ],
        })))
        .mount(&server)
        .await;
    // Every other zone answers empty. Mounted after the two mocks above
    // because wiremock serves the first match.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
        .mount(&server)
        .await;
    for name in ["jamstream-gone-allow", "jamstream-gone-deny"] {
        Mock::given(method("DELETE"))
            .and(path(format!("{}/{name}", firewalls_path())))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "name": "operation-del", "status": "DONE" })),
            )
            .expect(1)
            .mount(&server)
            .await;
    }

    let p = provider(&server);
    let deleted = p.destroy_orphan_firewalls().await.expect("cleanup");
    assert_eq!(
        deleted,
        vec![
            "jamstream-gone-allow".to_owned(),
            "jamstream-gone-deny".to_owned()
        ]
    );
    server.verify().await;
}
