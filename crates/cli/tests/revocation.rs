//! User story: the host revokes an invite mid-session and that musician is
//! ejected while the others play on. Real jamstreamd runtime, three real
//! headless clients over loopback UDP, WAV fixtures as instruments. The
//! deterministic core-level version lives in crates/session/tests/loopback.rs
//! (revoke_ejects_and_blocks_rejoin); this proves the same story through the
//! binaries' code path, including the exit-with-reason behavior.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use common::{fixture, wav_tail_rms};
use jamstream_cli::cli::JoinArgs;
use jamstream_cli::{CliError, join};
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, Server};

/// Whole-session length for every member that is not ejected.
const SESSION_SECS: u64 = 4;
/// The host fires the revoke this long after joining.
const REVOKE_AFTER_SECS: u64 = 1;

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jamstream-cli-revoke-{}-{name}",
        std::process::id()
    ))
}

fn join_args(invite: &Invite, input: PathBuf, output: PathBuf) -> JoinArgs {
    JoinArgs {
        invite: Some(invite.encode()),
        invite_file: None,
        headless: true,
        input,
        output,
        duration_secs: SESSION_SECS,
        chat: None,
        name: None,
        revoke_invite: None,
        revoke_invite_file: None,
        revoke_after_secs: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_revokes_musician_two_and_musician_one_plays_on() {
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
        issuer.mint(
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
    };
    let host_invite = invite_for(0, "host");
    let m1_invite = invite_for(1, "one");
    let m2_invite = invite_for(2, "two");

    let out_host = temp_path("out-host.wav");
    let out_one = temp_path("out-one.wav");
    let out_two = temp_path("out-two.wav");

    // The host plays a 5 s sine (longer than the session) so musician 1 has
    // something to hear for the whole run; musician 2 plays the other tone
    // until ejection.
    let mut host_args = join_args(&host_invite, fixture("sine-440-48k.wav"), out_host.clone());
    // The target's invite goes through a file, not argv: it is a second
    // member's bearer credential, and argv is readable by every local
    // process. The whole story runs through the file form, so the flag is
    // exercised by the same test that proves revocation works.
    let revoke_file = temp_path("revoke-invite.txt");
    std::fs::write(&revoke_file, format!("{}\n", m2_invite.encode())).unwrap();
    host_args.revoke_invite_file = Some(revoke_file.clone());
    host_args.revoke_after_secs = Some(REVOKE_AFTER_SECS);
    let m1_args = join_args(&m1_invite, fixture("silence-48k.wav"), out_one.clone());
    let m2_args = join_args(&m2_invite, fixture("sine-880-48k.wav"), out_two.clone());

    let mut out_h = Vec::new();
    let mut out_1 = Vec::new();
    let mut out_2 = Vec::new();
    let (res_host, res_one, res_two) = tokio::join!(
        join::run(&host_args, &mut out_h),
        join::run(&m1_args, &mut out_1),
        join::run(&m2_args, &mut out_2),
    );

    // The host and musician 1 finish their full session.
    res_host.unwrap();
    res_one.unwrap();
    let text_host = String::from_utf8(out_h).unwrap();
    let text_one = String::from_utf8(out_1).unwrap();
    assert!(
        text_host.contains("sent revoke after 1 s"),
        "host never fired the revoke: {text_host}"
    );

    // Musician 2's process path exits nonzero with the ejection reason:
    // join::run returns the same error main() prints and maps to a failing
    // exit code.
    let err = res_two.expect_err("the revoked musician must not exit cleanly");
    assert!(
        matches!(&err, CliError::Failed(msg) if msg.contains("ejected") && msg.contains("invite revoked")),
        "wrong revocation error: {err}"
    );
    let text_two = String::from_utf8(out_2).unwrap();
    assert!(
        text_two.contains("ejected: invite revoked"),
        "musician 2 never printed the ejection reason: {text_two}"
    );

    // Musician 1 saw the room shrink from three to two.
    assert!(
        text_one.contains("roster: 3 members"),
        "musician 1 never saw the full roster: {text_one}"
    );
    assert!(
        text_one.contains("roster: 2 members"),
        "musician 1 never saw the roster shrink after the revoke: {text_one}"
    );

    // Musician 1's audio continues after the revoke: the ejection lands at
    // ~1 s of a 4 s session, so the final 1.5 s is entirely post-revoke and
    // must still carry the host's sine.
    let tail = wav_tail_rms(&out_one, 1.5);
    assert!(
        tail > 0.01,
        "musician 1's post-revoke audio is near-silence (rms {tail}); \
         the session did not play on"
    );

    // The revoked invite cannot rejoin: the server refuses the handshake
    // silently, so the rerun times out and exits nonzero.
    let rejoin_args = join_args(&m2_invite, fixture("sine-880-48k.wav"), out_two.clone());
    let mut out_rejoin = Vec::new();
    let err = join::run(&rejoin_args, &mut out_rejoin)
        .await
        .expect_err("the revoked invite must not get back in");
    assert!(
        matches!(&err, CliError::Failed(msg) if msg.contains("timed out")),
        "wrong rejoin refusal: {err}"
    );

    let _ = stop_tx.send(());
    server_task.await.unwrap().unwrap();

    for path in [&out_host, &out_one, &out_two, &revoke_file] {
        let _ = std::fs::remove_file(path);
    }
}
