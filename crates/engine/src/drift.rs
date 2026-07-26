//! Capture/playout pacing against sound-card clock drift. A
//! `DriftCompensator` sits between a device-paced sample stream and the
//! nominal-rate frame path, built on rubato's `Slip` resampler: arbitrary
//! lengths in, fixed-size frames out, with the rate steered a few ppm by a
//! feedback loop so sustained drift never reaches the jitter buffers.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Adjustable, FixedAsync, Resampler, Slip};

/// Steering authority. Real crystals sit within +-100 ppm; +-500 covers the
/// worst of them plus controller overshoot while staying far inside Slip's
/// sustainable correction range (~7700 ppm at any chunk size).
pub const MAX_STEER_PPM: f64 = 500.0;
/// Largest ratio move per pulled frame. At 2.5 ms frames this is 400 ppm/s,
/// fast enough to outrun any controller yet gradual at the sample level.
const SLEW_PPM_PER_FRAME: f64 = 1.0;
/// Input backlog capacity in output chunks (~80 ms at 120/2.5 ms). A steered
/// loop holds a couple of frames; overflow means the consumer stopped, and
/// the oldest audio is dropped to keep latency bounded.
const BUFFER_CHUNKS: usize = 32;

/// Rate-matching resampler for one direction of the device boundary.
///
/// `push` accepts whatever the device delivered; `pull_frame` yields exactly
/// `chunk_out` frames whenever enough input has accumulated. The realized
/// input:output ratio follows `steer` through a slew limiter. Everything is
/// preallocated in `new`; `push` and `pull_frame` never allocate.
pub struct DriftCompensator {
    slip: Slip<f32>,
    channels: usize,
    chunk_out: usize,
    /// Interleaved device-paced samples awaiting resampling. Never grows
    /// past its constructed capacity.
    buf: Vec<f32>,
    target_ratio: f64,
    current_ratio: f64,
}

impl DriftCompensator {
    /// A compensator producing `chunk_out`-frame chunks of `channels`
    /// interleaved audio. `chunk_out` must be at least 4 (Slip's minimum).
    pub fn new(chunk_out: usize, channels: usize) -> Self {
        assert!(channels > 0, "compensator needs at least one channel");
        let slip = Slip::new(chunk_out, channels, FixedAsync::Output)
            .expect("chunk_out too small for Slip");
        // input_frames_next never exceeds chunk_out + max correction, which
        // is under 2 * chunk_out, so this capacity always fits one pull.
        let capacity = BUFFER_CHUNKS * chunk_out * channels;
        Self {
            slip,
            channels,
            chunk_out,
            buf: Vec::with_capacity(capacity),
            target_ratio: 1.0,
            current_ratio: 1.0,
        }
    }

    /// Sets the steering target as a relative ratio in ppm, clamped to
    /// +-`MAX_STEER_PPM`. Positive consumes fewer input samples per output
    /// frame (for a device running slow); negative consumes more. The
    /// applied ratio slews toward the target across subsequent pulls, so
    /// controller steps never jump the rate.
    pub fn steer(&mut self, ratio_ppm_relative: f64) {
        let ppm = if ratio_ppm_relative.is_finite() {
            ratio_ppm_relative.clamp(-MAX_STEER_PPM, MAX_STEER_PPM)
        } else {
            0.0
        };
        self.target_ratio = 1.0 + ppm * 1e-6;
    }

    /// Buffers device-paced input. `samples.len()` must be a multiple of the
    /// channel count. On overflow the oldest samples are dropped.
    pub fn push(&mut self, samples: &[f32]) {
        debug_assert_eq!(samples.len() % self.channels, 0);
        let cap = self.buf.capacity();
        if samples.len() >= cap {
            self.buf.clear();
            self.buf.extend_from_slice(&samples[samples.len() - cap..]);
            return;
        }
        let overflow = (self.buf.len() + samples.len()).saturating_sub(cap);
        if overflow > 0 {
            self.buf.copy_within(overflow.., 0);
            self.buf.truncate(self.buf.len() - overflow);
        }
        self.buf.extend_from_slice(samples);
    }

