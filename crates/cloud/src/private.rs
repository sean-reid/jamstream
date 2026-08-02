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
//!   directory it lands in, so the directories are what get tightened:
//!   inheritance cut, the multi-account groups dropped, whether this
//!   process created the directory or found it already there.
//!
//! A directory that already exists is still vetted, because the state dir
//! may be a path the user chose, and a permissive one lets someone else
//! swap the files under the keys. On unix a directory another account
//! owns, or one that is group- or world-writable, is refused outright,
//! with the chmod to run in the message. Windows has no one-line remedy to
//! hand back, so the exposure is repaired in place instead, and a repair
//! that fails is the same hard refusal.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Creates `dir` and any missing parents, private to this account, and
/// refuses a directory the whole machine can write to. On Windows the
/// exposed case is repaired rather than refused (see `harden_dir`); only a
/// repair that fails is an error.
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
    let opts = private_file_options();
    let write = || -> io::Result<()> {
        let mut file = opts.open(&tmp)?;
        file.write_all(bytes)?;
        // The rename is atomic on its own, but only the sync makes what
        // lands under the name after a crash be what we wrote.
        file.sync_all()?;
        drop(file);
        rename_with_retry(&tmp, path)
    };
    let result = write();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Reads a file that holds key material, refusing it when the directory
/// it sits in is not one only this account can write.
///
/// The write side vets the directory on the way in
/// ([`create_private_dir`]) and the read side has to do the same, because
/// the vetting is what says who could have put the bytes there. A path
/// that was private when it was written and is not now hands back
/// whatever the account that loosened it left behind, and the caller has
/// no way to tell.
pub fn read_private(path: &Path) -> io::Result<Vec<u8>> {
    check_exposure(parent_of(path))?;
    std::fs::read(path)
}

/// Creates `path` as a file only this account can read and hands back the
/// open handle, for the writer that keeps producing lines rather than
/// having its bytes ready in one buffer. [`write_private`] is the one for
/// anything that can be handed over whole; the app log cannot, and a log
/// of provider errors and panic backtraces is no more readable by the
/// machine than a key is.
///
/// Replaces rather than reopens, for the reason `write_private` does: a
/// file another account pre-created, or a symlink of theirs aimed at one
/// of their targets, must not be written through or truncated.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    private_file_options().open(path)
}

/// How every file this module creates is opened. `create_new` is
/// `O_CREAT|O_EXCL`, which fails rather than following a symlink or
/// opening a file another account got there first, and the mode applies
/// at creation, which is the only moment it can.
fn private_file_options() -> std::fs::OpenOptions {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    opts
}

/// Renames `from` over `to`, retrying a few times before giving up.
///
/// On Windows a freshly written file is routinely still open in an antivirus
/// or indexing scan, and the rename fails with a sharing violation that
/// clears within milliseconds; losing a session state file over one after
/// the VM is already launched and billing is not an answer. A rename that
/// keeps failing still fails, with the last error. On unix the first try is
/// the only one that ever runs.
pub fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    const TRIES: u32 = 5;
    const PAUSE: std::time::Duration = std::time::Duration::from_millis(50);
    let mut attempt = 1;
    loop {
        match std::fs::rename(from, to) {
            Err(_) if attempt < TRIES => {
                attempt += 1;
                std::thread::sleep(PAUSE);
            }
            result => return result,
        }
    }
}

/// A name in the target's own directory, so the rename stays on one
/// filesystem. Unique per process and per call, because two writers to the
/// same path must not collide on the temporary.
fn temp_path(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let parent = parent_of(path);
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

/// The directory a path sits in, where a bare file name means the working
/// directory rather than nothing.
fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn create_all(dir: &Path) -> io::Result<()> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        create_all(parent)?;
    }
    match private_dir_builder().create(dir) {
        Ok(()) => {
            harden_new_dir(dir);
            Ok(())
        }
        // Lost a race with another process; its directory is fine.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn private_dir_builder() -> std::fs::DirBuilder {
    use std::os::unix::fs::DirBuilderExt as _;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder
}

