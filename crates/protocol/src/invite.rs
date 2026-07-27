//! Invites are self-contained: possessing a valid one is necessary and
//! sufficient to join a session. The issuer signature is checked by the
//! server, which learned the issuer's public key at boot. The client cannot
//! verify an invite offline; it trusts the channel it received it through,
//! and the Noise handshake stops anyone who tampered with the server key
//! from completing a connection.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

use crate::Error;
use crate::ids::{MemberId, Role, SessionId, TokenId};

const URL_PREFIX: &str = "jamstream://join/";
const SIGN_DOMAIN: &[u8] = b"jamstream-token-v1";

/// A raw Ed25519 signature: 64 bytes, R then s.
///
/// Wire-carried crypto material is deliberately typed as a plain fixed-size
/// array rather than `ed25519_dalek::Signature`, and converted at the edges.
/// The reason is concrete: `ed25519` v3 changed `Signature`'s `Serialize`
/// impl from `serialize_tuple(64)` to `serdect`'s `serialize_bytes`, which
/// made postcard emit an extra `0x40` length prefix and silently altered our
/// invite and handshake bytes. Our wire format is ours to define, so no
/// dependency's serde impl gets a vote in it. Do not "simplify" this back to
/// the dalek type; `invite_wire_encoding_is_pinned` and
/// `handshake_payload_wire_encoding_is_pinned` will fail if you do.
pub type SignatureBytes = [u8; 64];

/// Serde adapter for [`SignatureBytes`], used via `#[serde(with = "sig_bytes")]`.
///
/// `serde` implements its array impls only up to length 32, so a 64-byte array
/// needs a hand-written adapter regardless. This one encodes as a fixed-size
/// tuple of 64 `u8`s, which postcard writes as 64 bare bytes with no length
/// prefix. That is byte-for-byte what the 0.1.1-beta release emitted, and it
/// is now pinned by golden vectors rather than inherited from a dependency.
pub mod sig_bytes {
    use super::SignatureBytes;
    use serde::{Deserializer, Serializer, de, ser::SerializeTuple};

    pub fn serialize<S: Serializer>(
        bytes: &SignatureBytes,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut tup = serializer.serialize_tuple(64)?;
        for byte in bytes {
            tup.serialize_element(byte)?;
        }
        tup.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<SignatureBytes, D::Error> {
        struct SigVisitor;

        impl<'de> de::Visitor<'de> for SigVisitor {
            type Value = SignatureBytes;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a 64-byte Ed25519 signature")
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = [0u8; 64];
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(i, &self))?;
                }
                Ok(out)
            }
        }

        deserializer.deserialize_tuple(64, SigVisitor)
    }
}

/// Per-person admission token, signed by the session issuer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Token {
    pub member_id: MemberId,
    pub role: Role,
    /// Display name suggestion shown until the member picks their own.
    pub name_hint: Option<String>,
    /// Unix seconds. Defaults to the session's maximum duration.
    pub expires_unix: u64,
    /// Revocation handle; the host can invalidate one invite mid-session.
    pub jti: TokenId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invite {
    pub session_id: SessionId,
    /// Candidate addresses for the session server, tried in order.
    pub addresses: Vec<SocketAddr>,
    /// Server static Noise public key; authenticates the server on the
    /// first handshake flight.
    pub server_pk: [u8; 32],
    pub token: Token,
    /// Raw 64-byte Ed25519 signature; see [`SignatureBytes`] for why this is
    /// an array and not `ed25519_dalek::Signature`.
    #[serde(with = "sig_bytes")]
    pub signature: SignatureBytes,
}

/// Session issuer keypair. Lives only on the host machine.
pub struct Issuer {
    key: SigningKey,
}

impl Issuer {
    pub fn generate() -> Self {
        Self {
            key: SigningKey::from_bytes(&crate::rand_bytes()),
        }
    }

    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(bytes),
        }
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.key.to_bytes()
    }

    pub fn public_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    pub fn mint(
        &self,
        session_id: SessionId,
        addresses: Vec<SocketAddr>,
        server_pk: [u8; 32],
        token: Token,
    ) -> Invite {
        let msg = claims(&session_id, &server_pk, &token);
        let signature = self.key.sign(&msg).to_bytes();
        Invite {
            session_id,
            addresses,
            server_pk,
            token,
            signature,
        }
    }
}

