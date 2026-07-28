//! `jamstream join --headless`: a real UDP client around ClientCore. Reads
//! capture audio from a WAV, writes the received stereo mix to a WAV, and
//! prints session events as plain lines.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use jamstream_protocol::control::MAX_DATAGRAM_BYTES;
use jamstream_protocol::control::{DestinationState, RecordingState};
use jamstream_protocol::ids::{HOST_MEMBER_ID, TokenId};
use jamstream_protocol::invite::Invite;
use jamstream_session::client::{ClientCore, ClientEvent, ClientState, ServerCandidates};
use tokio::time::MissedTickBehavior;

use crate::CliError;
use crate::cli::JoinArgs;

pub const SAMPLE_RATE: u32 = 48_000;
const TICK: Duration = Duration::from_micros(2_500);
const FRAME_MONO: usize = 120;
const FRAME_STEREO: usize = 240;
/// How long a lone client waits before sending --chat into an empty room.
const CHAT_ALONE_DELAY: Duration = Duration::from_secs(1);
/// Longest invite text read from a file or a pipe. A real invite is a couple
/// of hundred characters; the cap is what stops a mistyped path (a log, a
/// device) from being pulled into memory whole.
const MAX_INVITE_TEXT_BYTES: u64 = 4 * 1024;

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
    let source = invite_source(args.invite.as_deref(), args.invite_file.as_deref());
    let invite = Invite::decode(&load_invite(source, "invite-file", out)?)?;
    let revoke = revoke_plan(args, &invite, out)?;
    let mut source = WavSource::load(&args.input)?;

    // The invite offers candidates, not one address: a locally hosted
    // session names loopback as well as the LAN address so a join from the
    // hosting machine never leaves it. A timeout below moves to the next.
    let mut candidates = ServerCandidates::new(&invite)?;
    let mut socket = crate::host::connected_socket(candidates.current()).await?;

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
    let mut buf = [0u8; MAX_DATAGRAM_BYTES];
    let mut joined_at: Option<Instant> = None;
    let mut roster_size = 1usize;
    let mut chat_sent = false;
    let mut revoke_sent = false;

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
                if let Some((jti, after)) = &revoke
                    && !revoke_sent
                    && t0.elapsed() >= *after
                {
                    core.revoke(*jti)?;
                    revoke_sent = true;
                    writeln!(out, "sent revoke after {} s", after.as_secs())?;
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
            // Before joining, a timeout is a statement about one address,
            // not about the session: try the next one the invite offers
            // with a fresh handshake. Once joined it is a real loss of the
            // server, and the session lives on the address that admitted
            // us.
            ClientState::TimedOut if joined_at.is_none() && candidates.has_alternatives() => {
                let next = candidates.advance();
                writeln!(out, "no answer, trying {next}")?;
                socket = crate::host::connected_socket(next).await?;
                let init = core.reconnect(now())?;
                socket.send(&init).await?;
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

/// Where an invite is read from.
///
/// The invite is the session's only credential, and argv is world readable:
/// `/proc/<pid>/cmdline` is mode 0444 on Linux, so any local user can lift a
/// bandmate's seat out of `ps auxww` while their client runs, and the string
/// stays in shell history for good. The file and pipe forms are the ones to
/// use; the positional form stays, with a warning, because it is in every
/// existing script.
#[derive(Debug, PartialEq)]
enum InviteSource<'a> {
    Argv(&'a str),
    File(&'a Path),
    Stdin,
}

/// Picks the source from one flag pair. clap already forbids passing both
/// forms, so the file wins if one somehow arrives anyway.
fn invite_source<'a>(argv: Option<&'a str>, file: Option<&'a Path>) -> InviteSource<'a> {
    match (argv, file) {
        (_, Some(path)) if path.as_os_str() == "-" => InviteSource::Stdin,
        (_, Some(path)) => InviteSource::File(path),
        (Some(text), None) => InviteSource::Argv(text),
        (None, None) => InviteSource::Stdin,
    }
}

/// Reads the invite text a source names, warning on the argv form. `flag`
/// names the file flag to suggest instead.
fn load_invite<W: Write>(
    source: InviteSource<'_>,
    flag: &str,
    out: &mut W,
) -> Result<String, CliError> {
    match source {
        InviteSource::Argv(text) => {
            writeln!(
                out,
                "warning: an invite on the command line is readable by every local \
                 process and stays in shell history; pass --{flag} <PATH> instead"
            )?;
            Ok(text.to_owned())
        }
        InviteSource::File(path) => {
            let file = std::fs::File::open(path).map_err(|e| {
                CliError::Usage(format!("cannot read invite from {}: {e}", path.display()))
            })?;
            read_invite_line(std::io::BufReader::new(file), &path.display().to_string())
        }
        InviteSource::Stdin => {
            let stdin = std::io::stdin();
            // A terminal would just look hung, and the person at it has no
            // way to know what is being waited for.
            if stdin.is_terminal() {
                return Err(CliError::Usage(format!(
                    "no invite given: pass --{flag} <PATH>, or pipe the invite in on stdin"
                )));
            }
            read_invite_line(stdin.lock(), "stdin")
        }
    }
}

/// One line, at most [`MAX_INVITE_TEXT_BYTES`] of it. Bounded before the read
/// rather than after: the path is whatever the caller typed, and a line-free
/// file would otherwise be read into memory whole.
fn read_invite_line<R: BufRead>(reader: R, from: &str) -> Result<String, CliError> {
    let mut line = String::new();
    let read = reader
        .take(MAX_INVITE_TEXT_BYTES + 1)
        .read_line(&mut line)
        .map_err(|e| CliError::Usage(format!("cannot read invite from {from}: {e}")))?;
    if read as u64 > MAX_INVITE_TEXT_BYTES {
        return Err(CliError::Usage(format!(
            "invite from {from} is longer than {MAX_INVITE_TEXT_BYTES} bytes"
        )));
    }
    let line = line.trim();
    if line.is_empty() {
        return Err(CliError::Usage(format!("no invite found in {from}")));
    }
    Ok(line.to_owned())
}

/// Validates the hidden revocation test hook. The wire protocol revokes by
/// token id, so the flag takes the target's full invite (which the host, who
/// minted every invite, has at hand) and extracts the jti from it. Only the
/// host invite may carry the flags: the server treats revoke-by-non-host as
/// a protocol violation, so refusing early gives a readable error instead.
fn revoke_plan<W: Write>(
    args: &JoinArgs,
    own: &Invite,
    out: &mut W,
) -> Result<Option<(TokenId, Duration)>, CliError> {
    let named = args.revoke_invite.is_some() || args.revoke_invite_file.is_some();
    match (named, args.revoke_after_secs) {
        (false, None) => Ok(None),
        (true, Some(secs)) => {
            if own.token.member_id != HOST_MEMBER_ID {
                return Err(CliError::Usage(
                    "--revoke-invite is only usable with the host invite".to_owned(),
                ));
            }
            let source = invite_source(
                args.revoke_invite.as_deref(),
                args.revoke_invite_file.as_deref(),
            );
            let target = Invite::decode(&load_invite(source, "revoke-invite-file", out)?)?;
            Ok(Some((target.token.jti, Duration::from_secs(secs))))
        }
        _ => Err(CliError::Usage(
            "--revoke-invite and --revoke-after-secs must be passed together".to_owned(),
        )),
    }
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
        ClientEvent::StreamStatus(destinations) => {
            let live = destinations
                .iter()
                .filter(|d| d.state == DestinationState::Live)
                .count();
            writeln!(out, "stream: {live} of {} live", destinations.len())
        }
        ClientEvent::RecordStatus { state, stems } => {
            let what = match state {
                RecordingState::Idle => "idle".to_owned(),
                RecordingState::Recording if *stems => "recording (mix and stems)".to_owned(),
                RecordingState::Recording => "recording".to_owned(),
                RecordingState::Uploading => "uploading".to_owned(),
                RecordingState::Failed { reason } => format!("failed: {reason}"),
            };
            writeln!(out, "record: {what}")
        }
        // Once a second; too chatty for line output.
        ClientEvent::RttSample { .. } => Ok(()),
        // Mixer mirroring and avatars are UI concerns; line output ignores them.
        ClientEvent::BroadcastMixChanged { .. } | ClientEvent::AvatarReady { .. } => Ok(()),
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
    fn revoke_plan_is_host_only_and_paired() {
        use jamstream_protocol::ids::{MemberId, Role, SessionId};
        use jamstream_protocol::invite::{Issuer, Token};

        let issuer = Issuer::generate();
        let session_id = SessionId::generate();
        let mint = |member: u16| {
            issuer.mint(
                session_id,
                vec!["127.0.0.1:43210".parse().unwrap()],
                [7u8; 32],
                Token {
                    member_id: MemberId(member),
                    role: Role::Musician,
                    name_hint: None,
                    expires_unix: u64::MAX,
                    jti: jamstream_protocol::ids::TokenId::generate(),
                },
            )
        };
        let host = mint(0);
        let target = mint(2);
        let args = |revoke_invite: Option<String>, revoke_after_secs: Option<u64>| JoinArgs {
            invite: Some(host.encode()),
            invite_file: None,
            headless: true,
            input: PathBuf::from("in.wav"),
            output: PathBuf::from("out.wav"),
            duration_secs: 1,
            chat: None,
            name: None,
            revoke_invite,
            revoke_invite_file: None,
            revoke_after_secs,
        };

        let mut out = Vec::new();
        assert!(
            revoke_plan(&args(None, None), &host, &mut out)
                .unwrap()
                .is_none()
        );
        let (jti, after) = revoke_plan(&args(Some(target.encode()), Some(2)), &host, &mut out)
            .unwrap()
            .expect("a plan");
        assert_eq!(jti, target.token.jti);
        assert_eq!(after, Duration::from_secs(2));

        // Half a pair is a usage error even when clap is bypassed.
        assert!(matches!(
            revoke_plan(&args(Some(target.encode()), None), &host, &mut out),
            Err(CliError::Usage(_))
        ));
        // A non-host invite may not carry the hook.
        assert!(matches!(
            revoke_plan(&args(Some(host.encode()), Some(1)), &target, &mut out),
            Err(CliError::Usage(msg)) if msg.contains("host invite")
        ));

        // The file form reaches the same plan, and warns about nothing.
        let path = temp_wav("revoke-invite.txt");
        std::fs::write(&path, format!("{}\n", target.encode())).unwrap();
        let mut file_args = args(None, Some(2));
        file_args.revoke_invite_file = Some(path.clone());
        let mut quiet = Vec::new();
        let (jti, _) = revoke_plan(&file_args, &host, &mut quiet)
            .unwrap()
            .expect("a plan");
        assert_eq!(jti, target.token.jti);
        assert!(quiet.is_empty(), "the file form must not warn");
        std::fs::remove_file(&path).unwrap();
    }

    /// The argv form is the defect: `/proc/<pid>/cmdline` is world readable,
    /// so a bandmate's seat is one `ps auxww` away while their client runs.
    /// It still works, because every existing script passes it, but it says
    /// so on the way past.
    #[test]
    fn the_command_line_form_warns_and_the_file_form_does_not() {
        let mut out = Vec::new();
        let text = load_invite(
            InviteSource::Argv("jamstream://join/blob"),
            "invite-file",
            &mut out,
        )
        .unwrap();
        assert_eq!(text, "jamstream://join/blob");
        let warning = String::from_utf8(out).unwrap();
        assert!(
            warning.contains("readable by every local process"),
            "{warning}"
        );
        assert!(warning.contains("--invite-file"), "{warning}");

        let path = temp_wav("invite.txt");
        // Trailing newline and surrounding blanks are what a file written by
        // `jamstream host --json | jq -r` actually contains.
        std::fs::write(&path, "  jamstream://join/blob \n").unwrap();
        let mut quiet = Vec::new();
        assert_eq!(
            load_invite(InviteSource::File(&path), "invite-file", &mut quiet).unwrap(),
            "jamstream://join/blob"
        );
        assert!(quiet.is_empty());
        std::fs::remove_file(&path).unwrap();
    }

    /// `-` means stdin, a path means that path, and nothing named means
    /// stdin as well, so `... | jamstream join --headless` works with no
    /// credential anywhere in argv.
    #[test]
    fn the_invite_source_is_the_file_then_the_pipe_then_argv() {
        let dash = PathBuf::from("-");
        let path = PathBuf::from("/tmp/invite");
        assert_eq!(
            invite_source(None, Some(dash.as_path())),
            InviteSource::Stdin
        );
        assert_eq!(
            invite_source(Some("blob"), Some(path.as_path())),
            InviteSource::File(path.as_path())
        );
        assert_eq!(
            invite_source(Some("blob"), None),
            InviteSource::Argv("blob")
        );
        assert_eq!(invite_source(None, None), InviteSource::Stdin);
    }

    /// An invite is a couple of hundred characters. A file with no newline in
    /// it at all is a mistyped path, and reading it whole is how a mistyped
    /// path becomes an out-of-memory kill.
    #[test]
    fn an_oversized_invite_file_is_refused_without_being_read_whole() {
        let path = temp_wav("huge-invite.txt");
        let huge = "A".repeat(MAX_INVITE_TEXT_BYTES as usize + 1);
        std::fs::write(&path, &huge).unwrap();
        let mut out = Vec::new();
        assert!(matches!(
            load_invite(InviteSource::File(&path), "invite-file", &mut out),
            Err(CliError::Usage(msg)) if msg.contains("longer than")
        ));
        // Exactly at the cap still reads.
        std::fs::write(&path, "B".repeat(MAX_INVITE_TEXT_BYTES as usize)).unwrap();
        assert_eq!(
            load_invite(InviteSource::File(&path), "invite-file", &mut out)
                .unwrap()
                .len(),
            MAX_INVITE_TEXT_BYTES as usize
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn an_empty_or_missing_invite_file_is_a_usage_error() {
        let path = temp_wav("empty-invite.txt");
        std::fs::write(&path, "\n\n").unwrap();
        let mut out = Vec::new();
        assert!(matches!(
            load_invite(InviteSource::File(&path), "invite-file", &mut out),
            Err(CliError::Usage(msg)) if msg.contains("no invite found")
        ));
        std::fs::remove_file(&path).unwrap();

        let missing = temp_wav("not-here-invite.txt");
        assert!(matches!(
            load_invite(InviteSource::File(&missing), "invite-file", &mut out),
            Err(CliError::Usage(msg)) if msg.contains("cannot read invite")
        ));
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
