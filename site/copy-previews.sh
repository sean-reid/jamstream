#!/bin/sh
# Copies the curated UI previews into the book. Regenerate previews first:
#   cargo test -p jamstream-client --test ui_snapshots
# Default: refresh site/src/images/ from target/ui-previews/.
# --check: compare byte for byte and exit nonzero naming any image that
# differs, so a UI change that regenerates baselines must refresh these
# committed copies in the same change.
set -eu
cd "$(dirname "$0")"
SRC="../target/ui-previews"
DST="src/images"
CURATED="home_empty.png session_demo.png session_full.png wizard_region.png wizard_done.png"
[ -d "$SRC" ] || { echo "no $SRC; run the ui_snapshots test first" >&2; exit 1; }
status=0
for f in $CURATED; do
  if [ "${1:-}" = "--check" ]; then
    cmp -s "$SRC/$f" "$DST/$f" || { echo "stale docs image: $DST/$f differs from $SRC/$f" >&2; status=1; }
  else
    cp "$SRC/$f" "$DST/$f" && echo "copied $f"
  fi
done
exit $status
