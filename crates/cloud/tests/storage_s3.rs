//! Wiremock-backed tests for the S3 object store, covering both AWS S3 and
//! DigitalOcean Spaces: the whole multipart lifecycle including both abort
//! paths, the lifecycle-configuration call in both XML dialects, and the
//! head/list/delete paths. The store's `with_base_url` override routes
//! everything at one mock server in path-style, so the real signer, the real
//! request shapes, and the real XML parsers are all under test.

use jamstream_cloud::providers::aws::AwsProvider;
use jamstream_cloud::retention::{LifecycleDialect, Retention};
use jamstream_cloud::storage::{ChunkSink, S3Store};
use jamstream_cloud::{
    BytesSource, ObjectStore, Provider, ProviderError, ProviderKind, Result,
    assert_object_store_contract, mix_key, session_prefix,
};
use wiremock::matchers::{
    body_string_contains, header, header_exists, method, path, query_param, query_param_is_missing,
};
use wiremock::{Match, Mock, MockServer, Request, Respond, ResponseTemplate};

const ACCESS_KEY_ID: &str = "AKIDTEST";
const BUCKET: &str = "my-jams";
const WAV: &str = "audio/wav";

fn store(server: &MockServer) -> S3Store {
    S3Store::aws(
        "eu-west-1",
        ACCESS_KEY_ID.to_owned(),
        "test-secret-key".to_owned(),
    )
    .with_base_url(server.uri())
    // Tiny parts so a multipart lifecycle costs a few bytes, not 48 MiB.
    .with_part_size(8)
}

fn spaces_store(server: &MockServer) -> S3Store {
    S3Store::spaces("nyc3", "DO00TEST".to_owned(), "spaces-secret".to_owned())
        .with_base_url(server.uri())
        .with_part_size(8)
}

/// Matches a request carrying a well-formed SigV4 authorization for the S3
/// service in `region`, with the payload hash header S3 requires.
struct SignedForS3 {
    region: &'static str,
    access_key_id: &'static str,
}

impl Match for SignedForS3 {
    fn matches(&self, request: &Request) -> bool {
        let Some(auth) = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        auth.starts_with(&format!(
            "AWS4-HMAC-SHA256 Credential={}/",
            self.access_key_id
        ))
            && auth.contains(&format!("/{}/s3/aws4_request", self.region))
            // Every S3 request signs the payload hash, and it must be sent.
            && auth.contains("x-amz-content-sha256")
            && request.headers.get("x-amz-content-sha256").is_some()
            && request.headers.get("x-amz-date").is_some()
    }
}

fn signed() -> SignedForS3 {
    SignedForS3 {
        region: "eu-west-1",
        access_key_id: ACCESS_KEY_ID,
    }
}

fn object_path(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

// ---- Single-shot PUT ----

#[tokio::test]
async fn put_signs_and_sends_the_object() {
    let server = MockServer::start().await;
    let key = mix_key("s1");
    Mock::given(method("PUT"))
        .and(path(object_path(&key)))
        .and(signed())
        .and(header("content-type", WAV))
        .and(body_string_contains("riff-ish"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"abc123\""))
        .expect(1)
        .mount(&server)
        .await;

    let meta = store(&server)
        .put(BUCKET, &key, WAV, b"riff-ish bytes")
        .await
        .unwrap();
    assert_eq!(meta.key, key);
    assert_eq!(meta.size, 14);
    assert_eq!(
        meta.etag.as_deref(),
        Some("abc123"),
        "quotes must be stripped"
    );
}

// ---- Multipart: the happy path ----

/// Answers UploadPart with a per-part ETag so the complete body can be
/// checked against what S3 handed out.
struct PartEtag;

impl Respond for PartEtag {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let number = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "partNumber")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
        ResponseTemplate::new(200).insert_header("etag", format!("\"etag-{number}\"").as_str())
    }
}

