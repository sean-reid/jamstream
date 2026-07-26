//! Control-plane reassembly ingests decrypted but otherwise untrusted
//! datagrams; `ControlLink::receive` must never panic and the link state
//! must stay coherent for any input.

#![no_main]

use jamstream_protocol::control::ControlLink;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut link = ControlLink::new();
    let _ = link.receive(data);
    // Whatever came in, the link must still be pollable without panicking.
    let _ = link.poll(0);
});
