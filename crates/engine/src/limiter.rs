//! Brickwall lookahead limiter for the broadcast mix. The lookahead window
//! lets gain reach its floor before a peak plays, so the ceiling is a hard
//! guarantee rather than an aspiration.

use crate::mixer::db_to_lin;

/// Release time constant in samples: about 50 ms at 48 kHz.
const RELEASE_SAMPLES: f32 = 0.050 * 48_000.0;

/// Streaming stereo limiter. Latency is exactly `lookahead_samples` sample
/// frames: `process` emits input delayed by that much, and the first call
/// begins with that much silence.
pub struct Limiter {
    ceiling: f32,
    release: f32,
    gain: f32,
    /// Interleaved delay line, `2 * lookahead` long.
    delay: Vec<f32>,
    /// Per-pair peak magnitudes for the window scan, parallel to `delay`.
    peaks: Vec<f32>,
    pos: usize,
}

impl Limiter {
    pub fn new(ceiling_db: f32, lookahead_samples: usize) -> Self {
        Self {
            ceiling: db_to_lin(ceiling_db),
            release: 1.0 - (-1.0 / RELEASE_SAMPLES).exp(),
            gain: 1.0,
            delay: vec![0.0; lookahead_samples * 2],
            peaks: vec![0.0; lookahead_samples],
            pos: 0,
        }
    }

    pub fn latency_samples(&self) -> usize {
        self.peaks.len()
    }

    /// Processes interleaved stereo in place, streaming across calls.
    pub fn process(&mut self, interleaved_stereo: &mut [f32]) {
        assert!(
            interleaved_stereo.len() % 2 == 0,
            "stereo buffer length must be even"
        );
        for pair in interleaved_stereo.chunks_exact_mut(2) {
            let (l, r) = (sanitize(pair[0]), sanitize(pair[1]));
            let peak_new = l.abs().max(r.abs());

            // Window peak covers everything queued, the pair about to leave
            // included, plus the pair entering now.
            let window_peak = self.peaks.iter().fold(peak_new, |m, &p| m.max(p));
            let target = if window_peak > self.ceiling {
                self.ceiling / window_peak
            } else {
                1.0
            };
            // Instant attack downward keeps the guarantee; release eases
            // back up. Both leave gain <= ceiling / window_peak.
            if target < self.gain {
                self.gain = target;
            } else {
                self.gain += (target - self.gain) * self.release;
            }

            let (out_l, out_r) = if self.peaks.is_empty() {
                (l, r)
            } else {
                let slot = self.pos;
                let (dl, dr) = (self.delay[2 * slot], self.delay[2 * slot + 1]);
                self.delay[2 * slot] = l;
                self.delay[2 * slot + 1] = r;
                self.peaks[slot] = peak_new;
                self.pos = (slot + 1) % self.peaks.len();
                (dl, dr)
            };
            // Clamp only guards float rounding in gain; it never engages on
            // signals already under the ceiling, so quiet audio is bit-exact.
            pair[0] = (out_l * self.gain).clamp(-self.ceiling, self.ceiling);
            pair[1] = (out_r * self.gain).clamp(-self.ceiling, self.ceiling);
        }
    }
}

fn sanitize(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn stereo_sine(len_pairs: usize, freq: f32, amp: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(len_pairs * 2);
        for i in 0..len_pairs {
            let s = (core::f32::consts::TAU * freq * i as f32 / 48_000.0).sin() * amp;
            out.push(s);
            out.push(s);
        }
        out
    }

    #[test]
    fn hot_sine_stays_under_ceiling() {
        let ceiling_db = -1.0;
        let ceiling = db_to_lin(ceiling_db);
        let mut limiter = Limiter::new(ceiling_db, 96);
        // +6 dBFS
        let mut buf = stereo_sine(48_000, 440.0, db_to_lin(6.0));
        for chunk in buf.chunks_mut(240) {
            limiter.process(chunk);
        }
        let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= ceiling, "peak {peak} over ceiling {ceiling}");
        // It limits, it does not silence.
        assert!(peak > ceiling * 0.5);
    }

    #[test]
    fn silence_stays_silence() {
        let mut limiter = Limiter::new(-1.0, 64);
        let mut buf = vec![0.0f32; 2_048];
        limiter.process(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn quiet_signal_passes_bit_exact_after_latency() {
        let lookahead = 96;
        let mut limiter = Limiter::new(-1.0, lookahead);
        assert_eq!(limiter.latency_samples(), lookahead);
        let input = stereo_sine(4_800, 440.0, 0.1);
        let mut buf = input.clone();
        for chunk in buf.chunks_mut(256) {
            limiter.process(chunk);
        }
        let shift = lookahead * 2;
        assert_eq!(&buf[shift..], &input[..input.len() - shift]);
    }

    #[test]
    fn non_finite_input_produces_finite_output() {
        let mut limiter = Limiter::new(-1.0, 32);
        let mut buf = vec![
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            1e30,
            -1e30,
            0.5,
            f32::NAN,
            f32::NAN,
        ];
        buf.extend(std::iter::repeat_n(0.25f32, 256));
        limiter.process(&mut buf);
        let ceiling = db_to_lin(-1.0);
        for &s in &buf {
            assert!(s.is_finite());
            assert!(s.abs() <= ceiling);
        }
    }

    proptest! {
        #[test]
        fn never_exceeds_ceiling_never_nan(
            bits in prop::collection::vec(any::<u32>(), 0..512),
            ceiling_db in -20.0f32..0.0,
            lookahead in 0usize..128,
        ) {
            let mut buf: Vec<f32> = bits.iter().map(|&b| f32::from_bits(b)).collect();
            if buf.len() % 2 != 0 {
                buf.pop();
            }
            let ceiling = db_to_lin(ceiling_db);
            let mut limiter = Limiter::new(ceiling_db, lookahead);
            let mid = (buf.len() / 2) & !1;
            let (a, b) = buf.split_at_mut(mid);
            limiter.process(a);
            limiter.process(b);
            for &s in buf.iter() {
                prop_assert!(s.is_finite());
                prop_assert!(s.abs() <= ceiling);
            }
        }
    }
}
