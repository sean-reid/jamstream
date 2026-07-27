//! Outer datagram framing. Anything that fails to parse is dropped by the
//! caller without a response; the version reject is the single exception,
//! and it is MAC'd with a key only the server and the one client that sent
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

/// Channel byte inside decrypted transport plaintext.
pub const CHANNEL_MEDIA: u8 = 0;
pub const CHANNEL_CONTROL: u8 = 1;

const REJECT_DOMAIN: &[u8] = b"jamstream-version-reject";

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
    let expected = reject_mac(key, ours, theirs, init_packet_sent);
    // Not an oracle worth constant-time care, but it costs nothing.
    expected
        .iter()
        .zip(mac.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn reject_mac(key: &RejectKey, ours: u16, theirs: u16, init_packet: &[u8]) -> [u8; 16] {
    let mut mac =
        <Blake2sMac<U16> as KeyInit>::new_from_slice(key.0.as_slice()).expect("32-byte key");
    mac.update(REJECT_DOMAIN);
    mac.update(&ours.to_le_bytes());
    mac.update(&theirs.to_le_bytes());
    mac.update(&init_packet[..init_packet.len().min(64)]);
    mac.finalize().into_bytes().into()
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
