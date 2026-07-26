//! Repository tasks. `cargo xtask fixtures` writes the checked-out tree's
//! audio fixtures for humans who want files to listen to or ship; the test
//! suites do NOT read that directory. Each test binary regenerates the same
//! deterministic fixtures under target/fixtures/ at runtime (the generator
//! is duplicated in crates/cli/tests/common/mod.rs; keep them in sync).

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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
";

fn main() -> ExitCode {
    match env::args().nth(1).as_deref() {
        Some("fixtures") => fixtures(),
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

const RATE: u32 = 48_000;

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
    for name in [
        "impulse-train-48k.wav",
        "sine-440-48k.wav",
        "sine-880-48k.wav",
        "silence-48k.wav",
    ] {
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

/// One deterministic mono 48 kHz 16-bit fixture, selected by file name.
/// Duplicated in crates/cli/tests/common/mod.rs so tests need no dependency
/// on xtask; a change here must land there too.
fn write_fixture(name: &str, path: &Path) -> Result<(), hound::Error> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    type Writer = hound::WavWriter<std::io::BufWriter<std::fs::File>>;
    fn sine(writer: &mut Writer, hz: f32, secs: u32) -> Result<(), hound::Error> {
        for i in 0..(secs * RATE) {
            let t = i as f32 / RATE as f32;
            let sample = (t * hz * std::f32::consts::TAU).sin() * 0.5;
            writer.write_sample((sample * f32::from(i16::MAX)) as i16)?;
        }
        Ok(())
    }
    let mut writer = hound::WavWriter::create(path, spec)?;
    match name {
        "impulse-train-48k.wav" => {
            for i in 0..(10 * RATE) {
                let sample = if i % 4800 == 0 { i16::MAX } else { 0 };
                writer.write_sample(sample)?;
            }
        }
        "sine-440-48k.wav" => sine(&mut writer, 440.0, 5)?,
        "sine-880-48k.wav" => sine(&mut writer, 880.0, 5)?,
        "silence-48k.wav" => {
            for _ in 0..(2 * RATE) {
                writer.write_sample(0i16)?;
            }
        }
        other => panic!("unknown fixture {other:?}"),
    }
    writer.finalize()
}
