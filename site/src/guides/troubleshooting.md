# Troubleshooting

## The app does not start

Every launch writes a log, and a failure before the window opens lands in it. The app prints the path itself under Settings, then You. Read the file directly at:

| Platform | Log path |
|---|---|
| macOS | `~/Library/Application Support/jamstream/logs/app.log` |
| Windows | `%LOCALAPPDATA%\jamstream\logs\app.log` |
| Linux | `~/.local/share/jamstream/logs/app.log`, or under `$XDG_DATA_HOME` if you set it |

The file is truncated at every start, so it holds one run and no history. It opens with a version banner; everything after that line is a warning or a crash, and that is what to paste into the [bug report](../about.md#reporting-problems). A file with the banner alone means the run was healthy.

Running `.\jamstream-app.exe` from PowerShell shows nothing: a release build on Windows has no console attached, by design, so the log is the only place the error appears. On macOS and Linux a terminal run also prints to stderr.

A local session server that dies at startup is a different case with its own log; see [Playing on the same network](local.md#from-the-terminal).

## Latency feels high

Start with the number, not the feel. The headline figure in the session status bar is mouth to ear: the milliseconds from sound entering an interface to the last buffer JamStream hands your sound card, the playout cushion included. What the card holds after that is the one part no figure here can see.

The network is in that figure twice, because your sound crosses to the server and out again to the player hearing it, and your own round trip is charged for both crossings. That is right when the band's connections are alike, and low by the difference when somebody's is worse than yours.

Two players in the same room stand about 3 ms apart per meter of air; a total under 30 ms feels like playing across a large stage, and most people stop noticing under 20 ms.

Check these in order of payoff:

1. **Bluetooth.** Bluetooth headphones or earbuds add more delay than JamStream's entire network path. Use wired headphones, always. This is the most common cause of "it feels wrong" with a good-looking number.
2. **Wifi.** Wifi adds jitter, which inflates the jitter buffers. Hover the latency number for the `buffer` readout, which reads "buffer 3/4 frames": the depth it is holding against the depth it is aiming for, in 2.5 ms frames. If it sits high or climbs, plug in ethernet.
3. **Buffer size.** On the Audio tab, under Buffer size, pick the smallest of 120, 240, or 480 frames (2.5, 5, or 10 ms) that stays clean. A **crackling** tag beside the latency number, or the line under the choices saying the device is not keeping up, means take the next size up.

   Each step costs three times its own size in the number: the buffer is paid once going in and twice coming out. The hover names both, as `capture buffer` and `playout cushion`.

   The cushion is not fixed. A machine that keeps letting the playout ring run low gets a deeper one, 2.5 ms at a time and never past twice the buffer, and hands it back once it is keeping up again. The hover and the headline number both follow it, so a deeper cushion than twice your buffer is that, not a fault.
4. **Sample rate.** A direction JamStream's own converter carries costs about 3 ms, and says so with a muted **converting** tag naming both rates.

   The headline number already includes it and the hover breaks it out per direction. [Device problems](#device-problems) has the whole ladder.
5. **WASAPI mode, on Windows.** Exclusive adds about 10 ms, shared 20 to 30. The latency number's hover names which one this session got.

   JamStream asks for exclusive by default. The "Allow exclusive access" setting under Devices turns that off when another app needs the same device, at the shared-mode cost.

   Exclusive mode itself needs "Allow applications to take exclusive control of this device" ticked on the device's Advanced tab in the Sound dialog, for both input and output. Without it the session opens shared instead, and the hover is the only place that says so.
6. **Region.** If the session's round trip (`rtt`, on the latency number's hover) is high for you specifically, the server is far from you. The host can pick a fairer region next time; see [Hosting a session](hosting.md#the-region-table).
7. **Loss.** The same hover carries a rate per direction over the last second, so a bad moment clears once it passes. `uplink loss` is what the band is missing of you; `downlink loss` is what you are missing of them.

   Both should sit near 0.0%. Above 1% sustained points at the local network: congested wifi, a saturated uplink, a bad cable.

   An **uplink loss** tag beside the latency number means the band is missing enough of you to hear it. Nothing you hear will tell you, because your monitoring, your meters and the room are all downstream of it. Same fix, from your end.

## The band can't keep together

This is a different complaint from a high latency number: everyone sounds right alone, and only together does the tempo sag, worse the further apart the band is.

Each player hears their own instrument immediately and everyone else a full network path away, so the error each one feels is their own [mouth to ear number](#latency-feels-high), and no two players carry the same one.

Two fixes, and neither of them moves that number:

- The host turns the click on, in the Metronome panel; each player chooses whether to hear it. The server mixes it into every personal mix, so it reaches everyone on one timeline and the band follows it instead of each other.
- Turn on **Hear yourself through the server**, on the Audio tab. Your own sound joins the mix, so the gap you hear becomes the difference between two uplinks instead of the whole network path. Needs headphones: through speakers it loops your own signal back into the microphone.

When mouth to ear holds above about 30 ms with somebody else in the room, the Audio tab offers it above that control, once per session. Read the headphone line under it before ticking anything, and leave it off if you are on speakers.

## Device problems

Input and output devices are picked on the Audio tab of Settings, under Devices: a Capture picker, a Playback picker, and Rescan beside the heading. The Input level meter above them should move when you play.

The lists come from the platform's audio backend (CoreAudio on macOS, WASAPI on Windows, PipeWire or ALSA on Linux). Each starts with a System default entry that follows the operating system when the default moves, with the concrete devices listed after it.

A change mid-session reopens the stream on the new device without leaving the session.

A stream that stops says so in three places: the device's own reason above the mixer strips, a **no audio** tag beside the latency number, and both the reason and what the reopen cadence is doing under the pickers.

| Symptom | Cause | Fix |
|---|---|---|
| Input meter stays still | Wrong capture device selected, or the OS has not granted microphone access | Pick the right device; on macOS grant microphone permission when prompted, and on Windows allow desktop apps to access your microphone, or capture is silence |
| New interface missing from a picker | Devices plugged in or enabled after launch are not listed | Press Rescan, next to the Devices heading |
| "no devices found" in a picker | The platform reported nothing for that direction | Check the interface is connected and visible to other apps, then Rescan |
| Selected device disappears mid-session | A rescan found the device gone | The picker falls back to System default and says so under the pickers |
| A device pick does not take | The device refused to open | The pick stays selected, the device's own reason sits under the pickers, and the app tries again six times on a widening wait; pick another device to get sound back sooner |
| Sound stops, and a **no audio** tag appears beside the latency number | The stream stopped and is being reopened, or was reopened six times without staying open | The Audio tab says which; once it says the device did not stay open, nothing more is tried until you pick a device there |
| Short gaps the band hears, and a **cutting out** tag appears beside the latency number | The device has stopped and been reopened three or more times in the last few minutes. Each one is a gap, and each is reopened too fast to show as **no audio** | The Audio tab counts them under the pickers. Check the cable and the port first; on Windows, ticking off "Allow exclusive access" also helps, because an exclusive endpoint drops the stream on any hiccup |
| Other apps go silent while you play (Windows) | Exclusive mode holds the device alone for the lowest latency | Untick "Allow exclusive access" under Devices to share the device, at the 10 to 20 ms shared-mode cost |

Sessions run at 48 kHz, and JamStream carries a device at any other rate automatically. A note under the pickers, per direction, names whichever of these it landed on:

| What happened | The note | Cost |
|---|---|---|
| The device already runs at 48 kHz | Nothing is said | None |
| macOS moved the device's own clock | "moved the capture device to 48 kHz (was 44.1)" | None |
| Windows or PipeWire converts inside the platform | "the OS is converting capture to this device's 44.1 kHz" | Not measured, and no tag |
| JamStream's own converter carries the direction | "converting capture 44.1 kHz to 48 kHz (+2.7 ms)" | About 3 ms, counted in mouth to ear, with a **converting** tag |

On macOS a device another app holds at 44.1 kHz, BlackHole under GarageBand say, is moved to 48 kHz and GarageBand keeps playing, because macOS resamples app output to the device's rate. An app that takes the clock back mid-session gets it: JamStream stops asking and converts instead.

Only the last row costs milliseconds. Any of them goes away if you set the device to 48 kHz yourself:

| Platform | Where |
|---|---|
| macOS | Audio MIDI Setup, Format |
| Windows | Sound settings, then More sound settings (or run `mmsys.cpl`), the device's Properties, Advanced, Default Format; Recording tab for the input, Playback tab for the output |
| Linux | PipeWire carries any graph rate as it is; on bare ALSA the converter covers the card |

The one device still refused is a Bluetooth or headset microphone in its hands-free mode: it has no 48 kHz setting anywhere and would sound like a phone call, so use another capture device.

## Firewall and NAT

Members need only outbound UDP, so home routers and NAT need no configuration and no port forwarding.

- If joining times out after about 10 seconds, something between you and the server is dropping UDP. Corporate and campus networks sometimes block outbound UDP on unusual ports; a phone hotspot is a quick way to confirm that is the cause.
- Hosting on your own Windows machine is the exception: the first local host raises a Defender Firewall prompt for `jamstreamd.exe`, and it must be allowed on both Private and Public networks or bandmates on your network hit that same 10 second timeout.

## Session ends unexpectedly

| Message | Meaning | Action |
|---|---|---|
| "removed from the session" | The host revoked your invite or ended the session | None; ask the host for a new invite if the session is still running |
| "connection lost: no packets for 10 seconds" | A network drop | Rejoin with the same invite; your seat is kept |
| (session vanished, nothing shown) | Everyone left for longer than the idle window (default 10 minutes), and the server destroyed itself on purpose | Host a new session; see the [dead man's switch](../how-it-works.md#the-machine) |

## Something is still running that should not be

In the app, **Stop strays** on the Recent sessions card destroys every JamStream-tagged machine in every account this computer holds a key for, and brings the session records back in line with what it found.

It reports three things apart: what it stopped, what it could not stop, and which providers it could not search. A provider with no credentials saved here, or one whose listing failed, was never looked at, which is not the same as finding nothing.

From a terminal:

```console
$ jamstream sweep --dry-run
```

lists the same machines without destroying them. Run it without `--dry-run` to destroy them. Either way, a named instance that could not be stopped is still billing; destroy it from the provider's own console and report the bug.
