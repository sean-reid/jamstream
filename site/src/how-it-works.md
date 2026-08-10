# How it works

## The machine

When you host, your computer asks a cloud provider for one small Linux VM, hands it a boot script, and completes an encrypted handshake with it before showing you any invite. A session is never announced that cannot be joined.

In [local mode](guides/local.md) there is no VM: the same server runs as a process on your own machine.

Only the one UDP session port is reachable from outside. Nothing about a session persists in the cloud after it ends except a take you asked it to record, which lands in your own bucket.

The machine destroys itself three ways:

- When you end the session.
- When no musician has been connected for the idle window (default 10 minutes).
- At the hard cap (default 12 hours), regardless.

The provider and the machine enforce those, not your laptop, so quitting the app does not leave anything running and billing. `jamstream sweep` destroys anything tagged that somehow survives.

GCP differs in one way worth knowing: an idle session stops serving but the machine is not deleted until you end the session, until your next `jamstream sweep`, or at the hard cap. The other two providers delete it on idle.

## Invites

There are no accounts and no JamStream servers. Identity is the invite.

Hosting generates the session's keys on your computer and mints one invite per seat, each carrying one seat and an expiry. Invites are revocable mid-session and all of them expire at the hard cap. Every packet is encrypted and authenticated from the first handshake byte. There is no plaintext mode.

What that buys you: strangers cannot join, listen in, or disrupt a session, a leaked invite is one revocation away from useless, and no third party, JamStream included, sits between your band and your machine.

## Latency

The target is under 30 ms mouth to ear. Measured with real Opus and real encryption over a simulated network:

| Round trip to the server | Mouth to ear |
|---|---|
| 1 ms, one local network | 14.7 ms |
| 12 ms, same region | 24.3 ms |
| 45 ms, cross country over DSL | 69.8 ms |

Each figure runs from sound entering the interface to the last buffer JamStream hands the sound card, the playout buffer included, which is as far as a measurement from inside the app can see: what the card holds after that is beyond its reach.

The two network legs are the only parts that grow with distance, which is why [region choice](guides/hosting.md#the-region-table) is the decision that matters. Under about 30 ms it feels like standing on a stage together. At 70 ms it feels like a phone call.
