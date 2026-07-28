# jamstream host

Provision a session server and mint invites.

```text
Usage: jamstream host [OPTIONS]
```

Ranks the provider's regions by measured latency and price, shows a cost preview, asks for confirmation, launches the machine, verifies it answers a real encrypted handshake, prints one invite per seat, and records the session in a local state file.

With `--provider local` (the default) there is nothing to rank or bill: the server starts as a process on this computer, the cost line reads that local sessions cost nothing, and the handshake check still runs against the real server. See [Playing on the same network](../guides/local.md).

## Options

| Flag | Default | Meaning |
|---|---|---|
| `--provider <PROVIDER>` | `local` | Provider to host on: `local`, `digitalocean`, `aws`, or `gcp`. Local runs the server on this computer, with no credentials and no cost. |
| `--region <REGION>` | picked by ranking | Region id to use, skipping the latency ranking. Local has one region, `local`. |
| `--musicians <MUSICIANS>` | `4` | Musician seats in the session, **counting you**. 1 to 10, where 1 hosts alone and 10 is the server's capacity. `--musicians 4` mints your host invite plus 3 musician invites. |
| `--listeners <LISTENERS>` | `0` | Listener seats in the session; one listener invite is minted per seat. 0 to 20. |
| `--hours <HOURS>` | `3` | Expected session length in hours, for the cost preview. Does not limit the session. |
| `--destinations <DESTINATIONS>` | `0` | Stream destination count, for the egress estimate. |
| `--port <PORT>` | `43210` | UDP port the session server listens on. |
| `--idle-min <IDLE_MIN>` | `10` | Minutes without musicians before the server shuts itself down. |
| `--max-hours <MAX_HOURS>` | `12` | Hard cap on session length in hours. Invites expire at the cap. |
| `--record` | off | Let this session record. A local session's takes land as FLAC files in a recordings folder on this computer and the launch output prints its path; a cloud session needs `--bucket`, because the machine deletes itself at the end and a take on its disk goes with it. |
| `--record-stems` | off | Also capture a stereo stem per musician alongside the mix, named for them. Implies `--record`. |
| `--bucket <BUCKET>` | none | Bucket a cloud session records to, in the session's own region. Implies `--record`. The launch writes a probe object to prove the key can write, applies the retention rule, and saves the bucket beside the session record so [`jamstream recordings`](recordings.md) can find the takes later. Needs a storage key in the environment; see that page for the variables. |
| `--retention <RETENTION>` | `30d` | How long the bucket keeps this session's takes: `7d`, `30d`, `90d`, or `forever`. Applies to `--bucket`, and is enforced by the bucket's own lifecycle rule, so it keeps working after the machine is gone. |
| `--artifact-url <ARTIFACT_URL>` | pinned into release builds | Override the URL of the `jamstreamd` artifact the VM downloads at boot. Release builds carry the release's own server build pinned in, so cloud hosting normally needs no flag; a source build has no pin and must pass both artifact flags to host on a cloud provider. Local mode runs a binary already on this machine and downloads nothing. |
| `--artifact-sha256 <ARTIFACT_SHA256>` | pinned into release builds | Override the expected sha256 of the `jamstreamd` artifact. Must be passed together with `--artifact-url`; the VM refuses to start on a mismatch. |
| `--yes` | off | Skip the launch confirmation. |
| `--json` | off | Emit one JSON object instead of human-readable output. |

## Examples

A session on this computer, for the people in the room:

```console
$ jamstream host --provider local --yes
```

The same session able to record, with the folder takes land in printed at launch:

```console
$ jamstream host --record --yes
```

A two hour duo session (you and one other player) with two spare listener seats, on DigitalOcean, unattended:

```console
$ jamstream host --provider digitalocean --musicians 2 --listeners 2 \
    --hours 2 --yes
```

The same launch from a source build, which has no pinned server artifact, names one explicitly:

```console
$ jamstream host --provider digitalocean --musicians 2 --listeners 2 \
    --hours 2 --yes \
    --artifact-url https://your-host.example/jamstreamd \
    --artifact-sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
```

The [quickstart](../quickstart.md#from-the-terminal) shows a full run end to end.

## Notes

- `--musicians` counts the host. A session of N musician seats is you plus N-1 guests, and N is also what the server admits: it refuses an eleventh musician, so there is never an invite in hand that cannot get in. The desktop app's host wizard offers the same range with the same meaning, and its "musicians, including you" dial defaults to the same 4.
- If JamStream-tagged machines already exist on the provider, they are listed with a warning before anything launches; run [`jamstream sweep`](sweep.md) if they are strays.
- `--record` arms recording, it does not start it: the host presses Record in the session, and each Record to Stop is one take. Finished takes are FLAC files named by date and time in the printed folder (`record_dir` in the `--json` output), and they stay there after `jamstream end`. See [Recording a session](../guides/recording.md).
- `--json` prints the session id, address, invites, cost estimate, recording folder, and state file path as one object, for scripts.
- The state file lands under your platform's data directory in `jamstream/sessions/` and is what [`jamstream status`](status.md) and [`jamstream end`](end.md) read.
