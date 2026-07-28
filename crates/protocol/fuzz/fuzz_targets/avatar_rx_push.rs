//! `AvatarRx::push` driven directly, without the handshake or the core in
//! the way. The sibling `avatar_reassembly` target covers the production
//! entry point, `ServerCore::handle_datagram`, and that is exactly why it
//! is slow: every op is a sealed datagram through the whole core (~630
//! exec/s under the sanitizer's shadow). This target strips the path to the
//! state machine itself via `jamstream-session`'s `fuzzing` feature, so a
//! night buys orders of magnitude more interleavings, and it can assert
//! internal invariants the end-to-end target cannot see: the reassembly
//! buffer's cap and the cache's measured occupancy.
//!
//! Input layout. A cache budget, then an op program over four transfer
//! slots feeding one shared `AvatarCache`, so cross-transfer confusion and
//! eviction pressure are one mutation away:
//!
//! ```text
//!   budget:u16   cache budget = budget * 16 bytes, so eviction is
//!                reachable whatever size the completed avatars are
//!   then repeatedly, op = byte % 6, slots selected by sel % 4:
//!     0 START    sel:u8, seed:u8, len:u32, mode:u8
//!                define the slot's preimage synth(seed, len % 256 KB + 1)
//!                and open a reassembler for it; mode bits pick the declared
//!                length (none, truthful, or the raw u32), a truthful or
//!                wrong advertised hash, and whether the hash is pinned
//!     1 CHUNK    sel:u8, index:u16, total:u16
//!                the preimage's index-th slice, framing under fuzzer control
//!     2 RAW      sel:u8, index:u16, total:u16, len:u16, data:len
//!                arbitrary bytes under an arbitrary framing
//!     3 CROSS    dst:u8, src:u8, index:u16, total:u16
//!                src's slice fed to dst's reassembler
//!     4 RESTART  sel:u8   a fresh reassembler for the slot, mid-train
//!     5 TOUCH    sel:u8   LRU touch on the slot's hash or an unknown one
//! ```
//!
//! The synthetic preimage is what makes completion reachable: a finished
//! train must hash to the announced identity and no fuzzer finds a Blake2s
//! preimage, so START announces the hash of a pattern the driver can also
//! slice, and CHUNK emits real slices of it under fuzzer-chosen framing.
//!
//! Oracle beyond "does not panic": after every push the buffered bytes stay
//! at or under `MAX_AVATAR_BYTES` whatever the input order; a completed
//! transfer's bytes hash to the advertised identity, respect the cap, and
//! match any declared length; and the shared cache's occupancy, measured
//! from outside through `get`, never exceeds its budget by more than the
//! documented overshoot (pinned entries plus the entry just inserted), with
//! no insert ever evicting a pinned entry. The harness mirrors the contract
//! both cores hold `push` to: any step other than More ends the train.
//!
//! Seeds in `corpus/avatar_rx_push/` are op programs for the trains the
//! unit tests in `session/src/avatar.rs` assert: single-chunk, two-chunk
//! and full 256-chunk transfers that complete and verify, eviction under a
//! tiny budget with a pinned survivor, a cross-transfer completion, a
//! wrong-hash announce, a declared length past the cap, and truncations of
//! the single- and two-chunk programs.

#![no_main]

use std::collections::{BTreeMap, BTreeSet};

use jamstream_protocol::control::{AVATAR_CHUNK_BYTES, MAX_AVATAR_BYTES};
use jamstream_session::{AvatarCache, AvatarHash, AvatarRx, RxStep, avatar_hash};
use libfuzzer_sys::fuzz_target;

#[path = "fixtures.rs"]
mod fixtures;

/// Concurrent transfers; enough for cross-transfer confusion and eviction
/// pressure without diluting coverage across identical state machines.
const SLOTS: usize = 4;
/// Bounds work per execution: a full 256-chunk train plus setup and
/// interference fits, and each push is a 1 KB copy with at most one Blake2s
/// over 256 KB at completion.
const MAX_OPS: usize = 320;
/// Each START may synthesize a 256 KB preimage; eight per execution bounds
/// the allocation churn while leaving every slot a second generation.
const MAX_STARTS: usize = 8;

