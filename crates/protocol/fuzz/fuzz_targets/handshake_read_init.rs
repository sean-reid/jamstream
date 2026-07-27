//! `Responder::read_init` is the first code a session server runs on behalf
//! of a stranger: any unauthenticated UDP packet whose framing says
//! "handshake init" lands here, before any token is checked and before a
//! single byte is spent on a reply. It parses a Noise IK message with the
//! real static key and then postcard-decodes whatever plaintext came out.
//! It must never panic, never over-read, and never allocate on the
//! attacker's say-so.
//!
//! Seeds (`corpus/handshake_read_init/`) are the noise messages of genuine
//! `Initiator::new` first flights, minted against `fixtures::SERVER_PRIVATE`
//! and `fixtures::SESSION_ID` so they decrypt and their payloads decode.
//! They give the fuzzer a starting point that reaches all the way through
//! the AEAD to the postcard decode; mutations from there probe both.

#![no_main]

use jamstream_protocol::PROTOCOL_VERSION;
use jamstream_protocol::transport::Responder;
use libfuzzer_sys::fuzz_target;

#[path = "fixtures.rs"]
mod fixtures;

fuzz_target!(|noise: &[u8]| {
    // Version is pinned to ours: the mismatch branch is a two-line early
    // return already covered by the wire_parse target and the server's own
    // tests, and spending mutation budget on it would only starve the
    // cryptographic path this target exists for.
    let _ = Responder::read_init(
        &fixtures::SERVER_PRIVATE,
        &fixtures::SESSION_ID,
        PROTOCOL_VERSION,
        noise,
    );
});
