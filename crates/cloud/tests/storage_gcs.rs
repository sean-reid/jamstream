//! Wiremock-backed tests for the GCS object store: the media upload, the
//! resumable upload protocol end to end (initiate, 308 chunks, finalize,
//! cancel), the lifecycle patch, and the head/list/delete paths. Every
//! request must carry the bearer token from the shared GCP token source.

// Every test here is about GCP, which sits behind the `gcp` feature so
// jamstreamd can build without aws-lc. With the feature off this file is
// empty rather than broken.
#![cfg(feature = "gcp")]

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param, query_param_is_missing};
use wiremock::{Match, Mock, MockServer, Request, Respond, ResponseTemplate};

use jamstream_cloud::providers::gcp::TokenSource;
use jamstream_cloud::retention::Retention;
use jamstream_cloud::storage::GcsStore;
use jamstream_cloud::{
    BytesSource, ObjectStore, ProviderError, ProviderKind, Result, assert_object_store_contract,
    mix_key, session_prefix,
};

const TOKEN: &str = "ya29.test-token";
const BUCKET: &str = "my-jams";
const WAV: &str = "audio/wav";
/// The mix key with `/` percent-encoded, as it appears in a GCS object path.
const MIX_ENCODED: &str = "jamstream%2Frecordings%2Fs1%2Fmix.wav";

struct FakeToken;

#[async_trait]
impl TokenSource for FakeToken {
    async fn access_token(&self) -> Result<String> {
        Ok(TOKEN.to_owned())
    }
}

fn store(server: &MockServer) -> GcsStore {
    GcsStore::new(Arc::new(FakeToken))
        .with_base_url(server.uri())
        .with_part_size(8)
}

fn authorized() -> impl Match {
    header("authorization", format!("Bearer {TOKEN}").as_str())
}

fn object_json(name: &str, size: u64) -> Value {
    json!({
        "kind": "storage#object",
        "name": name,
        "bucket": BUCKET,
        "size": size.to_string(),
        "contentType": WAV,
        "etag": "CJ0=",
        "updated": "2026-07-25T10:00:00.000Z",
    })
}

// ---- Single-shot media upload ----

#[tokio::test]
async fn put_uses_the_media_upload_endpoint() {
    let server = MockServer::start().await;
    let key = mix_key("s1");
    Mock::given(method("POST"))
        .and(path(format!("/upload/storage/v1/b/{BUCKET}/o")))
        .and(query_param("uploadType", "media"))
        .and(query_param("name", key.as_str()))
        .and(authorized())
        .and(header("content-type", WAV))
        .respond_with(ResponseTemplate::new(200).set_body_json(object_json(&key, 14)))
        .expect(1)
        .mount(&server)
        .await;

    let meta = store(&server)
        .put(BUCKET, &key, WAV, b"riff-ish bytes")
        .await
        .unwrap();
    assert_eq!(meta.key, key);
    assert_eq!(meta.size, 14, "the size arrives as a JSON string");
    assert_eq!(meta.content_type.as_deref(), Some(WAV));
    assert_eq!(meta.etag.as_deref(), Some("CJ0="));
}

// ---- Resumable upload: the happy path ----

