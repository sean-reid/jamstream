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
- **Egress estimate** is predicted outbound traffic times the provider's per-GB rate. Four musicians for three hours is about 1.6 GB; add a Twitch or YouTube destination and it is about 5.2 GB.
- **Included egress credit** appears when the provider bundles free transfer that covers some or all of the estimate. DigitalOcean droplets include thousands of GB; AWS accounts include 100 GB per month; GCP includes close to nothing.

The expected length only shapes the estimate. The real bill is metered: elapsed time times the hourly rate, plus measured traffic. Play four hours after previewing three and you pay for four.

## What egress is

Cloud providers charge for data leaving their network, per gigabyte, and call it egress. Inbound is free. For JamStream that outbound data is the mixes the server sends to each member, so egress scales with people and hours, not with how loud you play. At DigitalOcean and AWS the included allowances mean a normal session's egress costs $0. On GCP it is real money but small: about $0.05 per musician per three hours.

Broadcasting is the exception: one Twitch or YouTube destination is about 1.2 GB per hour, more than four musicians' audio combined. Two of them for three hours is about 7 GB, still inside DigitalOcean's and AWS's allowances, and about $0.80 on GCP. See [Streaming to Twitch and YouTube](streaming.md).

## Recording

[Recording a session](recording.md) on your own computer costs nothing but disk: about 0.4 GB per hour for the mix, or about 2 GB per hour with stems for a four piece.

A cloud session records to a bucket, which adds two charges. Storage is the small one. A three hour take is about 1.2 GB for the mix or about 6 GB with stems, and at $0.023 per GB-month on S3 and about $0.02 on Cloud Storage, keeping the stems for the default 30 days is about $0.14. A DigitalOcean Spaces subscription is $5 a month including 250 GB, so a take fits inside what you already pay. Uploading costs nothing, because the machine and the bucket are at the same provider.

Both lines are in the preview before you launch, in the wizard and in `jamstream host --bucket`, and they move when you switch between the mix and stems.

**The egress lands on the download.** Pulling 6 GB of stems out of S3 is about $0.56, out of Cloud Storage about $0.75, and free inside the 1 TB Spaces includes. That is the one cost in JamStream that arrives after a session has finished pricing itself, which is why `jamstream recordings get` prints the figure and waits for a yes before it moves a byte.

## The guardrails

Three, all on by default:

- **The self-destruct timers.** Every server shuts itself down after `--idle-min` minutes with no musicians (default 10) and destroys itself at the `--max-hours` hard cap (default 12) no matter what. The cap is enforced on the machine itself, not by your laptop.

  GCP is the exception, and it costs money if you walk away. There the idle window stops the server but cannot delete the machine, so an abandoned session keeps billing until the hard cap: roughly $0.39 on an e2-medium if everyone leaves at twenty minutes and nobody comes back. End the session when you are done, or run `jamstream sweep` afterwards, and you pay for the time you played. Shortening `--max-hours` bounds it too.
- **The cost ticker.** While a session runs, cost so far and elapsed time sit at the right-hand end of the app's status bar, beside **Record** and **Leave**, and `jamstream status` prints accrued and projected cost per session. You always know the meter's reading; nothing accrues silently.
- **The sweeper.** Every machine JamStream launches is tagged. `jamstream sweep` finds everything with the tag across all configured providers and destroys it; `--dry-run` lists without destroying. `jamstream host` also warns you at launch if tagged machines already exist. When in doubt, sweep.

The worst case, all guardrails ignored and a session forgotten mid-jam, is 12 hours of a small machine: about $0.32 on DigitalOcean, about $0.40 on AWS or GCP, plus cents of egress.

## Checking the meter

```console
$ jamstream status
SESSION    PROVIDER/REGION      STATUS      ELAPSED      ACCRUED      PROJECTED TAKES
3f2a9c01   digitalocean/nyc3    running   1 h 04 min    $0.028576 $0.08037 at 3.0 h our-jams +stems
b7e5c9b6   local/local          ended     2 h 13 min        $0.00              - -
```

Accrued is hourly rate times elapsed time; it stops when the session ends. TAKES is the bucket the session recorded to, if it recorded to one; a take on your own disk shows a dash and lives in the folder [Recording a session](recording.md#on-this-computer) names.
