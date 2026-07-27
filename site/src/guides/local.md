# Playing on the same network

`jamstream host --provider local` runs the session server as a process on your own computer instead of a cloud machine. Everything else is the same flow: invites are minted up front, the CLI completes a real encrypted handshake with the server before printing them, `jamstream status` lists the session, and `jamstream end` tears it down.

## When local is the right choice

- Everyone is in the same room or on the same network. The invites carry your machine's network address (for example 192.168.1.12), so any machine on the same network can join.
- You want the lowest possible latency. The audio path is one hop across your own network, with no internet in it.
- You want to try JamStream before creating a cloud account. Local sessions need no credentials and cost nothing.

Playing alone on one machine also works: joining from the hosting computer goes over the loopback interface.

## The limits, honestly

An invite that names 192.168.1.12 means nothing outside your network. Reaching a local session across the internet would take router port forwarding: forwarding the session's UDP port (default 43210) to this computer in your router's admin pages, a public IP that many home connections behind carrier NAT do not have, and an invite carrying that public address, which the current build cannot mint (invites carry the network address it discovers, and multi-address invites are future work). JamStream automates none of that. If anyone is joining from outside your network, host in the cloud instead; see [Provider setup](providers.md).

## Ending and the idle exit

`jamstream end` kills the server process; for local sessions the instance id shown in `status` and `end` output is the process id. If you forget, the server watches its own activity and exits on its own after `--idle-min` minutes (default 10) with no musicians connected. There is no bill either way. `--max-hours` still shapes the invites: they expire at the cap, so nobody new can join after it.

If a laptop dies mid-session or a state file is lost, `jamstream sweep` finds local strays the same way it finds cloud ones: the local provider keeps an on-disk registry of the processes it spawned, so a later sweep from a fresh shell still sees and kills them.

## Where the server binary comes from

Local mode runs a `jamstreamd` binary already on your machine; nothing is downloaded, and the `--artifact-url` and `--artifact-sha256` flags are not needed. The binary is found in this order:

1. The `JAMSTREAMD_PATH` environment variable, if set, is used as the path.
2. A `jamstreamd` sitting next to the `jamstream` executable itself.
3. `jamstreamd` on your PATH.

On Linux x86_64, the [install script's](../download.md) `--with-server` flag puts `jamstreamd` next to the `jamstream` CLI, satisfying the second. The desktop app ships `jamstreamd` alongside the app binary, which is how hosting from the app works everywhere. Building from source with `cargo install --path crates/server` satisfies the last one. If none resolves, `host` fails before starting anything, with an error naming all three places it looked.

## What lands on disk

Under your platform's data directory in `jamstream/`: a `local.json` registry of running server processes, and one directory per session holding the server's config and its log (`server.log`, the first place to look if a local server exits at startup). The session state files that `status` and `end` read live in `jamstream/sessions/` as for any provider.
