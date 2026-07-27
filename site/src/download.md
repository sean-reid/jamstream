# Download

The first release has not been published yet; these links go live with it.

Every link on this page points at the latest release by a stable name, so a new release updates them all in place. All artifacts are listed on the [releases page](https://github.com/sean-reid/jamstream/releases/latest), and each release includes a [SHA256SUMS](https://github.com/sean-reid/jamstream/releases/latest/download/SHA256SUMS) file for verifying downloads. Building from source also works on every platform: clone [the repository](https://github.com/sean-reid/jamstream) and run `cargo install --path crates/cli`.

## Install the CLI in one line

On macOS and Linux:

```console
$ curl -fsSL https://sean-reid.github.io/jamstream/install.sh | sh
```

The script detects your platform, downloads the matching archive, verifies its sha256 against `SHA256SUMS`, and installs `jamstream` to `/usr/local/bin` when that is writable, otherwise to `~/.local/bin`. Set `JAMSTREAM_INSTALL_DIR` to pick the directory yourself. Appending `-s -- --with-server` also installs the `jamstreamd` session server on Linux x86_64, which [local mode](guides/local.md) uses.

On Windows:

```console
> powershell -ExecutionPolicy Bypass -c "irm https://sean-reid.github.io/jamstream/install.ps1 | iex"
```

## macOS

- Desktop app: [jamstream-app-macos.dmg](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-app-macos.dmg)
- CLI, one universal binary for Apple silicon and Intel: [jamstream-cli-macos-universal.tar.gz](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-cli-macos-universal.tar.gz)

The app in the DMG is signed with a Developer ID certificate but not yet notarized by Apple. Until notarization ships, the first launch is blocked with a message that the app could not be verified: open System Settings, Privacy & Security, and click Open Anyway next to the JamStream entry, or Control-click the app in Finder and choose Open. Once notarized builds ship, the app opens without any of that.

Verify a download against the release checksums:

```console
$ curl -fsSLO https://github.com/sean-reid/jamstream/releases/latest/download/SHA256SUMS
$ shasum -a 256 --check --ignore-missing SHA256SUMS
jamstream-app-macos.dmg: OK
```

## Windows

- Desktop app: [jamstream-app-windows-x86_64.zip](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-app-windows-x86_64.zip)
- CLI: [jamstream-cli-windows-x86_64.zip](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-cli-windows-x86_64.zip)

The Windows binaries are plain zips and are not code signed. SmartScreen will show "Windows protected your PC" the first time you run them: click More info, then Run anyway. The warning is expected and the checksum below is the way to confirm you have the real file; a winget package, which verifies downloads by hash, is planned.

Verify a download in PowerShell:

```console
> irm https://github.com/sean-reid/jamstream/releases/latest/download/SHA256SUMS -OutFile SHA256SUMS
> (Get-FileHash jamstream-cli-windows-x86_64.zip).Hash
> Select-String jamstream-cli-windows-x86_64.zip SHA256SUMS
```

The two hashes must match; `Get-FileHash` prints uppercase and `SHA256SUMS` is lowercase, which does not matter.

## Linux

- Desktop app (x86_64): [jamstream-app-linux-x86_64.tar.gz](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-app-linux-x86_64.tar.gz)
- CLI (x86_64): [jamstream-cli-linux-x86_64.tar.gz](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-cli-linux-x86_64.tar.gz)
- Session server, static musl build (x86_64): [jamstreamd-linux-x86_64-musl.tar.gz](https://github.com/sean-reid/jamstream/releases/latest/download/jamstreamd-linux-x86_64-musl.tar.gz)

The server tarball is the same artifact the cloud session machines download at boot; installing it locally (the install script's `--with-server` flag does this) is only needed for [hosting on your own computer](guides/local.md). No arm64 Linux builds are published yet; build from source on that platform.

Verify a download against the release checksums:

```console
$ curl -fsSLO https://github.com/sean-reid/jamstream/releases/latest/download/SHA256SUMS
$ sha256sum --check --ignore-missing SHA256SUMS
jamstream-cli-linux-x86_64.tar.gz: OK
```
