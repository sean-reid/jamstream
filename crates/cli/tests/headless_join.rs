//! The first true end-to-end user story: two headless musicians join a
//! real jamstreamd server over loopback UDP, each sending a sine tone from
//! a WAV fixture. Each must hear the other (the personal mix excludes self,
//! so a lone client would record near-silence) and see the other's chat.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use common::{fixture, wav_rms};
use jamstream_cli::cli::JoinArgs;
use jamstream_cli::join;
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, Server};

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jamstream-cli-headless-{}-{name}",
        std::process::id()
    ))
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
        revoke_invite: None,
        revoke_after_secs: None,
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
            recording: None,
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

    // The 5 s sine fixtures outlast the 3 s session, so neither feed goes
    // silent early.
    let in_one = fixture("sine-440-48k.wav");
    let in_two = fixture("sine-880-48k.wav");
    let out_one = temp_path("out-one.wav");
    let out_two = temp_path("out-two.wav");

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

    // Inputs are shared regenerable fixtures; only the outputs are ours.
    for path in [&out_one, &out_two] {
        let _ = std::fs::remove_file(path);
    }
}
