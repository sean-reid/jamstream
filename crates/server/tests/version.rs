//! The bootstrap's exec check (#139): cloud-init runs `jamstreamd
//! --version` once before enabling the unit, so a binary built for the
//! wrong architecture fails the bootstrap and trips its self-destruct trap
//! instead of dying silently at systemd's fork. That check only works if
//! the real binary answers the flag with exit 0 on a machine with no
//! config file, which is exactly the state of a VM mid-bootstrap.

use std::process::Command;

#[test]
fn version_exits_zero_without_a_config() {
    let out = Command::new(env!("CARGO_BIN_EXE_jamstreamd"))
        .arg("--version")
        // No /etc/jamstream/config exists on a developer machine or a CI
        // runner, which is the point: the flag must not need one.
        .output()
        .expect("spawn jamstreamd");
    assert!(out.status.success(), "status was {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    assert!(
        stdout.starts_with("jamstreamd "),
        "unexpected version line: {stdout:?}"
    );
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout:?}");
}
