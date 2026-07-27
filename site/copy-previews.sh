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
CURATED="home_empty.png session_demo.png session_full.png session_settings.png wizard_provider.png wizard_setup_digitalocean.png wizard_region.png wizard_preview.png session_invites.png session_destinations_live_two.png session_destinations_failed.png"
[ -d "$SRC" ] || { echo "no $SRC; run the ui_snapshots test first" >&2; exit 1; }

# A baseline reaches the website only if its fixture declared it fit to,
# by calling snapshot_for_docs instead of snapshot. Three published
# screenshots misrepresented the product before this existed: the wizard
# preview showed the development fallback with artifact fields and a dead
# Launch button, every destinations image showed a host with no Invites
# button, and the region step showed DigitalOcean egress at $0.00/GB. Each
# came from a fixture that stubbed something for the convenience of a test,
# by an author who had no reason to know the same file was on the docs site.
# A snapshot test cannot catch that, because an accepted baseline passes
# forever whatever it shows, so the decision is pushed back to the fixture
# and this checks it was made.
#
# CI starts from a clean target dir, so the manifest there is exactly this
# run. Locally it accumulates across runs, which only ever makes it more
# permissive: if you DEMOTE a snapshot out of the docs set, delete
# target/ui-previews/publishable.txt before trusting this.
MANIFEST="$SRC/publishable.txt"
[ -f "$MANIFEST" ] || { echo "no $MANIFEST; run the ui_snapshots test first" >&2; exit 1; }
undeclared=""
for f in $CURATED; do
  grep -qxF "$f" "$MANIFEST" || undeclared="$undeclared $f"
done
if [ -n "$undeclared" ]; then
  for f in $undeclared; do
    echo "not publishable: $f is in CURATED but its fixture calls snapshot(), not snapshot_for_docs()" >&2
  done
  echo "Either the fixture renders what a release build really shows, in which case say so with snapshot_for_docs, or it does not, in which case it should not be on the website." >&2
  exit 1
fi

status=0
for f in $CURATED; do
  if [ "${1:-}" = "--check" ]; then
    cmp -s "$SRC/$f" "$DST/$f" || { echo "stale docs image: $DST/$f differs from $SRC/$f" >&2; status=1; }
  else
    cp "$SRC/$f" "$DST/$f" && echo "copied $f"
  fi
done
exit $status
