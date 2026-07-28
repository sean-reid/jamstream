//! Avatar reassembly: a member announces a content hash and a length, then
//! streams up to 32 chunks of 8 KB, and the server rebuilds and hash-verifies
//! the bytes before caching them and fanning them out to everyone who asked.
//! It is the only place in the session core where an authenticated peer makes
//! the server accumulate megabytes across many datagrams, so the index/total
//! arithmetic, the size checks, and the cache accounting all matter.
//!
//! The sibling `avatar_rx_push` target calls `AvatarRx::push` directly. This
//! one drives the reassembler through the entry point production actually
//! uses: `ServerCore::handle_datagram`, fed sealed control frames from a real
//! member over a real handshake. Slower per op than calling `push` directly,
//! but it fuzzes the dispatch, the per-member `AvatarRx` lifecycle, the
//! content cache with its pin set, and the waiter fanout as well as the
//! reassembler.
//!
//! Op program (little-endian, short reads end the program):
//!
//! ```text
//!   op = byte % 5
//!   0  ANNOUNCE  seed:u8, len:u32       SetAvatar for the synthetic avatar
//!                                       synth(seed, len); becomes "current"
//!   1  CHUNK     index:u16, total:u16   AvatarChunk carrying the current
//!                                       avatar's index-th slice: the shape a
//!                                       well-behaved sender produces
//!   2  RAWCHUNK  sel:u8, index:u16,     AvatarChunk with arbitrary bytes and
//!                total:u16, len:u16,    optionally a hash nobody announced
//!                data:len
//!   3  REQUEST   sel:u8                 AvatarRequest for the current hash
//!                                       or an unknown one
//!   4  TICK      -                      advance the clock one mix tick and
//!                                       run it: paces outbound trains
//! ```
//!
//! The synthetic avatar is what makes the deep path reachable. A completed
//! train has to hash to the announced identity, and no fuzzer finds a
//! Blake2s preimage; so ANNOUNCE announces the hash of a byte pattern the
//! driver can also produce, and CHUNK emits real slices of it. Everything
//! past `avatar_hash(&buf) != self.hash` is then one mutation away instead of
//! unreachable.
//!
//! Seeds (`corpus/avatar_reassembly/`) are op programs transcribing the
//! trains `avatar.rs`'s own unit tests assert: a two-chunk upload that
//! completes and verifies, a single-chunk upload followed by a request and a
//! tick so the cache and the outbound pacer run, and the malformed-train
//! cases (out of order, short non-final chunk, total out of range).

#![no_main]

use std::net::SocketAddr;

use blake2::{Blake2s256, Digest};
use jamstream_protocol::control::{AVATAR_CHUNK_BYTES, ControlLink, ControlMsg, MAX_AVATAR_BYTES};
use jamstream_protocol::transport::{Initiator, Session};
use jamstream_protocol::wire;
use jamstream_session::{ServerConfig, ServerCore};
use libfuzzer_sys::fuzz_target;

#[path = "fixtures.rs"]
mod fixtures;

/// Bounds work per execution. Each op is a sealed datagram through the full
/// server core, so the ceiling is lower than for the pure-protocol targets.
const MAX_OPS: usize = 64;
/// Wall clock is irrelevant to this path but has to be past nothing and
/// before the token expiry.
const NOW_UNIX: u64 = 1_000;
/// One mix tick, so TICK ops advance time the way jamstreamd does.
const TICK_MS: u64 = 3;

fn addr() -> SocketAddr {
    "198.51.100.7:41100".parse().expect("literal address")
}

fn avatar_hash(bytes: &[u8]) -> [u8; 32] {
    Blake2s256::digest(bytes).into()
}

/// The byte pattern the driver can both announce and chunk.
fn synth(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u8) ^ seed).collect()
}

/// Runs the real handshake through the core so the fuzzed member is
/// genuinely admitted, and returns its client-side transport.
fn admit(core: &mut ServerCore) -> Option<Session> {
    let (initiator, init_packet) = Initiator::new(&fixtures::invite()).ok()?;
    let out = core.handle_datagram(0, NOW_UNIX, addr(), &init_packet);
    let _ = core.events();
    let mut noise = None;
    for (_, packet) in &out {
        if let Ok(wire::Packet::HandshakeResp { noise: n }) = wire::parse(packet) {
            noise = Some(n.to_vec());
            break;
        }
    }
    let (session, _welcome) = initiator.finish(&noise?).ok()?;
    Some(session)
}

