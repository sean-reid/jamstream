#!/bin/sh
# Renders every package-manager manifest in packaging/ for one published
# release, with that release's real sha256 hashes.
#
#   usage: render-packaging.sh [options] <tag>
#
#     -c, --check           render into a temp dir and compare against the
#                           committed tree instead of writing; exits 1
#                           naming every file that drifted, so a job can
#                           fail when the committed manifests fall behind
#                           a release (same shape as render-palette.sh
#                           --check and render-icon.sh --check)
#     -o, --output-dir DIR  write under DIR instead of ./packaging
#     -s, --sums FILE       use a local SHA256SUMS instead of downloading
#                           the release's (for tests). The license texts and
#                           the release date are still fetched, so this is
#                           not a fully offline mode.
#
# The hashes are never typed by hand: this script downloads SHA256SUMS from
# the named release and reads every asset hash out of it, which is the same
# file the install script and the downloads page verify against. Only the
# two license texts are hashed separately (they are repository files, not
# release assets, so they are fetched from raw.githubusercontent at the
# tag).
#
# Idempotent: two runs with the same tag produce byte-identical files, and
# a run for the current release on a clean tree changes nothing. Rendering
# a NEW version also prunes the previous version directory under
# packaging/winget/manifests/, which is versioned by winget's own layout.
#
# What the committed files under packaging/ are for: they are the source of
# truth a human copies into the four third-party repositories (a Homebrew
# tap, microsoft/winget-pkgs, the AUR, the Scoop bucket). release.yml
# re-renders them for the tag it is building, attaches the result as a
# release asset, and pushes the Homebrew tap when a token exists; see the
# packaging job there.
#
# POSIX sh. Kept shellcheck-clean by ci.yml (shellcheck scripts/*.sh).
set -eu

REPO="sean-reid/jamstream"
RELEASE_BASE="https://github.com/${REPO}/releases/download"
RAW_BASE="https://raw.githubusercontent.com/${REPO}"
HOMEPAGE="https://sean-reid.github.io/jamstream"

fail() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

usage() {
  cat <<'USAGE'
usage: render-packaging.sh [options] <tag>

Renders the Homebrew, winget, AUR, and Scoop manifests for a published
release.

  -c, --check           compare against the committed tree instead of
                        writing; exit 1 naming every drifted file
  -o, --output-dir DIR  write under DIR instead of ./packaging
  -s, --sums FILE       use a local SHA256SUMS instead of downloading it
  -h, --help            show this help

Example:
  scripts/render-packaging.sh v0.1.1-beta
USAGE
}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TAG=""
OUT_DIR=""
SUMS_FILE=""
MODE=render

while [ "$#" -gt 0 ]; do
  case "$1" in
    -c | --check)
      MODE=check
      shift
      ;;
    -o | --output-dir)
      [ "$#" -ge 2 ] || fail "$1 needs a directory"
      OUT_DIR=$2
      shift 2
      ;;
    -s | --sums)
      [ "$#" -ge 2 ] || fail "$1 needs a file"
      SUMS_FILE=$2
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    -*) fail "unknown option: $1 (see --help)" ;;
    *)
      [ -z "$TAG" ] || fail "give exactly one tag (got $TAG and $1)"
      TAG=$1
      shift
      ;;
  esac
done

[ -n "$TAG" ] || {
  usage >&2
  exit 2
}
[ -n "$OUT_DIR" ] || OUT_DIR="$ROOT/packaging"

# Tags are v<semver>, prerelease suffix included: v0.1.1-beta, v0.2.0.
case "$TAG" in
  v[0-9]*) ;;
  *) fail "tag must look like v0.1.1-beta or v0.2.0 (got $TAG)" ;;
esac