#[tokio::test]
async fn resumable_upload_initiates_streams_chunks_and_finalizes() {
    let server = MockServer::start().await;
    let key = mix_key("s1");
    let session_uri = format!("{}/resumable/session-1", server.uri());

    Mock::given(method("POST"))
        .and(path(format!("/upload/storage/v1/b/{BUCKET}/o")))
        .and(query_param("uploadType", "resumable"))
        .and(authorized())
        .and(header("x-upload-content-type", WAV))
        // The metadata body names the object and its type.
        .and(body_json(json!({ "name": key, "contentType": WAV })))
        .respond_with(ResponseTemplate::new(200).insert_header("location", session_uri.as_str()))
        .expect(1)
        .named("start resumable session")
        .mount(&server)
        .await;

    // Intermediate chunks: an open-ended range, answered with 308.
    for (start, end) in [(0u64, 7u64), (8, 15)] {
        Mock::given(method("PUT"))
            .and(path("/resumable/session-1"))
            .and(authorized())
            .and(header(
                "content-range",
                format!("bytes {start}-{end}/*").as_str(),
            ))
            .respond_with(
                ResponseTemplate::new(308)
                    .insert_header("range", format!("bytes=0-{end}").as_str()),
            )
            .expect(1)
            .named(format!("chunk {start}-{end}"))
            .mount(&server)
            .await;
    }
    // The final chunk declares the total, and the response is the object.
    Mock::given(method("PUT"))
        .and(path("/resumable/session-1"))
        .and(authorized())
        .and(header("content-range", "bytes 16-19/20"))
        .respond_with(ResponseTemplate::new(200).set_body_json(object_json(&key, 20)))
        .expect(1)
        .named("final chunk")
        .mount(&server)
        .await;

    let meta = store(&server)
        .put_stream(
            BUCKET,
            &key,
            WAV,
            &mut BytesSource::new((0..20u8).collect::<Vec<u8>>()),
        )
        .await
        .unwrap();
    assert_eq!(meta.size, 20);
    assert_eq!(meta.key, key);
    // There is no separate commit call and no cancel.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4, "one start plus three chunks");
    assert!(
        !requests
            .iter()
            .any(|r| r.method == wiremock::http::Method::DELETE),
        "a successful upload cancelled its own session"
    );
}

#[tokio::test]
async fn a_body_inside_one_chunk_uses_the_media_upload() {
    let server = MockServer::start().await;
    let key = mix_key("s1");
    Mock::given(method("POST"))
        .and(query_param("uploadType", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_json(object_json(&key, 8)))
        .expect(1)
        .mount(&server)
        .await;
    let meta = store(&server)
        .put_stream(
            BUCKET,
            &key,
            WAV,
            &mut BytesSource::new(b"12345678".to_vec()),
        )
        .await
        .unwrap();
    assert_eq!(meta.size, 8);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_missing_location_header_fails_before_any_bytes_are_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("uploadType", "resumable"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let err = store(&server)
        .put_stream(
            BUCKET,
            &mix_key("s1"),
            WAV,
            &mut BytesSource::new((0..20u8).collect::<Vec<u8>>()),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no Location header"), "{err}");
    // Nothing was uploaded, so there is nothing to cancel either.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

// ---- Resumable upload: cancellation ----

#[tokio::test]
async fn a_failed_chunk_cancels_the_session_and_leaves_nothing_behind() {
    let server = MockServer::start().await;
    let key = mix_key("s1");
    let session_uri = format!("{}/resumable/session-1", server.uri());

    Mock::given(method("POST"))
        .and(query_param("uploadType", "resumable"))
        .respond_with(ResponseTemplate::new(200).insert_header("location", session_uri.as_str()))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(header("content-range", "bytes 0-7/*"))
        .respond_with(ResponseTemplate::new(308))
        .expect(1)
        .mount(&server)
        .await;
    // The second chunk is rejected outright.
    Mock::given(method("PUT"))
        .and(header("content-range", "bytes 8-15/*"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad chunk"))
        .expect(1)
        .mount(&server)
        .await;
    // GCS answers a cancelled session with 499, which the store must accept
    // as success rather than treating it as a failure.
    Mock::given(method("DELETE"))
        .and(path("/resumable/session-1"))
        .and(authorized())
        .respond_with(ResponseTemplate::new(499))
        .expect(1)
        .named("cancel resumable session")
        .mount(&server)
        .await;

    let err = store(&server)
        .put_stream(
            BUCKET,
            &key,
            WAV,
            &mut BytesSource::new((0..40u8).collect::<Vec<u8>>()),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("bad chunk"), "{err}");

    let requests = server.received_requests().await.unwrap();
    let chunks = requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::PUT)
        .count();
    assert_eq!(chunks, 2, "the driver kept uploading after a failed chunk");
    let cancels = requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::DELETE)
        .count();
    assert_eq!(cancels, 1, "exactly one cancel");
}

#[tokio::test]
async fn a_session_that_never_finalizes_is_an_error_and_is_cancelled() {
    // 308 on the final chunk means GCS is still waiting for bytes, so the
    // object does not exist and reporting success would be a lie.
    let server = MockServer::start().await;
    let session_uri = format!("{}/resumable/session-1", server.uri());
    Mock::given(method("POST"))
        .and(query_param("uploadType", "resumable"))
        .respond_with(ResponseTemplate::new(200).insert_header("location", session_uri.as_str()))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(308))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(499))
        .expect(1)
        .mount(&server)
        .await;

    let err = store(&server)
        .put_stream(
            BUCKET,
            &mix_key("s1"),
            WAV,
            &mut BytesSource::new((0..20u8).collect::<Vec<u8>>()),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("still reports 308"), "{err}");
}

#[tokio::test]
async fn an_early_finalization_is_caught_and_cancelled() {
    // A 200 before the last chunk means GCS closed the object and the rest
    // of the recording would vanish silently.
    let server = MockServer::start().await;
    let key = mix_key("s1");
    let session_uri = format!("{}/resumable/session-1", server.uri());
    Mock::given(method("POST"))
        .and(query_param("uploadType", "resumable"))
        .respond_with(ResponseTemplate::new(200).insert_header("location", session_uri.as_str()))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(header("content-range", "bytes 0-7/*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(object_json(&key, 8)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(499))
        .expect(1)
        .mount(&server)
        .await;

    let err = store(&server)
        .put_stream(
            BUCKET,
            &key,
            WAV,
            &mut BytesSource::new((0..20u8).collect::<Vec<u8>>()),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("finalized the upload"), "{err}");
}

// ---- head / list / delete ----

#[tokio::test]
async fn head_reads_the_object_resource() {
    let server = MockServer::start().await;
    let key = mix_key("s1");
    Mock::given(method("GET"))
        .and(path(format!("/storage/v1/b/{BUCKET}/o/{MIX_ENCODED}")))
        .and(authorized())
        .respond_with(ResponseTemplate::new(200).set_body_json(object_json(&key, 1_382_400_044)))
        .expect(1)
        .mount(&server)
        .await;
    let meta = store(&server).head(BUCKET, &key).await.unwrap();
    assert_eq!(meta.size, 1_382_400_044);
    assert_eq!(
        meta.last_modified.as_deref(),
        Some("2026-07-25T10:00:00.000Z")
    );
}

#[tokio::test]
async fn head_of_a_missing_object_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": { "code": 404, "message": "No such object" }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let err = store(&server)
        .head(BUCKET, &mix_key("s1"))
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)), "{err:?}");
}

