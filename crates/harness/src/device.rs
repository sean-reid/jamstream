//! The device side of playout, as the client's worker thread runs it: a real
//! [`CallbackBridge`] ring, prefilled to a depth target, held there by a
//! top-up loop, and drained by a device callback at a time.
//!
//! Every sample of that depth sits ahead of the next one the engine hands
//! over, so it is latency, and a measurement taken at the engine boundary is
//! short by it. What lies beyond the callback, whatever the sound card holds
//! after our own buffer, is not knowable from this side and is not modelled
//! here; the ring depth and the callback size are.

use jamstream_audio_io::{CallbackBridge, DeviceSide, EngineSide};

use crate::scenario::{FRAME_SAMPLES, STEREO_FRAME};

/// Interleaved stereo, matching the client's playout path.
const CHANNELS: usize = 2;

/// One client's playout ring and the device callback that drains it, paced by
/// the harness's master tick.
pub struct PlayoutDevice {
    device: DeviceSide,
    engine: EngineSide,
    /// Interleaved stereo samples the top-up loop holds in the ring: two
    /// device callbacks, floored at one 2.5 ms frame, which is the depth the
    /// client's worker fills to.
    target: usize,
    /// Interleaved stereo samples one callback asks for.
    callback: usize,
    /// Decoded audio the ring had no room for, held so none is dropped.
    carry: [f32; STEREO_FRAME],
    carry_pos: usize,
    carry_len: usize,
    /// Interleaved stereo samples the device clock has earned and not yet
    /// asked for. Fractional so a skewed device stays on its own rate.
    earned: f64,
    /// This tick's callback output, reused across ticks.
    out: Vec<f32>,
}

impl PlayoutDevice {
    /// A device that calls back for `device_frames` frames at a time.
    ///
    /// The ring is cut deeper than the depth the top-up loop holds, because
    /// capacity costs memory and depth costs latency; the prefill is silence
    /// rather than audio pulled from the core, because a burst pull would run
    /// the jitter buffer's consumer clock past the sender and every later
    /// packet would arrive late.
    pub fn new(device_frames: u32) -> PlayoutDevice {
        let callback = device_frames as usize * CHANNELS;
        let target = 2 * (device_frames as usize).max(FRAME_SAMPLES) * CHANNELS;
        let (device, mut engine) = CallbackBridge::new(target, 2 * target);
        engine.push_playout(&vec![0.0; target]);
        PlayoutDevice {
            device,
            engine,
            target,
            callback,
            carry: [0.0; STEREO_FRAME],
            carry_pos: 0,
            carry_len: 0,
            earned: 0.0,
            out: Vec::new(),
        }
    }

    /// Advances the device by `frames` of its own clock and returns what it
    /// played: one top-up from `pull`, then every whole callback the device has
    /// earned since the last tick.
    ///
    /// One top-up per tick and not one per callback, because that is the
    /// client's own cadence: its worker tops the ring up once every 2.5 ms and
    /// the device drains it on its own clock in between. Topping up between
    /// callbacks instead would pull the engine a frame further ahead of the
    /// network than the worker ever does, and the jitter buffer would pay for
    /// it. A callback longer than a master tick therefore earns one every few
    /// ticks and plays nothing in between, which is what that device does.
    pub fn run(&mut self, frames: f64, mut pull: impl FnMut(&mut [f32])) -> &[f32] {
        self.out.clear();
        self.earned += frames * CHANNELS as f64;
        self.top_up(&mut pull);
        while self.earned >= self.callback as f64 {
            self.earned -= self.callback as f64;
            let filled = self.out.len();
            self.out.resize(filled + self.callback, 0.0);
            self.device.on_playback(&mut self.out[filled..]);
        }
        &self.out
    }

    /// Callbacks that found the ring short and padded with silence. Silence
    /// the device invents is latency of its own, so a latency figure is only
    /// the cushion's while this stays at zero.
    pub fn underruns(&self) -> u64 {
        self.engine.underruns()
    }

    /// Holds the ring at its depth target, pulling one 2.5 ms frame at a time
    /// from the core and carrying whatever the ring refuses.
    fn top_up(&mut self, pull: &mut impl FnMut(&mut [f32])) {
        loop {
            if self.carry_pos < self.carry_len {
                self.carry_pos += self.fill_to_target();
                if self.carry_pos < self.carry_len {
                    return;
                }
            }
            pull(&mut self.carry);
            self.carry_pos = 0;
            self.carry_len = STEREO_FRAME;
        }
    }

    /// Pushes the carry toward the depth target and no further; returns how
    /// much of it fit.
    fn fill_to_target(&mut self) -> usize {
        let room = self.target.saturating_sub(self.engine.playout_depth());
        if room == 0 {
            return 0;
        }
        let carry = &self.carry[self.carry_pos..self.carry_len];
        self.engine.push_playout(&carry[..room.min(carry.len())])
    }
}
