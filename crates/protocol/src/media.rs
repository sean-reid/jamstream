//! Media frames ride inside the encrypted transport, hand-packed rather
//! than postcard-encoded: this is the hot path and the layout is part of
//! the protocol. When loss protection is active, each packet piggybacks
//! the previous frame's payload so any single lost packet is recovered
//! from its successor.

use crate::Error;
use crate::wire::CHANNEL_MEDIA;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDuration {
    Ms2_5,
    Ms5,
    Ms10,
    Ms20,
}

impl FrameDuration {
    /// `const` so a caller can size a buffer or a tick from the wire's own
    /// answer instead of spelling the number again. Both of the numbers below
    /// were hand copied in jamstream-client for exactly that reason (#231).
    pub const fn samples(self) -> u32 {
        match self {
            FrameDuration::Ms2_5 => 120,
            FrameDuration::Ms5 => 240,
            FrameDuration::Ms10 => 480,
            FrameDuration::Ms20 => 960,
        }
    }

    /// `const` for the same reason as [`Self::samples`];
    /// `Duration::from_micros` is itself a const fn, so a tick interval can be
    /// derived from this.
    pub const fn micros(self) -> u32 {
        match self {
            FrameDuration::Ms2_5 => 2_500,
            FrameDuration::Ms5 => 5_000,
            FrameDuration::Ms10 => 10_000,
            FrameDuration::Ms20 => 20_000,
        }
    }

    fn to_bits(self) -> u8 {
        match self {
            FrameDuration::Ms2_5 => 0,
            FrameDuration::Ms5 => 1,
            FrameDuration::Ms10 => 2,
            FrameDuration::Ms20 => 3,
        }
    }

    fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0 => FrameDuration::Ms2_5,
            1 => FrameDuration::Ms5,
            2 => FrameDuration::Ms10,
            _ => FrameDuration::Ms20,
        }
    }
}

/// Both accessors have to stay const-callable, because another crate's buffer
/// sizes and tick interval are derived from them. Taking `const` off either one
/// fails to compile here rather than somewhere downstream.
const _: () = {
    assert!(FrameDuration::Ms2_5.samples() == 120);
    assert!(FrameDuration::Ms2_5.micros() == 2_500);
};

const FLAG_STEREO: u8 = 1 << 2;
const FLAG_REDUNDANT: u8 = 1 << 3;

/// Channel byte, seq, timestamp, flags: everything before the payload.
pub const HEADER_BYTES: usize = 14;

/// One Opus frame plus optionally the previous one.
#[derive(Debug, PartialEq)]
pub struct MediaFrame<'a> {
    /// Per-stream monotonic frame counter.
    pub seq: u32,
    /// Samples since session epoch, server clock domain.
    pub timestamp: u64,
    pub duration: FrameDuration,
    pub stereo: bool,
    pub payload: &'a [u8],
    /// The previous frame's payload, present when loss protection is on.
    pub redundant: Option<&'a [u8]>,
}

