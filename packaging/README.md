# Packaging

Package-manager manifests for the four channels JamStream targets. This
page is the only hand-maintained file in this directory; every manifest
below it is generated and must not be edited by hand:

```console
$ scripts/render-packaging.sh v0.1.6
```

The script downloads that release's `SHA256SUMS` and writes every hash from
it, so a manifest can never carry a checksum nobody verified. The only
hashes not taken from `SHA256SUMS` are the two license texts, which are
repository files rather than release assets and are fetched from
raw.githubusercontent at the tag. Rerunning the script for the same tag
rewrites the same bytes, and `--check` renders into a temp dir and
compares instead of writing, exiting nonzero and naming every committed
file that has drifted from the tag it is given.

`release.yml`'s `packaging` job runs the same script for the tag it is
building and attaches the result to the release as
`jamstream-packaging.tar.gz`, so the manual submissions below are a copy
and never a re-derivation of hashes by hand. The committed copies here
track the most recent release.

| Path | Channel | Lands in |
| --- | --- | --- |
| `homebrew/Casks/jamstream.rb` | desktop app | `sean-reid/homebrew-jamstream` |
| `homebrew/Formula/jamstream-cli.rb` | CLI | `sean-reid/homebrew-jamstream` |
| `winget/manifests/s/SeanReid/JamStream/<version>/` | desktop app | `microsoft/winget-pkgs` |
| `aur/jamstream-bin/` | desktop app | `ssh://aur@aur.archlinux.org/jamstream-bin.git` |
| `aur/jamstream-cli-bin/` | CLI | `ssh://aur@aur.archlinux.org/jamstream-cli-bin.git` |
| `scoop/bucket/` | CLI and desktop app | `sean-reid/scoop-jamstream` |

Homebrew and Scoop are live; winget and AUR are not. What each channel
needs or has:

## Homebrew

Live: `sean-reid/homebrew-jamstream` is the tap (the `homebrew-` prefix is
what makes `brew tap sean-reid/jamstream` work), and every release pushes
`Casks/jamstream.rb` and `Formula/jamstream-cli.rb` into it using the
`HOMEBREW_TAP_TOKEN` secret, a personal access token with
`contents: write` on the tap. Without the secret the release still
succeeds and the job warns.

Naming: the cask holds the plain `jamstream` token because it installs
`JamStream.app`, and the CLI formula takes `-cli`. Homebrew cannot resolve
a formula and a cask with the same token in one tap, and homebrew-core
settles the same collision the same way (cask `1password`, formula
`1password-cli`).

## winget

Submitting is a pull request against `microsoft/winget-pkgs`: copy
`winget/manifests/s/SeanReid/JamStream/<version>/` to the identical path in
a fork and open the PR. `winget validate --manifest <dir>` and
`winget install --manifest <dir>` check it locally first. Automating this
later means a `wingetcreate submit` step in the packaging job with a token
that can push to a fork.

The Windows binaries are unsigned on purpose, which is not a problem here:
winget verifies the download against `InstallerSha256`, so the manifest is
the trust anchor the missing Authenticode signature would otherwise be.
Because the artifact is a plain zip with no installer, the package uses
`InstallerType: zip` with `NestedInstallerType: portable`, which extracts
the archive and puts `jamstream-app` and `jamstreamd` on PATH as command
aliases. There is no Start Menu entry until a real installer exists;
Scoop, which can declare one for a plain zip, does.

## AUR

Both packages carry the `-bin` suffix because they install upstream
prebuilt binaries instead of compiling from source, which AUR package
naming requires. Publishing each one is a git push:

```console
$ git clone ssh://aur@aur.archlinux.org/jamstream-bin.git
$ cp packaging/aur/jamstream-bin/PKGBUILD packaging/aur/jamstream-bin/.SRCINFO jamstream-bin/
$ cd jamstream-bin && git add -A && git commit -m 'jamstream-bin 0.1.6-1' && git push
```

An AUR account with an SSH key is the only prerequisite. Run
`makepkg --printsrcinfo > .SRCINFO` on an Arch machine to confirm the
committed `.SRCINFO` matches, and `namcap PKGBUILD` before the first push;
`.SRCINFO` is written by the generator because `makepkg` needs Arch and the
generator runs anywhere.

`pkgver` drops the SemVer hyphen (`0.1.1-beta` becomes `0.1.1beta`) because
`pkgver` forbids hyphens, and dropping it is also the ordering pacman
wants: `vercmp` ranks `1.0beta` below `1.0` but `1.0.beta` above it. The
real tag stays in the PKGBUILD as `_tag` for the download URL.

The app's dependencies come from the published binary, not from guesswork.
Its `DT_NEEDED` entries are `libasound.so.2`, `libdbus-1.so.3`, and
`libpipewire-0.3.so.0` plus glibc and libgcc; eframe then dlopens its
window-system libraries at runtime (`libwayland-client.so.0`,
`libwayland-egl.so.1`, `libxkbcommon.so.0`, `libxkbcommon-x11.so.0`,
`libX11.so.6`, `libX11-xcb.so.1`, `libXcursor.so.1`, `libXi.so.6`,
`libEGL.so.1`), which no linker check can see and the app cannot open a
window without, so they are hard `depends` too. The CLI links glibc, libm,
and libgcc only.

## Scoop

Live: `sean-reid/scoop-jamstream` is the bucket, a plain git repository
holding `scoop/bucket/` verbatim, the same shape as the tap.

```console
$ scoop bucket add jamstream https://github.com/sean-reid/scoop-jamstream
$ scoop install jamstream
```

`jamstream` installs the CLI; `jamstream-app` installs the desktop app and
`jamstreamd` with a Start Menu shortcut, whose icon is the `jamstream.ico`
the app zip carries. The plain name goes to the CLI here, the reverse of
the Homebrew split, because Scoop's audience lives in the terminal.

Publishing a release is a copy of `scoop/bucket/` into the bucket and a
push. Each manifest also carries `checkver` and `autoupdate` against
GitHub releases, so Scoop's own checkver tooling can bump the bucket from
a new tag without touching this repository. The zip is unsigned like the
winget one, and the manifest hash from `SHA256SUMS` is the same trust
anchor.
