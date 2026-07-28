//! Shared helpers for the CLI end-to-end tests: deterministic audio
//! fixtures regenerated on demand under target/fixtures/ (the generator
//! mirrors `cargo xtask fixtures`; keep the two in sync), WAV energy
//! measurements for asserting on captured mixes, and the jamstreamd
//! build-and-kill plumbing the local session stories share.

#![allow(dead_code)] // each test binary uses a different subset

use std::path::{Path, PathBuf};

pub const RATE: u32 = 48_000;

#[cfg(windows)]
pub const BIN_NAME: &str = "jamstreamd.exe";
#[cfg(not(windows))]
pub const BIN_NAME: &str = "jamstreamd";

/// Builds (if needed) and returns the jamstreamd binary for this profile.
///
/// CARGO_BIN_EXE_<name> only covers binaries of the package under test, so
/// the cli tests cannot ask Cargo for jamstreamd directly. Instead this
/// derives the profile directory from the test's own executable path
/// (target/<profile>/deps/<test>-<hash>), runs `cargo build -p
/// jamstream-server --bin jamstreamd` against the workspace to guarantee
/// the binary exists and is fresh (a no-op when it already is), and returns
/// target/<profile>/jamstreamd for JAMSTREAMD_PATH.
pub fn jamstreamd_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let profile_dir = exe
        .parent() // deps/
        .and_then(|d| d.parent()) // target/<profile>/
        .expect("test executable must sit in target/<profile>/deps")
        .to_path_buf();
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = std::process::Command::new(cargo);
    build.args(["build", "-p", "jamstream-server", "--bin", "jamstreamd"]);
    if profile_dir.file_name().is_some_and(|n| n == "release") {
        build.arg("--release");
    }
    let status = build.current_dir(&workspace).status();

    let binary = profile_dir.join(BIN_NAME);
    match status {
        Ok(s) if s.success() => {}
        // A failed or unavailable cargo is tolerable if an earlier build
        // already produced the binary; without one the test cannot run.
        _ if binary.is_file() => eprintln!(
            "warning: cargo build -p jamstream-server failed; using the existing {}",
            binary.display()
        ),
        _ => panic!(
            "cannot build jamstreamd and none exists at {}; \
             run `cargo build -p jamstream-server` first",
            binary.display()
        ),
    }
    assert!(
        binary.is_file(),
        "cargo build succeeded but {} is missing; unusual target layout?",
        binary.display()
    );
    binary
}

/// Bind-then-drop; racy in principle, unique enough in practice.
pub fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Kills the spawned server if a test leaves without ending its session.
/// The failure path is the one that matters: a test about processes that
/// outlive their launcher must not leave one behind when it fails, and a
/// panic anywhere below drops this on the way out.
pub struct ServerGuard(std::sync::Mutex<Option<String>>);

impl ServerGuard {
    pub fn new() -> Self {
        ServerGuard(std::sync::Mutex::new(None))
    }

    pub fn watch(&self, pid: &str) {
        *self.0.lock().expect("guard") = Some(pid.to_owned());
    }

    /// The session ended cleanly; there is nothing left to kill.
    pub fn disarm(&self) {
        self.0.lock().expect("guard").take();
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let Some(pid) = self.0.lock().map(|mut g| g.take()).unwrap_or(None) else {
            return;
        };
        eprintln!("test left jamstreamd {pid} running; killing it");
        #[cfg(unix)]
        let mut kill = std::process::Command::new("/bin/kill");
        #[cfg(unix)]
        kill.args(["-9", &pid]);
        #[cfg(windows)]
        let mut kill = std::process::Command::new("taskkill");
        #[cfg(windows)]
        kill.args(["/PID", &pid, "/T", "/F"]);
        let _ = kill.status();
    }
}

/// Where the tests keep their regenerable fixtures. Deliberately inside
/// target/ so no top-level fixtures directory appears from running tests.
pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("fixtures")
        .join("audio")
}

/// Returns the path of a named fixture, generating it first if absent.
/// Safe against concurrent test binaries: generation goes to a process-
/// unique temp name and lands with an atomic rename.
pub fn fixture(name: &str) -> PathBuf {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).expect("create fixtures dir");
    let path = dir.join(name);
    if path.exists() {
        return path;
    }
    let tmp = dir.join(format!(
        ".{name}.{}.{:?}.tmp",
        std::process::id(),
        std::thread::current().id()
    ));
    write_fixture(name, &tmp);
    // A racing process may have won; either file is identical.
    if std::fs::rename(&tmp, &path).is_err() {
        assert!(path.exists(), "fixture rename failed and {name} is absent");
        let _ = std::fs::remove_file(&tmp);
    }
    path
}

/// One deterministic mono 48 kHz 16-bit fixture, selected by file name.
/// Duplicated from xtask/src/main.rs so tests need no dependency on xtask.
fn write_fixture(name: &str, path: &Path) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create fixture wav");
    let mut sine = |hz: f32, secs: u32| {
        for i in 0..(secs * RATE) {
            let t = i as f32 / RATE as f32;
            let sample = (t * hz * std::f32::consts::TAU).sin() * 0.5;
            writer
                .write_sample((sample * f32::from(i16::MAX)) as i16)
                .expect("write fixture sample");
        }
    };
    match name {
        "impulse-train-48k.wav" => {
            for i in 0..(10 * RATE) {
                let sample = if i % 4800 == 0 { i16::MAX } else { 0 };
                writer.write_sample(sample).expect("write fixture sample");
            }
        }
        "sine-440-48k.wav" => sine(440.0, 5),
        "sine-880-48k.wav" => sine(880.0, 5),
        "silence-48k.wav" => {
            for _ in 0..(2 * RATE) {
                writer.write_sample(0i16).expect("write fixture sample");
            }
        }
        other => panic!("unknown fixture {other:?}"),
    }
    writer.finalize().expect("finalize fixture wav");
}

/// All samples of a WAV in the -1..1 domain, channels interleaved as stored.
pub fn wav_samples(path: &Path) -> Vec<f64> {
    let mut reader = hound::WavReader::open(path).expect("open wav");
    reader
        .samples::<i16>()
        .map(|s| f64::from(s.expect("wav sample")) / f64::from(i16::MAX))
        .collect()
}

pub fn rms(samples: &[f64]) -> f64 {
    assert!(!samples.is_empty(), "no samples to measure");
    (samples.iter().map(|s| s * s).sum::<f64>() / samples.len() as f64).sqrt()
}

/// RMS of the whole file.
pub fn wav_rms(path: &Path) -> f64 {
    rms(&wav_samples(path))
}

/// RMS of the final `secs` seconds of a stereo 48 kHz file.
pub fn wav_tail_rms(path: &Path, secs: f64) -> f64 {
    let samples = wav_samples(path);
    let take = ((secs * f64::from(RATE)) as usize * 2).min(samples.len());
    assert!(take > 0, "window is empty for {path:?}");
    rms(&samples[samples.len() - take..])
}
