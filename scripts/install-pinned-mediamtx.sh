#!/bin/sh
# Installs the mediamtx a session VM runs, into the directory given as the
# first argument.
#
# Same pin file and same reasoning as install-pinned-ffmpeg.sh: the VM
# downloads the artifact named in crates/cloud/data/media_artifacts.json at
# boot and refuses to use it on a sha256 mismatch, so that build is the only
# relay whose behaviour is a fact about the product. There is one version
# here, read from the file cloud-init reads, because a second copy of "v1.19.3"
# in a workflow would drift from the VM's the first time the pin moves, and
# the relay_chain test would then judge the shipped relay config against a
# release that never runs a session.
#
# Linux only, for the same reason: the pinned artifacts are Linux binaries.
set -eu

DEST=${1:-}
if [ -z "$DEST" ]; then
  echo "usage: $0 <destination-directory>" >&2
  exit 2
fi

if [ "$(uname -s)" != Linux ]; then
  echo "$0 installs Linux binaries; run it on Linux, or in a container. For local work, mediamtx publishes a darwin build of the same version." >&2
  exit 2
fi

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PINS="$ROOT/crates/cloud/data/media_artifacts.json"

if [ ! -f "$PINS" ]; then
  echo "no pin file at $PINS" >&2
  exit 1
fi

ARCH=$(uname -m)
case "$ARCH" in
  arm64) ARCH=aarch64 ;;
esac

URL=$(jq -r --arg a "$ARCH" '.mediamtx.targets[$a].url // empty' "$PINS")
SHA=$(jq -r --arg a "$ARCH" '.mediamtx.targets[$a].sha256 // empty' "$PINS")
VERSION=$(jq -r '.mediamtx.version' "$PINS")
if [ -z "$URL" ] || [ -z "$SHA" ]; then
  echo "no mediamtx pin for $ARCH in $PINS" >&2
  exit 1
fi

echo "pinned mediamtx $VERSION for $ARCH"
echo "  $URL"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT INT TERM

curl -fsSL --proto '=https' --tlsv1.2 -o "$WORK/mediamtx.tar.gz" "$URL"

GOT=$(sha256sum "$WORK/mediamtx.tar.gz" | cut -d' ' -f1)
if [ "$GOT" != "$SHA" ]; then
  echo "sha256 mismatch for the pinned mediamtx." >&2
  echo "  pinned:   $SHA" >&2
  echo "  download: $GOT" >&2
  echo "A session VM refuses to boot on this, so CI refuses to test on it. Either the URL is not immutable or the pin is wrong; see the _notes in $PINS." >&2
  exit 1
fi

mkdir -p "$DEST"
# The archive is flat: the binary, a LICENSE, and the upstream sample config.
# Only the binary is wanted; the config under test is the one cloud-init
# generates, not upstream's.
tar -xzf "$WORK/mediamtx.tar.gz" -C "$DEST" mediamtx

if [ ! -x "$DEST/mediamtx" ]; then
  echo "the pinned archive did not contain mediamtx" >&2
  exit 1
fi
# Run it, and mind the status, for the reason the ffmpeg installer gives: a
# binary that cannot execute here would otherwise install "successfully" and
# fail later as a missing relay.
if ! "$DEST/mediamtx" --version > "$WORK/mediamtx.version" 2>&1; then
  echo "the pinned mediamtx does not run on this machine:" >&2
  cat "$WORK/mediamtx.version" >&2
  exit 1
fi
head -1 "$WORK/mediamtx.version"
