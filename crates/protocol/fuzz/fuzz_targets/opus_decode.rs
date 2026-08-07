//! `Decoder::decode` is the only place in the product where attacker-chosen
//! bytes reach C. Every uplink frame in the mix tick goes through it on a
//! publicly reachable VM (`session/src/server.rs`, the per-member decode at
//! the top of `tick`), into vendored libopus, so it is the highest-value
//! unfuzzed surface in the repo.
//!
//! Current posture, verified rather than assumed and recorded here so it
//! cannot regress quietly:
//!
//! - Vendored libopus is 1.6.1, current upstream, via opusic-c 1.6.1 /
//!   opusic-sys 0.7.4.
//! - opusic-sys's local `opus.patch` rewrites CMake MSVC-runtime generator
//!   expressions and touches no codec source.
//! - Resolved features are `["bundled"]` only, so its build.rs passes
//!   `OPUS_HARDENING=ON`, `OPUS_STACK_PROTECTOR=ON` and
//!   `OPUS_FORTIFY_SOURCE=ON`. The `no-hardening`, `no-stack-protector` and
//!   `no-fortify-source` features must stay off; the root Cargo.toml says so
//!   beside the dependency.
//! - The FFI frame_size contract is correct: opusic-c passes
//!   `output.len() / channels` as frame_size, so a packet declaring 960
//!   samples into a 120-sample buffer returns OPUS_BUFFER_TOO_SMALL instead
//!   of overflowing, and `codec.rs` checks the buffer length before the call
//!   and the decoded sample count after.
//!
//! Input layout. A selector byte picks the decoder configuration, then the
//! rest is a sequence of length-prefixed packets fed to one decoder:
//!
//! ```text
//!   selector:u8   bits 0-1 configuration, bit 2 request in-band FEC
//!   then repeatedly:
//!     len:u8      0 means a lost frame, so concealment runs
//!                 255 means "all remaining bytes", for packets over 254
//!     payload:len bytes (truncated to whatever is left)
//! ```
//!
//! A sequence rather than one packet per execution because the decoder is
//! stateful: concealment, the FEC path and the mode-switch logic all depend
//! on what came before, and a fresh decoder per input can never reach them.
//! The decoder is still built inside the target, so a crashing artifact
//! reproduces on its own.
//!
//! Oracle beyond "does not panic": on a successful decode the output must be
//! finite. The mixer sums decoder output without sanitising it (the limiter
//! sanitises, but only on the broadcast path, and only after the personal
//! mixes are already built), so one NaN out of libopus poisons every mix in
//! that tick. The server zeroes the buffer when decode returns an error, so
//! only the Ok case is asserted.
//!
//! Seeds in `corpus/opus_decode/` are programs in the layout above whose
//! packets are real Opus frames, produced by this workspace's own `Encoder`
//! at the shipped settings: mono 2.5 ms at 128 kbps (the server's uplink
//! decode), stereo 2.5 ms at 192 kbps, stereo 20 ms at 128 kbps, mono 20 ms
//! at 128 kbps, over a couple of hundred milliseconds of harmonics plus
//! noise, with variants that drop frames to reach concealment and set the FEC
//! bit. Random bytes almost never form a decodable packet, so without these
//! the fuzzer spends its night in the reject path.

#![no_main]

use jamstream_engine::{Channels, Decoder};
use jamstream_protocol::media::FrameDuration;
use libfuzzer_sys::fuzz_target;

#[path = "fixtures.rs"]
mod fixtures;

/// Bounds work per execution so exec/s stays useful. Sixty-four 2.5 ms
/// frames is 160 ms of audio, well past any state the decoder carries.
const MAX_FRAMES: usize = 64;

fuzz_target!(|data: &[u8]| {
    let mut r = fixtures::Reader::new(data);
    let Some(selector) = r.u8() else { return };

    // The four configurations the product actually constructs. Mono 2.5 ms is
    // the server's, and the only one an unauthenticated stranger could ever
    // aim at; the others are what a client runs on a malicious server's
    // downlink, which the threat model puts in scope for crashes.
    let (channels, duration) = match selector & 0b11 {
        0 => (Channels::Mono, FrameDuration::Ms2_5),
        1 => (Channels::Stereo, FrameDuration::Ms2_5),
        2 => (Channels::Stereo, FrameDuration::Ms20),
        _ => (Channels::Mono, FrameDuration::Ms20),
    };
    let fec = selector & 0b100 != 0;

    let Ok(mut decoder) = Decoder::new(channels, duration) else {
        return;
    };
    let mut pcm = vec![0.0f32; duration.samples() as usize * channels.count()];

    for _ in 0..MAX_FRAMES {
        let Some(len) = r.u8() else { break };
        let payload = match len {
            0 => None,
            255 => Some(r.take_up_to(usize::MAX)),
            n => Some(r.take_up_to(usize::from(n))),
        };
        if decoder.decode(payload, &mut pcm, fec).is_ok() {
            assert!(
                pcm.iter().all(|s| s.is_finite()),
                "decoder emitted a non-finite sample into the mix"
            );
        }
        if r.remaining() == 0 {
            break;
        }
    }
});
