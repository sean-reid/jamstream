//! Object storage for session recordings: the [`ObjectStore`] trait, the
//! key layout, and the multipart upload driver every backend shares.
//!
//! Recordings go to the *host's own* bucket. JamStream never proxies or
//! stores the audio, so the credentials, the bill, and the data all stay with
//! whoever pressed record. That shapes the whole API: there is no notion of a
//! JamStream account here, only a bucket name, a key prefix, and credentials
//! the host already has.
//!
//! # Why multipart is not optional
//!
//! A two-hour broadcast mix at 48 kHz, 16-bit stereo is 1.38 GB, and turning
//! on per-member stems multiplies that by the number of members. A single-shot
//! PUT of 1.38 GB is a bad idea on every provider — S3 caps a single PUT at
//! 5 GB, GCS wants a resumable session for anything large, and a transient
//! failure 90% of the way through a one-shot upload costs the whole upload —
//! and the session VM is racing an idle timer while it happens.
//!
//! So [`ObjectStore::put_stream`] is the upload path, and it escalates on its
//! own: it reads one part, looks ahead one more, and if the source ended it
//! does a plain PUT. Otherwise it opens a multipart upload. Callers never
//! choose, and never need to know the object's size in advance.
//!
//! # Abort semantics
//!
//! Failed multipart uploads are not free. Parts already sent keep billing as
//! storage, and they do not appear in `list`, so nobody finds them. The
//! driver in [`drive_upload`] therefore guarantees: **any** error after the
//! upload is opened — reading the source, sending a part, hitting the part
//! cap, or completing — is followed by an abort of that upload before the
//! error is returned. If the abort itself fails it is logged and the original
//! error is still what the caller sees, because the reason the upload failed
//! is the more useful fact. Backends implement [`MultipartBackend`] and get
//! this behavior for free, which is why the guarantee is testable once rather
//! than three times.
//!
//! Belt and braces: the lifecycle rule written by
//! [`ObjectStore::set_retention`] also expires incomplete multipart uploads
//! (see [`crate::retention`]), which covers the case where the VM dies before
//! the abort can be sent.
//!
//! # Downloads
//!
//! [`ObjectStore::get`] is the other direction, and it is a stream for the
//! same reason: the caller writes each chunk into a [`ChunkSink`] as it
//! arrives, so a 5 GB take never sits in memory. Every backend drains its
//! response through [`drain_body`], which refuses a body that came up short
//! of the `content-length` the provider promised. A truncated take that
//! reports success is the failure this path exists to prevent, and it is one
//! guarantee in one place rather than three.
//!
//! # Part size
//!
//! [`DEFAULT_PART_SIZE`] is 16 MiB, which satisfies both providers' rules at
//! once: above S3's 5 MiB floor for non-final parts, and a multiple of the
//! 256 KiB quantum GCS requires for non-final resumable chunks. It puts a
//! 1.38 GB mix in 83 parts, well inside S3's 10 000-part cap
//! ([`MAX_PARTS`]), and bounds memory at one part in flight: parts are
//! buffered because [`crate::http::send_retrying`] rebuilds each request per
//! attempt, and a body that cannot be replayed cannot be retried.
//! `with_part_size` on each store overrides it, which is how tests stay fast.

use std::io::{ErrorKind, Read};

use async_trait::async_trait;

use crate::provider::{ProviderError, Result};
use crate::retention::{Retention, RetentionEnforcement};
use crate::types::ProviderKind;

pub mod contract;
// Native GCS, which needs a service account token. Recording uses the S3
// interop endpoint instead; see providers::mod on why that matters.
#[cfg(feature = "gcp")]
pub mod gcs;
pub mod mock;
pub mod s3;
pub mod sink;

pub use contract::assert_object_store_contract;
#[cfg(feature = "gcp")]
pub use gcs::GcsStore;
pub use mock::MockStore;
pub use s3::S3Store;
pub use sink::ObjectSink;

/// S3 rejects any non-final part below 5 MiB.
pub const MIN_PART_SIZE: usize = 5 * 1024 * 1024;
/// GCS requires every non-final resumable chunk to be a multiple of 256 KiB.
pub const PART_SIZE_MULTIPLE: usize = 256 * 1024;
/// 16 MiB: see the module docs.
pub const DEFAULT_PART_SIZE: usize = 16 * 1024 * 1024;
/// S3 caps a multipart upload at 10 000 parts.
pub const MAX_PARTS: u32 = 10_000;