#[tokio::test]
async fn multipart_upload_initiates_sends_parts_and_completes() {
    let server = MockServer::start().await;
    let key = mix_key("s1");

    Mock::given(method("POST"))
        .and(path(object_path(&key)))
        .and(query_param("uploads", ""))
        .and(signed())
        .and(header("content-type", WAV))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<InitiateMultipartUploadResult>
  <Bucket>my-jams</Bucket>
  <Key>jamstream/recordings/s1/mix.wav</Key>
  <UploadId>upload-42</UploadId>
</InitiateMultipartUploadResult>"#,
        ))
        .expect(1)
        .named("CreateMultipartUpload")
        .mount(&server)
        .await;

    for number in 1..=3u32 {
        Mock::given(method("PUT"))
            .and(path(object_path(&key)))
            .and(query_param("partNumber", number.to_string()))
            .and(query_param("uploadId", "upload-42"))
            .and(signed())
            .respond_with(PartEtag)
            .expect(1)
            .named(format!("UploadPart {number}"))
            .mount(&server)
            .await;
    }

    Mock::given(method("POST"))
        .and(path(object_path(&key)))
        .and(query_param("uploadId", "upload-42"))
        .and(query_param_is_missing("partNumber"))
        .and(signed())
        // Every part must be echoed back, in order, with its ETag.
        .and(body_string_contains(
            "<Part><PartNumber>1</PartNumber><ETag>&quot;etag-1&quot;</ETag></Part>\
             <Part><PartNumber>2</PartNumber><ETag>&quot;etag-2&quot;</ETag></Part>\
             <Part><PartNumber>3</PartNumber><ETag>&quot;etag-3&quot;</ETag></Part>",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<CompleteMultipartUploadResult>
  <Location>https://my-jams.s3.eu-west-1.amazonaws.com/jamstream/recordings/s1/mix.wav</Location>
  <Bucket>my-jams</Bucket>
  <Key>jamstream/recordings/s1/mix.wav</Key>
  <ETag>&quot;final-etag-3&quot;</ETag>
</CompleteMultipartUploadResult>"#,
        ))
        .expect(1)
        .named("CompleteMultipartUpload")
        .mount(&server)
        .await;

    // 8 + 8 + 4 bytes at a part size of 8.
    let body: Vec<u8> = (0..20u8).collect();
    let meta = store(&server)
        .put_stream(BUCKET, &key, WAV, &mut BytesSource::new(body))
        .await
        .unwrap();
    assert_eq!(meta.size, 20, "the completed object reports the full body");
    assert_eq!(meta.etag.as_deref(), Some("final-etag-3"));
    // No DELETE: a successful upload must not abort.
    assert!(
        !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.method == wiremock::http::Method::DELETE),
        "a successful multipart upload aborted itself"
    );
}

#[tokio::test]
async fn a_body_inside_one_part_never_initiates_a_multipart_upload() {
    let server = MockServer::start().await;
    let key = mix_key("s1");
    Mock::given(method("PUT"))
        .and(path(object_path(&key)))
        .and(query_param_is_missing("partNumber"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"single\""))
        .expect(1)
        .mount(&server)
        .await;
    // Anything else, in particular a ?uploads POST, is an error.
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

// ---- Multipart: abort on failure ----

#[tokio::test]
async fn a_failed_part_aborts_the_upload_and_leaves_nothing_behind() {
    let server = MockServer::start().await;
    let key = mix_key("s1");

    Mock::given(method("POST"))
        .and(path(object_path(&key)))
        .and(query_param("uploads", ""))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<InitiateMultipartUploadResult><UploadId>upload-42</UploadId></InitiateMultipartUploadResult>"),
        )
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(object_path(&key)))
        .and(query_param("partNumber", "1"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"etag-1\""))
        .expect(1)
        .mount(&server)
        .await;
    // Part 2 is rejected outright (400 is fatal, so no retry storm).
    Mock::given(method("PUT"))
        .and(path(object_path(&key)))
        .and(query_param("partNumber", "2"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string("<Error><Code>InvalidPart</Code><Message>nope</Message></Error>"),
        )
        .expect(1)
        .mount(&server)
        .await;
    // The abort is the point of this test.
    Mock::given(method("DELETE"))
        .and(path(object_path(&key)))
        .and(query_param("uploadId", "upload-42"))
        .and(signed())
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .named("AbortMultipartUpload")
        .mount(&server)
        .await;

    let body: Vec<u8> = (0..30u8).collect();
    let err = store(&server)
        .put_stream(BUCKET, &key, WAV, &mut BytesSource::new(body))
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Other(_)), "{err:?}");
    assert!(err.to_string().contains("InvalidPart"), "{err}");

    // Exactly one abort, and nothing sent after the failure: no part 3, no
    // completion. Verified by the mock expectations plus this count.
    let requests = server.received_requests().await.unwrap();
    let parts = requests
        .iter()
        .filter(|r| r.url.query_pairs().any(|(k, _)| k == "partNumber"))
        .count();
    assert_eq!(parts, 2, "the driver kept uploading after a failed part");
    let aborts = requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::DELETE)
        .count();
    assert_eq!(aborts, 1);
    let completes = requests
        .iter()
        .filter(|r| {
            r.method == wiremock::http::Method::POST
                && r.url
                    .query_pairs()
                    .any(|(k, v)| k == "uploadId" && v == "upload-42")
        })
        .count();
    assert_eq!(completes, 0, "a failed upload must never be completed");
}

