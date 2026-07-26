//! Invites arrive as pasted text from anywhere; decoding arbitrary strings
//! must fail cleanly, never panic.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let _ = jamstream_protocol::invite::Invite::decode(text);
});