/// Content type for the recorded WAV objects.
pub const WAV_CONTENT_TYPE: &str = "audio/wav";
/// Content type for recorded FLAC objects.
pub const FLAC_CONTENT_TYPE: &str = "audio/flac";
/// Content type for the recording manifest.
pub const JSON_CONTENT_TYPE: &str = "application/json";

/// Key prefix every JamStream recording object lives under. Retention rules
/// are scoped to it, so nothing may be written outside it.
pub const RECORDING_PREFIX: &str = "jamstream/recordings";

/// Metadata for one stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    /// Provider entity tag, quotes stripped. Absent when the provider did
    /// not send one.
    pub etag: Option<String>,
    pub content_type: Option<String>,
    /// Provider-reported modification time, verbatim and opaque (S3 sends
    /// RFC 1123 on HEAD and ISO 8601 in listings; GCS sends RFC 3339). For
    /// display, not for arithmetic.
    pub last_modified: Option<String>,
}

impl ObjectMeta {
    /// Metadata with nothing but a key and a size, for backends and
    /// responses that report no more than that.
    pub fn new(key: impl Into<String>, size: u64) -> Self {
        ObjectMeta {
            key: key.into(),
            size,
            etag: None,
            content_type: None,
            last_modified: None,
        }
    }
}

/// The key prefix for one session's recording objects, always ending in `/`.
pub fn session_prefix(session_id: &str) -> String {
    format!("{RECORDING_PREFIX}/{}/", sanitize_component(session_id))
}

/// Key of the broadcast mix for a session.
pub fn mix_key(session_id: &str) -> String {
    format!("{}mix.wav", session_prefix(session_id))
}

/// Key of one member's stem. `member` is a display name or member id and is
/// sanitized: a `/` or `..` in a member name would place the object outside
/// the session prefix, where the retention rule does not reach.
pub fn stem_key(session_id: &str, member: &str) -> String {
    format!(
        "{}stems/{}.wav",
        session_prefix(session_id),
        sanitize_component(member)
    )
}

/// Key of the session's recording manifest: what was recorded, at what
/// format, with which retention.
pub fn manifest_key(session_id: &str) -> String {
    format!("{}manifest.json", session_prefix(session_id))
}

/// Reduces one key component to `[A-Za-z0-9._-]`, mapping everything else to
/// `-`.
///
/// Dots survive, because member names and file extensions want them, but a
/// leading dot is dropped and a run of dots collapses to one, so no component
/// can be or contain `.` or `..`. S3 and GCS treat keys as opaque strings and
/// would not care, but a host who runs `aws s3 sync` on their own bucket
/// does. A component that reduces to nothing becomes `unnamed`.
pub fn sanitize_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c == '.' {
            // Never leading, never doubled.
            if !out.is_empty() && !out.ends_with('.') {
                out.push('.');
            }
        } else if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    if out.trim_matches(['-', '.', '_']).is_empty() {
        return "unnamed".to_owned();
    }
    out
}

/// Yields an object body one part at a time.
///
/// Contract: `next_part` returns at most `max` bytes, and an empty vector
/// means end of input. An implementation should fill each part to `max`
/// whenever more input exists; a short part in the middle of a stream is not
/// a correctness problem for the driver, but S3 rejects a non-final part
/// below [`MIN_PART_SIZE`], so a source that dribbles would fail the upload.
/// [`ReadSource`] fills to `max` for exactly this reason.
#[async_trait]
pub trait PartSource: Send {
    async fn next_part(&mut self, max: usize) -> Result<Vec<u8>>;
}

/// Receives an object body one chunk at a time, in order.
#[async_trait]
pub trait ChunkSink: Send {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()>;
}

/// A [`PartSource`] over bytes already in memory: the manifest, or a short
/// recording.
pub struct BytesSource {
    bytes: Vec<u8>,
    pos: usize,
}

impl BytesSource {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        BytesSource {
            bytes: bytes.into(),
            pos: 0,
        }
    }
}

#[async_trait]
impl PartSource for BytesSource {
    async fn next_part(&mut self, max: usize) -> Result<Vec<u8>> {
        let end = self.bytes.len().min(self.pos.saturating_add(max));
        let part = self.bytes[self.pos..end].to_vec();
        self.pos = end;
        Ok(part)
    }
}

