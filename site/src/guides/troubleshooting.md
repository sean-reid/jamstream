# Troubleshooting

## The app does not start

Startup errors print to a console window that closes with the app, and there are no log files today, so a double-click that fails shows nothing to read. Open PowerShell in the folder holding `jamstream-app.exe`, run `.\jamstream-app.exe`, and the message stays on screen; paste it into the [bug report](../about.md#reporting-problems). The same trick works on macOS and Linux: run the binary from a terminal. A local session server that dies at startup is a different case with its own log; see [Playing on the same network](local.md#from-the-terminal).

## Latency feels high

Start with the number, not the feel. The headline figure in the session status bar is mouth to ear: the milliseconds from sound entering your interface to your bandmate's sound leaving theirs, measured, not estimated. Two players in the same room stand about 3 ms apart per meter of air; a total under 30 ms feels like playing across a large stage, and most people stop noticing under 20 ms.

Things to check, in order of payoff:

1. **Bluetooth.** Bluetooth headphones or earbuds add more delay than JamStream's entire network path. Use wired headphones, always. This is the most common cause of "it feels wrong" with a good-looking number.
2. **Wifi.** Wifi adds jitter, which inflates the jitter buffers. Hover the latency number in the status bar for the `buffer` readout, which reads "buffer 3/4 frames": the depth it is holding against the depth it is aiming for, in 2.5 ms frames. If it sits high or climbs, plug in ethernet.
3. **Buffer size.** On the Audio tab of Settings, under Buffer size, pick the smallest of 120, 240, or 480 frames (2.5, 5, or 10 ms) that plays clean. Crackles mean one step up.
4. **WASAPI mode, on Windows.** In exclusive mode the device adds about 10 ms; shared mode adds 20 to 30 ms. On the device's Advanced tab in the Sound dialog, tick "Allow applications to take exclusive control of this device", for both the input and the output.
5. **Region.** If the session's round trip (`rtt`, on the latency number's hover) is high for you specifically, the server is far from you. The host can pick a fairer region next time; see [Hosting a session](hosting.md#the-region-table).
6. **Loss.** The `loss` percentage, on the same hover, should sit near 0.0%. Sustained loss above 1% points at the local network: congested wifi, a saturated uplink, a bad cable.

## Device problems

Input and output devices are picked on the Audio tab of Settings, under Devices: a Capture picker, a Playback picker, and an input level meter that should move when you play. If the meter is still, the wrong capture device is selected or the operating system has not granted microphone access to the app. The lists come from the platform's audio backend (CoreAudio on macOS, WASAPI on Windows, PipeWire or ALSA on Linux), with the system default listed first, and a change mid-session reopens the stream on the new device without leaving the session.

- "no devices found" in a picker means the platform reported nothing for that direction; check that the interface is connected and visible to other apps.
- Sample rate is fixed at 48 kHz and nothing resamples, so a device that will not open at 48 kHz is refused rather than run at the wrong rate. The message names the device's rate and what to do about it, and the remedy depends on the platform: on macOS, look for a 48 kHz entry under Audio MIDI Setup, Format, and use another device if there is none; on Windows, open the classic Sound dialog (Sound settings, then More sound settings, on Windows 11, or run `mmsys.cpl`) and set 48 kHz under the device's Properties, Advanced, Default Format, on the Recording tab for the input device and the Playback tab for the output, because both must open at 48 kHz; on Linux under PipeWire, the graph rate is not the problem because PipeWire converts rates, so that device has no 48 kHz mode and another one is the answer; on bare ALSA, nothing converts at all, so start PipeWire or use a device with a 48 kHz mode.
- On macOS, grant microphone permission when prompted; without it, capture is silence.

## Firewall and NAT

Members need only outbound UDP, so home routers and NAT need no configuration and no port forwarding.

- If joining times out after about 10 seconds, something between you and the server is dropping UDP. Corporate and campus networks sometimes block outbound UDP on unusual ports; a phone hotspot is a quick way to confirm that is the cause.
- Hosting on your own Windows machine is the exception: the first local host raises a Defender Firewall prompt for `jamstreamd.exe`, and it must be allowed on both Private and Public networks or bandmates on your network hit that same 10 second timeout.

## Session ends unexpectedly

- "removed from the session" with a reason means the host revoked your invite or ended the session.
- "connection lost: no packets for 10 seconds" is a network drop; rejoin with the same invite, your seat is kept.
- If everyone left for longer than the idle window (default 10 minutes), the server destroyed itself on purpose. This is the [dead man's switch](../how-it-works.md#the-machine); host a new session.

## Something is still running that should not be

```console
$ jamstream sweep --dry-run
```

lists every JamStream-tagged machine across your configured providers. Run it without `--dry-run` to destroy them. If sweep reports a failure, the named instance is still billing; destroy it from the provider's own console and report the bug.
