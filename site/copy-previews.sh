#!/bin/sh
# Refreshes the screenshots the book shows from the previews the snapshot
# suite renders. Regenerate those first:
#   cargo test -p jamstream-client --test ui_snapshots
#
# That run owns publishable.txt: the first snapshot_for_docs of the run
# truncates it, so a fixture that stopped being publishable, or that was
# deleted outright, leaves no line behind for the manifest check below to
# keep passing against. Under nextest, where each test is its own process,
# the file stays append-only and can only list too many names; render with
# cargo test, as docs-check.yml does.
# Default: copy. --check: compare byte for byte and fail naming what differs,
# so a UI change that moves a baseline must refresh the committed copy in the
# same change.
#
# WHICH IMAGES: read out of the markdown, not listed here. An earlier version
# kept a hand-written CURATED list, which was a second place to remember
# something the book already says. It cost us: one change added an image to
# that list, another added the publishable gate below knowing only the images
# curated when it was written, both were green alone, and main broke when they
# met. Now referencing an image in a page is the whole act of publishing it.
#
# WHICH ARE ALLOWED: a generated image reaches the book only if its fixture
# called snapshot_for_docs rather than snapshot, which records it in the
# manifest. Three published screenshots misrepresented the product before that
# existed: the wizard preview showed the development fallback with artifact
# fields and a dead Launch button, every destinations image showed a host with
# no Invites button, and the region step showed DigitalOcean egress at
# $0.00/GB. Each came from a fixture that stubbed something for a test's
# convenience, decided by someone with no reason to know the same file was on
# a guide page. A snapshot test cannot catch that, because an accepted
# baseline passes forever whatever it shows.
set -eu
cd "$(dirname "$0")"
SRC="../target/ui-previews"
DST="src/images"
MANIFEST="$SRC/publishable.txt"

[ -d "$SRC" ] || { echo "no $SRC; run the ui_snapshots test first" >&2; exit 1; }
[ -f "$MANIFEST" ] || { echo "no $MANIFEST; run the ui_snapshots test first" >&2; exit 1; }

# Every images/*.png the book links to, from any page, deduplicated.
REFERENCED=$(grep -rhoE '\]\((\.\./)*images/[a-z0-9_]+\.png\)' --include='*.md' src \
  | sed -E 's#.*images/##; s#\)##' | sort -u)

status=0
managed=""
for f in $REFERENCED; do
  if [ -f "$SRC/$f" ]; then
    # Rendered by the snapshot suite, so it is ours to keep current.
    managed="$managed $f"
    if ! grep -qxF "$f" "$MANIFEST"; then
      echo "not publishable: $DST/$f is shown in the book but its fixture calls snapshot(), not snapshot_for_docs()" >&2
      echo "  Either it renders what a release build really shows, in which case say so at the fixture, or it should not be on the website." >&2
      status=1
    fi
  elif [ -f "$DST/$f" ]; then
    : # A static asset, not from the snapshot suite. Not ours to manage.
  else
    echo "broken image reference: the book links images/$f and nothing provides it" >&2
    status=1
  fi
done

[ "$status" -eq 0 ] || exit "$status"

for f in $managed; do
  if [ "${1:-}" = "--check" ]; then
    cmp -s "$SRC/$f" "$DST/$f" || { echo "stale docs image: $DST/$f differs from $SRC/$f" >&2; status=1; }
  else
    cp "$SRC/$f" "$DST/$f" && echo "copied $f"
  fi
done

# A committed screenshot no page shows is dead weight that still gets reviewed
# and still rots. A warning rather than an error, so that adding an image and
# the page that shows it can be two commits.
for path in "$DST"/*.png; do
  [ -e "$path" ] || continue
  f=$(basename "$path")
  [ -f "$SRC/$f" ] || continue
  case " $managed " in
    *" $f "*) ;;
    *) echo "warning: $DST/$f is committed but no page shows it" >&2 ;;
  esac
done

exit $status