/// A [`PartSource`] over any blocking reader: the finished WAV file on the
/// session VM's disk.
///
/// Reads run on the blocking pool, so pulling a 16 MiB part off a disk that
/// is also being written to never stalls the runtime the session is served
/// from. Each part is filled to `max` before returning, so short reads from
/// the underlying file never turn into undersized parts.
pub struct ReadSource<R: Read + Send + 'static> {
    inner: Option<R>,
}

impl<R: Read + Send + 'static> ReadSource<R> {
    pub fn new(reader: R) -> Self {
        ReadSource {
            inner: Some(reader),
        }
    }
}

#[async_trait]
impl<R: Read + Send + 'static> PartSource for ReadSource<R> {
    async fn next_part(&mut self, max: usize) -> Result<Vec<u8>> {
        let mut reader = self.inner.take().ok_or_else(|| {
            ProviderError::Other("read source is poisoned by an earlier failure".to_owned())
        })?;
        let joined = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; max];
            let mut filled = 0usize;
            let outcome = loop {
                if filled == max {
                    break Ok(filled);
                }
                match reader.read(&mut buf[filled..]) {
                    Ok(0) => break Ok(filled),
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => break Err(e),
                }
            };
            (
                reader,
                outcome.map(|n| {
                    buf.truncate(n);
                    buf
                }),
            )
        })
        .await
        .map_err(|e| ProviderError::Other(format!("recording read task failed: {e}")))?;
        let (reader, outcome) = joined;
        self.inner = Some(reader);
        // A read error poisons nothing on its own; the reader is handed back
        // so a caller that retries the upload gets a clear error rather than
        // a silently truncated object.
        outcome.map_err(|e| ProviderError::Other(format!("reading recording: {e}")))
    }
}

/// Cloud object storage, as much of it as session recording needs.
///
/// Object safe: the server holds a `Box<dyn ObjectStore>` chosen from the
/// host's configured provider. Every method maps to one provider API call
/// except [`ObjectStore::put_stream`], which is the multipart lifecycle.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// Which cloud this store talks to. DigitalOcean Spaces reports
    /// [`ProviderKind::DigitalOcean`] even though it speaks S3.
    fn kind(&self) -> ProviderKind;

    /// Part size this store uses for multipart uploads.
    fn part_size(&self) -> usize {
        DEFAULT_PART_SIZE
    }

    /// Uploads a small object in one request. Use [`ObjectStore::put_stream`]
    /// for recordings; this is for manifests and tests.
    async fn put(
        &self,
        bucket: &str,
        key: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<ObjectMeta>;

    /// Uploads `source` under `key`, escalating to a multipart upload when
    /// the body does not fit in one part. On any failure the upload is
    /// aborted before the error is returned; see the module docs.
    async fn put_stream(
        &self,
        bucket: &str,
        key: &str,
        content_type: &str,
        source: &mut (dyn PartSource + Send),
    ) -> Result<ObjectMeta>;

    /// Metadata for one object. [`ProviderError::NotFound`] when it is not
    /// there.
    async fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta>;

    /// Streams one object into `sink`, a chunk at a time. Returns metadata
    /// whose `size` is the number of bytes actually delivered.
    async fn get(
        &self,
        bucket: &str,
        key: &str,
        sink: &mut (dyn ChunkSink + Send),
    ) -> Result<ObjectMeta>;

    /// Every object under `prefix`, following the provider's pagination to
    /// the end. Sorted by key.
    async fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>>;

    /// Deletes one object. Idempotent: deleting a key that is not there
    /// succeeds. Cleanup paths run after partial failures and must not have
    /// to distinguish "I deleted it" from "it was already gone".
    async fn delete(&self, bucket: &str, key: &str) -> Result<()>;

    /// Applies the retention choice to `prefix` in `bucket`, server-side
    /// where the provider supports it. See [`crate::retention`].
    async fn set_retention(
        &self,
        bucket: &str,
        prefix: &str,
        retention: Retention,
    ) -> Result<RetentionEnforcement>;
}

/// One part on its way to a backend.
pub(crate) struct Part<'a> {
    /// 1-based part number, as S3 counts them.
    pub number: u32,
    /// Byte offset of this part within the object, which is what the GCS
    /// resumable protocol needs for its `Content-Range`.
    pub offset: u64,
    /// True for the final part. GCS learns the total object size from it;
    /// S3 ignores it.
    pub last: bool,
    pub body: &'a [u8],
}