#[tokio::test]
async fn an_error_inside_a_200_completion_response_still_aborts() {
    // S3 streams CompleteMultipartUpload and can report failure in the body
    // of a 200. Trusting the status code here would report a recording that
    // does not exist.
    let server = MockServer::start().await;
    let key = mix_key("s1");

    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<InitiateMultipartUploadResult><UploadId>u9</UploadId></InitiateMultipartUploadResult>"),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"e\""))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("uploadId", "u9"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<Error><Code>InternalError</Code><Message>We encountered an internal error</Message></Error>"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", "u9"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .named("abort after a failed completion")
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
    assert!(err.to_string().contains("InternalError"), "{err}");
}

// ---- ObjectSink over the real store ----

/// The sink is what the recorder feeds; here it drives the real signer and
/// the real multipart state machine, not a fake of them.
#[tokio::test]
async fn sink_streams_chunks_through_a_real_multipart_upload() {
    use jamstream_cloud::ObjectSink;
    use std::sync::Arc;

    let server = MockServer::start().await;
    let key = mix_key("s1");
    Mock::given(method("POST"))
        .and(path(object_path(&key)))
        .and(query_param("uploads", ""))
        .and(signed())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<InitiateMultipartUploadResult><UploadId>up-sink</UploadId></InitiateMultipartUploadResult>"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(object_path(&key)))
        .and(query_param("uploadId", "up-sink"))
        .and(signed())
        .respond_with(PartEtag)
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(object_path(&key)))
        .and(query_param("uploadId", "up-sink"))
        .and(query_param_is_missing("partNumber"))
        .and(signed())
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<CompleteMultipartUploadResult><ETag>&quot;sink-etag&quot;</ETag></CompleteMultipartUploadResult>",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let dir = std::env::temp_dir().join(format!("jamstream-s3-sink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut sink =
        ObjectSink::open_with_marker_dir(Arc::new(store(&server)), BUCKET, &key, WAV, &dir);
    // 20 bytes in ragged chunks at a part size of 8: 8 + 8 + 4.
    sink.write((0..5u8).collect()).await.unwrap();
    sink.write((5..19u8).collect()).await.unwrap();
    sink.write(vec![19]).await.unwrap();
    let meta = sink.finish().await.unwrap();
    assert_eq!(meta.size, 20);
    assert_eq!(meta.etag.as_deref(), Some("sink-etag"));
    assert!(
        !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.method == wiremock::http::Method::DELETE),
        "a successful sink upload aborted itself"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Abandoning the sink must reach S3 as AbortMultipartUpload, and never as
