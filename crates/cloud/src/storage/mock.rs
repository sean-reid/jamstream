//! In-memory [`ObjectStore`] for server, CLI, and end-to-end tests.
//!
//! It goes through the same `crate::storage::drive_upload` driver the real
//! backends do, so a test that exercises `put_stream` against `MockStore`
//! exercises the real escalation and abort logic, with only the transport
//! faked. Failure injection ([`MockStore::fail_part`],
//! [`MockStore::fail_complete`]) makes the abort path reachable without a
//! network, and [`MockStore::pending_uploads`] is how a test proves an
//! aborted upload left nothing behind.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::provider::{ProviderError, Result};
use crate::retention::{Retention, RetentionEnforcement, manual_note, rule_id};
use crate::storage::{
    ChunkSink, DEFAULT_PART_SIZE, MultipartBackend, ObjectMeta, ObjectStore, Part, PartSource,
    drive_upload,
};
use crate::types::ProviderKind;

/// Every call made against the store, in order, for test assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreCall {
    Put {
        bucket: String,
        key: String,
        size: usize,
    },
    Begin {
        bucket: String,
        key: String,
    },
    Part {
        key: String,
        number: u32,
        offset: u64,
        last: bool,
        size: usize,
    },
    Complete {
        key: String,
        parts: u32,
    },
    Abort {
        key: String,
        upload_id: String,
    },
    Head {
        bucket: String,
        key: String,
    },
    Get {
        bucket: String,
        key: String,
    },
    List {
        bucket: String,
        prefix: String,
    },
    Delete {
        bucket: String,
        key: String,
    },
    SetRetention {
        bucket: String,
        prefix: String,
        retention: Retention,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stored {
    body: Vec<u8>,
    content_type: String,
}

/// A multipart upload that has been opened and not yet completed or aborted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpload {
    pub upload_id: String,
    pub bucket: String,
    pub key: String,
    pub content_type: String,
    /// Parts accepted so far, in arrival order.
    pub parts: Vec<(u32, Vec<u8>)>,
}

#[derive(Default)]
struct State {
    /// (bucket, key) -> object.
    objects: BTreeMap<(String, String), Stored>,
    /// upload id -> in-flight upload. Anything left here after an upload
    /// returns is a leak.
    uploads: BTreeMap<String, PendingUpload>,
    /// (bucket, prefix) -> the applied retention rule.
    retention: BTreeMap<(String, String), Retention>,
    calls: Vec<StoreCall>,
    next_upload: u64,
    /// Fail the part with this 1-based number.
    fail_part: Option<u32>,
    fail_complete: bool,
}

/// In-memory object store.
pub struct MockStore {
    kind: ProviderKind,
    part_size: usize,
    /// False to simulate a target with no lifecycle API, which is the
    /// documented-note fallback path.
    lifecycle_supported: bool,
    state: Mutex<State>,
}

impl Default for MockStore {
    fn default() -> Self {
        Self::new(ProviderKind::Aws)
    }
}

impl MockStore {
    pub fn new(kind: ProviderKind) -> Self {
        MockStore {
            kind,
            part_size: DEFAULT_PART_SIZE,
            lifecycle_supported: true,
            state: Mutex::new(State::default()),
        }
    }

    /// Tiny parts, so a test can drive a multi-part upload with a handful of
    /// bytes instead of 48 MiB.
    ///
    /// Not clamped to [`crate::storage::MIN_PART_SIZE`] the way the real stores
    /// are: nothing here is a provider, so no provider rule applies, and the
    /// server and CLI tests that drive this store are about the upload
    /// lifecycle rather than about part sizes. A test that cares what a
    /// provider accepts has to talk to `S3Store` or `GcsStore`.
    pub fn with_part_size(mut self, bytes: usize) -> Self {
        self.part_size = bytes.max(1);
        self
    }