/// Windows takes its permissions from the ACL, which [`harden_new_dir`]
/// sets on the way past.
#[cfg(not(unix))]
fn private_dir_builder() -> std::fs::DirBuilder {
    std::fs::DirBuilder::new()
}

/// Refuses a directory this account does not own or that other accounts can
/// write to: either way somebody else controls what sits under the key
/// files. A pre-created `/tmp/jamstream` fails the write bits; a directory
/// another account handed over fails `st_uid == geteuid()`, the check
/// issue #49 asked for and only libc can make.
#[cfg(unix)]
fn check_exposure(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    let meta = std::fs::metadata(dir)?;
    // geteuid cannot fail; the unsafe is only FFI.
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by uid {}, not by this account (uid {euid}); \
                 refusing to keep key material in a directory someone else owns",
                dir.display(),
                meta.uid()
            ),
        ));
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode & 0o022 != 0 {
        let who = if mode & 0o002 != 0 {
            "every account on this machine"
        } else {
            "its group"
        };
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is writable by {who} (mode {mode:o}); refusing to keep key \
                 material there. Fix it with: chmod 700 {}",
                dir.display(),
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// Windows has no owner or mode bits to stat, so the exposure check *is*
/// the hardening, run whether or not this process created the directory:
/// the pre-existing `JAMSTREAM_STATE_DIR` under a shared root is exactly
/// the case the threat model names, and it never passes through
/// [`harden_new_dir`].
#[cfg(windows)]
fn check_exposure(dir: &Path) -> io::Result<()> {
    harden_dir(dir)
}

#[cfg(not(any(unix, windows)))]
fn check_exposure(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn harden_new_dir(_dir: &Path) {}

/// Directories created on the way to the key directory. A failure here is
/// a warning, not an error: the directory the key files actually land in
/// is the one [`check_exposure`] re-vets on every [`create_private_dir`],
/// and that one refuses.
#[cfg(windows)]
fn harden_new_dir(dir: &Path) {
    if let Err(err) = harden_dir(dir) {
        tracing::warn!(error = %err, "cannot tighten a fresh directory's ACL");
    }
}

/// Drops the multi-account groups from `dir`, so a key file is not
/// readable by every account on the machine just because the state dir was
/// pointed somewhere permissive. Under the default `%LOCALAPPDATA%` path
/// there is nothing to remove and this is a no-op in effect; it earns its
/// keep for a `JAMSTREAM_STATE_DIR` under `C:\` or another shared root,
/// which inherits `Authenticated Users: Modify` from the volume root.
///
/// Idempotent, and cheap to repeat: a directory this process already
/// tightened is remembered and skipped, so the per-write
/// [`create_private_dir`] calls do not spawn `icacls` every time.
///
/// Two `icacls` passes, because `/remove:g` cannot touch an inherited ACE:
///
/// 1. `/inheritance:d` copies the inherited ACEs into the directory's own
///    ACL, and `/grant:r` pins this account's full control so pass 2 can
///    never delete the last entry that lets us read our own state. The
///    grant is addressed by this account's SID when `whoami` can report
///    one: a bare `%USERNAME%` can fail to resolve for a domain account,
///    and the SID never does.
/// 2. `/remove:g` drops the multi-account groups a directory outside the
///    user profile can inherit (Everyone, Authenticated Users, Users,
///    Anonymous), addressed by well-known SID because the display names
///    are localized.
///
/// A failure in either pass is an error, not a log line: the directory is
/// about to hold key material, and on unix the same exposure is a hard
/// refusal. That covers a volume without ACLs (FAT/exFAT), where `icacls`
/// fails and which cannot protect a key from anyone.
///
/// Limits, deliberately:
///
/// * SYSTEM and Administrators keep their access, which is not a boundary
///   anyone can enforce anyway (an administrator can take ownership);
/// * an individual *other* account explicitly granted access on the parent
///   keeps it - only the four groups above are removed;
/// * the `(OI)(CI)` grant covers files created here afterwards; a file
///   that was already in a pre-existing directory keeps its own ACL, and
///   this module only writes fresh files ([`write_private`] replaces,
///   never reopens);
/// * and this is an external command rather than a DACL passed to
///   CreateFile, which is the correct fix and needs a windows-sys
///   dependency we are not taking on for this.
#[cfg(windows)]
fn harden_dir(dir: &Path) -> io::Result<()> {
    use std::collections::HashSet;
    use std::sync::{LazyLock, Mutex};
    static HARDENED: LazyLock<Mutex<HashSet<PathBuf>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    let key = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if HARDENED.lock().unwrap().contains(&key) {
        return Ok(());
    }
    harden_dir_with(&system32("icacls.exe"), dir)?;
    HARDENED.lock().unwrap().insert(key);
    Ok(())
}

/// [`harden_dir`] with the tool path injectable, which is the only way a
/// test can make `icacls` fail on demand.
#[cfg(windows)]
fn harden_dir_with(exe: &Path, dir: &Path) -> io::Result<()> {
    use std::process::Command;

    let grant = format!("{}:(OI)(CI)F", current_user_grantee(dir)?);
    let icacls = |args: &[&std::ffi::OsStr]| -> io::Result<()> {
        let out = Command::new(exe)
            .args(args)
            .output()
            .map_err(|err| exposed(dir, format_args!("cannot run {}: {err}", exe.display())))?;
        if out.status.success() {
            return Ok(());
        }
        // icacls splits its complaints across the two streams.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail = match stderr.trim() {
            "" => stdout.trim(),
            complaint => complaint,
        };
        Err(exposed(
            dir,
            format_args!("icacls exited with {:?}: {detail}", out.status.code()),
        ))
    };
    let o = std::ffi::OsStr::new;
    let target = dir.as_os_str();
    icacls(&[target, o("/inheritance:d"), o("/grant:r"), o(&grant)])?;
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
    ])
}

