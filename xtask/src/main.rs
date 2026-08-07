//! Repository tasks. `cargo xtask fixtures` writes the checked-out tree's
//! audio fixtures for humans who want files to listen to or ship; the test
//! suites do NOT read that directory. Each test binary regenerates the same
//! deterministic fixtures under target/fixtures/ at runtime, by calling the
//! same [`xtask::write_fixture`] this does.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use xtask::{FIXTURES, write_fixture};

const HELP: &str = "\
usage: cargo xtask <task>

tasks:
  fixtures   Write deterministic audio fixtures to fixtures/audio/ in the
             repository root:
               impulse-train-48k.wav  10 s, one-sample spikes every 4800 samples
               sine-440-48k.wav       5 s, 440 Hz sine at half scale
               sine-880-48k.wav       5 s, 880 Hz sine at half scale
               silence-48k.wav        2 s of silence
             All mono, 48 kHz, 16-bit; byte-identical on every run (no
             timestamps in content). Tests never read this directory: they
             regenerate the same files under target/fixtures/ on demand, so
             fixtures/ stays out of version control unless a human wants it.

  prerelease Run the four tests CI cannot: the audio round trip through a
             real device, the device and sharing-mode report, the dropped
             capture count against a device on its own clock, and the probe
             of the shipped region catalog. Every one is #[ignore]d and no
             workflow passes --run-ignored, so this is the only place they
             run. Work through it on a machine with audio devices before
             shipping a release.
";

fn main() -> ExitCode {
    match env::args().nth(1).as_deref() {
        Some("fixtures") => fixtures(),
        Some("prerelease") => xtask::prerelease::run(),
        Some("--help") | Some("-h") | None => {
            eprint!("{HELP}");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("unknown task {other:?}\n");
            eprint!("{HELP}");
            ExitCode::FAILURE
        }
    }
}

fn fixtures() -> ExitCode {
    // CARGO_MANIFEST_DIR points at xtask/, whose parent is the repo root.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("fixtures")
        .join("audio");
    if let Err(err) = std::fs::create_dir_all(&dir) {
        eprintln!("cannot create {}: {err}", dir.display());
        return ExitCode::FAILURE;
    }
    for name in FIXTURES {
        let path = dir.join(name);
        match write_fixture(name, &path) {
            Ok(()) => println!("wrote {}", path.display()),
            Err(err) => {
                eprintln!("cannot write {}: {err}", path.display());
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}
