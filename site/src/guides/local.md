# Playing on the same network

Picking **local** in the host wizard's first step runs the session server as a process on your own computer instead of a cloud machine. Every desktop app build bundles its own `jamstreamd`, so there is nothing to install and no account to create: local is selectable the moment the app opens, the wizard skips the region step, and the launch is **Start the session** instead of a cost preview. Everything else is the same flow: invites are minted up front, the app completes a real encrypted handshake with the server before showing them, joins you automatically, and the Invites tab ends the session.

## When local is the right choice

- Everyone is in the same room or on the same network. The invites carry your machine's network address (for example 192.168.1.12), so any machine on the same network can join.
- You want the lowest possible latency. The audio path is one hop across your own network, with no internet in it.
- You want to try JamStream before creating a cloud account. Local sessions need no credentials and cost nothing.

Playing alone on one machine also works.

## The limits, honestly

An invite that names 192.168.1.12 means nothing outside your network. Reaching a local session across the internet would take router port forwarding: forwarding the session's UDP port to this computer in your router's admin pages, a public IP that many home connections behind carrier NAT do not have, and an invite carrying that public address, which the current build cannot mint (invites carry the network address it discovers, and multi-address invites are future work). JamStream automates none of that. If anyone is joining from outside your network, host in the cloud instead; see [Provider setup](providers.md).

## Ending and the idle exit

**End session for everyone** on the Invites tab kills the server process, and the same shared state means `jamstream end` from a terminal does too; for local sessions the instance id shown in `status` and `end` output is the process id. If you forget, the server watches its own activity and exits on its own after 10 minutes with no musicians connected. There is no bill either way. The local server also exits at the 12 hour hard cap, and the invites expire with it.

If a laptop dies mid-session or a state file is lost, `jamstream sweep` finds local strays the same way it finds cloud ones: the local provider keeps an on-disk registry of the processes it spawned, so a later sweep from a fresh shell still sees and kills them.

## From the terminal

`jamstream host --provider local` (local is the default provider, so the flag is optional) runs the same flow: it prints one invite per seat after the handshake check and takes `--musicians`, `--listeners`, `--idle-min`, and `--port`; the wizard offers the same seats and windows, and picks the port itself. See the [host reference](../cli/host.md). The app picks a free UDP port for each local session; the CLI defaults to 43210.

Unlike the app, the CLI does not bundle the server, so local mode needs a `jamstreamd` binary already on your machine; nothing is downloaded. It is taken from `JAMSTREAMD_PATH`, then from beside the running executable, then from your PATH, and `host` fails before starting anything with an error naming every place it looked; on Linux x86_64 the [install script's](../download.md) `--with-server` flag puts one in the second of those places.

If a local server exits at startup, the first place to look is its `server.log`, in the session's own directory under your platform's data directory in `jamstream/sessions/`.
