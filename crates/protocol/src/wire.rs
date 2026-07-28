//! Outer datagram framing. Anything that fails to parse is dropped by the
//! caller without a response; the two rejects are the only exceptions, and
//! both are MAC'd with a key only the server and the one client that sent
//! the init can derive (see [`RejectKey`]).

use blake2::Blake2sMac;
use blake2::digest::{KeyInit, Mac, consts::U16};
use zeroize::Zeroizing;

use crate::Error;
use crate::ids::MemberId;

pub const TYPE_HANDSHAKE_INIT: u8 = 1;
pub const TYPE_HANDSHAKE_RESP: u8 = 2;
pub const TYPE_TRANSPORT: u8 = 3;
pub const TYPE_VERSION_REJECT: u8 = 4;
pub const TYPE_CAPACITY_REJECT: u8 = 5;

/// Channel byte inside decrypted transport plaintext.
pub const CHANNEL_MEDIA: u8 = 0;
pub const CHANNEL_CONTROL: u8 = 1;

const REJECT_DOMAIN: &[u8] = b"jamstream-version-reject";
/// Separate domain from the version reject: the two rejects share a key, and
/// one must never be replayable as the other.
const CAPACITY_DOMAIN: &[u8] = b"jamstream-capacity-reject";

/// The key a version reject is authenticated with: a hash of the X25519
/// shared secret between the server's static key and the per-connection
/// static key of the client that sent the init being answered.
///
/// It used to be the server's public key, which ships in every invite, so
/// any invite holder including a revoked one could forge a reject at any
/// client whose address they could see. A shared secret needs the server's
/// static private key on one side and that one client's private key on the
/// other, and neither is in an invite. [`crate::transport`] derives it.
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

        // Appending the capacity reject left the tags of every earlier packet
        // alone: same rule as the postcard variants in control.rs.
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
                TYPE_CAPACITY_REJECT
            ],
            [1, 2, 3, 4, 5]
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
