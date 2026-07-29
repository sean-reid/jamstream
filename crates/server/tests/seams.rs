//! Seams between halves that were built in separate crates and, until this
//! file, were only held together by comments claiming they agreed.
//!
//! Every assertion here spans a crate boundary that nothing else spans. The
//! server crate is the one place that sees the recording cost model
//! (jamstream-cloud), the encoder that decides what a take actually weighs
//! (this crate), and the stream pipeline's idea of the VM's filesystem
//! (jamstream-stream) at the same time. A test inside any one of them can
//! only agree with its own copy of the other's facts, which is how the cost
//! model came to quote mono WAV for a stereo FLAC take.

use jamstream_cloud::cloudinit::{
    ACTIVITY_FILE, BootConfig, RECORDING_CONFIG_PATH, STREAM_KEY_DIR, SelfDestruct,
};
use jamstream_cloud::recording::{BitDepth, MIX_CHANNELS, RecordingPlan, SAMPLE_RATE_HZ};
use jamstream_server::flac::{BITS_PER_SAMPLE, CHANNELS, FlacEncoder, SAMPLE_RATE};

/// The cost model prices a take by multiplying out a sample rate, a channel
/// count and a bit depth. The encoder writes a STREAMINFO header saying what
/// it really used. Decode that header with an independent implementation and
/// compare: this is the assertion #164 item 4 needed and did not have.
#[test]
fn the_cost_model_prices_the_format_the_encoder_writes() {
    let mut enc = FlacEncoder::new();
    let mut bytes = enc.header().expect("header");
    // A second of stereo, so the header is followed by real frames.
    let frames = SAMPLE_RATE;
    let signal: Vec<f32> = (0..frames * CHANNELS)
        .map(|i| ((i / CHANNELS) as f32 * 0.01).sin() * 0.5)
        .collect();
    enc.push(&signal, &mut bytes).expect("push");
    enc.finish(&mut bytes).expect("finish");

    let reader = claxon::FlacReader::new(std::io::Cursor::new(&bytes)).expect("decodes");
    let info = reader.streaminfo();

    assert_eq!(
        u64::from(info.sample_rate),
        SAMPLE_RATE_HZ,
        "the estimate's sample rate is not the one in the take"
    );
    assert_eq!(
        u64::from(info.channels),
        MIX_CHANNELS,
        "the estimate's channel count is not the one in the take"
    );
    assert_eq!(
        info.bits_per_sample, BITS_PER_SAMPLE as u32,
        "the encoder's own constant and its header disagree"
    );
    // The plan a default launch prices, against the same header.
    let plan = RecordingPlan::mix_only();
    assert_eq!(plan.bit_depth, BitDepth::Sixteen);
    assert_eq!(
        u64::from(info.bits_per_sample) / 8,
        plan.bit_depth.bytes_per_sample(),
        "the estimate would size samples the encoder does not write"
    );
    // The uncompressed basis the estimate reasons about, in bytes, is what a
    // second of this take would be without the codec. Exact, not a ratio:
    // FLAC_PERCENT_OF_PCM is deliberately an estimate, the arithmetic under
    // it is not.
    let pcm_bytes_per_second = SAMPLE_RATE_HZ * MIX_CHANNELS * plan.bit_depth.bytes_per_sample();
    assert_eq!(
        pcm_bytes_per_second,
        (SAMPLE_RATE * CHANNELS * (BITS_PER_SAMPLE / 8)) as u64
    );
    // And a whole hour of mix is that, times the codec estimate: the numbers
    // a host would read before agreeing to spend money.
    assert_eq!(
        plan.mix_bytes_per_hour(),
        pcm_bytes_per_second * 3600 * 60 / 100
    );
}

/// The VM's filesystem layout is written by cloud-init in jamstream-cloud and
/// used by two other crates. jamstream-stream cannot see jamstream-cloud, and
/// jamstreamd's activity file has no command-line flag on a cloud launch, so
/// nothing but this test puts the writer and the readers in one room.
#[test]
fn the_boot_script_creates_the_paths_the_processes_use() {
    let script = jamstream_cloud::cloudinit::render(&boot_config());

    // The dead man's switch. jamstreamd touches this path by default and the
    // guard stats it; the bootstrap has to create it, owned by the service
    // account, or the guard reads one mtime forever and destroys a session
    // with musicians playing on it.
    assert!(
        script.contains(&format!(
            "install -o jamstream -g jamstream -m 0644 /dev/null {ACTIVITY_FILE}"
        )),
        "bootstrap must create the activity file jamstreamd defaults to"
    );
    assert!(
        script.contains(&format!("stat -c %Y {ACTIVITY_FILE}")),
        "the guard must stat the same path"
    );

    // Stream keys. StreamConfig names this directory from a crate that cannot
    // import the constant, so a pusher whose key file is unreadable fails
    // every destination.
    let cfg = jamstream_stream::pipeline::StreamConfig::default();
    assert_eq!(
        cfg.key_dir,
        std::path::Path::new(STREAM_KEY_DIR),
        "the pipeline stages keys somewhere the bootstrap did not create"
    );
    assert!(
        script.contains(&format!(
            "install -d -o jamstream -g jamstream -m 0700 {STREAM_KEY_DIR}"
        )),
        "bootstrap must create the key directory 0700"
    );
}

/// A recording launch carries a second config file. jamstreamd reads it at
/// the path this constant names, and refuses to record without it.
#[test]
fn a_recording_launch_writes_the_config_jamstreamd_reads() {
    let mut cfg = boot_config();
    cfg.recording = Some(jamstream_cloud::cloudinit::RecordingStorage {
        provider: jamstream_cloud::ProviderKind::Aws,
        bucket: "my-jams".to_owned(),
        region: "us-east-1".to_owned(),
        retention: jamstream_cloud::Retention::default(),
        credential: jamstream_cloud::cloudinit::StorageCredential::KeyPair {
            access_key_id: "AKIAEXAMPLE".to_owned(),
            secret_access_key: "s3cret".to_owned(),
        },
        stems: true,
    });
    let script = jamstream_cloud::cloudinit::render(&cfg);
    assert!(script.contains(&format!("path: {RECORDING_CONFIG_PATH}")));

    // What the VM parses back is what jamstreamd's main parses, from the same
    // impl, so the round trip belongs here only as far as the launch surface:
    // the flag the bootstrap chgrps must be the file the server opens.
    assert!(script.contains(&format!("if [ -f {RECORDING_CONFIG_PATH} ]; then")));
}

fn boot_config() -> BootConfig {
    BootConfig {
        artifact_url: "https://example.invalid/jamstreamd".to_owned(),
        artifact_sha256: "0".repeat(64),
        server_private_key_b64: "AA==".to_owned(),
        issuer_public_key_b64: "AA==".to_owned(),
        session_id_hex: "deadbeefcafef00d".to_owned(),
        port: jamstream_cloud::DEFAULT_SESSION_PORT,
        idle_shutdown_min: 10,
        max_duration_min: 720,
        self_destruct: SelfDestruct::AwsShutdown,
        recording: None,
    }
}
