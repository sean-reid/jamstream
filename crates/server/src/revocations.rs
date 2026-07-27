//! Durable half of invite revocation. `ServerCore` holds the revoked token
//! ids in memory and reports every change as an event; this file is what makes
//! them survive the process.
//!
//! Why it has to: the systemd unit is `Restart=on-failure` with
//! `RestartSec=2`, so a revoked member who waits for, or causes, any exit had
//! their invite working again two seconds later, with the host seeing only a
//! reconnect.
//!
//! Format is one lowercase hex token id per line, appended. Append rather than
//! rewrite because the file is the record of a security decision: a truncated
//! rewrite that fails halfway would drop revocations, an append that fails
//! halfway leaves a short final line the loader ignores. Each append is
//! flushed and synced before the call returns, because the crash this defends
//! against can arrive immediately after.

use std::io::Write;
use std::path::{Path, PathBuf};

use jamstream_protocol::ids::TokenId;

/// Cap on lines the loader will take from one file. A session mints at most a
/// few dozen invites, so anything past this is a corrupt or hostile file and
/// reading it all would be the only unbounded allocation on the startup path.
const MAX_ENTRIES: usize = 4_096;

#[derive(Debug)]
pub struct Revocations {
    path: PathBuf,
}

impl Revocations {
    pub fn new(path: PathBuf) -> Revocations {
        Revocations { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every token id already revoked. A missing file means none, which is the
    /// normal first boot; a partly unreadable one yields what parsed, because
    /// dropping the whole list over one bad line would un-revoke the rest.
    pub fn load(&self) -> Vec<TokenId> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                tracing::error!(
                    error = %err,
                    path = %self.path.display(),
                    "cannot read the revocation list: revoked invites may work again"
                );
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for line in text.lines().take(MAX_ENTRIES) {
            match parse_jti(line.trim()) {
                Some(jti) => out.push(jti),
                None if line.trim().is_empty() => {}
                None => tracing::warn!(line, "skipping unparseable revocation entry"),
            }
        }
        out
    }

    /// Writes one revocation down, durably. Errors are logged and reported:
    /// the caller keeps serving either way, since refusing to run would turn a
    /// full disk into the denial of service the revocation was defending
    /// against.
    pub fn append(&self, jti: TokenId) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", hex(&jti.0))?;
        file.flush()?;
        file.sync_all()
    }
}

fn hex(bytes: &[u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_jti(s: &str) -> Option<TokenId> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(TokenId(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jamstream-revocations-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("revoked")
    }

    #[test]
    fn appends_and_reloads_in_order() {
        let store = Revocations::new(temp_path("roundtrip"));
        assert!(store.load().is_empty(), "no file means nothing revoked");
        let a = TokenId([1u8; 16]);
        let b = TokenId([0xABu8; 16]);
        store.append(a).unwrap();
        store.append(b).unwrap();
        assert_eq!(store.load(), vec![a, b]);
        // A second process reading the same path sees both, which is the
        // whole point.
        assert_eq!(Revocations::new(store.path().to_owned()).load(), vec![a, b]);
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }

    #[test]
    fn a_truncated_or_junk_line_does_not_lose_the_rest() {
        let path = temp_path("junk");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let good = TokenId([7u8; 16]);
        std::fs::write(
            &path,
            format!(
                "{}\n\nnot-hex\nzz{}\n0011\n{}",
                hex(&good.0),
                "0".repeat(30),
                hex(&TokenId([9u8; 16]).0)
            ),
        )
        .unwrap();
        let store = Revocations::new(path.clone());
        assert_eq!(store.load(), vec![good, TokenId([9u8; 16])]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_hostile_file_cannot_make_startup_allocate_without_bound() {
        let path = temp_path("huge");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let line = hex(&TokenId([3u8; 16]).0);
        let mut text = String::new();
        for _ in 0..MAX_ENTRIES + 100 {
            text.push_str(&line);
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        assert_eq!(Revocations::new(path.clone()).load().len(), MAX_ENTRIES);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
