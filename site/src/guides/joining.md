# Joining a session

You need exactly one thing: your invite, a string starting with `jamstream://join/`, sent to you by the host.

## What an invite is

Each invite admits one person to one seat in one session. It carries, signed by the host's key: the server's address, the session id, your seat and role (musician or listener), and an expiry matching the session's hard cap. The server accepts only invites signed by the host who launched it, so an invite cannot be forged, and a stolen one can be revoked without ending the session.

Practical consequences:

- Do not share your invite; two people cannot use the same one. If someone else needs in, the host has spare invites or hosts again with more seats.
- Losing your connection does not burn the invite. Close the app, move to another machine, rejoin with the same string; the seat is yours until the session ends or the host revokes it.
- Invites die with the session. There is nothing to clean up or keep secret afterward.
- Send invites over a channel you trust. Anyone holding your invite can join as you until it is revoked.

## Joining from the app

Paste the invite into the field on the home screen (hint text: "paste an invite, jamstream://join/...") and click Join, or press Enter. A malformed or expired invite shows the reason under the field instead of joining.

Once connected you are in the session screen:

![Session screen with four mixer strips, chat on the right, and a status bar showing 7.9 ms mouth to ear](../images/session_demo.png)
*A four piece session in the current build. One strip per musician; chat on the right; latency, buffer, and loss in the status bar.*

What you are looking at:

- One mixer strip per musician: a status dot, name, fader, pan, dB readout, and a Mute button. This is your personal monitor mix; moving Ana's fader changes what you hear, not what anyone else hears.
- Your own strip is dimmed with a "you" tag. Self monitoring is local, on your interface, not through the server, so your own channel has no fader in the mix.
- The host additionally sees a Revoke button on every other strip. Revoking ejects that member and kills their invite, with a confirmation step.
- Chat, with timestamps. The metronome panel shows tempo, beats per bar, and click state; the host sets them, and "hear the click" is your own choice.
- The status bar, in the same place every session: mouth to ear latency in ms as the headline number, then round trip, jitter buffer depth, packet loss, and input and output meters. Hosts also see elapsed time and cost so far. Leave is on the right, with a confirmation.

A member who stops responding grays out after 10 seconds and their fader freezes; when they reconnect with their invite, they come back in the same seat.

At ten musicians the strips extend past the window and scroll horizontally:

![Session screen at capacity, ten mixer strips with a horizontal scrollbar and ten listeners named below](../images/session_full.png)
*A session at the 10 musician cap in the current build, with 10 listeners connected.*

## Joining without the app

For test rigs and machines without a display, the CLI joins headlessly with a WAV file as its instrument and writes what it heard:

```console
$ jamstream join 'jamstream://join/...' --headless \
    --input take.wav --output mix.wav --duration-secs 120 \
    --chat "bot checking in"
joined
roster: 3 members
chat from 1: heard you
left after 120 s; wrote mix.wav
```

Input must be a 48 kHz WAV, mono or stereo; after the file ends, the client sends silence. The received stereo mix lands in `--output`. Flags and details in the [CLI reference](../cli/join.md).
