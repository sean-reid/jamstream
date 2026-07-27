#!/bin/sh
# Renders every derived app-icon asset from the single source of record,
# crates/client/assets/icon/jamstream.svg. The SVG is the only file a
# design change edits; everything else here is derived and this script is
# idempotent (rerunning it regenerates the same outputs in place).
#
# Outputs:
#   crates/client/assets/icon/jamstream-{16,32,64,128,256,512,1024}.png
#       rendered with rsvg-convert. Only the 512 is committed (the eframe
#       window icon embeds it via include_bytes! and the Linux tarball
#       ships it); the other sizes are regenerable intermediates and are
#       gitignored.
#   crates/client/assets/icon/jamstream.icns   (committed; macOS only:
#       iconutil over an .iconset with 16/32/128/256/512 plus @2x each)
#   crates/client/assets/icon/jamstream.ico    (committed; requires
#       ImageMagick's `magick`; contains 16/32/48/64/128/256)
#   site/theme/favicon.png (32) and site/theme/favicon.svg (committed;
#       mdBook 0.5 picks both up from theme/ automatically)
#
# The .icns, .ico, favicon files, and 512 png are committed so release
# builds and docs need no render step; CI never runs this script.
#
# Requires rsvg-convert. The .icns step needs iconutil (macOS) and the
# .ico step needs magick; each is skipped with a warning when its tool is
# missing so the PNG/favicon outputs still regenerate anywhere.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
SVG="$ROOT/crates/client/assets/icon/jamstream.svg"
ICON_DIR="$ROOT/crates/client/assets/icon"
THEME_DIR="$ROOT/site/theme"

if ! command -v rsvg-convert >/dev/null 2>&1; then
  echo "error: rsvg-convert is required (brew install librsvg)" >&2
  exit 1
fi

if [ ! -f "$SVG" ]; then
  echo "error: source SVG not found: $SVG" >&2
  exit 1
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

render() {
  # render <size> <output-path>
  rsvg-convert -w "$1" -h "$1" "$SVG" -o "$2"
}

# PNGs at every distributed size. Sizes other than 512 are regenerable
# intermediates (see the .gitignore beside them).
for SIZE in 16 32 64 128 256 512 1024; do
  render "$SIZE" "$ICON_DIR/jamstream-$SIZE.png"
  echo "rendered $ICON_DIR/jamstream-$SIZE.png"
done

# macOS .icns: Apple's iconset layout wants each point size plus its @2x
# pixel double.
if command -v iconutil >/dev/null 2>&1; then
  ICONSET="$TMP/jamstream.iconset"
  mkdir -p "$ICONSET"
  for SIZE in 16 32 128 256 512; do
    render "$SIZE" "$ICONSET/icon_${SIZE}x${SIZE}.png"
    render $((SIZE * 2)) "$ICONSET/icon_${SIZE}x${SIZE}@2x.png"
  done
  iconutil -c icns "$ICONSET" -o "$ICON_DIR/jamstream.icns"
  echo "rendered $ICON_DIR/jamstream.icns"
else
  echo "warning: iconutil not found (not macOS?); skipping jamstream.icns" >&2
fi

# Windows .ico: one container with every size Explorer and the taskbar
# pick from.
if command -v magick >/dev/null 2>&1; then
  for SIZE in 16 32 48 64 128 256; do
    render "$SIZE" "$TMP/ico-$SIZE.png"
  done
  magick "$TMP/ico-16.png" "$TMP/ico-32.png" "$TMP/ico-48.png" \
    "$TMP/ico-64.png" "$TMP/ico-128.png" "$TMP/ico-256.png" \
    "$ICON_DIR/jamstream.ico"
  echo "rendered $ICON_DIR/jamstream.ico"
else
  echo "warning: magick not found; skipping jamstream.ico" >&2
fi

# mdBook favicons: theme/favicon.png and theme/favicon.svg are picked up
# automatically by mdBook 0.5.
render 32 "$THEME_DIR/favicon.png"
cp "$SVG" "$THEME_DIR/favicon.svg"
echo "rendered $THEME_DIR/favicon.png and favicon.svg"
