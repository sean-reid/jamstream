//! Control-plane reassembly ingests decrypted but otherwise untrusted
//! datagrams; `ControlLink::receive` must never panic, the link state must
//! stay coherent for any input, and the reassembly buffer must stay inside
//! the receive window however the sequence numbers are chosen.

#![no_main]

use jamstream_protocol::control::{ControlLink, RECV_WINDOW};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut link = ControlLink::new();
    // Split the input into as many datagram attempts as it will make, so one
    // case can drive the link through a sequence of frames rather than one.
    for chunk in data.split(|&b| b == 0xFF) {
        let _ = link.receive(chunk);
        assert!(
            link.buffered() <= RECV_WINDOW as usize,
            "reassembly buffer grew to {}",
            link.buffered()
        );
    }
    // Whatever came in, the link must still be pollable without panicking.
    let _ = link.poll(0);
});
