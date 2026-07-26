//! User story: someone pastes a mangled invite and gets a specific decode
//! error and a failing exit code, not a hang or a stack trace. Covered at
//! both levels the binary is built from: the library function main()
//! dispatches to, and the actual built binary as a child process.

use std::path::PathBuf;
use std::process::Command;

use jamstream_cli::cli::JoinArgs;
use jamstream_cli::{CliError, join};

fn args_with_invite(invite: &str) -> JoinArgs {
    JoinArgs {
        invite: invite.to_owned(),
        headless: true,
        // Never reached: the invite is decoded before any file is opened.
        input: PathBuf::from("unused-in.wav"),
        output: PathBuf::from("unused-out.wav"),
        duration_secs: 1,
        chat: None,
        name: None,
        revoke_invite: None,
        revoke_after_secs: None,
    }
}

#[tokio::test]
async fn garbage_invite_yields_the_specific_decode_error() {
    let mut out: Vec<u8> = Vec::new();

    // Characters outside the base64url alphabet.
    let err = join::run(&args_with_invite("jamstream://join/@@garbage@@"), &mut out)
        .await
        .expect_err("garbage must not join");
    assert!(
        matches!(&err, CliError::Protocol(_)),
        "expected a protocol error, got {err:?}"
    );
    assert_eq!(err.to_string(), "invite is not valid: not valid encoding");

    // Valid encoding, corrupt payload: the other decode failure is named
    // differently so the user knows the paste was cut short.
    let truncated = data_encoding::BASE64URL_NOPAD.encode(&[1, 2, 3]);
    let err = join::run(&args_with_invite(&truncated), &mut out)
        .await
        .expect_err("a truncated blob must not join");
    assert_eq!(err.to_string(), "invite is not valid: truncated or corrupt");
}

#[test]
fn built_binary_prints_the_decode_error_and_exits_nonzero() {
    let output = Command::new(env!("CARGO_BIN_EXE_jamstream"))
        .args([
            "join",
            "jamstream://join/@@garbage@@",
            "--headless",
            "--input",
            "unused-in.wav",
            "--output",
            "unused-out.wav",
            "--duration-secs",
            "1",
        ])
        .output()
        .expect("spawn the jamstream binary");
    assert!(
        !output.status.success(),
        "a garbage invite must exit nonzero; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error: invite is not valid: not valid encoding"),
        "stderr must carry the specific decode error, was: {stderr}"
    );
}
