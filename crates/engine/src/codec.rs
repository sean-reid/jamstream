//! Opus encode/decode wrappers pinned to the session format: 48 kHz f32,
//! frame sizes from the protocol's FrameDuration set.

use jamstream_protocol::SAMPLE_RATE;
use jamstream_protocol::media::FrameDuration;

const _: () = assert!(SAMPLE_RATE == 48_000);

/// Largest single Opus frame per RFC 6716 is 1275 bytes; round up for slack.
const MAX_PACKET: usize = 1500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels {
    Mono,
    Stereo,
}

impl Channels {
    pub fn count(self) -> usize {
        match self {
            Channels::Mono => 1,
            Channels::Stereo => 2,
        }
    }

    fn to_opus(self) -> opus::Channels {
        match self {
            Channels::Mono => opus::Channels::Mono,
            Channels::Stereo => opus::Channels::Stereo,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("opus: {}", .0.message())]
    Opus(opus::ErrorCode),
    #[error("pcm buffer holds {got} samples, frame needs {expected}")]
    BadPcmLength { got: usize, expected: usize },
    #[error("decoder produced {got} samples per channel, expected {expected}")]
    FrameMismatch { got: usize, expected: usize },
}

impl From<opus::ErrorCode> for CodecError {
    fn from(code: opus::ErrorCode) -> Self {
        CodecError::Opus(code)
    }
}

pub struct Encoder {
    inner: opus::Encoder,
    pcm_len: usize,
}

impl Encoder {
    /// Durations under 10 ms force CELT via LowDelay; Opus in-band FEC
    /// (LBRR) only exists in the SILK/hybrid path, so it is enabled for
    /// the 10 and 20 ms listener frames only. Short frames rely on the
    /// app-layer redundancy in `redundancy`.
    pub fn new(
        channels: Channels,
        duration: FrameDuration,
        bitrate_bps: u32,
    ) -> Result<Self, CodecError> {
        let app = match duration {
            FrameDuration::Ms2_5 | FrameDuration::Ms5 => opus::Application::LowDelay,
            FrameDuration::Ms10 | FrameDuration::Ms20 => opus::Application::Audio,
        };
        let mut inner = opus::Encoder::new(channels.to_opus(), opus::SampleRate::Hz48000, app)?;
        inner.set_bitrate(opus::Bitrate::Value(bitrate_bps))?;
        if matches!(duration, FrameDuration::Ms10 | FrameDuration::Ms20) {
            inner.set_inband_fec(opus::InbandFec::Mode1)?;
            inner.set_packet_loss(10)?;
        }
        Ok(Self {
            inner,
            pcm_len: duration.samples() as usize * channels.count(),
        })
    }

    /// Encodes exactly one frame. `pcm` is interleaved when stereo and its
    /// length must equal `duration.samples() * channels`. `out` is cleared
    /// and refilled.
    pub fn encode(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> Result<(), CodecError> {
        if pcm.len() != self.pcm_len {
            return Err(CodecError::BadPcmLength {
                got: pcm.len(),
                expected: self.pcm_len,
            });
        }
        out.clear();
        out.reserve(MAX_PACKET);
        self.inner.encode_float_to_vec(pcm, out)?;
        Ok(())
    }
}

pub struct Decoder {
    inner: opus::Decoder,
    pcm_len: usize,
    frame_samples: usize,
}

impl Decoder {
    pub fn new(channels: Channels, duration: FrameDuration) -> Result<Self, CodecError> {
        let inner = opus::Decoder::new(channels.to_opus(), opus::SampleRate::Hz48000)?;
        Ok(Self {
            inner,
            pcm_len: duration.samples() as usize * channels.count(),
            frame_samples: duration.samples() as usize,
        })
    }

