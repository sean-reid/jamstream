#!/usr/bin/env bash
# Builds a plain-layout DMG: the app plus an /Applications symlink, no
# background image or window styling yet.
#
#   usage: macos-dmg.sh <path/to/JamStream.app> <out.dmg>
#
# release.yml calls this AFTER notarization+stapling so the DMG carries the
# stapled ticket; ditto (not cp) preserves the extended attributes the
# staple lives in. The caller signs the resulting DMG.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <path/to/JamStream.app> <out.dmg>" >&2
  exit 2
fi

APP=$1
OUT=$2

if [ ! -d "$APP" ]; then
  echo "error: app bundle not found: $APP" >&2
  exit 1
fi

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

ditto "$APP" "$STAGE/$(basename "$APP")"
ln -s /Applications "$STAGE/Applications"

rm -f "$OUT"
hdiutil create -volname JamStream -srcfolder "$STAGE" -ov -format UDZO "$OUT"
echo "built $OUT"
