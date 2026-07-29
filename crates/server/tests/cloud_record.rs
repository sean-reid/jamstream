//! The cloud half of a take: the real recorder drives the real streaming
//! uploader against a mock bucket, and the guard's marker contract holds.
//! The mock is only the far end; every byte passes through the same
//! encoder, sink bridge, and multipart driver a real launch uses.

mod common;

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{scratch_dir, wait_for};

use jamstream_cloud::storage::{MockStore, ObjectStore, session_prefix};
use jamstream_server::cloud_sink::CloudSink;
use jamstream_server::record::{
    RecordPayload, RecordWorker, RecordingObject, RecordingSink, RecordingState,
};

const BUCKET: &str = "my-jams";
const SESSION: &str = "abc123";

fn temp_marker_dir(tag: &str) -> std::path::PathBuf {
    scratch_dir(&format!("cloud-record-{tag}"))
}

#[test]
fn a_take_streams_to_the_bucket_and_the_marker_clears() {
    let store = Arc::new(MockStore::new(jamstream_cloud::ProviderKind::Aws));
    let markers = temp_marker_dir("happy");
    let sink = CloudSink::over(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        BUCKET.to_owned(),
        SESSION,
        markers.clone(),
    )
    .unwrap();
    let worker = RecordWorker::spawn(sink).unwrap();

    worker.start(1_753_000_000, None);
    // While the take is open, an in-flight marker defers the dead man's
    // switch. The upload starts with the take, not at stop.
    wait_for("the in-flight marker", || {
        std::fs::read_dir(&markers).unwrap().count() > 0
    });

    let mut payload = Box::new(RecordPayload::default());
    payload.mix.fill(0.25);
    for _ in 0..400 {
        worker.submit_tick(payload.clone());
    }
    worker.stop();
    wait_for("the recorder to go idle", || {
        worker.state() == RecordingState::Idle && worker.gap_ticks() == 0
    });
    wait_for("the marker to clear", || {
        std::fs::read_dir(&markers).unwrap().count() == 0
    });

    // The object landed under the scoped prefix, named for people, and the
    // upload was committed rather than abandoned. The prefix comes from the
    // function `jamstream recordings` lists with, not from a second copy of
    // its format string: a take written somewhere the reader does not look is
    // a take the band cannot fetch.
    let keys = store.keys(BUCKET);
    assert_eq!(keys.len(), 1, "one mix object: {keys:?}");
    let key = &keys[0];
    assert!(
        key.starts_with(&session_prefix(SESSION)) && key.ends_with("-mix.flac"),
        "scoped and human-named: {key}"
    );
    assert!(store.pending_uploads().is_empty(), "nothing left in flight");
    let body = store.body(BUCKET, key).unwrap();
    assert!(!body.is_empty(), "the take has bytes");
    assert_eq!(&body[..4], b"fLaC", "it is a flac stream");

    std::fs::remove_dir_all(&markers).ok();
}

#[test]
fn a_failed_upload_aborts_and_surfaces_the_reason() {
    let store = Arc::new(MockStore::new(jamstream_cloud::ProviderKind::Aws).with_part_size(1024));
    store.fail_part(1);
    let markers = temp_marker_dir("failing");
    let sink = CloudSink::over(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        BUCKET.to_owned(),
        SESSION,
        markers.clone(),
    )
    .unwrap();
    let worker = RecordWorker::spawn(sink).unwrap();

    worker.start(1_753_000_000, None);
    // Noise, not a constant: FLAC squeezes DC below the 1 KiB part size and
    // the multipart whose first part is rigged to fail would never begin.
    let mut lcg: u32 = 1;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut payload = Box::new(RecordPayload::default());
        for slot in payload.mix.iter_mut() {
            lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *slot = (lcg as f32 / u32::MAX as f32) - 0.5;
        }
        worker.submit_tick(payload);
        if matches!(worker.state(), RecordingState::Failed { .. }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the failure never surfaced; state {:?}",
            worker.state()
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // Failure is visible with a reason, nothing was committed, the wreck
    // was aborted rather than left pending, and the marker is gone so the
    // machine is free to die.
    let RecordingState::Failed { reason } = worker.state() else {
        unreachable!()
    };
    assert!(!reason.is_empty());
    assert!(store.keys(BUCKET).is_empty(), "no committed object");
    wait_for("the abort to reclaim the upload", || {
        store.pending_uploads().is_empty()
    });
    wait_for("the marker to clear on failure", || {
        std::fs::read_dir(&markers).unwrap().count() == 0
    });

    std::fs::remove_dir_all(&markers).ok();
}

/// A sink that parks in `finish` until the test lets go, wrapping the real
/// one. The window where a take is draining is short against a mock bucket,
/// short enough that polling for it is a race the test always wins; holding
/// the finishing call open makes the state observable instead of lucky.
struct HeldSink {
    inner: CloudSink,
    hold: Arc<Mutex<mpsc::Receiver<()>>>,
}

struct HeldObject {
    inner: Box<dyn RecordingObject>,
    hold: Arc<Mutex<mpsc::Receiver<()>>>,
}

impl RecordingSink for HeldSink {
    fn open(&mut self, name: &str) -> std::io::Result<Box<dyn RecordingObject>> {
        Ok(Box::new(HeldObject {
            inner: self.inner.open(name)?,
            hold: Arc::clone(&self.hold),
        }))
    }

    fn uploads(&self) -> bool {
        self.inner.uploads()
    }
}

impl RecordingObject for HeldObject {
    fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.inner.write(chunk)
    }

    fn finish(self: Box<Self>) -> std::io::Result<()> {
        // Blocks until the test sends; a hung-up sender means proceed. The
        // timeout is a backstop: a failing assertion drops the worker, which
        // joins its thread, and waiting here forever would hang the suite
        // instead of failing it.
        let _ = self
            .hold
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(20));
        self.inner.finish()
    }

    fn abort(self: Box<Self>) -> std::io::Result<()> {
        self.inner.abort()
    }
}

