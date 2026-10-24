# jamstream end

Destroy a session's server and mark the session ended.

```text
Usage: jamstream end [OPTIONS] [SESSION]
```

Destroys the machine, confirms with the provider that nothing tagged with the session is still listed, and rewrites the local state file as ended. Pass a session id prefix, or `--last`; one of the two is required.

Before destroying anything it asks the server whether a take is recording or uploading, through the session's host seat, and refuses if one is: destroying the machine loses the take. Stop the take in the app and let `UPLOADING` clear, then run it again, or pass `--force`.

## Arguments and options

| Flag | Meaning |
|---|---|
| `[SESSION]` | Session id prefix of the session to end. Any unambiguous prefix works; only running sessions match. |
| `--last` | End the most recently created running session. Conflicts with a prefix. |
| `--force` | End even if a take is recording or uploading, losing it, or when the host is connected from the app and the take state cannot be checked from here. |

## Example

```console
$ jamstream end 3f2a9c01
Session 3f2a9c01 ended. Instance 512190713 is destroyed.
```

Or, when only one session is running:

```console
$ jamstream end --last
```

## Notes

- If the machine is already gone (it idled out, hit its hard cap, or was swept), `end` says so and still marks the session ended:

  ```text
  Instance 512190713 was already gone; marking the session ended.
  ```

- If the app kept a copy of the session server's log, `end` names the file. The machine's own journal is destroyed along with the machine, so that copy is the only place a failed broadcast or take is explained:

  ```text
  The session server's log is at ~/.local/share/jamstream/sessions/logs/3f2a9c01....log.
  ```

- While the host is connected from the app, the host seat is taken and `end` cannot read the take state. It refuses and points at the app, whose End session button makes the same check; `--force` ends regardless.
- A server that does not answer is not a refusal. The machine is usually already gone, so `end` says it could not check and goes on.
- If the provider still lists instances for the session after the destroy call, `end` fails loudly and points you at [`jamstream sweep`](sweep.md); it never silently leaves something billing.
- An ambiguous prefix lists nothing and asks for more characters; run [`jamstream status`](status.md) to see the ids.
