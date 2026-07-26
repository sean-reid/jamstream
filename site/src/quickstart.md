# Quickstart: host your first session

This page walks the DigitalOcean path from nothing to a running session and back to nothing. Expect 20 minutes the first time, most of it on the DigitalOcean account. Every command is copy-pasteable.

If you want to see the flow before touching a cloud account, run it against the built-in mock provider first:

```console
$ jamstream host --provider mock --yes
```

The mock launches no real machine, costs nothing, and prints the same output shape you will see below.

## 1. Install the CLI

No packaged releases exist yet, so build from source. You need a Rust toolchain ([rustup.rs](https://rustup.rs)).

```console
$ git clone https://github.com/sean-reid/jamstream
$ cd jamstream
$ cargo install --path crates/cli
$ jamstream --version
```

## 2. Create a DigitalOcean account and token

Short version; the [DigitalOcean setup page](guides/providers/digitalocean.md) has every step from zero, including the exact token scopes.

1. Sign up at digitalocean.com and add a payment method.
2. In the control panel, open Account, then API, and click Generate New Token.
3. Choose Custom Scopes and grant the droplet, tag, and read scopes listed on the [setup page](guides/providers/digitalocean.md#2-create-an-api-token).
4. Copy the token; it is shown once.

Put the token in your environment:

```console
$ export DIGITALOCEAN_TOKEN=dop_v1_your_token_here
```

Verify it works. This lists anything JamStream-tagged in your account without touching it, so a fresh account prints one line:

```console
$ jamstream sweep --dry-run --provider digitalocean
No jamstream-tagged instances found.
```

## 3. Point at a server build

No `jamstreamd` release artifact is published yet. Until one is, hosting on a real provider needs two extra flags naming a Linux x86_64 musl build of `jamstreamd` that the new machine can download, plus its checksum:

```console
$ cargo build --release -p jamstream-server
$ shasum -a 256 target/release/jamstreamd
```

Host the binary anywhere the new VM can reach over HTTPS, and note the sha256. When releases exist this step disappears.

## 4. Host

```console
$ jamstream host --provider digitalocean \
    --artifact-url https://your-host.example/jamstreamd \
    --artifact-sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
```

The CLI probes each region from your machine, ranks them, and shows a cost preview before anything launches. Example output; region timings and the session id will differ:

```text
REGION              WORST RTT         HOURLY       EGRESS
nyc3                    14 ms    $0.02679/hr     $0.01/GB
tor1                    27 ms    $0.02679/hr     $0.01/GB
atl1                    31 ms    $0.02679/hr     $0.01/GB
sfo3                    72 ms    $0.02679/hr     $0.01/GB
fra1                    93 ms    $0.02679/hr     $0.01/GB
...

Cost preview for digitalocean nyc3 over 3.0 hours:
VM $0.02679/hr x 3.0 h                            $0.08037
Egress estimate 1.62 GB at $0.01/GB                $0.0162
Included egress credit (3000 GB free)             -$0.0162
Total (estimate)                                  $0.08037
Launch this session? [y/N] y

Launching in nyc3.

Session 3f2a9c01 is running.
server       203.0.113.10:43210
host         jamstream://join/r6edH1LCtlT3vPPiILRRVAEACgAAAcrRAjiV...
musician 1   jamstream://join/r6edH1LCtlT3vPPiILRRVAEACgAAAcrRAjiW...
musician 2   jamstream://join/r6edH1LCtlT3vPPiILRRVAEACgAAAcrRAjiX...
musician 3   jamstream://join/r6edH1LCtlT3vPPiILRRVAEACgAAAcrRAjiY...
musician 4   jamstream://join/r6edH1LCtlT3vPPiILRRVAEACgAAAcrRAjiZ...

State written to /Users/you/Library/Application Support/jamstream/sessions/3f2a9c01....json.
End the session with: jamstream end 3f2a9c01
```

The invite strings are shortened here; real ones are about 220 characters. Before printing this, the CLI has already completed a full encrypted handshake with the new server, so a printed invite is a working invite.

The meter is now running. The droplet bills by the second until you end the session, and it shuts itself down after 10 minutes with no musicians connected, or at the 12 hour hard cap, whichever comes first.

## 5. Share the invites

Send each `jamstream://join/...` line to exactly one person, over any channel you trust. Each invite admits one member; the `host` line is yours. Details in [Joining a session](guides/joining.md).

## 6. Join

From the desktop app: paste your invite into the field labeled "paste an invite, jamstream://join/..." on the home screen and click Join.

Without the app, for testing or for a machine without a display, the headless client joins with a WAV file as its instrument:

```console
$ jamstream join 'jamstream://join/r6edH1LCtlT3vPPiILRRVAEACgAAAcrRAjiV...' \
    --headless --input take.wav --output mix.wav --duration-secs 60
joined
roster: 2 members
left after 60 s; wrote mix.wav
```

The input WAV must be 48 kHz, mono or stereo.

## 7. Check on it

```console
$ jamstream status
SESSION    PROVIDER/REGION      STATUS      ELAPSED      ACCRUED      PROJECTED
3f2a9c01   digitalocean/nyc3    running    1 h 04 min    $0.028576 $0.08037 at 3.0 h
```

## 8. End it

```console
$ jamstream end 3f2a9c01
Session 3f2a9c01 ended. Instance 512190713 is destroyed.
```

`jamstream end --last` ends the most recent running session without the id. The CLI confirms with the provider that nothing with this session's tag is still listed before marking it ended. If you ever doubt that everything is gone:

```console
$ jamstream sweep --dry-run
No jamstream-tagged instances found.
```

That line is the whole point of the design: nothing left behind, nothing still billing.
