//! Outer datagram framing. Anything that fails to parse is dropped by the
//! caller without a response; the two rejects are the only exceptions, and
//! both are MAC'd with a key only the server and the one client that sent
//! the init can derive (see [`RejectKey`]).

use std::net::IpAddr;

use blake2::Blake2sMac;
use blake2::digest::{
    KeyInit, Mac,
    consts::{U16, U24},
};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use crate::Error;
use crate::ids::MemberId;

pub const TYPE_HANDSHAKE_INIT: u8 = 1;
pub const TYPE_HANDSHAKE_RESP: u8 = 2;
pub const TYPE_TRANSPORT: u8 = 3;
pub const TYPE_VERSION_REJECT: u8 = 4;
pub const TYPE_CAPACITY_REJECT: u8 = 5;
pub const TYPE_COOKIE_CHALLENGE: u8 = 6;
pub const TYPE_COOKIED_INIT: u8 = 7;

/// A cookie is a truncated MAC over the source address. 16 bytes is the same
/// width as the reject MACs and enough that guessing one is hopeless.
pub const COOKIE_BYTES: usize = 16;

/// XChaCha20Poly1305 nonce carried in a cookie challenge. Chosen by the
/// server and opaque to the receiver, which decrypts with whatever arrived.
pub const COOKIE_NONCE_BYTES: usize = 24;

/// The encrypted cookie in a challenge: the 16-byte cookie plus the 16-byte
/// Poly1305 tag.
pub const COOKIE_SEALED_BYTES: usize = COOKIE_BYTES + 16;

/// Channel byte inside decrypted transport plaintext.
pub const CHANNEL_MEDIA: u8 = 0;
pub const CHANNEL_CONTROL: u8 = 1;

const REJECT_DOMAIN: &[u8] = b"jamstream-version-reject";
/// Separate domain from the version reject: the two rejects share a key, and
/// one must never be replayable as the other.
const CAPACITY_DOMAIN: &[u8] = b"jamstream-capacity-reject";
const COOKIE_DOMAIN: &[u8] = b"jamstream-cookie";
/// Domain for the challenge nonce derivation; server-side only, see
/// [`challenge_nonce`].
const COOKIE_NONCE_DOMAIN: &[u8] = b"jamstream-cookie-nonce";

/// The key a version reject is authenticated with: a hash of the X25519
/// shared secret between the server's static key and the per-connection
/// static key of the client that sent the init being answered.
///
/// It cannot be the server's public key: that ships in every invite, so any
/// invite holder including a revoked one could forge a reject at any client
/// whose address they could see. A shared secret needs the server's static
/// private key on one side and that one client's private key on the other, and
/// neither is in an invite. [`crate::transport`] derives it.
#[derive(Clone)]
pub struct RejectKey(Zeroizing<[u8; 32]>);

impl RejectKey {
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> RejectKey {
        RejectKey(Zeroizing::new(bytes))
    }
}

impl std::fmt::Debug for RejectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RejectKey(..)")
    }
}

/// The rotating secret a handshake cookie is a MAC under. Server-side only:
/// clients echo the cookie they are handed and never compute one.
///
/// It is a hash of the server's static private key and an epoch number, so it
/// needs no random state and the core stays deterministic under the harness.
/// [`crate::transport::cookie_key`] derives it.
#[derive(Clone)]
pub struct CookieKey(Zeroizing<[u8; 32]>);

impl CookieKey {
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> CookieKey {
        CookieKey(Zeroizing::new(bytes))
    }
}

impl std::fmt::Debug for CookieKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CookieKey(..)")
    }
}

/// The AEAD key a cookie challenge is encrypted under:
/// Blake2s-256(`"jamstream-cookie-reply-key-v1"` || server static public key),
/// WireGuard's cookie-reply construction. Both ends can derive it, the server
/// from its own key and the client from the `server_pk` its invite carries,
/// so no new material is distributed. [`crate::transport::cookie_reply_key`]
/// derives it.
///
/// Not a secret against invite holders: anyone holding an invite knows
/// `server_pk`. What the AEAD adds is the binding, through its additional
/// authenticated data, of one challenge to the one init that drew it.
#[derive(Clone)]
pub struct CookieReplyKey(Zeroizing<[u8; 32]>);

