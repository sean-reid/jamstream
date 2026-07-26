//! Wiremock-backed tests for the DigitalOcean provider: every request the
//! provider can make is exercised against a faked v2 API.

use std::net::IpAddr;

use serde_json::json;
use wiremock::matchers::{
    body_partial_json, header, method, path, query_param, query_param_is_missing,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

use jamstream_cloud::providers::digitalocean::{
    BARE_TAG, DigitalOceanProvider, from_do_tag, session_do_tag, to_do_tag,
};
use jamstream_cloud::{
    Instance, InstanceClass, LaunchSpec, Provider, ProviderError, ProviderKind, Region, RegionId,
    session_id_from_tags, session_tag,
};

const TOKEN: &str = "dop_v1_test_token";

fn provider(server: &MockServer) -> DigitalOceanProvider {
    DigitalOceanProvider::new(TOKEN.to_owned()).with_base_url(server.uri())
}

fn region(p: &DigitalOceanProvider, slug: &str) -> Region {
    p.regions()
        .into_iter()
        .find(|r| r.id.as_str() == slug)
        .expect("slug in static catalog")
}

/// A realistic droplet object as the v2 API returns it.
fn droplet_json(
    id: u64,
    region: &str,
    tags: &[&str],
    public_ip: Option<&str>,
) -> serde_json::Value {
    let mut v4 = vec![json!({
        "ip_address": "10.128.10.5",
        "netmask": "255.255.0.0",
        "gateway": "10.128.0.1",
        "type": "private",
    })];
    if let Some(ip) = public_ip {
        v4.push(json!({
            "ip_address": ip,
            "netmask": "255.255.240.0",
            "gateway": "203.0.113.1",
            "type": "public",
        }));
    }
    json!({
        "id": id,
        "name": format!("jamstream-{id}"),
        "memory": 2048,
        "vcpus": 1,
        "disk": 50,
        "locked": false,
        "status": if public_ip.is_some() { "active" } else { "new" },
        "created_at": "2026-07-25T12:00:00Z",
        "features": ["monitoring"],
        "image": { "id": 63663980, "slug": "debian-12-x64" },
        "size_slug": "s-1vcpu-2gb",
        "networks": { "v4": v4, "v6": [] },
        "region": { "name": "New York 1", "slug": region, "available": true },
        "tags": tags,
    })
}

#[tokio::test]
async fn create_happy_path_sends_full_body_and_parses_response() {
    let server = MockServer::start().await;
    let tags = ["jamstream", "jamstream-session:sess1"];
    Mock::given(method("POST"))
        .and(path("/v2/droplets"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(body_partial_json(json!({
            "name": "jamstream-sess1",
            "region": "nyc1",
            "size": "s-1vcpu-2gb",
            "image": "debian-12-x64",
            "user_data": "#cloud-config\nhello\n",
            "tags": tags,
        })))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "droplet": droplet_json(3_164_494, "nyc1", &tags, None),
            "links": {},
            "meta": {},
        })))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let inst = p
        .launch(LaunchSpec {
            region: region(&p, "nyc1"),
            instance_class: InstanceClass::Small,
            user_data: "#cloud-config\nhello\n".to_owned(),
            tags: vec![session_tag("sess1")],
        })
        .await
        .expect("launch");

    assert_eq!(inst.provider, ProviderKind::DigitalOcean);
    assert_eq!(inst.id, "3164494");
    assert_eq!(inst.region.id.as_str(), "nyc1");
    // DO assigns the public IP asynchronously; create returns none.
    assert_eq!(inst.public_ip, None);
    // Tags come back in canonical (key, value) form.
    assert_eq!(inst.session_id(), Some("sess1"));
    assert!(inst.tags.contains(&(BARE_TAG.to_owned(), String::new())));
}

#[tokio::test]
async fn launch_in_unknown_region_is_not_found_without_a_request() {
    // No mocks mounted: any request would 404 the mock server and fail the
    // NotFound-vs-Other distinction below.
    let server = MockServer::start().await;
    let p = provider(&server);
    let err = p
        .launch(LaunchSpec {
            region: Region {
                provider: ProviderKind::DigitalOcean,
                id: RegionId::new("atlantis1"),
                display: String::new(),
                country: String::new(),
            },
            instance_class: InstanceClass::Small,
            user_data: String::new(),
            tags: vec![session_tag("s")],
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn refresh_parses_public_ipv4_from_networks() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/droplets/3164494"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "droplet": droplet_json(
                3_164_494,
                "fra1",
                &["jamstream", "jamstream-session:sess1"],
                Some("203.0.113.5"),
            ),
        })))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let inst = p
        .refresh(&RegionId::new("fra1"), "3164494")
        .await
        .expect("refresh");
    assert_eq!(
        inst.public_ip,
        Some("203.0.113.5".parse::<IpAddr>().unwrap())
    );
    assert_eq!(inst.region.id.as_str(), "fra1");
    assert_eq!(inst.session_id(), Some("sess1"));
}

#[tokio::test]
async fn delete_missing_droplet_maps_404_to_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/v2/droplets/999"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "id": "not_found",
            "message": "The resource you requested could not be found.",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let err = p.destroy(&RegionId::new("nyc1"), "999").await.unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)));
}

#[tokio::test]
async fn delete_204_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/v2/droplets/3164494"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    p.destroy(&RegionId::new("nyc1"), "3164494")
        .await
        .expect("destroy");
}