/// The per-provider half of a multipart upload. The lifecycle order, the
/// escalation decision, and the abort guarantee live in [`drive_upload`];
/// backends only say how to talk to their API.
///
/// `Session` carries whatever the provider needs between calls (S3: the
/// upload id and the per-part ETags; GCS: the resumable session URI and the
/// running offset), and is shared immutably, so backends that accumulate
/// state use interior mutability.
#[async_trait]
pub(crate) trait MultipartBackend: Send + Sync {
    type Session: Send + Sync;

    /// Single-request upload, used when the body fits in one part.
    async fn put_single(
        &self,
        bucket: &str,
        key: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<ObjectMeta>;

    /// Opens a multipart upload.
    async fn begin(&self, bucket: &str, key: &str, content_type: &str) -> Result<Self::Session>;

    /// Sends one part. S3 addresses the part by bucket and key plus the
    /// upload id; GCS addresses it by the session URI alone and ignores
    /// both.
    async fn send_part(
        &self,
        bucket: &str,
        key: &str,
        session: &Self::Session,
        part: Part<'_>,
    ) -> Result<()>;

    /// Commits the upload. Called once, after the final part.
    async fn finish(&self, bucket: &str, key: &str, session: &Self::Session) -> Result<ObjectMeta>;

    /// Discards the upload and every part already sent. Must be safe to call
    /// on an upload that was never going to complete.
    async fn abort(&self, bucket: &str, key: &str, session: &Self::Session) -> Result<()>;
}

/// Uploads `source` under `key` through `backend`.
///
/// Reads one part, looks ahead one more, and takes the single-PUT path when
/// the body ended. Otherwise it opens a multipart upload and streams parts,
/// with the lookahead telling it which part is last. Any failure after the
/// upload is opened aborts it. See the module docs.
pub(crate) async fn drive_upload<B: MultipartBackend + ?Sized>(
    backend: &B,
    bucket: &str,
    key: &str,
    content_type: &str,
    source: &mut (dyn PartSource + Send),
    part_size: usize,
) -> Result<ObjectMeta> {
    let part_size = part_size.max(1);
    let first = source.next_part(part_size).await?;
    let second = source.next_part(part_size).await?;
    if second.is_empty() {
        // One part, or none at all: no multipart bookkeeping, nothing that
        // could be left dangling.
        return backend.put_single(bucket, key, content_type, &first).await;
    }

    let session = backend.begin(bucket, key, content_type).await?;
    let outcome = match feed_parts(
        backend, bucket, key, &session, source, part_size, first, second,
    )
    .await
    {
        Ok(()) => backend.finish(bucket, key, &session).await,
        Err(err) => Err(err),
    };
    match outcome {
        Ok(meta) => Ok(meta),
        Err(err) => {
            if let Err(abort_err) = backend.abort(bucket, key, &session).await {
                // The upload failure is the actionable one; a failed abort
                // only means the lifecycle rule has to catch the parts.
                tracing::warn!(
                    bucket,
                    key,
                    error = %abort_err,
                    "aborting the failed multipart upload also failed; the \
                     abort-incomplete-multipart lifecycle rule will reclaim the parts"
                );
            }
            Err(err)
        }
    }
}

/// Streams parts until the source runs out, one part of lookahead ahead so
/// the final part can be flagged as final when it is sent.
#[allow(clippy::too_many_arguments)]
async fn feed_parts<B: MultipartBackend + ?Sized>(
    backend: &B,
    bucket: &str,
    key: &str,
    session: &B::Session,
    source: &mut (dyn PartSource + Send),
    part_size: usize,
    first: Vec<u8>,
    second: Vec<u8>,
) -> Result<()> {
    let mut current = first;
    let mut next = second;
    let mut number = 1u32;
    let mut offset = 0u64;
    loop {
        if number > MAX_PARTS {
            return Err(ProviderError::Other(format!(
                "recording exceeds {MAX_PARTS} parts at {part_size} bytes each; \
                 raise the part size"
            )));
        }
        let len = current.len() as u64;
        let last = next.is_empty();
        backend
            .send_part(
                bucket,
                key,
                session,
                Part {
                    number,
                    offset,
                    last,
                    body: &current,
                },
            )
            .await?;
        if last {
            return Ok(());
        }
        offset += len;
        number += 1;
        current = next;
        next = source.next_part(part_size).await?;
    }
}

/// Writes a response body into `sink` chunk by chunk, refusing a body that
/// came up short of the `content-length` the provider promised.
///
/// The whole download path shares this so the truncation guarantee is
/// testable once, the way [`drive_upload`] centralizes the abort guarantee.
/// Bodies are read with [`reqwest::Response::chunk`] rather than collected:
/// a take is gigabytes, and nothing here may hold one in memory.
pub(crate) async fn drain_body(
    mut resp: reqwest::Response,
    key: &str,
    sink: &mut (dyn ChunkSink + Send),
) -> Result<ObjectMeta> {
    let expected = header_value(&resp, "content-length").and_then(|v| v.parse::<u64>().ok());
    let etag = header_value(&resp, "etag");
    let content_type = header_value(&resp, "content-type");
    let last_modified = header_value(&resp, "last-modified");

    let mut delivered = 0u64;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                delivered += chunk.len() as u64;
                sink.write_chunk(&chunk).await?;
            }
            Ok(None) => break,
            Err(e) => {
                return Err(ProviderError::Other(format!(
                    "downloading {key} failed after {delivered} bytes: {e}"
                )));
            }
        }
    }
    check_delivered(expected, delivered, key)?;
    Ok(ObjectMeta {
        key: key.to_owned(),
        size: delivered,
        etag,
        content_type,
        last_modified,
    })
}

