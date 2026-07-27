//! Creating the files and directories that hold key material.
//!
//! Two crates need this and neither may have its own version of it: the
//! local provider writes the session server's private key into a per-session
//! config, and the CLI writes the issuer's private key into a session
//! record. The Windows half alone is fifty lines of `icacls`, and a second
//! copy of that would drift.
//!
//! What private means per platform:
//!
//! * unix: directories 0700, files 0600, and every write goes through an
//!   `O_EXCL` temporary plus a rename. That is what stops a file somebody
//!   else pre-created from being written through (including a symlink
//!   aimed at one of their targets), stops a rewrite from inheriting the
//!   permissive mode of the inode it replaced, and stops a crash from
//!   leaving a half-written key on disk.
//! * Windows: no mode bits. A new file takes the inheritable ACEs of the
//!   directory it lands in, so the directories are what get tightened, once,
//!   at creation, and pre-existing ones are left alone.
//!
//! A directory that already exists is inspected rather than altered: the
//! state dir may be a path the user chose, and silently rewriting its
//! permissions is not ours to do. World-writable is refused outright.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Creates `dir` and any missing parents, private to this account, and
/// refuses a directory the whole machine can write to.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    create_all(dir)?;
    check_exposure(dir)
}

/// Writes `bytes` to `path` as a file only this account can read,
/// replacing whatever was there. Never writes through what it replaces:
/// the bytes go to a fresh temporary in the same directory and the
/// temporary is renamed over the target.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    let tmp = temp_path(path);
    let mut opts = std::fs::OpenOptions::new();
    // create_new is O_CREAT|O_EXCL, which fails rather than following a
    // symlink or opening a file another account got there first.
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let write = || -> io::Result<()> {
        let mut file = opts.open(&tmp)?;
        file.write_all(bytes)?;
        // The rename is atomic on its own, but only the sync makes what
        // lands under the name after a crash be what we wrote.
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    };
    let result = write();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A name in the target's own directory, so the rename stays on one
