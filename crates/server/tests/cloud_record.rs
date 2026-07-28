//! The cloud half of a take: the real recorder drives the real streaming
//! uploader against a mock bucket, and the guard's marker contract holds.
//! The mock is only the far end; every byte passes through the same
//! encoder, sink bridge, and multipart driver a real launch uses.

use std::sync::Arc;
use std::time::{Duration, Instant};

use jamstream_cloud::storage::{MockStore, ObjectStore, RECORDING_PREFIX};
use jamstream_server::cloud_sink::CloudSink;
use jamstream_server::record::{RecordPayload, RecordWorker, RecordingState};

const BUCKET: &str = "my-jams";
const SESSION: &str = "abc123";

fn temp_marker_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jamstream-cloud-record-{tag}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
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
    // upload was committed rather than abandoned.
    let keys = store.keys(BUCKET);
    assert_eq!(keys.len(), 1, "one mix object: {keys:?}");
    let key = &keys[0];
    assert!(
        key.starts_with(&format!("{RECORDING_PREFIX}/{SESSION}/")) && key.ends_with("-mix.flac"),
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
