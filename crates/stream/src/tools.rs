//! Where the broadcast tooling is, and what to say when it is not there.
//!
//! A session VM downloads ffmpeg and MediaMTX at boot and puts them where its
//! own config says, so on a VM the layout is a fact. A host machine has
//! whatever the host installed, which makes it a question, and the only answer
//! worth putting in front of a musician is the name of the program that is
//! missing and how to get it. `No such file or directory (os error 2)` names
//! neither.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The encoder, and the process behind every destination.
pub const FFMPEG: &str = "ffmpeg";
/// The relay the encoder publishes to and every pusher reads from.
pub const MEDIAMTX: &str = "mediamtx";

/// Where `bin` is on this machine's `PATH`, if it is anywhere.
pub fn on_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_in(std::env::split_paths(&path), bin)
}

/// True when `program` is something this machine can spawn: a path that is
/// there, or a bare name `PATH` resolves. A bare name is deliberately allowed
/// to pass at resolution time and be looked up again at spawn time, so a host
/// who installs the tool mid-session gets a working broadcast without
/// restarting it.
pub fn installed(program: &Path) -> bool {
    match bare_name(program) {
        Some(name) => on_path(name).is_some(),
        None => program.is_file(),
    }
}

/// What a host does about a program that is not on this machine. One
/// sentence, inside the wire's reason budget, naming the program first so it
/// is the first thing read.
pub fn missing(program: &Path) -> String {
    let name = program
        .file_name()
        .map_or_else(|| program.to_string_lossy(), OsStr::to_string_lossy);
    match hint(&name) {
        Some(hint) => format!("{name} is not installed; put it on PATH ({hint})"),
        None => format!("{name} is not installed"),
    }
}

/// How to get one of the two programs a broadcast needs, for whichever
/// platform the host is on. Stripped of a `.exe` so a Windows path still
/// matches.
fn hint(name: &str) -> Option<&'static str> {
    match name.strip_suffix(".exe").unwrap_or(name) {
        FFMPEG => Some("brew install ffmpeg, apt install ffmpeg, winget install ffmpeg"),
        MEDIAMTX => Some("brew install mediamtx, or a release from github.com/bluenviron/mediamtx"),
        _ => None,
    }
}

/// The file name of a program given as a bare name, or None when it is a path
/// with a directory in it. Windows accepts both separators, so a name
/// carrying either is a path there.
fn bare_name(program: &Path) -> Option<&str> {
    let name = program.to_str()?;
    let looks_like_a_path =
        name.contains('/') || (cfg!(windows) && (name.contains('\\') || name.contains(':')));
    (!name.is_empty() && !looks_like_a_path).then_some(name)
}

/// The first directory in `dirs` holding `bin`, taking Windows' `.exe` into
/// account. Separate from [`on_path`] so the rule is testable against
/// directories a test owns rather than against whatever the runner's `PATH`
/// happens to be.
fn find_in(dirs: impl Iterator<Item = PathBuf>, bin: &str) -> Option<PathBuf> {
    let exe = format!("{bin}.exe");
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
        if cfg!(windows) {
            let candidate = dir.join(&exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamstream_protocol::control::fit_stream_reason;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("jamstream-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    /// A sentence that does not survive the wire is worse than no sentence:
    /// every reason travels in a `StreamStatus` that is cut to fit, so a hint
    /// over budget would reach the host with its install command chopped off.
    #[test]
    fn every_hint_reaches_a_host_whole() {
        for tool in [FFMPEG, MEDIAMTX] {
            let reason = missing(Path::new(tool));
            assert_eq!(fit_stream_reason(&reason), reason, "{tool}");
            assert!(reason.starts_with(tool), "{reason}");
            assert!(reason.contains("install"), "{reason}");
        }
    }

    /// The absolute path a session VM configures and a bare name a host
    /// machine resolves have to produce the same sentence: the file name is
    /// what a musician can act on, not the directory it was looked for in.
    #[test]
    fn a_missing_program_is_named_however_it_was_configured() {
        let absolute = missing(Path::new("/usr/local/bin/ffmpeg"));
        assert_eq!(absolute, missing(Path::new("ffmpeg")));
        assert!(!absolute.contains("/usr/local/bin"), "{absolute}");
        // Anything else is still named, without an install line invented for
        // it.
        let other = missing(Path::new("/opt/jamstream/encoder"));
        assert_eq!(other, "encoder is not installed");
    }

    /// PATH lookup against real directories and real files, because the whole
    /// job of this module is to observe the filesystem rather than to hold an
    /// opinion about it.
    #[test]
    fn a_program_is_found_in_the_first_directory_holding_it() {
        let root = scratch("onpath");
        let empty = root.join("empty");
        let first = root.join("first");
        let second = root.join("second");
        for dir in [&empty, &first, &second] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let name = if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        std::fs::write(first.join(name), b"").unwrap();
        std::fs::write(second.join(name), b"").unwrap();

        let dirs = [empty.clone(), first.clone(), second.clone()];
        assert_eq!(
            find_in(dirs.iter().cloned(), FFMPEG),
            Some(first.join(name))
        );
        assert_eq!(find_in(dirs.iter().cloned(), MEDIAMTX), None);
        // A directory that is not there, and the empty entry a trailing
        // separator in PATH leaves behind, are skipped rather than matched.
        assert_eq!(
            find_in(
                [PathBuf::new(), root.join("missing"), first.clone()].into_iter(),
                FFMPEG
            ),
            Some(first.join(name))
        );
        // A directory is not a program.
        assert_eq!(find_in([root.clone()].into_iter(), "first"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `installed` has to answer for both shapes a configured program takes:
    /// a path is checked where it points, a bare name through PATH.
    #[test]
    fn a_configured_path_is_checked_where_it_points() {
        let root = scratch("installed");
        let program = root.join("ffmpeg");
        assert!(!installed(&program));
        std::fs::write(&program, b"").unwrap();
        assert!(installed(&program));
        // A bare name never reads as a file in the working directory.
        assert!(!installed(Path::new("jamstream-no-such-program")));
        let _ = std::fs::remove_dir_all(&root);
    }
}
