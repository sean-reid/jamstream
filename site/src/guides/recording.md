# Recording a session

A take is the mix your listeners heard, written as 16 bit 48 kHz FLAC.

Nothing is captured by surprise. Recording is armed at launch, and a take runs only while the host holds it open: each Record to Stop is one take.

## Set up a bucket once

A cloud session records to a bucket in your own account, because the machine deletes itself when the session ends and a take on its disk goes with it. Open **Settings**, then **Recording**:

![The Recording tab in the settings drawer: provider rows, the bucket and its region, two masked key fields, and Check](../images/session_settings_recording.png)
*Set up once per computer. The key is masked and never shown again.*

1. Pick the provider holding the bucket, and name the bucket and the region it is in. Host in that region and the upload costs nothing.
2. Paste the storage key pair. **This is not the credential that launches machines**; the last section of your [provider's page](providers.md) creates it.
3. Click **Check**. It writes one small object to the bucket and deletes it. A pass saves the key in your system keychain and says so; a failure shows the bucket's own reason and saves nothing, so a wrong key fails while you are pasting rather than mid-song.
4. **Keep takes for** is the default retention for new sessions: 7, 30 or 90 days, or forever. The default is 30 days, and it is a rule on the bucket itself, so it keeps being enforced long after the machine is gone.

A session on your own computer records to your own disk and needs none of this.

## Arm it at launch

Whether a session can record, and whether stems are captured alongside the mix, are fixed before anyone plays and cannot change once the session is running. Both are on the host wizard's cost preview:

![Wizard step 3 of 4 with mix and stems selected, the bucket named under it, and the recording lines in the estimate](../images/wizard_preview_recording.png)
*Off, the mix, or the mix and stems, with what each costs before you launch.*

- **off** is where every launch starts.
- **mix only** captures the stereo mix listeners hear: about 1.2 GB for three hours.
- **mix and stems** adds one stereo file per musician, about five times the bytes for a four piece. The size sits beside each row and the estimate below moves as you pick, because that is the moment the difference matters.

With no bucket set up, the two recording rows are disabled and say so, pointing at the Recording tab. A local session has no such requirement: the rows are live and the takes land on this computer.

Launching proves the key can write this session's own prefix and puts the retention rule in place before the machine is paid for.

## From the terminal

`--record` records a local session; `--bucket` names a bucket and implies it. The CLI reads the storage key from the environment rather than the keychain, and [`jamstream recordings`](../cli/recordings.md#the-storage-key) names the two variables per provider.

```console
$ jamstream host --provider local --record --yes
$ jamstream host --provider aws --region eu-west-1 --bucket my-jams --yes
```

`--record-stems` captures stems and implies `--record`; `--retention` takes 7d, 30d, 90d or forever. Every flag is in the [host reference](../cli/host.md).

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

In a `recordings` folder under your platform's data directory, named in the wizard when you arm a local take and printed at launch by the CLI:

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

`jamstream status` names the bucket each session recorded to, and [`jamstream recordings`](../cli/recordings.md) lists what is in it and fetches it. Takes outlive the session: one that ended weeks ago still lists until the retention rule deletes it. Downloading is where recording costs money, so the command prices the egress and waits for a yes before it moves a byte.

## The mix, and stems

Without stems, a take is one stereo file. With stems it is one stereo file per musician as well, each carrying that player's own signal. The sheet reads back which of the two you launched with.

Stems are stereo rather than mono, so a stem is the same size as the mix: stems turn a 1.2 GB three hour take into about 6 GB for a four piece. Every file in a take starts at the same zero, so they line up when you import them.

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
| `recording is not configured for this session` | the session was launched with recording off; host a new one with it on |
| `cannot start the recorder`, `cannot open the mix file` | the folder, or the first object in the bucket, could not be created. For a local session, check that the printed path exists and is writable |
| `recording failed` | a write failed mid-take: a full disk, or a bucket that stopped accepting the upload. The take is abandoned rather than left half written |
| `recording could not be finished` | the end of the take could not be written. Earlier takes in the session are unaffected |

## What it costs

A local take costs disk and nothing else. A take in a bucket costs a few cents of storage while it sits there, and egress when somebody downloads it. [Understanding cost](cost.md#recording) has the numbers.
