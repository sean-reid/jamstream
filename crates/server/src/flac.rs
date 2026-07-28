//! Streaming FLAC encode for session recording: stereo f32 in, FLAC bytes
//! out block by block, so a recording can leave the machine while it is
//! still being made. STREAMINFO is written up front with the total sample
//! count left unknown, which the format permits and a multipart upload
//! requires: the first bytes sent are final.

use std::io;

use flacenc::bitsink::{BitSink, ByteSink};
use flacenc::component::{BitRepr, StreamInfo};
use flacenc::error::{Verified, Verify};
use flacenc::source::{Fill, FrameBuf};

/// The mix clock's sample rate, which is the session's.
pub const SAMPLE_RATE: usize = jamstream_protocol::SAMPLE_RATE as usize;
/// The shape of every take. Public because the recording cost model lives in
/// another crate and estimates bytes from these three numbers; it once quoted
/// mono WAV for a stereo FLAC take, so a test pins them together.
pub const CHANNELS: usize = 2;
pub const BITS_PER_SAMPLE: usize = 16;
/// Samples per channel per FLAC frame: the format's customary block, ~85 ms.
pub const BLOCK_SAMPLES: usize = 4096;
/// Interleaved samples per FLAC frame.
pub const BLOCK_INTERLEAVED: usize = BLOCK_SAMPLES * CHANNELS;

/// Encodes one stereo signal to 16-bit FLAC, block by block.
pub struct FlacEncoder {
    config: Verified<flacenc::config::Encoder>,
    info: StreamInfo,
    fb: FrameBuf,
    /// Interleaved samples waiting for a full block.
    pending: Vec<i32>,
    frames: usize,
}

impl FlacEncoder {
    pub fn new() -> FlacEncoder {
        let config = flacenc::config::Encoder::default()
            .into_verified()
            .expect("default encoder config verifies");
        let mut info =
            StreamInfo::new(SAMPLE_RATE, CHANNELS, BITS_PER_SAMPLE).expect("constant parameters");
        info.set_block_sizes(BLOCK_SAMPLES, BLOCK_SAMPLES)
            .expect("constant block size");
        let fb = FrameBuf::with_size(CHANNELS, BLOCK_SAMPLES).expect("constant frame shape");
        FlacEncoder {
            config,
            info,
            fb,
            pending: Vec::with_capacity(BLOCK_SAMPLES * CHANNELS),
            frames: 0,
        }
    }

    /// An encoder whose stream already holds `frames` written blocks and
    /// `pending` interleaved samples of silence not yet in a block. Frames
    /// carry no state beyond their number here, so a silent head can be
    /// copied in from elsewhere and continued from.
    pub fn resume_silent(frames: usize, pending: usize) -> FlacEncoder {
        debug_assert!(pending < BLOCK_INTERLEAVED, "pending is under one block");
        let mut enc = FlacEncoder::new();
        enc.frames = frames;
        enc.pending.resize(pending, 0);
        enc
    }

    /// The stream header: `fLaC`, then STREAMINFO with total samples and MD5
    /// unknown. Write it before any bytes from [`FlacEncoder::push`].
    pub fn header(&self) -> io::Result<Vec<u8>> {
        let mut sink = ByteSink::new();
        sink.write_bytes_aligned(b"fLaC").map_err(io_err)?;
        // Metadata block header: last-block flag, type 0, 34-byte body.
        sink.write_bytes_aligned(&[0x80, 0, 0, 34])
            .map_err(io_err)?;
        self.info.write(&mut sink).map_err(io_err)?;
        Ok(sink.into_inner())
    }

    /// Feeds interleaved stereo samples in [-1, 1], appending the bytes of
    /// every block they complete to `out`.
    pub fn push(&mut self, interleaved: &[f32], out: &mut Vec<u8>) -> io::Result<()> {
        debug_assert_eq!(interleaved.len() % CHANNELS, 0);
        self.pending.extend(interleaved.iter().map(|&s| to_i16(s)));
        while self.pending.len() >= BLOCK_SAMPLES * CHANNELS {
            let rest = self.pending.split_off(BLOCK_SAMPLES * CHANNELS);
            let block = std::mem::replace(&mut self.pending, rest);
            self.encode_block(&block, out)?;
        }
        Ok(())
    }

