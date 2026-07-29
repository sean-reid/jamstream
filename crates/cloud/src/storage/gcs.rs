//! Google Cloud Storage object store: the JSON API plus the resumable
//! upload protocol, authenticated with the same [`TokenSource`] the Compute
//! provider uses, so one GCP identity covers both the VM and the recording.
//!
//! # The resumable protocol, mapped onto multipart
//!
//! GCS has no notion of numbered parts. A large upload is a *session*:
//!
//! 1. `POST /upload/storage/v1/b/{bucket}/o?uploadType=resumable` with the
//!    object metadata returns a session URI in the `Location` header.
//! 2. Each chunk is a `PUT` to that URI with a
//!    `Content-Range: bytes {start}-{end}/*` header. GCS answers an
//!    intermediate chunk with **308 Resume Incomplete**, which is neither
//!    success nor failure; [`crate::http::send_retrying_accepting`] exists so
//!    that response can travel the one shared HTTP path like everything else.
//! 3. The final chunk sends the now-known total instead of `*`
//!    (`bytes {start}-{end}/{total}`) and GCS answers `200` with the finished
//!    object's JSON. There is no separate commit call, so
//!    [`MultipartBackend::finish`] sends nothing and returns the metadata the
//!    last chunk already produced.
//! 4. `DELETE` on the session URI cancels the upload and discards every byte
//!    received. GCS answers `499`, which is also in the accept list.
//!
//! Two GCS-specific rules the shared driver already satisfies: every
//! non-final chunk must be a multiple of 256 KiB (see
//! [`crate::storage::PART_SIZE_MULTIPLE`]), and chunks must arrive in order,
//! which the driver does by construction.
//!
//! Abandoned sessions are garbage-collected by GCS after a week even if the
//! `DELETE` never arrives, which is why the GCS lifecycle document has no
//! incomplete-upload rule where S3's does.

use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::http;
use crate::provider::{ProviderError, Result};
use crate::providers::gcp::TokenSource;
use crate::retention::{
    GCS_MAX_LIFECYCLE_RULES, Retention, RetentionEnforcement, at_capacity_note,
    merge_gcs_lifecycle, rule_id, unreadable_note,
};
use crate::storage::{
    ChunkSink, DEFAULT_PART_SIZE, JSON_CONTENT_TYPE, MultipartBackend, ObjectMeta, ObjectStore,
    Part, PartSource, clamp_part_size, drain_body, drive_upload,
};
use crate::types::ProviderKind;

const DEFAULT_BASE_URL: &str = "https://storage.googleapis.com";
/// 308 Resume Incomplete: every intermediate chunk of a resumable upload.
const RESUME_INCOMPLETE: u16 = 308;
/// 499 Client Closed Request: GCS's answer to a cancelled upload session.
const UPLOAD_CANCELLED: u16 = 499;

/// A GCS bucket, addressed through the JSON API.
pub struct GcsStore {
    token: Arc<dyn TokenSource>,
    base_url: String,
    part_size: usize,
    http: reqwest::Client,
    /// Used only by `get`, whose response body is a recording and cannot
    /// finish inside the API client's 30-second deadline.
    streaming: reqwest::Client,
}