#[tokio::test]
async fn list_follows_the_page_token() {
    let server = MockServer::start().await;
    let prefix = session_prefix("s1");
    Mock::given(method("GET"))
        .and(path(format!("/storage/v1/b/{BUCKET}/o")))
        .and(query_param("prefix", prefix.as_str()))
        .and(query_param_is_missing("pageToken"))
        .and(authorized())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [object_json(&format!("{prefix}mix.wav"), 1_382_400_044u64)],
            "nextPageToken": "page2",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/storage/v1/b/{BUCKET}/o")))
        .and(query_param("pageToken", "page2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [object_json(&format!("{prefix}stems/bass.wav"), 691_200_044u64)],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let items = store(&server).list(BUCKET, &prefix).await.unwrap();
    assert_eq!(items.len(), 2, "pagination dropped a page: {items:?}");
    assert_eq!(items[0].key, format!("{prefix}mix.wav"));
    assert_eq!(items[1].key, format!("{prefix}stems/bass.wav"));
}

#[tokio::test]
async fn delete_is_idempotent_despite_gcs_returning_404() {
    let server = MockServer::start().await;
    let key = mix_key("s1");
    Mock::given(method("DELETE"))
        .and(path(format!("/storage/v1/b/{BUCKET}/o/{MIX_ENCODED}")))
        .and(authorized())
        .respond_with(ResponseTemplate::new(204))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // GCS 404s a second delete; the store normalizes that to success so
    // cleanup paths do not have to care.
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": { "code": 404, "message": "No such object" }
        })))
        .mount(&server)
        .await;
    let store = store(&server);
    store.delete(BUCKET, &key).await.unwrap();
    store.delete(BUCKET, &key).await.unwrap();
}

// ---- Lifecycle ----