impl CookieReplyKey {
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> CookieReplyKey {
        CookieReplyKey(Zeroizing::new(bytes))
    }
}

impl std::fmt::Debug for CookieReplyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CookieReplyKey(..)")
    }
}

#[derive(Debug, PartialEq)]
pub enum Packet<'a> {
    HandshakeInit {
        version: u16,
        noise: &'a [u8],
    },
    HandshakeResp {
        noise: &'a [u8],
    },
    Transport {
        member: MemberId,
        counter: u64,
        ciphertext: &'a [u8],
    },
    VersionReject {
        ours: u16,
        theirs: u16,
        mac: [u8; 16],
    },
    /// The role this peer's invite names is full. Carries no fields beyond
    /// the MAC: the client knows its own role, and anything else would be
    /// something the server tells an unadmitted peer for no reason.
    CapacityReject {
        mac: [u8; 16],
    },
    /// Come back with the cookie sealed inside. Sent instead of reading an
    /// init while the server is under handshake load. The cookie is encrypted
    /// and bound, through the AEAD's additional data, to the exact init it
    /// answers, so only a peer that knows `server_pk` and saw that init can
    /// produce one this client will accept. See [`build_cookie_challenge`]
    /// for the complete construction.
    CookieChallenge {
        nonce: [u8; COOKIE_NONCE_BYTES],
        sealed: [u8; COOKIE_SEALED_BYTES],
    },
    /// A handshake init carrying a cookie from an earlier challenge. A
    /// separate type rather than a field on [`Packet::HandshakeInit`], so the
    /// init's bytes are exactly what they always were.
    CookiedInit {
        cookie: [u8; COOKIE_BYTES],
        version: u16,
        noise: &'a [u8],
    },
}

pub fn parse(buf: &[u8]) -> Result<Packet<'_>, Error> {
    let (&tag, rest) = buf.split_first().ok_or(Error::Malformed)?;
    match tag {
        TYPE_HANDSHAKE_INIT => {
            if rest.len() < 2 {
                return Err(Error::Malformed);
            }
            let version = u16::from_le_bytes([rest[0], rest[1]]);
            Ok(Packet::HandshakeInit {
                version,
                noise: &rest[2..],
            })
        }
        TYPE_HANDSHAKE_RESP => Ok(Packet::HandshakeResp { noise: rest }),
        TYPE_TRANSPORT => {
            if rest.len() < 10 {
                return Err(Error::Malformed);
            }
            let member = MemberId(u16::from_le_bytes([rest[0], rest[1]]));
            let counter = u64::from_le_bytes(rest[2..10].try_into().unwrap());
            Ok(Packet::Transport {
                member,
                counter,
                ciphertext: &rest[10..],
            })
        }
        TYPE_VERSION_REJECT => {
            if rest.len() != 20 {
                return Err(Error::Malformed);
            }
            let ours = u16::from_le_bytes([rest[0], rest[1]]);
            let theirs = u16::from_le_bytes([rest[2], rest[3]]);
            let mac = rest[4..20].try_into().unwrap();
            Ok(Packet::VersionReject { ours, theirs, mac })
        }
        TYPE_CAPACITY_REJECT => {
            // Exactly the MAC, no more and no less: a trailing byte would
            // mean two encodings of the same packet.
            if rest.len() != 16 {
                return Err(Error::Malformed);
            }
            Ok(Packet::CapacityReject {
                mac: rest.try_into().unwrap(),
            })
        }
        TYPE_COOKIE_CHALLENGE => {
            if rest.len() != COOKIE_NONCE_BYTES + COOKIE_SEALED_BYTES {
                return Err(Error::Malformed);
            }
            let (nonce, sealed) = rest.split_at(COOKIE_NONCE_BYTES);
            Ok(Packet::CookieChallenge {
                nonce: nonce.try_into().unwrap(),
                sealed: sealed.try_into().unwrap(),
            })
        }
        TYPE_COOKIED_INIT => {
            if rest.len() < COOKIE_BYTES + 2 {
                return Err(Error::Malformed);
            }
            let (cookie, rest) = rest.split_at(COOKIE_BYTES);
            Ok(Packet::CookiedInit {
                cookie: cookie.try_into().unwrap(),
                version: u16::from_le_bytes([rest[0], rest[1]]),
                noise: &rest[2..],
            })
        }
        other => Err(Error::UnknownPacketType(other)),
    }
}