/// CompleteMultipartUpload over a truncated body.
#[tokio::test]
async fn sink_abort_sends_a_real_abort_and_never_completes() {
    use jamstream_cloud::ObjectSink;
    use std::sync::Arc;

    let server = MockServer::start().await;
    let key = mix_key("s1");
    Mock::given(method("POST"))
        .and(path(object_path(&key)))
        .and(query_param("uploads", ""))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<InitiateMultipartUploadResult><UploadId>up-gone</UploadId></InitiateMultipartUploadResult>"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(object_path(&key)))
        .and(query_param("uploadId", "up-gone"))
        .respond_with(PartEtag)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(object_path(&key)))
        .and(query_param("uploadId", "up-gone"))
        .and(signed())
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .named("AbortMultipartUpload from the sink")
        .mount(&server)
        .await;

    let dir = std::env::temp_dir().join(format!("jamstream-s3-sink-abort-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut sink =
        ObjectSink::open_with_marker_dir(Arc::new(store(&server)), BUCKET, &key, WAV, &dir);
    sink.write((0..30u8).collect()).await.unwrap();
    sink.abort().await;

    let requests = server.received_requests().await.unwrap();
    let completes = requests
        .iter()
        .filter(|r| {
            r.method == wiremock::http::Method::POST
                && r.url.query_pairs().any(|(k, _)| k == "uploadId")
        })
        .count();
    assert_eq!(completes, 0, "an aborted sink must never complete");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_part_response_without_an_etag_fails_and_aborts() {
    // Without the ETag the completion body cannot be built, so continuing
    // would only waste the rest of the upload.
    let server = MockServer::start().await;
    let key = mix_key("s1");
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<InitiateMultipartUploadResult><UploadId>u1</UploadId></InitiateMultipartUploadResult>"),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(query_param("partNumber", "1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", "u1"))
        .respond_with(ResponseTemplate::new(204))
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
    assert!(err.to_string().contains("no ETag"), "{err}");
}

#[tokio::test]
async fn a_failed_abort_surfaces_the_original_error() {
    // The upload failure is the actionable one; the lifecycle rule reclaims
    // the parts if the abort cannot be delivered.
    let server = MockServer::start().await;
    let key = mix_key("s1");
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<InitiateMultipartUploadResult><UploadId>u1</UploadId></InitiateMultipartUploadResult>"),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(query_param("partNumber", "1"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string(
                "<Error><Code>EntityTooSmall</Code><Message>tiny</Message></Error>",
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(403))
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
    assert!(
        err.to_string().contains("EntityTooSmall"),
        "the abort failure masked the real error: {err}"
    );
}

// ---- head / list / delete ----

#[tokio::test]
async fn head_reads_size_etag_and_type_from_headers() {
    let server = MockServer::start().await;
    let key = mix_key("s1");
    Mock::given(method("HEAD"))
        .and(path(object_path(&key)))
        .and(signed())
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-length", "1382400044")
                .insert_header("content-type", WAV)
                .insert_header("etag", "\"abc\"")
                .insert_header("last-modified", "Sat, 25 Jul 2026 10:00:00 GMT"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let meta = store(&server).head(BUCKET, &key).await.unwrap();
    assert_eq!(meta.size, 1_382_400_044);
    assert_eq!(meta.content_type.as_deref(), Some(WAV));
    assert_eq!(meta.etag.as_deref(), Some("abc"));
    assert_eq!(
        meta.last_modified.as_deref(),
        Some("Sat, 25 Jul 2026 10:00:00 GMT")
    );
}

#[tokio::test]
async fn head_of_a_missing_object_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let err = store(&server).head(BUCKET, "nope.wav").await.unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)), "{err:?}");
}

// ---- get ----

/// Collects a downloaded body in arrival order, which is what the assertions
/// compare against; hyper may coalesce chunks, so the bytes are the only thing
/// worth asserting on.
#[derive(Default)]
struct Collector {
    bytes: Vec<u8>,
}

#[async_trait::async_trait]
impl ChunkSink for Collector {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }
}

#[tokio::test]
async fn get_streams_a_signed_object_to_the_sink() {
    let server = MockServer::start().await;
    let key = mix_key("s1");
    // Deterministic bytes, regenerated in the assertion rather than compared
    // against anything the mock or the store handed back.
    let body: Vec<u8> = (0..600usize).map(|i| (i % 251) as u8).collect();
    Mock::given(method("GET"))
        .and(path(object_path(&key)))
        .and(signed())
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", WAV)
                .insert_header("etag", "\"take-etag\"")
                .insert_header("last-modified", "Sat, 25 Jul 2026 10:00:00 GMT")
                .set_body_bytes(body),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut sink = Collector::default();
    let meta = store(&server).get(BUCKET, &key, &mut sink).await.unwrap();
    assert_eq!(
        sink.bytes,
        (0..600usize).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
        "the sink did not receive the body in order"
    );
    assert_eq!(meta.key, key);
    assert_eq!(meta.size, 600, "size must be what was delivered");
    assert_eq!(
        meta.etag.as_deref(),
        Some("take-etag"),
        "quotes must be stripped"
    );
    assert_eq!(meta.content_type.as_deref(), Some(WAV));
    assert_eq!(
        meta.last_modified.as_deref(),
        Some("Sat, 25 Jul 2026 10:00:00 GMT")
    );
}

#[tokio::test]
async fn get_of_a_missing_object_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(signed())
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_string("<Error><Code>NoSuchKey</Code><Message>gone</Message></Error>"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut sink = Collector::default();
    let err = store(&server)
        .get(BUCKET, &mix_key("s1"), &mut sink)
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::NotFound(_)), "{err:?}");
    assert!(sink.bytes.is_empty(), "a 404 body reached the sink");
}

// A short body under an inflated content-length has no test here on purpose:
// hyper refuses to serve one. Inserting a content-length that disagrees with
// the payload panics inside hyper's encoder ("payload claims content-length of
// 100, custom content-length header claims 4096"), so wiremock cannot lie and
// the case is unreachable over HTTP. The truncation guard is unit-tested
// against `check_delivered` in storage.rs instead, which is what `drain_body`
// calls once the body is drained.

