# Joining a session

You need exactly two things: the app, from the [Download](../download.md) page, and your invite, a string starting with `jamstream://join/`, sent to you by the host.

## What an invite is

Each invite admits one person to one seat in one session. It carries, signed by the host's key: the server's address, the session id, your seat and role (musician or listener), and an expiry matching the session's hard cap. The server accepts only invites signed by the host who launched it, so an invite cannot be forged, and a stolen one can be revoked without ending the session.

- Do not share your invite; two people cannot use the same one. If someone else needs in, the host mints another seat from the invites panel.
- Losing your connection does not burn the invite. Close the app, move to another machine, rejoin with the same string; the seat is yours until the session ends or the host revokes it.
- Invites die with the session. There is nothing to clean up or keep secret afterward.
- Send invites over a channel you trust. Anyone holding your invite can join as you until it is revoked.

## Joining

Paste the invite into the **Join a session** field on the home screen (hint text: "paste an invite, jamstream://join/...") and click Join, or press Enter. A malformed or expired invite shows the reason under the field instead of joining.

## The session screen

Once connected you are in the session screen:

![Session screen with four mixer strips, chat on the right, and a status bar showing 7.9 ms mouth to ear](../images/session_demo.png)
*A four piece session in the current build. One strip per musician; chat on the right; latency, buffer, and loss in the status bar.*

What you are looking at:

- One mixer strip per musician: an avatar disc, a status dot, name, fader, pan, dB readout, and a Mute button. This is your personal monitor mix; moving Ana's fader changes what you hear, not what anyone else hears.
- The avatar disc shows a member's picture if they set one, and their initials on a color hashed from their name if they have not. The same disc and the same color appear on the card the broadcast renders, so a member looks the same in the app and on a stream. Pictures are cover cropped into the circle, never squashed, and the space is reserved either way, so nothing shifts when one arrives mid-session.
- Your own strip is dimmed with a "you" tag. Self monitoring is local, on your interface, not through the server, so your own channel has no fader in the mix.
- The host additionally sees a Revoke button on every other strip. Revoking ejects that member and kills their invite, with a confirmation step.
- Chat, with timestamps. The metronome panel shows tempo, beats per bar, and click state; the host sets them, and "hear the click" is your own choice.
- The status bar, in the same place every session. Leave is on the right, with a confirmation; leaving does not end the session, and your seat is kept.
- The **on air** lamp, beside the session id: dark until the host starts a broadcast, amber while one is running, with a count of how many platforms are receiving it beside the latency readout. Only the host can start or stop one, but everyone sees the lamp, because everyone is in it. See [Streaming to Twitch and YouTube](streaming.md).

A member who stops responding grays out after 10 seconds and their fader freezes; when they reconnect with their invite, they come back in the same seat.

At ten musicians the strips extend past the window and scroll horizontally:

![Session screen at capacity, ten mixer strips with a horizontal scrollbar and ten listeners named below](../images/session_full.png)
*A session at the 10 musician cap in the current build, with 10 listeners connected.*

## The latency readout

The headline number in the status bar is mouth to ear latency in milliseconds: from sound entering your interface to a bandmate's sound leaving theirs, measured, not estimated. Under 20 ms most people stop noticing; under 30 ms feels like playing across a large stage. Next to it: `rtt`, your round trip to the server; `buffer`, the jitter buffer depth in 2.5 ms frames, which climbs when your network jitters; `loss`, the packet loss percentage, which should sit near 0.0%; and your input and output meters. Hosts also see elapsed time and cost so far. [Troubleshooting](troubleshooting.md#latency-feels-high) turns each number into an action.

## Devices and buffer size, mid-session

Settings in the top bar opens over the session without covering the strips or the status bar:

![Settings drawer beside the session: buffer size choices with the current mouth to ear figure, an input level meter, capture and playback pickers, and the avatar row](../images/session_settings.png)
*Settings mid-session: device and buffer changes apply immediately; the stream reopens in place.*

- **Buffer size** offers 120, 240, or 480 frames (2.5, 5, or 10 ms). Pick the smallest that plays clean; crackles mean one step up. The mouth to ear figure under the choices moves with them, because the buffer is part of it.
- The **Input level** meter should move when you play. If it is still, the wrong capture device is selected or the operating system has not granted microphone access.
- **Capture** and **Playback** list your machine's real audio devices, system default first. Changing one mid-session reopens the audio stream on the new device without leaving the session.
- **Your avatar** opens a file picker: choose a PNG or JPEG and everyone in the session gets the picture over the same encrypted link as the audio. A photo is cropped square and fitted to 256x256 first, so a picture straight off a phone works as it is. The choice lasts for this run of the app; set it again after a restart. Remove drops it here and on your next join, though members already in the session keep the picture you sent them.

## From the terminal

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