/// Fails when the provider promised a length and then delivered a different
/// one, because a silently short recording is worse than no recording.
pub(crate) fn check_delivered(expected: Option<u64>, delivered: u64, key: &str) -> Result<()> {
    match expected {
        Some(expected) if expected != delivered => Err(ProviderError::Other(format!(
            "download of {key} is truncated: content-length promised {expected} bytes, \
             {delivered} arrived"
        ))),
        _ => Ok(()),
    }
}

fn header_value(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

impl crate::cloudinit::RecordingStorage {
    /// The store this config points at, ready for [`ObjectSink::open`]. One
    /// factory so the VM and any probe build the same client from the same
    /// file. Every provider signs SigV4 with the same key pair; only the
    /// endpoint differs, which is what keeps asymmetric crypto out of the
    /// session server.
    pub fn object_store(&self) -> Result<std::sync::Arc<dyn ObjectStore>> {
        use crate::cloudinit::StorageCredential::KeyPair;
        let KeyPair {
            access_key_id,
            secret_access_key,
        } = &self.credential;
        let (id, secret) = (access_key_id.clone(), secret_access_key.clone());
        Ok(match self.provider {
            ProviderKind::Aws => std::sync::Arc::new(S3Store::aws(self.region.clone(), id, secret)),
            ProviderKind::DigitalOcean => {
                std::sync::Arc::new(S3Store::spaces(self.region.clone(), id, secret))
            }
            ProviderKind::Gcp => std::sync::Arc::new(S3Store::gcs_interop(id, secret)),
            other => {
                return Err(ProviderError::Other(format!(
                    "provider {other:?} has no recording storage"
                )));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_layout_stays_under_one_prefix() {
        assert_eq!(session_prefix("abc123"), "jamstream/recordings/abc123/");
        assert_eq!(mix_key("abc123"), "jamstream/recordings/abc123/mix.wav");
        assert_eq!(
            stem_key("abc123", "sean"),
            "jamstream/recordings/abc123/stems/sean.wav"
        );
        assert_eq!(
            manifest_key("abc123"),
            "jamstream/recordings/abc123/manifest.json"
        );
        for key in [
            mix_key("abc123"),
            stem_key("abc123", "sean"),
            manifest_key("abc123"),
        ] {
            assert!(
                key.starts_with(&session_prefix("abc123")),
                "{key} escaped the session prefix"
            );
        }
    }

    #[test]
    fn member_names_cannot_escape_the_prefix() {
        // A slash, a parent-directory hop, and a name that is nothing but
        // punctuation: all have to land inside the session prefix, because
        // the retention rule is scoped to it.
        for name in ["../../etc/passwd", "a/b", "..", ".", "", "  ", "Sean's Amp"] {
            let key = stem_key("s1", name);
            assert!(
                key.starts_with("jamstream/recordings/s1/stems/"),
                "{name:?} produced {key}"
            );
            assert!(!key.contains(".."), "{name:?} produced {key}");
        }
        assert_eq!(
            stem_key("s1", "Sean's Amp"),
            "jamstream/recordings/s1/stems/Sean-s-Amp.wav"
        );
        assert_eq!(
            stem_key("s1", ".."),
            "jamstream/recordings/s1/stems/unnamed.wav"
        );
        assert_eq!(
            stem_key("s1", "../../etc/passwd"),
            "jamstream/recordings/s1/stems/-.-etc-passwd.wav"
        );
    }

    #[test]
    fn session_ids_are_sanitized_too() {
        assert_eq!(session_prefix("../evil"), "jamstream/recordings/-evil/");
        // The common case is untouched: session ids are lowercase hex.
        assert_eq!(
            session_prefix("deadbeefcafef00d"),
            "jamstream/recordings/deadbeefcafef00d/"
        );
    }

    #[test]
    fn default_part_size_satisfies_both_providers() {
        const {
            assert!(
                DEFAULT_PART_SIZE >= MIN_PART_SIZE,
                "S3 rejects non-final parts below 5 MiB"
            );
            assert!(
                DEFAULT_PART_SIZE % PART_SIZE_MULTIPLE == 0,
                "GCS requires non-final chunks to be a multiple of 256 KiB"
            );
        }
        // A two-hour 16-bit stereo mix has to fit inside the part cap.
        let two_hour_mix: u64 = 1_382_400_044;
        let parts = two_hour_mix.div_ceil(DEFAULT_PART_SIZE as u64);
        assert!(parts < MAX_PARTS as u64, "{parts} parts");
    }

    #[tokio::test]
    async fn bytes_source_yields_parts_then_empty() {
        let mut src = BytesSource::new(b"abcdefg".to_vec());
        assert_eq!(src.next_part(3).await.unwrap(), b"abc");
        assert_eq!(src.next_part(3).await.unwrap(), b"def");
        assert_eq!(src.next_part(3).await.unwrap(), b"g");
        assert!(src.next_part(3).await.unwrap().is_empty());
        assert!(src.next_part(3).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn read_source_fills_parts_despite_short_reads() {
        /// A reader that hands back one byte at a time, the way a pipe or a
        /// file still being flushed can.
        struct Dribble(Vec<u8>, usize);
        impl Read for Dribble {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.1 >= self.0.len() || buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = self.0[self.1];
                self.1 += 1;
                Ok(1)
            }
        }
        let mut src = ReadSource::new(Dribble(b"0123456789".to_vec(), 0));
        // Full parts despite one-byte reads underneath: S3 would reject an
        // undersized non-final part.
        assert_eq!(src.next_part(4).await.unwrap(), b"0123");
        assert_eq!(src.next_part(4).await.unwrap(), b"4567");
        assert_eq!(src.next_part(4).await.unwrap(), b"89");
        assert!(src.next_part(4).await.unwrap().is_empty());
    }

    /// The guard behind every download: a body that came up short of the
    /// promised length must not be reported as a recording.
    ///
    /// Tested here rather than over HTTP because no HTTP server will send the
    /// mismatch. hyper panics inside its own encoder when a manually set
    /// content-length disagrees with the payload, so a wiremock far end cannot
    /// be made to lie; the only honest seam is the comparison itself, which is
    /// what `drain_body` calls once the last chunk is in.
    #[test]
    fn a_short_body_is_refused_and_names_both_sizes() {
        let err = check_delivered(Some(1_382_400_044), 691_200_000, "mix.wav").unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ProviderError::Other(_)), "{err:?}");
        assert!(msg.contains("mix.wav"), "{msg}");
        assert!(msg.contains("1382400044"), "the expected size: {msg}");
        assert!(msg.contains("691200000"), "the delivered size: {msg}");
        assert!(msg.contains("truncated"), "{msg}");
    }

    #[test]
    fn a_body_that_matches_or_promised_nothing_is_accepted() {
        assert!(check_delivered(Some(20), 20, "k.wav").is_ok());
        assert!(check_delivered(Some(0), 0, "empty.wav").is_ok());
        // No content-length is a chunked response, which promises nothing;
        // rejecting it would break downloads that are otherwise fine.
        assert!(check_delivered(None, 4096, "k.wav").is_ok());
        // A body longer than promised is just as wrong as a short one.
        assert!(check_delivered(Some(20), 21, "k.wav").is_err());
    }

    #[tokio::test]
    async fn read_source_surfaces_io_errors() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(ErrorKind::PermissionDenied, "nope"))
            }
        }
        let mut src = ReadSource::new(Broken);
        let err = src.next_part(8).await.unwrap_err();
        assert!(err.to_string().contains("reading recording"), "{err}");
    }
}
