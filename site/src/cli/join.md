# jamstream join

Join a session as a headless client.

```text
Usage: jamstream join [OPTIONS] --input <INPUT> --output <OUTPUT> --duration-secs <DURATION_SECS> <INVITE>
```

A real client without a screen: it joins with an invite, plays a WAV file as its capture signal, records the stereo mix it receives, and prints session events as plain lines. Built for test rigs and automation; people use the desktop app.

## Arguments and options

| Flag | Meaning |
|---|---|
| `<INVITE>` | Invite string, with or without the `jamstream://join/` prefix. |
| `--headless` | Run without a UI. Required; the desktop app is the interactive client. |
| `--input <INPUT>` | 48 kHz mono or stereo WAV sent as the capture signal. Silence after the file ends. |
| `--output <OUTPUT>` | Output WAV path for the received stereo mix. |
| `--duration-secs <DURATION_SECS>` | Seconds to stay in the session after joining. |
| `--chat <CHAT>` | Chat message to send once after joining. |
| `--name <NAME>` | Display name to request. Not sent yet; names come from the invite. |

## Example

Hold a seat for two minutes, contribute a pre-recorded take, and keep what came back:

```console
$ jamstream join 'jamstream://join/r6edH1LCtlT3vPPiILRRVAEACgAAAcrRAjiV...' \
    --headless --input take.wav --output mix.wav --duration-secs 120 \
    --chat "bot in the room"
joined
roster: 3 members
chat from 1: heard you
metronome: 112 bpm, 4 beats per bar, on
left after 120 s; wrote mix.wav
```

## Notes

- The input WAV must be 48 kHz; stereo files are downmixed to mono. Anything else is rejected with a message naming the problem.
- The output WAV is written even when the session ends badly (ejected, rejected, timed out), so a test run always leaves evidence.
- Chat lines, roster changes, metronome changes, and ejection reasons print one per line; latency samples are not printed.
- A version mismatch fails at the handshake with both versions named, never with silence.