/// Server-side check at admission time. `now_unix` comes from the caller so
/// the check stays deterministic under test.
pub fn verify_token(
    issuer_pk: &VerifyingKey,
    session_id: &SessionId,
    server_pk: &[u8; 32],
    token: &Token,
    signature: &SignatureBytes,
    now_unix: u64,
) -> Result<(), Error> {
    let msg = claims(session_id, server_pk, token);
    // `Signature::from_bytes` is infallible: malformed encodings are rejected
    // by `verify` below, exactly as before.
    issuer_pk
        .verify(&msg, &Signature::from_bytes(signature))
        .map_err(|_| Error::Token("bad signature"))?;
    if now_unix >= token.expires_unix {
        return Err(Error::Token("expired"));
    }
    Ok(())
}

fn claims(session_id: &SessionId, server_pk: &[u8; 32], token: &Token) -> Vec<u8> {
    let mut msg = SIGN_DOMAIN.to_vec();
    let body = postcard::to_stdvec(&(session_id, server_pk, token)).expect("claims serialize");
    msg.extend_from_slice(&body);
    msg
}

impl Invite {
    /// `jamstream://join/<blob>`. The bare blob is also accepted on parse,
    /// for people pasting into a text field.
    pub fn encode(&self) -> String {
        let blob = postcard::to_stdvec(self).expect("invite serialize");
        format!(
            "{URL_PREFIX}{}",
            data_encoding::BASE64URL_NOPAD.encode(&blob)
        )
    }

