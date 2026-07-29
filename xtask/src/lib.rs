//! The repository's deterministic audio fixture generator, as a library so
//! `cargo xtask fixtures` and the cli test suite call the same code.

use std::path::Path;

pub const RATE: u32 = 48_000;

/// Every fixture [`write_fixture`] knows how to make.
pub const FIXTURES: [&str; 4] = [
    "impulse-train-48k.wav",
    "sine-440-48k.wav",
    "sine-880-48k.wav",
    "silence-48k.wav",
];

/// Writes one deterministic mono 48 kHz 16-bit fixture, selected by file name.
/// Byte-identical on every run: nothing in the content depends on the clock.
pub fn write_fixture(name: &str, path: &Path) -> Result<(), hound::Error> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    type Writer = hound::WavWriter<std::io::BufWriter<std::fs::File>>;
    fn sine(writer: &mut Writer, hz: f32, secs: u32) -> Result<(), hound::Error> {
        for i in 0..(secs * RATE) {
            let t = i as f32 / RATE as f32;
            let sample = (t * hz * std::f32::consts::TAU).sin() * 0.5;
            writer.write_sample((sample * f32::from(i16::MAX)) as i16)?;
        }
        Ok(())
    }
    let mut writer = hound::WavWriter::create(path, spec)?;
    match name {
        "impulse-train-48k.wav" => {
            for i in 0..(10 * RATE) {
                let sample = if i % 4800 == 0 { i16::MAX } else { 0 };
                writer.write_sample(sample)?;
            }
        }
        "sine-440-48k.wav" => sine(&mut writer, 440.0, 5)?,
        "sine-880-48k.wav" => sine(&mut writer, 880.0, 5)?,
        "silence-48k.wav" => {
            for _ in 0..(2 * RATE) {
                writer.write_sample(0i16)?;
            }
        }
        other => panic!("unknown fixture {other:?}"),
    }
    writer.finalize()
}