#[tokio::test]
async fn list_follows_the_continuation_token() {
    let server = MockServer::start().await;
    let prefix = session_prefix("s1");

    Mock::given(method("GET"))
        .and(path(format!("/{BUCKET}")))
        .and(query_param("list-type", "2"))
        .and(query_param("prefix", prefix.as_str()))
        .and(query_param_is_missing("continuation-token"))
        .and(signed())
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<ListBucketResult>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>page2</NextContinuationToken>
  <Contents><Key>{prefix}mix.wav</Key><Size>1382400044</Size><ETag>&quot;a&quot;</ETag></Contents>
</ListBucketResult>"#
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/{BUCKET}")))
        .and(query_param("continuation-token", "page2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<ListBucketResult>
  <IsTruncated>false</IsTruncated>
  <Contents><Key>{prefix}stems/bass.wav</Key><Size>691200044</Size></Contents>
</ListBucketResult>"#
        )))
        .expect(1)
        .mount(&server)
        .await;

    let items = store(&server).list(BUCKET, &prefix).await.unwrap();
    assert_eq!(items.len(), 2, "pagination dropped a page: {items:?}");
    // Sorted by key.
    assert_eq!(items[0].key, format!("{prefix}mix.wav"));
    assert_eq!(items[0].size, 1_382_400_044);
    assert_eq!(items[1].key, format!("{prefix}stems/bass.wav"));
}

#[tokio::test]
async fn delete_is_idempotent() {
    let server = MockServer::start().await;
    let key = mix_key("s1");
    // S3 answers 204 whether or not the key was there.
    Mock::given(method("DELETE"))
        .and(path(object_path(&key)))
        .and(signed())
        .respond_with(ResponseTemplate::new(204))
        .expect(2)
        .mount(&server)
        .await;
    let store = store(&server);
    store.delete(BUCKET, &key).await.unwrap();
    store.delete(BUCKET, &key).await.unwrap();
}

// ---- Lifecycle configuration ----

#[tokio::test]
async fn aws_lifecycle_put_sends_the_filter_form_with_a_content_md5() {
    let server = MockServer::start().await;
    let prefix = session_prefix("s1");
    Mock::given(method("PUT"))
        .and(path(format!("/{BUCKET}")))
        .and(query_param("lifecycle", ""))
        .and(signed())
        .and(header("content-type", "application/xml"))
        // PutBucketLifecycleConfiguration wants a body integrity header.
        .and(header_exists("content-md5"))
        .and(body_string_contains(
            "<ID>jamstream-recording-retention</ID>",
        ))
        .and(body_string_contains(format!(
            "<Filter><Prefix>{prefix}</Prefix></Filter>"
        )))
        .and(body_string_contains(
            "<Expiration><Days>30</Days></Expiration>",
        ))
        .and(body_string_contains(
            "<AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation>",
        ))
        .respond_with(ResponseTemplate::new(200))
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
        applied.describe().contains("delete this recording 30 days"),
        "{}",
        applied.describe()
    );
}

#[tokio::test]
async fn the_content_md5_matches_the_body_that_was_sent() {
    // A wrong Content-MD5 is a 400 from real S3, so the header has to be the
    // digest of this exact document.
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(query_param("lifecycle", ""))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    store(&server)
        .set_retention(BUCKET, "jamstream/recordings/", Retention::Days90)
        .await
        .unwrap();

    let request = &server.received_requests().await.unwrap()[0];
    let sent_md5 = request
        .headers
        .get("content-md5")
        .and_then(|v| v.to_str().ok())
        .expect("content-md5")
        .to_owned();
    // Recomputed independently of the store: base64 of the raw MD5 digest.
    let expected = md5_base64(&request.body);
    assert_eq!(sent_md5, expected, "Content-MD5 does not match the body");
}

#[tokio::test]
async fn keep_forever_omits_the_expiration_but_keeps_the_cleanup_rule() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(query_param("lifecycle", ""))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    store(&server)
        .set_retention(BUCKET, "jamstream/recordings/", Retention::KeepForever)
        .await
        .unwrap();
    let body =
        String::from_utf8(server.received_requests().await.unwrap()[0].body.clone()).unwrap();
    assert!(!body.contains("<Expiration>"), "{body}");
    assert!(body.contains("<AbortIncompleteMultipartUpload>"), "{body}");
}