impl<'a> MediaFrame<'a> {
    pub fn encode(&self) -> Vec<u8> {
        let extra = self.redundant.map_or(0, |r| 2 + r.len());
        let mut out = Vec::with_capacity(15 + self.payload.len() + extra);
        out.push(CHANNEL_MEDIA);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.timestamp.to_le_bytes());
        let mut flags = self.duration.to_bits();
        if self.stereo {
            flags |= FLAG_STEREO;
        }
        if self.redundant.is_some() {
            flags |= FLAG_REDUNDANT;
        }
        out.push(flags);
        if let Some(red) = self.redundant {
            let len = u16::try_from(self.payload.len()).expect("opus frame fits u16");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(self.payload);
            out.extend_from_slice(red);
        } else {
            out.extend_from_slice(self.payload);
        }
        out
    }

    pub fn decode(buf: &'a [u8]) -> Result<Self, Error> {
        if buf.len() < HEADER_BYTES || buf[0] != CHANNEL_MEDIA {
            return Err(Error::Malformed);
        }
        let seq = u32::from_le_bytes(buf[1..5].try_into().unwrap());
        let timestamp = u64::from_le_bytes(buf[5..13].try_into().unwrap());
        let flags = buf[13];
        let duration = FrameDuration::from_bits(flags);
        let stereo = flags & FLAG_STEREO != 0;
        let body = &buf[HEADER_BYTES..];
        if flags & FLAG_REDUNDANT != 0 {
            if body.len() < 2 {
                return Err(Error::Malformed);
            }
            let plen = u16::from_le_bytes(body[..2].try_into().unwrap()) as usize;
            let rest = &body[2..];
            if rest.len() < plen {
                return Err(Error::Malformed);
            }
            Ok(MediaFrame {
                seq,
                timestamp,
                duration,
                stereo,
                payload: &rest[..plen],
                redundant: Some(&rest[plen..]),
            })
        } else {
            Ok(MediaFrame {
                seq,
                timestamp,
                duration,
                stereo,
                payload: body,
                redundant: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_plain() {
        let frame = MediaFrame {
            seq: 7,
            timestamp: 48_000 * 60,
            duration: FrameDuration::Ms2_5,
            stereo: false,
            payload: &[1, 2, 3, 4, 5],
            redundant: None,
        };
        let bytes = frame.encode();
        assert_eq!(MediaFrame::decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn round_trips_with_redundancy() {
        let frame = MediaFrame {
            seq: u32::MAX,
            timestamp: u64::MAX,
            duration: FrameDuration::Ms20,
            stereo: true,
            payload: &[9; 40],
            redundant: Some(&[8; 38]),
        };
        let bytes = frame.encode();
        assert_eq!(MediaFrame::decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn empty_payload_is_representable() {
        // DTX stays off, but a zero-length payload must not panic the codec.
        let frame = MediaFrame {
            seq: 0,
            timestamp: 0,
            duration: FrameDuration::Ms5,
            stereo: false,
            payload: &[],
            redundant: Some(&[]),
        };
        let bytes = frame.encode();
        assert_eq!(MediaFrame::decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn rejects_malformed() {
        assert!(MediaFrame::decode(&[]).is_err());
        assert!(MediaFrame::decode(&[CHANNEL_MEDIA; 5]).is_err());
        // Redundant flag set but truncated length prefix.
        let mut bytes = MediaFrame {
            seq: 1,
            timestamp: 2,
            duration: FrameDuration::Ms5,
            stereo: false,
            payload: &[1, 2, 3],
            redundant: Some(&[4, 5]),
        }
        .encode();
        bytes.truncate(15);
        assert!(MediaFrame::decode(&bytes).is_err());
        // Wrong channel byte.
        let mut ok = MediaFrame {
            seq: 1,
            timestamp: 2,
            duration: FrameDuration::Ms5,
            stereo: false,
            payload: &[1],
            redundant: None,
        }
        .encode();
        ok[0] = 9;
        assert!(MediaFrame::decode(&ok).is_err());
    }

    /// The same fence as the invite and reject vectors, for the one layout
    /// that had none: media is the hot path, hand-packed, and every other
    /// test of it encodes and decodes with the same code, so endianness, the
    /// flag bit positions, the duration bits, the 14-byte header and the
    /// redundant length prefix could all move and still round trip. These are
    /// bytes on a wire between two builds. Fix the encoding, not the vector.
    #[test]
    fn media_wire_encoding_is_pinned() {
        // Little-endian seq and timestamp, duration 0b11 with the stereo bit
        // at 1 << 2, no length prefix without redundancy.
        let plain = MediaFrame {
            seq: 0x0102_0304,
            timestamp: 48_000 * 60,
            duration: FrameDuration::Ms20,
            stereo: true,
            payload: &[0xAA, 0xBB, 0xCC],
            redundant: None,
        };
        assert_eq!(
            data_encoding::HEXLOWER.encode(&plain.encode()),
            "000403020100f22b000000000007aabbcc",
            "plain media frame encoding drifted"
        );

        // With redundancy the payload gains a u16 little-endian length prefix
        // and the previous frame's bytes follow it to the end of the packet.
        let redundant = MediaFrame {
            seq: 1,
            timestamp: 120,
            duration: FrameDuration::Ms2_5,
            stereo: false,
            payload: &[0x11, 0x22, 0x33],
            redundant: Some(&[0x44, 0x55]),
        };
        assert_eq!(
            data_encoding::HEXLOWER.encode(&redundant.encode()),
            "000100000078000000000000000803001122334455",
            "redundant media frame encoding drifted"
        );

        // The header is 14 bytes and the length prefix is the only thing
        // redundancy adds before the payload.
        assert_eq!(HEADER_BYTES, 14);
        assert_eq!(plain.encode().len(), HEADER_BYTES + 3);
        assert_eq!(redundant.encode().len(), HEADER_BYTES + 2 + 3 + 2);
        assert_eq!(plain.encode()[0], CHANNEL_MEDIA);
        assert_eq!(CHANNEL_MEDIA, 0);

        // Duration in the low two bits, stereo at 1 << 2, redundancy at
        // 1 << 3. A shifted bit would swap 5 ms for 2.5 ms on every packet.
        let flags_of = |d: FrameDuration, stereo: bool, red: bool| -> u8 {
            MediaFrame {
                seq: 0,
                timestamp: 0,
                duration: d,
                stereo,
                payload: &[],
                redundant: red.then_some(&[][..]),
            }
            .encode()[13]
        };
        for (d, bits) in [
            (FrameDuration::Ms2_5, 0b00u8),
            (FrameDuration::Ms5, 0b01),
            (FrameDuration::Ms10, 0b10),
            (FrameDuration::Ms20, 0b11),
        ] {
            assert_eq!(flags_of(d, false, false), bits, "{d:?} duration bits");
            assert_eq!(flags_of(d, true, false), bits | 0x04, "{d:?} stereo bit");
            assert_eq!(flags_of(d, false, true), bits | 0x08, "{d:?} redundant bit");
            assert_eq!(flags_of(d, true, true), bits | 0x0C, "{d:?} both flags");
            // And the duration a decoder reads back out of those bits is the
            // sample and microsecond count the mix tick is scheduled on.
            assert_eq!(FrameDuration::from_bits(bits), d);
        }
        assert_eq!(
            [120u32, 240, 480, 960],
            [
                FrameDuration::Ms2_5.samples(),
                FrameDuration::Ms5.samples(),
                FrameDuration::Ms10.samples(),
                FrameDuration::Ms20.samples()
            ]
        );
        assert_eq!(
            [2_500u32, 5_000, 10_000, 20_000],
            [
                FrameDuration::Ms2_5.micros(),
                FrameDuration::Ms5.micros(),
                FrameDuration::Ms10.micros(),
                FrameDuration::Ms20.micros()
            ]
        );
    }

    /// The length prefix is attacker-supplied on any packet a member can
    /// seal, so it is bounded by the bytes that actually arrived rather than
    /// trusted. Nothing here indexes past the end and nothing allocates on
    /// the strength of the claim.
    #[test]
    fn a_redundant_length_prefix_cannot_exceed_the_body() {
        let good = MediaFrame {
            seq: 1,
            timestamp: 2,
            duration: FrameDuration::Ms2_5,
            stereo: false,
            payload: &[7; 40],
            redundant: Some(&[8; 38]),
        }
        .encode();
        assert!(MediaFrame::decode(&good).is_ok());
        let body = good.len() - HEADER_BYTES - 2;

        // Every prefix from "exactly the body" upwards, including the widest
        // a u16 can state, against a packet of 80 payload bytes.
        for claim in [body, body + 1, 1_000, u16::MAX as usize] {
            let mut forged = good.clone();
            forged[HEADER_BYTES..HEADER_BYTES + 2].copy_from_slice(&(claim as u16).to_le_bytes());
            assert_eq!(
                MediaFrame::decode(&forged).is_ok(),
                claim <= body,
                "a prefix claiming {claim} of {body} bytes decoded wrongly"
            );
        }
        // A prefix at the body length leaves an empty redundant copy, which is
        // legal and distinct from no redundancy at all.
        let mut exact = good.clone();
        exact[HEADER_BYTES..HEADER_BYTES + 2].copy_from_slice(&(body as u16).to_le_bytes());
        assert_eq!(
            MediaFrame::decode(&exact).unwrap().redundant,
            Some(&[][..]),
            "the redundant flag must survive an empty redundant copy"
        );
    }

    #[test]
    fn all_durations_round_trip() {
        for d in [
            FrameDuration::Ms2_5,
            FrameDuration::Ms5,
            FrameDuration::Ms10,
            FrameDuration::Ms20,
        ] {
            let frame = MediaFrame {
                seq: 1,
                timestamp: 1,
                duration: d,
                stereo: false,
                payload: &[0xAB],
                redundant: None,
            };
            assert_eq!(MediaFrame::decode(&frame.encode()).unwrap().duration, d);
        }
    }
}
