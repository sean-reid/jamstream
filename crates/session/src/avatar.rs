//! Avatar plumbing shared by both cores: a bounded content-addressed byte
//! cache, chunk-train reassembly for the receiving side of the control
//! link, and the chunk cursor for the sending side.
//!
//! Pacing: the control link is ordered, so avatar chunks queued ahead of a
//! chat or ping delay it. Rather than a priority lane, both cores feed at
//! most `AVATAR_CHUNKS_PER_POLL` chunks into the link per poll cycle. The
//! link flushes its whole queue every poll, so on a healthy path a control
//! message is sequenced behind at most one cycle's allotment (2 x 8 KB) and
//! rides the same flush, while a full 256 KB avatar still moves in
//! 32 / 2 = 16 ticks (40 ms per hop at the 2.5 ms cadence, a 6.4 MB/s
//! per-link ceiling). Simple, honest, and starvation-free by construction.

use std::collections::{BTreeMap, BTreeSet};

use blake2::{Blake2s256, Digest};
use jamstream_protocol::control::{AVATAR_CHUNK_BYTES, ControlMsg, MAX_AVATAR_BYTES};

pub type AvatarHash = [u8; 32];

/// Chunks fed into a link per poll cycle; see the module comment.
pub(crate) const AVATAR_CHUNKS_PER_POLL: usize = 2;

pub fn avatar_hash(bytes: &[u8]) -> AvatarHash {
    Blake2s256::digest(bytes).into()
}

pub fn chunk_total(len: usize) -> u16 {
    len.div_ceil(AVATAR_CHUNK_BYTES) as u16
}

/// Content-addressed avatar store with a byte budget. Eviction removes the
/// least-recently-referenced entry whose hash is not pinned (callers pin
/// the hashes the current roster references), so a returning bandmate's
/// avatar is still here and transfers zero bytes. Pinned entries never
/// leave; the budget can overshoot by at most the pinned set, which the
/// roster size bounds to a few dozen 256 KB entries.
pub struct AvatarCache {
    max_bytes: usize,
    total_bytes: usize,
    clock: u64,
    entries: BTreeMap<AvatarHash, Entry>,
}

struct Entry {
    bytes: Vec<u8>,
    last_ref: u64,
}

impl AvatarCache {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            total_bytes: 0,
            clock: 0,
            entries: BTreeMap::new(),
        }
    }

    pub fn contains(&self, hash: &AvatarHash) -> bool {
        self.entries.contains_key(hash)
    }

    pub fn get(&self, hash: &AvatarHash) -> Option<&[u8]> {
        self.entries.get(hash).map(|e| e.bytes.as_slice())
    }

    /// Marks a hash as referenced now, for eviction ordering.
    pub fn touch(&mut self, hash: &AvatarHash) {
        self.clock += 1;
        if let Some(e) = self.entries.get_mut(hash) {
            e.last_ref = self.clock;
        }
    }

    /// Inserts hash-verified bytes, then evicts unpinned entries (never the
    /// one just inserted) oldest-reference-first until back under budget.
    pub fn insert(&mut self, hash: AvatarHash, bytes: Vec<u8>, pinned: &BTreeSet<AvatarHash>) {
        if self.contains(&hash) {
            self.touch(&hash);
            return;
        }
        self.clock += 1;
        self.total_bytes += bytes.len();
        self.entries.insert(
            hash,
            Entry {
                bytes,
                last_ref: self.clock,
            },
        );
        while self.total_bytes > self.max_bytes {
            let victim = self
                .entries
                .iter()
                .filter(|(h, _)| **h != hash && !pinned.contains(*h))
                .min_by_key(|(_, e)| e.last_ref)
                .map(|(h, _)| *h);
            let Some(v) = victim else { break };
            if let Some(e) = self.entries.remove(&v) {
                self.total_bytes -= e.bytes.len();
            }
        }
    }
}

