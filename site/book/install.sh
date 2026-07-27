#!/bin/sh
# JamStream CLI installer. This file lives in site/src/ so mdBook copies it
# verbatim into the built site, which serves it at /install.sh:
#
#   curl -fsSL https://sean-reid.github.io/jamstream/install.sh | sh
#
# Downloads the jamstream CLI from the latest GitHub release, verifies its
# sha256 against the release's SHA256SUMS file, and installs it.
#
# Options and environment:
#   --with-server           also install the jamstreamd session server
#                           (published for Linux x86_64, musl, only)
#   JAMSTREAM_INSTALL_DIR   install into this directory instead of the
#                           default (/usr/local/bin when writable,
#                           otherwise ~/.local/bin)
#
# POSIX sh. No colors beyond bold on a terminal. Checked by shellcheck in
# CI (.github/workflows/docs-check.yml).

set -eu

REPO="sean-reid/jamstream"
BASE_URL="https://github.com/${REPO}/releases/latest/download"
SITE_URL="https://sean-reid.github.io/jamstream"

if [ -t 1 ]; then
  bold=$(tput bold 2>/dev/null) || bold=""
  normal=$(tput sgr0 2>/dev/null) || normal=""
else
  bold=""
  normal=""
fi

say() { printf '%s\n' "$1"; }
fail() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

usage() {
  say "usage: install.sh [--with-server]"
  say ""
  say "Installs the jamstream CLI from the latest release."
  say "  --with-server   also install the jamstreamd session server"
  say "                  (published for Linux x86_64, musl, only)"
  say ""
  say "Set JAMSTREAM_INSTALL_DIR to choose the install directory."
}

with_server=0
for arg in "$@"; do
  case "$arg" in
    --with-server) with_server=1 ;;
    -h|--help)
      usage
      exit 0
      ;;
    *) fail "unknown option: $arg (this script takes --with-server only)" ;;
  esac
done

# Platform detection. The macOS CLI archive is a universal binary, so both
# Mac architectures map to the same asset.
os=$(uname -s)
arch=$(uname -m)

case "$os" in
  Darwin) os=darwin ;;
  Linux) os=linux ;;
  MINGW*|MSYS*|CYGWIN*)
    fail "this is the POSIX installer; on Windows run:
  powershell -ExecutionPolicy Bypass -c \"irm ${SITE_URL}/install.ps1 | iex\"" ;;
  *) fail "unsupported operating system: $os (releases cover macOS, Linux, and Windows)" ;;
esac

case "$arch" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=arm64 ;;
  *) fail "unsupported architecture: $arch (releases cover x86_64 and arm64)" ;;
esac

if [ "$os" = darwin ]; then
  cli_asset="jamstream-cli-macos-universal.tar.gz"
elif [ "$arch" = x86_64 ]; then
  cli_asset="jamstream-cli-linux-x86_64.tar.gz"
else
  fail "no Linux arm64 CLI build is published yet; build from source instead:
  cargo install --path crates/cli   (in a clone of https://github.com/${REPO})"
fi

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

if command -v sha256sum >/dev/null 2>&1; then
  sha_tool=sha256sum
elif command -v shasum >/dev/null 2>&1; then
  sha_tool=shasum
else
  fail "neither sha256sum nor shasum is available, so the download cannot be verified"
fi

hash_of() {
  if [ "$sha_tool" = sha256sum ]; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

# fetch <asset> <dest>: returns 1 on HTTP 404 (a missing asset or, for the
# very first fetch, no release at all); dies on any other failure. The HTTP
# status is captured explicitly because curl's --fail exit code for a 404
# varies with the HTTP version GitHub negotiates.
fetch() {
  fetch_status=0
  http_code=$(curl -sSL --proto '=https' -o "$2" -w '%{http_code}' "${BASE_URL}/$1") || fetch_status=$?
  if [ "$fetch_status" -ne 0 ]; then
    fail "download failed for ${BASE_URL}/$1 (curl exit $fetch_status); check your network and retry"
  fi
  case "$http_code" in
    200) return 0 ;;
    404)
      rm -f "$2"
      return 1
      ;;
    *) fail "download failed for ${BASE_URL}/$1 (HTTP $http_code); retry later" ;;
  esac
}

# install_binary <asset> <binary> <destdir>: download, verify, extract, place.
install_binary() {
  if ! fetch "$1" "$tmp/$1"; then
    fail "the latest release has no asset named $1; if a release was just published, its uploads may still be running, so retry in a few minutes"
  fi
  expected=$(awk -v name="$1" '{ f = $2; sub(/^\*/, "", f); if (f == name) print $1 }' "$tmp/SHA256SUMS")
  [ -n "$expected" ] || fail "SHA256SUMS has no entry for $1"
  actual=$(hash_of "$tmp/$1")
  [ "$expected" = "$actual" ] || fail "checksum mismatch for $1
  expected $expected
  got      $actual
Delete the download and retry; if it repeats, report it."
  tar -xzf "$tmp/$1" -C "$tmp"
  [ -f "$tmp/$2" ] || fail "the archive $1 did not contain a $2 binary"
  cp "$tmp/$2" "$3/.$2.new"
  chmod 755 "$3/.$2.new"
  mv -f "$3/.$2.new" "$3/$2"
  say "installed ${bold}$3/$2${normal} (sha256 verified)"
}

if [ -n "${JAMSTREAM_INSTALL_DIR:-}" ]; then
  dest="$JAMSTREAM_INSTALL_DIR"
  mkdir -p "$dest" || fail "cannot create $dest"
  [ -w "$dest" ] || fail "JAMSTREAM_INSTALL_DIR=$dest is not writable"
elif [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
  dest=/usr/local/bin
else
  dest="${HOME}/.local/bin"
  mkdir -p "$dest"
fi

say "${bold}JamStream installer${normal}"
say "platform: $os $arch, asset: $cli_asset"
say "install directory: $dest"

if ! fetch SHA256SUMS "$tmp/SHA256SUMS"; then
  say ""
  say "No JamStream release has been published yet, so there is nothing to download."
  say "The repository builds from source (Rust toolchain required):"
  say "  git clone https://github.com/${REPO} && cd jamstream"
  say "  cargo install --path crates/cli"
  say "This script starts working with the first release."
  exit 1
fi

install_binary "$cli_asset" jamstream "$dest"

if [ "$with_server" -eq 1 ]; then
  if [ "$os" = linux ] && [ "$arch" = x86_64 ]; then
    install_binary "jamstreamd-linux-x86_64-musl.tar.gz" jamstreamd "$dest"
  else
    say "jamstreamd binaries are published for Linux x86_64 (musl) only."
    say "On this platform, local mode uses the jamstreamd that ships next to"
    say "the desktop app, or a from-source build: cargo install --path crates/server"
  fi
fi

case ":$PATH:" in
  *":$dest:"*) ;;
  *)
    say ""
    say "note: $dest is not on your PATH. Add it, for example:"
    say "  export PATH=\"$dest:\$PATH\""
    ;;
esac

say ""
say "Done. Check the install with: jamstream --version"
