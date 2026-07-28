//! Chunk-at-a-time upload of one recording object.
//!
//! [`ObjectSink`] is the piece between the encoder and
//! [`crate::storage::ObjectStore::put_stream`]: chunks go in over the life of
//! a session, one object comes out at [`ObjectSink::finish`], and
//! [`ObjectSink::abort`] (or dropping the sink) abandons the upload, aborting
//! any multipart upload it opened. While an upload is in flight the sink
//! keeps a marker file under [`crate::cloudinit::UPLOAD_MARKER_DIR`], which
//! is what the VM's idle guard checks before self-destructing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::cloudinit::UPLOAD_MARKER_DIR;
use crate::provider::{ProviderError, Result};
use crate::storage::{ObjectMeta, ObjectStore, PartSource, sanitize_component};

/// Chunks queued ahead of the upload before `write` awaits. Parts buffer
/// downstream anyway; this only smooths bursts.
const CHUNK_QUEUE: usize = 16;

enum Msg {
    Chunk(Vec<u8>),
    Finish,
}

/// Feeds queued chunks to the upload driver, filling every part to `max` so
/// no non-final part goes out undersized.
struct ChannelSource {
    rx: mpsc::Receiver<Msg>,
    buf: Vec<u8>,
    finished: bool,
}

#[async_trait]
impl PartSource for ChannelSource {
    async fn next_part(&mut self, max: usize) -> Result<Vec<u8>> {
        while !self.finished && self.buf.len() < max {
            match self.rx.recv().await {
                Some(Msg::Chunk(chunk)) => self.buf.extend_from_slice(&chunk),
                Some(Msg::Finish) => self.finished = true,
                // The sink was dropped without finish(); erroring here is
                // what makes the driver abort the multipart upload instead
                // of completing a truncated object.
                None => {
                    return Err(ProviderError::Other(
                        "recording sink dropped mid-upload".to_owned(),
                    ));
                }
            }
        }
        let take = self.buf.len().min(max);
        Ok(self.buf.drain(..take).collect())
    }
}

/// Marker file the idle guard reads as "an upload is in flight". Removed on
/// drop.
struct UploadMarker {
    path: PathBuf,
}

impl UploadMarker {
    /// Best effort: a marker that cannot be written costs the guard's
    /// deferral, never the recording.
    fn create(dir: &Path, key: &str) -> Option<UploadMarker> {
        let path = dir.join(sanitize_component(key));
        match std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&path, [])) {
            Ok(()) => Some(UploadMarker { path }),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "cannot write the upload marker; the idle guard will not wait for this upload"
                );
                None
            }
        }
    }
}

impl Drop for UploadMarker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A streaming upload of one object: chunk writes, an explicit finish, and
/// abort on failure or abandonment.
pub struct ObjectSink {
    tx: Option<mpsc::Sender<Msg>>,
    task: JoinHandle<Result<ObjectMeta>>,
    marker: Option<UploadMarker>,
}

impl ObjectSink {
    /// Opens an upload of `key` into `bucket`, with the in-flight marker
    /// under [`UPLOAD_MARKER_DIR`]. Must be called on a Tokio runtime.
    pub fn open(
        store: Arc<dyn ObjectStore>,
        bucket: impl Into<String>,
        key: impl Into<String>,
        content_type: impl Into<String>,
    ) -> Self {
        Self::open_with_marker_dir(
            store,
            bucket,
            key,
            content_type,
            Path::new(UPLOAD_MARKER_DIR),
        )
    }

    /// [`ObjectSink::open`] with the marker directory supplied, for tests.
    pub fn open_with_marker_dir(
        store: Arc<dyn ObjectStore>,
        bucket: impl Into<String>,
        key: impl Into<String>,
        content_type: impl Into<String>,
        marker_dir: &Path,
    ) -> Self {
        let (bucket, key, content_type) = (bucket.into(), key.into(), content_type.into());
        let marker = UploadMarker::create(marker_dir, &key);
        let (tx, rx) = mpsc::channel(CHUNK_QUEUE);
        let task = tokio::spawn(async move {
            let mut source = ChannelSource {
                rx,
                buf: Vec::new(),
                finished: false,
            };
            store
                .put_stream(&bucket, &key, &content_type, &mut source)
                .await
        });
        ObjectSink {
            tx: Some(tx),
            task,
            marker,
        }
    }

