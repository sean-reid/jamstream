//! Media frames are hand-packed hot-path decoding of attacker-influenced
//! plaintext; `MediaFrame::decode` must never panic or over-read.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = jamstream_protocol::media::MediaFrame::decode(data);
});
