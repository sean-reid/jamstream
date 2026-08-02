//! S3 object store, covering both AWS S3 and DigitalOcean Spaces.
//!
//! Spaces is S3-compatible, so this is one implementation with a different
//! endpoint, different credentials, and one different lifecycle dialect (see
//! [`crate::retention::LifecycleDialect`]). Nothing else forks.
//!
//! # DigitalOcean Spaces uses different credentials, and that matters
//!
//! The DigitalOcean *API token* that launches droplets (`DIGITALOCEAN_TOKEN`,
//! used by [`crate::providers::digitalocean::DigitalOceanProvider`]) cannot
//! talk to Spaces at all. Spaces is an S3-compatible service signed with
//! SigV4, so it takes a separate **Spaces access key** pair, generated in a
//! different part of the control panel.
//!
//! That is a product consequence, not an implementation detail: on the provider
//! JamStream recommends for being "one token in one screen", turning on
//! recording is the one feature that sends a host back for a second credential.
//! So recording stays off by default, a missing Spaces key is an error naming
//! the two variables rather than a 403 from a signer, and the key is only
//! needed on the host's machine.
//!
//! AWS has the same shape for a different reason: the recording credential
//! wants `s3:PutObject`, `s3:DeleteObject` (the launch probe) and
//! `s3:AbortMultipartUpload` on one prefix, plus `s3:GetLifecycleConfiguration`
//! and `s3:PutLifecycleConfiguration` on the bucket (the retention rule is
//! merged into the bucket's existing rules, so it is read before it is
//! written), which is not the EC2 policy.
//! It must not *be* the EC2 policy either, and nothing reads
//! `AWS_ACCESS_KEY_ID` for it: this key is written into the session machine's
//! user data. See `jamstream_cli::storage`.
//! GCS is the only one where the same identity naturally does both.
//!
//! # Addressing
//!
//! Production uses virtual-hosted-style URLs (`{bucket}.s3.{region}.amazonaws.com`,
//! `{bucket}.{region}.digitaloceanspaces.com`), which is the form both clouds
//! document and the only one AWS promises for new buckets.
//! [`S3Store::with_base_url`] switches to path-style against one host, which
//! is what lets a wiremock server stand in for S3 without inventing DNS.

use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use data_encoding::BASE64;
use reqwest::{Method, Response};

use crate::http;
use crate::provider::{ProviderError, Result};
use crate::providers::aws::{amz_date_now, aws_encode, sigv4, take_tag, xml_unescape, xml_value};
use crate::retention::{
    LifecycleDialect, Retention, RetentionEnforcement, S3_MAX_LIFECYCLE_RULES, at_capacity_note,
    merge_s3_lifecycle, rule_id, unreadable_note,
};
use crate::storage::{
    ChunkSink, DEFAULT_PART_SIZE, MultipartBackend, ObjectMeta, ObjectStore, Part, PartSource,
    clamp_part_size, drain_body, drive_upload,
};
use crate::types::ProviderKind;

const XML_CONTENT_TYPE: &str = "application/xml";

/// How object URLs are built.
#[derive(Debug, Clone)]
enum Endpoint {
    /// `https://{bucket}.s3.{region}.amazonaws.com/{key}`.
    Aws,
    /// `https://{bucket}.{region}.digitaloceanspaces.com/{key}`.
    Spaces,
    /// `https://storage.googleapis.com/{bucket}/{key}`, Cloud Storage's
    /// S3-compatible interop endpoint. Path style, and the SigV4 region is
    /// always `auto`: interop signing ignores the bucket's real location.
    GcsInterop,
    /// Path-style against one base URL: `{base}/{bucket}/{key}`. Tests only.
    Override(String),
}

/// An S3 or S3-compatible object store.
pub struct S3Store {
    access_key_id: String,
    secret_access_key: String,
    /// SigV4 signing region. For Spaces this is the Spaces region slug
    /// (`nyc3`), which is what DigitalOcean expects, not `us-east-1`.
    region: String,
    endpoint: Endpoint,
    dialect: LifecycleDialect,
    kind: ProviderKind,
    part_size: usize,
    http: reqwest::Client,
    /// Used only by `get`, whose response body is a recording and cannot
    /// finish inside the API client's 30-second deadline.
    streaming: reqwest::Client,
}

/// The secret must never reach a log or an error message.
impl fmt::Debug for S3Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Store")
            .field("kind", &self.kind)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("part_size", &self.part_size)
            .finish()
    }
}

impl S3Store {
    /// AWS S3 in `region`, signed with an AWS access key pair.
    pub fn aws(
        region: impl Into<String>,
        access_key_id: String,
        secret_access_key: String,
    ) -> Self {
        S3Store {
            access_key_id,
            secret_access_key,
            region: region.into(),
            endpoint: Endpoint::Aws,
            dialect: LifecycleDialect::S3v2,
            kind: ProviderKind::Aws,
            part_size: DEFAULT_PART_SIZE,
            http: http::client(),
            streaming: http::streaming_client(),
        }
    }

