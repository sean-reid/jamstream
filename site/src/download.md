# Download


Most people want the desktop app: pick your platform below, download it, open it, and you can host and join sessions with nothing else installed, because every app build bundles its own `jamstreamd` session server. The `jamstream` CLI, for terminals and automation, installs in one line at the [bottom of this page](#install-the-cli-in-one-line).

Every link on this page points at the latest release by a stable name, so a new release updates them all in place. All artifacts are listed on the [releases page](https://github.com/sean-reid/jamstream/releases/latest), and each release includes a [SHA256SUMS](https://github.com/sean-reid/jamstream/releases/latest/download/SHA256SUMS) file for verifying downloads. Building from source also works on every platform: clone [the repository](https://github.com/sean-reid/jamstream) and run `cargo install --path crates/cli`.

## macOS

- Desktop app: [jamstream-app-macos.dmg](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-app-macos.dmg)
- CLI, one universal binary for Apple silicon and Intel: [jamstream-cli-macos-universal.tar.gz](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-cli-macos-universal.tar.gz)

The disk image and the app inside it are both signed with a Developer ID certificate and notarized by Apple, and the notarization ticket is stapled to each, so the app opens on first launch with no warning and without needing a network check.

Verify a download against the release checksums:

```console
$ curl -fsSLO https://github.com/sean-reid/jamstream/releases/latest/download/SHA256SUMS
$ shasum -a 256 --check --ignore-missing SHA256SUMS
jamstream-app-macos.dmg: OK
```

## Windows

- Desktop app: [jamstream-app-windows-x86_64.zip](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-app-windows-x86_64.zip)
- CLI: [jamstream-cli-windows-x86_64.zip](https://github.com/sean-reid/jamstream/releases/latest/download/jamstream-cli-windows-x86_64.zip)

The Windows binaries are plain zips and are not code signed. SmartScreen will show "Windows protected your PC" the first time you run them: click More info, then Run anyway. The warning is expected and the checksum below is the way to confirm you have the real file. A [winget package](#package-managers), which verifies downloads by hash, is prepared but not yet submitted.

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
- Session server, static musl build (x86_64), tarred: [jamstreamd-linux-x86_64-musl.tar.gz](https://github.com/sean-reid/jamstream/releases/latest/download/jamstreamd-linux-x86_64-musl.tar.gz)
- Session server, the same build as a bare binary: [jamstreamd-linux-x86_64-musl](https://github.com/sean-reid/jamstream/releases/latest/download/jamstreamd-linux-x86_64-musl)

The bare server binary is the exact artifact a release's clients tell cloud session machines to download and verify at boot; its URL and sha256 are pinned into that release's app and CLI at build time, so you never handle it yourself. The tarball packages the same binary for humans and the install script (the script's `--with-server` flag installs it), which is only needed for [hosting on your own computer](guides/local.md) with the CLI alone; the desktop app tarball already bundles `jamstreamd`. No arm64 Linux builds are published yet; build from source on that platform.

Verify a download against the release checksums:

```console
$ curl -fsSLO https://github.com/sean-reid/jamstream/releases/latest/download/SHA256SUMS
$ sha256sum --check --ignore-missing SHA256SUMS
jamstream-cli-linux-x86_64.tar.gz: OK
```

## Package managers

None of these channels is live yet: the manifests are generated from each release's own checksums, but the Homebrew tap does not exist, the winget package has not been submitted, and the AUR packages have not been published, so none of the commands below works today. Use the platform downloads above until each one lands.

Homebrew, once the tap exists:

```console
$ brew install --cask sean-reid/jamstream/jamstream
$ brew install sean-reid/jamstream/jamstream-cli
```

The first line installs the desktop app, the second the CLI. The formula also works with brew on Linux x86_64, from the same tarball this page links.

winget, once the package is accepted into the community repository:

```console
> winget install SeanReid.JamStream
```

That installs the desktop app and puts `jamstream-app` and `jamstreamd` on your PATH. winget checks the download against the hash in its manifest, which is the verification the unsigned zip otherwise leaves to you.

Arch Linux, once the packages are published, with `paru` or `yay`:

```console
$ paru -S jamstream-bin
$ paru -S jamstream-cli-bin
```

Again the first is the desktop app and the second the CLI. The app package installs `jamstream-app`, `jamstreamd`, the icon, and the desktop entry; the CLI package installs `jamstream` alone.

The manifests for all three live in [packaging/](https://github.com/sean-reid/jamstream/tree/main/packaging), and every release after v0.1.1-beta attaches them as a `jamstream-packaging.tar.gz` asset with that release's checksums filled in, so you can read exactly what each channel would install.

## Install the CLI in one line

The CLI suits scripts, automation, and machines without a display; the [CLI reference](cli/index.md) documents every command. On macOS and Linux:

```console
$ curl -fsSL https://sean-reid.github.io/jamstream/install.sh | sh
```

The script detects your platform, downloads the matching archive, verifies its sha256 against `SHA256SUMS`, and installs `jamstream` to `/usr/local/bin` when that is writable, otherwise to `~/.local/bin`. Set `JAMSTREAM_INSTALL_DIR` to pick the directory yourself. Appending `-s -- --with-server` also installs the `jamstreamd` session server on Linux x86_64, which [local mode](guides/local.md) with the CLI alone uses.

On Windows:

```console
> powershell -ExecutionPolicy Bypass -c "irm https://sean-reid.github.io/jamstream/install.ps1 | iex"
```