    /// Fills `out` (exactly `chunk_out * channels` long) with resampled
    /// audio. Returns false without touching `out` when the input backlog
    /// cannot cover a whole chunk yet. Each call advances the slewed ratio
    /// one step.
    pub fn pull_frame(&mut self, out: &mut [f32]) -> bool {
        assert_eq!(out.len(), self.chunk_out * self.channels);
        self.slew();
        let need_frames = self.slip.input_frames_next();
        let need = need_frames * self.channels;
        if self.buf.len() < need {
            return false;
        }
        let input = InterleavedSlice::new(&self.buf[..need], self.channels, need_frames)
            .expect("input adapter");
        let mut output =
            InterleavedSlice::new_mut(out, self.channels, self.chunk_out).expect("output adapter");
        self.slip
            .process_into_buffer(&input, &mut output, None)
            .expect("slip process");
        self.buf.copy_within(need.., 0);
        let len = self.buf.len() - need;
        self.buf.truncate(len);
        true
    }

    /// Input frames (samples per channel, not chunks) currently buffered;
    /// divide by `chunk_out` for the backlog in output-chunk units, the
    /// natural feedback signal for a backlog-driven controller.
    pub fn buffered_frames(&self) -> usize {
        self.buf.len() / self.channels
    }

    /// The ratio currently applied (post-slew), in ppm relative to 1.0.
    pub fn ratio_ppm(&self) -> f64 {
        (self.current_ratio - 1.0) * 1e6
    }

