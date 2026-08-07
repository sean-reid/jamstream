//! The checks no runner can make, as a step somebody works through.
//!
//! Four tests in this workspace are `#[ignore]`d because they need something a
//! hosted runner has not got: a real audio endpoint, a loopback device, the
//! open internet. No workflow passes `--run-ignored`, so none of them has ever
//! run in CI, and three of them are the only coverage of what they cover:
//! audio content through a real device, the sharing mode the Windows backend
//! reports, and a device producing on its own clock rather than being pumped by
//! its own consumer.
//!
//! `cargo xtask prerelease` runs all four on the machine in front of you and
//! says what each one needs first. The test at the bottom keeps the list
//! honest: a fifth ignored test anywhere in the workspace fails it until it
//! is named here.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// One test CI cannot run, and what a person needs to run it.
pub struct HandCheck {
    pub package: &'static str,
    /// `"lib"` for a unit test, else the integration binary's file stem.
    pub target: &'static str,
    pub test: &'static str,
    /// What the machine has to have, said before the run rather than after
    /// it fails.
    pub needs: &'static str,
}

/// Every `#[ignore]`d test in the workspace, in the order to work through
/// them: the cheap network one first, then the two that want hardware.
pub const HAND_CHECKS: [HandCheck; 4] = [
    HandCheck {
        package: "jamstream-cloud",
        target: "lib",
        test: "probe::tests::probe_the_shipped_catalog",
        needs: "the open internet; it dials every region in the shipped catalog",
    },
    HandCheck {
        package: "jamstream-audio-io",
        target: "cpal_devices",
        test: "enumerate_and_open_default_duplex",
        needs: "a real capture and playback device, and Windows for the sharing mode",
    },
    HandCheck {
        package: "jamstream-audio-io",
        target: "hardware_loopback",
        test: "a_tone_survives_the_round_trip_through_real_hardware",
        needs: "a loopback device (BlackHole, VB-CABLE, or a null sink) selected as \
                both the input and the output",
    },
    HandCheck {
        package: "jamstream-client",
        target: "lib",
        test: "live::tests::a_real_device_loses_no_capture_while_a_session_comes_up",
        needs: "a real capture and playback device, and a machine that is not \
                otherwise busy; it counts dropped capture against the client's own \
                ring sizes",
    },
];

impl HandCheck {
    /// The command line that runs this one check and nothing else.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec!["test".to_owned(), "-p".to_owned(), self.package.to_owned()];
        if self.target == "lib" {
            args.push("--lib".to_owned());
        } else {
            args.push("--test".to_owned());
            args.push(self.target.to_owned());
        }
        args.push(self.test.to_owned());
        args.extend(["--", "--ignored", "--exact", "--nocapture"].map(str::to_owned));
        args
    }

    /// The test's own name, without the module path a unit test carries.
    fn short_name(&self) -> &str {
        self.test.rsplit("::").next().unwrap_or(self.test)
    }
}

/// Runs every check in order, carrying on after a failure so one missing
/// device does not hide the rest, and reports at the end.
pub fn run() -> ExitCode {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut failed = Vec::new();
    for check in &HAND_CHECKS {
        println!("\n=== {} needs {}", check.short_name(), check.needs);
        let status = Command::new(&cargo)
            .args(check.args())
            .current_dir(workspace_root())
            .status();
        match status {
            Ok(status) if status.success() => println!("--- {} passed", check.short_name()),
            Ok(status) => {
                println!("--- {} failed ({status})", check.short_name());
                failed.push(check.short_name());
            }
            Err(err) => {
                println!("--- {} could not be started: {err}", check.short_name());
                failed.push(check.short_name());
            }
        }
    }
    if failed.is_empty() {
        println!("\nAll {} hand checks passed.", HAND_CHECKS.len());
        return ExitCode::SUCCESS;
    }
    println!(
        "\n{} of {} hand checks did not pass: {}",
        failed.len(),
        HAND_CHECKS.len(),
        failed.join(", ")
    );
    ExitCode::FAILURE
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Every `#[ignore]`d test the workspace holds, found by reading the source
/// rather than by running anything: `(package, target, test name)`.
pub fn ignored_tests() -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for crate_dir in sorted(&workspace_root().join("crates")) {
        let Ok(manifest) = std::fs::read_to_string(crate_dir.join("Cargo.toml")) else {
            continue;
        };
        let package = manifest
            .lines()
            .find_map(|line| line.strip_prefix("name = \""))
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_else(|| panic!("no package name in {}", crate_dir.display()))
            .to_owned();
        for (target, file) in rust_files(&crate_dir) {
            let source = std::fs::read_to_string(&file).expect("source is readable");
            for name in ignored_in(&source) {
                out.push((package.clone(), target.clone(), name));
            }
        }
    }
    out.sort();
    out
}

/// The test names an `#[ignore]` attribute precedes in one file. The
/// attribute has to open its own line, or the module comment explaining why
/// a suite is not ignored counts as one.
fn ignored_in(source: &str) -> Vec<String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut names = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        if !line.trim_start().starts_with("#[ignore") {
            continue;
        }
        let name = lines[at + 1..]
            .iter()
            .find_map(|line| {
                let line = line.trim_start().trim_start_matches("pub ");
                line.trim_start_matches("async ")
                    .strip_prefix("fn ")
                    .map(|rest| rest.split(['(', '<']).next().unwrap_or_default().to_owned())
            })
            .expect("an #[ignore] attribute sits on a function");
        names.push(name);
    }
    names
}

/// Every `.rs` file under a crate, tagged with the cargo target holding it.
/// `src/` is the library and each top-level file under `tests/` is its own
/// binary; nothing else in a crate holds a test.
fn rust_files(crate_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![crate_dir.join("src")];
    while let Some(dir) = stack.pop() {
        for path in sorted(&dir) {
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(("lib".to_owned(), path));
            }
        }
    }
    for path in sorted(&crate_dir.join("tests")) {
        if path.extension().is_some_and(|ext| ext == "rs") {
            let stem = path
                .file_stem()
                .expect("a .rs file has a stem")
                .to_string_lossy()
                .into_owned();
            out.push((stem, path));
        }
    }
    out
}

fn sorted(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list is the whole point, so it has to be the whole list. A test
    /// nobody runs and nobody has written down reads like coverage and is
    /// not (#394).
    #[test]
    fn the_hand_checks_are_every_ignored_test() {
        let mut listed: Vec<(String, String, String)> = HAND_CHECKS
            .iter()
            .map(|c| {
                (
                    c.package.to_owned(),
                    c.target.to_owned(),
                    c.short_name().to_owned(),
                )
            })
            .collect();
        listed.sort();
        assert_eq!(
            ignored_tests(),
            listed,
            "an ignored test is missing from `cargo xtask prerelease`, or the \
             task names one that has moved. Nothing in CI runs these, so the \
             task is the only place they run at all"
        );
    }

    /// The command has to name one test, in one target, and ask for the
    /// ignored ones: without `--ignored` it runs nothing and passes.
    #[test]
    fn each_check_runs_exactly_its_own_ignored_test() {
        for check in &HAND_CHECKS {
            let args = check.args();
            for want in ["--ignored", "--exact", check.test] {
                assert!(args.contains(&want.to_owned()), "{args:?}");
            }
            let target = if check.target == "lib" {
                "--lib"
            } else {
                check.target
            };
            assert!(args.contains(&target.to_owned()), "{args:?}");
        }
    }
}
