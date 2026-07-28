//! Parses /etc/jamstream/config, the flat key = value file cloud-init
//! writes at boot. Unknown keys are rejected: a typo in provisioning must
//! fail loudly at startup, not surface mid-jam.

use std::path::Path;

use jamstream_protocol::ids::SessionId;

#[derive(PartialEq)]
pub struct Config {
    pub session_id: SessionId,
    pub port: u16,
    pub server_private_key: Vec<u8>,
    pub issuer_public_key: [u8; 32],
    pub idle_shutdown_min: u32,
    pub max_duration_min: u32,
}

/// Redacts the server's static private key, the one secret on the VM that a
/// session's whole transport rests on. Everything else here is public or a
/// number and is worth seeing in a log. `BootConfig`, which is the same fields
/// on the host's side of the wire, redacts the same field for the same reason;
/// a derive here would have made one `tracing::debug!(?cfg)` in jamstreamd
/// enough to put the key in the journal.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("session_id", &self.session_id)
            .field("port", &self.port)
            .field("server_private_key", &"<redacted>")
            .field("issuer_public_key", &self.issuer_public_key)
            .field("idle_shutdown_min", &self.idle_shutdown_min)
            .field("max_duration_min", &self.max_duration_min)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("line {0}: expected key = value")]
    Syntax(usize),
    #[error("unknown key {0:?}")]
    UnknownKey(String),
    #[error("missing key {0:?}")]
    MissingKey(&'static str),
    #[error("bad value for {0}")]
    BadValue(&'static str),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut session_id_hex = None;
        let mut port = None;
        let mut server_private = None;
        let mut issuer_public = None;
        let mut idle = None;
        let mut max_dur = None;

        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or(ConfigError::Syntax(i + 1))?;
            let (key, value) = (key.trim(), value.trim());
            match key {
                "session_id_hex" => session_id_hex = Some(value.to_string()),
                "port" => port = Some(value.to_string()),
                "server_private_key_b64" => server_private = Some(value.to_string()),
                "issuer_public_key_b64" => issuer_public = Some(value.to_string()),
                "idle_shutdown_min" => idle = Some(value.to_string()),
                "max_duration_min" => max_dur = Some(value.to_string()),
                other => return Err(ConfigError::UnknownKey(other.to_string())),
            }
        }

        let session_id_hex = session_id_hex.ok_or(ConfigError::MissingKey("session_id_hex"))?;
        let session_bytes =
            hex_decode(&session_id_hex).ok_or(ConfigError::BadValue("session_id_hex"))?;
        let session_id = SessionId(
            session_bytes
                .try_into()
                .map_err(|_| ConfigError::BadValue("session_id_hex"))?,
        );

        let port = port
            .ok_or(ConfigError::MissingKey("port"))?
            .parse()
            .map_err(|_| ConfigError::BadValue("port"))?;

        let server_private = data_encoding::BASE64
            .decode(
                server_private
                    .ok_or(ConfigError::MissingKey("server_private_key_b64"))?
                    .as_bytes(),
            )
            .map_err(|_| ConfigError::BadValue("server_private_key_b64"))?;

        let issuer_public: [u8; 32] = data_encoding::BASE64
            .decode(
                issuer_public
                    .ok_or(ConfigError::MissingKey("issuer_public_key_b64"))?
                    .as_bytes(),
            )
            .map_err(|_| ConfigError::BadValue("issuer_public_key_b64"))?
            .try_into()
            .map_err(|_| ConfigError::BadValue("issuer_public_key_b64"))?;

        let idle_shutdown_min = idle
            .ok_or(ConfigError::MissingKey("idle_shutdown_min"))?
            .parse()
            .map_err(|_| ConfigError::BadValue("idle_shutdown_min"))?;
        let max_duration_min = max_dur
            .ok_or(ConfigError::MissingKey("max_duration_min"))?
            .parse()
            .map_err(|_| ConfigError::BadValue("max_duration_min"))?;

        Ok(Config {
            session_id,
            port,
            server_private_key: server_private,
            issuer_public_key: issuer_public,
            idle_shutdown_min,
            max_duration_min,
        })
    }
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> String {
        format!(
            "session_id_hex = {}\nport = 43210\nserver_private_key_b64 = {}\nissuer_public_key_b64 = {}\nidle_shutdown_min = 10\nmax_duration_min = 720\n",
            "00112233445566778899aabbccddeeff",
            data_encoding::BASE64.encode(&[9u8; 32]),
            data_encoding::BASE64.encode(&[7u8; 32]),
        )
    }

    #[test]
    fn parses_the_cloudinit_format() {
        let cfg = Config::parse(&valid()).unwrap();
        assert_eq!(cfg.port, 43210);
        assert_eq!(cfg.idle_shutdown_min, 10);
        assert_eq!(cfg.max_duration_min, 720);
        assert_eq!(cfg.server_private_key, vec![9u8; 32]);
        assert_eq!(cfg.issuer_public_key, [7u8; 32]);
        assert_eq!(
            cfg.session_id.0,
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ]
        );
    }

    /// The parsed config holds the server's static private key, so the first
    /// thing that formats one must not put it in the journal. jamstreamd runs
    /// on a VM whose journal survives the session, and a private key there is
    /// every future session's transport as well as this one's.
    #[test]
    fn debug_never_reveals_the_server_private_key() {
        let cfg = Config::parse(&valid()).unwrap();
        let rendered = format!("{cfg:?}");
        let key_b64 = data_encoding::BASE64.encode(&[9u8; 32]);
        assert!(!rendered.contains(&key_b64), "{rendered}");
        assert!(!rendered.contains("9, 9, 9"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        // The fields worth seeing in a log are still there.
        assert!(rendered.contains("43210"), "{rendered}");
    }

    #[test]
    fn comments_and_blank_lines_are_fine() {
        let text = format!("# jamstream\n\n{}", valid());
        assert!(Config::parse(&text).is_ok());
    }

    #[test]
    fn rejects_unknown_keys() {
        let text = format!("{}mystery = 1\n", valid());
        assert!(matches!(
            Config::parse(&text),
            Err(ConfigError::UnknownKey(k)) if k == "mystery"
        ));
    }

    #[test]
    fn rejects_missing_and_bad_values() {
        assert!(matches!(
            Config::parse("port = 43210\n"),
            Err(ConfigError::MissingKey("session_id_hex"))
        ));
        let bad_port = valid().replace("43210", "not-a-port");
        assert!(matches!(
            Config::parse(&bad_port),
            Err(ConfigError::BadValue("port"))
        ));
        let short_session = valid().replace("00112233445566778899aabbccddeeff", "0011");
        assert!(matches!(
            Config::parse(&short_session),
            Err(ConfigError::BadValue("session_id_hex"))
        ));
    }

    #[test]
    fn rejects_syntax_errors_with_line_numbers() {
        assert!(matches!(
            Config::parse("just some words\n"),
            Err(ConfigError::Syntax(1))
        ));
    }
}