#[tokio::test]
async fn lifecycle_patch_scopes_the_rule_to_the_prefix() {
    let server = MockServer::start().await;
    let prefix = session_prefix("s1");
    Mock::given(method("PATCH"))
        .and(path(format!("/storage/v1/b/{BUCKET}")))
        .and(authorized())
        .and(header("content-type", "application/json"))
        .and(body_json(json!({
            "lifecycle": {
                "rule": [{
                    "action": { "type": "Delete" },
                    "condition": { "age": 30, "matchesPrefix": [prefix] },
                }],
            },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "kind": "storage#bucket" })))
        .expect(1)
        .mount(&server)
        .await;

    let applied = store(&server)
        .set_retention(BUCKET, &prefix, Retention::Days30)
        .await
        .unwrap();
    assert!(applied.is_server_side());
    assert_eq!(applied.retention(), Retention::Days30);
    assert!(
        applied.describe().contains("gcp will delete"),
        "{}",
        applied.describe()
    );
}

#[tokio::test]
async fn keep_forever_clears_the_rule_list() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(body_json(json!({ "lifecycle": { "rule": [] } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("an empty rule list clears a previous expiration")
        .mount(&server)
        .await;
    let applied = store(&server)
        .set_retention(BUCKET, "jamstream/recordings/", Retention::KeepForever)
        .await
        .unwrap();
    assert!(applied.describe().contains("kept until you delete it"));
}

#[tokio::test]
async fn an_unauthorized_token_is_an_auth_error_and_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": { "code": 401, "message": "Invalid Credentials" }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let err = store(&server)
        .put(BUCKET, &mix_key("s1"), WAV, b"x")
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn the_store_reports_the_gcp_provider_kind() {
    let server = MockServer::start().await;
    assert_eq!(store(&server).kind(), ProviderKind::Gcp);
}

// ---- The full contract against a stateful fake GCS ----

#[tokio::test]
async fn gcs_store_passes_the_object_store_contract() {
    let server = MockServer::start().await;
    let base = server.uri();
    server
        .register(Mock::given(Anything).respond_with(FakeGcs::new(base.clone())))
        .await;
    let store = GcsStore::new(Arc::new(FakeToken))
        .with_base_url(base)
        // Above the contract's single-chunk cap of 1 KiB, so both the media
        // and resumable legs run.
        .with_part_size(2048);
    assert_object_store_contract(&store, BUCKET).await;
}

struct Anything;

impl Match for Anything {
    fn matches(&self, _request: &Request) -> bool {
        true
    }
}

/// A small stateful GCS: enough of the media upload, the resumable protocol,
/// object get/list/delete, and the bucket patch for the generic contract
/// suite to run against the real client.
struct FakeGcs {
    base_url: String,
    state: std::sync::Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    objects: std::collections::BTreeMap<String, Vec<u8>>,
    types: std::collections::BTreeMap<String, String>,
    /// session id -> (object name, bytes received so far).
    sessions: std::collections::BTreeMap<String, (String, Vec<u8>)>,
    next_session: u64,
}

impl FakeGcs {
    fn new(base_url: String) -> Self {
        FakeGcs {
            base_url,
            state: std::sync::Mutex::new(FakeState::default()),
        }
    }
}

impl Respond for FakeGcs {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        // Every request has to carry the bearer token.
        if request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            != Some(&format!("Bearer {TOKEN}"))
        {
            return ResponseTemplate::new(401);
        }
        let mut state = self.state.lock().unwrap();
        let path = request.url.path().to_owned();
        let query: std::collections::HashMap<String, String> = request
            .url
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let method = request.method.as_str();
        let object_prefix = format!("/storage/v1/b/{BUCKET}/o/");
        let upload_path = format!("/upload/storage/v1/b/{BUCKET}/o");

        let meta = |name: &str, body: &[u8], types: &std::collections::BTreeMap<String, String>| {
            json!({
                "kind": "storage#object",
                "name": name,
                "size": body.len().to_string(),
                "etag": format!("e-{}", body.len()),
                "contentType": types.get(name).cloned().unwrap_or_default(),
                "updated": "2026-07-25T10:00:00.000Z",
            })
        };

        // ---- Uploads ----
        if method == "POST" && path == upload_path {
            match query.get("uploadType").map(String::as_str) {
                Some("media") => {
                    let name = query.get("name").cloned().unwrap_or_default();
                    let content_type = request
                        .headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    state.types.insert(name.clone(), content_type);
                    state.objects.insert(name.clone(), request.body.clone());
                    let body = state.objects.get(&name).cloned().unwrap_or_default();
                    return ResponseTemplate::new(200).set_body_json(meta(
                        &name,
                        &body,
                        &state.types,
                    ));
                }
                Some("resumable") => {
                    let metadata: Value =
                        serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
                    let name = metadata["name"].as_str().unwrap_or_default().to_owned();
                    let content_type = metadata["contentType"].as_str().unwrap_or("").to_owned();
                    state.types.insert(name.clone(), content_type);
                    state.next_session += 1;
                    let id = format!("session-{}", state.next_session);
                    state.sessions.insert(id.clone(), (name, Vec::new()));
                    return ResponseTemplate::new(200).insert_header(
                        "location",
                        format!("{}/resumable/{id}", self.base_url).as_str(),
                    );
                }
                _ => return ResponseTemplate::new(400),
            }
        }

        // ---- Resumable chunks ----
        if let Some(id) = path.strip_prefix("/resumable/") {
            let id = id.to_owned();
            match method {
                "PUT" => {
                    let range = request
                        .headers
                        .get("content-range")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    let Some((name, buffer)) = state.sessions.get_mut(&id) else {
                        return ResponseTemplate::new(404);
                    };
                    // "bytes {start}-{end}/{total|*}"
                    let total = range.rsplit('/').next().unwrap_or("*").to_owned();
                    buffer.extend_from_slice(&request.body);
                    let received = buffer.len();
                    if total == "*" {
                        return ResponseTemplate::new(308)
                            .insert_header("range", format!("bytes=0-{}", received - 1).as_str());
                    }
                    // Final chunk: the declared total must match.
                    if total.parse::<usize>() != Ok(received) {
                        return ResponseTemplate::new(400);
                    }
                    let name = name.clone();
                    let body = buffer.clone();
                    state.sessions.remove(&id);
                    state.objects.insert(name.clone(), body.clone());
                    ResponseTemplate::new(200).set_body_json(meta(&name, &body, &state.types))
                }
                "DELETE" => {
                    state.sessions.remove(&id);
                    // GCS's documented answer to a cancelled session.
                    ResponseTemplate::new(499)
                }
                _ => ResponseTemplate::new(405),
            }
        // ---- Object level ----
        } else if let Some(encoded) = path.strip_prefix(&object_prefix) {
            let name = percent_decode(encoded);
            match method {
                // alt=media asks for the bytes; without it the object
                // resource itself is what GCS returns.
                "GET" if query.get("alt").map(String::as_str) == Some("media") => {
                    match state.objects.get(&name) {
                        Some(body) => ResponseTemplate::new(200)
                            .insert_header("content-length", body.len().to_string().as_str())
                            .insert_header(
                                "content-type",
                                state
                                    .types
                                    .get(&name)
                                    .map(String::as_str)
                                    .unwrap_or("application/octet-stream"),
                            )
                            .insert_header("etag", format!("\"e-{}\"", body.len()).as_str())
                            .set_body_bytes(body.clone()),
                        None => ResponseTemplate::new(404).set_body_json(json!({
                            "error": { "code": 404, "message": "No such object" }
                        })),
                    }
                }
                "GET" => match state.objects.get(&name) {
                    Some(body) => {
                        let body = body.clone();
                        ResponseTemplate::new(200).set_body_json(meta(&name, &body, &state.types))
                    }
                    None => ResponseTemplate::new(404).set_body_json(json!({
                        "error": { "code": 404, "message": "No such object" }
                    })),
                },
                "DELETE" => {
                    if state.objects.remove(&name).is_some() {
                        ResponseTemplate::new(204)
                    } else {
                        ResponseTemplate::new(404).set_body_json(json!({
                            "error": { "code": 404, "message": "No such object" }
                        }))
                    }
                }
                _ => ResponseTemplate::new(405),
            }
        } else if method == "GET" && path == format!("/storage/v1/b/{BUCKET}/o") {
            let prefix = query.get("prefix").cloned().unwrap_or_default();
            let items: Vec<Value> = state
                .objects
                .iter()
                .filter(|(k, _)| k.starts_with(&prefix))
                .map(|(k, body)| meta(k, body, &state.types))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "items": items }))
        } else if method == "PATCH" && path == format!("/storage/v1/b/{BUCKET}") {
            ResponseTemplate::new(200).set_body_json(json!({ "kind": "storage#bucket" }))
        } else {
            ResponseTemplate::new(501).set_body_string(format!("{method} {path}"))
        }
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
