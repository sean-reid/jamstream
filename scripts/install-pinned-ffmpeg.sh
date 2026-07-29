#!/bin/sh
# Installs the ffmpeg and ffprobe a session VM runs, into the directory given
# as the first argument.
#
# The VM downloads the artifact named in crates/cloud/data/media_artifacts.json
# at boot and refuses to use it on a sha256 mismatch. That build, not the one
# apt happens to ship, is the only ffmpeg whose behaviour is a fact about the
# product: broadcast either works against it or it does not. Testing the
# encoder against a runner's distro ffmpeg answers a question nobody asked, and
# can pass while the shipped configuration is broken.
#
# Verifying the digest here is a second gate for free: a pin edited to a URL
# whose content moved fails in CI instead of at somebody's session boot.
#
# Linux only, because the pinned artifacts are Linux binaries, which is also
# the only place the pipeline runs.
set -eu

DEST=${1:-}
if [ -z "$DEST" ]; then
  echo "usage: $0 <destination-directory>" >&2
  exit 2
fi

# Said up front, because the first thing that fails elsewhere is bsdtar
# rejecting --wildcards, and a tar usage message is not an explanation.
if [ "$(uname -s)" != Linux ]; then
  echo "$0 installs Linux binaries and needs GNU tar; run it on Linux, or in a container. On a mac, brew's ffmpeg@7 is the same upstream version for local work." >&2
  exit 2
fi

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PINS="$ROOT/crates/cloud/data/media_artifacts.json"

if [ ! -f "$PINS" ]; then
  echo "no pin file at $PINS" >&2
  exit 1
fi

# uname -m says aarch64 on Linux; the pin file keys on that and on x86_64.
ARCH=$(uname -m)
case "$ARCH" in
  arm64) ARCH=aarch64 ;;
esac

URL=$(jq -r --arg a "$ARCH" '.ffmpeg.targets[$a].url // empty' "$PINS")
SHA=$(jq -r --arg a "$ARCH" '.ffmpeg.targets[$a].sha256 // empty' "$PINS")
VERSION=$(jq -r '.ffmpeg.version' "$PINS")
if [ -z "$URL" ] || [ -z "$SHA" ]; then
  echo "no ffmpeg pin for $ARCH in $PINS" >&2
  exit 1
fi

echo "pinned ffmpeg $VERSION for $ARCH"
echo "  $URL"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT INT TERM

curl -fsSL --proto '=https' --tlsv1.2 -o "$WORK/ffmpeg.tar.xz" "$URL"

GOT=$(sha256sum "$WORK/ffmpeg.tar.xz" | cut -d' ' -f1)
if [ "$GOT" != "$SHA" ]; then
  echo "sha256 mismatch for the pinned ffmpeg." >&2
  echo "  pinned:   $SHA" >&2
  echo "  download: $GOT" >&2
  echo "A session VM refuses to boot on this, so CI refuses to test on it. Either the URL is not immutable or the pin is wrong; see the _notes in $PINS." >&2
  exit 1
fi

mkdir -p "$DEST"
# The archive is <name>/bin/{ffmpeg,ffprobe,ffplay}; take the two we use and
# drop the directory prefix. ffprobe is the judge in the real_ffmpeg test, so
# it has to come from the same build as the encoder.
tar -xJf "$WORK/ffmpeg.tar.xz" -C "$DEST" --strip-components=2 \
  --wildcards '*/bin/ffmpeg' '*/bin/ffprobe'

for tool in ffmpeg ffprobe; do
  if [ ! -x "$DEST/$tool" ]; then
    echo "the pinned archive did not contain bin/$tool" >&2
    exit 1
  fi
  # Run it, and mind the status. Piping the version straight into `head` would
  # report the pipe's status instead, so a binary that cannot execute here at
  # all would install "successfully" and fail later as a missing encoder.
  if ! "$DEST/$tool" -version > "$WORK/$tool.version" 2>&1; then
    echo "the pinned $tool does not run on this machine:" >&2
    cat "$WORK/$tool.version" >&2
    exit 1
  fi
  head -1 "$WORK/$tool.version"
done