#[tokio::test]
async fn list_by_tag_follows_pagination_links() {
    let server = MockServer::start().await;
    let next = format!(
        "{}/v2/droplets?tag_name=jamstream&per_page=200&page=2",
        server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/v2/droplets"))
        .and(query_param("tag_name", "jamstream"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "droplets": [droplet_json(101, "nyc1", &["jamstream", "jamstream-session:aa"], Some("203.0.113.10"))],
            "links": { "pages": { "next": next, "last": next } },
            "meta": { "total": 2 },
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/droplets"))
        .and(query_param("tag_name", "jamstream"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "droplets": [droplet_json(102, "lon1", &["jamstream", "jamstream-session:bb"], Some("203.0.113.11"))],
            "links": {},
            "meta": { "total": 2 },
        })))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let all = p.list_tagged(None).await.expect("list");
    assert_eq!(all.len(), 2, "both pages must be fetched");
    assert_eq!(all[0].id, "101");
    assert_eq!(all[0].session_id(), Some("aa"));
    assert_eq!(all[1].id, "102");
    assert_eq!(all[1].session_id(), Some("bb"));
    assert_eq!(all[1].region.country, "GB");
}

#[tokio::test]
async fn list_for_one_session_queries_the_session_tag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/droplets"))
        .and(query_param("tag_name", "jamstream-session:sess1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "droplets": [droplet_json(7, "ams3", &["jamstream", "jamstream-session:sess1"], None)],
            "links": {},
        })))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let got = p.list_tagged(Some("sess1")).await.expect("list");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].session_id(), Some("sess1"));
}

#[tokio::test]
async fn bulk_destroy_by_tag_sends_the_tag_query() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/v2/droplets"))
        .and(query_param("tag_name", "jamstream-session:s9"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v2/droplets"))
        .and(query_param("tag_name", "jamstream"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    p.destroy_by_tag(Some("s9")).await.expect("bulk by session");
    p.destroy_by_tag(None).await.expect("bulk all");
}

fn sizes_body() -> serde_json::Value {
    json!({
        "sizes": [
            {
                "slug": "s-1vcpu-2gb",
                "memory": 2048,
                "vcpus": 1,
                "disk": 50,
                "transfer": 2.0,
                "price_monthly": 12.0,
                "price_hourly": 0.01786,
                "regions": ["nyc1", "fra1"],
                "available": true,
            },
            {
                "slug": "s-2vcpu-2gb",
                "memory": 2048,
                "vcpus": 2,
                "disk": 60,
                "transfer": 3.0,
                "price_monthly": 18.0,
                "price_hourly": 0.02679,
                "regions": ["nyc1"],
                "available": true,
            },
        ],
        "links": {},
        "meta": { "total": 2 },
    })
}

#[tokio::test]
async fn price_converts_exactly_and_caches_the_sizes_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/sizes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sizes_body()))
        .expect(1) // the whole point: one fetch across all price() calls
        .mount(&server)
        .await;

    let p = provider(&server);
    let nyc1 = RegionId::new("nyc1");

    let first = p.price(&nyc1).await.expect("price 1");
    assert_eq!(first.hourly_microusd, 26_790, "0.02679 -> 26790 microusd");
    assert_eq!(first.egress_microusd_per_gb, 10_000);
    assert_eq!(first.included_egress_gb, 3000);

    let second = p.price(&nyc1).await.expect("price 2");
    assert_eq!(second, first);

    // Other classes and regions reuse the same cached catalog.
    let small = p
        .price_for(&nyc1, InstanceClass::Small)
        .await
        .expect("small price");
    assert_eq!(small.hourly_microusd, 17_860);
    assert_eq!(small.included_egress_gb, 2000);

    // fra1 has the Small size but not the Standard one.
    let fra1 = RegionId::new("fra1");
    assert!(p.price_for(&fra1, InstanceClass::Small).await.is_ok());
    let err = p.price(&fra1).await.unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)));

    // Unknown regions never reach the API.
    let err = p.price(&RegionId::new("nope1")).await.unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)));

    server.verify().await;
}

#[tokio::test]
async fn transient_500_then_success_proves_send_retrying_in_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/droplets/42"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/droplets/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "droplet": droplet_json(42, "syd1", &["jamstream"], Some("203.0.113.42")),
        })))
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server);
    let inst = p
        .refresh(&RegionId::new("syd1"), "42")
        .await
        .expect("retried to success");
    assert_eq!(inst.id, "42");
}

#[tokio::test]
async fn unauthorized_maps_to_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/sizes"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "id": "Unauthorized",
            "message": "Unable to authenticate you",
        })))
        .expect(1) // auth failures must not retry
        .mount(&server)
        .await;

    let p = provider(&server);
    let err = p.price(&RegionId::new("nyc1")).await.unwrap_err();
    assert!(matches!(err, ProviderError::Auth(_)));
}

#[test]
fn tag_mapping_round_trips_for_realistic_session_ids() {
    for id in [
        "abc123",
        "deadbeefcafef00d",
        "sess-2026-07-25",
        "a",
        "with_underscore",
        "UPPER-and-lower-0123456789",
    ] {
        let (key, value) = session_tag(id);
        let do_tag = to_do_tag(&key, &value);
        assert_eq!(do_tag, format!("jamstream-session:{id}"));
        assert_eq!(do_tag, session_do_tag(id));
        // Simulate what list/refresh do with the API's tag strings.
        let tags: Vec<(String, String)> = [BARE_TAG.to_owned(), do_tag]
            .iter()
            .map(|t| from_do_tag(t))
            .collect();
        assert_eq!(session_id_from_tags(&tags), Some(id));
        let inst = Instance {
            provider: ProviderKind::DigitalOcean,
            region: Region {
                provider: ProviderKind::DigitalOcean,
                id: RegionId::new("nyc1"),
                display: "New York 1".to_owned(),
                country: "US".to_owned(),
            },
            id: "1".to_owned(),
            public_ip: None,
            tags,
        };
        assert_eq!(inst.session_id(), Some(id));
    }
}
