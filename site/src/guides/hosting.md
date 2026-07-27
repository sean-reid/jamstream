# Hosting a session

Hosting means launching one small virtual machine in your own cloud account, minting the invites, and ending the session when you are done. The CLI does all three, and so does the desktop app: its host wizard walks the same steps, stores your provider credentials in the system keychain, joins you automatically once the server answers, and both record the session in the same place, so a session hosted in the app shows up in `jamstream status` and can be ended from either side.

Hosting does not have to involve a cloud at all: `jamstream host --provider local` runs the server on your own computer, costs nothing, and is the right choice when everyone is on the same network. This page covers the cloud path; [Playing on the same network](local.md) covers local mode.

## The region table

Before launching, JamStream measures the round trip from your machine to each of the provider's regions and prints a table:

```text
REGION              WORST RTT         HOURLY       EGRESS
nyc3                    14 ms    $0.02679/hr     $0.01/GB
tor1                    27 ms    $0.02679/hr     $0.01/GB
sfo3                    72 ms    $0.02679/hr     $0.01/GB
```

- **WORST RTT** is the slowest measured round trip, in milliseconds, among the people probing. At create time that is only you; your bandmates' distances are not known yet. The probes time a TCP handshake against each region's public endpoints, which tracks the UDP path closely enough to rank regions.
- **HOURLY** is the machine's current price per hour, fetched live where the provider offers it.
- **EGRESS** is what the provider charges per GB of outbound audio.

The pick weighs worst round trip and hourly price equally, and the table is printed in that order, best first. The top row launches unless you override it:

```console
$ jamstream host --provider digitalocean --region tor1 ...
```

A region under 30 ms from everyone keeps the network's share of latency in single digits each way, which is what makes the total playable. If the band spans a continent, pick the region that is mediocre for everyone over the one that is perfect for you; the person with the worst round trip sets the feel. See [Troubleshooting](troubleshooting.md) for what the numbers mean in the ear.

![Wizard step 2 of 4, a region table with worst rtt, hourly, and egress columns](../images/wizard_region.png)
*The same table in the app's host wizard, step 2 of 4, with prices fetched live and latencies probed from the host's machine.*

## Invites are minted at launch

`host` mints one invite per seat, up front, on your machine:

- `--musicians N`: musician seats, **counting you**, default 4. A session of 4 is you plus 3 invites to hand out; `--musicians 1` hosts alone. The cap is 10, which is exactly what the server admits.
- `--listeners N`: seats for people who only listen, default 0, one invite each. The cap is 20.

Counting yourself in `--musicians` is a change from earlier builds, where the number meant guests only and 10 could mint an eleventh seat the server would refuse. One number now means one thing on every surface: the flag, the app's "musicians, including you" dial, the invites panel's mint limit, and the server's own admission check.

Each invite is tied to one seat and signed. The CLI cannot add seats to a running session; the app's invites panel can mint more mid-session, within the same caps. Unused invites cost nothing. See [Joining a session](joining.md) for how invites behave.

![Invites panel over the session screen listing per-person invites with copy and revoke buttons](../images/session_invites.png)
*The app after launch: the wizard joins you automatically and opens the invites panel. Each link admits one person; copy, revoke, or mint more.*

## The safety knobs

Every session carries two timers, both set at launch:

- `--idle-min`, default 10: with no musicians connected for this many minutes, the server shuts itself down and the machine is destroyed.
- `--max-hours`, default 12: the hard cap. The machine is destroyed at the cap no matter what, and invites expire with it.

The app's host wizard shows both on its preview step, as "idle exit" and "hard cap", with the same defaults and the same meaning, so neither surface hides a timer the other exposes.

There is no way to extend a running session; host a new one. The point of the caps is that a forgotten session costs a bounded, small amount, not a month of billing. [Understanding cost](cost.md) covers the other guardrails.

## While it runs

`jamstream status` lists your sessions with elapsed time, cost accrued so far, and a projection. In the app, the host's session screen shows the same cost figure live in the status bar, next to latency.

If you host from the app or another terminal later, the CLI also warns at host time when it finds JamStream-tagged machines already running in your account, so a stray session does not hide behind a new one.

## Ending

`jamstream end 3f2a9c01` (any unambiguous prefix of the session id works) or `jamstream end --last`. In the app, "End session for everyone" in the invites panel does the same. Ending destroys the machine, confirms with the provider that nothing tagged with the session is still listed, and marks the local record ended. The invites are dead from that moment.