    /// Encodes whatever tail is pending as a final short frame. The encoder
    /// is spent afterwards.
    pub fn finish(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        let tail = std::mem::take(&mut self.pending);
        if !tail.is_empty() {
            self.encode_block(&tail, out)?;
        }
        Ok(())
    }

    fn encode_block(&mut self, interleaved: &[i32], out: &mut Vec<u8>) -> io::Result<()> {
        self.fb.fill_interleaved(interleaved).map_err(io_err)?;
        let frame =
            flacenc::encode_fixed_size_frame(&self.config, &self.fb, self.frames, &self.info)
                .map_err(io_err)?;
        self.frames += 1;
        let mut sink = ByteSink::new();
        frame.write(&mut sink).map_err(io_err)?;
        out.extend_from_slice(sink.as_slice());
        Ok(())
    }
}

impl Default for FlacEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// The 16-bit conversion every recorded sample goes through.
pub fn to_i16(sample: f32) -> i32 {
    i32::from((sample.clamp(-1.0, 1.0) * 32767.0).round() as i16)
}

fn io_err<E: std::fmt::Display>(err: E) -> io::Error {
    io::Error::other(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes through the streaming path in tick-sized pushes and decodes
    /// with claxon. The decoder is a different implementation on purpose: an
    /// encoder bug mirrored by its own decoder would pass forever.
    fn round_trip(signal: &[f32]) -> (claxon::metadata::StreamInfo, Vec<i32>) {
        let mut enc = FlacEncoder::new();
        let mut bytes = enc.header().unwrap();
        for tick in signal.chunks(240) {
            enc.push(tick, &mut bytes).unwrap();
        }
        enc.finish(&mut bytes).unwrap();
        let mut reader = claxon::FlacReader::new(std::io::Cursor::new(bytes)).unwrap();
        let info = reader.streaminfo();
        let decoded: Vec<i32> = reader.samples().map(|s| s.unwrap()).collect();
        (info, decoded)
    }

    /// Busy, full-scale-ish material: every subframe type gets exercised and
    /// nothing collapses to a constant the encoder stores for free.
    fn band_signal(samples: usize) -> Vec<f32> {
        let mut signal = vec![0.0f32; samples * 2];
        let mut noise = 0x2545_F491u32;
        for (i, pair) in signal.chunks_exact_mut(2).enumerate() {
            let t = i as f32 / 48_000.0;
            let tone = (std::f32::consts::TAU * 220.0 * t).sin() * 0.5
                + (std::f32::consts::TAU * 331.0 * t).sin() * 0.3;
            noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let hiss = (noise >> 16) as f32 / 65_535.0 - 0.5;
            pair[0] = tone + hiss * 0.05;
            pair[1] = tone * 0.8 - hiss * 0.05;
        }
        signal
    }

    #[test]
    fn flac_round_trip_is_lossless() {
        // Deliberately not a multiple of the block size or the tick: the tail
        // goes out as a short final frame.
        let signal = band_signal(10_000);
        let (info, decoded) = round_trip(&signal);
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 2);
        assert_eq!(info.bits_per_sample, 16);
        let expected: Vec<i32> = signal.iter().map(|&s| to_i16(s)).collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn clipping_input_is_clamped_not_wrapped() {
        let signal = vec![1.7f32, -1.7, 1.0, -1.0];
        let (_, decoded) = round_trip(&signal);
        assert_eq!(decoded, vec![32_767, -32_767, 32_767, -32_767]);
    }

    #[test]
    fn header_leaves_the_total_sample_count_unknown() {
        // The header goes to the sink before the length is knowable, so a
        // header claiming a length would be wrong in every finished file.
        let (info, decoded) = round_trip(&band_signal(5_000));
        assert_eq!(info.samples, None);
        assert_eq!(decoded.len(), 5_000 * 2);
    }

    #[test]
    fn an_empty_take_is_still_a_valid_stream() {
        let (info, decoded) = round_trip(&[]);
        assert_eq!(info.samples, None);
        assert!(decoded.is_empty());
    }
}