#[tokio::test]
async fn spaces_lifecycle_put_uses_the_bare_prefix_dialect() {
    let server = MockServer::start().await;
    let prefix = session_prefix("s1");
    Mock::given(method("PUT"))
        .and(path(format!("/{BUCKET}")))
        .and(query_param("lifecycle", ""))
        // Signed for the Spaces region slug with the Spaces access key.
        .and(SignedForS3 {
            region: "nyc3",
            access_key_id: "DO00TEST",
        })
        .and(body_string_contains(format!("<Prefix>{prefix}</Prefix>")))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let applied = spaces_store(&server)
        .set_retention(BUCKET, &prefix, Retention::Days7)
        .await
        .unwrap();
    assert!(applied.is_server_side());
    assert!(
        applied.describe().contains("digitalocean will delete"),
        "{}",
        applied.describe()
    );
    let body =
        String::from_utf8(server.received_requests().await.unwrap()[0].body.clone()).unwrap();
    assert!(
        !body.contains("<Filter>"),
        "Spaces rejects the Filter form: {body}"
    );
}

#[tokio::test]
async fn spaces_speaks_the_same_object_api_as_s3() {
    // The only differences are the endpoint, the credentials, and the
    // lifecycle dialect; the object calls are byte-identical in shape.
    let server = MockServer::start().await;
    let key = mix_key("s1");
    Mock::given(method("PUT"))
        .and(path(object_path(&key)))
        .and(SignedForS3 {
            region: "nyc3",
            access_key_id: "DO00TEST",
        })
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"spaces-etag\""))
        .expect(1)
        .mount(&server)
        .await;
    let meta = spaces_store(&server)
        .put(BUCKET, &key, WAV, b"bytes")
        .await
        .unwrap();
    assert_eq!(meta.etag.as_deref(), Some("spaces-etag"));
}

// ---- Auth and signing ----

#[tokio::test]
async fn a_rejected_signature_is_an_auth_error_and_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(403).set_body_string(
            "<Error><Code>SignatureDoesNotMatch</Code><Message>bad</Message></Error>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let err = store(&server)
        .put(BUCKET, "k.wav", WAV, b"x")
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn a_transient_500_is_retried_by_the_shared_http_path() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"ok\""))
        .expect(1)
        .mount(&server)
        .await;
    let meta = store(&server)
        .put(BUCKET, "k.wav", WAV, b"x")
        .await
        .unwrap();
    assert_eq!(meta.etag.as_deref(), Some("ok"));
}

#[tokio::test]
async fn the_ec2_provider_still_signs_ec2_requests_after_the_s3_extension() {
    // Guard on the shared signer: the EC2 caller's signed header set is
    // unchanged by S3's extra requirements.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(SignedForEc2)
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<DescribeInstancesResponse><reservationSet/></DescribeInstancesResponse>",
        ))
        .expect(1..)
        .mount(&server)
        .await;
    let provider = AwsProvider::new(ACCESS_KEY_ID.to_owned(), "test-secret-key".to_owned())
        .with_base_url(server.uri());
    let instances = provider.list_tagged(Some("s1")).await.unwrap();
    assert!(instances.is_empty());
    assert_eq!(provider.kind(), ProviderKind::Aws);
}

struct SignedForEc2;

impl Match for SignedForEc2 {
    fn matches(&self, request: &Request) -> bool {
        let Some(auth) = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        // The EC2 signed header set has no x-amz-content-sha256 in it.
        auth.contains("/ec2/aws4_request")
            && auth.contains("SignedHeaders=content-type;host;x-amz-date,")
            && request.headers.get("x-amz-content-sha256").is_none()
    }
}

// ---- The full contract against a stateful fake S3 ----

#[tokio::test]
async fn s3_store_passes_the_object_store_contract() {
    let server = MockServer::start().await;
    server
        .register(Mock::given(FakeS3Matcher).respond_with(FakeS3::default()))
        .await;
    // A part size above the contract's single-part cap of 1 KiB, so both the
    // single-PUT and the multipart legs are exercised.
    let store = S3Store::aws(
        "eu-west-1",
        ACCESS_KEY_ID.to_owned(),
        "test-secret-key".to_owned(),
    )
    .with_base_url(server.uri())
    .with_part_size(2048);
    assert_object_store_contract(&store, BUCKET).await;
}

#[tokio::test]
async fn spaces_store_passes_the_object_store_contract() {
    let server = MockServer::start().await;
    server
        .register(Mock::given(FakeS3Matcher).respond_with(FakeS3::default()))
        .await;
    let store = S3Store::spaces("nyc3", "DO00TEST".to_owned(), "secret".to_owned())
        .with_base_url(server.uri())
        .with_part_size(2048)
        .with_lifecycle_dialect(LifecycleDialect::SpacesV1);
    assert_object_store_contract(&store, BUCKET).await;
}