    /// Google Cloud Storage through its S3-compatible interop endpoint,
    /// signed with an HMAC key pair rather than a service account.
    ///
    /// This is what lets the session server record to GCS without RSA. The
    /// only thing that needed asymmetric crypto was signing a service
    /// account JWT, which would put the whole GCP provider into jamstreamd;
    /// an HMAC key needs nothing the SigV4 path does not already have.
    ///
    /// The signing region is `auto`, which is what Cloud Storage expects
    /// from interop clients: the bucket's real location is not part of the
    /// signature.
    pub fn gcs_interop(access_key_id: String, secret: String) -> Self {
        S3Store {
            access_key_id,
            secret_access_key: secret,
            region: "auto".to_owned(),
            endpoint: Endpoint::GcsInterop,
            dialect: LifecycleDialect::S3v2,
            kind: ProviderKind::Gcp,
            part_size: DEFAULT_PART_SIZE,
            http: http::client(),
            streaming: http::streaming_client(),
        }
    }

    /// DigitalOcean Spaces in `region` (a Spaces region slug such as
    /// `nyc3`), signed with a **Spaces access key pair**. See the module
    /// docs: this is not the DigitalOcean API token.
    pub fn spaces(region: impl Into<String>, access_key_id: String, secret: String) -> Self {
        S3Store {
            access_key_id,
            secret_access_key: secret,
            region: region.into(),
            endpoint: Endpoint::Spaces,
            dialect: LifecycleDialect::SpacesV1,
            kind: ProviderKind::DigitalOcean,
            part_size: DEFAULT_PART_SIZE,
            http: http::client(),
            streaming: http::streaming_client(),
        }
    }