    /// Pretends the target has no lifecycle API, so `set_retention` returns
    /// [`RetentionEnforcement::Manual`].
    pub fn without_lifecycle_support(mut self) -> Self {
        self.lifecycle_supported = false;
        self
    }

    /// Makes the part with this 1-based number fail, the way a network drop
    /// mid-upload would.
    pub fn fail_part(&self, number: u32) {
        self.state.lock().expect("mock store lock").fail_part = Some(number);
    }

    /// Makes the completion call fail after every part was accepted.
    pub fn fail_complete(&self) {
        self.state.lock().expect("mock store lock").fail_complete = true;
    }

    pub fn calls(&self) -> Vec<StoreCall> {
        self.state.lock().expect("mock store lock").calls.clone()
    }

    /// Keys currently stored in `bucket`.
    pub fn keys(&self, bucket: &str) -> Vec<String> {
        self.state
            .lock()
            .expect("mock store lock")
            .objects
            .keys()
            .filter(|(b, _)| b == bucket)
            .map(|(_, k)| k.clone())
            .collect()
    }

    /// Body of one stored object, for content assertions the trait itself
    /// offers no way to make.
    pub fn body(&self, bucket: &str, key: &str) -> Option<Vec<u8>> {
        self.state
            .lock()
            .expect("mock store lock")
            .objects
            .get(&(bucket.to_owned(), key.to_owned()))
            .map(|o| o.body.clone())
    }

    /// Multipart uploads that were opened and never completed or aborted.
    /// Must be empty after any `put_stream`, successful or not: that is the
    /// whole point of the abort guarantee.
    pub fn pending_uploads(&self) -> Vec<PendingUpload> {
        self.state
            .lock()
            .expect("mock store lock")
            .uploads
            .values()
            .cloned()
            .collect()
    }

    /// The retention rule applied to one prefix, if any.
    pub fn retention_for(&self, bucket: &str, prefix: &str) -> Option<Retention> {
        self.state
            .lock()
            .expect("mock store lock")
            .retention
            .get(&(bucket.to_owned(), prefix.to_owned()))
            .copied()
    }
}

#[async_trait]
impl MultipartBackend for MockStore {
    type Session = String;