pub fn build_handshake_init(version: u16, noise: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + noise.len());
    out.push(TYPE_HANDSHAKE_INIT);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(noise);
    out
}

pub fn build_handshake_resp(noise: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + noise.len());
    out.push(TYPE_HANDSHAKE_RESP);
    out.extend_from_slice(noise);
    out
}

pub fn build_transport(member: MemberId, counter: u64, ciphertext: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(11 + ciphertext.len());
    append_transport_header(member, counter, &mut out);
    out.extend_from_slice(ciphertext);
    out
}

/// Appends just the transport header; the caller appends the ciphertext.
/// Lives beside `build_transport` so the layout has exactly one home.
pub fn append_transport_header(member: MemberId, counter: u64, out: &mut Vec<u8>) {
    out.push(TYPE_TRANSPORT);
    out.extend_from_slice(&member.0.to_le_bytes());
    out.extend_from_slice(&counter.to_le_bytes());
}

/// The reject echoes a slice of the init packet it answers, inside the MAC,
/// so it cannot be replayed against a different connection attempt. It is
/// fixed-size and smaller than the request: useless for amplification.
pub fn build_version_reject(
    key: &RejectKey,
    ours: u16,
    theirs: u16,
    init_packet: &[u8],
) -> Vec<u8> {
    let mac = reject_mac(key, ours, theirs, init_packet);
    let mut out = Vec::with_capacity(21);
    out.push(TYPE_VERSION_REJECT);
    out.extend_from_slice(&ours.to_le_bytes());
    out.extend_from_slice(&theirs.to_le_bytes());
    out.extend_from_slice(&mac);
    out
}

pub fn verify_version_reject(
    key: &RejectKey,
    ours: u16,
    theirs: u16,
    mac: &[u8; 16],
    init_packet_sent: &[u8],
) -> bool {
    equal(&reject_mac(key, ours, theirs, init_packet_sent), mac)
}

/// Tells a peer whose token has already verified that the role its invite
/// names is full.
///
/// Only reachable after the token check, so it is never an answer to an
/// arbitrary packet. It echoes the init inside the MAC like the version
/// reject, so it cannot be replayed at a later connection attempt, and it is
/// 17 bytes against an init of over 90: no use as an amplifier.
pub fn build_capacity_reject(key: &RejectKey, init_packet: &[u8]) -> Vec<u8> {
    let mac = capacity_mac(key, init_packet);
    let mut out = Vec::with_capacity(17);
    out.push(TYPE_CAPACITY_REJECT);
    out.extend_from_slice(&mac);
    out
}

pub fn verify_capacity_reject(key: &RejectKey, mac: &[u8; 16], init_packet_sent: &[u8]) -> bool {
    equal(&capacity_mac(key, init_packet_sent), mac)
}

/// The cookie for one source address under one epoch's secret.
///
/// The address and nothing else, WireGuard style: what a cookie proves is
/// that whoever holds it receives packets at the address it names, which is
/// the one thing a spoofed source cannot fake. Not the port, which changes
/// under NAT between a challenge and the answer to it, and not the init, so
/// one round trip covers a client's whole run of resends.
pub fn cookie_for(key: &CookieKey, source: IpAddr) -> [u8; COOKIE_BYTES] {
    let mut mac =
        <Blake2sMac<U16> as KeyInit>::new_from_slice(key.0.as_slice()).expect("32-byte key");
    mac.update(COOKIE_DOMAIN);
    // Tagged by family, so a v4 address and the v4-mapped v6 form of it
    // cannot be made to collide.
    match source {
        IpAddr::V4(v4) => {
            mac.update(&[4]);
            mac.update(&v4.octets());
        }
        IpAddr::V6(v6) => {
            mac.update(&[6]);
            mac.update(&v6.octets());
        }
    }
    mac.finalize().into_bytes().into()
}

pub fn cookie_matches(key: &CookieKey, source: IpAddr, cookie: &[u8; COOKIE_BYTES]) -> bool {
    equal(&cookie_for(key, source), cookie)
}

