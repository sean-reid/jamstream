//! The first true end-to-end user story: two headless musicians join a
//! real jamstreamd server over loopback UDP, each sending a sine tone from
//! a WAV file. Each must hear the other (the personal mix excludes self,
//! so a lone client would record near-silence) and see the other's chat.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use jamstream_cli::cli::JoinArgs;
use jamstream_cli::join;
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, Server};

const RATE: u32 = 48_000;

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jamstream-cli-headless-{}-{name}",
        std::process::id()
    ))
}

fn write_sine(path: &Path, freq_hz: f32, secs: f32) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    let total = (secs * RATE as f32) as usize;
    for i in 0..total {
        let t = i as f32 / RATE as f32;
        let sample = (t * freq_hz * std::f32::consts::TAU).sin() * 0.5;
        writer
            .write_sample((sample * f32::from(i16::MAX)) as i16)
            .unwrap();
    }
    writer.finalize().unwrap();
}

/// RMS of the whole file in the -1..1 domain.
fn wav_rms(path: &Path) -> f64 {
    let mut reader = hound::WavReader::open(path).unwrap();
    let mut sum = 0.0f64;
    let mut count = 0u64;
    for sample in reader.samples::<i16>() {
        let v = f64::from(sample.unwrap()) / f64::from(i16::MAX);
        sum += v * v;
        count += 1;
    }
    assert!(count > 0, "output wav {path:?} is empty");
    (sum / count as f64).sqrt()
}

fn join_args(invite: String, input: PathBuf, output: PathBuf, chat: &str) -> JoinArgs {
    JoinArgs {
        invite,
        headless: true,
        input,
        output,
        duration_secs: 3,
        chat: Some(chat.to_owned()),
        name: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_musicians_hear_each_other_and_chat() {
    let issuer = Issuer::generate();
    let server_keys = generate_keypair();
    let session_id = SessionId::generate();

    let cfg = Config {
        session_id,
        port: 0,
        server_private_key: server_keys.private.to_vec(),
        issuer_public_key: issuer.public_key().to_bytes(),
        idle_shutdown_min: 10,
        max_duration_min: 720,
    };
    let server = Server::bind(
        &cfg,
        Options {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            activity_path: None,
        },
    )
    .await
    .unwrap();
    let server_addr = server.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(server.run(async {
        let _ = stop_rx.await;
    }));

    let invite_for = |member: u16, name: &str| {
        issuer
            .mint(
                session_id,
                vec![server_addr],
                server_keys.public,
                Token {
                    member_id: MemberId(member),
                    role: Role::Musician,
                    name_hint: Some(name.to_owned()),
                    expires_unix: u64::MAX,
                    jti: TokenId::generate(),
                },
            )
            .encode()
    };

    let in_one = temp_path("in-one.wav");
    let in_two = temp_path("in-two.wav");
    let out_one = temp_path("out-one.wav");
    let out_two = temp_path("out-two.wav");
    // Longer than the 3 s session so neither feed goes silent early.
    write_sine(&in_one, 440.0, 4.0);
    write_sine(&in_two, 880.0, 4.0);

    let args_one = join_args(
        invite_for(1, "one"),
        in_one.clone(),
        out_one.clone(),
        "hello from one",
    );
    let args_two = join_args(
        invite_for(2, "two"),
        in_two.clone(),
        out_two.clone(),
        "hello from two",
    );

    let mut stdout_one = Vec::new();
    let mut stdout_two = Vec::new();
    let (result_one, result_two) = tokio::join!(
        join::run(&args_one, &mut stdout_one),
        join::run(&args_two, &mut stdout_two),
    );
    result_one.unwrap();
    result_two.unwrap();

    let text_one = String::from_utf8(stdout_one).unwrap();
    let text_two = String::from_utf8(stdout_two).unwrap();

    // Both joined cleanly and left at the duration cap.
    assert!(text_one.contains("joined"), "client one output: {text_one}");
    assert!(text_two.contains("joined"), "client two output: {text_two}");
    assert!(text_one.contains("left after 3 s"));
    assert!(text_two.contains("left after 3 s"));

    // Each saw the other's chat message.
    assert!(
        text_one.contains("chat from 2: hello from two"),
        "client one never saw client two's chat: {text_one}"
    );
    assert!(
        text_two.contains("chat from 1: hello from one"),
        "client two never saw client one's chat: {text_two}"
    );

    // Each heard the other: the personal mix excludes self, so energy in
    // the output proves the peer's sine crossed the server.
    let rms_one = wav_rms(&out_one);
    let rms_two = wav_rms(&out_two);
    assert!(
        rms_one > 0.01,
        "client one's output is near-silence (rms {rms_one}); it never heard client two"
    );
    assert!(
        rms_two > 0.01,
        "client two's output is near-silence (rms {rms_two}); it never heard client one"
    );

    let _ = stop_tx.send(());
    server_task.await.unwrap().unwrap();

    for path in [&in_one, &in_two, &out_one, &out_two] {
        let _ = std::fs::remove_file(path);
    }
}
