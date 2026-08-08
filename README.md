# JamStream

Spin up a short-lived jam server in your own cloud account, play together
over the internet at latencies you can actually play on, and stream the
session live to Twitch or YouTube. When the last musician leaves, the server
deletes itself.

There are no JamStream servers in the middle and no accounts. A host launches
a server on their own computer or in their own cloud account, bandmates join
with personal invite links, and the bill goes to the host's provider by the
second. A local session costs nothing.

In beta. Downloads for macOS, Windows, and Linux, and the full guide, are
at [sean-reid.github.io/jamstream](https://sean-reid.github.io/jamstream/).

## Build from source

Rust stable, 1.85 or newer. On Debian or Ubuntu the desktop app also needs:

```sh
sudo apt-get install libasound2-dev libxkbcommon-dev libwayland-dev \
    libpipewire-0.3-dev libdbus-1-dev
```

```sh
cargo test --workspace          # unit and integration tests
cargo run -p jamstream-client   # the desktop app
cargo run -p jamstream-cli -- --help
```

Three binaries ship: `jamstream-app` is the desktop app, `jamstream` is the
command line tool, and `jamstreamd` is the session server the app carries and
launches for you.

## What is here

One Cargo workspace. `protocol` holds the wire format, `session` the client
and server halves of it, `engine` the mixer and jitter buffer, `audio-io` the
device layer, `cloud` the provider clients, `stream` and `broadcast` the
outbound streaming, `client` the desktop app, `cli` and `server` the two
other binaries, and `harness` the end-to-end scenarios.

## Contributing and reporting

[CONTRIBUTING.md](CONTRIBUTING.md) covers the checks a change has to pass.
Security problems go through [SECURITY.md](SECURITY.md) rather than a public
issue. Everyone taking part is held to the [code of
conduct](CODE_OF_CONDUCT.md).

Licensed under MIT or Apache-2.0, at your option.
