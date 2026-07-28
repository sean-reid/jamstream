//! `jamstream recordings` against a real S3 client and a fake bucket.
//!
//! The far end is a wiremock server answering ListObjectsV2 and GetObject, so
//! the listing, the signing, the pagination and the streaming download are the
//! shipped code; only the network is fake. The mock never sees a
//! `jamstream_cli` type, so a test cannot pass by agreeing with itself: the
//! assertions compare what landed on disk against the bytes the fake bucket
//! held.
//!
//! The state directory is process-global environment, so every test in this
//! file uses one it owns and no test may mutate the variable.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jamstream_cli::cli::{RecordingsGetArgs, RecordingsListArgs};
use jamstream_cli::state::RecordingRecord;
use jamstream_cli::storage::Stores;
use jamstream_cli::{CliError, recordings, state};
use jamstream_cloud::{ObjectStore, S3Store, session_prefix};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BUCKET: &str = "my-jams";
const SESSION: &str = "deadbeefcafef00ddeadbeefcafef00d";

/// Points the real S3 store at the mock server, path style.
struct MockBucket {
    uri: String,
}

impl Stores for MockBucket {
    fn open(&self, record: &RecordingRecord) -> Result<Arc<dyn ObjectStore>, CliError> {
        Ok(Arc::new(
            S3Store::aws(
                record.region.clone(),
                "AKIDTEST".to_owned(),
                "test-secret-key".to_owned(),
            )
            .with_base_url(&self.uri),
        ))
    }
}

/// A state directory of this test's own, with one session record in it and
/// bucket details beside it.
struct Machine {
    dir: PathBuf,
}

