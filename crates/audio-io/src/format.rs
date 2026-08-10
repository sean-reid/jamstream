//! Sample and frame conversion at the device edge, plus the exclusive-mode
//! format candidate list.
//!
//! Kept free of any platform API so it is unit-testable on every host: the
//! Windows exclusive backend cannot be exercised on macOS or Linux, but the
//! negotiation order and every byte-level conversion it performs can.
//!
//! Everything here is allocation-free except `format_candidates`, which
//! runs once per stream open, never on a device thread.

/// Interleaved sample layouts a WASAPI exclusive-mode device may accept.
///
/// Integer samples are little-endian two's complement. `I24In32` is a 24-bit
/// sample left-justified in a 32-bit container, which is how
/// `WAVEFORMATEXTENSIBLE` describes `wBitsPerSample = 32,
/// wValidBitsPerSample = 24`; the low 8 bits are zero on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleFormat {
    /// 32-bit IEEE float, the layout the handler already speaks.
    F32,
    /// 32-bit integer, all bits valid.
    I32,
    /// 24 valid bits, left-justified in a 32-bit container.
    I24In32,
    /// Packed 24-bit integer, three bytes per sample.
    I24,
    /// 16-bit integer.
    I16,
}

impl SampleFormat {
    /// Container size in bits (`wBitsPerSample`).
    pub(crate) const fn store_bits(self) -> u16 {
        match self {
            Self::F32 | Self::I32 | Self::I24In32 => 32,
            Self::I24 => 24,
            Self::I16 => 16,
        }
    }

    /// Meaningful bits in the container (`wValidBitsPerSample`).
    pub(crate) const fn valid_bits(self) -> u16 {
        match self {
            Self::F32 | Self::I32 => 32,
            Self::I24In32 | Self::I24 => 24,
            Self::I16 => 16,
        }
    }

    pub(crate) const fn is_float(self) -> bool {
        matches!(self, Self::F32)
    }

    /// Bytes per single sample, i.e. per channel per frame.
    pub(crate) const fn bytes(self) -> usize {
        self.store_bits() as usize / 8
    }
}

/// One fully specified exclusive-mode format to offer the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FormatSpec {
    pub(crate) format: SampleFormat,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
}

impl FormatSpec {
    /// Bytes per frame (`nBlockAlign`).
    pub(crate) const fn block_align(&self) -> usize {
        self.format.bytes() * self.channels as usize
    }

    /// True when a format the driver accepted frames audio exactly as this
    /// spec does.
    ///
    /// `is_supported_exclusive_with_quirks` may hand back a plain
    /// `WAVEFORMATEX` copy whose `SubFormat` GUID is zeroed, so these three
    /// framing fields are the only ones worth comparing, and a candidate whose
    /// framing disagrees with ours is skipped rather than trusted.
    pub(crate) const fn frames_like(
        &self,
        channels: u16,
        sample_rate: u32,
        block_align: u32,
    ) -> bool {
        self.channels == channels
            && self.sample_rate == sample_rate
            && self.block_align() == block_align as usize
    }
}

/// Scratch sizes for one direction's conversion stage, worked out once during
/// the open so a device thread only ever takes subslices.
///
/// The byte buffer and the device float buffer always describe the same
/// samples, which is what lets [`decode_to_f32`] and [`encode_from_f32`] be
/// handed a pair of subslices and trusted to agree on how many there are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StageLayout {
    pub(crate) format: SampleFormat,
    pub(crate) device_channels: usize,
    pub(crate) handler_channels: usize,
    /// Bytes per frame on the device side. Derived from `device_channels`
    /// rather than the spec's own count, so it cannot disagree with the float
    /// buffer about how many samples a frame holds.
    pub(crate) block_align: usize,
    frames: usize,
}

impl StageLayout {
    /// `periods` is how many device periods the scratch must hold: one for
    /// render, which writes at most a period per event, and more for capture,
    /// where a late wake-up can find several waiting.
    pub(crate) fn new(
        spec: FormatSpec,
        handler_channels: u16,
        buffer_frames: u32,
        periods: usize,
    ) -> Self {
        let device_channels = usize::from(spec.channels.max(1));
        Self {
            format: spec.format,
            device_channels,
            handler_channels: usize::from(handler_channels.max(1)),
            block_align: spec.format.bytes() * device_channels,
            frames: periods * buffer_frames as usize,
        }
    }