impl fmt::Debug for GcsStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcsStore")
            .field("base_url", &self.base_url)
            .field("part_size", &self.part_size)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl GcsStore {
    /// Shares a token source with [`crate::providers::gcp::GcpProvider`], so
    /// the recording upload authenticates as the same identity that launched
    /// the VM.
    pub fn new(token: Arc<dyn TokenSource>) -> Self {
        GcsStore {
            token,
            base_url: DEFAULT_BASE_URL.to_owned(),
            part_size: DEFAULT_PART_SIZE,
            http: http::client(),
            streaming: http::streaming_client(),
        }
    }

    /// Credentials from the environment, by the same rules as
    /// [`crate::providers::gcp::GcpProvider::from_env`].
    pub fn from_env() -> Result<Self> {
        Ok(Self::new(
            crate::providers::gcp::GcpProvider::from_env()?.token_source(),
        ))
    }

    /// Overrides the API endpoint (tests point this at a mock server).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Overrides the resumable chunk size, raised to a multiple of the 256 KiB
    /// GCS requires by [`clamp_part_size`]. Real uploads should keep the
    /// default.
    pub fn with_part_size(mut self, bytes: usize) -> Self {
        self.part_size = clamp_part_size(bytes);
        self
    }

    fn object_url(&self, bucket: &str, key: &str) -> String {
        format!(
            "{}/storage/v1/b/{}/o/{}",
            self.base_url,
            encode(bucket),
            encode(key)
        )
    }

    fn bucket_url(&self, bucket: &str) -> String {
        format!("{}/storage/v1/b/{}", self.base_url, encode(bucket))
    }

    /// The bucket's metadata as far as its lifecycle rules, None when this
    /// identity may not read them.
    ///
    /// The patch that writes a rule replaces the whole rule list, so a
    /// blind write would delete rules the host set themselves. Reading is
    /// `storage.buckets.get`, which an identity scoped to writing objects need
    /// not have.
    async fn lifecycle_rules(&self, bucket: &str) -> Result<Option<serde_json::Value>> {
        let url = self.bucket_url(bucket);
        let token = self.token().await?;
        let resp = match http::send_retrying(|| {
            self.http
                .get(&url)
                .bearer_auth(&token)
                .query(&[("fields", "lifecycle")])
        })
        .await
        {
            Ok(resp) => resp,
            Err(ProviderError::Auth(err)) => {
                tracing::warn!(
                    bucket,
                    error = %err,
                    "cannot read the bucket's lifecycle rules, so no retention rule was written"
                );
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        // `fields=lifecycle` on a bucket with no rules is `{}`. A body that is
        // not JSON is an error rather than an empty rule list: treating it as
        // empty would write a patch that deletes the host's own rules.
        resp.json()
            .await
            .map(Some)
            .map_err(|e| ProviderError::Other(format!("reading {bucket}'s lifecycle rules: {e}")))
    }

    fn upload_url(&self, bucket: &str) -> String {
        format!("{}/upload/storage/v1/b/{}/o", self.base_url, encode(bucket))
    }

    async fn token(&self) -> Result<String> {
        self.token.access_token().await
    }

    async fn parse_object(resp: reqwest::Response, key: &str) -> Result<ObjectMeta> {
        let raw: GcsObject = resp
            .json()
            .await
            .map_err(|e| ProviderError::Other(format!("gcs object response parse: {e}")))?;
        Ok(raw.into_meta(key))
    }
}

/// The slice of the GCS object resource this crate needs. `size` arrives as
/// a decimal string because the JSON API renders int64 that way.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcsObject {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    updated: Option<String>,
}

impl GcsObject {
    fn into_meta(self, fallback_key: &str) -> ObjectMeta {
        ObjectMeta {
            key: self.name.unwrap_or_else(|| fallback_key.to_owned()),
            size: self
                .size
                .as_deref()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0),
            etag: self.etag,
            content_type: self.content_type,
            last_modified: self.updated,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcsObjectList {
    #[serde(default)]
    items: Vec<GcsObject>,
    #[serde(default)]
    next_page_token: Option<String>,
}

/// An open resumable upload.
pub struct GcsSession {
    /// The session URI GCS handed back; it already encodes bucket, object,
    /// and an upload id, which is why the part calls ignore bucket and key.
    session_uri: String,
    /// Metadata from the final chunk's response, which is where a GCS
    /// resumable upload reports the finished object.
    completed: Mutex<Option<ObjectMeta>>,
}

impl GcsSession {
    pub fn session_uri(&self) -> &str {
        &self.session_uri
    }
}

#[async_trait]
impl MultipartBackend for GcsStore {
    type Session = GcsSession;

    async fn put_single(
        &self,
        bucket: &str,
        key: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<ObjectMeta> {
        let url = self.upload_url(bucket);
        let token = self.token().await?;
        let resp = http::send_retrying(|| {
            self.http
                .post(&url)
                .bearer_auth(&token)
                .query(&[("uploadType", "media"), ("name", key)])
                .header("content-type", content_type)
                .body(body.to_vec())
        })
        .await?;
        Self::parse_object(resp, key).await
    }

    async fn begin(&self, bucket: &str, key: &str, content_type: &str) -> Result<GcsSession> {
        let url = self.upload_url(bucket);
        let token = self.token().await?;
        let metadata = json!({ "name": key, "contentType": content_type });
        let resp = http::send_retrying(|| {
            self.http
                .post(&url)
                .bearer_auth(&token)
                .query(&[("uploadType", "resumable")])
                // Tells GCS the content type of the bytes to come; the
                // metadata body carries it too, and they must agree.
                .header("x-upload-content-type", content_type)
                .json(&metadata)
        })
        .await?;
        let session_uri = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| {
                ProviderError::Other(
                    "gcs resumable upload start returned no Location header, so there is no \
                     session to upload into"
                        .to_owned(),
                )
            })?;
        Ok(GcsSession {
            session_uri,
            completed: Mutex::new(None),
        })
    }

    async fn send_part(
        &self,
        _bucket: &str,
        key: &str,
        session: &GcsSession,
        part: Part<'_>,
    ) -> Result<()> {
        let len = part.body.len() as u64;
        // GCS ranges are inclusive. A final zero-length chunk cannot occur:
        // the driver single-shots an empty object instead of opening a
        // session.
        let last_byte = part.offset + len.saturating_sub(1);
        let total = if part.last {
            (part.offset + len).to_string()
        } else {
            "*".to_owned()
        };
        let content_range = format!("bytes {}-{last_byte}/{total}", part.offset);
        let token = self.token().await?;
        let resp = http::send_retrying_accepting(&[RESUME_INCOMPLETE], || {
            self.http
                .put(&session.session_uri)
                .bearer_auth(&token)
                .header("content-range", &content_range)
                .body(part.body.to_vec())
        })
        .await?;

        let status = resp.status().as_u16();
        if part.last {
            if status == RESUME_INCOMPLETE {
                return Err(ProviderError::Transient(format!(
                    "gcs did not finalize the upload of {key}: it still reports 308 after the \
                     final chunk"
                )));
            }
            let meta = Self::parse_object(resp, key).await?;
            *session.completed.lock().expect("gcs session lock") = Some(meta);
        } else if status != RESUME_INCOMPLETE {
            // A 200 before the last chunk means GCS considers the object
            // finished and the rest of the recording would be silently
            // dropped.
            return Err(ProviderError::Other(format!(
                "gcs finalized the upload of {key} early: chunk {} got HTTP {status} instead of 308",
                part.number
            )));
        }
        Ok(())
    }

    async fn finish(&self, _bucket: &str, key: &str, session: &GcsSession) -> Result<ObjectMeta> {
        // Nothing to send: the final chunk was the commit.
        session
            .completed
            .lock()
            .expect("gcs session lock")
            .clone()
            .ok_or_else(|| {
                ProviderError::Other(format!(
                    "gcs upload of {key} never reported a finished object"
                ))
            })
    }

    async fn abort(&self, _bucket: &str, _key: &str, session: &GcsSession) -> Result<()> {
        let token = self.token().await?;
        http::send_retrying_accepting(&[UPLOAD_CANCELLED], || {
            self.http
                .delete(&session.session_uri)
                .bearer_auth(&token)
                // Cancelling wants an explicit empty body.
                .header("content-length", "0")
        })
        .await?;
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for GcsStore {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Gcp
    }

    fn part_size(&self) -> usize {
        self.part_size
    }

    async fn put(
        &self,
        bucket: &str,
        key: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<ObjectMeta> {
        MultipartBackend::put_single(self, bucket, key, content_type, body).await
    }

    async fn put_stream(
        &self,
        bucket: &str,
        key: &str,
        content_type: &str,
        source: &mut (dyn PartSource + Send),
    ) -> Result<ObjectMeta> {
        drive_upload(self, bucket, key, content_type, source, self.part_size).await
    }

    async fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta> {
        let url = self.object_url(bucket, key);
        let token = self.token().await?;
        let resp = http::send_retrying(|| self.http.get(&url).bearer_auth(&token)).await?;
        Self::parse_object(resp, key).await
    }

    async fn get(
        &self,
        bucket: &str,
        key: &str,
        sink: &mut (dyn ChunkSink + Send),
    ) -> Result<ObjectMeta> {
        // alt=media is what turns the object resource URL into the bytes;
        // without it GCS answers with the JSON metadata head reads.
        let url = self.object_url(bucket, key);
        let token = self.token().await?;
        let resp = http::send_retrying(|| {
            self.streaming
                .get(&url)
                .bearer_auth(&token)
                .query(&[("alt", "media")])
        })
        .await?;
        let mut meta = drain_body(resp, key, sink).await?;
        // A media download reports the ETag as an HTTP header, quoted, where
        // the JSON API reports it bare.
        meta.etag = meta
            .etag
            .map(|e| e.trim().trim_matches('"').to_owned())
            .filter(|e| !e.is_empty());
        Ok(meta)
    }

    async fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let url = format!("{}/o", self.bucket_url(bucket));
        let token = self.token().await?;
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let resp = http::send_retrying(|| {
                let mut req = self
                    .http
                    .get(&url)
                    .bearer_auth(&token)
                    .query(&[("prefix", prefix)]);
                if let Some(t) = &page_token {
                    req = req.query(&[("pageToken", t.as_str())]);
                }
                req
            })
            .await?;
            let list: GcsObjectList = resp
                .json()
                .await
                .map_err(|e| ProviderError::Other(format!("gcs list response parse: {e}")))?;
            out.extend(list.items.into_iter().map(|o| o.into_meta("")));
            page_token = list.next_page_token.filter(|t| !t.is_empty());
            if page_token.is_none() {
                break;
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        let url = self.object_url(bucket, key);
        let token = self.token().await?;
        match http::send_retrying(|| self.http.delete(&url).bearer_auth(&token)).await {
            Ok(_) => Ok(()),
            // GCS 404s a missing object where S3 returns 204. The trait
            // promises idempotent deletes, so normalize here rather than
            // making every cleanup path handle both.
            Err(ProviderError::NotFound(_)) => Ok(()),
            Err(err) => Err(err),
        }
    }

    async fn set_retention(
        &self,
        bucket: &str,
        prefix: &str,
        retention: Retention,
    ) -> Result<RetentionEnforcement> {
        let url = self.bucket_url(bucket);
        // Read first: the patch replaces the bucket's whole rule list, so the
        // rules already there have to be carried across. See crate::retention.
        let Some(existing) = self.lifecycle_rules(bucket).await? else {
            return Ok(RetentionEnforcement::Manual {
                retention,
                note: unreadable_note(retention, "storage.buckets.get"),
            });
        };
        let Some(patch) =
            merge_gcs_lifecycle(&existing, prefix, retention, GCS_MAX_LIFECYCLE_RULES)
        else {
            return Ok(RetentionEnforcement::Manual {
                retention,
                note: at_capacity_note(retention, GCS_MAX_LIFECYCLE_RULES),
            });
        };
        let token = self.token().await?;
        http::send_retrying(|| {
            self.http
                .patch(&url)
                .bearer_auth(&token)
                .header("content-type", JSON_CONTENT_TYPE)
                .json(&patch)
        })
        .await?;
        Ok(RetentionEnforcement::ServerSide {
            provider: ProviderKind::Gcp,
            retention,
            rule_id: rule_id(prefix),
            rule: patch.to_string(),
        })
    }
}

/// Percent-encodes one path component of a GCS URL. Object names go in a
/// single path segment, so `/` has to be escaped too: an unescaped key would
/// address a different resource entirely.
fn encode(component: &str) -> String {
    let mut out = String::with_capacity(component.len());
    for b in component.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeToken;

    #[async_trait]
    impl TokenSource for FakeToken {
        async fn access_token(&self) -> Result<String> {
            Ok("ya29.fake".to_owned())
        }
    }

    fn store() -> GcsStore {
        GcsStore::new(Arc::new(FakeToken))
    }

    #[test]
    fn object_names_are_fully_escaped_into_one_path_segment() {
        // A slash inside an object name must not become a path separator.
        assert_eq!(
            store().object_url("my-jams", "jamstream/recordings/s1/mix.wav"),
            "https://storage.googleapis.com/storage/v1/b/my-jams/o/\
             jamstream%2Frecordings%2Fs1%2Fmix.wav"
        );
        assert_eq!(encode("a b&c"), "a%20b%26c");
        assert_eq!(encode("plain-._~"), "plain-._~");
    }

    #[test]
    fn urls_follow_the_base_override() {
        let s = store().with_base_url("http://127.0.0.1:9/");
        assert_eq!(s.bucket_url("b"), "http://127.0.0.1:9/storage/v1/b/b");
        assert_eq!(
            s.upload_url("b"),
            "http://127.0.0.1:9/upload/storage/v1/b/b/o"
        );
    }

    #[test]
    fn object_json_parses_the_string_encoded_size() {
        let raw: GcsObject = serde_json::from_str(
            r#"{"name":"a/b.wav","size":"1382400044","etag":"CJ0=",
                "contentType":"audio/wav","updated":"2026-07-25T10:00:00.000Z"}"#,
        )
        .unwrap();
        let meta = raw.into_meta("fallback");
        assert_eq!(meta.key, "a/b.wav");
        assert_eq!(meta.size, 1_382_400_044);
        assert_eq!(meta.content_type.as_deref(), Some("audio/wav"));
        assert_eq!(meta.etag.as_deref(), Some("CJ0="));
        assert_eq!(
            meta.last_modified.as_deref(),
            Some("2026-07-25T10:00:00.000Z")
        );
    }

    #[test]
    fn a_response_without_a_name_falls_back_to_the_requested_key() {
        let raw: GcsObject = serde_json::from_str("{}").unwrap();
        let meta = raw.into_meta("k.wav");
        assert_eq!(meta.key, "k.wav");
        assert_eq!(meta.size, 0);
    }

    #[test]
    fn debug_never_reveals_the_token() {
        let rendered = format!("{:?}", store());
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("ya29"));
    }

    #[test]
    fn default_part_size_is_a_valid_gcs_chunk() {
        assert_eq!(store().part_size() % crate::storage::PART_SIZE_MULTIPLE, 0);
    }
}
