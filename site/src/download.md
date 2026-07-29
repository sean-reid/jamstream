# Download


Most people want the desktop app: pick your platform below, download it, open it, and you can host and join sessions with nothing else installed, because every app build bundles its own `jamstreamd` session server. The `jamstream` CLI, for terminals and automation, installs in one line at the [bottom of this page](#install-the-cli-in-one-line).

Every link on this page points at the latest release by a stable name, so a new release updates them all in place. All artifacts are listed on the [releases page](https://github.com/sean-reid/jamstream/releases/latest). Building from source also works on every platform: clone [the repository](https://github.com/sean-reid/jamstream) and run `cargo install --path crates/cli`.

## macOS

- Desktop app: [jamstream-app-macos.dmg](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-app-macos.dmg)
- CLI, one universal binary for Apple silicon and Intel: [jamstream-cli-macos-universal.tar.gz](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-cli-macos-universal.tar.gz)

## Windows

- Desktop app: [jamstream-app-windows-x86_64.zip](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-app-windows-x86_64.zip)
- CLI: [jamstream-cli-windows-x86_64.zip](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-cli-windows-x86_64.zip)

The Windows binaries are plain zips and are not code signed. SmartScreen will show "Windows protected your PC" the first time you run them: click More info, then Run anyway. The warning is expected, and [verifying the download](#verifying-a-download) is the way to confirm you have the real file.

## Linux

- Desktop app (x86_64): [jamstream-app-linux-x86_64.tar.gz](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-app-linux-x86_64.tar.gz)
- CLI (x86_64): [jamstream-cli-linux-x86_64.tar.gz](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-cli-linux-x86_64.tar.gz)
- Session server, static musl build (x86_64), tarred: [jamstreamd-linux-x86_64-musl.tar.gz](https://github.com/sean-reid/jamstream/releases/latest/download/jamstreamd-linux-x86_64-musl.tar.gz)
- Session server, the same build as a bare binary: [jamstreamd-linux-x86_64-musl](https://github.com/sean-reid/jamstream/releases/latest/download/jamstreamd-linux-x86_64-musl)
- Session server, static musl build (aarch64), tarred: [jamstreamd-linux-aarch64-musl.tar.gz](https://github.com/sean-reid/jamstream/releases/latest/download/jamstreamd-linux-aarch64-musl.tar.gz)
- Session server, the same build as a bare binary: [jamstreamd-linux-aarch64-musl](https://github.com/sean-reid/jamstream/releases/latest/download/jamstreamd-linux-aarch64-musl)

The session server downloads are only for [hosting on your own computer](guides/local.md) with the CLI alone; the desktop app bundles its own. No arm64 Linux app or CLI builds are published yet; build from source on that platform.

## Verifying a download

Every release ships a `SHA256SUMS` file covering every artifact:

```console
$ curl -fsSLO https://github.com/sean-reid/jamstream/releases/latest/download/SHA256SUMS
$ sha256sum --check --ignore-missing SHA256SUMS
jamstream-cli-linux-x86_64.tar.gz: OK
```

On macOS that command is `shasum -a 256`. In PowerShell, compare `(Get-FileHash <file>).Hash` against that file's line in `SHA256SUMS`; `Get-FileHash` prints uppercase and the file is lowercase, which does not matter.

## Package managers

Homebrew:

```console
$ brew install --cask sean-reid/jamstream/jamstream
$ brew install sean-reid/jamstream/jamstream-cli
```

The first line installs the desktop app, the second the CLI with shell completions set up for you. The [tap](https://github.com/sean-reid/homebrew-jamstream) is updated by the release pipeline, so `brew upgrade` tracks releases on its own. The formula also works with brew on Linux x86_64, from the same tarball this page links.

winget and the AUR are coming; neither is published yet.

## Install the CLI in one line

The CLI suits scripts, automation, and machines without a display; the [CLI reference](cli/index.md) documents every command. On macOS and Linux:

```console
$ curl -fsSL https://sean-reid.github.io/jamstream/install.sh | sh
```

The script detects your platform, downloads the matching archive, verifies its sha256 against `SHA256SUMS`, and installs `jamstream` to `/usr/local/bin` when that is writable, otherwise to `~/.local/bin`. Set `JAMSTREAM_INSTALL_DIR` to pick the directory yourself. Appending `-s -- --with-server` also installs the `jamstreamd` session server on Linux x86_64, which [local mode](guides/local.md) with the CLI alone uses.

Uninstalling is the same shape:

```console
$ curl -fsSL https://sean-reid.github.io/jamstream/uninstall.sh | sh
```

It removes what install.sh installed and nothing else. A session still running makes it stop and say so, since the binary being removed is what ends sessions; your session records are kept unless you pass `--purge`, and credentials stay in your OS keychain either way.

On Windows:

```console
> powershell -ExecutionPolicy Bypass -c "irm https://sean-reid.github.io/jamstream/install.ps1 | iex"
```

Uninstalling on Windows is the same shape:

```console
> powershell -ExecutionPolicy Bypass -c "irm https://sean-reid.github.io/jamstream/uninstall.ps1 | iex"
```

It removes the binaries install.ps1 put in place and nothing else. A session still running makes it stop and say so; your session records are kept unless you pass `-Purge`, and credentials stay in Credential Manager either way.
