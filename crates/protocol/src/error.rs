/// Every failure this crate reports.
///
/// # The contract on these messages
///
/// Each variant renders as a *fragment*: lowercase, no trailing punctuation,
/// naming its own subject. It has to read correctly on its own and after any
/// prefix a caller puts in front of it, because there are several and none of
/// them agree. Today the same value reaches a musician as the whole sentence
/// under the join field, as `error: {}` from the cli, and as `session: {}`
/// through the client's `LiveError`.
///
/// Skip the contract and the screen renders "invite is not valid: not
/// valid encoding", from a format string that says the invite is not
/// valid and a detail that says it again. So:
///
/// - Name the subject once. `invite has invalid encoding`, not
///   `invite is not valid: invalid invite encoding`.
/// - No word appears twice in one rendering. `messages_read_as_fragments`
///   enforces that for words of four letters or more, which is what a stutter
///   is made of.
/// - No leading capital and no full stop, so a caller can prefix or embed it.
/// - Do not write a detail that only parses after one particular caller's
///   words. The detail strings on `Invite` and `Token` complete the subject in
///   the format string and nothing else.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("packet too short or malformed")]
    Malformed,
    #[error("unknown packet type {0:#04x}")]
    UnknownPacketType(u8),
    #[error("protocol version {theirs} is not supported (this build speaks {ours})")]
    VersionMismatch { ours: u16, theirs: u16 },
    #[error("handshake failed")]
    Handshake(#[from] snow::Error),
    /// The detail completes "invite ...": a verb phrase, present tense.
    #[error("invite {0}")]
    Invite(&'static str),
    /// The detail completes "token ...", as with [`Error::Invite`].
    #[error("token {0}")]
    Token(&'static str),
    #[error("replayed or expired packet counter")]
    Replay,
    #[error("decryption failed")]
    Decrypt,
    #[error("encoding failed")]
    Encode(#[from] postcard::Error),
    #[error("control link is backed up")]
    LinkFull,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One value per variant. The match is exhaustive and has no wildcard, so
    /// a new variant does not compile until it is listed here and its message
    /// has been through the checks below.
    fn every_variant() -> Vec<Error> {
        let all = vec![
            Error::Malformed,
            Error::UnknownPacketType(7),
            Error::VersionMismatch { ours: 1, theirs: 3 },
            Error::Handshake(snow::Error::Decrypt),
            Error::Invite("has invalid encoding"),
            Error::Invite("is truncated or corrupt"),
            Error::Invite("has no server address"),
            Error::Token("has a bad signature"),
            Error::Token("has expired"),
            Error::Replay,
            Error::Decrypt,
            Error::Encode(postcard::Error::SerdeSerCustom),
            Error::LinkFull,
        ];
        for e in &all {
            match e {
                Error::Malformed
                | Error::UnknownPacketType(_)
                | Error::VersionMismatch { .. }
                | Error::Handshake(_)
                | Error::Invite(_)
                | Error::Token(_)
                | Error::Replay
                | Error::Decrypt
                | Error::Encode(_)
                | Error::LinkFull => {}
            }
        }
        all
    }

    /// The mechanical half of the contract in [`Error`]'s doc comment.
    ///
    /// The duplicate-word check is the one that matters: it is what the shipped
    /// "invite is not valid: not valid encoding" would have failed on, twice
    /// over ("not", if it were long enough to count, and "valid").
    #[test]
    fn messages_read_as_fragments() {
        for e in every_variant() {
            let msg = e.to_string();
            assert!(!msg.is_empty(), "{e:?} renders as nothing");
            let first = msg.chars().next().unwrap();
            assert!(
                !first.is_uppercase(),
                "{msg:?} starts with a capital, so it cannot follow a caller's prefix"
            );
            assert_eq!(msg.trim(), msg, "{msg:?} has leading or trailing space");
            assert!(
                !msg.ends_with('.') && !msg.ends_with('!'),
                "{msg:?} ends a sentence, so a caller cannot embed it"
            );
            assert!(!msg.contains('\n'), "{msg:?} spans lines");

            let mut seen: Vec<String> = Vec::new();
            for word in msg
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.chars().count() >= 4)
                .map(str::to_lowercase)
            {
                assert!(
                    !seen.contains(&word),
                    "{msg:?} says {word:?} twice, which is what a stutter is"
                );
                seen.push(word);
            }
        }
    }

    /// The renderings themselves, so a reword is a deliberate act and shows up
    /// in a diff next to the callers it changes. Three of these are read by a
    /// musician who pasted something that did not work.
    #[test]
    fn the_wording_is_pinned() {
        let rendered: Vec<String> = every_variant().iter().map(Error::to_string).collect();
        assert_eq!(
            rendered,
            vec![
                "packet too short or malformed",
                "unknown packet type 0x07",
                "protocol version 3 is not supported (this build speaks 1)",
                "handshake failed",
                "invite has invalid encoding",
                "invite is truncated or corrupt",
                "invite has no server address",
                "token has a bad signature",
                "token has expired",
                "replayed or expired packet counter",
                "decryption failed",
                "encoding failed",
                "control link is backed up",
            ]
        );
    }
}