    /// Routes every request at one base URL in path-style, for tests.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.endpoint = Endpoint::Override(url.into().trim_end_matches('/').to_owned());
        self
    }

    /// Overrides the multipart part size, raised to something S3 accepts by
    /// [`clamp_part_size`]. Real uploads should keep the default; a test that
    /// wants the cheapest possible multipart upload asks for
    /// [`crate::storage::MIN_PART_SIZE`] and gets it.
    pub fn with_part_size(mut self, bytes: usize) -> Self {
        self.part_size = clamp_part_size(bytes);
        self
    }

    /// Forces the lifecycle XML dialect, for an S3-compatible endpoint that
    /// is neither AWS nor Spaces.
    pub fn with_lifecycle_dialect(mut self, dialect: LifecycleDialect) -> Self {
        self.dialect = dialect;
        self
    }

    /// Request URL, `host` header value, and canonical URI for one target.
    /// `key` of None addresses the bucket itself.
    fn address(&self, bucket: &str, key: Option<&str>) -> Result<(String, String, String)> {
        if bucket.is_empty() {
            return Err(ProviderError::Other("bucket name is empty".to_owned()));
        }
        check_authority_part("bucket name", bucket)?;
        // The signing region is interpolated into the host beside the bucket,
        // so it gets the same treatment.
        check_authority_part("region", &self.region)?;
        if key == Some("") {
            return Err(ProviderError::Other("object key is empty".to_owned()));
        }
        let (host, path) = match &self.endpoint {
            Endpoint::Aws => (
                format!("{bucket}.s3.{}.amazonaws.com", self.region),
                key.map_or_else(|| "/".to_owned(), |k| format!("/{}", encode_key(k))),
            ),
            Endpoint::Spaces => (
                format!("{bucket}.{}.digitaloceanspaces.com", self.region),
                key.map_or_else(|| "/".to_owned(), |k| format!("/{}", encode_key(k))),
            ),
            Endpoint::GcsInterop => (
                "storage.googleapis.com".to_owned(),
                match key {
                    Some(k) => format!("/{bucket}/{}", encode_key(k)),
                    None => format!("/{bucket}"),
                },
            ),
            Endpoint::Override(base) => {
                let parsed = reqwest::Url::parse(base)
                    .map_err(|e| ProviderError::Other(format!("bad s3 base url {base}: {e}")))?;
                let host = match (parsed.host_str(), parsed.port()) {
                    (Some(h), Some(p)) => format!("{h}:{p}"),
                    (Some(h), None) => h.to_owned(),
                    (None, _) => {
                        return Err(ProviderError::Other(format!(
                            "s3 base url {base} has no host"
                        )));
                    }
                };
                let path = match key {
                    Some(k) => format!("/{bucket}/{}", encode_key(k)),
                    None => format!("/{bucket}"),
                };
                (host, path)
            }
        };
        let url = match &self.endpoint {
            Endpoint::Override(base) => format!("{base}{path}"),
            _ => format!("https://{host}{path}"),
        };
        Ok((url, host, path))
    }

    /// Signs and sends one S3 request through the shared retrying HTTP path.
    ///
    /// The signature is recomputed per attempt so `x-amz-date` never goes
    /// stale across backoff, which means the whole header set is assembled
    /// inside the builder closure. Every header signed here is also sent,
    /// except `host`, which reqwest emits from the URL.
    async fn send(&self, req: S3Request<'_>) -> Result<Response> {
        self.send_with(&self.http, req).await
    }

    /// [`S3Store::send`] on a chosen client, so the download path can use the
    /// one without a whole-request deadline while keeping the same signer,
    /// retries and backoff.
    async fn send_with(&self, client: &reqwest::Client, req: S3Request<'_>) -> Result<Response> {
        let (url, host, canonical_uri) = self.address(req.bucket, req.key)?;
        let query = canonical_query(&req.query);
        let full_url = if query.is_empty() {
            url
        } else {
            format!("{url}?{query}")
        };
        // Bodyless calls (HEAD, GET, DELETE) all hash to the same well-known
        // digest, so there is no point recomputing it.
        let payload_sha256 = if req.body.is_empty() {
            sigv4::EMPTY_PAYLOAD_SHA256.to_owned()
        } else {
            sigv4::hex_sha256(&req.body)
        };
        let method = Method::from_bytes(req.method.as_bytes())
            .map_err(|e| ProviderError::Other(format!("bad http method {}: {e}", req.method)))?;

        // Everything signed except the per-attempt date, lowercase, so the
        // whole set can be sorted into SigV4's ascending order.
        let mut base: Vec<(String, String)> = vec![
            ("host".to_owned(), host),
            ("x-amz-content-sha256".to_owned(), payload_sha256.clone()),
        ];
        if let Some(ct) = req.content_type {
            base.push(("content-type".to_owned(), ct.to_owned()));
        }
        for (name, value) in &req.extra_headers {
            base.push(((*name).to_owned(), value.clone()));
        }

        let sends_body = matches!(req.method, "PUT" | "POST");
        // A body on a method that does not send one is signed and then dropped,
        // which S3 answers with SignatureDoesNotMatch and no hint as to why.
        debug_assert!(
            sends_body || req.body.is_empty(),
            "{} carries a {}-byte body that would be signed and not sent",
            req.method,
            req.body.len()
        );
        let build = || {
            let amz_date = amz_date_now();
            let mut headers = base.clone();
            headers.push(("x-amz-date".to_owned(), amz_date.clone()));
            headers.sort();
            // SigV4 signs the header names in ascending order, lowercase, once
            // each. A duplicate or a capital here produces a signature over a
            // canonical request the far end cannot reproduce.
            debug_assert!(
                headers.windows(2).all(|w| w[0].0 < w[1].0),
                "the signed header set is not sorted and unique: {headers:?}"
            );
            debug_assert!(
                headers
                    .iter()
                    .all(|(name, _)| *name == name.to_ascii_lowercase()),
                "the signed header set is not lowercase: {headers:?}"
            );
            let refs: Vec<(&str, &str)> = headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let authorization = sigv4::authorization_for(&sigv4::SignedRequest {
                access_key_id: &self.access_key_id,
                secret_access_key: &self.secret_access_key,
                region: &self.region,
                service: "s3",
                method: req.method,
                canonical_uri: &canonical_uri,
                query: &query,
                amz_date: &amz_date,
                headers: &refs,
                payload_sha256: &payload_sha256,
            });
            let mut rb = client.request(method.clone(), &full_url);
            for (name, value) in &headers {
                // reqwest emits Host from the URL; everything else signed
                // has to be sent explicitly.
                if name != "host" {
                    rb = rb.header(name.as_str(), value.as_str());
                }
            }
            rb = rb.header("authorization", authorization);
            // An explicit empty body keeps Content-Length: 0 on the wire,
            // which S3 requires of every PUT and POST.
            if sends_body {
                rb = rb.body(req.body.clone());
            }
            rb
        };

        http::send_retrying(build).await
    }

    /// The bucket's current lifecycle document, empty when it has none, None
    /// when this key may not read it.
    ///
    /// Reading is `s3:GetLifecycleConfiguration`, a separate permission from
    /// the `s3:PutLifecycleConfiguration` that writes. A key with only the
    /// write half must not blind-write, because the PUT replaces the whole
    /// document and would delete rules the host set themselves.
    async fn lifecycle_document(&self, bucket: &str) -> Result<Option<String>> {
        let request = S3Request::new("GET", bucket, None).query("lifecycle", "");
        match self.send(request).await {
            Ok(resp) => Self::text(resp, "GetBucketLifecycleConfiguration")
                .await
                .map(Some),
            // A bucket with no configuration answers 404
            // NoSuchLifecycleConfiguration, which is an empty document.
            Err(ProviderError::NotFound(_)) => Ok(Some(String::new())),
            Err(ProviderError::Auth(err)) => {
                tracing::warn!(
                    bucket,
                    error = %err,
                    "cannot read the bucket's lifecycle rules, so no retention rule was written"
                );
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    /// Reads a response body as text, for the XML-bearing calls.
    async fn text(resp: Response, what: &str) -> Result<String> {
        resp.text()
            .await
            .map_err(|e| ProviderError::Other(format!("reading {what} response: {e}")))
    }
}

/// One S3 request, before signing.
struct S3Request<'a> {
    method: &'a str,
    bucket: &'a str,
    /// None addresses the bucket rather than an object.
    key: Option<&'a str>,
    /// Query parameters; canonicalized (sorted and encoded) before signing.
    query: Vec<(&'a str, String)>,
    content_type: Option<&'a str>,
    /// Extra signed headers, lowercase names (`content-md5`).
    extra_headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl<'a> S3Request<'a> {
    fn new(method: &'a str, bucket: &'a str, key: Option<&'a str>) -> Self {
        S3Request {
            method,
            bucket,
            key,
            query: Vec::new(),
            content_type: None,
            extra_headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn query(mut self, key: &'a str, value: impl Into<String>) -> Self {
        self.query.push((key, value.into()));
        self
    }

    fn body(mut self, content_type: &'a str, body: Vec<u8>) -> Self {
        self.content_type = Some(content_type);
        self.body = body;
        self
    }

    /// A body with no `Content-Type`. `UploadPart` takes none: the completed
    /// object's type was fixed by `CreateMultipartUpload`, and signing an
    /// empty content-type header would be a signature mismatch waiting to
    /// happen.
    fn raw_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.extra_headers.push((name, value.into()));
        self
    }
}

/// SigV4 canonical query string: pairs sorted by encoded key then encoded
/// value, both percent-encoded, valueless sub-resources rendered as `key=`.
fn canonical_query(pairs: &[(&str, String)]) -> String {
    let mut encoded: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (aws_encode(k), aws_encode(v)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encodes an object key for the request path: every segment gets
/// the AWS unreserved treatment, and `/` stays a separator. S3 signs the
/// path exactly as sent and must not see it double-encoded.
fn encode_key(key: &str) -> String {
    key.split('/').map(aws_encode).collect::<Vec<_>>().join("/")
}

/// Refuses a bucket name or region that would mean something to a URL.
///
/// Unlike the object key, both go into the request unencoded, and on AWS and
/// Spaces they go into the *host*: a bucket of `evil.com/x` addresses
/// `https://evil.com/x.s3.eu-west-1.amazonaws.com/...`, which sends a request
/// carrying this store's access key id and a signature to somebody else's
/// server. Encoding them is not the fix, because a real bucket name never
/// needs it; refusing is. `.`, `-` and `_` survive, which covers every name
/// AWS, Spaces and Cloud Storage will issue, and the same check keeps a
/// newline out of the flat config the VM parses.
fn check_authority_part(what: &str, value: &str) -> Result<()> {
    match value
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
    {
        Some(bad) => Err(ProviderError::Other(format!(
            "{what} {value:?} contains {bad:?}, which is not a character any bucket or region \
             name uses"
        ))),
        None => Ok(()),
    }
}

/// Strips the quotes S3 wraps around an ETag.
fn clean_etag(raw: &str) -> String {
    raw.trim().trim_matches('"').to_owned()
}

fn header_string(resp: &Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// State of one in-flight S3 multipart upload.
pub struct S3Session {
    upload_id: String,
    /// (part number, ETag) for every part accepted so far; the complete call
    /// has to echo them back.
    parts: Mutex<Vec<(u32, String)>>,
    size: AtomicU64,
}

impl S3Session {
    /// The upload id, so a test or an operator can correlate an abort.
    pub fn upload_id(&self) -> &str {
        &self.upload_id
    }
}

#[async_trait]
impl MultipartBackend for S3Store {
    type Session = S3Session;

    async fn put_single(
        &self,
        bucket: &str,
        key: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<ObjectMeta> {
        let size = body.len() as u64;
        let resp = self
            .send(S3Request::new("PUT", bucket, Some(key)).body(content_type, body.to_vec()))
            .await?;
        Ok(ObjectMeta {
            key: key.to_owned(),
            size,
            etag: header_string(&resp, "etag").map(|e| clean_etag(&e)),
            content_type: Some(content_type.to_owned()),
            last_modified: None,
        })
    }

    async fn begin(&self, bucket: &str, key: &str, content_type: &str) -> Result<S3Session> {
        let resp = self
            .send(
                S3Request::new("POST", bucket, Some(key))
                    .query("uploads", "")
                    // No body, but the object's content type is fixed here
                    // and inherited by the completed object.
                    .body(content_type, Vec::new()),
            )
            .await?;
        let xml = Self::text(resp, "CreateMultipartUpload").await?;
        let upload_id = xml_value(&xml, "UploadId")
            .map(xml_unescape)
            .ok_or_else(|| {
                ProviderError::Other(format!(
                    "CreateMultipartUpload response carried no UploadId: {}",
                    xml.chars().take(256).collect::<String>()
                ))
            })?;
        Ok(S3Session {
            upload_id,
            parts: Mutex::new(Vec::new()),
            size: AtomicU64::new(0),
        })
    }

    async fn send_part(
        &self,
        bucket: &str,
        key: &str,
        session: &S3Session,
        part: Part<'_>,
    ) -> Result<()> {
        let len = part.body.len() as u64;
        let resp = self
            .send(
                S3Request::new("PUT", bucket, Some(key))
                    .query("partNumber", part.number.to_string())
                    .query("uploadId", session.upload_id.clone())
                    .raw_body(part.body.to_vec()),
            )
            .await?;
        // The ETag is not decoration: CompleteMultipartUpload is rejected
        // unless every part is echoed back with the ETag S3 assigned it.
        let etag = header_string(&resp, "etag")
            .map(|e| clean_etag(&e))
            .filter(|e| !e.is_empty())
            .ok_or_else(|| {
                ProviderError::Other(format!(
                    "UploadPart {} of {key} returned no ETag, so the upload cannot be completed",
                    part.number
                ))
            })?;
        session
            .parts
            .lock()
            .expect("s3 part list lock")
            .push((part.number, etag));
        session.size.fetch_add(len, Ordering::Relaxed);
        Ok(())
    }

    async fn finish(&self, bucket: &str, key: &str, session: &S3Session) -> Result<ObjectMeta> {
        let mut parts = session.parts.lock().expect("s3 part list lock").clone();
        parts.sort_by_key(|(number, _)| *number);
        let mut body =
            String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><CompleteMultipartUpload>");
        for (number, etag) in &parts {
            body.push_str(&format!(
                "<Part><PartNumber>{number}</PartNumber><ETag>&quot;{etag}&quot;</ETag></Part>"
            ));
        }
        body.push_str("</CompleteMultipartUpload>");

        let resp = self
            .send(
                S3Request::new("POST", bucket, Some(key))
                    .query("uploadId", session.upload_id.clone())
                    .body(XML_CONTENT_TYPE, body.into_bytes()),
            )
            .await?;
        let xml = Self::text(resp, "CompleteMultipartUpload").await?;
        // S3 streams this response and can report a failure inside a 200
        // body, precisely because it may take minutes. A caller that trusts
        // the status code here would report a successful recording upload
        // that does not exist.
        if let Some(code) = xml_value(&xml, "Code").map(xml_unescape) {
            return Err(ProviderError::Other(format!(
                "CompleteMultipartUpload failed with {code}: {}",
                xml_value(&xml, "Message")
                    .map(xml_unescape)
                    .unwrap_or_default()
            )));
        }
        if xml_value(&xml, "ETag").is_none() {
            return Err(ProviderError::Other(format!(
                "CompleteMultipartUpload response for {key} carried neither an ETag nor an error: {}",
                xml.chars().take(256).collect::<String>()
            )));
        }
        Ok(ObjectMeta {
            key: key.to_owned(),
            size: session.size.load(Ordering::Relaxed),
            etag: xml_value(&xml, "ETag").map(|e| clean_etag(&xml_unescape(e))),
            content_type: None,
            last_modified: None,
        })
    }

    async fn abort(&self, bucket: &str, key: &str, session: &S3Session) -> Result<()> {
        self.send(
            S3Request::new("DELETE", bucket, Some(key))
                .query("uploadId", session.upload_id.clone()),
        )
        .await?;
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for S3Store {
    fn kind(&self) -> ProviderKind {
        self.kind
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
        let resp = self.send(S3Request::new("HEAD", bucket, Some(key))).await?;
        let size = header_string(&resp, "content-length")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        Ok(ObjectMeta {
            key: key.to_owned(),
            size,
            etag: header_string(&resp, "etag").map(|e| clean_etag(&e)),
            content_type: header_string(&resp, "content-type"),
            last_modified: header_string(&resp, "last-modified"),
        })
    }

    async fn get(
        &self,
        bucket: &str,
        key: &str,
        sink: &mut (dyn ChunkSink + Send),
    ) -> Result<ObjectMeta> {
        // A plain signed GetObject on the streaming client: the body is a
        // recording, so it cannot be held in memory or finished in 30 s.
        let resp = self
            .send_with(&self.streaming, S3Request::new("GET", bucket, Some(key)))
            .await?;
        let mut meta = drain_body(resp, key, sink).await?;
        meta.etag = meta.etag.map(|e| clean_etag(&e));
        Ok(meta)
    }

    async fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = S3Request::new("GET", bucket, None)
                .query("list-type", "2")
                .query("prefix", prefix);
            if let Some(t) = &token {
                req = req.query("continuation-token", t.clone());
            }
            let xml = Self::text(self.send(req).await?, "ListObjectsV2").await?;
            out.extend(parse_list(&xml));
            token = xml_value(&xml, "NextContinuationToken")
                .map(xml_unescape)
                .filter(|t| !t.is_empty());
            if token.is_none() {
                break;
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        // S3 answers 204 whether or not the key existed, which is the
        // idempotence the trait promises.
        self.send(S3Request::new("DELETE", bucket, Some(key)))
            .await?;
        Ok(())
    }

    async fn set_retention(
        &self,
        bucket: &str,
        prefix: &str,
        retention: Retention,
    ) -> Result<RetentionEnforcement> {
        // Read first: the PUT replaces the bucket's whole configuration, so
        // the document has to carry every rule that is already there. See
        // crate::retention.
        let Some(existing) = self.lifecycle_document(bucket).await? else {
            return Ok(RetentionEnforcement::Manual {
                retention,
                note: unreadable_note(retention, "s3:GetLifecycleConfiguration"),
            });
        };
        let Some(xml) = merge_s3_lifecycle(
            &existing,
            prefix,
            retention,
            self.dialect,
            S3_MAX_LIFECYCLE_RULES,
        ) else {
            return Ok(RetentionEnforcement::Manual {
                retention,
                note: at_capacity_note(retention, S3_MAX_LIFECYCLE_RULES),
            });
        };
        let body = xml.clone().into_bytes();
        // PutBucketLifecycleConfiguration is one of the few S3 calls that
        // wants a body integrity header; Content-MD5 is the form both AWS
        // and Spaces accept.
        let md5 = BASE64.encode(&md5::digest(&body));
        self.send(
            S3Request::new("PUT", bucket, None)
                .query("lifecycle", "")
                .header("content-md5", md5)
                .body(XML_CONTENT_TYPE, body),
        )
        .await?;
        Ok(RetentionEnforcement::ServerSide {
            provider: self.kind,
            retention,
            rule_id: rule_id(prefix),
            rule: xml,
        })
    }
}

/// Pulls every `<Contents>` entry out of a ListObjectsV2 body.
fn parse_list(xml: &str) -> Vec<ObjectMeta> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some((entry, after)) = take_tag(rest, "Contents") {
        rest = after;
        let Some(key) = xml_value(entry, "Key").map(xml_unescape) else {
            continue;
        };
        out.push(ObjectMeta {
            key,
            size: xml_value(entry, "Size")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0),
            etag: xml_value(entry, "ETag").map(|e| clean_etag(&xml_unescape(e))),
            content_type: None,
            last_modified: xml_value(entry, "LastModified").map(xml_unescape),
        });
    }
    out
}

/// RFC 1321 MD5, present only because `PutBucketLifecycleConfiguration`
/// requires a `Content-MD5` header and the workspace carries no hash crates.
/// Pinned by the RFC's own test suite below. Not used for anything
/// security-bearing: SigV4's SHA-256 payload hash is what authenticates the
/// request.
mod md5 {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    #[rustfmt::skip]
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee,
        0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be,
        0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa,
        0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c,
        0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05,
        0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039,
        0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1,
        0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];

    pub fn digest(data: &[u8]) -> [u8; 16] {
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
                m[i] = u32::from_le_bytes(word.try_into().expect("4-byte chunk"));
            }
            let [mut a, mut b, mut c, mut d] = state;
            for i in 0..64 {
                let (f, g) = match i {
                    0..=15 => ((b & c) | (!b & d), i),
                    16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                    32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                    _ => (c ^ (b | !d), (7 * i) % 16),
                };
                let f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
                a = d;
                d = c;
                c = b;
                b = b.wrapping_add(f.rotate_left(S[i]));
            }
            for (s, v) in state.iter_mut().zip([a, b, c, d]) {
                *s = s.wrapping_add(v);
            }
        }
        let mut out = [0u8; 16];
        for (i, word) in state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GCS interop is path style against one fixed host, and the signing
    /// region is `auto` regardless of where the bucket lives. Both are
    /// requirements of Cloud Storage's S3 compatibility, not choices, and
    /// getting either wrong yields a 403 nobody can read.
    #[test]
    fn gcs_interop_signs_path_style_against_one_host() {
        let store = S3Store::gcs_interop("GOOG1EXAMPLE".to_owned(), "secret".to_owned());
        assert_eq!(store.region, "auto");
        assert_eq!(store.kind, ProviderKind::Gcp);
        let (url, host, path) = store
            .address("my-jams", Some("jamstream/recordings/abc/take-mix.flac"))
            .unwrap();
        assert_eq!(host, "storage.googleapis.com");
        assert_eq!(path, "/my-jams/jamstream/recordings/abc/take-mix.flac");
        assert_eq!(url, format!("https://{host}{path}"));
        // The bucket-level form the lifecycle rule uses.
        let (_, _, bucket_path) = store.address("my-jams", None).unwrap();
        assert_eq!(bucket_path, "/my-jams");
    }
    use data_encoding::HEXLOWER;

    fn aws_store() -> S3Store {
        S3Store::aws("eu-west-1", "AKIDTEST".to_owned(), "secret".to_owned())
    }

    // ---- MD5: the RFC 1321 test suite ----

    #[test]
    fn md5_rfc1321_vectors() {
        let cases: [(&[u8], &str); 5] = [
            (b"", "d41d8cd98f00b204e9800998ecf8427e"),
            (b"a", "0cc175b9c0f1b6a831c399e269772661"),
            (b"abc", "900150983cd24fb0d6963f7d28e17f72"),
            (b"message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                b"abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(HEXLOWER.encode(&md5::digest(input)), expected);
        }
    }

    #[test]
    fn md5_spans_multiple_blocks() {
        // 80 bytes: two padded blocks, which is where a length or padding
        // mistake shows up.
        assert_eq!(
            HEXLOWER.encode(&md5::digest(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            )),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    // ---- Addressing ----

    #[test]
    fn aws_uses_virtual_hosted_urls() {
        let (url, host, path) = aws_store()
            .address("my-jams", Some("jamstream/recordings/s1/mix.wav"))
            .unwrap();
        assert_eq!(
            url,
            "https://my-jams.s3.eu-west-1.amazonaws.com/jamstream/recordings/s1/mix.wav"
        );
        assert_eq!(host, "my-jams.s3.eu-west-1.amazonaws.com");
        assert_eq!(path, "/jamstream/recordings/s1/mix.wav");
    }

    #[test]
    fn spaces_uses_its_own_hostname_and_region_slug() {
        let store = S3Store::spaces("nyc3", "DO00KEY".to_owned(), "secret".to_owned());
        let (url, host, path) = store.address("my-jams", Some("a/b.wav")).unwrap();
        assert_eq!(url, "https://my-jams.nyc3.digitaloceanspaces.com/a/b.wav");
        assert_eq!(host, "my-jams.nyc3.digitaloceanspaces.com");
        assert_eq!(path, "/a/b.wav");
        // Spaces signs with the region slug, not us-east-1.
        assert_eq!(store.region, "nyc3");
        assert_eq!(store.kind(), ProviderKind::DigitalOcean);
        assert_eq!(store.dialect, LifecycleDialect::SpacesV1);
    }

    #[test]
    fn base_url_override_switches_to_path_style() {
        let store = aws_store().with_base_url("http://127.0.0.1:9123");
        let (url, host, path) = store.address("my-jams", Some("a/b.wav")).unwrap();
        assert_eq!(url, "http://127.0.0.1:9123/my-jams/a/b.wav");
        assert_eq!(
            host, "127.0.0.1:9123",
            "the signed host must carry the port"
        );
        assert_eq!(path, "/my-jams/a/b.wav");
        // Bucket-level calls (list, lifecycle) address the bucket path.
        let (url, _, path) = store.address("my-jams", None).unwrap();
        assert_eq!(url, "http://127.0.0.1:9123/my-jams");
        assert_eq!(path, "/my-jams");
    }

    #[test]
    fn bucket_level_virtual_hosted_path_is_root() {
        let (url, _, path) = aws_store().address("my-jams", None).unwrap();
        assert_eq!(url, "https://my-jams.s3.eu-west-1.amazonaws.com/");
        assert_eq!(path, "/");
    }

    #[test]
    fn empty_bucket_or_key_is_rejected_before_signing() {
        assert!(aws_store().address("", Some("k")).is_err());
        assert!(aws_store().address("b", Some("")).is_err());
    }

    /// The bucket and the region go into the request unencoded, and on AWS and
    /// Spaces they go into the host. A bucket that carries a `/` would address
    /// somebody else's server and send this store's access key id and a
    /// signature there; one that carries a `?` or a `#` splits the query or
    /// fragment so the sent request no longer matches the signed one. Neither
    /// is a name a bucket can have, so both are refused before anything is
    /// signed or sent.
    #[test]
    fn a_bucket_or_region_that_would_redirect_the_request_is_refused() {
        for hostile in [
            "evil.com/x",
            "b?acl",
            "b#frag",
            "b:8080",
            "user@host",
            "b\nregion = elsewhere",
            "b bucket",
            "b/../..",
        ] {
            let err = aws_store()
                .address(hostile, Some("k"))
                .expect_err("a hostile bucket was addressed")
                .to_string();
            assert!(err.contains("not a character"), "{hostile:?}: {err}");
            // Path style is no safer: the same string splits the path there.
            assert!(
                S3Store::gcs_interop("GOOG1".to_owned(), "s".to_owned())
                    .address(hostile, Some("k"))
                    .is_err(),
                "{hostile:?} was addressed path style"
            );
        }
        // A hostile region is refused with any bucket, since it lands in the
        // host beside it.
        assert!(
            S3Store::spaces("nyc3/../..", "DO00KEY".to_owned(), "s".to_owned())
                .address("my-jams", Some("k"))
                .is_err()
        );
        // Every name a real bucket can have still addresses.
        for fine in ["my-jams", "my.jams.2026", "MyJams", "a_b-c.d", "jams1"] {
            assert!(
                aws_store().address(fine, Some("a/b.flac")).is_ok(),
                "{fine:?} was refused"
            );
        }
    }

    #[test]
    fn keys_are_encoded_per_segment_with_slashes_preserved() {
        assert_eq!(encode_key("a/b c/d.wav"), "a/b%20c/d.wav");
        assert_eq!(encode_key("plain.wav"), "plain.wav");
        // '+' and '=' must not survive raw: S3 signs the path as sent.
        assert_eq!(encode_key("a+b=c"), "a%2Bb%3Dc");
        assert_eq!(encode_key("~tilde-._"), "~tilde-._");
    }

    // ---- Canonical query ----

    #[test]
    fn canonical_query_sorts_and_encodes() {
        assert_eq!(
            canonical_query(&[
                ("uploadId", "up 1".to_owned()),
                ("partNumber", "2".to_owned())
            ]),
            "partNumber=2&uploadId=up%201"
        );
        // Valueless sub-resources canonicalize with a trailing '='.
        assert_eq!(canonical_query(&[("uploads", String::new())]), "uploads=");
        assert_eq!(canonical_query(&[]), "");
        assert_eq!(
            canonical_query(&[
                ("prefix", "jamstream/recordings/".to_owned()),
                ("list-type", "2".to_owned()),
                ("continuation-token", "t/1".to_owned()),
            ]),
            "continuation-token=t%2F1&list-type=2&prefix=jamstream%2Frecordings%2F"
        );
    }

    // ---- Response parsing ----

    #[test]
    fn etags_lose_their_quotes() {
        assert_eq!(clean_etag("\"abc123\""), "abc123");
        assert_eq!(clean_etag("  \"abc\"  "), "abc");
        assert_eq!(clean_etag("abc"), "abc");
    }

    #[test]
    fn list_response_parses_every_entry() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult>
  <Name>my-jams</Name>
  <Prefix>jamstream/recordings/</Prefix>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>jamstream/recordings/s1/mix.wav</Key>
    <LastModified>2026-07-25T10:00:00.000Z</LastModified>
    <ETag>&quot;abc123&quot;</ETag>
    <Size>1382400044</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <Contents>
    <Key>jamstream/recordings/s1/stems/bass &amp; amp.wav</Key>
    <LastModified>2026-07-25T10:00:01.000Z</LastModified>
    <ETag>&quot;def456&quot;</ETag>
    <Size>691200044</Size>
  </Contents>
</ListBucketResult>"#;
        let items = parse_list(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key, "jamstream/recordings/s1/mix.wav");
        assert_eq!(items[0].size, 1_382_400_044);
        assert_eq!(items[0].etag.as_deref(), Some("abc123"));
        assert_eq!(
            items[0].last_modified.as_deref(),
            Some("2026-07-25T10:00:00.000Z")
        );
        // XML entities in keys are decoded, so a member name with an
        // ampersand round trips.
        assert_eq!(items[1].key, "jamstream/recordings/s1/stems/bass & amp.wav");
    }

    #[test]
    fn empty_listing_parses_to_nothing() {
        assert!(parse_list("<ListBucketResult><Name>b</Name></ListBucketResult>").is_empty());
        assert!(parse_list("").is_empty());
    }

    #[test]
    fn debug_redacts_the_secret() {
        let rendered = format!(
            "{:?}",
            S3Store::aws("us-east-1", "AKIDVISIBLE".into(), "hunter2".into())
        );
        assert!(rendered.contains("AKIDVISIBLE"));
        assert!(!rendered.contains("hunter2"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn part_size_defaults_and_overrides() {
        assert_eq!(aws_store().part_size(), DEFAULT_PART_SIZE);
        // An override S3 would refuse is raised to one it accepts rather than
        // taken as given: EntityTooSmall on part two of a real recording is not
        // something to discover in production.
        assert_eq!(
            aws_store().with_part_size(8).part_size(),
            crate::storage::MIN_PART_SIZE
        );
        assert_eq!(
            aws_store().with_part_size(0).part_size(),
            crate::storage::MIN_PART_SIZE
        );
        assert_eq!(
            aws_store().with_part_size(DEFAULT_PART_SIZE).part_size(),
            DEFAULT_PART_SIZE
        );
    }
}
