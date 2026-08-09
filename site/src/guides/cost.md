# Understanding cost

A session costs machine time plus network traffic, paid to your cloud provider. Both are small.

This page covers how the preview is computed, what egress is, and the guardrails that keep a mistake from costing more than a coffee.

Local sessions cost nothing: hosting on this computer (the wizard's local row, or `--provider local` in the CLI) rents no machine and meters no egress. The preview says so in one line.

The only guardrail a local session needs is the idle exit, since a forgotten process bills nobody. The rest of this page is about the cloud providers.

## The preview

Before anything launches, the app's wizard and `jamstream host` show the same preview:

```text
Cost preview for digitalocean nyc3 over 3.0 hours:
VM $0.02679/hr x 3.0 h                            $0.08037
Egress estimate 1.62 GB at $0.01/GB                $0.0162
Included egress credit (3000 GB free)             -$0.0162
Total (estimate)                                  $0.08037
```

| Line | What it means |
|---|---|
| **VM** | Hourly price times your expected length, set on the preview step (`--hours` in the CLI). Fetched live from the provider where possible (DigitalOcean's sizes API), otherwise from a bundled snapshot of public pricing, so the numbers here are approximations. |
| **Egress estimate** | Predicted outbound traffic times the provider's per-GB rate. |
| **Included egress credit** | Free transfer the provider bundles in, when it covers some or all of the estimate. |

| Provider | Included egress |
|---|---|
| DigitalOcean droplets | thousands of GB |
| AWS | 100 GB per month |
| GCP | close to nothing |

Four musicians for three hours is about 1.6 GB; add a Twitch or YouTube destination and it is about 5.2 GB.

The expected length only shapes the estimate. The real bill is metered: elapsed time times the hourly rate, plus measured traffic. Play four hours after previewing three and you pay for four.

## What egress is

Cloud providers charge for data leaving their network, per gigabyte, and call it egress. Inbound is free.

For JamStream that outbound data is the mixes the server sends to each member, so egress scales with people and hours, not with how loud you play.

| Provider | A normal session's egress |
|---|---|
| DigitalOcean | $0, inside the included allowance |
| AWS | $0, inside the included allowance |
| GCP | about $0.05 per musician per three hours |

Broadcasting is the exception: one Twitch or YouTube destination is about 1.2 GB per hour, more than four musicians' audio combined.

| Two broadcast destinations, three hours (about 7 GB) | Cost |
|---|---|
| DigitalOcean / AWS | $0, inside the included allowance |
| GCP | about $0.80 |

See [Streaming to Twitch and YouTube](streaming.md).

## Recording

[Recording a session](recording.md) on your own computer costs nothing but disk.

| Local disk | Size per hour |
|---|---|
| Mix only | about 0.4 GB |
| With stems (four piece) | about 2 GB |

A cloud session records to a bucket, which adds two charges: storage now, and download egress later.

| A three hour take | Size |
|---|---|
| Mix only | about 1.2 GB |
| With stems | about 6 GB |

Storage is the small one:

| Storage | Price | 30 days of stems, the default retention (~6 GB) |
|---|---|---|
| S3 | $0.023/GB-month | about $0.14 |
| Cloud Storage | about $0.02/GB-month | about $0.12 |
| DigitalOcean Spaces | $5/month flat, includes 250 GB | included |

A Spaces subscription already covers a take at that price. Uploading itself costs nothing, since the machine and the bucket sit at the same provider.

Both lines are in the preview before you launch, in the wizard and in `jamstream host --bucket`, and they move when you switch between the mix and stems.

**The egress lands on the download.** That is the one cost in JamStream that arrives after a session has finished pricing itself, which is why `jamstream recordings get` prints the figure and waits for a yes before it moves a byte.

| Downloading 6 GB of stems | Cost |
|---|---|
| S3 | about $0.56 |
| Cloud Storage | about $0.75 |
| DigitalOcean Spaces | free, inside the 1 TB included |

## The guardrails

Three, all on by default.

| Guardrail | Default | What it does |
|---|---|---|
| Idle timeout (`--idle-min`) | 10 minutes | Shuts the server down once no musicians are connected. |
| Hard cap (`--max-hours`) | 12 hours | Destroys the server no matter what, enforced on the machine itself, not by your laptop. |

GCP is the exception, and it costs money if you walk away. There the idle window stops the server but cannot delete the machine, so an abandoned session keeps billing until the hard cap.

Roughly $0.39 on an e2-medium if everyone leaves at twenty minutes and nobody comes back. End the session when you are done, or press **Stop strays** afterwards, and you pay for the time you played. Shortening `--max-hours` bounds it too.

**The cost ticker.** While a session runs, cost so far and elapsed time sit at the right-hand end of the app's status bar, beside **Record** and **Leave**, and `jamstream status` prints accrued and projected cost per session. You always know the meter's reading; nothing accrues silently.

**The sweeper.** Every machine JamStream launches is tagged. **Stop strays**, on the app's Recent sessions card, finds everything with the tag across every account this computer holds a key for and destroys it, then says what it could not account for.

`jamstream sweep` does the same from a terminal, with `--dry-run` to list without destroying. `jamstream host` also warns you at launch if tagged machines already exist. When in doubt, sweep.

| 12-hour walkaway, all guardrails ignored | Cost |
|---|---|
| DigitalOcean | about $0.32 |
| AWS | about $0.40 |
| GCP | about $0.40 |

Plus cents of egress.

## Checking the meter

```console
$ jamstream status
SESSION    PROVIDER/REGION      STATUS      ELAPSED      ACCRUED      PROJECTED TAKES
3f2a9c01   digitalocean/nyc3    running   1 h 04 min    $0.028576 $0.08037 at 3.0 h our-jams +stems
b7e5c9b6   local/local          ended     2 h 13 min        $0.00              - -
```

Accrued is hourly rate times elapsed time; it stops when the session ends. TAKES is the bucket the session recorded to, if it recorded to one; a take on your own disk shows a dash and lives in the folder [Recording a session](recording.md#on-this-computer) names.
