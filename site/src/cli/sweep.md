# jamstream sweep

Find and destroy orphaned jamstream instances.

```text
Usage: jamstream sweep [OPTIONS]
```

Every machine JamStream launches carries a `jamstream` tag with its session id. Sweep lists everything with that tag, across every provider whose credentials are in the environment, and destroys it. Local sessions need no credentials and are always included, so a stray server process on this computer is found the same way. This is the backstop for crashed sessions, lost laptops, and anything else that slipped past [`jamstream end`](end.md).

## Options

| Flag | Meaning |
|---|---|
| `--dry-run` | Report what would be destroyed without destroying anything. |
| `--provider <PROVIDER>` | Sweep one provider instead of every configured provider. |

## Example

```console
$ jamstream sweep --dry-run
PROVIDER       REGION         INSTANCE         RESULT
digitalocean   nyc3           512190713        would destroy
1 found, 0 destroyed, 0 failed.

$ jamstream sweep
PROVIDER       REGION         INSTANCE         RESULT
digitalocean   nyc3           512190713        destroyed
1 found, 1 destroyed, 0 failed.
```

A clean account prints one line:

```console
$ jamstream sweep --dry-run
No jamstream-tagged instances found.
```

## Notes

- `sweep --dry-run --provider <name>` doubles as the credential check for a newly configured provider; see [Provider setup](../guides/providers.md#verifying-any-provider).
- Sweep destroys by tag, so it also catches machines from other computers and old versions, and nothing untagged is ever touched.
- Sweep also closes this machine's record of any session whose instance it destroyed or found already gone, so [`jamstream status`](status.md) stops reporting it as running. Records on a provider that could not be searched are left alone.
- If any destroy fails, sweep exits nonzero and says so plainly, because the failed instance is still billing. Destroy it from the provider's console and report the bug.
