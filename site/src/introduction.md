# Introduction

JamStream is a desktop app that lets a band play together over the internet, with latency low enough to actually play.

- **One of you hosts.** The app starts a small server, on your own computer or in your own cloud account, that exists only for the length of the session.
- **Everyone else joins with a link.** Paste a personal invite into the app; there are no accounts and no JamStream servers in the middle.
- **Every packet is encrypted**, and the server shuts itself down the moment the music stops.
- **The cost lands on you, not JamStream.** A cloud session runs a few cents on your own provider account; nobody else is paid anything.

![A four piece mid session: one mixer strip per musician with avatar, fader and mute, a chat column, the metronome, and a status bar reading 7.9 ms mouth to ear](images/session_demo.png)
*A four piece playing. One strip per musician, the click, and the latency in the bar.*

## What it costs

Sessions hosted on your own computer cost nothing. A three hour cloud session with four musicians costs about $0.08 on DigitalOcean, paid to your provider by the second. The preview shown before every launch pulls current pricing and is the number to trust; see [Understanding cost](guides/cost.md).

## Streaming

The host can put a session on air to Twitch and YouTube Live, either one or both at once, and one platform dropping out never interrupts the other. Stream keys are masked, never shown back, and kept in your system keychain. See [Streaming to Twitch and YouTube](guides/streaming.md).

## Project status

JamStream is in beta. Download it for macOS, Windows, and Linux from the [Download](download.md) page.

- The desktop app is the product. Its wizard hosts real sessions on your own computer or in your cloud account, joins you automatically, and manages the invites while the session runs.
- Every app build bundles its own `jamstreamd` session server, so hosting locally needs nothing else installed.
- Screenshots on this site are from the current build and will change.
- The `jamstream` command line tool hosts, monitors, and ends the same sessions for automation, scripting, and headless use; see the [CLI reference](cli/index.md).

If something here does not match what the software does, that is a bug in one of them. [Report it.](about.md)
