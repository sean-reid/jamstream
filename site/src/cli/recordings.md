# jamstream recordings

List and fetch the takes a session recorded to a bucket.

```text
Usage: jamstream recordings [OPTIONS] [COMMAND]
       jamstream recordings get [OPTIONS] <SESSION>
```

A cloud session records into your own bucket, because the machine deletes itself at the end and a take on its disk goes with it. This is how the takes come back out. Local sessions need none of this: their takes are already on this computer, in the directory [`jamstream host`](host.md) printed.

Both forms read the bucket details [`jamstream host`](host.md) saved beside each session record when the session was launched with a bucket, and the storage key from the environment.

## Options

| Flag | Default | Meaning |
|---|---|---|
| `--json` | off | Emit a JSON array instead of a table. |

## jamstream recordings get

| Flag | Default | Meaning |
|---|---|---|
| `<SESSION>` | required | Session id prefix of the session whose takes to download. Any unambiguous prefix works. |
| `--out <OUT>` | current directory | Directory to write the takes into. |
| `--yes` | off | Skip the download confirmation, which is where the egress cost is shown. |

## Listing

```console
$ jamstream recordings
SESSION    TAKE                                           SIZE  MODIFIED
3f2a9c01   mix.flac                                    1.38 GB  2026-07-28 19:30
3f2a9c01   stems/bass.flac                            691.2 MB  2026-07-28 19:30
3f2a9c01   stems/drums.flac                           691.2 MB  2026-07-28 19:31

Fetch a session's takes with: jamstream recordings get <session>
```

A session that recorded nothing says so on its own line rather than showing an empty table.

## Fetching, and what it costs

Downloading is the one part of recording that costs money after the session has already been paid for: the bucket bills egress on every byte that leaves it. So the size and the price come first, and nothing moves until you say yes.

```console
$ jamstream recordings get 3f2a9c01 --out ~/takes
Session 3f2a9c01 recorded 3 takes in my-jams (aws/eu-west-1), delete after 30 days.
Download 2.76 GB at $0.09/GB                    $0.248832
Egress is billed on the download, not on the recording.
Your plan includes 100 GB/month of free download, so this is an upper bound.
Billed to your own cloud account at list prices; JamStream never sees it.
Download these takes? [y/N] y
  mix.flac                                 100%
  stems/bass.flac                          100%
  stems/drums.flac                         100%
3 takes in /Users/you/takes, 2.76 GB.
Egress for this download: $0.248832.
```

Pass `--yes` in a script. Progress is whole lines at fixed percentages, so a log reads the same as a terminal.

## The storage key

The object stores want an access key pair, which is not the token that launches machines. Set the pair for the provider holding the bucket:

| Provider | Variables |
|---|---|
| AWS | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` |
| DigitalOcean | `SPACES_ACCESS_KEY_ID`, `SPACES_SECRET_ACCESS_KEY` |
| GCP | `GCS_ACCESS_KEY_ID`, `GCS_SECRET_ACCESS_KEY` |

These are the same keys you set to launch a recorded session, so a host reading takes back on their own machine has nothing new to configure. The key is never written to disk: only the bucket, region, and retention are kept beside the session record.

## Notes

- Takes outlive the session. A session ended weeks ago still lists, until the bucket's retention rule deletes the objects.
- Every take is streamed to disk, never held in memory, and what lands is checked against the size the bucket listed. A file that arrives short is deleted rather than left looking like a recording.
- A take already in the output directory at the right size is skipped and costs no egress. One at a different size stops the download instead of being overwritten.
- If a bucket cannot be reached, that session's line says why and the other sessions still list, but the command exits nonzero.