/// The byte pattern the driver can both announce and slice into chunks.
fn synth(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u8) ^ seed).collect()
}

/// The index-th slice a well-behaved sender would emit; empty past the end.
fn chunk_of(bytes: &[u8], index: u16) -> &[u8] {
    let start = usize::from(index).saturating_mul(AVATAR_CHUNK_BYTES);
    if start >= bytes.len() {
        return &[];
    }
    let end = start.saturating_add(AVATAR_CHUNK_BYTES).min(bytes.len());
    &bytes[start..end]
}

/// One announced transfer: the preimage CHUNK slices, the advertised
/// identity (truthful or deliberately wrong), and the live reassembler.
struct Slot {
    bytes: Vec<u8>,
    hash: AvatarHash,
    expected: Option<u32>,
    rx: Option<AvatarRx>,
}

struct Harness {
    cache: AvatarCache,
    max_bytes: usize,
    pins: BTreeSet<AvatarHash>,
    /// Every hash ever inserted, with its length, so occupancy is measured
    /// from outside the cache instead of trusting its own accounting.
    inserted: BTreeMap<AvatarHash, usize>,
}

impl Harness {
    /// One chunk into a reassembler, with the invariants asserted and the
    /// cores' contract mirrored: any step other than More ends the train.
    fn deliver(
        &mut self,
        rx_slot: &mut Option<AvatarRx>,
        expected: Option<u32>,
        index: u16,
        total: u16,
        data: &[u8],
    ) {
        let Some(rx) = rx_slot.as_mut() else { return };
        let advertised = *rx.hash();
        let step = rx.push(index, total, data);
        assert!(
            rx.buffered_len() <= MAX_AVATAR_BYTES,
            "reassembly buffer grew past the avatar cap"
        );
        match step {
            Ok(RxStep::More) => {}
            Ok(RxStep::Done(bytes)) => {
                *rx_slot = None;
                assert_eq!(
                    avatar_hash(&bytes),
                    advertised,
                    "completed transfer does not hash to its advertised identity"
                );
                assert!(
                    bytes.len() <= MAX_AVATAR_BYTES,
                    "completed transfer exceeds the avatar cap"
                );
                if let Some(len) = expected {
                    assert_eq!(
                        bytes.len(),
                        len as usize,
                        "completed transfer does not match its declared length"
                    );
                }
                self.insert(advertised, bytes);
            }
            Err(_) => *rx_slot = None,
        }
    }

    /// Caches a verified transfer, then checks the budget held: pinned
    /// entries present before the insert survive it, and measured occupancy
    /// stays within budget plus the documented overshoot.
    fn insert(&mut self, hash: AvatarHash, bytes: Vec<u8>) {
        let len = bytes.len();
        self.inserted.insert(hash, len);
        let pinned_before: Vec<AvatarHash> = self
            .pins
            .iter()
            .filter(|h| self.cache.contains(h))
            .copied()
            .collect();
        self.cache.insert(hash, bytes, &self.pins);
        for h in &pinned_before {
            assert!(self.cache.contains(h), "insert evicted a pinned avatar");
        }
        let stored = self.cache.get(&hash).expect("just-inserted entry present");
        assert_eq!(
            avatar_hash(stored),
            hash,
            "cache returned bytes that are not the hash's preimage"
        );
        let present = |h: &AvatarHash| self.cache.contains(h);
        let occupied: usize = self
            .inserted
            .iter()
            .filter(|(h, _)| present(h))
            .map(|(_, l)| *l)
            .sum();
        let pinned: usize = self
            .inserted
            .iter()
            .filter(|(h, _)| self.pins.contains(*h) && present(h))
            .map(|(_, l)| *l)
            .sum();
        assert!(
            occupied <= self.max_bytes + pinned + len,
            "cache occupancy exceeds budget plus its documented overshoot"
        );
    }
}