/// Builds a cookie challenge answering one handshake init. This is the wire
/// contract, exactly; a second implementation that follows it interoperates:
///
/// - AEAD: XChaCha20Poly1305 (RFC 8439 ChaCha20-Poly1305 with the HChaCha20
///   extension for a 24-byte nonce).
/// - Key: Blake2s-256 over `"jamstream-cookie-reply-key-v1"` followed by the
///   server's 32-byte X25519 static public key; see [`CookieReplyKey`].
/// - Plaintext: the 16-byte cookie of [`cookie_for`].
/// - Additional authenticated data: the complete
///   [`TYPE_HANDSHAKE_INIT`] datagram being answered, unabridged, from the
///   type byte through the two little-endian version bytes to the end of the
///   Noise message. This is what binds the reply to one init: an AEAD open
///   against any other init fails.
/// - Layout: the [`TYPE_COOKIE_CHALLENGE`] byte, the 24-byte nonce, then the
///   32-byte ciphertext (cookie plus Poly1305 tag). 57 bytes, always smaller
///   than any init the server answers.
///
/// The nonce is the sender's to choose and opaque to the receiver. This
/// server derives it deterministically (see `challenge_nonce`) so the core
/// stays clock-and-input driven under the harness; a random 24-byte nonce,
/// which is what WireGuard uses, would interoperate identically.
pub fn build_cookie_challenge(
    reply_key: &CookieReplyKey,
    cookie_key: &CookieKey,
    source: IpAddr,
    init_packet: &[u8],
) -> Vec<u8> {
    let cookie = cookie_for(cookie_key, source);
    let nonce = challenge_nonce(cookie_key, source, init_packet);
    let sealed = XChaCha20Poly1305::new(Key::from_slice(reply_key.0.as_slice()))
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &cookie,
                aad: init_packet,
            },
        )
        .expect("cookie seal is infallible at this size");
    let mut out = Vec::with_capacity(1 + COOKIE_NONCE_BYTES + COOKIE_SEALED_BYTES);
    out.push(TYPE_COOKIE_CHALLENGE);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    out
}

/// Opens a cookie challenge against the init this client actually sent.
/// `None` is a forgery, a tamper, or an answer to some other init, and the
/// caller drops the packet; the cookie inside a `Some` is bound to
/// `init_packet_sent` by the AEAD and safe to echo in a cookied init.
pub fn open_cookie_challenge(
    reply_key: &CookieReplyKey,
    nonce: &[u8; COOKIE_NONCE_BYTES],
    sealed: &[u8; COOKIE_SEALED_BYTES],
    init_packet_sent: &[u8],
) -> Option<[u8; COOKIE_BYTES]> {
    let plain = XChaCha20Poly1305::new(Key::from_slice(reply_key.0.as_slice()))
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: sealed,
                aad: init_packet_sent,
            },
        )
        .ok()?;
    plain.as_slice().try_into().ok()
}

/// The challenge nonce for one (epoch, source, init) triple.
///
/// A MAC under the epoch's cookie secret rather than a random draw, so the
/// server core stays deterministic under the harness with no RNG in it.
/// Every input the plaintext depends on is under the MAC: the epoch through
/// the key, the source address, and the init that is also the AEAD's
/// additional data. A repeated nonce therefore means the identical
/// (key, plaintext, AAD) triple, which re-produces the identical ciphertext
/// and leaks nothing, and two different plaintexts can share a nonce only by
/// a 192-bit collision. Keyed so the nonce is no oracle for confirming a
/// cookie guess offline.
fn challenge_nonce(
    key: &CookieKey,
    source: IpAddr,
    init_packet: &[u8],
) -> [u8; COOKIE_NONCE_BYTES] {
    let mut mac =
        <Blake2sMac<U24> as KeyInit>::new_from_slice(key.0.as_slice()).expect("32-byte key");
    mac.update(COOKIE_NONCE_DOMAIN);
    match source {
        IpAddr::V4(v4) => {
            mac.update(&[4]);
            mac.update(&v4.octets());
        }
        IpAddr::V6(v6) => {
            mac.update(&[6]);
            mac.update(&v6.octets());
        }
    }
    mac.update(init_packet);
    mac.finalize().into_bytes().into()
}

