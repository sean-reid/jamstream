# jamstream status

List known sessions with elapsed time and accrued cost.

```text
Usage: jamstream status [OPTIONS]
```

Reads the local state files written by [`jamstream host`](host.md) and prints one row per session. Running sessions accrue by the second; ended ones show their final figures.

## Options

| Flag | Default | Meaning |
|---|---|---|
| `--hours <HOURS>` | `3` | Hours to project the total cost over. |
| `--json` | off | Emit a JSON array instead of a table. |

## Example

```console
$ jamstream status --hours 4
SESSION    PROVIDER/REGION      STATUS      ELAPSED      ACCRUED      PROJECTED
3f2a9c01   digitalocean/nyc3    running    1 h 04 min    $0.028576 $0.10716 at 4.0 h
b7e5c9b6   local/local          ended      2 h 13 min        $0.00              -
```

## Notes

- ACCRUED is the machine's hourly rate times elapsed time; egress is not included, so the provider's bill can be cents higher.
- PROJECTED extends the rate to the `--hours` horizon, for running sessions only.
- This reads local records; it does not call the provider. A machine destroyed behind JamStream's back still shows running here until [`jamstream end`](end.md) or [`jamstream sweep`](sweep.md) reconciles it.