/// True for any well-formed SigV4 S3 signature, whatever the region or key.
fn signed_for_any_s3(request: &Request) -> bool {
    let Some(auth) = request
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    auth.starts_with("AWS4-HMAC-SHA256 Credential=")
        && auth.contains("/s3/aws4_request")
        && auth.contains("x-amz-content-sha256")
        && request.headers.get("x-amz-content-sha256").is_some()
        && request.headers.get("x-amz-date").is_some()
}

/// Matches everything, so the fake sees every request.
struct FakeS3Matcher;

impl Match for FakeS3Matcher {
    fn matches(&self, _request: &Request) -> bool {
        true
    }
}

/// A small stateful S3: enough of PutObject, the multipart lifecycle,
/// HeadObject, ListObjectsV2, DeleteObject, and PutBucketLifecycle for the
/// generic contract suite to run against the real signer and the real XML
/// parsers.
#[derive(Default)]
struct FakeS3 {
    state: std::sync::Mutex<FakeState>,
}

/// One in-flight multipart upload in the fake: the target key and the parts
/// received so far.
type FakeUpload = (String, Vec<(u32, Vec<u8>)>);

#[derive(Default)]
struct FakeState {
    /// key -> body.
    objects: std::collections::BTreeMap<String, Vec<u8>>,
    /// key -> content type.
    types: std::collections::BTreeMap<String, String>,
    /// upload id -> in-flight upload.
    uploads: std::collections::BTreeMap<String, FakeUpload>,
    next_upload: u64,
}

impl Respond for FakeS3 {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        // Every request must be signed for s3, or the fake is not testing
        // the signer. The region and key differ between the AWS and Spaces
        // runs, so this checks the shape rather than the identity.
        if !signed_for_any_s3(request) {
            return ResponseTemplate::new(403).set_body_string(
                "<Error><Code>AccessDenied</Code><Message>unsigned</Message></Error>",
            );
        }
        let mut state = self.state.lock().unwrap();
        let path = request.url.path().to_owned();
        let bucket_root = format!("/{BUCKET}");
        // Path-style: everything after "/{bucket}/" is the key.
        let key = path
            .strip_prefix(&format!("{bucket_root}/"))
            .map(percent_decode);
        let query: std::collections::HashMap<String, String> = request
            .url
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let method = request.method.as_str();