/// The same first flight as [`build_handshake_init`] with a cookie in front of
/// it, so a client offering both sends the identical Noise message either way
/// and the server's cached response still fits whichever arrives.
pub fn build_cookied_init(cookie: &[u8; COOKIE_BYTES], version: u16, noise: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + COOKIE_BYTES + noise.len());
    out.push(TYPE_COOKIED_INIT);
    out.extend_from_slice(cookie);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(noise);
    out
}

fn reject_mac(key: &RejectKey, ours: u16, theirs: u16, init_packet: &[u8]) -> [u8; 16] {
    let mut mac = keyed(key, REJECT_DOMAIN);
    mac.update(&ours.to_le_bytes());
    mac.update(&theirs.to_le_bytes());
    mac.update(&init_packet[..init_packet.len().min(64)]);
    mac.finalize().into_bytes().into()
}

fn capacity_mac(key: &RejectKey, init_packet: &[u8]) -> [u8; 16] {
    let mut mac = keyed(key, CAPACITY_DOMAIN);
    mac.update(&init_packet[..init_packet.len().min(64)]);
    mac.finalize().into_bytes().into()
}

fn keyed(key: &RejectKey, domain: &[u8]) -> Blake2sMac<U16> {
    let mut mac =
        <Blake2sMac<U16> as KeyInit>::new_from_slice(key.0.as_slice()).expect("32-byte key");
    mac.update(domain);
    mac
}

