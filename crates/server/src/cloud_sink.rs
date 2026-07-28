//! Bridges the recorder's synchronous sink to the async object store: the
//! recorder thread calls blocking writes, a single-worker Tokio runtime
//! owned here drives the upload, and the in-flight marker each open sink
//! drops under /run/jamstream/uploads is what defers the dead man's switch.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use jamstream_cloud::ObjectSink;
use jamstream_cloud::cloudinit::RecordingStorage;
use jamstream_cloud::cloudinit::UPLOAD_MARKER_DIR;
use jamstream_cloud::storage::{ObjectStore, RECORDING_PREFIX};

use crate::record::{RecordingObject, RecordingSink};

fn to_io(err: impl std::fmt::Display) -> io::Error {
    io::Error::other(err.to_string())
}

/// A [`RecordingSink`] that streams each take to a bucket while the session
/// plays, so teardown has only the tail to wait for.
pub struct CloudSink {
    /// One worker: the upload is one ordered byte stream, and the recorder
    /// thread is the producer. Owned here so a slow bucket never couples to
    /// the server's own runtime.
    rt: tokio::runtime::Runtime,
    store: Arc<dyn ObjectStore>,
    bucket: String,
    /// `jamstream/recordings/<session>`; the write-only key the provider
    /// docs describe is scoped to exactly this.
    prefix: String,
    marker_dir: PathBuf,
}

impl CloudSink {
    pub fn new(storage: &RecordingStorage, session_hex: &str) -> io::Result<CloudSink> {
        Self::with_marker_dir(storage, session_hex, PathBuf::from(UPLOAD_MARKER_DIR))
    }

    /// [`CloudSink::new`] with the marker directory supplied, for tests.
    pub fn with_marker_dir(
        storage: &RecordingStorage,
        session_hex: &str,
        marker_dir: PathBuf,
    ) -> io::Result<CloudSink> {
        let store = storage.object_store().map_err(to_io)?;
        Self::over(store, storage.bucket.clone(), session_hex, marker_dir)
    }

    /// The store supplied directly, so a test drives the real sink and
    /// marker machinery against a mock bucket.
    pub fn over(
        store: Arc<dyn ObjectStore>,
        bucket: String,
        session_hex: &str,
        marker_dir: PathBuf,
    ) -> io::Result<CloudSink> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("jamstream-upload")
            .enable_all()
            .build()?;
        Ok(CloudSink {
            rt,
            store,
            bucket,
            prefix: format!("{RECORDING_PREFIX}/{session_hex}"),
            marker_dir,
        })
    }
}

impl RecordingSink for CloudSink {
    /// Finishing here waits on the bucket, so the room is told the take is
    /// uploading rather than left watching a stopped session that is not
    /// done yet.
    fn uploads(&self) -> bool {
        true
    }

    fn open(&mut self, name: &str) -> io::Result<Box<dyn RecordingObject>> {
        // ObjectSink spawns its driver task, which needs the runtime
        // entered; the task then runs on the worker thread regardless of
        // who blocks where.
        let _guard = self.rt.enter();
        let sink = ObjectSink::open_with_marker_dir(
            Arc::clone(&self.store),
            self.bucket.clone(),
            format!("{}/{name}", self.prefix),
            "audio/flac",
            &self.marker_dir,
        );
        Ok(Box::new(CloudObject {
            handle: self.rt.handle().clone(),
            sink: Some(sink),
        }))
    }
}

struct CloudObject {
    handle: tokio::runtime::Handle,
    sink: Option<ObjectSink>,
}

impl RecordingObject for CloudObject {
    fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
        let sink = self.sink.as_mut().expect("written after finish");
        self.handle
            .block_on(sink.write(chunk.to_vec()))
            .map_err(to_io)
    }

    fn finish(mut self: Box<Self>) -> io::Result<()> {
        let sink = self.sink.take().expect("finished twice");
        self.handle
            .block_on(sink.finish())
            .map(|_| ())
            .map_err(to_io)
    }

    fn abort(mut self: Box<Self>) -> io::Result<()> {
        let sink = self.sink.take().expect("aborted twice");
        self.handle.block_on(sink.abort());
        Ok(())
    }
}

impl Drop for CloudObject {
    /// A dropped object without finish is an abort: no crash path may
    /// commit a truncated take, and the marker must not outlive the upload.
    fn drop(&mut self) {
        if let Some(sink) = self.sink.take() {
            self.handle.block_on(sink.abort());
        }
    }
}