VERSION=${TAG#v}

# Arch's pkgver forbids hyphens, so the SemVer prerelease separator is
# dropped rather than replaced: 0.1.1-beta becomes 0.1.1beta. That is also
# the ordering pacman wants, since vercmp ranks 1.0beta BELOW 1.0 while it
# ranks 1.0.beta ABOVE it (vercmp(8): 1.0a < 1.0beta < 1.0 < 1.0.a). The
# real tag stays in the PKGBUILD as _tag for the download url.
PKGVER=$(printf '%s' "$VERSION" | tr -d '-')

command -v curl >/dev/null 2>&1 || fail "curl is required"
if command -v sha256sum >/dev/null 2>&1; then
  SHA_TOOL=sha256sum
elif command -v shasum >/dev/null 2>&1; then
  SHA_TOOL=shasum
else
  fail "neither sha256sum nor shasum is available"
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

# In check mode everything renders into the temp dir and the committed
# tree is only read, so a failing check leaves it untouched.
TARGET_DIR=$OUT_DIR
if [ "$MODE" = check ]; then
  OUT_DIR="$TMP/render"
fi

# api_get <url>: authenticated when GH_TOKEN is set (CI), anonymous
# otherwise (the unauthenticated rate limit is plenty for one release).
api_get() {
  if [ -n "${GH_TOKEN:-}" ]; then
    curl -fsSL -H "Authorization: Bearer ${GH_TOKEN}" "$1"
  else
    curl -fsSL "$1"
  fi
}

hash_of() {
  if [ "$SHA_TOOL" = sha256sum ]; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

if [ -n "$SUMS_FILE" ]; then
  [ -f "$SUMS_FILE" ] || fail "no such file: $SUMS_FILE"
  SUMS="$SUMS_FILE"
  printf 'using local checksums: %s\n' "$SUMS"
else
  SUMS="$TMP/SHA256SUMS"
  # gh, not the public download url: a release is a draft until its last
  # asset lands, and draft assets are served only by the api that gh uses.
  if [ -n "${GH_TOKEN:-}" ] && command -v gh > /dev/null 2>&1; then
    gh release download "$TAG" --repo "$REPO" --pattern SHA256SUMS \
      --output "$SUMS" --clobber \
      || fail "cannot download SHA256SUMS from $TAG; did the checksums job run?"
  else
    api_get "${RELEASE_BASE}/${TAG}/SHA256SUMS" > "$SUMS" \
      || fail "cannot download ${RELEASE_BASE}/${TAG}/SHA256SUMS; is $TAG published and are its uploads finished?"
  fi
  printf 'fetched checksums for %s\n' "$TAG"
fi

# sum_for <asset>: the release's recorded sha256, or a hard failure. The
# leading '*' sha256sum writes for binary mode is tolerated.
sum_for() {
  sum_for_value=$(awk -v name="$1" '{ f = $2; sub(/^\*/, "", f); if (f == name) print $1 }' "$SUMS")
  [ -n "$sum_for_value" ] || fail "SHA256SUMS for $TAG has no entry for $1"
  case "$sum_for_value" in
    *[!0-9a-f]*) fail "SHA256SUMS entry for $1 is not lowercase hex: $sum_for_value" ;;
  esac
  [ "${#sum_for_value}" -eq 64 ] || fail "SHA256SUMS entry for $1 is not 64 hex digits: $sum_for_value"
  printf '%s' "$sum_for_value"
}

# license_sum <file>: sha256 of a repository file at this tag. The license
# texts are not release assets, so AUR pins them by their own hash.
license_sum() {
  api_get "${RAW_BASE}/${TAG}/$1" > "$TMP/$1" \
    || fail "cannot download $1 at $TAG"
  hash_of "$TMP/$1"
}

upper() { printf '%s' "$1" | tr 'a-f' 'A-F'; }

SHA_APP_MACOS=$(sum_for jamstream-app-macos.dmg)
SHA_CLI_MACOS=$(sum_for jamstream-cli-macos-universal.tar.gz)
SHA_APP_LINUX=$(sum_for jamstream-app-linux-x86_64.tar.gz)
SHA_CLI_LINUX=$(sum_for jamstream-cli-linux-x86_64.tar.gz)
SHA_APP_WINDOWS=$(sum_for jamstream-app-windows-x86_64.zip)
SHA_CLI_WINDOWS=$(sum_for jamstream-cli-windows-x86_64.zip)
SHA_LICENSE_MIT=$(license_sum LICENSE-MIT)
SHA_LICENSE_APACHE=$(license_sum LICENSE-APACHE)

# The copyright line comes out of the license text that was just fetched, so
# it cannot drift from the repository.
COPYRIGHT=$(sed -n 's/^\(Copyright (c) .*[^ ]\) *$/\1/p' "$TMP/LICENSE-MIT" | head -1)
[ -n "$COPYRIGHT" ] || fail "no copyright line found in LICENSE-MIT at $TAG"

# winget-pkgs writes InstallerSha256 uppercase (wingetcreate does), and the
# schema accepts either case. Matching the repository keeps review diffs
# boring.
SHA_APP_WINDOWS_UPPER=$(upper "$SHA_APP_WINDOWS")

# ReleaseDate is optional in the winget schema and must be the release's
# real date, so it comes from the API. A failure here (offline run, rate
# limit) drops the field rather than guessing a date.
RELEASE_DATE=$(api_get "https://api.github.com/repos/${REPO}/releases/tags/${TAG}" 2> /dev/null |
  sed -n 's/^[[:space:]]*"published_at":[[:space:]]*"\([0-9][0-9-]*\)T.*/\1/p' | head -1) || RELEASE_DATE=""
if [ -n "$RELEASE_DATE" ]; then
  WINGET_RELEASE_DATE="ReleaseDate: $RELEASE_DATE"
else
  printf 'warning: could not read the release date from the API; omitting winget ReleaseDate\n' >&2
  WINGET_RELEASE_DATE="# ReleaseDate omitted: the release date could not be read from the API"
fi

printf 'rendering %s (version %s, pkgver %s) into %s\n' "$TAG" "$VERSION" "$PKGVER" "$OUT_DIR"

GENERATED="Generated by scripts/render-packaging.sh from the ${TAG} release; do not edit by hand."

# ---------------------------------------------------------------- Homebrew
# A tap holds casks in Casks/ and formulae in Formula/, and the release job
# copies these two directories into sean-reid/homebrew-jamstream verbatim.
#
# Naming: the cask keeps the plain token (jamstream) because it installs
# JamStream.app, and the CLI formula takes the -cli suffix. Homebrew treats
# a formula and a cask with the same token in the same tap as ambiguous, so
# one of the two has to move; homebrew-core settles this the same way
# (cask 1password, formula 1password-cli), and the app is the name users
# mean. A hyphenated formula file maps to a CamelCase class, so
# jamstream-cli.rb defines JamstreamCli.
mkdir -p "$OUT_DIR/homebrew/Casks" "$OUT_DIR/homebrew/Formula"

cat > "$OUT_DIR/homebrew/Casks/jamstream.rb" <<CASK
# $GENERATED
#
# Install: brew install --cask sean-reid/jamstream/jamstream
cask "jamstream" do
  version "$VERSION"
  sha256 "$SHA_APP_MACOS"

  url "https://github.com/$REPO/releases/download/v#{version}/jamstream-app-macos.dmg",
      verified: "github.com/$REPO/"
  name "JamStream"
  desc "Host a short-lived jam server in your own cloud account and play together"
  homepage "$HOMEPAGE"

  livecheck do
    url :url
    strategy :github_latest
  end

  # The app has no updater of its own: brew upgrade is the update path.
  auto_updates false
  # Info.plist sets LSMinimumSystemVersion 11.0.
  depends_on macos: ">= :big_sur"

  app "JamStream.app"

  # The app writes provider state and session records under
  # ~/Library/Application Support/jamstream (dirs::data_local_dir).
  zap trash: [
    "~/Library/Application Support/jamstream",
    "~/Library/Saved Application State/com.seanreid.jamstream.savedState",
  ]

  # Cloud credentials live in the login keychain (service "jamstream",
  # accounts like "digitalocean.token"), which a cask cannot delete. To
  # remove them by hand after zapping:
  #   security delete-generic-password -s jamstream
  # once per stored account, until it reports no matching item.
end
CASK

cat > "$OUT_DIR/homebrew/Formula/jamstream-cli.rb" <<FORMULA
# $GENERATED
#
# Install: brew install sean-reid/jamstream/jamstream-cli
#
# Prebuilt release binaries, so there is no bottle block and none is
# needed: "building from source" here is extracting one tarball, which is
# why brew on Linux works from the same gnu tarball the downloads page
# serves. That tarball is built on ubuntu-latest, so it needs that glibc or
# newer; older distributions should use cargo install --path crates/cli.
class JamstreamCli < Formula
  desc "Terminal client for jam sessions hosted in your own cloud account"
  homepage "$HOMEPAGE"
  version "$VERSION"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    # One universal binary for Apple silicon and Intel.
    url "https://github.com/$REPO/releases/download/$TAG/jamstream-cli-macos-universal.tar.gz"
    sha256 "$SHA_CLI_MACOS"
  end

  on_linux do
    on_intel do
      url "https://github.com/$REPO/releases/download/$TAG/jamstream-cli-linux-x86_64.tar.gz"
      sha256 "$SHA_CLI_LINUX"
    end
    # No arm64 Linux build is published yet; brew falls back to an error
    # rather than installing the wrong architecture.
  end

  livecheck do
    url "https://github.com/$REPO/releases/latest"
    strategy :github_latest
  end

  def install
    bin.install "jamstream"
    generate_completions_from_executable(bin/"jamstream", "completions")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/jamstream --version")
    assert_match "host", shell_output("#{bin}/jamstream --help")
  end
end
FORMULA

# ------------------------------------------------------------------ winget
# winget-pkgs stores manifests at manifests/<first letter, lowercase>/<
# Publisher>/<Package>/<version>/, so rendering into that exact layout
# makes the submission a directory copy with no renaming.
WINGET_PARENT="$OUT_DIR/winget/manifests/s/SeanReid/JamStream"
WINGET_DIR="$WINGET_PARENT/$VERSION"
mkdir -p "$WINGET_DIR"

# Prune older version directories so the committed tree tracks exactly one
# release; winget-pkgs itself keeps every version, this repository does not.
if [ -d "$WINGET_PARENT" ]; then
  for stale in "$WINGET_PARENT"/*; do
    [ -d "$stale" ] || continue
    [ "$(basename "$stale")" != "$VERSION" ] || continue
    printf 'pruning stale winget version directory: %s\n' "$stale"
    rm -rf "$stale"
  done
fi

cat > "$WINGET_DIR/SeanReid.JamStream.yaml" <<WINGET_VERSION
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.1.6.0.schema.json
# $GENERATED
#
# Submitting: copy this directory into a fork of microsoft/winget-pkgs at
# manifests/s/SeanReid/JamStream/$VERSION/ and open a pull request.
PackageIdentifier: SeanReid.JamStream
PackageVersion: $VERSION
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.6.0
WINGET_VERSION

cat > "$WINGET_DIR/SeanReid.JamStream.installer.yaml" <<WINGET_INSTALLER
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.1.6.0.schema.json
# $GENERATED
#
# The Windows artifact is a plain zip of unsigned binaries, so the package
# is a zip carrying portable nested installers: winget extracts the archive
# and puts a command alias for each nested file on PATH. winget verifies
# the download against InstallerSha256 below, which is the check the
# missing Authenticode signature would otherwise provide.
#
# jamstreamd.exe is extracted beside jamstream-app.exe and gets its own
# alias. The app resolves its session server app-adjacent, so hosting on
# this computer works from the extracted directory with nothing else
# installed.
PackageIdentifier: SeanReid.JamStream
PackageVersion: $VERSION
MinimumOSVersion: 10.0.17763.0
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
  - RelativeFilePath: jamstream-app.exe
    PortableCommandAlias: jamstream-app
  - RelativeFilePath: jamstreamd.exe
    PortableCommandAlias: jamstreamd
UpgradeBehavior: install
$WINGET_RELEASE_DATE
Installers:
  - Architecture: x64
    InstallerUrl: https://github.com/$REPO/releases/download/$TAG/jamstream-app-windows-x86_64.zip
    InstallerSha256: $SHA_APP_WINDOWS_UPPER
ManifestType: installer
ManifestVersion: 1.6.0
WINGET_INSTALLER

cat > "$WINGET_DIR/SeanReid.JamStream.locale.en-US.yaml" <<WINGET_LOCALE
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.1.6.0.schema.json
# $GENERATED
PackageIdentifier: SeanReid.JamStream
PackageVersion: $VERSION
PackageLocale: en-US
Publisher: Sean Reid
PublisherUrl: https://github.com/sean-reid
PublisherSupportUrl: https://github.com/$REPO/issues
Author: JamStream contributors
PackageName: JamStream
PackageUrl: $HOMEPAGE
License: MIT OR Apache-2.0
# Either license, at the user's option. LICENSE-APACHE sits beside the file
# below in the same tree; winget takes one url.
LicenseUrl: https://github.com/$REPO/blob/$TAG/LICENSE-MIT
Copyright: $COPYRIGHT
ShortDescription: Host a short-lived jam server in your own cloud account and play together
Description: >-
  JamStream runs a jam session server in your own cloud account, or on your
  own computer, and plays audio between musicians at latencies low enough
  to play on. The desktop app carries its own session server, so hosting
  needs nothing else installed. Sessions are invite-only, every link admits
  one person, and the server deletes itself when the last musician leaves.
  This package installs unsigned binaries; winget verifies the download
  against the hash in the installer manifest.
Moniker: jamstream
Tags:
  - audio
  - band
  - collaboration
  - jam
  - low-latency
  - music
  - rehearsal
  - streaming
ReleaseNotesUrl: https://github.com/$REPO/releases/tag/$TAG
Documentations:
  - DocumentLabel: Documentation
    DocumentUrl: $HOMEPAGE
ManifestType: defaultLocale
ManifestVersion: 1.6.0
WINGET_LOCALE

# --------------------------------------------------------------------- AUR
# Naming: both packages carry the -bin suffix because they install upstream
# prebuilt binaries rather than compiling from source, which the AUR
# package-naming rules require. The plain names stay free for future
# from-source packages, and each -bin package provides and conflicts with
# its plain name so the two can never be installed at once.
#
# Dependencies come from the published binaries, not from guesswork. The
# app's DT_NEEDED entries are libasound.so.2, libdbus-1.so.3, and
# libpipewire-0.3.so.0 plus glibc/gcc-libs; eframe 0.35 (winit) then
# dlopens its window-system libraries at runtime, and the binary carries
# their sonames as strings: libwayland-client.so.0, libwayland-egl.so.1,
# libxkbcommon.so.0, libxkbcommon-x11.so.0, libX11.so.6, libX11-xcb.so.1,
# libXcursor.so.1, libXi.so.6, libEGL.so.1. Runtime dlopen is invisible to
# a linker check, so those are listed as hard depends: the app fails to
# open a window without them. ci.yml's build-time apt list is the same set
# minus the X11 half, which Debian pulls in transitively.
#
# The CLI links glibc, libm, and libgcc_s only (it has no audio or keyring
# dependency), so jamstream-cli-bin needs nothing else.
mkdir -p "$OUT_DIR/aur/jamstream-bin" "$OUT_DIR/aur/jamstream-cli-bin"

APP_SOURCE="jamstream-app-\$pkgver-x86_64.tar.gz::https://github.com/$REPO/releases/download/\$_tag/jamstream-app-linux-x86_64.tar.gz"
CLI_SOURCE="jamstream-cli-\$pkgver-x86_64.tar.gz::https://github.com/$REPO/releases/download/\$_tag/jamstream-cli-linux-x86_64.tar.gz"
MIT_SOURCE="LICENSE-MIT-\$pkgver::https://raw.githubusercontent.com/$REPO/\$_tag/LICENSE-MIT"
APACHE_SOURCE="LICENSE-APACHE-\$pkgver::https://raw.githubusercontent.com/$REPO/\$_tag/LICENSE-APACHE"

cat > "$OUT_DIR/aur/jamstream-bin/PKGBUILD" <<APP_PKGBUILD
# Maintainer: Sean Reid <sean-reid@users.noreply.github.com>
# $GENERATED
#
# Desktop app, from the official release tarball. license=('MIT OR
# Apache-2.0') is the SPDX expression Arch now asks for, and it is the one
# that says what the dual license means: either license, at your option.
pkgname=jamstream-bin
_tag=$TAG
pkgver=$PKGVER
pkgrel=1
pkgdesc="Host a short-lived jam server in your own cloud account and play together (desktop app)"
arch=('x86_64')
url="$HOMEPAGE"
license=('MIT OR Apache-2.0')
depends=('alsa-lib' 'dbus' 'gcc-libs' 'glibc' 'libglvnd' 'libpipewire'
         'libx11' 'libxcursor' 'libxi' 'libxkbcommon' 'libxkbcommon-x11'
         'wayland')
optdepends=('gnome-keyring: store cloud provider credentials in the Secret Service keyring'
            'kwallet: store cloud provider credentials in KWallet')
# Each -bin package provides and conflicts with exactly the names it
# installs, so a future from-source package replaces it cleanly and the two
# -bin packages still coexist: this one owns jamstream-app and jamstreamd,
# jamstream-cli-bin owns jamstream, and no name appears in both lists.
provides=('jamstream-app' 'jamstreamd')
conflicts=('jamstream-app' 'jamstreamd')
# Release binaries are already stripped and carry no debug info.
options=('!strip' '!debug')
source=("$APP_SOURCE"
        "$MIT_SOURCE"
        "$APACHE_SOURCE")
sha256sums=('$SHA_APP_LINUX'
            '$SHA_LICENSE_MIT'
            '$SHA_LICENSE_APACHE')

package() {
  cd "\$srcdir"

  # The app resolves its session server app-adjacent, so jamstreamd goes in
  # the same directory: hosting on this computer needs nothing else.
  install -Dm755 jamstream-app "\$pkgdir/usr/bin/jamstream-app"
  install -Dm755 jamstreamd "\$pkgdir/usr/bin/jamstreamd"

  # The icon and desktop entry were added to the app tarball after
  # v0.1.1-beta, whose archive holds the two binaries only. Install them
  # when the archive carries them so this PKGBUILD builds either release.
  # The icon is named jamstream.png to match the entry's Icon=jamstream.
  if [ -f jamstream.png ]; then
    install -Dm644 jamstream.png \\
      "\$pkgdir/usr/share/icons/hicolor/512x512/apps/jamstream.png"
  fi
  if [ -f jamstream.desktop ]; then
    install -Dm644 jamstream.desktop \\
      "\$pkgdir/usr/share/applications/jamstream.desktop"
  fi

  install -Dm644 "LICENSE-MIT-\$pkgver" "\$pkgdir/usr/share/licenses/\$pkgname/LICENSE-MIT"
  install -Dm644 "LICENSE-APACHE-\$pkgver" "\$pkgdir/usr/share/licenses/\$pkgname/LICENSE-APACHE"
}
APP_PKGBUILD

cat > "$OUT_DIR/aur/jamstream-cli-bin/PKGBUILD" <<CLI_PKGBUILD
# Maintainer: Sean Reid <sean-reid@users.noreply.github.com>
# $GENERATED
#
# CLI only, from the official release tarball. The binary links glibc,
# libm, and libgcc_s and nothing else. Local hosting with the CLI alone
# also wants jamstreamd, which jamstream-bin installs.
pkgname=jamstream-cli-bin
_tag=$TAG
pkgver=$PKGVER
pkgrel=1
pkgdesc="Terminal client for jam sessions hosted in your own cloud account"
arch=('x86_64')
url="$HOMEPAGE"
license=('MIT OR Apache-2.0')
depends=('gcc-libs' 'glibc')
optdepends=('jamstream-bin: desktop app and the jamstreamd session server')
provides=('jamstream')
conflicts=('jamstream')
options=('!strip' '!debug')
source=("$CLI_SOURCE"
        "$MIT_SOURCE"
        "$APACHE_SOURCE")
sha256sums=('$SHA_CLI_LINUX'
            '$SHA_LICENSE_MIT'
            '$SHA_LICENSE_APACHE')

package() {
  cd "\$srcdir"
  install -Dm755 jamstream "\$pkgdir/usr/bin/jamstream"
  install -Dm644 "LICENSE-MIT-\$pkgver" "\$pkgdir/usr/share/licenses/\$pkgname/LICENSE-MIT"
  install -Dm644 "LICENSE-APACHE-\$pkgver" "\$pkgdir/usr/share/licenses/\$pkgname/LICENSE-APACHE"
}
CLI_PKGBUILD

# .SRCINFO is what the AUR reads, and makepkg --printsrcinfo (the usual
# generator) needs an Arch box. These are written here instead, from the
# same values as the PKGBUILD above, in makepkg's field order and with its
# tab indentation, so the two can never disagree. Regenerate with
# `makepkg --printsrcinfo > .SRCINFO` on Arch if you edit a PKGBUILD by
# hand, which the header there asks you not to do.
srcinfo_app() {
  printf 'pkgbase = jamstream-bin\n'
  printf '\tpkgdesc = Host a short-lived jam server in your own cloud account and play together (desktop app)\n'
  printf '\tpkgver = %s\n' "$PKGVER"
  printf '\tpkgrel = 1\n'
  printf '\turl = %s\n' "$HOMEPAGE"
  printf '\tarch = x86_64\n'
  printf '\tlicense = MIT OR Apache-2.0\n'
  for srcinfo_dep in alsa-lib dbus gcc-libs glibc libglvnd libpipewire \
    libx11 libxcursor libxi libxkbcommon libxkbcommon-x11 wayland; do
    printf '\tdepends = %s\n' "$srcinfo_dep"
  done
  printf '\toptdepends = gnome-keyring: store cloud provider credentials in the Secret Service keyring\n'
  printf '\toptdepends = kwallet: store cloud provider credentials in KWallet\n'
  printf '\tprovides = jamstream-app\n'
  printf '\tprovides = jamstreamd\n'
  printf '\tconflicts = jamstream-app\n'
  printf '\tconflicts = jamstreamd\n'
  printf '\toptions = !strip\n'
  printf '\toptions = !debug\n'
  printf '\tsource = jamstream-app-%s-x86_64.tar.gz::https://github.com/%s/releases/download/%s/jamstream-app-linux-x86_64.tar.gz\n' \
    "$PKGVER" "$REPO" "$TAG"
  printf '\tsource = LICENSE-MIT-%s::https://raw.githubusercontent.com/%s/%s/LICENSE-MIT\n' \
    "$PKGVER" "$REPO" "$TAG"
  printf '\tsource = LICENSE-APACHE-%s::https://raw.githubusercontent.com/%s/%s/LICENSE-APACHE\n' \
    "$PKGVER" "$REPO" "$TAG"
  printf '\tsha256sums = %s\n' "$SHA_APP_LINUX"
  printf '\tsha256sums = %s\n' "$SHA_LICENSE_MIT"
  printf '\tsha256sums = %s\n' "$SHA_LICENSE_APACHE"
  printf '\n'
  printf 'pkgname = jamstream-bin\n'
}

srcinfo_cli() {
  printf 'pkgbase = jamstream-cli-bin\n'
  printf '\tpkgdesc = Terminal client for jam sessions hosted in your own cloud account\n'
  printf '\tpkgver = %s\n' "$PKGVER"
  printf '\tpkgrel = 1\n'
  printf '\turl = %s\n' "$HOMEPAGE"
  printf '\tarch = x86_64\n'
  printf '\tlicense = MIT OR Apache-2.0\n'
  printf '\tdepends = gcc-libs\n'
  printf '\tdepends = glibc\n'
  printf '\toptdepends = jamstream-bin: desktop app and the jamstreamd session server\n'
  printf '\tprovides = jamstream\n'
  printf '\tconflicts = jamstream\n'
  printf '\toptions = !strip\n'
  printf '\toptions = !debug\n'
  printf '\tsource = jamstream-cli-%s-x86_64.tar.gz::https://github.com/%s/releases/download/%s/jamstream-cli-linux-x86_64.tar.gz\n' \
    "$PKGVER" "$REPO" "$TAG"
  printf '\tsource = LICENSE-MIT-%s::https://raw.githubusercontent.com/%s/%s/LICENSE-MIT\n' \
    "$PKGVER" "$REPO" "$TAG"
  printf '\tsource = LICENSE-APACHE-%s::https://raw.githubusercontent.com/%s/%s/LICENSE-APACHE\n' \
    "$PKGVER" "$REPO" "$TAG"
  printf '\tsha256sums = %s\n' "$SHA_CLI_LINUX"
  printf '\tsha256sums = %s\n' "$SHA_LICENSE_MIT"
  printf '\tsha256sums = %s\n' "$SHA_LICENSE_APACHE"
  printf '\n'
  printf 'pkgname = jamstream-cli-bin\n'
}

srcinfo_app > "$OUT_DIR/aur/jamstream-bin/.SRCINFO"
srcinfo_cli > "$OUT_DIR/aur/jamstream-cli-bin/.SRCINFO"

# ------------------------------------------------------------------- Scoop
# A Scoop bucket is a plain git repository with manifests under bucket/,
# so scoop/bucket/ here mirrors sean-reid/scoop-jamstream verbatim, the
# way homebrew/ mirrors the tap.
#
# Naming: Scoop's audience lives in the terminal, so the plain name
# installs the CLI, matching its binary (jamstream.exe), and the app
# takes -app, matching its own exe. That is the Homebrew split reversed:
# there the cask owns the plain token because the app is what mac users
# mean by "install jamstream".
#
# The app manifest declares a Start Menu shortcut, which is what a zip of
# portable binaries cannot get from winget; the app zip carries
# jamstream.ico beside the exe for exactly this entry. checkver's github
# strategy reads the latest stable release, so a manifest rendered from a
# prerelease tag sits still until the next stable release, which is what
# a bucket should do.
mkdir -p "$OUT_DIR/scoop/bucket"

cat > "$OUT_DIR/scoop/bucket/jamstream.json" <<SCOOP_CLI
{
    "##": "$GENERATED",
    "version": "$VERSION",
    "description": "Terminal client for jam sessions hosted in your own cloud account",
    "homepage": "$HOMEPAGE",
    "license": "MIT|Apache-2.0",
    "architecture": {
        "64bit": {
            "url": "https://github.com/$REPO/releases/download/$TAG/jamstream-cli-windows-x86_64.zip",
            "hash": "$SHA_CLI_WINDOWS"
        }
    },
    "bin": "jamstream.exe",
    "suggest": {
        "Desktop app and the jamstreamd session server": "jamstream/jamstream-app"
    },
    "checkver": {
        "github": "https://github.com/$REPO"
    },
    "autoupdate": {
        "architecture": {
            "64bit": {
                "url": "https://github.com/$REPO/releases/download/v\$version/jamstream-cli-windows-x86_64.zip"
            }
        },
        "hash": {
            "url": "https://github.com/$REPO/releases/download/v\$version/SHA256SUMS"
        }
    }
}
SCOOP_CLI

cat > "$OUT_DIR/scoop/bucket/jamstream-app.json" <<SCOOP_APP
{
    "##": "$GENERATED",
    "version": "$VERSION",
    "description": "Host a short-lived jam server in your own cloud account and play together",
    "homepage": "$HOMEPAGE",
    "license": "MIT|Apache-2.0",
    "architecture": {
        "64bit": {
            "url": "https://github.com/$REPO/releases/download/$TAG/jamstream-app-windows-x86_64.zip",
            "hash": "$SHA_APP_WINDOWS"
        }
    },
    "bin": [
        "jamstream-app.exe",
        "jamstreamd.exe"
    ],
    "shortcuts": [
        [
            "jamstream-app.exe",
            "JamStream",
            "",
            "jamstream.ico"
        ]
    ],
    "checkver": {
        "github": "https://github.com/$REPO"
    },
    "autoupdate": {
        "architecture": {
            "64bit": {
                "url": "https://github.com/$REPO/releases/download/v\$version/jamstream-app-windows-x86_64.zip"
            }
        },
        "hash": {
            "url": "https://github.com/$REPO/releases/download/v\$version/SHA256SUMS"
        }
    }
}
SCOOP_APP

# ------------------------------------------------------------------- check
if [ "$MODE" = check ]; then
  DRIFT="$TMP/drift"
  : > "$DRIFT"
  # Every rendered file must match its committed copy byte for byte.
  (cd "$OUT_DIR" && find . -type f) | sort | while IFS= read -r rel; do
    rel=${rel#./}
    cmp -s "$OUT_DIR/$rel" "$TARGET_DIR/$rel" 2> /dev/null ||
      printf '%s\n' "$rel" >> "$DRIFT"
  done
  # And the committed channel directories must hold nothing this render
  # did not produce, or a winget version directory the render would have
  # pruned survives the check.
  for channel in homebrew winget aur scoop; do
    [ -d "$TARGET_DIR/$channel" ] || continue
    (cd "$TARGET_DIR" && find "$channel" -type f) | sort | while IFS= read -r rel; do
      [ -f "$OUT_DIR/$rel" ] || printf '%s\n' "$rel" >> "$DRIFT"
    done
  done
  if [ -s "$DRIFT" ]; then
    {
      printf 'render-packaging: %s does not match what this script renders for %s:\n' \
        "$TARGET_DIR" "$TAG"
      sort -u "$DRIFT" | sed 's/^/  /'
      printf 'Run scripts/render-packaging.sh %s and commit the result.\n' "$TAG"
    } >&2
    exit 1
  fi
  printf 'packaging: every committed manifest matches the %s release\n' "$TAG"
  exit 0
fi

printf 'wrote:\n'
printf '  %s\n' \
  "$OUT_DIR/homebrew/Casks/jamstream.rb" \
  "$OUT_DIR/homebrew/Formula/jamstream-cli.rb" \
  "$WINGET_DIR/SeanReid.JamStream.yaml" \
  "$WINGET_DIR/SeanReid.JamStream.installer.yaml" \
  "$WINGET_DIR/SeanReid.JamStream.locale.en-US.yaml" \
  "$OUT_DIR/aur/jamstream-bin/PKGBUILD" \
  "$OUT_DIR/aur/jamstream-bin/.SRCINFO" \
  "$OUT_DIR/aur/jamstream-cli-bin/PKGBUILD" \
  "$OUT_DIR/aur/jamstream-cli-bin/.SRCINFO" \
  "$OUT_DIR/scoop/bucket/jamstream.json" \
  "$OUT_DIR/scoop/bucket/jamstream-app.json"