        match (method, key.as_deref()) {
            // ---- Bucket level ----
            ("PUT", None) if query.contains_key("lifecycle") => {
                if request.headers.get("content-md5").is_none() {
                    return ResponseTemplate::new(400).set_body_string(
                        "<Error><Code>MissingContentMD5</Code><Message>md5</Message></Error>",
                    );
                }
                ResponseTemplate::new(200)
            }
            ("GET", None) if query.get("list-type").map(String::as_str) == Some("2") => {
                let prefix = query.get("prefix").cloned().unwrap_or_default();
                let mut xml = String::from("<ListBucketResult><IsTruncated>false</IsTruncated>");
                for (k, body) in state.objects.iter().filter(|(k, _)| k.starts_with(&prefix)) {
                    xml.push_str(&format!(
                        "<Contents><Key>{}</Key><Size>{}</Size><ETag>&quot;e-{}&quot;</ETag>\
                         <LastModified>2026-07-25T10:00:00.000Z</LastModified></Contents>",
                        xml_escape(k),
                        body.len(),
                        body.len()
                    ));
                }
                xml.push_str("</ListBucketResult>");
                ResponseTemplate::new(200).set_body_string(xml)
            }
            // ---- Multipart ----
            ("POST", Some(key)) if query.contains_key("uploads") => {
                state.next_upload += 1;
                let id = format!("fake-upload-{}", state.next_upload);
                let content_type = request
                    .headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/octet-stream")
                    .to_owned();
                state.types.insert(key.to_owned(), content_type);
                state
                    .uploads
                    .insert(id.clone(), (key.to_owned(), Vec::new()));
                ResponseTemplate::new(200).set_body_string(format!(
                    "<InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"
                ))
            }
            ("PUT", Some(_)) if query.contains_key("partNumber") => {
                let id = query.get("uploadId").cloned().unwrap_or_default();
                let number: u32 = query
                    .get("partNumber")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                match state.uploads.get_mut(&id) {
                    Some((_, parts)) => {
                        parts.push((number, request.body.clone()));
                        ResponseTemplate::new(200)
                            .insert_header("etag", format!("\"part-{number}\"").as_str())
                    }
                    None => ResponseTemplate::new(404).set_body_string(
                        "<Error><Code>NoSuchUpload</Code><Message>gone</Message></Error>",
                    ),
                }
            }
            ("POST", Some(_)) if query.contains_key("uploadId") => {
                let id = query.get("uploadId").cloned().unwrap_or_default();
                let Some((key, mut parts)) = state.uploads.remove(&id) else {
                    return ResponseTemplate::new(404).set_body_string(
                        "<Error><Code>NoSuchUpload</Code><Message>gone</Message></Error>",
                    );
                };
                // The completion body must echo every part's ETag.
                let body = String::from_utf8_lossy(&request.body).to_string();
                for (number, _) in &parts {
                    if !body.contains(&format!("<PartNumber>{number}</PartNumber>")) {
                        return ResponseTemplate::new(400).set_body_string(
                            "<Error><Code>InvalidPart</Code><Message>missing part</Message></Error>",
                        );
                    }
                }
                parts.sort_by_key(|(n, _)| *n);
                let assembled: Vec<u8> = parts.into_iter().flat_map(|(_, b)| b).collect();
                let len = assembled.len();
                state.objects.insert(key.clone(), assembled);
                ResponseTemplate::new(200).set_body_string(format!(
                    "<CompleteMultipartUploadResult><Key>{}</Key><ETag>&quot;final-{len}&quot;</ETag></CompleteMultipartUploadResult>",
                    xml_escape(&key)
                ))
            }
            ("DELETE", Some(_)) if query.contains_key("uploadId") => {
                let id = query.get("uploadId").cloned().unwrap_or_default();
                state.uploads.remove(&id);
                ResponseTemplate::new(204)
            }
            // ---- Object level ----
            ("PUT", Some(key)) => {
                let content_type = request
                    .headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/octet-stream")
                    .to_owned();
                state.types.insert(key.to_owned(), content_type);
                let len = request.body.len();
                state.objects.insert(key.to_owned(), request.body.clone());
                ResponseTemplate::new(200).insert_header("etag", format!("\"e-{len}\"").as_str())
            }
            ("HEAD", Some(key)) => match state.objects.get(key) {
                Some(body) => ResponseTemplate::new(200)
                    .insert_header("content-length", body.len().to_string().as_str())
                    .insert_header(
                        "content-type",
                        state
                            .types
                            .get(key)
                            .map(String::as_str)
                            .unwrap_or("application/octet-stream"),
                    )
                    .insert_header("etag", format!("\"e-{}\"", body.len()).as_str()),
                None => ResponseTemplate::new(404),
            },
            // No sub-resource in the query, so this is GetObject rather than
            // one of the multipart calls or the bucket listing above.
            ("GET", Some(key)) if query.is_empty() => match state.objects.get(key) {
                Some(body) => ResponseTemplate::new(200)
                    .insert_header("content-length", body.len().to_string().as_str())
                    .insert_header(
                        "content-type",
                        state
                            .types
                            .get(key)
                            .map(String::as_str)
                            .unwrap_or("application/octet-stream"),
                    )
                    .insert_header("etag", format!("\"e-{}\"", body.len()).as_str())
                    .set_body_bytes(body.clone()),
                None => ResponseTemplate::new(404).set_body_string(
                    "<Error><Code>NoSuchKey</Code><Message>gone</Message></Error>",
                ),
            },
            ("DELETE", Some(key)) => {
                // 204 whether or not it was there.
                state.objects.remove(key);
                ResponseTemplate::new(204)
            }
            _ => ResponseTemplate::new(501).set_body_string(format!(
                "<Error><Code>NotImplemented</Code><Message>{method} {path}</Message></Error>"
            )),
        }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Minimal percent-decoding for the fake's path handling.
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

/// Independent MD5 for the Content-MD5 cross-check, so the assertion does not
/// lean on the implementation it is checking. RFC 1321.
fn md5_base64(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32)
        .collect();
    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            m[i] = u32::from_le_bytes(word.try_into().unwrap());
        }
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        for (s, v) in state.iter_mut().zip([a, b, c, d]) {
            *s = s.wrapping_add(v);
        }
    }
    let mut digest = [0u8; 16];
    for (i, word) in state.iter().enumerate() {
        digest[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    data_encoding::BASE64.encode(&digest)
}
