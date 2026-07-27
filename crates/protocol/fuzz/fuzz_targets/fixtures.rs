//! Deterministic key material and a byte-stream reader shared by the
//! targets that need more than a single `&[u8]` argument.
//!
//! Included with `#[path]` rather than living in a library so the fuzz
//! crate keeps the shape cargo-fuzz expects: every `[[bin]]` in Cargo.toml
//! is a fuzz target and nothing else. Each target compiles its own copy,
//! hence the blanket dead-code allow.

#![allow(dead_code)]

use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::{Initiator, Responder, Session, Welcome, derive_public};
use jamstream_protocol::wire;

/// Server static X25519 private key. Every 32-byte string is a usable
/// scalar (x25519 clamps), so a constant is fine here, and it has to be a
/// constant: the committed seed corpora were generated against this exact
/// key and are only meaningful against it.
pub const SERVER_PRIVATE: [u8; 32] = [
    0x91, 0x2c, 0x5d, 0x0e, 0x77, 0xa4, 0x18, 0x63, 0xbb, 0x02, 0xf1, 0x49, 0x8a, 0xd6, 0x35, 0x7e,
    0xc0, 0x11, 0x9f, 0x5a, 0x24, 0x68, 0xe3, 0x0d, 0x73, 0xba, 0x46, 0x8c, 0x1f, 0x52, 0xd9, 0x07,
];

/// Issuer signing seed, so minted invites are byte-identical every run.
pub const ISSUER_SEED: [u8; 32] = [
    0x4a, 0xd1, 0x60, 0x33, 0x8e, 0x0b, 0xc7, 0x25, 0x19, 0xf4, 0x5c, 0xa8, 0x72, 0x36, 0xed, 0x81,
    0x0f, 0x9a, 0x43, 0xb6, 0x28, 0x51, 0xcf, 0x7d, 0x14, 0xe2, 0x69, 0x30, 0xab, 0x58, 0x86, 0x3c,
];

pub const SESSION_ID: SessionId = SessionId([
    0x5a, 0x3f, 0x11, 0xc8, 0x04, 0x9d, 0x62, 0x7b, 0xe0, 0x2a, 0x57, 0xf3, 0x8c, 0x16, 0xd4, 0x69,
]);

pub const TOKEN_ID: TokenId = TokenId([
    0x33, 0x71, 0x0c, 0xa5, 0x4e, 0x92, 0x18, 0xbf, 0x27, 0x60, 0xdb, 0x35, 0x89, 0x1a, 0xe6, 0x74,
]);

/// The member id the fuzzed connection joins as. Not the host seat (0), so
/// host-only control paths stay out of the way.
pub const MEMBER: MemberId = MemberId(2);

/// Far enough out that `verify_token` never sees an expired token.
pub const EXPIRES_UNIX: u64 = 4_000_000_000;

pub fn issuer() -> Issuer {
    Issuer::from_bytes(&ISSUER_SEED)
}

pub fn server_public() -> [u8; 32] {
    derive_public(&SERVER_PRIVATE).expect("32-byte private key")
}

/// The one invite every handshake in these targets is driven from.
pub fn invite() -> Invite {
    issuer().mint(
        SESSION_ID,
        vec!["192.0.2.4:43210".parse().expect("literal address")],
        server_public(),
        Token {
            member_id: MEMBER,
            role: Role::Musician,
            name_hint: None,
            expires_unix: EXPIRES_UNIX,
            jti: TOKEN_ID,
        },
    )
}

/// Runs a genuine Noise IK handshake to completion and hands back both
/// halves of the resulting transport: `(client, server)`.
///
/// The initiator's ephemeral and static keys come from the OS RNG inside
/// `Initiator::new`, so the session keys differ per execution. That is
/// deliberate and harmless: `Session::open` on attacker-supplied bytes
/// fails authentication whatever the keys are, so which branch an input
/// takes depends on the input's shape (lengths, counters, tamper
/// positions), not on the keys. Nothing here is a coverage oracle that
/// keys could perturb.
pub fn established() -> (Session, Session) {
    let invite = invite();
    let (initiator, init_packet) = Initiator::new(&invite).expect("initiator");
    let wire::Packet::HandshakeInit { version, noise } =
        wire::parse(&init_packet).expect("our own init parses")
    else {
        unreachable!("build_handshake_init produces an init packet")
    };
    let (_payload, responder) = Responder::read_init(&SERVER_PRIVATE, &SESSION_ID, version, noise)
        .expect("responder reads our own init");
    let welcome = Welcome {
        member_id: MEMBER,
        sample_clock: 480_000,
    };
    let (server, resp_packet) = responder.respond(&welcome).expect("responder responds");
    let wire::Packet::HandshakeResp { noise } =
        wire::parse(&resp_packet).expect("our own response parses")
    else {
        unreachable!("build_handshake_resp produces a response packet")
    };
    let (client, _welcome) = initiator.finish(noise).expect("initiator finishes");
    (client, server)
}

/// Cursor over the fuzzer's bytes. Every read is checked and short reads
/// end the op program, so the driver itself can never panic or overflow on
/// a truncated input.
pub struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { rest: data }
    }

    pub fn remaining(&self) -> usize {
        self.rest.len()
    }

    pub fn u8(&mut self) -> Option<u8> {
        let (&b, rest) = self.rest.split_first()?;
        self.rest = rest;
        Some(b)
    }

    pub fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Option<u64> {
        let b = self.take(8)?;
        Some(u64::from_le_bytes(b.try_into().expect("8 bytes")))
    }

    /// Exactly `n` bytes, or None if fewer remain.
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.rest.len() < n {
            return None;
        }
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        Some(head)
    }

    /// Up to `n` bytes: whatever is left when the input runs short. Used
    /// for payload bodies, where a truncated body is a fine thing to feed
    /// the code under test.
    pub fn take_up_to(&mut self, n: usize) -> &'a [u8] {
        let n = n.min(self.rest.len());
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        head
    }
}
