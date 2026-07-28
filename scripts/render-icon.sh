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
# builds and docs need no render step.
#
# Modes:
#   (no args)  render everything in place. Needs rsvg-convert; the .icns
#              step needs iconutil (macOS) and the .ico step needs magick,
#              and each is skipped with a warning when its tool is absent
#              so the PNG and favicon outputs still regenerate anywhere.
#   --check    assert the committed assets were rendered from the SVG as it
#              stands. Needs no rendering tools at all, which is what lets
#              docs-check run it on a Linux runner. This is what CI runs.
#
# WHAT --check ACTUALLY PROVES, because the difference matters:
# scripts/render-palette.sh already fails when a palette change has not
# reached jamstream.svg, so after any recolour the SVG has moved. The gap
# was between the SVG and the assets derived from it: an old-coloured
# icns, ico, tarball PNG and favicon shipped with docs-check green.
#
# theme/favicon.svg is a verbatim copy of the source SVG, so it is a
# content stamp for the whole render pass: the script writes every output
# in one go, so a favicon.svg that matches means the pass ran on this SVG,
# and one that does not means it did not. Comparing the rasters byte for
# byte was the other option and it was rejected: rsvg output is not stable
# across librsvg versions, so that gate would go red whenever GitHub
# updated a runner image, and a gate that cries wolf is worse than none.
#
# So the honest limit: hand-copying the SVG over favicon.svg without
# rerunning the render defeats this, the same way editing any stamp does.
# Everything short of that it catches.
set -eu

MODE=render
case "${1:-}" in
  '') ;;
  --check) MODE=check ;;
  *)
    echo "usage: $0 [--check]" >&2
    exit 2
    ;;
esac

ROOT=$(cd "$(dirname "$0")/.." && pwd)
SVG="$ROOT/crates/client/assets/icon/jamstream.svg"
ICON_DIR="$ROOT/crates/client/assets/icon"
THEME_DIR="$ROOT/site/theme"

if [ ! -f "$SVG" ]; then
  echo "error: source SVG not found: $SVG" >&2
  exit 1
fi

if [ "$MODE" = check ]; then
  STATUS=0
  # The stamp. Everything else in the render pass is written beside it.
  if ! cmp -s "$SVG" "$THEME_DIR/favicon.svg"; then
    echo "render-icon: site/theme/favicon.svg is not a copy of" \
      "crates/client/assets/icon/jamstream.svg, so the icns, the ico," \
      "jamstream-512.png and favicon.png were all rendered from an older" \
      "icon. Run scripts/render-icon.sh (on macOS, with librsvg and" \
      "imagemagick installed, so the icns and the ico are rendered too) and" \
      "commit the result." >&2
    STATUS=1
  fi
  # Each committed asset exists, is not empty, and is the format its name
  # claims. A truncated or wrong-typed asset is not something the stamp can
  # see, and it is what a botched render leaves behind.
  #
  # magic <file> <od-prefix> <label>
  magic() {
    if [ ! -s "$1" ]; then
      echo "render-icon: $3 is missing or empty" >&2
      STATUS=1
      return
    fi
    got=$(od -An -tx1 -N 8 "$1" | tr -d ' \n')
    case "$got" in
      "$2"*) ;;
      *)
        echo "render-icon: $3 does not start with the $2 header of its format (got $got)" >&2
        STATUS=1
        ;;
    esac
  }
  magic "$ICON_DIR/jamstream-512.png" 89504e470d0a1a0a crates/client/assets/icon/jamstream-512.png
  magic "$THEME_DIR/favicon.png" 89504e470d0a1a0a site/theme/favicon.png
  # 'icns' in ascii, then the container length.
  magic "$ICON_DIR/jamstream.icns" 69636e73 crates/client/assets/icon/jamstream.icns
  # ICONDIR: reserved 0, type 1 (icon), then the image count, little endian.
  magic "$ICON_DIR/jamstream.ico" 00000100 crates/client/assets/icon/jamstream.ico
  if [ "$STATUS" -eq 0 ]; then
    echo "icon: every committed asset was rendered from the current jamstream.svg"
  fi
  exit "$STATUS"
fi

if ! command -v rsvg-convert >/dev/null 2>&1; then
  echo "error: rsvg-convert is required (brew install librsvg)" >&2
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