    pub fn decode(text: &str) -> Result<Self, Error> {
        let raw = text.trim();
        let blob = raw.strip_prefix(URL_PREFIX).unwrap_or(raw);
        let bytes = data_encoding::BASE64URL_NOPAD
            .decode(blob.as_bytes())
            .map_err(|_| Error::Invite("not valid encoding"))?;
        let invite: Invite =
            postcard::from_bytes(&bytes).map_err(|_| Error::Invite("truncated or corrupt"))?;
        if invite.addresses.is_empty() {
            return Err(Error::Invite("no server address"));
        }
        Ok(invite)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_token(expires: u64) -> Token {
        Token {
            member_id: MemberId(3),
            role: Role::Musician,
            name_hint: Some("ana".into()),
            expires_unix: expires,
            jti: TokenId::generate(),
        }
    }

    fn sample_invite(issuer: &Issuer, expires: u64) -> Invite {
        issuer.mint(
            SessionId::generate(),
            vec!["203.0.113.10:43210".parse().unwrap()],
            [7u8; 32],
            sample_token(expires),
        )
    }

    #[test]
    fn round_trips_through_url() {
        let issuer = Issuer::generate();
        let invite = sample_invite(&issuer, 10_000);
        let encoded = invite.encode();
        assert!(encoded.starts_with(URL_PREFIX));
        assert_eq!(Invite::decode(&encoded).unwrap(), invite);
        // Bare blob without the scheme prefix also parses.
        let bare = encoded.strip_prefix(URL_PREFIX).unwrap();
        assert_eq!(Invite::decode(bare).unwrap(), invite);
    }

    #[test]
    fn verifies_and_rejects() {
        let issuer = Issuer::generate();
        let invite = sample_invite(&issuer, 10_000);
        let pk = issuer.public_key();
        let ok = verify_token(
            &pk,
            &invite.session_id,
            &invite.server_pk,
            &invite.token,
            &invite.signature,
            5_000,
        );
        assert!(ok.is_ok());

        // Expired.
        assert!(
            verify_token(
                &pk,
                &invite.session_id,
                &invite.server_pk,
                &invite.token,
                &invite.signature,
                10_000,
            )
            .is_err()
        );

        // Tampered role.
        let mut tampered = invite.token.clone();
        tampered.role = Role::Listener;
        assert!(
            verify_token(
                &pk,
                &invite.session_id,
                &invite.server_pk,
                &tampered,
                &invite.signature,
                5_000,
            )
            .is_err()
        );

        // Signed for a different server key.
        assert!(
            verify_token(
                &pk,
                &invite.session_id,
                &[8u8; 32],
                &invite.token,
                &invite.signature,
                5_000,
            )
            .is_err()
        );

        // Wrong issuer entirely.
        let other = Issuer::generate();
        assert!(
            verify_token(
                &other.public_key(),
                &invite.session_id,
                &invite.server_pk,
                &invite.token,
                &invite.signature,
                5_000,
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(Invite::decode("jamstream://join/%%%").is_err());
        assert!(Invite::decode("").is_err());
        assert!(Invite::decode("aGVsbG8").is_err());
    }

    /// The fixed inputs behind the golden vectors below. Everything is
    /// constant and Ed25519 signing is deterministic, so the encodings are
    /// stable byte-for-byte.
    fn pinned_token() -> Token {
        Token {
            member_id: MemberId(3),
            role: Role::Musician,
            name_hint: Some("ana".into()),
            expires_unix: 4_000_000_000,
            jti: TokenId([9u8; 16]),
        }
    }

    fn pinned_invite() -> Invite {
        Issuer::from_bytes(&[7u8; 32]).mint(
            SessionId([5u8; 16]),
            vec!["203.0.113.10:43210".parse().unwrap()],
            [7u8; 32],
            pinned_token(),
        )
    }

    /// The exact 149-byte invite blob produced by the 0.1.1-beta release
    /// (ed25519-dalek 2 / ed25519 2.2.3), generated against that version and
    /// pinned here as the compatibility reference.
    #[rustfmt::skip]
    const V2_INVITE_HEX: &str = "050505050505050505050505050505050100cb00710acad102070707070707070707070707070707070707070707070707070707070707070703000103616e6180d0acf30e09090909090909090909090909090909719eaeb59a4eac1698d844f7f649100f996eca2dffd9aa11d350ffaa08a26bf49f071be5d2902e70293c393fc28f96b1582321f19e8beaaf3e7970c0482d7801";

    /// Pins the on-the-wire invite encoding against the published beta.
    ///
    /// This is a regression fence, not a snapshot: if it fails, a dependency
    /// has changed *our* protocol bytes and shipped clients will stop
    /// matching. Fix the encoding, do not update the vector. See
    /// [`SignatureBytes`] for the incident that motivated it.
    #[test]
    fn invite_wire_encoding_is_pinned() {
        let invite = pinned_invite();
        let blob = postcard::to_stdvec(&invite).expect("serialize");
        assert_eq!(
            data_encoding::HEXLOWER.encode(&blob),
            V2_INVITE_HEX,
            "invite wire encoding drifted from the 0.1.1-beta bytes"
        );
        // 149, not 150: the signature is 64 bare bytes with no length prefix.
        assert_eq!(blob.len(), 149);
        assert_eq!(blob[blob.len() - 64..], invite.signature[..]);
        assert_eq!(Invite::decode(&invite.encode()).unwrap(), invite);
    }

    /// The compatibility proof that actually matters: an invite string issued
    /// by the published beta must still decode, and still verify, here.
    #[test]
    fn beta_issued_invite_still_decodes_and_verifies() {
        let v2_blob = data_encoding::HEXLOWER
            .decode(V2_INVITE_HEX.as_bytes())
            .unwrap();
        let url = format!(
            "{URL_PREFIX}{}",
            data_encoding::BASE64URL_NOPAD.encode(&v2_blob)
        );
        // The literal invite string a 0.1.1-beta host would have handed out.
        const V2_INVITE_URL: &str = "jamstream://join/BQUFBQUFBQUFBQUFBQUFBQEAywBxCsrRAgcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHAwABA2FuYYDQrPMOCQkJCQkJCQkJCQkJCQkJCXGerrWaTqwWmNhE9_ZJEA-Zbsot_9mqEdNQ_6oIomv0nwcb5dKQLnApPDk_wo-WsVgjIfGei-qvPnlwwEgteAE";
        assert_eq!(url, V2_INVITE_URL);
        let decoded = Invite::decode(&url).expect("beta invite must still decode");
        assert_eq!(decoded, pinned_invite());
        // Symmetry: what we emit today is the same string the beta emitted, so
        // beta clients can read our invites too, not just the reverse.
        assert_eq!(pinned_invite().encode(), V2_INVITE_URL);
        // And the beta-issued signature still verifies against the issuer.
        verify_token(
            &Issuer::from_bytes(&[7u8; 32]).public_key(),
            &decoded.session_id,
            &decoded.server_pk,
            &decoded.token,
            &decoded.signature,
            1_000,
        )
        .expect("beta invite signature must still verify");
    }
}
