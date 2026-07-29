//! The Takes screen against a bucket that really holds takes.
//!
//! Every assertion here goes through the shipped path: the CLI's listing, its
//! plan, its download engine, and its size check, driven by the screen's own
//! methods on the app's own executor. Nothing in here stands in for the part
//! being tested, which is why the store is seeded with real bytes and the
//! files on disk are read back and compared.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jamstream_cli::CliError;
use jamstream_cli::state::{
    RecordingRecord, RetentionApplied, SessionState, SessionStatus, recording_path_for,
};
use jamstream_cli::storage::Stores;
use jamstream_client::creds::{EnvReader, MemStore};
use jamstream_client::exec::Executor;
use jamstream_client::screens::takes::{
    Half, KeychainStores, SessionTakes, TakesScreen, rows_from,
};
use jamstream_cloud::{FLAC_CONTENT_TYPE, MockStore, ObjectStore, ProviderKind, session_prefix};

const SESSION: &str = "a3f29c41deadbeefa3f29c41deadbeef";
const BUCKET: &str = "our-takes";
/// One take, in the shape the recorder writes: a mix and one stem per player.
const MIX: &str = "jamstream-2026-07-28-1930-mix.flac";
const STEM: &str = "jamstream-2026-07-28-1930-Ana.flac";

fn seeded_store() -> Arc<MockStore> {
    let store = Arc::new(MockStore::new(ProviderKind::DigitalOcean));
    let prefix = session_prefix(SESSION);
    let rt = tokio::runtime::Runtime::new().expect("a runtime to seed with");
    for (name, byte, len) in [(MIX, 1u8, 3_000usize), (STEM, 2u8, 5_000usize)] {
        let body = vec![byte; len];
        rt.block_on(store.put(BUCKET, &format!("{prefix}{name}"), FLAC_CONTENT_TYPE, &body))
            .expect("seed the bucket");
    }
    store
}

/// A bucket already open, which is what the keychain store returns once a key
/// has been found. The seam the CLI's own integration tests use for wiremock.
struct OpenStore(Arc<MockStore>);

impl Stores for OpenStore {
    fn open(&self, _record: &RecordingRecord) -> Result<Arc<dyn ObjectStore>, CliError> {
        Ok(Arc::clone(&self.0) as Arc<dyn ObjectStore>)
    }
}

fn session() -> SessionState {
    SessionState {
        session_id_hex: SESSION.to_owned(),
        provider: "digitalocean".to_owned(),
        region: "sfo3".to_owned(),
        instance_id: "droplet-1".to_owned(),
        address: "203.0.113.7:43210".to_owned(),
        created_unix: 1_785_264_000,
        hourly_microusd: 16_800,
        issuer_private_key_b64: String::new(),
        server_public_key_b64: "c2VydmVy".to_owned(),
        invites: Vec::new(),
        status: SessionStatus::Ended,
        ended_unix: Some(1_785_266_820),
    }
}

fn record() -> RecordingRecord {
    RecordingRecord {
        provider: "digitalocean".to_owned(),
        bucket: BUCKET.to_owned(),
        region: "sfo3".to_owned(),
        retention: "30d".to_owned(),
        stems: true,
        applied: Some(RetentionApplied::ServerSide),
    }
}