    pub(crate) const fn byte_len(self) -> usize {
        self.frames * self.block_align
    }

    pub(crate) const fn device_float_len(self) -> usize {
        self.frames * self.device_channels
    }

    pub(crate) const fn handler_float_len(self) -> usize {
        self.frames * self.handler_channels
    }
}

/// Sample layouts in the order we offer them, best first.
///
/// `F32` first because it needs no conversion at all; then the widest integer
/// container, since exclusive mode means the driver, not the audio engine,
/// does any final requantisation. `I16` last: it is the only one that costs
/// audible resolution.
const FORMAT_PREFERENCE: [SampleFormat; 5] = [
    SampleFormat::F32,
    SampleFormat::I32,
    SampleFormat::I24In32,
    SampleFormat::I24,
    SampleFormat::I16,
];

/// Formats to try against a device, best first.
///
/// The requested channel count is tried in every layout before the device's
/// own channel count is tried in any of them: converting channels is cheap
/// and lossless-ish (see [`map_frames`]), whereas an unnecessary sample
/// format change is not. `native_channels` is skipped when it is absent, zero,
/// or equal to the request.
pub(crate) fn format_candidates(
    sample_rate: u32,
    channels: u16,
    native_channels: Option<u16>,
) -> Vec<FormatSpec> {
    let mut counts = vec![channels];
    if let Some(native) = native_channels
        && native != 0
        && native != channels
    {
        counts.push(native);
    }
    let mut out = Vec::with_capacity(counts.len() * FORMAT_PREFERENCE.len());
    for count in counts {
        for format in FORMAT_PREFERENCE {
            out.push(FormatSpec {
                format,
                sample_rate,
                channels: count,
            });
        }
    }
    out
}

/// Map one interleaved frame layout onto another: destination channel i takes
/// source channel min(i, src_channels - 1), so a mono source fans out to every
/// destination channel and extra source channels are dropped.
///
/// Shared with the cpal path so both backends present the same channel
/// semantics to the handler.
pub(crate) fn map_frames(src: &[f32], src_ch: usize, dst: &mut [f32], dst_ch: usize) {
    for (s, d) in src.chunks_exact(src_ch).zip(dst.chunks_exact_mut(dst_ch)) {
        for (i, slot) in d.iter_mut().enumerate() {
            *slot = s[i.min(src_ch - 1)];
        }
    }
}

/// Decode interleaved device bytes into f32 samples in [-1, 1].
///
/// Converts `min(dst.len(), src.len() / format.bytes())` samples; a partial
/// trailing sample in `src` is ignored and surplus `dst` is left untouched, so
/// a caller that sized its scratch generously cannot trip over a short packet.
pub(crate) fn decode_to_f32(src: &[u8], format: SampleFormat, dst: &mut [f32]) {
    let chunks = src.chunks_exact(format.bytes());
    match format {
        SampleFormat::F32 => {
            for (out, raw) in dst.iter_mut().zip(chunks) {
                *out = f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            }
        }
        SampleFormat::I32 | SampleFormat::I24In32 => {
            for (out, raw) in dst.iter_mut().zip(chunks) {
                let v = i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
                *out = v as f32 / I32_SCALE;
            }
        }
        SampleFormat::I24 => {
            for (out, raw) in dst.iter_mut().zip(chunks) {
                // Sign-extend the packed 24-bit value by landing it in the
                // top three bytes of an i32 and shifting arithmetically.
                let v = i32::from_le_bytes([0, raw[0], raw[1], raw[2]]) >> 8;
                *out = v as f32 / I24_SCALE;
            }
        }
        SampleFormat::I16 => {
            for (out, raw) in dst.iter_mut().zip(chunks) {
                let v = i16::from_le_bytes([raw[0], raw[1]]);
                *out = f32::from(v) / I16_SCALE;
            }
        }
    }
}

