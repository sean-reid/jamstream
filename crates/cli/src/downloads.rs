//! Where a fetched take lands on this computer, and what is already there.
//!
//! # One folder, two surfaces
//!
//! The app downloads a take into a JamStream folder in the platform's music
//! directory, one subfolder per session, and `jamstream recordings` has to read
//! the same place: once a retention rule deletes the objects, the files a
//! download left behind are the only copy, and two tools disagreeing about
//! whether a take exists is worse than either being wrong. So the convention
//! lives in the crate both surfaces see rather than in the app.
//!
//! # A folder is not a listing
//!
//! Reading a folder goes around [`plan_downloads`](crate::recordings::plan_downloads),
//! which is what refuses a key that would land outside it, so only the names
//! the recorder writes count and only files. Nothing found this way has a size
//! to be measured against either, so nothing found this way may be called
//! whole.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Where a downloaded take lands: a JamStream folder in the platform's music
/// directory, which is where music belongs and somewhere a DAW already looks.
pub fn dir() -> PathBuf {
    resolve(
        dirs::audio_dir()
            .or_else(dirs::download_dir)
            .or_else(dirs::home_dir)
            // The state directory refuses outright with no platform directory,
            // because it holds keys. This is where downloads land and what the
            // app's reveal button opens, so refusal would kill the feature;
            // the current directory keeps the path absolute, where the bare
            // name resolves against whatever the process started in (System32,
            // from the Windows Start menu).
            .or_else(|| std::env::current_dir().ok()),
    )
}

/// The chosen base with our folder inside it; no base at all leaves the bare
/// relative name, the honest floor when even the current directory is
/// unknowable.
fn resolve(base: Option<PathBuf>) -> PathBuf {
    base.map(|dir| dir.join("JamStream"))
        .unwrap_or_else(|| PathBuf::from("JamStream"))
}

/// One session's own folder under `base`, named for the short form of its id.
///
/// A folder per session because two sessions can record takes a minute apart
/// and the recorder names a take by clock time, so one folder for all of them
/// would collide.
pub fn session_dir(base: &Path, session_id_hex: &str) -> PathBuf {
    base.join(session_id_hex.chars().take(8).collect::<String>())
}

/// A take file found on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTake {
    pub path: PathBuf,
    pub bytes: u64,
    /// When the file was last written, which for a finished take is when Stop
    /// was pressed.
    pub modified_unix: u64,
}

impl LocalTake {
    /// The name the recorder gave it, which carries the take's time and, for a
    /// stem, the player.
    pub fn name(&self) -> Cow<'_, str> {
        self.path.file_name().unwrap_or_default().to_string_lossy()
    }

    /// Still being written: the recorder closes a take by renaming it out of
    /// `.part`, so anything still carrying that suffix is not a take yet and
    /// nothing may offer it as a recording.
    pub fn partial(&self) -> bool {
        self.name().ends_with(".part")
    }
}

/// Reads the take files in one folder, whatever state they are in. A folder
/// that is not there is no takes rather than an error: it is created by the
/// first recording, or by the first download.
pub fn local_takes(dir: &Path) -> Vec<LocalTake> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with("jamstream-") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified_unix = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(LocalTake {
            path,
            bytes: meta.len(),
            modified_unix,
        });
    }
    out
}

/// The takes in one folder that are not still being written, sorted by name so
/// what reads them prints in a fixed order rather than the directory's.
pub fn takes_in(dir: &Path) -> Vec<LocalTake> {
    let mut found: Vec<LocalTake> = local_takes(dir)
        .into_iter()
        .filter(|take| !take.partial())
        .collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The folder is ours by name wherever it lands, and a machine that can
    /// name any directory at all gets an absolute path: a relative one
    /// resolves against whatever the process started in, which for a Start
    /// menu launch on Windows is System32, and both the app's reveal button
    /// and the "landed in" line would point there.
    #[test]
    fn the_downloads_dir_is_absolute_whenever_any_base_exists() {
        let base = std::env::temp_dir();
        assert_eq!(resolve(Some(base.clone())), base.join("JamStream"));
        assert_eq!(resolve(None), PathBuf::from("JamStream"));
        // The live chain ends at current_dir, so on any machine where this
        // test can run the real answer is absolute.
        let dir = dir();
        assert!(dir.is_absolute(), "{}", dir.display());
        assert!(dir.ends_with("JamStream"), "{}", dir.display());
    }

    /// The folder name is the short id both surfaces print, and an id shorter
    /// than that is still a folder rather than a panic.
    #[test]
    fn a_session_gets_the_folder_its_short_id_names() {
        let base = Path::new("/Users/you/Music/JamStream");
        assert_eq!(session_dir(base, "5aed5593aaaa1111"), base.join("5aed5593"));
        assert_eq!(session_dir(base, "5aed"), base.join("5aed"));
    }

    /// The folder is a folder on a musician's computer, so what is in it is
    /// not a take just because it is there. Reading it goes around the
    /// download plan's own refusal, and this is the discipline that replaces
    /// it.
    #[test]
    fn only_the_recorders_own_finished_files_are_takes() {
        let dir = std::env::temp_dir().join(format!("jamstream-downloads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A folder that is not there yet holds nothing, and says so quietly.
        assert!(takes_in(&dir).is_empty());

        std::fs::create_dir_all(&dir).expect("create the folder");
        let mix = dir.join("jamstream-2026-07-29-1658-mix.flac");
        std::fs::write(&mix, vec![7u8; 4_096]).expect("write the mix");
        std::fs::write(dir.join("jamstream-2026-07-29-1658-Ana.flac"), b"stem").expect("write");
        std::fs::write(dir.join("notes.txt"), b"chords").expect("write");
        std::fs::write(dir.join("mix.flac"), b"someone else's mix").expect("write");
        std::fs::write(dir.join("jamstream-2026-07-29-1702-mix.flac.part"), b"ab").expect("write");
        std::fs::create_dir(dir.join("jamstream-2026-07-29-1710-mix.flac")).expect("mkdir");

        let found = takes_in(&dir);
        assert_eq!(
            found
                .iter()
                .map(|t| t.name().to_string())
                .collect::<Vec<_>>(),
            vec![
                "jamstream-2026-07-29-1658-Ana.flac",
                "jamstream-2026-07-29-1658-mix.flac",
            ],
            "only the recorder's own names, only files, and nothing half written"
        );
        assert_eq!(found[1].path, mix);
        assert_eq!(found[1].bytes, 4_096);
        assert!(found[1].modified_unix > 0, "the disk knows when it landed");
        // The half written file is still a file this machine holds, which is
        // what the app draws as being written.
        assert!(
            local_takes(&dir).iter().any(LocalTake::partial),
            "the .part file is readable, it is just not a take"
        );
        std::fs::remove_dir_all(&dir).expect("clean up");
    }
}