/// filesystem. Unique per process and per call, because two writers to the
/// same path must not collide on the temporary.
fn temp_path(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    parent.join(format!(
        ".{name}.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn create_all(dir: &Path) -> io::Result<()> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        create_all(parent)?;
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    match builder.create(dir) {
        Ok(()) => {
            harden_new_dir(dir);
            Ok(())
        }
        // Lost a race with another process; its directory is fine.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

/// Refuses a directory every account on the machine can write to, which is
/// what a pre-created `/tmp/jamstream` looks like. Group-writable only
/// warns: user-private groups make it common and usually harmless, and
/// deciding otherwise would mean resolving group membership.
#[cfg(unix)]
fn check_exposure(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(dir)?.permissions().mode() & 0o7777;
    if mode & 0o002 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is writable by every account on this machine (mode {mode:o}); \
                 refusing to keep key material there",
                dir.display()
            ),
        ));
    }
    if mode & 0o020 != 0 {
        tracing::warn!(
            dir = %dir.display(),
            mode = format!("{mode:o}"),
            "state directory is group-writable"
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_exposure(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn harden_new_dir(_dir: &Path) {}

/// Drops the multi-account groups from a freshly created directory, so a
/// key file is not readable by every account on the machine just because
/// the state dir was pointed somewhere permissive. Under the default
/// `%LOCALAPPDATA%` path there is nothing to remove and this is a no-op in
/// effect; it earns its keep for a `JAMSTREAM_STATE_DIR` under `C:\` or
/// another shared root, which inherits `Authenticated Users: Modify` from
/// the volume root.
///
/// Two `icacls` passes, because `/remove:g` cannot touch an inherited ACE:
///
/// 1. `/inheritance:d` copies the inherited ACEs into the directory's own
///    ACL, and `/grant:r` pins this account's full control so pass 2 can
///    never delete the last entry that lets us read our own state.
/// 2. `/remove:g` drops the multi-account groups a directory outside the
///    user profile can inherit (Everyone, Authenticated Users, Users,
///    Anonymous), addressed by well-known SID because the display names
///    are localized.
///
/// Limits, deliberately:
///
/// * SYSTEM and Administrators keep their access, which is not a boundary
///   anyone can enforce anyway (an administrator can take ownership);
/// * an individual *other* account explicitly granted access on the parent
///   keeps it - only the four groups above are removed;
/// * the `(OI)(CI)` grant covers files created here afterwards, which is
///   all of them, since the directory is new;
/// * a volume without ACLs (FAT/exFAT) has nothing to tighten, `icacls`
///   fails there, and we only log it;
/// * and this is an external command rather than a DACL passed to
///   CreateFile, which is the correct fix and needs a windows-sys
///   dependency we are not taking on for this.
#[cfg(windows)]
fn harden_new_dir(dir: &Path) {
    use std::process::Command;

    let Some(user) = std::env::var_os("USERNAME").filter(|u| !u.is_empty()) else {
        tracing::warn!(
            dir = %dir.display(),
            "USERNAME is unset, leaving the directory ACL inherited: a state dir outside \
             the user profile may be readable by other accounts"
        );
        return;
    };
    let grant = format!("{}:(OI)(CI)F", user.to_string_lossy());
    let icacls = |args: &[&std::ffi::OsStr]| -> bool {
        match Command::new("icacls").args(args).output() {
            Ok(out) if out.status.success() => true,
            Ok(out) => {
                tracing::warn!(
                    dir = %dir.display(),
                    status = ?out.status.code(),
                    output = %String::from_utf8_lossy(&out.stderr).trim(),
                    "icacls did not tighten the directory ACL"
                );
                false
            }
            Err(err) => {
                tracing::warn!(error = %err, "cannot run icacls; directory ACL left inherited");
                false
            }
        }
    };
    let o = std::ffi::OsStr::new;
    let target = dir.as_os_str();
    if !icacls(&[target, o("/inheritance:d"), o("/grant:r"), o(&grant)]) {
        return;
    }
    icacls(&[
        target,
        o("/remove:g"),
        o("*S-1-1-0"), // Everyone
        o("/remove:g"),
        o("*S-1-5-11"), // Authenticated Users
        o("/remove:g"),
        o("*S-1-5-32-545"), // Users
        o("/remove:g"),
        o("*S-1-5-7"), // Anonymous Logon
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jamstream-private-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn private_dirs_are_created_recursively_and_idempotently() {
        let root = temp_dir("privdir");
        let nested = root.join("a").join("b").join("c");
        create_private_dir(&nested).unwrap();
        assert!(nested.is_dir());
        // Second call is a no-op, not an error.
        create_private_dir(&nested).unwrap();
        // And the point of it all: a file written inside is ours to read.
        let f = nested.join("config");
        write_private(&f, b"server_private_key=...").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"server_private_key=...");
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&nested), 0o700, "session dirs are not for sharing");
            assert_eq!(mode_of(&f), 0o600);
        }
        // Nothing is left lying around next to it.
        let strays: Vec<_> = std::fs::read_dir(&nested)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name != "config")
            .collect();
        assert!(strays.is_empty(), "temporaries left behind: {strays:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The attack this write path exists for: somebody pre-creates our file
    /// as a symlink to a file of theirs, and we truncate and overwrite it
    /// for them.
    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_way_is_replaced_not_followed() {
        let root = temp_dir("symlink");
        let victim = root.join("victim");
        std::fs::write(&victim, b"do not touch").unwrap();
        let target = root.join("local.json");
        std::os::unix::fs::symlink(&victim, &target).unwrap();

        write_private(&target, b"[]").unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"do not touch");
        assert_eq!(std::fs::read(&target).unwrap(), b"[]");
        assert!(!std::fs::symlink_metadata(&target).unwrap().is_symlink());
        assert_eq!(mode_of(&target), 0o600);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// mode() only applies at create, so a rewrite of a file that was left
    /// world-writable has to replace the inode, not open it.
    #[cfg(unix)]
    #[test]
    fn a_permissive_mode_does_not_survive_a_rewrite() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = temp_dir("mode");
        let path = root.join("local.json");
        std::fs::write(&path, b"[]").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

        write_private(&path, b"[{}]").unwrap();

        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"[{}]");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A directory anyone can write to is not a place for a private key,
    /// however it came to be one.
    #[cfg(unix)]
    #[test]
    fn a_world_writable_state_dir_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = temp_dir("shared");
        let dir = root.join("jamstream");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();

        let err = create_private_dir(&dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("every account"));

        // Tightened, it is fine again.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        create_private_dir(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The ACL tightening must leave the directory usable by us: the worst
    /// outcome of getting icacls wrong is locking the host out of its own
    /// state. Also asserts inheritance is really broken, which is
    /// locale-independent: icacls marks inherited entries `(I)`.
    #[cfg(windows)]
    #[test]
    fn windows_new_dirs_lose_inherited_aces_and_stay_writable() {
        let root = temp_dir("acl");
        let dir = root.join("state").join("sessions").join("abc");
        create_private_dir(&dir).unwrap();

        let config = dir.join("config");
        write_private(&config, b"server_private_key=secret").unwrap();
        assert_eq!(
            std::fs::read(&config).unwrap(),
            b"server_private_key=secret"
        );

        let out = std::process::Command::new("icacls")
            .arg(&dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "icacls query failed");
        let acl = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            !acl.contains("(I)"),
            "inherited ACEs survived on a directory we created: {acl}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