/// One inbound chunk train. The control link is ordered and reliable, so a
/// well-behaved train is exactly index 0..total, each chunk full-size except
/// the last; anything else is an error the caller surfaces per its style
/// (server: protocol violation, client: silent drop).
pub struct AvatarRx {
    hash: AvatarHash,
    /// Declared length when known (server side, from SetAvatar); the client
    /// learns the size from the train itself.
    expected_len: Option<u32>,
    total: Option<u16>,
    next: u16,
    buf: Vec<u8>,
}

pub enum RxStep {
    More,
    /// Reassembled and verified against the content hash.
    Done(Vec<u8>),
}

impl AvatarRx {
    pub fn new(hash: AvatarHash, expected_len: Option<u32>) -> Self {
        Self {
            hash,
            expected_len,
            total: None,
            next: 0,
            buf: Vec::new(),
        }
    }

    pub fn hash(&self) -> &AvatarHash {
        &self.hash
    }

    /// Bytes buffered so far; the fuzz harness asserts this stays capped.
    #[cfg(feature = "fuzzing")]
    pub fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    pub fn push(&mut self, index: u16, total: u16, data: &[u8]) -> Result<RxStep, &'static str> {
        let max_total = chunk_total(MAX_AVATAR_BYTES);
        if total == 0 || total > max_total {
            return Err("avatar chunk total out of range");
        }
        if let Some(len) = self.expected_len
            && total != chunk_total(len as usize)
        {
            return Err("avatar chunk total mismatch");
        }
        if let Some(t) = self.total
            && t != total
        {
            return Err("avatar chunk total changed mid-train");
        }
        self.total = Some(total);
        if index != self.next {
            return Err("avatar chunk out of order");
        }
        let last = index + 1 == total;
        let size_ok = if last {
            (1..=AVATAR_CHUNK_BYTES).contains(&data.len())
        } else {
            data.len() == AVATAR_CHUNK_BYTES
        };
        if !size_ok {
            return Err("avatar chunk size invalid");
        }
        self.buf.extend_from_slice(data);
        self.next += 1;
        if !last {
            return Ok(RxStep::More);
        }
        if let Some(len) = self.expected_len
            && self.buf.len() != len as usize
        {
            return Err("avatar length mismatch");
        }
        if avatar_hash(&self.buf) != self.hash {
            return Err("avatar hash mismatch");
        }
        Ok(RxStep::Done(std::mem::take(&mut self.buf)))
    }
}

/// One outbound chunk train: a cursor over cached bytes. The pacer pulls a
/// few chunks per poll; reliability is the link's job.
pub(crate) struct AvatarTx {
    hash: AvatarHash,
    next: u16,
}

impl AvatarTx {
    pub fn new(hash: AvatarHash) -> Self {
        Self { hash, next: 0 }
    }

    pub fn hash(&self) -> &AvatarHash {
        &self.hash
    }