/// Encode f32 samples into interleaved device bytes, clamping to full scale.
///
/// Converts `min(src.len(), dst.len() / format.bytes())` samples. Out-of-range
/// input clips to full scale and NaN encodes as silence, so a misbehaving
/// handler cannot turn into a full-scale click in anyone's headphones. `F32`
/// is a straight byte copy and passes non-finite values through unchanged.
pub(crate) fn encode_from_f32(src: &[f32], format: SampleFormat, dst: &mut [u8]) {
    let chunks = dst.chunks_exact_mut(format.bytes());
    match format {
        SampleFormat::F32 => {
            for (raw, &s) in chunks.zip(src) {
                raw.copy_from_slice(&s.to_le_bytes());
            }
        }
        SampleFormat::I32 => {
            for (raw, &s) in chunks.zip(src) {
                // Float-to-int casts saturate in Rust, so +1.0 lands on
                // i32::MAX rather than wrapping.
                let v = (clamp_unit(s) * I32_SCALE) as i32;
                raw.copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::I24In32 => {
            for (raw, &s) in chunks.zip(src) {
                // Left-justified: keep 24 bits, zero the unused low byte.
                let v = ((clamp_unit(s) * I32_SCALE) as i32) & !0xFF;
                raw.copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::I24 => {
            for (raw, &s) in chunks.zip(src) {
                let v = (clamp_unit(s) * I24_SCALE).min(I24_MAX) as i32;
                raw.copy_from_slice(&v.to_le_bytes()[..3]);
            }
        }
        SampleFormat::I16 => {
            for (raw, &s) in chunks.zip(src) {
                let v = (clamp_unit(s) * I16_SCALE).min(I16_MAX) as i16;
                raw.copy_from_slice(&v.to_le_bytes());
            }
        }
    }
}

const I32_SCALE: f32 = 2_147_483_648.0;
const I24_SCALE: f32 = 8_388_608.0;
const I16_SCALE: f32 = 32_768.0;
const I24_MAX: f32 = 8_388_607.0;
const I16_MAX: f32 = 32_767.0;

/// Clamp to full scale, mapping NaN to silence.
///
/// The NaN branch is load-bearing: `f32::min` *returns the other operand* when
/// either is NaN, so without it a NaN would encode as positive full scale in
/// the formats whose ceiling is enforced with `min`.
fn clamp_unit(s: f32) -> f32 {
    if s.is_nan() { 0.0 } else { s.clamp(-1.0, 1.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(format: SampleFormat, channels: u16) -> FormatSpec {
        FormatSpec {
            format,
            sample_rate: 48_000,
            channels,
        }
    }

    /// The negotiation loop keys on these three fields and nothing else,
    /// because the driver may return a WAVEFORMATEX copy whose SubFormat GUID
    /// is zeroed. Each one has to be able to reject on its own.
    #[test]
    fn accepted_framing_is_compared_field_by_field() {
        let want = spec(SampleFormat::F32, 2);
        assert!(want.frames_like(2, 48_000, 8));
        assert!(!want.frames_like(1, 48_000, 8), "channel count ignored");
        assert!(!want.frames_like(2, 44_100, 8), "sample rate ignored");
        assert!(!want.frames_like(2, 48_000, 4), "block align ignored");
    }

    /// A driver that accepts I24In32 while framing it as packed I24 reports the
    /// channel count and rate we asked for and a block align that is 3 bytes a
    /// channel rather than 4. Trusting it would misread every frame, so the
    /// candidate has to be skipped.
    #[test]
    fn a_same_width_format_with_a_different_container_is_rejected() {
        let want = spec(SampleFormat::I24In32, 2);
        assert_eq!(want.block_align(), 8);
        assert!(!want.frames_like(2, 48_000, 6));
        assert!(spec(SampleFormat::I24, 2).frames_like(2, 48_000, 6));
    }

    /// The invariant the real-time path depends on: the byte scratch and the
    /// device float scratch always describe the same samples, so a decode or
    /// encode handed a subslice of each cannot disagree about how many there
    /// are.
    #[test]
    fn the_byte_and_float_scratch_always_hold_the_same_samples() {
        for format in FORMAT_PREFERENCE {
            for channels in [1u16, 2, 4, 8] {
                for frames in [1u32, 32, 240, 4_800] {
                    for periods in [1usize, 2] {
                        let layout = StageLayout::new(spec(format, channels), 2, frames, periods);
                        assert_eq!(
                            layout.byte_len(),
                            layout.device_float_len() * format.bytes(),
                            "{format:?} {channels} ch {frames} frames x{periods}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn scratch_covers_every_period_it_promised_to_hold() {
        let layout = StageLayout::new(spec(SampleFormat::I16, 4), 2, 240, 2);
        assert_eq!(layout.device_channels, 4);
        assert_eq!(layout.handler_channels, 2);
        assert_eq!(layout.block_align, 8);
        // Two periods of 240 frames, 4 channels of 2 bytes.
        assert_eq!(layout.byte_len(), 2 * 240 * 8);
        assert_eq!(layout.device_float_len(), 2 * 240 * 4);
        assert_eq!(layout.handler_float_len(), 2 * 240 * 2);
        // Render sizes for one period, and gets exactly half of each.
        let one = StageLayout::new(spec(SampleFormat::I16, 4), 2, 240, 1);
        assert_eq!(one.byte_len() * 2, layout.byte_len());
        assert_eq!(one.handler_float_len() * 2, layout.handler_float_len());
    }

    /// A zero channel count is refused during the open, so this is only about
    /// the arithmetic staying self-consistent if one ever arrived: a stage that
    /// sized its bytes from 0 channels and its floats from 1 would slice past
    /// the end of the byte buffer.
    #[test]
    fn zero_channels_cannot_desynchronise_the_two_buffers() {
        let layout = StageLayout::new(spec(SampleFormat::F32, 0), 0, 240, 1);
        assert_eq!(layout.device_channels, 1);
        assert_eq!(layout.handler_channels, 1);
        assert_eq!(layout.byte_len(), layout.device_float_len() * 4);
    }

    #[test]
    fn format_metrics_match_wave_format_fields() {
        assert_eq!(SampleFormat::F32.store_bits(), 32);
        assert_eq!(SampleFormat::F32.valid_bits(), 32);
        assert!(SampleFormat::F32.is_float());
        assert_eq!(SampleFormat::I24In32.store_bits(), 32);
        assert_eq!(SampleFormat::I24In32.valid_bits(), 24);
        assert!(!SampleFormat::I24In32.is_float());
        assert_eq!(SampleFormat::I24.store_bits(), 24);
        assert_eq!(SampleFormat::I24.valid_bits(), 24);
        assert_eq!(SampleFormat::I24.bytes(), 3);
        assert_eq!(SampleFormat::I16.bytes(), 2);
        assert_eq!(
            FormatSpec {
                format: SampleFormat::I24,
                sample_rate: 48_000,
                channels: 2
            }
            .block_align(),
            6
        );
    }

    #[test]
    fn candidates_prefer_f32_at_the_requested_channel_count() {
        let c = format_candidates(48_000, 2, Some(8));
        assert_eq!(c.len(), 10);
        assert_eq!(c[0].format, SampleFormat::F32);
        assert_eq!(c[0].channels, 2);
        assert_eq!(c[0].sample_rate, 48_000);
        // Every layout at the requested count before any at the native count.
        assert!(c[..5].iter().all(|s| s.channels == 2));
        assert!(c[5..].iter().all(|s| s.channels == 8));
        assert_eq!(c[5].format, SampleFormat::F32);
        // Sixteen-bit is the last resort in both groups.
        assert_eq!(c[4].format, SampleFormat::I16);
        assert_eq!(c[9].format, SampleFormat::I16);
    }

    #[test]
    fn candidates_skip_redundant_native_channel_count() {
        assert_eq!(format_candidates(48_000, 2, Some(2)).len(), 5);
        assert_eq!(format_candidates(48_000, 2, None).len(), 5);
        assert_eq!(format_candidates(48_000, 2, Some(0)).len(), 5);
    }

    #[test]
    fn candidates_carry_the_requested_rate() {
        assert!(
            format_candidates(96_000, 1, None)
                .iter()
                .all(|s| s.sample_rate == 96_000 && s.channels == 1)
        );
    }

    #[test]
    fn map_frames_fans_out_mono_and_drops_extra_channels() {
        let mono = [1.0, 2.0];
        let mut stereo = [0.0; 4];
        map_frames(&mono, 1, &mut stereo, 2);
        assert_eq!(stereo, [1.0, 1.0, 2.0, 2.0]);

        let quad = [1.0, 2.0, 3.0, 4.0];
        let mut down = [0.0; 2];
        map_frames(&quad, 4, &mut down, 2);
        assert_eq!(down, [1.0, 2.0]);
    }

    #[test]
    fn map_frames_ignores_partial_frames() {
        let src = [1.0, 2.0, 3.0];
        let mut dst = [9.0; 2];
        map_frames(&src, 2, &mut dst, 2);
        assert_eq!(dst, [1.0, 2.0]);
    }

    fn round_trip(format: SampleFormat, samples: &[f32]) -> Vec<f32> {
        let mut bytes = vec![0u8; samples.len() * format.bytes()];
        encode_from_f32(samples, format, &mut bytes);
        let mut back = vec![0.0f32; samples.len()];
        decode_to_f32(&bytes, format, &mut back);
        back
    }

    #[test]
    fn every_format_round_trips_within_its_resolution() {
        let samples = [0.0, 0.5, -0.5, 0.25, -1.0, 0.999, -0.001];
        for (format, tolerance) in [
            (SampleFormat::F32, 0.0),
            (SampleFormat::I32, 1e-6),
            (SampleFormat::I24In32, 1e-6),
            (SampleFormat::I24, 1e-6),
            (SampleFormat::I16, 1e-4),
        ] {
            let back = round_trip(format, &samples);
            for (got, want) in back.iter().zip(samples.iter()) {
                assert!(
                    (got - want).abs() <= tolerance,
                    "{format:?}: {got} vs {want}"
                );
            }
        }
    }

    #[test]
    fn full_scale_positive_never_wraps_to_negative() {
        for format in [
            SampleFormat::I32,
            SampleFormat::I24In32,
            SampleFormat::I24,
            SampleFormat::I16,
        ] {
            let back = round_trip(format, &[1.0, 2.0, f32::INFINITY]);
            for got in back {
                assert!(got > 0.99, "{format:?} clipped to {got}");
                assert!(got <= 1.0, "{format:?} exceeded full scale: {got}");
            }
        }
    }

    #[test]
    fn out_of_range_negative_clamps_to_minus_one() {
        for format in [
            SampleFormat::I32,
            SampleFormat::I24In32,
            SampleFormat::I24,
            SampleFormat::I16,
        ] {
            let back = round_trip(format, &[-1.0, -3.0, f32::NEG_INFINITY]);
            for got in back {
                assert!((got + 1.0).abs() < 1e-6, "{format:?} gave {got}");
            }
        }
    }

    #[test]
    fn nan_encodes_as_silence_not_a_click() {
        for format in [
            SampleFormat::I32,
            SampleFormat::I24In32,
            SampleFormat::I24,
            SampleFormat::I16,
        ] {
            let back = round_trip(format, &[f32::NAN]);
            assert_eq!(back[0], 0.0, "{format:?} turned NaN into {}", back[0]);
        }
    }

    #[test]
    fn i24_in_32_leaves_the_low_byte_clear() {
        let mut bytes = [0xFFu8; 4];
        encode_from_f32(&[0.3333], SampleFormat::I24In32, &mut bytes);
        assert_eq!(bytes[0], 0, "low byte must be zeroed for 24-in-32");
    }

    #[test]
    fn known_byte_patterns_decode_as_expected() {
        let mut out = [0.0f32; 2];
        // i16: -32768 and 32767.
        decode_to_f32(&[0x00, 0x80, 0xFF, 0x7F], SampleFormat::I16, &mut out);
        assert_eq!(out[0], -1.0);
        assert!((out[1] - 1.0).abs() < 1e-4);

        // Packed i24: 0x800000 (min) and 0x7FFFFF (max), little-endian.
        let mut out = [0.0f32; 2];
        decode_to_f32(
            &[0x00, 0x00, 0x80, 0xFF, 0xFF, 0x7F],
            SampleFormat::I24,
            &mut out,
        );
        assert_eq!(out[0], -1.0);
        assert!((out[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn conversion_tolerates_short_or_long_buffers() {
        // Source shorter than the destination: surplus is untouched.
        let mut dst = [7.0f32; 4];
        decode_to_f32(&[0, 0, 0, 0], SampleFormat::F32, &mut dst);
        assert_eq!(dst, [0.0, 7.0, 7.0, 7.0]);

        // Destination shorter than the source: the tail is ignored.
        let mut dst = [7.0f32; 1];
        decode_to_f32(&[0, 0, 0, 0, 0, 0, 0, 0], SampleFormat::F32, &mut dst);
        assert_eq!(dst, [0.0]);

        // Trailing partial sample in the source is ignored, not panicked on.
        let mut dst = [7.0f32; 2];
        decode_to_f32(&[0, 0, 0, 0, 1, 2], SampleFormat::F32, &mut dst);
        assert_eq!(dst, [0.0, 7.0]);

        let mut bytes = [9u8; 6];
        encode_from_f32(&[0.0], SampleFormat::F32, &mut bytes);
        assert_eq!(bytes, [0, 0, 0, 0, 9, 9]);
    }
}
