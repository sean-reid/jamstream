//! Mono musician sources into an interleaved stereo bus with per-member
//! fader, constant-power pan, and an optional excluded member for the
//! personal mixes.

use jamstream_protocol::ids::MemberId;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fader {
    pub gain_db: f32,
    /// -1 hard left, 0 center, 1 hard right.
    pub pan: f32,
    pub muted: bool,
}

impl Default for Fader {
    fn default() -> Self {
        Self {
            gain_db: 0.0,
            pan: 0.0,
            muted: false,
        }
    }
}

pub fn db_to_lin(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// Sums mono `sources` into `out` (interleaved stereo, twice the source
/// length), zeroing `out` first. Constant-power pan: left/right weights are
/// cos/sin over the quarter circle, so panning moves energy, not level.
pub fn mix_into(
    sources: &[(MemberId, &[f32])],
    faders: impl Fn(MemberId) -> Fader,
    exclude: Option<MemberId>,
    out: &mut [f32],
) {
    assert!(out.len() % 2 == 0, "stereo bus length must be even");
    out.fill(0.0);
    let frame = out.len() / 2;
    for &(member, source) in sources {
        assert_eq!(source.len(), frame, "source length must match the bus");
        if exclude == Some(member) {
            continue;
        }
        let fader = faders(member);
        if fader.muted {
            continue;
        }
        let gain = db_to_lin(fader.gain_db);
        let angle = (fader.pan.clamp(-1.0, 1.0) + 1.0) * core::f32::consts::FRAC_PI_4;
        let left = angle.cos() * gain;
        let right = angle.sin() * gain;
        for (pair, &sample) in out.chunks_exact_mut(2).zip(source) {
            pair[0] += sample * left;
            pair[1] += sample * right;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const CENTER: f32 = core::f32::consts::FRAC_1_SQRT_2;

    fn id(n: u16) -> MemberId {
        MemberId(n)
    }

    #[test]
    fn unity_center_pan_sums_sources() {
        let a = [0.5f32, -0.25, 1.0, 0.0];
        let b = [0.1f32, 0.2, -0.3, 0.4];
        let sources = [(id(1), &a[..]), (id(2), &b[..])];
        let mut out = [f32::NAN; 8];
        mix_into(&sources, |_| Fader::default(), None, &mut out);
        for i in 0..4 {
            let expected = (a[i] + b[i]) * CENTER;
            assert!((out[2 * i] - expected).abs() < 1e-6);
            assert!((out[2 * i + 1] - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn exclude_drops_that_member_only() {
        let a = [1.0f32, 1.0];
        let b = [0.5f32, 0.5];
        let sources = [(id(1), &a[..]), (id(2), &b[..])];
        let mut out = [0.0f32; 4];
        mix_into(&sources, |_| Fader::default(), Some(id(1)), &mut out);
        for pair in out.chunks_exact(2) {
            assert!((pair[0] - 0.5 * CENTER).abs() < 1e-6);
            assert!((pair[1] - 0.5 * CENTER).abs() < 1e-6);
        }
    }

    #[test]
    fn mute_silences_a_member() {
        let a = [1.0f32; 4];
        let sources = [(id(7), &a[..])];
        let mut out = [1.0f32; 8];
        let faders = |_| Fader {
            muted: true,
            ..Fader::default()
        };
        mix_into(&sources, faders, None, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn gain_applies_in_db() {
        let a = [1.0f32];
        let sources = [(id(1), &a[..])];
        let mut out = [0.0f32; 2];
        let faders = |_| Fader {
            gain_db: -6.0,
            ..Fader::default()
        };
        mix_into(&sources, faders, None, &mut out);
        let expected = db_to_lin(-6.0) * CENTER;
        assert!((out[0] - expected).abs() < 1e-6);
    }

    #[test]
    fn constant_power_pan_preserves_energy() {
        let a = [1.0f32];
        let sources = [(id(1), &a[..])];
        let mut pan = -1.0f32;
        while pan <= 1.0 {
            let faders = move |_| Fader {
                pan,
                ..Fader::default()
            };
            let mut out = [0.0f32; 2];
            mix_into(&sources, faders, None, &mut out);
            let energy_db = 10.0 * (out[0] * out[0] + out[1] * out[1]).log10();
            assert!(
                energy_db.abs() < 0.1,
                "pan {pan}: energy off by {energy_db} dB"
            );
            pan += 0.05;
        }
    }

    proptest! {
        #[test]
        fn output_is_finite_and_bounded(
            (faders, frames) in (1usize..5).prop_flat_map(|n| (
                prop::collection::vec(
                    (-60.0f32..12.0, -1.0f32..1.0, any::<bool>()),
                    n,
                ),
                prop::collection::vec(
                    prop::collection::vec(-1.0f32..1.0, 32),
                    n,
                ),
            ))
        ) {
            let sources: Vec<(MemberId, &[f32])> = frames
                .iter()
                .enumerate()
                .map(|(i, f)| (id(i as u16), f.as_slice()))
                .collect();
            let fader_of = |m: MemberId| {
                let (gain_db, pan, muted) = faders[m.0 as usize];
                Fader { gain_db, pan, muted }
            };
            let mut out = vec![0.0f32; 64];
            mix_into(&sources, fader_of, None, &mut out);
            let bound: f32 = faders
                .iter()
                .filter(|(_, _, muted)| !muted)
                .map(|&(gain_db, _, _)| db_to_lin(gain_db))
                .sum();
            for &s in &out {
                prop_assert!(s.is_finite());
                prop_assert!(s.abs() <= bound + 1e-3);
            }
        }
    }
}
