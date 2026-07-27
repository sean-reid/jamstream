# How it works

One honest page on what actually happens when you host and play. Deeper detail lives in the source.

## The machine's life

When you host on a cloud provider, your computer asks it for one small Linux VM and hands it a boot script. (In [local mode](guides/local.md) there is no VM: the same server starts as a process on your machine, with the same keys and settings, and the story picks up at step 6.) The machine:

1. writes its keys and settings from launch data held in memory, never onto a disk image that outlives it;
2. arms its dead man's switch and closes its firewall down to the one UDP session port, before anything that can fail has run, so a boot that goes wrong still ends with the machine destroying itself;
3. blocks the cloud's metadata service for every account except root, so the session server, which does not run as root, cannot read the launch data back out of it;
4. downloads a pinned build of `jamstreamd`, the session server; release builds carry the URL of their release's own server build baked in, so host and server always match;
5. verifies the download's sha256 checksum against the one pinned alongside the URL, and refuses to start on a mismatch;
6. starts serving.

The hosting app or CLI then performs a full encrypted handshake against it before showing any invite, so a session is never announced that cannot actually be joined.

From its first second the machine is under a dead man's switch: with no musicians connected for the idle window (default 10 minutes) it destroys itself, and at the hard cap (default 12 hours) it is destroyed regardless, enforced by the machine and the provider rather than by your laptop. Ending the session destroys it immediately. Every machine is tagged, and `jamstream sweep` destroys anything tagged that somehow survives. Nothing about a session persists in the cloud after it ends.

## Invites and encryption, in plain language

There are no accounts and no JamStream servers. Identity is the invite.

When you host, your computer generates the session's keys locally and mints one invite per seat. Each invite is a signed statement: this person may occupy seat 3 of session `3f2a9c01` at this address until this time. The server, which received your public key at boot, admits only holders of statements you signed. Invites are individually revocable mid-session, and all of them expire at the session's hard cap.

Every packet between every member and the server is encrypted and authenticated, from the first handshake byte; there is no plaintext mode. Replayed or tampered packets are dropped. The server itself knows nothing about anyone beyond what the host signed into their invite: a seat number, a role, an expiry.

What this buys you concretely: strangers cannot join, listen in, or disrupt a session; a leaked invite is one revocation away from useless; and no third party, JamStream included, sits between your band and your machine.

## Where the milliseconds go

The design target is mouth to ear under 30 ms when everyone's round trip to the server is under 20 ms, which is why region choice gets its own [table](guides/hosting.md#the-region-table). The budget, simplified from the protocol's accounting at 2.5 ms audio frames:

| Stage | Typical |
|---|---|
| Your capture buffer | 2.5 to 5 ms |
| Encode | under 1 ms |
| Network to the server | 3 to 10 ms |
| Server buffer and mix | 5 to 7.5 ms |
| Network back to a bandmate | 3 to 10 ms |
| Their buffer, decode, and playout | 5.5 to 10 ms |
| Total mouth to ear | 21 to 34 ms |

The two network legs are the only stages not under JamStream's control, and the only ones that grow with distance. Everything else is fixed small: audio is cut into 2.5 ms frames, buffers adapt to measured jitter and report their depth in the status bar, and the mix runs on a 2.5 ms tick. The `7.9 ms mouth to ear` readout in the session screenshots is a same-city case; across a region expect the mid-20s, which still feels like a stage, not a phone call.
