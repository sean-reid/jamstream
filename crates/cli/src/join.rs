//! `jamstream join --headless`: a real UDP client around ClientCore. Reads
//! capture audio from a WAV, writes the received stereo mix to a WAV, and
//! prints session events as plain lines.

use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use jamstream_protocol::invite::Invite;
use jamstream_session::client::{ClientCore, ClientEvent, ClientState};
use tokio::net::UdpSocket;
use tokio::time::MissedTickBehavior;

use crate::CliError;
use crate::cli::JoinArgs;

pub const SAMPLE_RATE: u32 = 48_000;
const TICK: Duration = Duration::from_micros(2_500);
const FRAME_MONO: usize = 120;
const FRAME_STEREO: usize = 240;
/// How long a lone client waits before sending --chat into an empty room.
const CHAT_ALONE_DELAY: Duration = Duration::from_secs(1);

/// Mono 48 kHz capture feed from a WAV file; silence after EOF.
pub struct WavSource {
    samples: Vec<f32>,
    pos: usize,
}

impl WavSource {
    pub fn load(path: &Path) -> Result<Self, CliError> {
        let mut reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        if spec.sample_rate != SAMPLE_RATE {
            return Err(CliError::Usage(format!(
                "input wav must be 48 kHz, {} has {} Hz",
                path.display(),
                spec.sample_rate
            )));
        }
        if !(1..=2).contains(&spec.channels) {
            return Err(CliError::Usage(format!(
                "input wav must be mono or stereo, {} has {} channels",
                path.display(),
                spec.channels
            )));
        }
        let raw: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
            hound::SampleFormat::Int => {
                let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.map(|v| v as f32 * scale))
                    .collect::<Result<_, _>>()?
            }
        };
        let samples = if spec.channels == 2 {
            raw.chunks_exact(2)
                .map(|lr| (lr[0] + lr[1]) * 0.5)
                .collect()
        } else {
            raw
        };
        Ok(WavSource { samples, pos: 0 })
    }

    /// Fills one capture frame, zero-padding past end of file.
    pub fn next_frame(&mut self, buf: &mut [f32]) {
        for slot in buf.iter_mut() {
            *slot = self.samples.get(self.pos).copied().unwrap_or(0.0);
            self.pos = self.pos.saturating_add(1);
        }
    }
}

/// Interleaved stereo f32 to a 16-bit WAV.
pub fn write_stereo_wav(path: &Path, samples: &[f32]) -> Result<(), CliError> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        writer.write_sample((s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}

pub async fn run<W: Write>(args: &JoinArgs, out: &mut W) -> Result<(), CliError> {
    if !args.headless {
        return Err(CliError::Usage(
            "only headless mode is implemented here; pass --headless, or use the desktop \
             app for an interactive session"
                .to_owned(),
        ));
    }
    let invite = Invite::decode(&args.invite)?;
    let mut source = WavSource::load(&args.input)?;

    let server = invite.addresses[0];
    let bind: SocketAddr = if server.is_ipv4() {
        "0.0.0.0:0".parse().expect("static addr")
    } else {
        "[::]:0".parse().expect("static addr")
    };
    let socket = UdpSocket::bind(bind).await?;
    socket.connect(server).await?;

    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let (mut core, init) = ClientCore::connect(&invite, now())?;
    socket.send(&init).await?;

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Burst);
    let duration = Duration::from_secs(args.duration_secs);
    let mut capture = [0.0f32; FRAME_MONO];
    let mut playout = [0.0f32; FRAME_STEREO];
    let mut received: Vec<f32> = Vec::new();
    let mut buf = [0u8; 2048];
    let mut joined_at: Option<Instant> = None;
    let mut roster_size = 1usize;
    let mut chat_sent = false;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let t = now();
                if joined_at.is_some() {
                    source.next_frame(&mut capture);
                    for pkt in core.push_capture(t, &capture) {
                        socket.send(&pkt).await?;
                    }
                    core.pull_playout(&mut playout);
                    received.extend_from_slice(&playout);
                }
                for pkt in core.poll(t) {
                    socket.send(&pkt).await?;
                }
            }
            result = socket.recv(&mut buf) => {
                if let Ok(len) = result {
                    for pkt in core.handle_datagram(now(), &buf[..len]) {
                        socket.send(&pkt).await?;
                    }
                }
            }
        }

        for event in core.events() {
            if let ClientEvent::Roster(members) = &event {
                roster_size = members.len();
            }
            print_event(out, &event)?;
        }

        match core.state().clone() {
            ClientState::Joined => {
                let t0 = *joined_at.get_or_insert_with(Instant::now);
                if let Some(msg) = &args.chat
                    && !chat_sent
                    // Hold the message until somebody else is in the room
                    // (or briefly, when nobody shows), so it is not relayed
                    // into an empty session and lost.
                    && (roster_size > 1 || t0.elapsed() >= CHAT_ALONE_DELAY)
                {
                    core.send_chat(msg)?;
                    chat_sent = true;
                }
            }
            ClientState::Rejected { ours, theirs } => {
                write_stereo_wav(&args.output, &received)?;
                return Err(CliError::Failed(format!(
                    "rejected: this client speaks protocol {ours}, the server speaks {theirs}"
                )));
            }
            ClientState::Ejected { reason } => {
                write_stereo_wav(&args.output, &received)?;
                return Err(CliError::Failed(format!("ejected: {reason}")));
            }
            ClientState::TimedOut => {
                write_stereo_wav(&args.output, &received)?;
                return Err(CliError::Failed(
                    "timed out waiting for the server".to_owned(),
                ));
            }
            ClientState::Connecting => {}
        }

        if joined_at.is_some_and(|t0| t0.elapsed() >= duration) {
            break;
        }
    }

    core.leave("duration complete")?;
    for pkt in core.poll(now()) {
        socket.send(&pkt).await?;
    }
    write_stereo_wav(&args.output, &received)?;
    writeln!(
        out,
        "left after {} s; wrote {}",
        args.duration_secs,
        args.output.display()
    )?;
    Ok(())
}