    async fn put_single(
        &self,
        bucket: &str,
        key: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<ObjectMeta> {
        let mut s = self.state.lock().expect("mock store lock");
        s.calls.push(StoreCall::Put {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            size: body.len(),
        });
        s.objects.insert(
            (bucket.to_owned(), key.to_owned()),
            Stored {
                body: body.to_vec(),
                content_type: content_type.to_owned(),
            },
        );
        Ok(ObjectMeta {
            key: key.to_owned(),
            size: body.len() as u64,
            etag: Some(format!("mock-{}", body.len())),
            content_type: Some(content_type.to_owned()),
            last_modified: None,
        })
    }

    async fn begin(&self, bucket: &str, key: &str, content_type: &str) -> Result<String> {
        let mut s = self.state.lock().expect("mock store lock");
        s.calls.push(StoreCall::Begin {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
        });
        s.next_upload += 1;
        let upload_id = format!("mock-upload-{}", s.next_upload);
        s.uploads.insert(
            upload_id.clone(),
            PendingUpload {
                upload_id: upload_id.clone(),
                bucket: bucket.to_owned(),
                key: key.to_owned(),
                content_type: content_type.to_owned(),
                parts: Vec::new(),
            },
        );
        Ok(upload_id)
    }

    async fn send_part(
        &self,
        _bucket: &str,
        key: &str,
        session: &String,
        part: Part<'_>,
    ) -> Result<()> {
        let mut s = self.state.lock().expect("mock store lock");
        s.calls.push(StoreCall::Part {
            key: key.to_owned(),
            number: part.number,
            offset: part.offset,
            last: part.last,
            size: part.body.len(),
        });
        if s.fail_part == Some(part.number) {
            return Err(ProviderError::Transient(format!(
                "mock part {} failed",
                part.number
            )));
        }
        let upload = s
            .uploads
            .get_mut(session)
            .ok_or_else(|| ProviderError::NotFound(format!("upload {session}")))?;
        upload.parts.push((part.number, part.body.to_vec()));
        Ok(())
    }

    async fn finish(&self, bucket: &str, key: &str, session: &String) -> Result<ObjectMeta> {
        let mut s = self.state.lock().expect("mock store lock");
        let upload = s
            .uploads
            .get(session)
            .cloned()
            .ok_or_else(|| ProviderError::NotFound(format!("upload {session}")))?;
        s.calls.push(StoreCall::Complete {
            key: key.to_owned(),
            parts: upload.parts.len() as u32,
        });
        if s.fail_complete {
            return Err(ProviderError::Transient("mock complete failed".to_owned()));
        }
        // Reassemble in part order, as the provider does.
        let mut parts = upload.parts.clone();
        parts.sort_by_key(|(number, _)| *number);
        let body: Vec<u8> = parts.into_iter().flat_map(|(_, bytes)| bytes).collect();
        let size = body.len() as u64;
        s.objects.insert(
            (bucket.to_owned(), key.to_owned()),
            Stored {
                body,
                content_type: upload.content_type.clone(),
            },
        );
        s.uploads.remove(session);
        Ok(ObjectMeta {
            key: key.to_owned(),
            size,
            etag: Some(format!("mock-multipart-{}", upload.parts.len())),
            content_type: Some(upload.content_type),
            last_modified: None,
        })
    }

    async fn abort(&self, _bucket: &str, key: &str, session: &String) -> Result<()> {
        let mut s = self.state.lock().expect("mock store lock");
        s.calls.push(StoreCall::Abort {
            key: key.to_owned(),
            upload_id: session.clone(),
        });
        // Dropping the pending upload discards every part with it, which is
        // exactly what the provider does on AbortMultipartUpload.
        s.uploads.remove(session);
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for MockStore {
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
        let mut s = self.state.lock().expect("mock store lock");
        s.calls.push(StoreCall::Head {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
        });
        let stored = s
            .objects
            .get(&(bucket.to_owned(), key.to_owned()))
            .ok_or_else(|| ProviderError::NotFound(format!("{bucket}/{key}")))?;
        Ok(ObjectMeta {
            key: key.to_owned(),
            size: stored.body.len() as u64,
            etag: Some(format!("mock-{}", stored.body.len())),
            content_type: Some(stored.content_type.clone()),
            last_modified: None,
        })
    }

    async fn get(
        &self,
        bucket: &str,
        key: &str,
        sink: &mut (dyn ChunkSink + Send),
    ) -> Result<ObjectMeta> {
        // The body is cloned out before the first await: the lock cannot be
        // held across one, and a sink is free to be slow.
        let stored = {
            let mut s = self.state.lock().expect("mock store lock");
            s.calls.push(StoreCall::Get {
                bucket: bucket.to_owned(),
                key: key.to_owned(),
            });
            s.objects
                .get(&(bucket.to_owned(), key.to_owned()))
                .cloned()
                .ok_or_else(|| ProviderError::NotFound(format!("{bucket}/{key}")))?
        };
        // Served a part at a time, so a multi-chunk sink is exercised by
        // every test that downloads through the mock.
        for chunk in stored.body.chunks(self.part_size()) {
            sink.write_chunk(chunk).await?;
        }
        Ok(ObjectMeta {
            key: key.to_owned(),
            size: stored.body.len() as u64,
            etag: Some(format!("mock-{}", stored.body.len())),
            content_type: Some(stored.content_type),
            last_modified: None,
        })
    }

    async fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let mut s = self.state.lock().expect("mock store lock");
        s.calls.push(StoreCall::List {
            bucket: bucket.to_owned(),
            prefix: prefix.to_owned(),
        });
        Ok(s.objects
            .iter()
            .filter(|((b, k), _)| b == bucket && k.starts_with(prefix))
            .map(|((_, k), stored)| ObjectMeta {
                key: k.clone(),
                size: stored.body.len() as u64,
                etag: Some(format!("mock-{}", stored.body.len())),
                content_type: Some(stored.content_type.clone()),
                last_modified: None,
            })
            .collect())
    }

    async fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        let mut s = self.state.lock().expect("mock store lock");
        s.calls.push(StoreCall::Delete {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
        });
        // Idempotent, as the trait promises.
        s.objects.remove(&(bucket.to_owned(), key.to_owned()));
        Ok(())
    }

