# CLI reference

```text
Usage: jamstream <COMMAND>
```

The `jamstream` CLI is built for automation, scripting, and headless use.

It hosts, monitors, and ends sessions unattended, joins them without a display, and reads and writes the same session state files the app does, so either tool can watch or end what the other started.

People playing music want the app and the [quickstart](../quickstart.md); the pages here document every command and flag.

| Command | What it does |
|---|---|
| [`jamstream host`](host.md) | Provision a session server and mint invites. |
| [`jamstream status`](status.md) | List known sessions with elapsed time and accrued cost. |
| [`jamstream end`](end.md) | Destroy a session's server and mark the session ended. |
| [`jamstream sweep`](sweep.md) | Find and destroy orphaned jamstream instances. |
| [`jamstream join`](join.md) | Join a session as a headless client. |
| [`jamstream recordings`](recordings.md) | List and fetch the takes a session recorded to a bucket. |
| [`jamstream completions`](completions.md) | Print shell completions for jamstream. |
