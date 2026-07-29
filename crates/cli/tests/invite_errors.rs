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
        invite: Some(invite.to_owned()),
        invite_file: None,
        headless: true,
        // Never reached: the invite is decoded before any file is opened.
        input: PathBuf::from("unused-in.wav"),
        output: PathBuf::from("unused-out.wav"),
        duration_secs: 1,
        chat: None,
        name: None,
        revoke_invite: None,
        revoke_invite_file: None,
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
    assert_eq!(err.to_string(), "invite has invalid encoding");

    // Valid encoding, corrupt payload: the other decode failure is named
    // differently so the user knows the paste was cut short.
    let truncated = data_encoding::BASE64URL_NOPAD.encode(&[1, 2, 3]);
    let err = join::run(&args_with_invite(&truncated), &mut out)
        .await
        .expect_err("a truncated blob must not join");
    assert_eq!(err.to_string(), "invite is truncated or corrupt");
}

/// The whole point of the file and pipe forms is that the credential never
/// appears in argv, so the real binary has to reach the decode with nothing
/// but a pipe. `-` and a path are the two spellings; both end up in the same
/// place.
#[test]
fn built_binary_reads_the_invite_from_a_pipe_and_from_a_file() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_jamstream"))
        .args([
            "join",
            "--invite-file",
            "-",
            "--headless",
            "--input",
            "unused-in.wav",
            "--output",
            "unused-out.wav",
            "--duration-secs",
            "1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the jamstream binary");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"jamstream://join/@@garbage@@\n")
        .expect("write the invite");
    let output = child.wait_with_output().expect("wait");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error: invite has invalid encoding"),
        "the piped invite must reach the decode: {stderr}"
    );
    // Nothing warned: no credential passed through argv.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("warning:"), "stdout was: {stdout}");

    let path = std::env::temp_dir().join(format!("jamstream-invite-{}.txt", std::process::id()));
    std::fs::write(&path, "jamstream://join/@@garbage@@\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_jamstream"))
        .args([
            "join",
            "--invite-file",
            path.to_str().unwrap(),
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
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("error: invite has invalid encoding"),
        "the file invite must reach the decode"
    );
    std::fs::remove_file(&path).unwrap();
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
        stderr.contains("error: invite has invalid encoding"),
        "stderr must carry the specific decode error, was: {stderr}"
    );
}
