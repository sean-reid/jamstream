#!/bin/sh
# Refreshes the fonts the book serves from fonts/ at the root of the tree,
# which is the one copy of them: the app and the stream card embed those same
# files through include_bytes!.
#
# These used to be symlinks. Windows git does not create symlinks without
# Developer Mode or core.symlinks, so a clone there wrote each one as a text
# file holding its target path, mdBook copied that into the book as a font,
# the browser silently fell back, and nothing anywhere reported an error.
# Every job that builds the site runs on Linux or macOS, so CI could not see
# it either. A copy needs no filesystem feature half our platforms disable.
#
# Default: copy. --check: compare byte for byte and fail naming what differs,
# so a font change has to reach the book in the same change that makes it.
#
# WHICH FILES: read out of theme/fonts/fonts.css, not listed here. The
# stylesheet already names every face it loads and the licence beside it, so
# adding a face there is the whole act of shipping it.
set -eu
cd "$(dirname "$0")"
SRC="../fonts"
DST="theme/fonts"
CSS="$DST/fonts.css"

[ -f "$CSS" ] || { echo "no $CSS; mdBook needs it to serve theme fonts at all" >&2; exit 1; }

# Space separated, so the membership test in the sweep below can look for a
# name with a space on either side of it.
WANTED=$(grep -ohE '[A-Za-z0-9]+-[A-Za-z0-9]+\.(ttf|txt)' "$CSS" | sort -u | tr '\n' ' ')
# An unreadable stylesheet would otherwise mean nothing to copy and a green
# --check over an empty set.
[ -n "$WANTED" ] || { echo "$CSS names no font files; it should name at least one" >&2; exit 1; }

status=0
for f in $WANTED; do
  if [ ! -f "$SRC/$f" ]; then
    echo "$CSS names $f and $SRC has no such file" >&2
    status=1
    continue
  fi
  if [ "${1:-}" = "--check" ]; then
    cmp -s "$SRC/$f" "$DST/$f" || { echo "stale theme font: $DST/$f differs from $SRC/$f; run site/copy-fonts.sh" >&2; status=1; }
  else
    cp "$SRC/$f" "$DST/$f" && echo "copied $f"
  fi
done

# A file left here after the stylesheet stopped naming it still ships to
# everyone who loads a page, so it goes rather than warning.
for path in "$DST"/*.ttf "$DST"/*.txt; do
  [ -e "$path" ] || continue
  f=$(basename "$path")
  case " $WANTED " in
    *" $f "*) continue ;;
  esac
  if [ "${1:-}" = "--check" ]; then
    echo "orphan theme font: $DST/$f is committed and $CSS does not name it; run site/copy-fonts.sh" >&2
    status=1
  else
    rm "$path" && echo "removed $f"
  fi
done

exit $status