    /// Decodes one frame into `out`, whose length must equal
    /// `duration.samples() * channels`. `None` runs packet loss concealment.
    /// `fec` asks the packet for its in-band FEC copy of the previous frame.
    pub fn decode(
        &mut self,
        payload: Option<&[u8]>,
        out: &mut [f32],
        fec: bool,
    ) -> Result<(), CodecError> {
        if out.len() != self.pcm_len {
            return Err(CodecError::BadPcmLength {
                got: out.len(),
                expected: self.pcm_len,
            });
        }
        let input = payload.unwrap_or(&[]);
        let decoded = self
            .inner
            .decode_float_to_slice(input, out, fec && payload.is_some())?;
        if decoded != self.frame_samples {
            return Err(CodecError::FrameMismatch {
                got: decoded,
                expected: self.frame_samples,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_DURATIONS: [FrameDuration; 4] = [
        FrameDuration::Ms2_5,
        FrameDuration::Ms5,
        FrameDuration::Ms10,
        FrameDuration::Ms20,
    ];

    fn sine(len: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|i| (core::f32::consts::TAU * freq * i as f32 / 48_000.0).sin() * amp)
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// Goertzel amplitude of one tone in a mono window: a sine at amplitude a
    /// reads ~a, and a window without that tone reads near zero.
    fn tone_amp(mono: &[f32], hz: f32) -> f32 {
        let w = core::f32::consts::TAU * hz / 48_000.0;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for x in mono {
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0);
        2.0 * power.sqrt() / mono.len() as f32
    }

    /// Peak normalized cross-correlation of y against x over positive lags,
    /// which absorbs codec delay.
    fn corr_peak(x: &[f32], y: &[f32], max_lag: usize, span: usize) -> f32 {
        let start = span / 2;
        let mut best = f32::NEG_INFINITY;
        for lag in 0..=max_lag {
            let mut xy = 0.0f64;
            let mut xx = 0.0f64;
            let mut yy = 0.0f64;
            for i in start..start + span {
                let (a, b) = (f64::from(x[i]), f64::from(y[i + lag]));
                xy += a * b;
                xx += a * a;
                yy += b * b;
            }
            if xx > 0.0 && yy > 0.0 {
                best = best.max((xy / (xx * yy).sqrt()) as f32);
            }
        }
        best
    }

    #[test]
    fn sine_round_trips_at_every_duration() {
        for duration in ALL_DURATIONS {
            let frame = duration.samples() as usize;
            let total = frame * (19_200 / frame);
            let input = sine(total, 440.0, 0.5);
            let mut enc = Encoder::new(Channels::Mono, duration, 128_000).unwrap();
            let mut dec = Decoder::new(Channels::Mono, duration).unwrap();
            let mut packet = Vec::new();
            let mut decoded = vec![0.0f32; total];
            for (i, chunk) in input.chunks_exact(frame).enumerate() {
                enc.encode(chunk, &mut packet).unwrap();
                assert!(!packet.is_empty());
                dec.decode(
                    Some(&packet),
                    &mut decoded[i * frame..(i + 1) * frame],
                    false,
                )
                .unwrap();
            }
            let peak = corr_peak(&input, &decoded, 1_500, 4_800);
            assert!(peak > 0.8, "{duration:?}: correlation peak {peak}");
        }
    }

    /// Stereo is what the personal mix encodes and every client decodes, and
    /// each channel has to arrive on its own side: a mono collapse or a swap
    /// reads as the wrong tone here, while both survive any assertion that only
    /// asks for finite numbers. Distinct tones per channel are what tells them
    /// apart, and a mono sum of two equal sines correlates 0.71 with either.
    #[test]
    fn stereo_round_trips_on_both_channels() {
        for duration in ALL_DURATIONS {
            let frame = duration.samples() as usize;
            let per_channel = frame * (19_200 / frame);
            let left = sine(per_channel, 440.0, 0.5);
            let right = sine(per_channel, 660.0, 0.5);
            let mut input = vec![0.0f32; per_channel * 2];
            for (i, pair) in input.chunks_exact_mut(2).enumerate() {
                pair[0] = left[i];
                pair[1] = right[i];
            }
            let mut enc = Encoder::new(Channels::Stereo, duration, 192_000).unwrap();
            let mut dec = Decoder::new(Channels::Stereo, duration).unwrap();
            let mut packet = Vec::new();
            let mut decoded = vec![0.0f32; per_channel * 2];
            for (i, chunk) in input.chunks_exact(frame * 2).enumerate() {
                enc.encode(chunk, &mut packet).unwrap();
                dec.decode(
                    Some(&packet),
                    &mut decoded[i * frame * 2..(i + 1) * frame * 2],
                    false,
                )
                .unwrap();
            }
            let out_left: Vec<f32> = decoded.chunks_exact(2).map(|p| p[0]).collect();
            let out_right: Vec<f32> = decoded.chunks_exact(2).map(|p| p[1]).collect();
            let peak_left = corr_peak(&left, &out_left, 1_500, 4_800);
            let peak_right = corr_peak(&right, &out_right, 1_500, 4_800);
            assert!(peak_left > 0.8, "{duration:?}: left peak {peak_left}");
            assert!(peak_right > 0.8, "{duration:?}: right peak {peak_right}");
        }
    }

    /// Concealment stands in for audio, so it has to sound like the audio it
    /// replaced: Opus extends the pitch it was playing. Every lost frame in the
    /// client and the server takes this path, and a zero fill is hard silence
    /// and a click on each one.
    #[test]
    fn plc_continues_the_tone_it_conceals() {
        for duration in ALL_DURATIONS {
            let frame = duration.samples() as usize;
            let mut enc = Encoder::new(Channels::Mono, duration, 128_000).unwrap();
            let mut dec = Decoder::new(Channels::Mono, duration).unwrap();
            let mut packet = Vec::new();
            let mut out = vec![0.0f32; frame];
            for chunk in sine(frame * 8, 440.0, 0.9).chunks_exact(frame) {
                enc.encode(chunk, &mut packet).unwrap();
                dec.decode(Some(&packet), &mut out, false).unwrap();
            }
            let playing = rms(&out);

            // 10 ms of concealment: four cycles of 440 Hz to measure, and
            // inside the span Opus extends a pitch before fading it out.
            let mut concealed = Vec::new();
            for _ in 0..(480 / frame).max(1) {
                // `out` holds a real frame, so a decode that wrote nothing at
                // all would leave audio here for the measurement to find.
                out.fill(f32::NAN);
                dec.decode(None, &mut out, false).unwrap();
                concealed.extend_from_slice(&out);
            }
            assert!(
                concealed.iter().all(|s| s.is_finite()),
                "{duration:?}: concealment left samples unwritten"
            );
            let level = rms(&concealed);
            assert!(
                level > 0.5 * playing,
                "{duration:?}: concealment at {level} against {playing} playing"
            );
            // A pure sine's Goertzel amplitude is 1.41 times its rms, so this
            // floor puts over half the concealed energy at the pitch that was
            // lost; noise and a click both read far under it.
            let amp = tone_amp(&concealed, 440.0);
            assert!(
                amp > level,
                "{duration:?}: concealment is not the tone, 440 Hz at {amp} in {level}"
            );
        }
    }

    #[test]
    fn wrong_pcm_length_errors() {
        let duration = FrameDuration::Ms5;
        let frame = duration.samples() as usize;
        let mut enc = Encoder::new(Channels::Mono, duration, 96_000).unwrap();
        let mut out = Vec::new();
        assert!(matches!(
            enc.encode(&vec![0.0; frame - 1], &mut out),
            Err(CodecError::BadPcmLength { .. })
        ));
        assert!(matches!(
            enc.encode(&vec![0.0; frame * 2], &mut out),
            Err(CodecError::BadPcmLength { .. })
        ));
        let mut dec = Decoder::new(Channels::Mono, duration).unwrap();
        let mut short = vec![0.0f32; frame - 1];
        assert!(matches!(
            dec.decode(None, &mut short, false),
            Err(CodecError::BadPcmLength { .. })
        ));
    }
}