impl Machine {
    fn new(name: &str) -> Machine {
        let dir = std::env::temp_dir().join(format!(
            "jamstream-cli-recordings-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let machine = Machine { dir };
        machine.write_session(SESSION);
        machine
    }

    fn write_session(&self, session_id_hex: &str) {
        let state = state::SessionState {
            session_id_hex: session_id_hex.to_owned(),
            provider: "aws".to_owned(),
            region: "eu-west-1".to_owned(),
            instance_id: "i-0123456789".to_owned(),
            address: "203.0.113.7:43210".to_owned(),
            created_unix: 1_784_000_000,
            hourly_microusd: 16_800,
            issuer_private_key_b64: String::new(),
            server_public_key_b64: "c2VydmVy".to_owned(),
            invites: Vec::new(),
            status: state::SessionStatus::Ended,
            ended_unix: Some(1_784_007_200),
        };
        state::write_to(&self.dir.join(format!("{session_id_hex}.json")), &state).unwrap();
        state::write_recording_to(
            &self
                .dir
                .join("buckets")
                .join(format!("{session_id_hex}.json")),
            &RecordingRecord {
                provider: "aws".to_owned(),
                bucket: BUCKET.to_owned(),
                region: "eu-west-1".to_owned(),
                retention: "30d".to_owned(),
                stems: true,
            },
        )
        .unwrap();
    }

    /// Points the state directory at this machine's.
    ///
    /// Safety: this crate's integration tests run one binary per file and this
    /// file's tests are serialized by the mutex below, so nothing else reads
    /// the variable while it is set.
    fn enter(&self) {
        unsafe { std::env::set_var(state::STATE_DIR_ENV, &self.dir) };
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Serializes the tests, which share the state directory variable. Async
/// aware, because every test holds it across a request.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One take in the fake bucket.
struct Object {
    name: &'static str,
    body: Vec<u8>,
}

fn objects() -> Vec<Object> {
    Object::pair()
}

impl Object {
    /// A mix and a stem, both larger than one HTTP chunk is likely to be, and
    /// distinguishable byte for byte.
    fn pair() -> Vec<Object> {
        vec![
            Object {
                name: "mix.flac",
                body: (0..40_000u32).map(|i| (i % 251) as u8).collect(),
            },
            Object {
                name: "stems/bass.flac",
                body: (0..9_000u32).map(|i| (i % 97) as u8).collect(),
            },
        ]
    }
}

/// Mounts ListObjectsV2 for the session prefix plus a GET per object.
///
/// `listed_sizes` is what the listing claims, which is normally the real body
/// length; the truncation test claims more than it serves.
async fn mount_bucket(server: &MockServer, objects: &[Object], listed_sizes: &[u64]) {
    let prefix = session_prefix(SESSION);
    let mut xml = String::from("<ListBucketResult><IsTruncated>false</IsTruncated>");
    for (object, size) in objects.iter().zip(listed_sizes) {
        xml.push_str(&format!(
            "<Contents><Key>{prefix}{}</Key><Size>{size}</Size><ETag>&quot;e-{size}&quot;</ETag>\
             <LastModified>2026-07-28T19:30:02.000Z</LastModified></Contents>",
            object.name
        ));
    }
    xml.push_str("</ListBucketResult>");
    Mock::given(method("GET"))
        .and(path(format!("/{BUCKET}")))
        .and(query_param("list-type", "2"))
        .and(query_param("prefix", prefix.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_string(xml))
        .mount(server)
        .await;
    for object in objects {
        Mock::given(method("GET"))
            .and(path(format!("/{BUCKET}/{prefix}{}", object.name)))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(object.body.clone())
                    .insert_header("etag", "\"e\""),
            )
            .mount(server)
            .await;
    }
}

fn real_sizes(objects: &[Object]) -> Vec<u64> {
    objects.iter().map(|o| o.body.len() as u64).collect()
}

async fn list_output(stores: &dyn Stores, json: bool) -> String {
    let mut out = Vec::new();
    recordings::list(&RecordingsListArgs { json }, stores, &mut out)
        .await
        .unwrap();
    String::from_utf8(out).unwrap()
}

/// How the egress question gets answered, since a test has no terminal.
#[derive(Clone, Copy)]
enum Answer {
    Yes,
    No,
    /// Asking at all is the failure: this is what proves `--yes` skips the
    /// prompt rather than answering it.
    Never,
}

/// Downloads with the question answered by `answer`, and returns everything
/// printed.
async fn get_output(
    stores: &dyn Stores,
    args: &RecordingsGetArgs,
    answer: Answer,
) -> Result<String, CliError> {
    let mut out: Vec<u8> = Vec::new();
    let mut confirm = |_: &mut Vec<u8>| match answer {
        Answer::Yes => Ok(true),
        Answer::No => Ok(false),
        Answer::Never => panic!("--yes must not ask before spending egress"),
    };
    let mut prompt = recordings::Prompt {
        terminal: false,
        confirm: &mut confirm,
    };
    let outcome = recordings::get(args, stores, &mut prompt, &mut out).await;
    let text = String::from_utf8(out).unwrap();
    outcome.map(|()| text)
}

fn get_args(dir: &Path, yes: bool) -> RecordingsGetArgs {
    RecordingsGetArgs {
        session: "deadbeef".to_owned(),
        out: Some(dir.to_path_buf()),
        yes,
    }
}

#[tokio::test]
async fn the_listing_names_every_take_with_its_size_and_time() {
    let _serial = SERIAL.lock().await;
    let machine = Machine::new("list");
    machine.enter();
    let server = MockServer::start().await;
    let objects = objects();
    mount_bucket(&server, &objects, &real_sizes(&objects)).await;
    let stores = MockBucket { uri: server.uri() };

    let text = list_output(&stores, false).await;
    assert!(text.contains("SESSION"), "{text}");
    assert!(text.contains("deadbeef"), "{text}");
    assert!(text.contains("mix.flac"), "{text}");
    assert!(text.contains("stems/bass.flac"), "{text}");
    // 40 000 bytes in the units a bucket bills in, and the time trimmed to
    // the minute.
    assert!(text.contains("40.0 KB"), "{text}");
    assert!(text.contains("9.0 KB"), "{text}");
    assert!(text.contains("2026-07-28 19:30"), "{text}");
    assert!(text.contains("jamstream recordings get"), "{text}");

    let json = list_output(&stores, true).await;
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value[0]["session_id"], SESSION);
    assert_eq!(value[0]["bucket"], BUCKET);
    assert_eq!(value[0]["total_bytes"], 49_000);
    assert_eq!(value[0]["takes"][0]["name"], "mix.flac");
    assert_eq!(value[0]["takes"][0]["bytes"], 40_000);
    assert_eq!(value[0]["takes"][1]["name"], "stems/bass.flac");
    assert!(value[0]["error"].is_null(), "{json}");
}

#[tokio::test]
async fn a_session_with_nothing_in_the_bucket_says_so() {
    let _serial = SERIAL.lock().await;
    let machine = Machine::new("empty");
    machine.enter();
    let server = MockServer::start().await;
    mount_bucket(&server, &[], &[]).await;
    let stores = MockBucket { uri: server.uri() };

    let text = list_output(&stores, false).await;
    assert!(text.contains("no takes"), "{text}");
    assert!(
        !text.contains("SESSION"),
        "an empty table header reads as a take of unknown size: {text}"
    );

    // And get refuses rather than pretending to download nothing.
    let dir = machine.dir.join("out");
    let err = get_output(&stores, &get_args(&dir, true), Answer::Never)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no takes"), "{err}");
}

#[tokio::test]
async fn get_prices_the_egress_and_writes_exactly_what_the_bucket_held() {
    let _serial = SERIAL.lock().await;
    let machine = Machine::new("get");
    machine.enter();
    let server = MockServer::start().await;
    let objects = objects();
    mount_bucket(&server, &objects, &real_sizes(&objects)).await;
    let stores = MockBucket { uri: server.uri() };
    let dir = machine.dir.join("out");

    // Answered at the prompt rather than with --yes, which is the path a
    // person takes.
    let text = get_output(&stores, &get_args(&dir, false), Answer::Yes)
        .await
        .unwrap();
    // The cost rule: the size, the rate, and where the charge falls, before
    // a byte moves. 49 000 bytes at $0.09/GB rounds to nothing, so the rate
    // and the reason are what matter here; the arithmetic is pinned in
    // jamstream_cloud::recording.
    assert!(text.contains("$0.09/GB"), "{text}");
    assert!(
        text.contains("Egress is billed on the download"),
        "the prompt has to say when the charge lands: {text}"
    );
    assert!(text.contains("2 takes"), "{text}");
    // Progress readable in a pipe, with no carriage returns.
    assert!(text.contains("100%"), "{text}");
    assert!(!text.contains('\r'), "{text}");

    for object in &objects {
        let landed = std::fs::read(dir.join(object.name)).unwrap();
        assert_eq!(
            landed, object.body,
            "{} is not the bytes the bucket held",
            object.name
        );
    }

    // Running it again with --yes spends no egress and asks nothing: what is
    // already here is skipped.
    let again = get_output(&stores, &get_args(&dir, true), Answer::Never)
        .await
        .unwrap();
    assert!(again.contains("already here"), "{again}");
    assert!(again.contains("Every take is already in"), "{again}");
}

#[tokio::test]
async fn declining_the_cost_downloads_nothing() {
    let _serial = SERIAL.lock().await;
    let machine = Machine::new("decline");
    machine.enter();
    let server = MockServer::start().await;
    let objects = objects();
    mount_bucket(&server, &objects, &real_sizes(&objects)).await;
    let stores = MockBucket { uri: server.uri() };
    let dir = machine.dir.join("out");

    let text = get_output(&stores, &get_args(&dir, false), Answer::No)
        .await
        .unwrap();
    assert!(text.contains("Aborted. Nothing was downloaded."), "{text}");
    assert!(
        !dir.join("mix.flac").exists(),
        "a declined download wrote a file"
    );
    // Nothing but the listing was ever requested.
    let gets = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("mix.flac"))
        .count();
    assert_eq!(gets, 0, "the object was fetched before the question");
}

/// The defect this guards: a take that arrives short must not be left on disk
/// looking like a recording. The listing claims more bytes than the bucket
/// serves, which is what a half-delivered download looks like from here.
#[tokio::test]
async fn a_take_that_arrives_short_is_refused_and_the_partial_file_removed() {
    let _serial = SERIAL.lock().await;
    let machine = Machine::new("short");
    machine.enter();
    let server = MockServer::start().await;
    let objects = objects();
    let mut claimed = real_sizes(&objects);
    claimed[0] += 4_096;
    mount_bucket(&server, &objects, &claimed).await;
    let stores = MockBucket { uri: server.uri() };
    let dir = machine.dir.join("out");

    let err = get_output(&stores, &get_args(&dir, true), Answer::Never)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("truncated"), "{err}");
    assert!(err.contains("mix.flac"), "{err}");
    assert!(
        !dir.join("mix.flac").exists(),
        "the short file was left behind at {}",
        dir.display()
    );
}

#[tokio::test]
async fn an_unknown_session_and_one_that_recorded_nowhere_read_differently() {
    let _serial = SERIAL.lock().await;
    let machine = Machine::new("select");
    machine.enter();
    // A second session with no bucket beside it: local takes are already on
    // this machine, and saying "no such session" would be wrong.
    let local = "0000111122223333444455556666777";
    let state = state::SessionState {
        session_id_hex: local.to_owned(),
        provider: "local".to_owned(),
        region: "local".to_owned(),
        instance_id: "12345".to_owned(),
        address: "127.0.0.1:43210".to_owned(),
        created_unix: 1_784_000_100,
        hourly_microusd: 0,
        issuer_private_key_b64: String::new(),
        server_public_key_b64: "c2VydmVy".to_owned(),
        invites: Vec::new(),
        status: state::SessionStatus::Ended,
        ended_unix: Some(1_784_000_200),
    };
    state::write_to(&machine.dir.join(format!("{local}.json")), &state).unwrap();

    let server = MockServer::start().await;
    let stores = MockBucket { uri: server.uri() };
    let dir = machine.dir.join("out");

    let err = get_output(
        &stores,
        &RecordingsGetArgs {
            session: "00001111".to_owned(),
            out: Some(dir.clone()),
            yes: true,
        },
        Answer::Never,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("recorded to no bucket"), "{err}");

    let err = get_output(
        &stores,
        &RecordingsGetArgs {
            session: "nosuchsession".to_owned(),
            out: Some(dir),
            yes: true,
        },
        Answer::Never,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("no recorded session matches"), "{err}");
}
