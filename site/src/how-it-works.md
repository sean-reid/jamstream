# How it works

## The machine

When you host, your computer asks a cloud provider for one small Linux VM, hands it a boot script, and completes an encrypted handshake with it before showing you any invite, so a session is never announced that cannot be joined. In [local mode](guides/local.md) there is no VM: the same server runs as a process on your own machine.

Only the one UDP session port is reachable from outside. Nothing about a session persists in the cloud after it ends.

The machine destroys itself three ways: when you end the session, when no musician has been connected for the idle window (default 10 minutes), and at the hard cap (default 12 hours) regardless. The provider and the machine enforce those, not your laptop, so quitting the app does not leave anything running and billing. `jamstream sweep` destroys anything tagged that somehow survives.

One exception worth knowing: on GCP the idle window does not currently work, so the hard cap is the only thing that ends a session. End GCP sessions explicitly.

## Invites

There are no accounts and no JamStream servers. Identity is the invite.

Hosting generates the session's keys on your computer and mints one invite per seat, each naming one person, one seat, and an expiry. Invites are revocable mid-session and all of them expire at the hard cap. Every packet is encrypted and authenticated from the first handshake byte. There is no plaintext mode.

What that buys you: strangers cannot join, listen in, or disrupt a session, a leaked invite is one revocation away from useless, and no third party, JamStream included, sits between your band and your machine.

## Latency

The target is under 30 ms mouth to ear. Measured with real Opus and real encryption over a simulated network:

| Round trip to the server | Mouth to ear |
|---|---|
| Same city | 9.7 ms |
| Same region | 19.3 ms |
| Cross country over DSL | 64.8 ms |

The two network legs are the only parts that grow with distance, which is why [region choice](guides/hosting.md#the-region-table) is the decision that matters. Under about 30 ms it feels like standing on a stage together. At 65 ms it feels like a phone call.