/// Decrypts what the server sent back and feeds control datagrams to the
/// client link, which keeps the link's pending queue from growing without
/// bound and exercises the reply direction (rosters, avatar requests, and
/// the server's own outbound chunk trains).
fn absorb(out: Vec<(SocketAddr, Vec<u8>)>, client: &mut Session, link: &mut ControlLink) {
    for (_, packet) in out {
        let Ok(wire::Packet::Transport {
            counter,
            ciphertext,
            ..
        }) = wire::parse(&packet)
        else {
            continue;
        };
        let Ok(plain) = client.open(counter, ciphertext) else {
            continue;
        };
        // The channel byte is part of what ControlLink::receive inspects, so
        // the plaintext goes in whole; media downlinks are refused there.
        let _ = link.receive(&plain);
    }
}

/// Seals and delivers everything the client link has queued.
fn pump(now_ms: u64, core: &mut ServerCore, client: &mut Session, link: &mut ControlLink) {
    for datagram in link.poll(now_ms) {
        let Ok(packet) = client.seal(fixtures::MEMBER, &datagram) else {
            continue;
        };
        let out = core.handle_datagram(now_ms, NOW_UNIX, addr(), &packet);
        absorb(out, client, link);
    }
    // Events accumulate in a Vec until drained; a violation per op would
    // otherwise be a slow leak rather than a finding.
    let _ = core.events();
}

fuzz_target!(|data: &[u8]| {
    let issuer = fixtures::issuer();
    let mut core = ServerCore::new(ServerConfig::new(
        fixtures::SESSION_ID,
        fixtures::SERVER_PRIVATE.to_vec(),
        fixtures::server_public(),
        issuer.public_key(),
    ));
    let Some(mut client) = admit(&mut core) else {
        return;
    };
    let mut link = ControlLink::new();
    let mut r = fixtures::Reader::new(data);
    let mut now_ms = 0u64;
    // The avatar the driver most recently announced: its hash always, its
    // bytes only when the declared length was small enough to be real.
    let mut current: Option<Vec<u8>> = None;
    let mut current_hash = [0u8; 32];

    for _ in 0..MAX_OPS {
        let Some(op) = r.u8() else { break };
        match op % 5 {
            // ANNOUNCE: creates (or replaces) the server-side AvatarRx and
            // makes the server ask for the bytes.
            0 => {
                let (Some(seed), Some(len)) = (r.u8(), r.u32()) else {
                    break;
                };
                if len as usize <= MAX_AVATAR_BYTES {
                    let bytes = synth(seed, len as usize);
                    current_hash = avatar_hash(&bytes);
                    current = Some(bytes);
                } else {
                    // Out-of-range length: the server rejects it before it
                    // ever looks at the hash, so do not spend a 4 MB
                    // allocation proving that.
                    current_hash = [seed; 32];
                    current = None;
                }
                let _ = link.send(ControlMsg::SetAvatar {
                    hash: current_hash,
                    len,
                });
            }
            // CHUNK: the shape a well-behaved AvatarTx emits, with the index
            // and total under fuzzer control.
            1 => {
                let (Some(index), Some(total)) = (r.u16(), r.u16()) else {
                    break;
                };
                let data = match &current {
                    Some(bytes) => {
                        let start = usize::from(index).saturating_mul(AVATAR_CHUNK_BYTES);
                        if start >= bytes.len() {
                            Vec::new()
                        } else {
                            let end = start.saturating_add(AVATAR_CHUNK_BYTES).min(bytes.len());
                            bytes[start..end].to_vec()
                        }
                    }
                    None => Vec::new(),
                };
                let _ = link.send(ControlMsg::AvatarChunk {
                    hash: current_hash,
                    index,
                    total,
                    data,
                });
            }
            // RAWCHUNK: arbitrary payload, arbitrary framing, and half the
            // time a hash the server never solicited.
            2 => {
                let (Some(sel), Some(index), Some(total), Some(len)) =
                    (r.u8(), r.u16(), r.u16(), r.u16())
                else {
                    break;
                };
                let hash = if sel & 1 == 1 {
                    current_hash
                } else {
                    [sel; 32]
                };
                let data = r.take_up_to(usize::from(len)).to_vec();
                let _ = link.send(ControlMsg::AvatarChunk {
                    hash,
                    index,
                    total,
                    data,
                });
            }
            // REQUEST: the waiter bookkeeping and the cache lookup.
            3 => {
                let Some(sel) = r.u8() else { break };
                let hash = if sel & 1 == 1 {
                    current_hash
                } else {
                    [sel; 32]
                };
                let _ = link.send(ControlMsg::AvatarRequest { hash });
            }
            // TICK: paces outbound trains and ages the member.
            _ => {
                now_ms += TICK_MS;
                let out = core.tick(now_ms);
                absorb(out, &mut client, &mut link);
            }
        }
        pump(now_ms, &mut core, &mut client, &mut link);
    }
});
