//! Sample-accurate click generator. Every sample is a pure function of the
//! absolute sample clock, so any render window split reproduces the same
//! audio and server restarts cannot drift the click.

use core::f32::consts::TAU;

/// 4 ms at 48 kHz.
const CLICK_SAMPLES: u128 = 192;
/// Envelope time constant: 1 ms.
const DECAY_SAMPLES: f32 = 48.0;
const SAMPLES_PER_MINUTE: u128 = 60 * 48_000;
const BEAT_HZ: f32 = 1_000.0;
const BAR_HZ: f32 = 1_500.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metronome {
    pub bpm: u16,
    pub beats_per_bar: u8,
}

impl Metronome {
    /// Adds click samples into `out_mono` for the absolute sample window
    /// `[sample_clock, sample_clock + out_mono.len())`.
    pub fn render(&self, sample_clock: u64, out_mono: &mut [f32], gain: f32) {
        if self.bpm == 0 {
            return;
        }
        for (i, slot) in out_mono.iter_mut().enumerate() {
            *slot += self.click_sample(sample_clock + i as u64) * gain;
        }
    }

    fn click_sample(&self, sample: u64) -> f32 {
        let bpm = u128::from(self.bpm);
        let sample = u128::from(sample);
        // Beat n spans [ceil(n * spm / bpm), ceil((n + 1) * spm / bpm)).
        let beat = sample * bpm / SAMPLES_PER_MINUTE;
        let beat_start = (beat * SAMPLES_PER_MINUTE).div_ceil(bpm);
        let offset = sample - beat_start;
        if offset >= CLICK_SAMPLES {
            return 0.0;
        }
        let accented = beat % u128::from(self.beats_per_bar.max(1)) == 0;
        let freq = if accented { BAR_HZ } else { BEAT_HZ };
        let t = offset as f32;
        (TAU * freq * t / 48_000.0).sin() * (-t / DECAY_SAMPLES).exp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const CLICK: usize = 192;

    #[test]
    fn click_starts_exactly_on_beat_boundaries() {
        // 120 bpm at 48 kHz: one beat every 24_000 samples exactly.
        let m = Metronome {
            bpm: 120,
            beats_per_bar: 4,
        };
        for beat in 1u64..6 {
            let boundary = beat * 24_000;
            let mut buf = vec![0.0f32; 400];
            m.render(boundary - 200, &mut buf, 1.0);
            // Silent right up to the boundary: the previous click is long
            // over and the new one has not begun.
            assert!(buf[..200].iter().all(|&s| s == 0.0), "beat {beat}");
            // Phase zero at the boundary itself, energy right after.
            assert_eq!(buf[200], 0.0);
            assert!(buf[201] != 0.0, "beat {beat}");
            assert!(buf[200..].iter().any(|&s| s.abs() > 0.1), "beat {beat}");
        }
    }

    #[test]
    fn off_beat_regions_are_silent() {
        let m = Metronome {
            bpm: 120,
            beats_per_bar: 4,
        };
        let mut buf = vec![0.0f32; 24_000 - CLICK - 2];
        m.render(CLICK as u64 + 1, &mut buf, 1.0);
        assert!(buf.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn bar_accent_uses_a_higher_frequency() {
        let m = Metronome {
            bpm: 120,
            beats_per_bar: 4,
        };
        let mut bar_click = vec![0.0f32; CLICK];
        let mut beat_click = vec![0.0f32; CLICK];
        m.render(0, &mut bar_click, 1.0);
        m.render(24_000, &mut beat_click, 1.0);
        // 1.5 kHz crosses zero more often than 1 kHz over the same window.
        let bar_crossings = zero_crossings(&bar_click);
        let beat_crossings = zero_crossings(&beat_click);
        assert!(
            bar_crossings > beat_crossings,
            "bar {bar_crossings} vs beat {beat_crossings}"
        );
        assert_ne!(bar_click, beat_click);
    }

    #[test]
    fn render_adds_instead_of_overwriting() {
        let m = Metronome {
            bpm: 120,
            beats_per_bar: 4,
        };
        let mut base = vec![0.25f32; 64];
        let mut click = vec![0.0f32; 64];
        m.render(0, &mut click, 0.5);
        m.render(0, &mut base, 0.5);
        for i in 0..64 {
            assert_eq!(base[i], 0.25 + click[i]);
        }
    }

    #[test]
    fn zero_bpm_is_silent() {
        let m = Metronome {
            bpm: 0,
            beats_per_bar: 4,
        };
        let mut buf = vec![0.0f32; 128];
        m.render(0, &mut buf, 1.0);
        assert!(buf.iter().all(|&s| s == 0.0));
    }

    fn zero_crossings(buf: &[f32]) -> usize {
        buf.windows(2)
            .filter(|w| (w[0] > 0.0 && w[1] <= 0.0) || (w[0] < 0.0 && w[1] >= 0.0))
            .count()
    }

    proptest! {
        #[test]
        fn window_splits_are_seamless(
            clock in 0u64..(1 << 40),
            len in 1usize..512,
            split_frac in 0.0f64..1.0,
            bpm in 1u16..400,
            beats_per_bar in 1u8..12,
        ) {
            let m = Metronome { bpm, beats_per_bar };
            let split = (len as f64 * split_frac) as usize;
            let mut whole = vec![0.0f32; len];
            m.render(clock, &mut whole, 0.8);
            let mut parts = vec![0.0f32; len];
            let (head, tail) = parts.split_at_mut(split);
            m.render(clock, head, 0.8);
            m.render(clock + split as u64, tail, 0.8);
            prop_assert_eq!(whole, parts);
        }
    }
}