fn print_event<W: Write>(out: &mut W, event: &ClientEvent) -> std::io::Result<()> {
    match event {
        ClientEvent::Joined => writeln!(out, "joined"),
        ClientEvent::Roster(members) => writeln!(out, "roster: {} members", members.len()),
        ClientEvent::Chat { from, text } => writeln!(out, "chat from {}: {}", from.0, text),
        ClientEvent::MetronomeChanged {
            bpm,
            beats_per_bar,
            enabled,
        } => writeln!(
            out,
            "metronome: {bpm} bpm, {beats_per_bar} beats per bar, {}",
            if *enabled { "on" } else { "off" }
        ),
        ClientEvent::Ejected { reason } => writeln!(out, "ejected: {reason}"),
        ClientEvent::Rejected { ours, theirs } => writeln!(
            out,
            "rejected: this client speaks protocol {ours}, the server speaks {theirs}"
        ),
        ClientEvent::TimedOut => writeln!(out, "timed out"),
        // Once a second; too chatty for line output.
        ClientEvent::RttSample { .. } => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_wav(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("jamstream-cli-join-{}-{name}", std::process::id()))
    }

    fn write_wav(path: &Path, channels: u16, samples: &[i16]) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &s in samples {
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn source_downmixes_stereo_and_pads_silence() {
        let path = temp_wav("stereo.wav");
        // Two stereo frames: (16384, -16384) averages to 0, (8192, 8192)
        // averages to 8192.
        write_wav(&path, 2, &[16384, -16384, 8192, 8192]);
        let mut source = WavSource::load(&path).unwrap();
        let mut frame = [1.0f32; FRAME_MONO];
        source.next_frame(&mut frame);
        assert!(frame[0].abs() < 1e-4);
        assert!((frame[1] - 0.25).abs() < 1e-3);
        // Past EOF: silence, not a loop.
        assert!(frame[2..].iter().all(|&s| s == 0.0));
        let mut second = [1.0f32; FRAME_MONO];
        source.next_frame(&mut second);
        assert!(second.iter().all(|&s| s == 0.0));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn source_rejects_wrong_rate() {
        let path = temp_wav("rate.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        w.write_sample(0i16).unwrap();
        w.finalize().unwrap();
        assert!(matches!(
            WavSource::load(&path),
            Err(CliError::Usage(msg)) if msg.contains("48 kHz")
        ));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn output_wav_round_trips() {
        let path = temp_wav("out.wav");
        let samples = vec![0.5f32, -0.5, 0.0, 1.0];
        write_stereo_wav(&path, &samples).unwrap();
        let mut reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, SAMPLE_RATE);
        let read: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
        assert_eq!(read.len(), 4);
        assert_eq!(read[3], i16::MAX);
        std::fs::remove_file(&path).unwrap();
    }
}
