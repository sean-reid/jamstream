//! The control plane: everything that is not audio, carried over the same
//! socket with a small reliability layer. Sequence numbers per link,
//! cumulative ack plus a 32-bit selective-ack bitmap, retransmit on timeout
//! with exponential backoff. The module is time-free: callers pass
//! milliseconds in, which keeps it deterministic under the harness.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

use crate::Error;
use crate::ids::{MemberId, Role, TokenId};
use crate::wire::CHANNEL_CONTROL;

pub const MAX_CHAT_LEN: usize = 1_000;
pub const MAX_NAME_LEN: usize = 64;

const RTO_INITIAL_MS: u64 = 100;
const RTO_MAX_MS: u64 = 2_000;
const MAX_SENDS: u32 = 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub id: MemberId,
    pub role: Role,
    pub name: String,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMsg {
    /// Full roster snapshot; the server sends one on every change.
    Roster(Vec<MemberInfo>),
    Chat {
        from: MemberId,
        text: String,
    },
    /// A member shaping their own monitor mix.
    MixerSet {
        target: MemberId,
        gain_db: f32,
        pan: f32,
        muted: bool,
    },
    /// Host-controlled metronome; the click is mixed in server-side.
    MetronomeSet {
        bpm: u16,
        beats_per_bar: u8,
        enabled: bool,
    },
    /// A member enabling or disabling the click in their own mix.
    ClickEnable {
        enabled: bool,
    },
    Ping {
        nonce: u32,
        sent_ms: u64,
    },
    Pong {
        nonce: u32,
        sent_ms: u64,
    },
    /// Host invalidates one invite; the server drops the matching member.
    Revoke {
        jti: TokenId,
    },
    Bye {
        reason: String,
    },
    /// Server's once-per-second view of a musician's uplink, feeding the
    /// sender's redundancy decision. Percentages are 0..=100 over the last
    /// reporting window.
    ///
    /// Appended last on purpose: postcard encodes the variant index as a
    /// varint discriminant, so adding a variant at the end leaves every
    /// existing variant's bytes unchanged. A peer without this variant only
    /// fails to decode messages that actually carry it; protocol version 1
    /// is unreleased, so no such peer exists.
    Stats {
        uplink_loss_pct: f32,
        uplink_jitter_depth: u16,
        uplink_recovered_pct: f32,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct CtlPacket {
    /// Next sequence number the sender expects: everything below it has
    /// been received.
    ack: u64,
    /// Bit i set means `ack + 1 + i` was received out of order.
    ack_bits: u32,
    frame: Option<(u64, ControlMsg)>,
}

#[derive(Debug)]
struct Pending {
    seq: u64,
    msg: ControlMsg,
    next_send_ms: u64,
    sends: u32,
}

/// One reliable, ordered control link. Each side of a connection owns one.
#[derive(Debug, Default)]
pub struct ControlLink {
    next_seq: u64,
    pending: VecDeque<Pending>,
    recv_next: u64,
    out_of_order: BTreeMap<u64, ControlMsg>,
    need_ack: bool,
    dead: bool,
}

impl ControlLink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a message for reliable delivery. It goes on the wire at the
    /// next `poll`.
    pub fn send(&mut self, msg: ControlMsg) -> Result<(), Error> {
        match &msg {
            ControlMsg::Chat { text, .. } if text.len() > MAX_CHAT_LEN => {
                return Err(Error::Malformed);
            }
            ControlMsg::Bye { reason } if reason.len() > MAX_CHAT_LEN => {
                return Err(Error::Malformed);
            }
            _ => {}
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.push_back(Pending {
            seq,
            msg,
            next_send_ms: 0,
            sends: 0,
        });
        Ok(())
    }

    /// Returns the plaintext datagrams due now: fresh sends, retransmits,
    /// and a bare ack when one is owed and nothing else is going out.
    /// Callers seal each one into its own transport packet.
    pub fn poll(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let ack = self.recv_next;
        let ack_bits = self.ack_bits();
        for p in self.pending.iter_mut() {
            if now_ms < p.next_send_ms {
                continue;
            }
            if p.sends >= MAX_SENDS {
                self.dead = true;
                continue;
            }
            let backoff = RTO_INITIAL_MS << p.sends.min(6);
            p.next_send_ms = now_ms + backoff.min(RTO_MAX_MS);
            p.sends += 1;
            out.push(encode(&CtlPacket {
                ack,
                ack_bits,
                frame: Some((p.seq, p.msg.clone())),
            }));
            self.need_ack = false;
        }
        if self.need_ack && out.is_empty() {
            out.push(encode(&CtlPacket {
                ack,
                ack_bits,
                frame: None,
            }));
            self.need_ack = false;
        }
        out
    }

    /// Ingests one received control datagram (channel byte included) and
    /// returns any messages now deliverable in order.
    pub fn receive(&mut self, buf: &[u8]) -> Result<Vec<ControlMsg>, Error> {
        if buf.first() != Some(&CHANNEL_CONTROL) {
            return Err(Error::Malformed);
        }
        let pkt: CtlPacket = postcard::from_bytes(&buf[1..])?;

        // Their ack state clears our pending queue: everything below the
        // cumulative ack, plus whatever the selective bitmap covers.
        self.pending.retain(|p| {
            if p.seq < pkt.ack {
                return false;
            }
            if p.seq > pkt.ack && p.seq - pkt.ack <= 32 {
                let bit = (p.seq - pkt.ack - 1) as u32;
                if (pkt.ack_bits >> bit) & 1 == 1 {
                    return false;
                }
            }
            true
        });

        let mut delivered = Vec::new();
        if let Some((seq, msg)) = pkt.frame {
            self.need_ack = true;
            if seq >= self.recv_next && !self.out_of_order.contains_key(&seq) {
                self.out_of_order.insert(seq, msg);
                while let Some(next) = self.out_of_order.remove(&self.recv_next) {
                    delivered.push(next);
                    self.recv_next += 1;
                }
            }
        }
        Ok(delivered)
    }

    /// True once a message has been retransmitted past the give-up limit;
    /// the peer is unreachable and the caller should drop the connection.
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// Anything still awaiting acknowledgment?
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn ack_bits(&self) -> u32 {
        let mut bits = 0u32;
        for i in 0..32u64 {
            if self.out_of_order.contains_key(&(self.recv_next + 1 + i)) {
                bits |= 1 << i;
            }
        }
        bits
    }
}

fn encode(pkt: &CtlPacket) -> Vec<u8> {
    // Serialize straight into the datagram; no intermediate Vec.
    postcard::to_extend(pkt, vec![CHANNEL_CONTROL]).expect("control serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(n: u64) -> ControlMsg {
        ControlMsg::Chat {
            from: MemberId(1),
            text: format!("message {n}"),
        }
    }

    /// Shuttles every due datagram from `a` to `b`, dropping the ones whose
    /// index appears in `drop`. Returns delivered messages.
    fn shuttle(
        a: &mut ControlLink,
        b: &mut ControlLink,
        now: u64,
        drop: &[usize],
    ) -> Vec<ControlMsg> {
        let mut delivered = Vec::new();
        for (i, dgram) in a.poll(now).into_iter().enumerate() {
            if drop.contains(&i) {
                continue;
            }
            delivered.extend(b.receive(&dgram).unwrap());
        }
        delivered
    }

    #[test]
    fn delivers_in_order() {
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        for n in 0..5 {
            a.send(chat(n)).unwrap();
        }
        let got = shuttle(&mut a, &mut b, 0, &[]);
        assert_eq!(got, (0..5).map(chat).collect::<Vec<_>>());
        // Acks flow back and clear the pending queue.
        shuttle(&mut b, &mut a, 1, &[]);
        assert!(!a.has_pending());
    }

    #[test]
    fn retransmits_after_loss() {
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        for n in 0..3 {
            a.send(chat(n)).unwrap();
        }
        // First transmission: middle datagram lost.
        let got = shuttle(&mut a, &mut b, 0, &[1]);
        assert_eq!(got, vec![chat(0)]);
        // Ack round trip tells `a` what survived.
        shuttle(&mut b, &mut a, 10, &[]);
        // After the RTO, only the lost message goes again.
        let resent = a.poll(200);
        assert_eq!(resent.len(), 1);
        let got: Vec<_> = resent
            .into_iter()
            .flat_map(|d| b.receive(&d).unwrap())
            .collect();
        assert_eq!(got, vec![chat(1), chat(2)]);
    }

    #[test]
    fn duplicates_deliver_once() {
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        a.send(chat(0)).unwrap();
        let dgrams = a.poll(0);
        assert_eq!(b.receive(&dgrams[0]).unwrap(), vec![chat(0)]);
        assert_eq!(b.receive(&dgrams[0]).unwrap(), vec![]);
    }

    #[test]
    fn bare_ack_when_nothing_to_say() {
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        a.send(chat(0)).unwrap();
        shuttle(&mut a, &mut b, 0, &[]);
        // `b` owes an ack and has nothing queued: exactly one bare ack.
        let acks = b.poll(1);
        assert_eq!(acks.len(), 1);
        a.receive(&acks[0]).unwrap();
        assert!(!a.has_pending());
        // Nothing further owed.
        assert!(b.poll(2).is_empty());
    }

    #[test]
    fn link_dies_after_giving_up() {
        let mut a = ControlLink::new();
        a.send(chat(0)).unwrap();
        let mut now = 0;
        for _ in 0..MAX_SENDS + 1 {
            a.poll(now);
            now += RTO_MAX_MS + 1;
        }
        assert!(a.is_dead());
    }

    #[test]
    fn rejects_oversized_chat() {
        let mut a = ControlLink::new();
        let big = ControlMsg::Chat {
            from: MemberId(1),
            text: "x".repeat(MAX_CHAT_LEN + 1),
        };
        assert!(a.send(big).is_err());
    }

    #[test]
    fn survives_heavy_loss_and_reorder() {
        // Deterministic pseudo-random loss pattern; no rng dependency.
        let mut a = ControlLink::new();
        let mut b = ControlLink::new();
        let total = 40u64;
        for n in 0..total {
            a.send(chat(n)).unwrap();
        }
        let mut received = Vec::new();
        let mut state = 0x2545F4914F6CDD1Du64;
        let mut now = 0u64;
        while received.len() < total as usize && now < 60_000 {
            for dgram in a.poll(now) {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                // Drop roughly a third of everything.
                if state >> 60 < 5 {
                    continue;
                }
                received.extend(b.receive(&dgram).unwrap());
            }
            for dgram in b.poll(now) {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                if state >> 60 < 5 {
                    continue;
                }
                a.receive(&dgram).unwrap();
            }
            now += 50;
        }
        assert_eq!(received, (0..total).map(chat).collect::<Vec<_>>());
        assert!(!a.is_dead());
    }
}