/// #164: `Uploading` was in the wire protocol, rendered by the app, and
/// printed by the CLI, but nothing ever emitted it. The state exists to say
/// "the take ended and its bytes are still going to storage", which is only
/// true while the finishing call is running, so this test holds that call open
/// and requires the state rather than hoping to catch it.
#[test]
fn a_draining_take_reports_uploading_until_the_bucket_has_it() {
    let store = Arc::new(MockStore::new(jamstream_cloud::ProviderKind::Aws));
    let markers = temp_marker_dir("uploading");
    let inner = CloudSink::over(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        BUCKET.to_owned(),
        SESSION,
        markers.clone(),
    )
    .unwrap();
    assert!(inner.uploads(), "a bucket take is worth announcing");
    let (release, hold) = mpsc::channel::<()>();
    let worker = RecordWorker::spawn(HeldSink {
        inner,
        hold: Arc::new(Mutex::new(hold)),
    })
    .unwrap();

    worker.start(1_753_000_000, None);
    let mut payload = Box::new(RecordPayload::default());
    payload.mix.fill(0.2);
    for _ in 0..200 {
        worker.submit_tick(payload.clone());
    }
    wait_for("the take to be recording", || {
        matches!(worker.state(), RecordingState::Recording { .. })
    });

    // Stop. The finishing call is parked in the sink, so the room must be
    // told the take is uploading, and must stay told until it lands.
    worker.stop();
    wait_for("the take to report uploading", || {
        worker.state() == RecordingState::Uploading
    });
    assert!(
        store.keys(BUCKET).is_empty(),
        "nothing is committed while the drain is held"
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        worker.state(),
        RecordingState::Uploading,
        "the state holds for as long as the bytes are in flight"
    );

    // Let the tail go: the object commits, the state clears to Idle, and the
    // marker the dead man's switch reads disappears.
    drop(release);
    wait_for("the take to be committed", || store.keys(BUCKET).len() == 1);
    wait_for("the recorder to go idle", || {
        worker.state() == RecordingState::Idle
    });
    wait_for("the marker to clear", || {
        std::fs::read_dir(&markers).unwrap().count() == 0
    });
    std::fs::remove_dir_all(&markers).ok();
}

/// A disk take must never claim to be uploading: it finishes locally, and
/// telling the room otherwise would be a lie about where their music is.
#[test]
fn a_disk_take_never_reports_uploading() {
    use jamstream_server::record::DiskSink;
    let dir = temp_marker_dir("disk-no-upload");
    let worker = RecordWorker::spawn(DiskSink::new(&dir)).unwrap();
    worker.start(1_753_000_000, None);
    let mut payload = Box::new(RecordPayload::default());
    payload.mix.fill(0.2);
    for _ in 0..100 {
        worker.submit_tick(payload.clone());
    }
    worker.stop();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = worker.state();
        assert_ne!(
            state,
            RecordingState::Uploading,
            "a disk sink has nothing to upload"
        );
        if state == RecordingState::Idle {
            break;
        }
        assert!(Instant::now() < deadline, "the disk take never finished");
    }
    std::fs::remove_dir_all(&dir).ok();
}
