# jamstream status

List known sessions with elapsed time and accrued cost.

```text
Usage: jamstream status [OPTIONS]
```

Reads the local state files written by [`jamstream host`](host.md) and prints one row per session. Running sessions accrue by the second; ended ones show their final figures. A row recorded as running is checked against its provider first: if the instance is gone the row prints `stale` instead, with a pointer at [`jamstream end`](end.md) to close the record.

## Options

| Flag | Default | Meaning |
|---|---|---|
| `--hours <HOURS>` | `3` | Hours to project the total cost over. |
| `--json` | off | Emit a JSON array instead of a table. |

## Example

```console
$ jamstream status --hours 4
SESSION    PROVIDER/REGION      STATUS      ELAPSED      ACCRUED      PROJECTED TAKES
3f2a9c01   digitalocean/nyc3    running  1 h 04 min    $0.028576 $0.10716 at 4.0 h our-jams +stems
b7e5c9b6   local/local          ended    2 h 13 min        $0.00              - -
```

## Notes

- ACCRUED is the machine's hourly rate times elapsed time; egress is not included, so the provider's bill can be cents higher.
- PROJECTED extends the rate to the `--hours` horizon, for running sessions only.
- A provider that cannot be checked (credentials not in this shell, network down, one of its regions unreachable) proves nothing, so the row keeps its recorded status and a line under the table says it could not be checked.
- In `--json`, rows recorded as running carry `"corroborated"`, and `"status"` is `"stale"` when the provider no longer lists the instance. Nothing is rewritten on disk; [`jamstream end`](end.md) and [`jamstream sweep`](sweep.md) do that.
