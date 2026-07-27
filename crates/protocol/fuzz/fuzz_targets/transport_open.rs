//! `Session::open` is the whole post-handshake attack surface: every
//! datagram a peer receives after admission goes through it, with a counter
//! and a ciphertext length both chosen by whoever sent the packet. Two
//! things live in there worth fuzzing hard: the AEAD boundary (a
//! caller-sized plaintext buffer, and the tag length subtracted from an
//! attacker-controlled ciphertext length) and the sliding replay window,
//! whose bookkeeping is bit shifts by attacker-influenced distances.
//!
//! A real handshake runs in setup, so this is the genuine transport state,
//! not a hand-built one. The input is then read as a small op program, which
//! is what lets the window get exercised at all: garbage never authenticates
//! and so never reaches `ReplayWindow::accept`, and only a sequence of real
//! seals opened in a fuzzer-chosen order, with fuzzer-chosen duplicates and
//! fuzzer-chosen counter gaps, gets there.
//!
//! Op program (little-endian, short reads end the program):
//!
//! ```text
//!   op = byte % 5
//!   0  SEAL      len:u8, plaintext:len bytes    seal and remember the packet
//!   1  OPEN      slot:u8                        open remembered packet slot%n
//!   2  TAMPER    slot:u8, pos:u8, mask:u8       open it with one byte flipped
//!   3  RAW       counter:u64, len:u8, ct:len    open arbitrary bytes
//!   4  BURN      n:u8                           seal and discard n packets,
//!                                               opening counter gaps
//! ```
//!
//! Seeds (`corpus/transport_open/`) are op programs transcribing the
//! scenarios `transport.rs`'s own unit tests assert: a seal/open round trip,
//! a replay, an out-of-order burst, a tampered packet followed by the intact
//! one, and a window-overrun. The ciphertexts themselves cannot be seeds:
//! they are bound to the session keys of the handshake that produced them,
//! so they are produced at run time by the crate's own `Session::seal`.
//!
//! Oracle beyond "does not panic": the server must never accept the same
//! counter twice. That is the entire promise of the replay window, and it is
//! checkable without knowing anything about the plaintext.

#![no_main]

use std::collections::HashSet;

use jamstream_protocol::wire;
use libfuzzer_sys::fuzz_target;

#[path = "fixtures.rs"]
mod fixtures;

/// Bounds the work per execution so exec/s stays useful; a longer program
/// buys nothing the window state machine does not already show in 128 ops.
const MAX_OPS: usize = 128;
/// Sealed packets kept for replay/reorder ops.
const MAX_STORED: usize = 64;

fuzz_target!(|data: &[u8]| {
    let (mut client, mut server) = fixtures::established();
    let mut r = fixtures::Reader::new(data);
    let mut stored: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut accepted: HashSet<u64> = HashSet::new();

    for _ in 0..MAX_OPS {
        let Some(op) = r.u8() else { break };
        match op % 5 {
            // SEAL: a real packet from the real encoder, remembered so
            // later ops can open, replay, and corrupt it.
            0 => {
                let len = r.u8().unwrap_or(0) as usize;
                let plaintext = r.take_up_to(len);
                let Ok(packet) = client.seal(fixtures::MEMBER, plaintext) else {
                    continue;
                };
                let Ok(wire::Packet::Transport {
                    counter,
                    ciphertext,
                    ..
                }) = wire::parse(&packet)
                else {
                    continue;
                };
                if stored.len() < MAX_STORED {
                    stored.push((counter, ciphertext.to_vec()));
                }
            }
            // OPEN: the accept path, and on a repeat the replay path.
            1 => {
                let Some(slot) = r.u8() else { break };
                if stored.is_empty() {
                    continue;
                }
                let (counter, ciphertext) = &stored[slot as usize % stored.len()];
                if server.open(*counter, ciphertext).is_ok() {
                    assert!(
                        accepted.insert(*counter),
                        "replay window accepted counter {counter} twice"
                    );
                }
            }
            // TAMPER: authentication must reject, and must not burn the
            // counter (a later OPEN of the intact packet proves it).
            2 => {
                let (Some(slot), Some(pos), Some(mask)) = (r.u8(), r.u8(), r.u8()) else {
                    break;
                };
                if stored.is_empty() {
                    continue;
                }
                let (counter, ciphertext) = &stored[slot as usize % stored.len()];
                let mut bad = ciphertext.clone();
                if bad.is_empty() {
                    continue;
                }
                let at = pos as usize % bad.len();
                bad[at] ^= mask;
                if server.open(*counter, &bad).is_ok() {
                    // A zero mask is a no-op flip, so success here is only
                    // a bug if the bytes really changed.
                    assert!(
                        mask == 0,
                        "authentication accepted a modified ciphertext at {at}"
                    );
                    assert!(
                        accepted.insert(*counter),
                        "replay window accepted counter {counter} twice"
                    );
                }
            }
            // RAW: arbitrary counter, arbitrary body. The short-ciphertext
            // and empty-ciphertext cases live here.
            3 => {
                let Some(counter) = r.u64() else { break };
                let len = r.u8().unwrap_or(0) as usize;
                let ciphertext = r.take_up_to(len);
                if server.open(counter, ciphertext).is_ok() {
                    assert!(
                        accepted.insert(counter),
                        "replay window accepted counter {counter} twice"
                    );
                }
            }
            // BURN: advance the send counter without remembering the
            // packets, so the receiver's window can be pushed past its
            // 64-packet reach while older stored packets are still around
            // to open. Both the shift-clears-the-bitmap and the
            // older-than-the-window branches need this.
            _ => {
                let n = r.u8().unwrap_or(0);
                for _ in 0..n {
                    let _ = client.seal(fixtures::MEMBER, &[]);
                }
            }
        }
    }
});