    /// Next chunk of `bytes` (the preimage of `hash`); None once the train
    /// is fully queued.
    pub fn next_chunk(&mut self, bytes: &[u8]) -> Option<ControlMsg> {
        let total = chunk_total(bytes.len());
        if self.next >= total {
            return None;
        }
        let start = usize::from(self.next) * AVATAR_CHUNK_BYTES;
        let end = (start + AVATAR_CHUNK_BYTES).min(bytes.len());
        let msg = ControlMsg::AvatarChunk {
            hash: self.hash,
            index: self.next,
            total,
            data: bytes[start..end].to_vec(),
        };
        self.next += 1;
        Some(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> AvatarHash {
        [n; 32]
    }

    #[test]
    fn cache_evicts_lru_unpinned_only() {
        let mut cache = AvatarCache::new(10);
        let pins: BTreeSet<AvatarHash> = [h(1)].into();
        cache.insert(h(1), vec![0; 4], &pins);
        cache.insert(h(2), vec![0; 4], &pins);
        cache.touch(&h(2));
        cache.insert(h(3), vec![0; 4], &pins);
        // Over budget by 2: h(1) is pinned, h(2) was touched after h(1)'s
        // insert but is the oldest unpinned reference. h(2) goes.
        assert!(cache.contains(&h(1)));
        assert!(!cache.contains(&h(2)));
        assert!(cache.contains(&h(3)));
        // Everything pinned: overshoot is tolerated rather than evicting.
        let pins: BTreeSet<AvatarHash> = [h(1), h(3), h(4)].into();
        cache.insert(h(4), vec![0; 8], &pins);
        assert!(cache.contains(&h(1)) && cache.contains(&h(3)) && cache.contains(&h(4)));
        // Reinserting an existing hash is a touch, not a duplicate.
        cache.insert(h(1), vec![0; 4], &pins);
        cache.insert(h(5), vec![0; 1], &BTreeSet::new());
        assert!(cache.contains(&h(1)), "reinsert refreshed h(1)");
    }

    #[test]
    fn rx_reassembles_a_two_chunk_train() {
        let mut bytes = vec![7u8; AVATAR_CHUNK_BYTES + 100];
        bytes[0] = 1;
        let hash = avatar_hash(&bytes);
        let mut rx = AvatarRx::new(hash, Some(bytes.len() as u32));
        assert!(matches!(
            rx.push(0, 2, &bytes[..AVATAR_CHUNK_BYTES]),
            Ok(RxStep::More)
        ));
        match rx.push(1, 2, &bytes[AVATAR_CHUNK_BYTES..]) {
            Ok(RxStep::Done(got)) => assert_eq!(got, bytes),
            _ => panic!("expected completion"),
        }
    }

    #[test]
    fn rx_rejects_malformed_trains() {
        let bytes = vec![3u8; AVATAR_CHUNK_BYTES * 2];
        let hash = avatar_hash(&bytes);
        // Total inconsistent with the declared length.
        let mut rx = AvatarRx::new(hash, Some(bytes.len() as u32));
        assert!(rx.push(0, 5, &bytes[..AVATAR_CHUNK_BYTES]).is_err());
        // Out of order.
        let mut rx = AvatarRx::new(hash, None);
        assert!(rx.push(1, 2, &bytes[..AVATAR_CHUNK_BYTES]).is_err());
        // Short non-final chunk.
        let mut rx = AvatarRx::new(hash, None);
        assert!(rx.push(0, 2, &bytes[..100]).is_err());
        // Total zero or beyond the 256 KB cap.
        let mut rx = AvatarRx::new(hash, None);
        assert!(rx.push(0, 0, &[]).is_err());
        let mut rx = AvatarRx::new(hash, None);
        assert!(
            rx.push(
                0,
                chunk_total(MAX_AVATAR_BYTES) + 1,
                &bytes[..AVATAR_CHUNK_BYTES]
            )
            .is_err()
        );
        // Content that does not hash to the announced identity.
        let mut rx = AvatarRx::new(h(9), None);
        assert!(matches!(
            rx.push(0, 1, &[1, 2, 3]),
            Err("avatar hash mismatch")
        ));
    }

    #[test]
    fn tx_walks_chunks_and_round_trips_through_rx() {
        let bytes: Vec<u8> = (0..MAX_AVATAR_BYTES).map(|i| i as u8).collect();
        let hash = avatar_hash(&bytes);
        let mut tx = AvatarTx::new(hash);
        let mut rx = AvatarRx::new(hash, Some(bytes.len() as u32));
        let mut done = None;
        let mut chunks = 0;
        while let Some(ControlMsg::AvatarChunk {
            hash: ch,
            index,
            total,
            data,
        }) = tx.next_chunk(&bytes)
        {
            assert_eq!(ch, hash);
            chunks += 1;
            if let RxStep::Done(got) = rx.push(index, total, &data).unwrap() {
                done = Some(got);
            }
        }
        assert_eq!(chunks, usize::from(chunk_total(bytes.len())));
        assert_eq!(done.expect("train completed"), bytes);
        assert!(tx.next_chunk(&bytes).is_none());
    }
}
