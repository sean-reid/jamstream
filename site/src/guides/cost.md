# Understanding cost

A session costs machine time plus network traffic, paid to your cloud provider. Both are small; this page explains how the preview is computed, what egress is, and the guardrails that keep a mistake from costing more than a coffee.

Local sessions cost nothing: hosting on this computer (the wizard's local row, or `--provider local` in the CLI) rents no machine and meters no egress. The preview says so in one line, and the only guardrail a local session needs is the idle exit, since a forgotten process bills nobody. The rest of this page is about the cloud providers.

## The preview

Before anything launches, the app's wizard and `jamstream host` show the same preview:

```text
Cost preview for digitalocean nyc3 over 3.0 hours:
VM $0.02679/hr x 3.0 h                            $0.08037
Egress estimate 1.62 GB at $0.01/GB                $0.0162
Included egress credit (3000 GB free)             -$0.0162
Total (estimate)                                  $0.08037
```

Line by line:

- **VM** is the machine's hourly price times your expected length, set on the preview step (`--hours` in the CLI). The price is fetched live from the provider where possible (DigitalOcean's sizes API) and from a bundled snapshot of public pricing otherwise, so the preview tracks reality and the numbers in this documentation are the approximations.
- **Egress estimate** is predicted outbound traffic times the provider's per-GB rate. Each musician's personal mix streams down at about 300 kbit/s, each listener at about 150 kbit/s, and each broadcast destination at 2628 kbit/s. Four musicians for three hours is about 1.6 GB; add a Twitch or YouTube destination and it is about 5.2 GB.
- **Included egress credit** appears when the provider bundles free transfer that covers some or all of the estimate. DigitalOcean droplets include thousands of GiB; AWS accounts include 100 GB per month; GCP includes close to nothing.

The expected length only shapes the estimate. The real bill is metered: elapsed time times the hourly rate, plus measured traffic. Play four hours after previewing three and you pay for four.

## What egress is

Cloud providers charge for data leaving their network, per gigabyte, and call it egress. Inbound is free. For JamStream that outbound data is the mixes the server sends to each member, so egress scales with people and hours, not with how loud you play. At DigitalOcean and AWS the included allowances mean a normal session's egress costs $0. On GCP it is real money but small: about $0.06 per musician per three hours.

Broadcasting is the exception: one Twitch or YouTube destination is about 1.2 GB per hour, more than four musicians' audio combined. Two of them for three hours is about 7 GB, still inside DigitalOcean's and AWS's allowances, and about $0.80 on GCP. See [Streaming to Twitch and YouTube](streaming.md).

## The guardrails

Ephemeral cloud machines have one classic failure: you forget one and it bills for a month. JamStream treats that as a design problem, in layers:

- **The self-destruct timers.** Every server shuts itself down after `--idle-min` minutes with no musicians (default 10) and destroys itself at the `--max-hours` hard cap (default 12) no matter what. The cap is enforced on the machine itself, not by your laptop, so it works even if your laptop is in a lake.
- **The cost ticker.** While a session runs, cost so far sits in the app's status bar next to latency, and `jamstream status` prints accrued and projected cost per session. You always know the meter's reading; nothing accrues silently.
- **The sweeper.** Every machine JamStream launches is tagged. `jamstream sweep` finds everything with the tag across all configured providers and destroys it; `--dry-run` lists without destroying. `jamstream host` also warns you at launch if tagged machines already exist. When in doubt, sweep.

The worst case, all guardrails ignored and a session forgotten mid-jam, is 12 hours of a small machine: about $0.32 on DigitalOcean, about $0.40 on AWS or GCP, plus cents of egress.

## Checking the meter

```console
$ jamstream status
SESSION    PROVIDER/REGION      STATUS      ELAPSED      ACCRUED      PROJECTED
3f2a9c01   digitalocean/nyc3    running    1 h 04 min    $0.028576 $0.08037 at 3.0 h
b7e5c9b6   local/local          ended      2 h 13 min        $0.00              -
```

Accrued is hourly rate times elapsed time; it stops when the session ends. The state files behind this table live on your machine, one JSON file per session, under your platform's data directory in `jamstream/sessions/`.
