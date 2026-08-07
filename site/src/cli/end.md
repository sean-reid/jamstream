# jamstream end

Destroy a session's server and mark the session ended.

```text
Usage: jamstream end [OPTIONS] [SESSION]
```

Destroys the machine, confirms with the provider that nothing tagged with the session is still listed, and rewrites the local state file as ended. Pass a session id prefix, or `--last`; one of the two is required.

## Arguments and options

| Flag | Meaning |
|---|---|
| `[SESSION]` | Session id prefix of the session to end. Any unambiguous prefix works; only running sessions match. |
| `--last` | End the most recently created running session. Conflicts with a prefix. |

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

- If the provider still lists instances for the session after the destroy call, `end` fails loudly and points you at [`jamstream sweep`](sweep.md); it never silently leaves something billing.
- An ambiguous prefix lists nothing and asks for more characters; run [`jamstream status`](status.md) to see the ids.
