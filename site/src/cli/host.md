# jamstream host

Provision a session server and mint invites.

```text
Usage: jamstream host [OPTIONS]
```

Ranks the provider's regions by measured latency and price, shows a cost preview, asks for confirmation, launches the machine, verifies it answers a real encrypted handshake, prints one invite per seat, and records the session in a local state file.

## Options

| Flag | Default | Meaning |
|---|---|---|
| `--provider <PROVIDER>` | `mock` | Cloud provider to host on: `mock`, `aws`, `digitalocean`, or `gcp`. The mock runs the whole flow with no credentials and no real machine. |
| `--region <REGION>` | picked by ranking | Region id to use, skipping the latency ranking. |
| `--musicians <MUSICIANS>` | `4` | Musician invites to mint, not counting the host. 1 to 10. |
| `--listeners <LISTENERS>` | `0` | Listener invites to mint. 0 to 20. |
| `--hours <HOURS>` | `3` | Expected session length in hours, for the cost preview. Does not limit the session. |
| `--destinations <DESTINATIONS>` | `0` | Stream destination count, for the egress estimate. |
| `--port <PORT>` | `43210` | UDP port the session server listens on. |
| `--idle-min <IDLE_MIN>` | `10` | Minutes without musicians before the server shuts itself down. |
| `--max-hours <MAX_HOURS>` | `12` | Hard cap on session length in hours. Invites expire at the cap. |
| `--artifact-url <ARTIFACT_URL>` | none | URL of the `jamstreamd` artifact the VM downloads at boot. Required for real providers until releases are published. |
| `--artifact-sha256 <ARTIFACT_SHA256>` | none | Expected sha256 of the `jamstreamd` artifact. Required alongside `--artifact-url`. |
| `--yes` | off | Skip the launch confirmation. |
| `--json` | off | Emit one JSON object instead of human-readable output. |

## Example

A two hour duo session with two spare listener seats, on DigitalOcean, unattended:

```console
$ jamstream host --provider digitalocean --musicians 1 --listeners 2 \
    --hours 2 --yes \
    --artifact-url https://your-host.example/jamstreamd \
    --artifact-sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
```

Full output of a host run is shown in the [quickstart](../quickstart.md#4-host).

## Notes

- If JamStream-tagged machines already exist on the provider, they are listed with a warning before anything launches; run [`jamstream sweep`](sweep.md) if they are strays.
- `--json` prints the session id, address, invites, cost estimate, and state file path as one object, for scripts.
- The state file lands under your platform's data directory in `jamstream/sessions/` and is what [`jamstream status`](status.md) and [`jamstream end`](end.md) read.
