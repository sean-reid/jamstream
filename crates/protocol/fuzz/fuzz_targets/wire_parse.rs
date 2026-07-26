//! Outer datagram framing is reachable by any unauthenticated UDP packet;
//! `wire::parse` must never panic, whatever the bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = jamstream_protocol::wire::parse(data);
});
