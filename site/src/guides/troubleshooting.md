# Troubleshooting

## Latency feels high

Start with the number, not the feel. The headline figure in the session status bar is mouth to ear: the milliseconds from sound entering your interface to your bandmate's sound leaving theirs, measured, not estimated. Two players in the same room stand about 3 ms apart per meter of air; a total under 30 ms feels like playing across a large stage, and most people stop noticing under 20 ms.

The total is built from pieces, and you control several:

| Piece | Typical | You control it with |
|---|---|---|
| Your capture buffer | 2.5 to 10 ms | buffer size on the Audio tab of Settings |
| Network, both legs | 6 to 20 ms | region choice, wired ethernet |
| Server mix and jitter buffers | 7 to 12 ms | mostly automatic |
| Decode and playout | 3 to 5 ms | buffer size, device |

Things to check, in order of payoff:

1. **Bluetooth.** Bluetooth headphones or earbuds add more delay than JamStream's entire network path. Use wired headphones, always. This is the most common cause of "it feels wrong" with a good-looking number.
2. **Wifi.** Wifi adds jitter, which inflates the jitter buffers. Hover the latency number in the status bar for the `buffer` readout, which reads "buffer 3/4 frames": the depth it is holding against the depth it is aiming for, in 2.5 ms frames. If it sits high or climbs, plug in ethernet.
3. **Buffer size.** On the Audio tab of Settings, under Buffer size, pick the smallest of 120, 240, or 480 frames (2.5, 5, or 10 ms) that plays clean. Crackles mean one step up.
4. **Region.** If the session's round trip (`rtt`, on the latency number's hover) is high for you specifically, the server is far from you. The host can pick a fairer region next time; see [Hosting a session](hosting.md#the-region-table).
5. **Loss.** The `loss` percentage, on the same hover, should sit near 0.0%. Sustained loss above 1% points at the local network: congested wifi, a saturated uplink, a bad cable.

## Device problems

Input and output devices are picked on the Audio tab of Settings, under Devices: a Capture picker, a Playback picker, and an input level meter that should move when you play. If the meter is still, the wrong capture device is selected or the operating system has not granted microphone access to the app. The lists come from the platform's audio backend (CoreAudio on macOS, WASAPI on Windows, PipeWire or ALSA on Linux), with the system default listed first, and a change mid-session reopens the stream on the new device without leaving the session.

- "no devices found" in a picker means the platform reported nothing for that direction; check that the interface is connected and visible to other apps.
- Sample rate is fixed at 48 kHz and nothing resamples, so a device that will not open at 48 kHz is refused rather than run at the wrong rate. The message names the device's rate and what to do about it, and the remedy depends on the platform: on macOS, look for a 48 kHz entry under Audio MIDI Setup, Format, and use another device if there is none; on Windows, set the device to 48 kHz in Sound, the device's Properties, Advanced, Default Format; on Linux under PipeWire, the graph rate is not the problem because PipeWire converts rates, so that device has no 48 kHz mode and another one is the answer; on bare ALSA, nothing converts at all, so start PipeWire or use a device with a 48 kHz mode.
- On macOS, grant microphone permission when prompted; without it, capture is silence.

## Firewall and NAT

Members need only outbound UDP. The client opens one UDP socket to the server's address and port, both baked into the invite (cloud sessions use port 43210; local sessions hosted from the app pick a free port); it listens on no ports, so home routers and NAT need no configuration and no port forwarding. Steady keepalive traffic holds the NAT mapping open for the whole session.

- If joining times out after about 10 seconds, something between you and the server is dropping UDP. Corporate and campus networks sometimes block outbound UDP on unusual ports; a phone hotspot is a quick way to confirm that is the cause.
- The server side needs nothing from you: only the session port is reachable on the machine, and nothing else is.

## Session ends unexpectedly

- "removed from the session" with a reason means the host revoked your invite or ended the session.
- "connection lost: no packets for 10 seconds" is a network drop; rejoin with the same invite, your seat is kept.
- If everyone left for longer than the idle window (default 10 minutes), the server destroyed itself on purpose. This is the [dead man's switch](../how-it-works.md#the-machines-life); host a new session.

## Something is still running that should not be

```console
$ jamstream sweep --dry-run
```

lists every JamStream-tagged machine across your configured providers. Run it without `--dry-run` to destroy them. If sweep reports a failure, the named instance is still billing; destroy it from the provider's own console and report the bug.
