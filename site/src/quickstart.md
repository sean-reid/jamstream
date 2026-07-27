# Quickstart: host your first session

Two paths from nothing to a running session. The local path takes about five minutes, needs no cloud account, and works for musicians in the same room or on the same network. The internet path launches the server in your own DigitalOcean account so bandmates anywhere can join; expect 20 minutes the first time, most of it on the DigitalOcean account. Every command is copy-pasteable.

## 1. Install

One line downloads the `jamstream` CLI from the latest release, verifies its checksum, and installs it:

```console
$ curl -fsSL https://sean-reid.github.io/jamstream/install.sh | sh
$ jamstream --version
```

The [Download](download.md) page has the desktop app, the Windows install script, and direct links to every artifact. If no release has been published yet, the script says so and exits; building from source works on every platform with a Rust toolchain ([rustup.rs](https://rustup.rs)): clone the repository, then `cargo install --path crates/cli`.

If you use the desktop app, that is the whole install: the app carries its own `jamstreamd` session server, so both paths on this page work from the app with nothing else. The local path with the CLI alone also needs `jamstreamd` on this computer: on Linux x86_64, append `-s -- --with-server` to the install line to get it; elsewhere `cargo install --path crates/server` builds it. The internet path does not need it locally; skip it if you are only hosting in the cloud.

## 2. Host on this computer

`local` is the default provider, so the flag is optional:

```console
$ jamstream host --provider local

Local sessions cost nothing.
Launch this session? [y/N] y

Starting the server on this computer.

Session 10c79bc1 is running.
server       192.168.1.12:43210
host         jamstream://join/EMebwaHOL2MhApakencB7QEAwKgBDMrRAjgF...
musician 1   jamstream://join/EMebwaHOL2MhApakencB7QEAwKgBDMrRAjgG...
musician 2   jamstream://join/EMebwaHOL2MhApakencB7QEAwKgBDMrRAjgH...
musician 3   jamstream://join/EMebwaHOL2MhApakencB7QEAwKgBDMrRAjgI...
musician 4   jamstream://join/EMebwaHOL2MhApakencB7QEAwKgBDMrRAjgJ...

State written to /Users/you/Library/Application Support/jamstream/sessions/10c79bc1....json.
End the session with: jamstream end 10c79bc1
```

The invite strings are shortened here; real ones are about 220 characters. This starts a real `jamstreamd` process on your machine and completes a full encrypted handshake with it before printing anything, so a printed invite is a working invite.

The invites carry your machine's network address (192.168.1.12 above), so they work from this computer and from any other machine on the same network. They do not work across the internet; bandmates elsewhere need the DigitalOcean path below, or router port forwarding, which [Playing on the same network](guides/local.md) explains honestly.

## 3. Join

From the desktop app: paste your invite into the field labeled "paste an invite, jamstream://join/..." on the home screen and click Join.

Without the app, for testing or for a machine without a display, the headless client joins with a WAV file as its instrument:

```console
$ jamstream join 'jamstream://join/EMebwaHOL2MhApakencB7QEAwKgBDMrRAjgF...' \
    --headless --input take.wav --output mix.wav --duration-secs 60
joined
roster: 2 members
left after 60 s; wrote mix.wav
```

The input WAV must be 48 kHz, mono or stereo.

## 4. End it

```console
$ jamstream end 10c79bc1
Session 10c79bc1 ended. Instance 67706 is destroyed.
```

For a local session the instance id is the server's process id, and ending it kills the process. A forgotten local session costs nothing, and the server also exits on its own after 10 minutes with no musicians connected (`--idle-min`).

That is the whole local loop. The rest of this page is the internet path.

## Host on the internet with DigitalOcean

The flow is the same; the server runs on a small droplet in your DigitalOcean account instead of your machine, and the invites work from anywhere.

### Create an account and token

Short version; the [DigitalOcean setup page](guides/providers/digitalocean.md) has every step from zero, including the exact token scopes.

1. Sign up at digitalocean.com and add a payment method.
2. In the control panel, open Account, then API, and click Generate New Token.
3. Choose Custom Scopes and grant the droplet, tag, and read scopes listed on the [setup page](guides/providers/digitalocean.md#2-create-an-api-token).
4. Copy the token; it is shown once.

Put the token in your environment and verify it works:

```console
$ export DIGITALOCEAN_TOKEN=dop_v1_your_token_here
$ jamstream sweep --dry-run --provider digitalocean
No jamstream-tagged instances found.
```

### Host

Every release build knows which server the new machine should run: the exact `jamstreamd` build published with that release, and its checksum, are pinned in at compile time, and the VM verifies the download at boot. There is no server URL to find and nothing extra to pass:

```console
$ jamstream host --provider digitalocean
```

(Only if you built the CLI from source is there no pin; then `--artifact-url` and `--artifact-sha256` must name a Linux x86_64 musl build of `jamstreamd` the machine can download; see the flags in the [host reference](cli/host.md).)

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
...

State written to /Users/you/Library/Application Support/jamstream/sessions/3f2a9c01....json.
End the session with: jamstream end 3f2a9c01
```

The meter is now running. The droplet bills by the second until you end the session, and it shuts itself down after 10 minutes with no musicians connected, or at the 12 hour hard cap, whichever comes first.

### Share, join, check, end

Send each `jamstream://join/...` line to exactly one person, over any channel you trust. Each invite admits one member; the `host` line is yours. Details in [Joining a session](guides/joining.md). Joining works exactly as in the local path above.

```console
$ jamstream status
SESSION    PROVIDER/REGION      STATUS      ELAPSED      ACCRUED      PROJECTED
3f2a9c01   digitalocean/nyc3    running    1 h 04 min    $0.028576 $0.08037 at 3.0 h

$ jamstream end 3f2a9c01
Session 3f2a9c01 ended. Instance 512190713 is destroyed.
```

`jamstream end --last` ends the most recent running session without the id. The CLI confirms with the provider that nothing with this session's tag is still listed before marking it ended. If you ever doubt that everything is gone:

```console
$ jamstream sweep --dry-run
No jamstream-tagged instances found.
```

That line is the whole point of the design: nothing left behind, nothing still billing.