fuzz_target!(|data: &[u8]| {
    let mut r = fixtures::Reader::new(data);
    let Some(budget) = r.u16() else { return };
    let max_bytes = usize::from(budget) * 16;
    let mut harness = Harness {
        cache: AvatarCache::new(max_bytes),
        max_bytes,
        pins: BTreeSet::new(),
        inserted: BTreeMap::new(),
    };
    let mut slots: [Option<Slot>; SLOTS] = [None, None, None, None];
    let mut starts = 0usize;

    for _ in 0..MAX_OPS {
        let Some(op) = r.u8() else { break };
        match op % 6 {
            // START: announce a preimage and open its reassembler.
            0 => {
                let (Some(sel), Some(seed), Some(len), Some(mode)) =
                    (r.u8(), r.u8(), r.u32(), r.u8())
                else {
                    break;
                };
                if starts == MAX_STARTS {
                    continue;
                }
                starts += 1;
                let bytes = synth(seed, len as usize % (MAX_AVATAR_BYTES + 1));
                let hash = if mode & 0b100 == 0 {
                    avatar_hash(&bytes)
                } else {
                    [seed; 32]
                };
                let expected = match mode & 0b11 {
                    // The client learns the size from the train itself.
                    0 => None,
                    // The server-side shape: a truthful SetAvatar length.
                    1 => Some(bytes.len() as u32),
                    // A declared length the bytes need not honor.
                    _ => Some(len),
                };
                if mode & 0b1000 != 0 {
                    harness.pins.insert(hash);
                }
                slots[usize::from(sel) % SLOTS] = Some(Slot {
                    bytes,
                    hash,
                    expected,
                    rx: Some(AvatarRx::new(hash, expected)),
                });
            }
            // CHUNK: a real slice of the slot's preimage, framed at will.
            1 => {
                let (Some(sel), Some(index), Some(total)) = (r.u8(), r.u16(), r.u16()) else {
                    break;
                };
                let Some(slot) = slots[usize::from(sel) % SLOTS].as_mut() else {
                    continue;
                };
                let data = chunk_of(&slot.bytes, index);
                harness.deliver(&mut slot.rx, slot.expected, index, total, data);
            }
            // RAW: arbitrary payload under an arbitrary framing.
            2 => {
                let (Some(sel), Some(index), Some(total), Some(len)) =
                    (r.u8(), r.u16(), r.u16(), r.u16())
                else {
                    break;
                };
                let data = r.take_up_to(usize::from(len));
                let Some(slot) = slots[usize::from(sel) % SLOTS].as_mut() else {
                    continue;
                };
                harness.deliver(&mut slot.rx, slot.expected, index, total, data);
            }
            // CROSS: one transfer's bytes into another's reassembler.
            3 => {
                let (Some(dst), Some(src), Some(index), Some(total)) =
                    (r.u8(), r.u8(), r.u16(), r.u16())
                else {
                    break;
                };
                let data = slots[usize::from(src) % SLOTS]
                    .as_ref()
                    .map(|s| chunk_of(&s.bytes, index).to_vec())
                    .unwrap_or_default();
                let Some(slot) = slots[usize::from(dst) % SLOTS].as_mut() else {
                    continue;
                };
                harness.deliver(&mut slot.rx, slot.expected, index, total, &data);
            }
            // RESTART: abandon the train and open a fresh reassembler.
            4 => {
                let Some(sel) = r.u8() else { break };
                let Some(slot) = slots[usize::from(sel) % SLOTS].as_mut() else {
                    continue;
                };
                slot.rx = Some(AvatarRx::new(slot.hash, slot.expected));
            }
            // TOUCH: exercise the eviction ordering.
            _ => {
                let Some(sel) = r.u8() else { break };
                let hash = slots[usize::from(sel) % SLOTS]
                    .as_ref()
                    .map(|s| s.hash)
                    .unwrap_or([sel; 32]);
                harness.cache.touch(&hash);
            }
        }
    }
});
