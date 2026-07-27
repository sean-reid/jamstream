//! Staging a stream key for one pusher spawn.
//!
//! The rule from the threat model: a stream key lives in server memory and,
//! for the instant it takes to spawn a pusher, in a root-only file on tmpfs.
//! It is never an argument and never touches persistent disk.
//!
//! The file is created with mode 0600 by `open`, not by a later chmod, so
//! there is no window where it is group or world readable. The host opens it
//! and unlinks it before the child runs (see [`crate::proc::Stdin`]), which
//! means the path is gone by the time the spawn call returns, whether the
//! spawn succeeded or failed. The pusher receives the ingest URL on stdin
//! from the inherited descriptor.
//!
//! Residual, stated plainly: the launcher shell passes the URL to ffmpeg as
//! an argument, so it appears in *ffmpeg's* `/proc/<pid>/cmdline`. Only root
//! can read that on the session VM, because the cloud-init bootstrap remounts
//! /proc with `hidepid=2` and the VM has no other users. Removing that
//! residual entirely would mean speaking RTMP ourselves instead of shelling
//! out to ffmpeg.

use std::io;
use std::path::{Path, PathBuf};

use jamstream_protocol::ids::DestinationId;

/// Directory of one-shot key files, root-only, on tmpfs.
#[derive(Debug, Clone)]
pub struct KeyStore {
    dir: PathBuf,
}

impl KeyStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        KeyStore { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes `secret` to a fresh 0600 file for `id` and returns the path.
    /// Any leftover file for the same id (a previous spawn that never ran) is
    /// removed first.
    pub fn stage(&self, id: DestinationId, secret: &str) -> io::Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)?;
        restrict_dir(&self.dir)?;
        let path = self.path(id);
        let _ = std::fs::remove_file(&path);
        let mut file = create_0600(&path)?;
        use std::io::Write;
        // One line: the pusher reads it with a single shell `read`.
        writeln!(file, "{secret}")?;
        file.flush()?;
        Ok(path)
    }

    /// Best-effort removal, for teardown paths where no spawn happened.
    pub fn discard(&self, id: DestinationId) {
        let _ = std::fs::remove_file(self.path(id));
    }

    fn path(&self, id: DestinationId) -> PathBuf {
        self.dir.join(format!("dest-{}", id.0))
    }
}

#[cfg(unix)]
fn create_0600(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_0600(path: &Path) -> io::Result<std::fs::File> {
    // No mode bits to set; the pipeline does not run pushers off unix.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("jamstream-keys-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn staged_file_is_root_only_from_the_moment_it_exists() {
        let dir = tmp_dir("mode");
        let store = KeyStore::new(&dir);
        let path = store
            .stage(DestinationId(3), "rtmps://x/app/secret")
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "got {dir_mode:o}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "rtmps://x/app/secret\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restaging_replaces_a_leftover_file() {
        let dir = tmp_dir("restage");
        let store = KeyStore::new(&dir);
        store.stage(DestinationId(1), "first").unwrap();
        let path = store.stage(DestinationId(1), "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");
        store.discard(DestinationId(1));
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