/// The refusal every failed tightening becomes: name the directory, say
/// what failed, and say what to do about it.
#[cfg(windows)]
fn exposed(dir: &Path, detail: std::fmt::Arguments<'_>) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "cannot tighten the ACL on {} ({detail}); refusing to keep key \
             material in a directory other accounts may be able to write. \
             Point JAMSTREAM_STATE_DIR at a directory under %LOCALAPPDATA%, \
             or repair this one with icacls by hand",
            dir.display()
        ),
    )
}

/// This account, in the form `icacls /grant` takes: `*SID` when the SID
/// can be read, the `%USERNAME%` display name as the fallback, and an
/// error when neither exists, because pass 2 without the pinned grant can
/// remove the last ACE that lets us read our own state.
#[cfg(windows)]
fn current_user_grantee(dir: &Path) -> io::Result<String> {
    if let Some(sid) = current_user_sid() {
        return Ok(format!("*{sid}"));
    }
    match std::env::var("USERNAME") {
        Ok(user) if !user.is_empty() => Ok(user),
        _ => Err(exposed(
            dir,
            format_args!("whoami reports no SID and USERNAME is unset"),
        )),
    }
}

/// This account's SID, from `whoami /user /fo csv /nh`, whose one row is
/// `"machine\user","S-1-5-21-..."`. Same discipline as the tasklist probe
/// in the local provider: quoted-CSV shape plus an exact prefix test
/// rather than trust in localized text, so an error message or a header
/// row can never pass for a SID.
#[cfg(windows)]
fn current_user_sid() -> Option<String> {
    use std::sync::OnceLock;
    static SID: OnceLock<Option<String>> = OnceLock::new();
    SID.get_or_init(|| {
        let out = std::process::Command::new(system32("whoami.exe"))
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_whoami_sid(&String::from_utf8_lossy(&out.stdout))
    })
    .clone()
}

/// The parse half of [`current_user_sid`], split out so every platform's
/// test suite can exercise it.
#[cfg(any(windows, test))]
fn parse_whoami_sid(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        // `/fo csv` quotes every field, so a row is `"name","sid"` exactly.
        let Some(row) = line
            .trim()
            .strip_prefix('"')
            .and_then(|r| r.strip_suffix('"'))
        else {
            continue;
        };
        let Some((_, sid)) = row.rsplit_once("\",\"") else {
            continue;
        };
        let shape = sid.strip_prefix("S-1-").is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'-')
        });
        if shape {
            return Some(sid.to_owned());
        }
    }
    None
}

