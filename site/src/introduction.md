# Introduction

JamStream lets a band play together over the internet with latency low enough to actually play. One of you hosts: a small server is created in your own cloud account, close to your group, and exists only for the length of the session. Everyone else joins with a personal invite link; there are no accounts and no JamStream servers in the middle. Every packet is encrypted, and the server shuts itself down when the music stops. When the session ends, you have paid your cloud provider a few cents and nobody else anything.

![JamStream home screen with a field to paste an invite and a button to host a session](images/home_empty.png)
*The home screen in the current build: paste an invite or host a session.*

## What it costs

You pay your cloud provider directly, by the second, for one small virtual machine. For a three hour session with four musicians on the cheapest suitable machines:

| Provider | Machine | Machine cost | Audio traffic (about 1.6 GB) | Total |
|---|---|---|---|---|
| DigitalOcean | s-2vcpu-2gb, $0.02679/hr | $0.080 | inside the included transfer | about $0.08 |
| AWS | t4g.medium, about $0.034/hr | $0.101 | inside the free 100 GB/month | about $0.10 |
| GCP | e2-medium, about $0.034/hr | $0.101 | about $0.19 at $0.12/GiB | about $0.30 |

These are public on-demand prices as of July 2026 and they drift. The cost preview shown before every launch pulls current pricing and is the number to trust; see [Understanding cost](guides/cost.md).

## Streaming

Broadcasting a session to platforms like Twitch and YouTube is designed but not shipped, so this documentation does not cover it further.

## Project status

JamStream is under active development and not yet at a first release.

- The command line tool hosts, monitors, and ends real sessions. It also runs the whole flow against a built-in mock provider, so you can try everything without a cloud account.
- The desktop app is usable but unfinished. Its host wizard currently launches only the mock provider; real sessions are hosted from the CLI. Screenshots on this site are from the current build and will change.
- No release artifacts are published yet, so hosting on a real provider means pointing the CLI at a `jamstreamd` server build you host yourself. The [quickstart](quickstart.md) shows where those flags go.

If something here does not match what the software does, that is a bug in one of them. [Report it.](about.md)