    /// Queues one chunk, awaiting when the upload is behind. An error means
    /// the upload already failed and was aborted; [`ObjectSink::finish`]
    /// returns the reason.
    pub async fn write(&mut self, chunk: Vec<u8>) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        self.tx
            .as_ref()
            .expect("the sender lives until finish or abort")
            .send(Msg::Chunk(chunk))
            .await
            .map_err(|_| {
                ProviderError::Other(
                    "recording upload already failed and was aborted; finish() has the reason"
                        .to_owned(),
                )
            })
    }

    /// Sends the remaining bytes, commits the upload, and removes the
    /// marker. The error of a failed upload surfaces here; the upload itself
    /// was already aborted.
    pub async fn finish(mut self) -> Result<ObjectMeta> {
        if let Some(tx) = self.tx.take() {
            // Refused only when the task already stopped, and then the join
            // below carries the reason.
            let _ = tx.send(Msg::Finish).await;
        }
        let outcome = self
            .task
            .await
            .map_err(|e| ProviderError::Other(format!("recording upload task failed: {e}")))?;
        // Only now: the marker must outlive the store call it covers.
        self.marker.take();
        outcome
    }

    /// Abandons the upload. Any multipart upload it opened is aborted before
    /// this returns, and the marker is removed.
    pub async fn abort(mut self) {
        // Dropping the sender fails the source, which makes the driver
        // abort; see ChannelSource.
        self.tx.take();
        let _ = (&mut self.task).await;
        self.marker.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::{MockStore, StoreCall};
    use crate::storage::{FLAC_CONTENT_TYPE, mix_key};
    use crate::types::ProviderKind;
    use std::time::Duration;

    fn store(part_size: usize) -> Arc<MockStore> {
        Arc::new(MockStore::new(ProviderKind::Aws).with_part_size(part_size))
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("jamstream-sink-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sink(store: Arc<MockStore>, key: &str, dir: &Path) -> ObjectSink {
        ObjectSink::open_with_marker_dir(store, "b", key, FLAC_CONTENT_TYPE, dir)
    }

    #[tokio::test]
    async fn chunks_of_any_size_assemble_into_one_object() {
        let store = store(8);
        let dir = scratch("assemble");
        let key = mix_key("s1");
        let mut s = sink(store.clone(), &key, &dir);
        // 27 bytes in ragged chunks: smaller and larger than a part.
        let body: Vec<u8> = (0..27).collect();
        for chunk in [&body[..3], &body[3..20], &body[20..]] {
            s.write(chunk.to_vec()).await.unwrap();
        }
        s.write(Vec::new()).await.unwrap();
        let meta = s.finish().await.unwrap();
        assert_eq!(meta.size, 27);
        assert_eq!(store.body("b", &key).unwrap(), body);
        assert!(store.pending_uploads().is_empty());
        // Ragged writes still produce full 8-byte parts plus a tail.
        let sizes: Vec<usize> = store
            .calls()
            .iter()
            .filter_map(|c| match c {
                StoreCall::Part { size, .. } => Some(*size),
                _ => None,
            })
            .collect();
        assert_eq!(sizes, vec![8, 8, 8, 3]);
    }

    #[tokio::test]
    async fn a_short_recording_takes_the_single_put_path() {
        let store = store(64);
        let dir = scratch("short");
        let mut s = sink(store.clone(), "k.flac", &dir);
        s.write(b"tiny".to_vec()).await.unwrap();
        let meta = s.finish().await.unwrap();
        assert_eq!(meta.size, 4);
        assert!(
            store
                .calls()
                .iter()
                .all(|c| !matches!(c, StoreCall::Begin { .. })),
            "a one-part body must not open a multipart upload"
        );
    }

    #[tokio::test]
    async fn abort_aborts_the_multipart_upload_before_returning() {
        let store = store(4);
        let dir = scratch("abort");
        let mut s = sink(store.clone(), "k.flac", &dir);
        for chunk in [vec![1u8; 4], vec![2u8; 4], vec![3u8; 4]] {
            s.write(chunk).await.unwrap();
        }
        s.abort().await;
        // Settled by the time abort returns: the multipart upload was
        // opened, aborted, and nothing was stored or left pending.
        assert!(
            store
                .calls()
                .iter()
                .any(|c| matches!(c, StoreCall::Begin { .. }))
        );
        assert!(
            store
                .calls()
                .iter()
                .any(|c| matches!(c, StoreCall::Abort { .. }))
        );
        assert!(
            !store
                .calls()
                .iter()
                .any(|c| matches!(c, StoreCall::Complete { .. }))
        );
        assert!(store.pending_uploads().is_empty());
        assert!(store.keys("b").is_empty());
    }

    #[tokio::test]
    async fn dropping_the_sink_aborts_instead_of_completing_short() {
        let store = store(4);
        let dir = scratch("drop");
        let mut s = sink(store.clone(), "k.flac", &dir);
        for chunk in [vec![1u8; 4], vec![2u8; 4], vec![3u8; 4]] {
            s.write(chunk).await.unwrap();
        }
        drop(s);
        // The upload task finishes on its own; poll until it settles.
        for _ in 0..200 {
            if store
                .calls()
                .iter()
                .any(|c| matches!(c, StoreCall::Abort { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            store
                .calls()
                .iter()
                .any(|c| matches!(c, StoreCall::Abort { .. })),
            "a dropped sink completed or leaked its upload: {:?}",
            store.calls()
        );
        assert!(store.keys("b").is_empty(), "a truncated object was stored");
    }

    #[tokio::test]
    async fn an_upload_failure_surfaces_in_finish_and_is_aborted() {
        let store = store(4);
        store.fail_part(2);
        let dir = scratch("fail");
        let mut s = sink(store.clone(), "k.flac", &dir);
        for chunk in [vec![1u8; 4], vec![2u8; 4], vec![3u8; 4]] {
            // Failure may race the writes; the queue soaks them either way.
            let _ = s.write(chunk).await;
        }
        let err = s.finish().await.unwrap_err();
        assert!(err.to_string().contains("part 2"), "{err}");
        assert!(store.pending_uploads().is_empty());
        assert!(
            store
                .calls()
                .iter()
                .any(|c| matches!(c, StoreCall::Abort { .. }))
        );
    }

    #[tokio::test]
    async fn the_marker_covers_exactly_the_life_of_the_upload() {
        let store = store(8);
        let dir = scratch("marker");
        let key = mix_key("s1");
        let mut s = sink(store.clone(), &key, &dir);
        let marker = dir.join(sanitize_component(&key));
        assert!(marker.exists(), "the marker goes down before any byte");
        s.write(vec![7u8; 20]).await.unwrap();
        assert!(marker.exists());
        s.finish().await.unwrap();
        assert!(
            !marker.exists(),
            "a finished upload must not defer the guard"
        );

        // Aborting clears it too.
        let s = sink(store, &key, &dir);
        assert!(marker.exists());
        s.abort().await;
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn an_unwritable_marker_dir_costs_the_marker_not_the_recording() {
        let store = store(8);
        let dir = scratch("unwritable");
        // A file where the directory should be: create_dir_all fails.
        let not_a_dir = dir.join("occupied");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let mut s = ObjectSink::open_with_marker_dir(
            store.clone(),
            "b",
            "k.flac",
            FLAC_CONTENT_TYPE,
            &not_a_dir,
        );
        s.write(vec![1u8; 20]).await.unwrap();
        let meta = s.finish().await.unwrap();
        assert_eq!(meta.size, 20);
        assert_eq!(store.body("b", "k.flac").unwrap().len(), 20);
    }
}