/// By absolute path: these run while a directory that is about to hold a
/// private key is being created, and a writable directory early in PATH
/// would otherwise choose the program that decides its permissions.
#[cfg(windows)]
fn system32(exe: &str) -> PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_owned());
    [
        PathBuf::from(format!("{root}\\System32\\{exe}")),
        PathBuf::from(format!("C:\\Windows\\System32\\{exe}")),
    ]
    .into_iter()
    .find(|p| p.is_file())
    .unwrap_or_else(|| PathBuf::from(exe))
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

    /// The retry exists for Windows sharing violations, which no test can
    /// stage deterministically; what is checkable everywhere is that a clean
    /// rename moves the bytes and a rename that cannot ever work still fails
    /// with the real error instead of hanging or lying.
    #[test]
    fn a_retried_rename_moves_the_file_and_still_reports_a_real_failure() {
        let root = temp_dir("rename");
        let from = root.join("from");
        let to = root.join("to");
        std::fs::write(&from, b"payload").unwrap();
        rename_with_retry(&from, &to).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"payload");
        assert!(!from.exists());

        let err = rename_with_retry(&from, &to).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
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

    /// The handle the app log is written through gets what a key file
    /// gets: a fresh 0600 inode, and a symlink left at the path replaced
    /// rather than followed.
    #[cfg(unix)]
    #[test]
    fn an_open_private_file_replaces_what_it_finds() {
        use std::io::Write as _;
        let root = temp_dir("openfile");
        let victim = root.join("victim");
        std::fs::write(&victim, b"do not touch").unwrap();
        let path = root.join("app.log");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        let mut file = create_private_file(&path).unwrap();
        file.write_all(b"warning\n").unwrap();
        drop(file);

        assert_eq!(std::fs::read(&victim).unwrap(), b"do not touch");
        assert_eq!(std::fs::read(&path).unwrap(), b"warning\n");
        assert!(!std::fs::symlink_metadata(&path).unwrap().is_symlink());
        assert_eq!(mode_of(&path), 0o600);
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

    /// Group-writable is refused the same way: whoever shares the group can
    /// swap the files under the keys, and nothing here resolves membership
    /// to prove nobody does.
    #[cfg(unix)]
    #[test]
    fn a_group_writable_state_dir_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = temp_dir("groupw");
        let dir = root.join("jamstream");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();

        let err = create_private_dir(&dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("group"), "error was: {err}");
        assert!(err.to_string().contains("chmod 700"), "error was: {err}");

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        create_private_dir(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A real directory owned by a real other account: `/` belongs to root,
    /// and issuer keys must not live in a directory someone else owns even
    /// when its modes look tight. The dirs this suite creates prove the
    /// other side: they are ours and pass.
    #[cfg(unix)]
    #[test]
    fn a_state_dir_owned_by_another_account_is_refused() {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, everything is ours");
            return;
        }
        let err = check_exposure(Path::new("/")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            err.to_string().contains("owned by uid 0"),
            "error was: {err}"
        );

        let ours = temp_dir("owned");
        check_exposure(&ours).unwrap();
        let _ = std::fs::remove_dir_all(&ours);
    }

    /// The parse behind the SID grant, runnable everywhere: only the quoted
    /// two-field row with a SID-shaped second field counts, so a header row
    /// (`/nh` missing or ignored), a localized error line, or empty output
    /// yields nothing and the caller falls back.
    #[test]
    fn whoami_sid_parse_takes_the_sid_row_and_nothing_else() {
        let sid = "S-1-5-21-1004336348-1177238915-682003330-1001";
        assert_eq!(
            parse_whoami_sid(&format!("\"desktop\\sean\",\"{sid}\"\r\n")),
            Some(sid.to_owned())
        );
        // A header line ahead of the row is skipped, not mistaken for one,
        // and a domain name with a comma in it does not shift the SID field.
        assert_eq!(
            parse_whoami_sid(&format!(
                "\"User Name\",\"SID\"\r\n\"corp,inc\\sean\",\"{sid}\"\r\n"
            )),
            Some(sid.to_owned())
        );
        assert_eq!(parse_whoami_sid("\"User Name\",\"SID\"\r\n"), None);
        assert_eq!(
            parse_whoami_sid("ERROR: Unable to get user info.\r\n"),
            None
        );
        assert_eq!(parse_whoami_sid("\"sean\",\"S-1-\"\r\n"), None);
        assert_eq!(parse_whoami_sid("\"sean\",\"S-1-5-21-bogus\"\r\n"), None);
        assert_eq!(parse_whoami_sid(""), None);
    }

    #[cfg(windows)]
    fn acl_of(dir: &Path) -> String {
        let out = std::process::Command::new("icacls")
            .arg(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "icacls query failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
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

        let acl = acl_of(&dir);
        assert!(
            !acl.contains("(I)"),
            "inherited ACEs survived on a directory we created: {acl}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The case issue #333 is about: a `JAMSTREAM_STATE_DIR` that existed
    /// before this process did, carrying whatever ACEs its parent handed
    /// down, gets tightened on the write path rather than trusted.
    #[cfg(windows)]
    #[test]
    fn windows_pre_existing_state_dirs_are_hardened_too() {
        let root = temp_dir("preacl");
        // The shared-root shape, staged rather than assumed: the CI runner's
        // temp hands children explicit ACEs and nothing inherited, so the
        // parent gets an inheritable multi-account ACE of its own (Everyone,
        // by SID, read-only so nothing real is exposed while the test runs).
        let parent = root.join("shared");
        std::fs::create_dir(&parent).unwrap();
        let granted = std::process::Command::new("icacls")
            .arg(&parent)
            .args(["/grant", "*S-1-1-0:(OI)(CI)(RX)"])
            .output()
            .unwrap();
        assert!(
            granted.status.success(),
            "cannot stage the inheritable ACE: {}",
            String::from_utf8_lossy(&granted.stderr)
        );

        // A plain create, as a user making the dir themselves would: it
        // inherits the parent's ACEs, which is what makes the assertion
        // after create_private_dir mean something.
        let dir = parent.join("state");
        std::fs::create_dir(&dir).unwrap();
        let before = acl_of(&dir);
        assert!(
            before.contains("(I)"),
            "a plain directory carried no inherited ACEs, nothing to test: {before}"
        );

        create_private_dir(&dir).unwrap();
        let key = dir.join("issuer.key");
        write_private(&key, b"issuer_private_key=secret").unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), b"issuer_private_key=secret");

        let after = acl_of(&dir);
        assert!(
            !after.contains("(I)"),
            "inherited ACEs survived on a pre-existing directory: {after}"
        );
        // And the staged ACE is gone outright, not merely re-marked as
        // explicit: /findsid names the directory only on a match, which
        // holds in any locale.
        let found = std::process::Command::new("icacls")
            .arg(&dir)
            .args(["/findsid", "*S-1-1-0"])
            .output()
            .unwrap();
        assert!(found.status.success(), "icacls /findsid failed");
        let report = String::from_utf8_lossy(&found.stdout).into_owned();
        assert!(
            !report.contains(&dir.display().to_string()),
            "the Everyone ACE survived the hardening: {report}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A tightening that fails must surface as an error naming the fix, not
    /// as a log line the default filter drops while the key lands anyway.
    /// icacls itself cannot be made to fail deterministically here, so the
    /// injectable tool path stands in for it.
    #[cfg(windows)]
    #[test]
    fn windows_a_failed_hardening_is_an_error_with_the_remedy() {
        let root = temp_dir("aclfail");
        let dir = root.join("state");
        std::fs::create_dir(&dir).unwrap();

        let bogus = root.join("no-such-icacls.exe");
        let err = harden_dir_with(&bogus, &dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let msg = err.to_string();
        let named = dir.display().to_string();
        for expected in [
            named.as_str(),
            "JAMSTREAM_STATE_DIR",
            "%LOCALAPPDATA%",
            "icacls",
        ] {
            assert!(msg.contains(expected), "no {expected:?} in: {msg}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
