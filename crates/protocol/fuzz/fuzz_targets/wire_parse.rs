//! Outer datagram framing is reachable by any unauthenticated UDP packet;
//! `wire::parse` must never panic, whatever the bytes. A connecting client
//! also opens every cookie challenge that parses, so the AEAD open runs on
//! the same attacker-chosen bytes: it must never panic either, and bytes
//! sealed without the reply key must never open.

#![no_main]

use jamstream_protocol::transport::cookie_reply_key;
use jamstream_protocol::wire;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(wire::Packet::CookieChallenge { nonce, sealed }) = wire::parse(data) {
        let key = cookie_reply_key(&[0u8; 32]);
        assert!(
            wire::open_cookie_challenge(&key, &nonce, &sealed, b"the-init-this-end-sent")
                .is_none(),
            "fuzzer bytes forged a sealed cookie"
        );
    }
});