    fn slew(&mut self) {
        let delta = self.target_ratio - self.current_ratio;
        if delta == 0.0 {
            return;
        }
        let step = SLEW_PPM_PER_FRAME * 1e-6;
        self.current_ratio += delta.clamp(-step, step);
        // +-MAX_STEER_PPM is always inside Slip's sustainable range, so the
        // saturation error cannot fire.
        let _ = self.slip.set_resample_ratio(self.current_ratio, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    /// Counts heap operations on this thread so the RT-path test can assert
    /// push/pull are allocation-free after construction. Counting is
    /// thread-local, so parallel tests in this binary do not interfere.
    struct CountingAlloc;

    thread_local! {
        static HEAP_OPS: Cell<u64> = const { Cell::new(0) };
    }

    fn heap_ops() -> u64 {
        HEAP_OPS.with(Cell::get)
    }

    unsafe impl GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            HEAP_OPS.with(|c| c.set(c.get() + 1));
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            HEAP_OPS.with(|c| c.set(c.get() + 1));
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            HEAP_OPS.with(|c| c.set(c.get() + 1));
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static ALLOC: CountingAlloc = CountingAlloc;

    fn sine(idx: u64) -> f32 {
        (440.0 * std::f64::consts::TAU * idx as f64 / 48_000.0).sin() as f32
    }

    #[test]
    fn identity_passthrough_is_bit_exact() {
        let mut c = DriftCompensator::new(120, 1);
        let mut fed = Vec::new();
        let mut produced = Vec::new();
        let mut idx = 0u64;
        for _ in 0..400 {
            let mut pcm = [0.0f32; 120];
            for s in pcm.iter_mut() {
                *s = sine(idx);
                idx += 1;
            }
            c.push(&pcm);
            fed.extend_from_slice(&pcm);
            let mut out = [0.0f32; 120];
            while c.pull_frame(&mut out) {
                produced.extend_from_slice(&out);
            }
        }
        // At ratio 1.0 Slip needs exactly one chunk of input per chunk of
        // output, so nothing beyond one chunk may linger buffered.
        assert!(
            fed.len() - produced.len() < 120,
            "length not conserved: fed {} produced {}",
            fed.len(),
            produced.len()
        );
        assert_eq!(
            produced,
            fed[..produced.len()],
            "unit ratio must pass samples through bit-exactly"
        );
    }

    // A +200 ppm device against nominal one-frame-per-tick consumption,
    // steered by a small PI controller on the compensator's own backlog.
    // Gains against the plant (backlog integrates the rate error at
    // 4e-4 frames/s/ppm): kp 300 puts the crossover at ~0.12 rad/s and
    // ki 10 the PI zero a factor ~3.6 below it, converging in well under a
    // minute without ringing.
    #[test]
    fn plus_200ppm_stays_balanced_when_steered() {
        let mut c = DriftCompensator::new(120, 1);
        let mut acc = 0.0f64;
        let mut idx = 0u64;
        let mut integral = 0.0f64;
        let mut pcm = [0.0f32; 128];
        let mut out = [0.0f32; 120];
        let mut empty_pulls = 0u32;
        let mut max_backlog = 0usize;
        // 60 s of 2.5 ms ticks.
        for tick in 0..24_000u64 {
            acc += 120.0 * (1.0 + 200e-6);
            let n = acc as usize;
            acc -= n as f64;
            for s in pcm[..n].iter_mut() {
                *s = sine(idx);
                idx += 1;
            }
            c.push(&pcm[..n]);
            if !c.pull_frame(&mut out) {
                empty_pulls += 1;
            }
            max_backlog = max_backlog.max(c.buffered_frames());
            if tick % 400 == 0 {
                let e = c.buffered_frames() as f64 / 120.0 - 2.0;
                integral += 10.0 * e;
                c.steer(-(300.0 * e + integral));
            }
        }
        assert!(
            (-260.0..=-140.0).contains(&c.ratio_ppm()),
            "steering should converge near -200 ppm, ended at {:.1}",
            c.ratio_ppm()
        );
        assert!(
            max_backlog <= 8 * 120,
            "backlog should stay bounded, peaked at {max_backlog} samples"
        );
        // Only the very first ticks may come up short of a full chunk.
        assert!(
            empty_pulls <= 4,
            "consumption starved {empty_pulls} times over 60 s"
        );
    }

    #[test]
    fn steer_slews_and_clamps() {
        let mut c = DriftCompensator::new(120, 1);
        // Beyond the authority: clamped to +500 ppm, approached at
        // <= 1 ppm per pulled frame.
        c.steer(50_000.0);
        let pcm = [0.0f32; 121];
        let mut out = [0.0f32; 120];
        let mut prev = c.ratio_ppm();
        assert_eq!(prev, 0.0);
        for _ in 0..1_000 {
            c.push(&pcm);
            assert!(c.pull_frame(&mut out));
            let now = c.ratio_ppm();
            assert!(
                (now - prev).abs() <= 1.0 + 1e-9,
                "ratio jumped from {prev:.3} to {now:.3} ppm"
            );
            assert!(now <= 500.0 + 1e-9, "ratio exceeded the clamp: {now:.3}");
            prev = now;
        }
        assert!(
            (prev - 500.0).abs() < 1e-6,
            "ratio should have reached the +500 ppm clamp, sits at {prev:.3}"
        );
        // And back down toward a negative target at the same bounded pace.
        c.steer(-200.0);
        for _ in 0..1_000 {
            c.push(&pcm[..120]);
            assert!(c.pull_frame(&mut out));
            let now = c.ratio_ppm();
            assert!((now - prev).abs() <= 1.0 + 1e-9);
            prev = now;
        }
        assert!(
            (prev + 200.0).abs() < 1e-6,
            "expected -200 ppm, got {prev:.3}"
        );
    }

    #[test]
    fn no_allocation_after_construction() {
        let mut c = DriftCompensator::new(120, 2);
        c.steer(400.0);
        let pcm = [0.05f32; 250];
        let mut out = [0.0f32; 240];
        // Warmup: reach steady state and exercise the correction path once.
        for _ in 0..500 {
            c.push(&pcm[..240]);
            while c.pull_frame(&mut out) {}
        }
        let before = heap_ops();
        for i in 0..5_000usize {
            // Odd lengths, steering changes, and pulls: the full RT surface.
            c.push(&pcm[..240 + 2 * (i % 6)]);
            c.steer(if i % 2 == 0 { -350.0 } else { 450.0 });
            while c.pull_frame(&mut out) {}
        }
        // Overflow path: push without pulling until the ring drops oldest.
        for _ in 0..100 {
            c.push(&pcm);
        }
        assert_eq!(
            heap_ops() - before,
            0,
            "push/pull_frame/steer allocated after construction"
        );
    }

    #[test]
    fn overflow_drops_oldest_and_stays_bounded() {
        let mut c = DriftCompensator::new(120, 1);
        let pcm = [1.0f32; 120];
        for _ in 0..10_000 {
            c.push(&pcm);
        }
        assert!(
            c.buffered_frames() <= BUFFER_CHUNKS * 120,
            "backlog grew past capacity: {}",
            c.buffered_frames()
        );
        let mut out = [0.0f32; 120];
        assert!(c.pull_frame(&mut out));
    }
}
