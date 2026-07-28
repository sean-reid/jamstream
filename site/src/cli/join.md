# jamstream join

Join a session as a headless client.

```text
Usage: jamstream join [OPTIONS] --input <INPUT> --output <OUTPUT> --duration-secs <DURATION_SECS> [INVITE]
```

A real client without a screen: it joins with an invite, plays a WAV file as its capture signal, records the stereo mix it receives, and prints session events as plain lines. Built for test rigs and automation; people use the desktop app.

The invite is the seat. Pass it on stdin or in a file, never as an argument: process arguments are readable by every account on the machine, and the string stays in shell history.

## Arguments and options

| Flag | Meaning |
|---|---|
| `[INVITE]` | Invite string, with or without the `jamstream://join/` prefix. Deprecated: readable by any local user in the process list. Prints a warning. |
| `--invite-file <PATH>` | Read the invite from a file, one line, or `-` for stdin. With neither this nor the positional form, the invite is read from stdin. |
| `--headless` | Run without a UI. Required; the desktop app is the interactive client. |
| `--input <INPUT>` | 48 kHz mono or stereo WAV sent as the capture signal. Silence after the file ends. |
| `--output <OUTPUT>` | Output WAV path for the received stereo mix. |
| `--duration-secs <DURATION_SECS>` | Seconds to stay in the session after joining. |
| `--chat <CHAT>` | Chat message to send once after joining. |
| `--name <NAME>` | Display name to request. Not sent yet; names come from the invite. |

## Example

Hold a seat for two minutes, contribute a pre-recorded take, and keep what came back:

```console
$ jamstream join --invite-file seat.txt \
    --headless --input take.wav --output mix.wav --duration-secs 120 \
    --chat "bot in the room"
joined
roster: 3 members
chat from 1: heard you
metronome: 112 bpm, 4 beats per bar, on
left after 120 s; wrote mix.wav
```

Or with nothing on disk at all:

```console
$ pass show band/seat | jamstream join --headless \
    --input take.wav --output mix.wav --duration-secs 120
```

## Notes

- The invite file is read one line at a time and capped at 4 KiB; a trailing newline and surrounding blanks are ignored.
- The input WAV must be 48 kHz; stereo files are downmixed to mono. Anything else is rejected with a message naming the problem.
- The output WAV is written even when the session ends badly (ejected, rejected, timed out), so a test run always leaves evidence.
- Chat lines, roster changes, metronome changes, and ejection reasons print one per line; latency samples are not printed.
- The session's recorder prints as `record: idle`, `record: recording (mix and stems)`, or `record: failed: <reason>` on every transition, so a rig can assert that a take ran. See [Recording a session](../guides/recording.md).
- A version mismatch fails at the handshake with both versions named, never with silence.
- A session with no free seat for the invite's role prints `session full` and exits nonzero, instead of waiting out a connection timeout. The desktop app keeps retrying instead, since a seat frees when somebody leaves.