/// Not an oracle worth constant-time care, but it costs nothing.
fn equal(expected: &[u8; 16], got: &[u8; 16]) -> bool {
    expected
        .iter()
        .zip(got.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Splits decrypted transport plaintext into its channel and body.
pub fn split_channel(plain: &[u8]) -> Result<(u8, &[u8]), Error> {
    let (&chan, body) = plain.split_first().ok_or(Error::Malformed)?;
    Ok((chan, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_round_trips() {
        let p = build_handshake_init(1, b"noise-bytes");
        assert_eq!(
            parse(&p).unwrap(),
            Packet::HandshakeInit {
                version: 1,
                noise: b"noise-bytes"
            }
        );

        let p = build_handshake_resp(b"resp");
        assert_eq!(parse(&p).unwrap(), Packet::HandshakeResp { noise: b"resp" });

        let p = build_transport(MemberId(9), 424242, b"ct");
        assert_eq!(
            parse(&p).unwrap(),
            Packet::Transport {
                member: MemberId(9),
                counter: 424242,
                ciphertext: b"ct"
            }
        );
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse(&[]).is_err());
        assert!(parse(&[TYPE_HANDSHAKE_INIT, 1]).is_err());
        assert!(parse(&[TYPE_TRANSPORT, 0, 0, 1, 2]).is_err());
        assert!(parse(&[TYPE_VERSION_REJECT; 5]).is_err());
        assert!(parse(&[99, 1, 2, 3]).is_err());
    }

    /// A capacity reject is a tag and a 16-byte MAC and nothing else. One
    /// byte short is a truncation and one byte long is a second encoding of
    /// the same packet; both are refused rather than tolerated.
    #[test]
    fn capacity_reject_is_exactly_seventeen_bytes() {
        let key = RejectKey::from_bytes([5u8; 32]);
        let init = build_handshake_init(1, &[0xAB; 96]);
        let reject = build_capacity_reject(&key, &init);
        assert_eq!(reject.len(), 17);
        assert!(reject.len() < init.len(), "never larger than the init");
        for len in [0usize, 1, 15, 17, 64] {
            let mut wrong = vec![TYPE_CAPACITY_REJECT];
            wrong.extend(std::iter::repeat_n(0u8, len));
            assert_eq!(
                parse(&wrong).is_ok(),
                len == 16,
                "{len} MAC bytes parsed wrongly"
            );
        }
    }

    /// Same fence as the invite and handshake payload vectors: these are
    /// bytes on a wire between two builds, so the encoding is pinned rather
    /// than merely round-tripped. Fix the encoding, not the vector.
    #[test]
    fn reject_wire_encodings_are_pinned() {
        let key = RejectKey::from_bytes([3u8; 32]);
        let init = build_handshake_init(9, b"a-fixed-init-for-the-vector");

        let version = build_version_reject(&key, 1, 9, &init);
        assert_eq!(
            data_encoding::HEXLOWER.encode(&version),
            "0401000900935450b7abba314a3227a9d2de11fdeb",
            "version reject encoding drifted"
        );

        let capacity = build_capacity_reject(&key, &init);
        assert_eq!(
            data_encoding::HEXLOWER.encode(&capacity),
            "0500f57a286e8cad38de17443b484d60ea",
            "capacity reject encoding drifted"
        );

        // The transport header is on every media and control datagram a
        // session sends, and it is the AEAD's associated framing: a member id
        // or counter that changed width or endianness would make every
        // shipped build unable to open the other's packets.
        let mut header = Vec::new();
        append_transport_header(MemberId(0x0102), 0x0102_0304_0506_0708, &mut header);
        assert_eq!(
            data_encoding::HEXLOWER.encode(&header),
            "0302010807060504030201",
            "transport header encoding drifted"
        );
        assert_eq!(header.len(), 11);
        // build_transport is the same header with the ciphertext appended, so
        // there is one layout and not two.
        let mut with_ct = header.clone();
        with_ct.extend_from_slice(b"ct");
        assert_eq!(
            build_transport(MemberId(0x0102), 0x0102_0304_0506_0708, b"ct"),
            with_ct
        );

        let cookie_key = CookieKey::from_bytes([6u8; 32]);
        let reply_key = CookieReplyKey::from_bytes([7u8; 32]);
        let src: IpAddr = "203.0.113.7".parse().unwrap();
        let cookie = cookie_for(&cookie_key, src);
        let challenge = build_cookie_challenge(&reply_key, &cookie_key, src, &init);
        assert_eq!(
            data_encoding::HEXLOWER.encode(&challenge),
            "06d0377c257164b14d157ec21be2a5a1960ebd6d4392001991419e4f0212d68a\
             9df1ca5282e01ac38fcb02b2f81101651cfafffdddd889eb3f",
            "cookie challenge encoding drifted"
        );
        // And the pinned bytes open back to the very cookie that was sealed,
        // so the vector pins the whole construction, not just its length.
        let Packet::CookieChallenge { nonce, sealed } = parse(&challenge).unwrap() else {
            panic!("wrong packet type");
        };
        assert_eq!(
            open_cookie_challenge(&reply_key, &nonce, &sealed, &init),
            Some(cookie)
        );
        let cookied = build_cookied_init(&cookie, 9, b"a-fixed-init-for-the-vector");
        assert_eq!(
            data_encoding::HEXLOWER.encode(&cookied),
            "072bc7213d87f96d944f831f5d6209ef93\
             0900612d66697865642d696e69742d666f722d7468652d766563746f72",
            "cookied init encoding drifted"
        );
        // The cookied form carries the same Noise bytes as the plain one, so a
        // client offering both sends one handshake and not two.
        let Packet::HandshakeInit { noise, .. } = parse(&init).unwrap() else {
            panic!("wrong packet type");
        };
        let Packet::CookiedInit {
            noise: cookied_noise,
            version: cookied_version,
            ..
        } = parse(&cookied).unwrap()
        else {
            panic!("wrong packet type");
        };
        assert_eq!(cookied_noise, noise);
        assert_eq!(cookied_version, 9);

        // Appending the rejects and the cookie types left the tags of every
        // earlier packet alone: same rule as the postcard variants in
        // control.rs, which appends and never reorders.
        assert_eq!(build_handshake_init(1, b"")[0], TYPE_HANDSHAKE_INIT);
        assert_eq!(build_handshake_resp(b"")[0], TYPE_HANDSHAKE_RESP);
        assert_eq!(build_transport(MemberId(0), 0, b"")[0], TYPE_TRANSPORT);
        assert_eq!(version[0], TYPE_VERSION_REJECT);
        assert_eq!(capacity[0], TYPE_CAPACITY_REJECT);
        assert_eq!(
            [
                TYPE_HANDSHAKE_INIT,
                TYPE_HANDSHAKE_RESP,
                TYPE_TRANSPORT,
                TYPE_VERSION_REJECT,
                TYPE_CAPACITY_REJECT,
                TYPE_COOKIE_CHALLENGE,
                TYPE_COOKIED_INIT
            ],
            [1, 2, 3, 4, 5, 6, 7]
        );
    }

    /// A cookie proves the holder receives packets at one address, so it is
    /// bound to that address and to nothing else: not the port, which NAT
    /// changes between a challenge and the answer to it, and not the epoch
    /// secret's neighbour.
    #[test]
    fn a_cookie_is_bound_to_one_address_under_one_secret() {
        let key = CookieKey::from_bytes([1u8; 32]);
        let other = CookieKey::from_bytes([2u8; 32]);
        let v4: IpAddr = "203.0.113.7".parse().unwrap();
        let cookie = cookie_for(&key, v4);
        assert!(cookie_matches(&key, v4, &cookie));
        assert!(!cookie_matches(&other, v4, &cookie));
        assert!(!cookie_matches(
            &key,
            "203.0.113.8".parse().unwrap(),
            &cookie
        ));

        // A v4 address and its v4-mapped v6 spelling are different addresses,
        // and the family tag keeps them from colliding.
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
        assert!(!cookie_matches(&key, mapped, &cookie));
        // A v6 host routinely holds a whole /64, and each address in it gets
        // its own cookie: this is proof of reachability, not of identity.
        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "2001:db8::2".parse().unwrap();
        assert_ne!(cookie_for(&key, a), cookie_for(&key, b));
    }

    /// A challenge answers exactly one init. Opened against any other init,
    /// under any other server key, or after a flipped bit anywhere in the
    /// packet, the AEAD refuses: this is what an off-path forger runs into,
    /// because a challenge the victim will accept requires both the reply key
    /// and the exact bytes of the init being answered.
    #[test]
    fn a_challenge_opens_only_against_the_init_it_answers() {
        let reply_key = CookieReplyKey::from_bytes([7u8; 32]);
        let other_key = CookieReplyKey::from_bytes([8u8; 32]);
        let cookie_key = CookieKey::from_bytes([6u8; 32]);
        let src: IpAddr = "203.0.113.7".parse().unwrap();
        let init = build_handshake_init(2, b"the-init-being-answered");
        let challenge = build_cookie_challenge(&reply_key, &cookie_key, src, &init);
        let Packet::CookieChallenge { nonce, sealed } = parse(&challenge).unwrap() else {
            panic!("wrong packet type");
        };

        assert_eq!(
            open_cookie_challenge(&reply_key, &nonce, &sealed, &init),
            Some(cookie_for(&cookie_key, src))
        );
        // A different init, which is what a challenge sprayed at some other
        // client's handshake is; a different key, which is what a forger
        // without the server public key holds.
        let other_init = build_handshake_init(2, b"somebody-elses-init");
        assert_eq!(
            open_cookie_challenge(&reply_key, &nonce, &sealed, &other_init),
            None
        );
        assert_eq!(
            open_cookie_challenge(&other_key, &nonce, &sealed, &init),
            None
        );
        // Any flipped bit, nonce included, is a refusal.
        for i in 0..(COOKIE_NONCE_BYTES + COOKIE_SEALED_BYTES) {
            let mut bad = challenge.clone();
            bad[1 + i] ^= 0x01;
            let Packet::CookieChallenge { nonce, sealed } = parse(&bad).unwrap() else {
                panic!("wrong packet type");
            };
            assert_eq!(
                open_cookie_challenge(&reply_key, &nonce, &sealed, &init),
                None,
                "a challenge with byte {i} flipped still opened"
            );
        }

        // Deterministic on purpose: the same (key, epoch, source, init) is
        // the same packet, so the server core needs no RNG. Any other input
        // moves the nonce.
        assert_eq!(
            challenge,
            build_cookie_challenge(&reply_key, &cookie_key, src, &init)
        );
        let elsewhere = build_cookie_challenge(
            &reply_key,
            &cookie_key,
            "203.0.113.8".parse().unwrap(),
            &init,
        );
        assert_ne!(challenge[1..25], elsewhere[1..25], "nonce ignored the ip");
        let another = build_cookie_challenge(&reply_key, &cookie_key, src, &other_init);
        assert_ne!(challenge[1..25], another[1..25], "nonce ignored the init");
        let rotated =
            build_cookie_challenge(&reply_key, &CookieKey::from_bytes([9u8; 32]), src, &init);
        assert_ne!(challenge[1..25], rotated[1..25], "nonce ignored the epoch");
    }

    /// A challenge is a tag, a nonce, and a sealed cookie; a cookied init
    /// needs at least a cookie and a version before any Noise bytes. Anything
    /// else is refused rather than read past the end or tolerated as a second
    /// encoding.
    #[test]
    fn cookie_packets_refuse_wrong_lengths() {
        for len in [0usize, 1, 16, 24, 55, 57, 64] {
            let mut wrong = vec![TYPE_COOKIE_CHALLENGE];
            wrong.extend(std::iter::repeat_n(0u8, len));
            assert_eq!(
                parse(&wrong).is_ok(),
                len == COOKIE_NONCE_BYTES + COOKIE_SEALED_BYTES,
                "{len}-byte challenge payload parsed wrongly"
            );
        }
        for len in [0usize, 1, 15, 16, 17, 18, 19] {
            let mut wrong = vec![TYPE_COOKIED_INIT];
            wrong.extend(std::iter::repeat_n(0u8, len));
            assert_eq!(
                parse(&wrong).is_ok(),
                len >= COOKIE_BYTES + 2,
                "{len}-byte cookied init payload parsed wrongly"
            );
        }
        // An empty Noise message parses and is refused later, by the same
        // minimum-size rule that keeps the server from answering a stub.
        let empty = build_cookied_init(&[0u8; COOKIE_BYTES], 1, b"");
        assert_eq!(
            parse(&empty).unwrap(),
            Packet::CookiedInit {
                cookie: [0u8; COOKIE_BYTES],
                version: 1,
                noise: b""
            }
        );
    }

    /// The two rejects share a key, so the MACs are domain separated: a
    /// capacity reject must not be replayable as a version reject or the
    /// other way round, and neither must answer a different init.
    #[test]
    fn capacity_reject_authenticates_and_is_not_a_version_reject() {
        let key = RejectKey::from_bytes([3u8; 32]);
        let other = RejectKey::from_bytes([4u8; 32]);
        let init = build_handshake_init(1, b"the-init-being-answered");
        let reject = build_capacity_reject(&key, &init);
        let Packet::CapacityReject { mac } = parse(&reject).unwrap() else {
            panic!("wrong packet type");
        };
        assert!(verify_capacity_reject(&key, &mac, &init));
        // Wrong key, wrong init echo: refused.
        assert!(!verify_capacity_reject(&other, &mac, &init));
        assert!(!verify_capacity_reject(&key, &mac, b"another-init"));
        // And the same MAC does not authenticate a version reject, whatever
        // versions are claimed with it.
        for ours in 0..4u16 {
            assert!(!verify_version_reject(&key, ours, 1, &mac, &init));
        }
        let Packet::VersionReject { mac: vmac, .. } =
            parse(&build_version_reject(&key, 1, 9, &init)).unwrap()
        else {
            panic!("wrong packet type");
        };
        assert!(!verify_capacity_reject(&key, &vmac, &init));
    }

    #[test]
    fn version_reject_authenticates() {
        let key = RejectKey::from_bytes([3u8; 32]);
        let other = RejectKey::from_bytes([4u8; 32]);
        let init = build_handshake_init(2, b"whatever");
        let reject = build_version_reject(&key, 1, 2, &init);
        let Packet::VersionReject { ours, theirs, mac } = parse(&reject).unwrap() else {
            panic!("wrong packet type");
        };
        assert_eq!((ours, theirs), (1, 2));
        assert!(verify_version_reject(&key, ours, theirs, &mac, &init));
        // Wrong key, wrong init echo, tampered versions: all refused.
        assert!(!verify_version_reject(&other, ours, theirs, &mac, &init));
        assert!(!verify_version_reject(
            &key,
            ours,
            theirs,
            &mac,
            b"other-init"
        ));
        assert!(!verify_version_reject(&key, 3, theirs, &mac, &init));
    }
}
