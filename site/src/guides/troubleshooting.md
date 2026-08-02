# Troubleshooting

## The app does not start

Every launch writes a log, and a failure before the window opens lands in it. Read that file:

- macOS: `~/Library/Application Support/jamstream/logs/app.log`
- Windows: `%LOCALAPPDATA%\jamstream\logs\app.log`
- Linux: `~/.local/share/jamstream/logs/app.log`, or under `$XDG_DATA_HOME` if you set it

The file is truncated at every start, so it holds one run and no history. It opens with a version banner; everything after that line is a warning or a crash, and that is what to paste into the [bug report](../about.md#reporting-problems). A file with the banner alone means the run was healthy. The app prints the path itself under Settings, then You, so you do not have to type any of the above.

Running `.\jamstream-app.exe` from PowerShell shows nothing: a release build on Windows has no console attached, by design, so the log is the only place the error appears. On macOS and Linux a terminal run also prints to stderr. A local session server that dies at startup is a different case with its own log; see [Playing on the same network](local.md#from-the-terminal).

## Latency feels high

Start with the number, not the feel. The headline figure in the session status bar is mouth to ear: the milliseconds from sound entering your interface to your bandmate's sound leaving theirs, measured, not estimated. Two players in the same room stand about 3 ms apart per meter of air; a total under 30 ms feels like playing across a large stage, and most people stop noticing under 20 ms.

Things to check, in order of payoff:

1. **Bluetooth.** Bluetooth headphones or earbuds add more delay than JamStream's entire network path. Use wired headphones, always. This is the most common cause of "it feels wrong" with a good-looking number.
2. **Wifi.** Wifi adds jitter, which inflates the jitter buffers. Hover the latency number in the status bar for the `buffer` readout, which reads "buffer 3/4 frames": the depth it is holding against the depth it is aiming for, in 2.5 ms frames. If it sits high or climbs, plug in ethernet.
3. **Buffer size.** On the Audio tab of Settings, under Buffer size, pick the smallest of 120, 240, or 480 frames (2.5, 5, or 10 ms) that plays clean. Crackles mean one step up. A choice below the device's own minimum is annotated with the size the device really delivers, so the number you read is the number you get.
4. **Sample rate.** A device that does not run at 48 kHz plays through a converter that adds about 3 ms per converted direction. While that happens a muted tag sits beside the latency number naming both rates ("converting 44.1 to 48 kHz"), the number already includes the cost, and the hover names each converted direction with its milliseconds. To remove the cost, set the device itself to 48 kHz in the system's sound settings; see [Device problems](#device-problems) for the walk on each platform.
5. **WASAPI mode, on Windows.** In exclusive mode the device adds about 10 ms; shared mode adds 20 to 30 ms. The latency number's hover says which one this session got, and whether the OS is converting playback to the device's own rate, which costs more still. JamStream asks for exclusive by default; the "Allow exclusive access" setting under Devices turns that off when you need other apps audible on the same device, at the shared-mode cost. For exclusive to be available at all, tick "Allow applications to take exclusive control of this device" on the device's Advanced tab in the Sound dialog, for both the input and the output.
6. **Region.** If the session's round trip (`rtt`, on the latency number's hover) is high for you specifically, the server is far from you. The host can pick a fairer region next time; see [Hosting a session](hosting.md#the-region-table).
7. **Loss.** The `loss` percentage, on the same hover, should sit near 0.0%. Sustained loss above 1% points at the local network: congested wifi, a saturated uplink, a bad cable.

## Device problems

Input and output devices are picked on the Audio tab of Settings, under Devices: a Capture picker, a Playback picker, and an input level meter that should move when you play. If the meter is still, the wrong capture device is selected or the operating system has not granted microphone access to the app. The lists come from the platform's audio backend (CoreAudio on macOS, WASAPI on Windows, PipeWire or ALSA on Linux). Each starts with a System default entry that follows the operating system when the default moves; the concrete devices are listed after it. A change mid-session reopens the stream on the new device without leaving the session.

- An interface plugged in or enabled after launch is not listed until you press Rescan, next to the Devices heading. If a rescan finds your selected device gone, the picker falls back to System default and says so under the pickers.
- "no devices found" in a picker means the platform reported nothing for that direction; check that the interface is connected and visible to other apps, then Rescan.
- A mid-session pick the device refuses does not switch you anywhere else: the pick stays selected, the app retries it every half second, and the device's own reason sits under the pickers until an open succeeds. Pick another device to get sound back sooner.
- Sessions run at 48 kHz, and JamStream carries a device at any other rate automatically. A device that can run at 48 kHz is opened there; on macOS that moves the device's own clock, and chat says so ("moved the capture device to 48 kHz (was 44.1)"). A device that cannot plays anyway through JamStream's converter: a tag beside the latency number names both rates, chat notes the conversion once, and its few milliseconds are counted in the mouth-to-ear figure. The worked example is BlackHole held at 44.1 kHz by GarageBand: joining moves BlackHole to 48 kHz and GarageBand keeps playing, because macOS resamples app output to the device's rate. If GarageBand takes the clock back mid-session, JamStream does not fight it: it stops touching that device's clock, converts instead, and says so in one line.
- The conversion is free to ignore. To shave its ~3 ms per direction, set the device to 48 kHz yourself: on macOS, Audio MIDI Setup, Format; on Windows, the classic Sound dialog (Sound settings, then More sound settings, on Windows 11, or run `mmsys.cpl`), the device's Properties, Advanced, Default Format, on the Recording tab for the input and the Playback tab for the output; on Linux, PipeWire carries any graph rate as it is, and on bare ALSA the converter covers the card. The one device still refused is a Bluetooth or headset microphone in its hands-free mode: it has no 48 kHz setting anywhere and would sound like a phone call, so use another capture device.
- On Windows, other apps going silent while you play is exclusive mode doing what it says: JamStream holds the device alone for the lowest latency. To jam over a backing track from a browser or DAW on the same device, untick "Allow exclusive access" under Devices and accept the 10 to 20 ms shared mode adds.
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

In the app, **Stop strays** on the Recent sessions card destroys every JamStream-tagged machine in every account this computer holds a key for, and brings the session records back in line with what it found. It reports three things apart: what it stopped, what it could not stop, and which providers it could not search. A provider with no credentials saved here, or one whose listing failed, was never looked at, which is not the same as finding nothing.

From a terminal:

```console
$ jamstream sweep --dry-run
```

lists the same machines without destroying them. Run it without `--dry-run` to destroy them. Either way, a named instance that could not be stopped is still billing; destroy it from the provider's own console and report the bug.
