# Recording a session

A take is the mix your listeners heard, written as 16 bit 48 kHz FLAC.

Nothing is captured by surprise. Recording is armed at launch, and a take runs only while the host holds it open: each Record to Stop is one take.

## Arming it at launch

Two things are fixed before anyone plays and cannot change once the session is running: whether the session can record at all, and whether stems are captured alongside the mix.

A session on your own computer records to your own disk and needs no account and no credential:

```console
$ jamstream host --provider local --record --yes
```

A cloud session records to a bucket in your own account, because the machine deletes itself when the session ends and a take on its disk goes with it. Name the bucket with `--bucket`, which implies `--record`, after setting a storage key in the environment; [`jamstream recordings`](../cli/recordings.md#the-storage-key) names the two variables for each provider.

```console
$ jamstream host --provider aws --region eu-west-1 --bucket my-jams --yes
```

The launch proves that key can write the session's own prefix and puts the retention rule in place before the machine is paid for, so a bucket that refuses fails while you are still configuring rather than mid-song. `--retention` keeps takes for 7, 30 or 90 days, or forever. The default is 30 days, and it is a rule on the bucket itself, so it keeps being enforced long after the machine is gone.

Stems are `--record-stems`, which implies `--record` and works either way. Every flag is in the [host reference](../cli/host.md).

Arming a cloud session is a terminal job today: the app's wizard cannot point a session at a bucket, though a session the CLI launched records normally once you join it in the app.

A session launched without recording cannot be talked into it later. Press Record on one and it answers `recording is not configured for this session` straight away, so you find out before the song rather than after it.

## Starting a take

**Record** in the session's status bar opens the Record sheet. Only the host has either.

The sheet shows the take's state, a line saying whether stems are being captured, and one button: **Record** to start a take, **Stop** to end it. Closing the sheet does not stop a take, and neither does leaving the session; a take stops when the host presses Stop or when the session ends.

While a take runs, **REC** lights red in the middle of the bar for everyone in the session, beside ON AIR. Hover it and it says the session is being recorded. Nothing lights while the recorder is idle.

## Where takes land

Files carry the take's date and time in UTC, and stems carry the player's name:

```text
jamstream-2026-07-28-1930-mix.flac
jamstream-2026-07-28-1930-Ana.flac
```

### On this computer

In a `recordings` folder under your platform's data directory, printed at launch:

```console
$ jamstream host --provider local --record --yes

Session 3f2a9c01 is running.
server       192.168.1.12:43210
record dir   /Users/you/Library/Application Support/jamstream/recordings
host         jamstream://join/r6edH1LCtlT3vPPiILRRVAEACgAAAcrRAjiV...
```

| Platform | Folder |
|---|---|
| macOS | `~/Library/Application Support/jamstream/recordings` |
| Linux | `~/.local/share/jamstream/recordings`, or under `$XDG_DATA_HOME` |
| Windows | `%LOCALAPPDATA%\jamstream\recordings` |

`jamstream end` never removes a recording. Nothing but you deletes a take.

A take still being written ends in `.part` and is renamed when it finishes, so a laptop that dies mid-song leaves a file that does not look like a finished recording.

### In a bucket

Under `jamstream/recordings/` and the session id, in the bucket you named. The take uploads while you play, so ending the session waits only for the last of it. Let the `UPLOADING` lamp clear before you end the session: the machine holds on for ten minutes to finish an upload and then shuts down regardless, and a take still in flight at that point is lost.

[`jamstream recordings`](../cli/recordings.md) lists what each session recorded and fetches it, and takes outlive the session: one that ended weeks ago still lists until the retention rule deletes it. Downloading is where recording costs money, so the command prices the egress and waits for a yes before it moves a byte.

## The mix, and stems

Without stems, a take is one stereo file. With stems it is one stereo file per musician as well, each carrying that player's own signal. The sheet reads back which of the two you launched with.

Stems are stereo rather than mono, so a stem is the same size as the mix: stems turn a 1.1 GB three hour take into about 5.5 GB for a four piece. Every file in a take starts at the same zero, so they line up when you import them.

## What the room sees

| The sheet reads | The bar shows | What it means |
|---|---|---|
| `idle` | nothing | armed, with no take running |
| `recording` | `REC` in red | a take is running |
| `uploading` | `UPLOADING` in amber | the take ended and the last of it is still going to the bucket. Record comes back when it clears |
| `failed` | `REC FAILED` in red | the take stopped, with the reason |

A take on your own disk is finished the moment you press Stop, so it never reads `uploading`.

## When it fails

The reason is in the sheet and on the lamp's hover, verbatim, for everyone in the room. A failure ends the take, not the session: press Record again for the next one.

| Reason begins | What to do |
|---|---|
| `recording is not configured for this session` | the session was launched without `--record` or `--bucket`; host a new one with it |
| `cannot start the recorder`, `cannot open the mix file` | the folder, or the first object in the bucket, could not be created. For a local session, check that the printed path exists and is writable |
| `recording failed` | a write failed mid-take: a full disk, or a bucket that stopped accepting the upload. The take is abandoned rather than left half written |
| `recording could not be finished` | the end of the take could not be written. Earlier takes in the session are unaffected |

## What it costs

A local take costs disk and nothing else. A take in a bucket costs a few cents of storage while it sits there, and egress when somebody downloads it. [Understanding cost](cost.md#recording) has the numbers.