    async fn set_retention(
        &self,
        bucket: &str,
        prefix: &str,
        retention: Retention,
    ) -> Result<RetentionEnforcement> {
        let mut s = self.state.lock().expect("mock store lock");
        s.calls.push(StoreCall::SetRetention {
            bucket: bucket.to_owned(),
            prefix: prefix.to_owned(),
            retention,
        });
        if !self.lifecycle_supported {
            return Ok(RetentionEnforcement::Manual {
                retention,
                note: manual_note(retention),
            });
        }
        s.retention
            .insert((bucket.to_owned(), prefix.to_owned()), retention);
        // The whole bucket's rules, because that is what the real stores
        // return: both providers replace the entire lifecycle document on
        // every write, so a store that reported only the rule it just set
        // would hide the bug where the rest of them vanish.
        let rule = s
            .retention
            .iter()
            .filter(|((b, _), _)| b == bucket)
            .map(|((_, p), r)| format!("{}: {p} -> {r}\n", rule_id(p)))
            .collect();
        Ok(RetentionEnforcement::ServerSide {
            provider: self.kind,
            retention,
            rule_id: rule_id(prefix),
            rule,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{BytesSource, FLAC_CONTENT_TYPE, session_prefix};

    /// A take's key, the shape the recorder produces.
    fn take_key() -> String {
        format!("{}jamstream-2026-07-25-1030-mix.flac", session_prefix("s1"))
    }

    /// Body that spans three parts of 8 bytes plus a 3-byte tail.
    fn body(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn multipart_upload_reassembles_exactly() {
        let store = MockStore::new(ProviderKind::Aws).with_part_size(8);
        let bytes = body(27);
        let meta = store
            .put_stream(
                "b",
                "take.flac",
                FLAC_CONTENT_TYPE,
                &mut BytesSource::new(bytes.clone()),
            )
            .await
            .unwrap();
        assert_eq!(meta.size, 27);
        assert_eq!(store.body("b", "take.flac").unwrap(), bytes);
        assert!(store.pending_uploads().is_empty());

        // 8 + 8 + 8 + 3, with `last` only on the final part.
        let parts: Vec<StoreCall> = store
            .calls()
            .into_iter()
            .filter(|c| matches!(c, StoreCall::Part { .. }))
            .collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(
            parts[0],
            StoreCall::Part {
                key: "take.flac".to_owned(),
                number: 1,
                offset: 0,
                last: false,
                size: 8
            }
        );
        assert_eq!(
            parts[3],
            StoreCall::Part {
                key: "take.flac".to_owned(),
                number: 4,
                offset: 24,
                last: true,
                size: 3
            }
        );
    }

    #[tokio::test]
    async fn a_body_that_fits_in_one_part_never_opens_a_multipart_upload() {
        let store = MockStore::new(ProviderKind::Gcp).with_part_size(64);
        store
            .put_stream(
                "b",
                "take.flac",
                FLAC_CONTENT_TYPE,
                &mut BytesSource::new(body(64)),
            )
            .await
            .unwrap();
        assert!(
            store
                .calls()
                .iter()
                .all(|c| !matches!(c, StoreCall::Begin { .. })),
            "a single-part body must take the plain PUT path: {:?}",
            store.calls()
        );
        assert_eq!(store.body("b", "take.flac").unwrap().len(), 64);
    }

    #[tokio::test]
    async fn an_empty_body_is_a_single_put() {
        let store = MockStore::new(ProviderKind::Aws).with_part_size(8);
        let meta = store
            .put_stream(
                "b",
                "empty.flac",
                FLAC_CONTENT_TYPE,
                &mut BytesSource::new(vec![]),
            )
            .await
            .unwrap();
        assert_eq!(meta.size, 0);
        assert_eq!(store.body("b", "empty.flac").unwrap(), Vec::<u8>::new());
        assert!(store.pending_uploads().is_empty());
    }

    #[tokio::test]
    async fn a_failed_part_aborts_and_leaves_nothing_behind() {
        let store = MockStore::new(ProviderKind::Aws).with_part_size(8);
        store.fail_part(3);
        let key = take_key();
        let err = store
            .put_stream(
                "b",
                &key,
                FLAC_CONTENT_TYPE,
                &mut BytesSource::new(body(40)),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Transient(_)), "{err:?}");

        assert!(
            store.pending_uploads().is_empty(),
            "the upload was not aborted: {:?}",
            store.pending_uploads()
        );
        assert!(
            store.keys("b").is_empty(),
            "a failed upload must not leave an object"
        );
        let aborts: Vec<StoreCall> = store
            .calls()
            .into_iter()
            .filter(|c| matches!(c, StoreCall::Abort { .. }))
            .collect();
        assert_eq!(aborts.len(), 1, "exactly one abort: {:?}", store.calls());
        // No parts were sent after the failure.
        assert!(
            !store.calls().iter().any(|c| matches!(
                c,
                StoreCall::Part { number, .. } if *number > 3
            )),
            "the driver kept sending parts after a failure"
        );
    }

    #[tokio::test]
    async fn a_failed_completion_also_aborts() {
        let store = MockStore::new(ProviderKind::Aws).with_part_size(8);
        store.fail_complete();
        let err = store
            .put_stream(
                "b",
                "take.flac",
                FLAC_CONTENT_TYPE,
                &mut BytesSource::new(body(20)),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Transient(_)), "{err:?}");
        assert!(store.pending_uploads().is_empty());
        assert!(store.keys("b").is_empty());
        assert!(
            store
                .calls()
                .iter()
                .any(|c| matches!(c, StoreCall::Abort { .. })),
            "a failed complete must still abort the upload"
        );
    }

    #[tokio::test]
    async fn a_source_that_fails_mid_stream_aborts_too() {
        /// Yields two parts, then errors: a disk read failing while the VM
        /// is being torn down.
        struct Flaky(u32);
        #[async_trait]
        impl PartSource for Flaky {
            async fn next_part(&mut self, max: usize) -> Result<Vec<u8>> {
                self.0 += 1;
                if self.0 > 2 {
                    return Err(ProviderError::Other("disk went away".to_owned()));
                }
                Ok(vec![7u8; max])
            }
        }
        let store = MockStore::new(ProviderKind::Aws).with_part_size(8);
        let err = store
            .put_stream("b", "take.flac", FLAC_CONTENT_TYPE, &mut Flaky(0))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("disk went away"), "{err}");
        assert!(store.pending_uploads().is_empty());
        assert!(store.keys("b").is_empty());
    }

    #[tokio::test]
    async fn retention_is_recorded_or_reported_as_manual() {
        let store = MockStore::new(ProviderKind::Aws);
        let applied = store
            .set_retention("b", "jamstream/recordings/", Retention::Days90)
            .await
            .unwrap();
        assert!(applied.is_server_side());
        assert_eq!(
            store.retention_for("b", "jamstream/recordings/"),
            Some(Retention::Days90)
        );

        let no_lifecycle = MockStore::new(ProviderKind::Local).without_lifecycle_support();
        let applied = no_lifecycle
            .set_retention("b", "p/", Retention::Days30)
            .await
            .unwrap();
        assert!(!applied.is_server_side());
        assert!(applied.describe().contains("cannot be enforced for you"));
        assert_eq!(no_lifecycle.retention_for("b", "p/"), None);
    }
}
