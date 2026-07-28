//! The credential check both launch surfaces run, against a real S3 client and
//! a fake bucket.
//!
//! The far end is a wiremock server, so the signing, the request shapes and the
//! error text are the shipped code and only the network is fake. The mock knows
//! nothing about `jamstream_cli`, so the assertions are about what the bucket
//! actually received: one PUT under the session's own prefix, then the DELETE
//! that takes it away again.

use jamstream_cli::host::{probe_bucket, probe_prefix};
use jamstream_cloud::cloudinit::{RecordingStorage, StorageCredential};
use jamstream_cloud::storage::S3Store;
use jamstream_cloud::{ProviderKind, Retention, session_prefix};
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BUCKET: &str = "my-jams";
const SESSION: &str = "deadbeefcafef00ddeadbeefcafef00d";
const SECRET: &str = "test-secret-key";

fn store(server: &MockServer) -> S3Store {
    S3Store::aws("eu-west-1", "AKIDTEST".to_owned(), SECRET.to_owned()).with_base_url(server.uri())
}

fn probe_path() -> String {
    format!("/{BUCKET}/{}.jamstream-probe", session_prefix(SESSION))
}

#[tokio::test]
async fn a_passing_check_writes_one_probe_and_deletes_it() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(probe_path()))
        // Signed for real: the check proves the key, so an unsigned request
        // would prove nothing.
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"abc123\""))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(probe_path()))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let prefix = session_prefix(SESSION);
    probe_prefix(&store(&server), BUCKET, "eu-west-1", &prefix)
        .await
        .expect("a bucket that accepts the write passes the check");

    // Nothing else was touched: a check that listed or read would need
    // permissions the launch key is deliberately not given.
    let seen = server.received_requests().await.expect("recorded requests");
    assert_eq!(seen.len(), 2, "a check is one write and one delete");
}

#[tokio::test]
async fn a_bucket_that_refuses_the_write_reports_the_reason_and_the_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(probe_path()))
        .respond_with(ResponseTemplate::new(403).set_body_string(
            "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let prefix = session_prefix(SESSION);
    let err = probe_prefix(&store(&server), BUCKET, "eu-west-1", &prefix)
        .await
        .expect_err("a key that cannot write must fail the check")
        .to_string();
    assert!(err.contains(BUCKET), "the bucket must be named: {err}");
    assert!(err.contains("eu-west-1"), "the region must be named: {err}");
    assert!(
        err.contains(&prefix),
        "the prefix a key has to write must be named: {err}"
    );
    // The provider's own words, not ours.
    assert!(err.contains("403"), "the reason must be verbatim: {err}");
    // A refusal must not leak the key that was refused.
    assert!(!err.contains(SECRET), "the secret reached an error: {err}");

    // Nothing was deleted: there is no probe object to remove.
    let seen = server.received_requests().await.expect("recorded requests");
    assert!(
        seen.iter()
            .all(|r| r.method != wiremock::http::Method::DELETE),
        "a failed write must not be followed by a delete"
    );
}

/// The app's Check calls [`probe_bucket`], which builds its client from a whole
/// storage config and cannot be pointed at a mock server. What it adds over the
/// probe above is that one call, so this asserts the part a mock cannot: a
/// provider with no bucket service has no client to build, and the refusal
/// carries no key.
#[tokio::test]
async fn a_config_with_no_bucket_service_fails_the_check_without_a_secret_in_the_reason() {
    let storage = RecordingStorage {
        provider: ProviderKind::Local,
        bucket: BUCKET.to_owned(),
        region: "local".to_owned(),
        retention: Retention::Days30,
        credential: StorageCredential::KeyPair {
            access_key_id: "AKIDTEST".to_owned(),
            secret_access_key: SECRET.to_owned(),
        },
        stems: false,
    };
    let err = probe_bucket(&storage, SESSION)
        .await
        .expect_err("this computer's disk is not a bucket")
        .to_string();
    assert!(!err.contains(SECRET), "the secret reached an error: {err}");
    // And the config it was refused for keeps the secret out of Debug too,
    // which is where a launch failure would print one.
    let debug = format!("{storage:?}");
    assert!(!debug.contains(SECRET), "Debug leaked the secret: {debug}");
}