/// Rows as the app builds them, pointed at a scratch folder so a download
/// lands somewhere a test owns rather than in this machine's music folder.
fn rows(dir: &Path) -> Vec<SessionTakes> {
    rows_from(
        &[(session(), Some(record()))],
        &[],
        Path::new("/nowhere"),
        dir,
        1_785_270_000,
    )
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jamstream-takes-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Runs the screen's own poll until `done`, the way the frame loop does.
fn settle(screen: &mut TakesScreen, what: &str, done: impl Fn(&TakesScreen) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(screen) {
        screen.poll();
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The whole point of #237: a take in a bucket is listed, priced, fetched onto
/// this computer, and from then on shown rather than fetched again.
#[test]
fn a_take_is_listed_then_fetched_and_then_shown_where_it_landed() {
    let dir = scratch("fetch");
    let store = seeded_store();
    let mut screen = TakesScreen::with_stores(
        Arc::new(OpenStore(Arc::clone(&store))),
        Arc::new(Executor::new()),
    );
    screen.load(rows(&dir));
    settle(&mut screen, "the listing", |s| !s.rows[0].listing);

    let row = &screen.rows[0];
    assert_eq!(row.error, None);
    assert_eq!(row.takes.len(), 1, "one Record to Stop is one row");
    let take = &row.takes[0];
    assert_eq!(take.at(), "19:30 UTC");
    assert_eq!(take.mix.bytes(), 3_000);
    assert_eq!(take.stems.bytes(), 5_000);
    // Nothing is here yet, so both halves would cost egress.
    assert_eq!(take.mix.missing_bytes(), 3_000);
    assert!(take.mix.here().is_none());
    let base = take.base.clone();

    assert!(screen.begin_download(SESSION, &base, Half::Mix));
    // One download at a time: the second is refused rather than queued behind
    // the first, so the bar on screen is always the download that is running.
    assert!(!screen.begin_download(SESSION, &base, Half::Stems));
    settle(&mut screen, "the download", |s| s.landed.is_some());

    let landed = screen.landed.as_ref().expect("a download to have landed");
    assert_eq!(landed.half, Half::Mix);
    let mix = dir
        .join(SESSION.chars().take(8).collect::<String>())
        .join(MIX);
    assert_eq!(
        landed.result.as_deref().map(Path::to_path_buf),
        Ok(mix.parent().expect("a folder").to_owned())
    );
    // The bytes the bucket held, not a truncated file that looks like a take.
    assert_eq!(
        std::fs::read(&mix).expect("the mix on disk"),
        vec![1u8; 3_000]
    );
    assert!(
        !mix.with_file_name(STEM).exists(),
        "the stems were not asked for"
    );

    // And the row has noticed: the mix is a Reveal now, the stems are still a
    // download.
    let take = &screen.rows[0].takes[0];
    assert_eq!(take.mix.missing_bytes(), 0);
    assert_eq!(take.mix.here(), Some(mix.as_path()));
    assert_eq!(take.stems.missing_bytes(), 5_000);

    assert!(screen.begin_download(SESSION, &base, Half::Stems));
    settle(&mut screen, "the stems", |s| s.landed.is_some());
    assert_eq!(
        std::fs::read(mix.with_file_name(STEM)).expect("the stem on disk"),
        vec![2u8; 5_000]
    );
    std::fs::remove_dir_all(&dir).expect("clean up");
}

/// A key that is not on this computer stops the listing with the reason that
/// says where to put one, and never with a shell variable: the app is where
/// the bucket was set up.
#[test]
fn with_no_key_in_the_keychain_the_row_says_which_tab_takes_one() {
    let dir = scratch("nokey");
    let env: EnvReader = Arc::new(|_| None);
    let mut screen = TakesScreen::with_stores(
        Arc::new(KeychainStores::new(
            Arc::new(MemStore::default()),
            Arc::clone(&env),
        )),
        Arc::new(Executor::new()),
    );
    screen.load(rows(&dir));
    settle(&mut screen, "the listing", |s| !s.rows[0].listing);
    let err = screen.rows[0].error.clone().expect("a refusal");
    assert!(err.contains("Recording tab"), "{err}");
    assert!(screen.rows[0].takes.is_empty());
}

/// A bucket that refuses is one session's problem and not the screen's: the
/// reason lands on that row.
#[test]
fn a_bucket_that_holds_nothing_is_a_row_with_no_takes() {
    let dir = scratch("empty");
    let empty = Arc::new(MockStore::new(ProviderKind::DigitalOcean));
    let mut screen =
        TakesScreen::with_stores(Arc::new(OpenStore(empty)), Arc::new(Executor::new()));
    screen.load(rows(&dir));
    settle(&mut screen, "the listing", |s| !s.rows[0].listing);
    assert_eq!(screen.rows[0].error, None);
    assert!(screen.rows[0].takes.is_empty());
    // Nothing was downloaded, so nothing was written.
    assert!(!dir.exists());
}

/// The sidecar the launch writes has to carry what the bucket did with the
/// retention rule, because the Takes screen reads it back weeks later and a
/// countdown for a deletion nothing will perform is the thing the retention
/// module says not to draw.
#[test]
fn the_bucket_sidecar_round_trips_what_the_retention_call_answered() {
    let dir = scratch("sidecar");
    std::fs::create_dir_all(&dir).expect("create");
    let path = dir.join("bucket.json");
    let unenforced = RecordingRecord {
        applied: Some(RetentionApplied::Unenforced {
            note: "this target has no lifecycle API".to_owned(),
        }),
        ..record()
    };
    jamstream_cli::state::write_recording_to(&path, &unenforced).expect("write");
    let read = jamstream_cli::state::read_recording_at(&path)
        .expect("read")
        .expect("a record");
    assert_eq!(read, unenforced);
    // A record from before the field existed reads as an unknown answer, which
    // is not the same as "nothing is enforcing it".
    std::fs::write(
        &path,
        br#"{"provider":"aws","bucket":"b","region":"eu-west-1","retention":"30d","stems":false}"#,
    )
    .expect("write");
    let old = jamstream_cli::state::read_recording_at(&path)
        .expect("read")
        .expect("a record");
    assert_eq!(old.applied, None);
    // And the path is still the sidecar beside the session records.
    if let Ok(resolved) = recording_path_for(SESSION) {
        assert!(resolved.ends_with(format!("buckets/{SESSION}.json")));
    }
    std::fs::remove_dir_all(&dir).expect("clean up");
}
