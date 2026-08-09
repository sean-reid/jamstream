# Hosting a session

Hosting means launching one small server, minting the invites, and ending the session when you are done. In the app, **Host a session** on the home screen walks four steps: where the server runs, which region, a cost preview, and the launch itself.

Hosting does not have to involve a cloud at all: picking **local** in the wizard's first step runs the server on your own computer, costs nothing, and is the right choice when everyone is on the same network. This page covers the cloud path; [Playing on the same network](local.md) covers local mode.

## The provider step

The first step lists where the server can run: local, then the three clouds, each with its status. A cloud with saved credentials reads `ready`; one without reads `setup needed`.

Selecting an unset provider opens an inline pane that takes the credential, checks it with a real API call, and saves it to your keychain. [Provider setup](providers.md) walks each provider from zero.

## The region table

Before launching, JamStream measures the round trip from your machine to each of the provider's regions and shows a table:

![Wizard step 2 of 4, a region table with worst rtt, hourly, and egress columns](../images/wizard_region.png)
*Step 2 of 4, with prices fetched live and latencies probed from the host's machine. The CLI prints the same table.*

| Column | What it means |
|---|---|
| worst rtt | The slowest measured round trip, in milliseconds, among the people probing. At create time that is only you; your bandmates' distances are not known yet. The probes time a TCP handshake against each region's public endpoints, which tracks the UDP path closely enough to rank regions. |
| hourly | The machine's current price per hour, fetched live where the provider offers it. |
| egress | What the provider charges per GB of outbound audio. |

Regions are sorted by worst round trip in 5 ms steps, with price breaking ties inside a step, best first. The top row is preselected; click another to override it (`--region` in the CLI).

A region under 30 ms from everyone keeps the network's share of latency in single digits each way, which is what makes the total playable.

If the band spans a continent, pick the region that is mediocre for everyone over the one that is perfect for you; the person with the worst round trip sets the feel. See [Troubleshooting](troubleshooting.md) for what the numbers mean in the ear.

## The cost preview and the seats

Step 3 shows the expected hours, the seat counts, and the resulting estimate, all editable in place:

![Wizard step 3 of 4, the cost preview with hours, seat counts, and the VM, egress, and credit lines](../images/wizard_preview.png)
*Step 3 of 4. The same lines `jamstream host` prints, from the same live prices.*

| Field | What it does |
|---|---|
| hours | Shapes the estimate only; the real bill is metered. Play longer and you pay for the time played. |
| musicians | Playing seats, counting you: 4 means your own seat plus three invites to hand out. The cap is 10, which is also what the server admits. |
| listeners | People who only hear the mix. |
| stream destinations | How many platforms you expect to broadcast to; it is the number here that moves the estimate most. It configures nothing: platforms are set up in the session itself, in [Streaming to Twitch and YouTube](streaming.md). |
| Recording | Off, the mix, or the mix and stems. It is the one choice here that is fixed for the session: a session launched with it off cannot record later. A cloud take needs a bucket, set up once in Settings; [Recording a session](recording.md) walks it. |

[Understanding cost](cost.md) explains every line of the estimate.

Clicking **Launch** boots the machine, waits for its address, proves the server answers a real encrypted handshake, and joins you.

## Invites are minted at launch

One invite per seat is minted up front, on your machine, and the wizard opens Settings on the Invites tab the moment you are in:

![Invites panel over the session screen: one seat per link, a freed seat reading was Ben with a New link button, and a seat count of 3 of 10 musicians](../images/session_invites.png)
*Each link admits one person. Copy, revoke, or mint more, and end the session for everyone from here.*

- Each row is one seat with a live status: `not joined`, `connected`, or `free`. **Copy link** puts that person's invite on the clipboard; send it to exactly one person over a channel you trust.
- Revoking a seat frees it: the row keeps the name it had, greyed, and **New link** mints a replacement into the same chair.
- **Revoke** ejects that member and kills their invite, with a confirmation step. The host also sees a Revoke button on each mixer strip.
- **Mint invite** adds a musician or listener seat mid-session, up to 10 musicians (you included) and 20 listeners. Unused invites cost nothing.
- The **for** field names the next link you mint. The name rides inside the invite, so the roster and any recorded stems say "Ana" from that person's first packet instead of "musician 2".
- An unused seat named this way reads `not joined, for Ana`. People can also set their own name when they join, which wins over the invite's.

See [Joining a session](joining.md) for how invites behave on the other end.

The CLI mints its seats at launch and cannot add more to a running session; the app's panel can, even for sessions the CLI hosted.

## The safety knobs

Every session carries two timers, both set at launch:

| Timer | Default | Wizard field | CLI flag | Range |
|---|---|---|---|---|
| Idle exit | 10 minutes | idle exit | `--idle-min` | 1 to 120 minutes in the wizard; any value from the CLI |
| Hard cap | 12 hours | hard cap | `--max-hours` | 1 to 24 hours in the wizard; any value from the CLI |

With no musicians connected for the idle window, the server shuts itself down. On DigitalOcean and AWS the machine is destroyed with it; on GCP it stops serving and is deleted when you end the session, on your next sweep, or at the hard cap.

The machine is destroyed at the hard cap no matter what, and invites expire with it.

There is no way to extend a running session; host a new one. The point of the caps is that a forgotten session costs a bounded, small amount, not a month of billing. [Understanding cost](cost.md) covers the other guardrails.

## While it runs

The **Broadcast** tab of Settings holds both halves of streaming: **Stream mix** sets what the platforms and the listeners hear, and **Destinations** puts the session on air to Twitch and YouTube Live.

**Record** is the one action in the bar itself, and [Recording a session](recording.md) covers takes.

Devices and buffer size can change mid-session from Settings in the top bar, and the change applies immediately; [Joining a session](joining.md#the-session-screen) walks the whole screen.

`jamstream status` lists the same sessions from the terminal, with elapsed time, cost accrued so far, and a projection.

If tagged machines already exist in your account when you host again, the CLI warns at host time, so a stray session does not hide behind a new one.

## Ending

**End session for everyone** on the Invites tab destroys the machine, confirms with the provider that nothing tagged with the session is still listed, and marks the local record ended, with a progress sheet until the provider confirms. The invites are dead from that moment.

Leaving is not ending: **Leave** disconnects you and the server keeps running until the host ends it or the idle exit fires.

The machine takes its own log with it, so the app keeps a copy of the last of it while the session runs, in `jamstream/sessions/logs/<id>.log` under your platform's data directory; `jamstream end` prints the path.

That file is where the reason a broadcast or a take failed is written down, and stream keys are stripped from it before it is written.

Closing the app window while your session runs asks the same question rather than deciding for you:

- end the session and quit
- keep it running and quit; the band plays on, and the server stops itself 10 minutes after the last musician leaves, at its hard cap, or with `jamstream end`
- cancel

No dialog appears when nothing you launched is running.

## From the terminal

The CLI runs the same flow without a screen:

```console
$ jamstream host --provider digitalocean
$ jamstream status
$ jamstream end 3f2a9c01        # any unambiguous prefix, or --last
```

`host` prints the region table and cost preview, asks for confirmation, and prints one invite per seat; `end` confirms with the provider that nothing tagged with the session is still listed. Every flag is in the [CLI reference](../cli/index.md).
