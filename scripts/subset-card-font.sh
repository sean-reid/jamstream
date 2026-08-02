#!/usr/bin/env bash
# Regenerates fonts/StageDigits-Regular.ttf, the digits-only face the
# broadcast card draws the listener count with, from the full IBM Plex Mono
# beside it. The full face stays where it is: the desktop app draws arbitrary
# monospace text with the same file.
#
# The subset is committed, so this script is not part of any build. What holds
# it to its source is crates/broadcast/tests/card_font.rs, which rasterises
# every supported glyph out of both faces and compares.
#
# The rename is not cosmetic. IBM Plex is licensed OFL 1.1 with the reserved
# font name "Plex", so a modified version may not carry it. IBM's copyright
# and the license text stay in the name table, and fonts/IBMPlexMono-OFL.txt
# covers the derivative.
#
# Byte output depends on the fontTools version, so --check is a local
# convenience and not a CI gate; the Rust test is the contract.
set -euo pipefail

# fontTools stamps head.modified with the wall clock unless this is set, which
# would make every regeneration a different file.
export SOURCE_DATE_EPOCH=0

usage() {
    echo "usage: ${0##*/} [--check]" >&2
    exit 2
}

check_only=false
case "${1-}" in
    --check) check_only=true ;;
    "") ;;
    *) usage ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
src="$root/fonts/IBMPlexMono-Regular.ttf"
dst="$root/fonts/StageDigits-Regular.ttf"

# Every character the card can put through this face. render.rs draws exactly
# one mono string, `listeners.to_string()` for a usize, so the set is the ten
# digits; the " listening" label beside it is sans.
glyphs="0123456789"

if ! python3 -c "import fontTools" 2>/dev/null; then
    echo "needs fonttools: pip install fonttools" >&2
    exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
out="$tmp/subset.ttf"

# Hinting goes because the renderer is ab_glyph, which ignores it. Layout
# tables go because ab_glyph does no shaping and reads kerning out of the
# legacy `kern` table, which this face does not have.
python3 -m fontTools.subset "$src" \
    --text="$glyphs" \
    --no-hinting \
    --layout-features= \
    --drop-tables+=DSIG,meta,GDEF,GSUB,GPOS \
    --name-IDs=0,1,2,4,6,13,14 \
    --output-file="$out"

python3 - "$out" <<'PY'
import sys
from fontTools.ttLib import TTFont

font = TTFont(sys.argv[1])
name = font["name"]
for record in list(name.names):
    if record.nameID in (1, 4):
        name.setName("Stage Digits", record.nameID, record.platformID,
                     record.platEncID, record.langID)
    elif record.nameID == 6:
        name.setName("StageDigits-Regular", record.nameID, record.platformID,
                     record.platEncID, record.langID)
    elif record.nameID == 2:
        name.setName("Regular", record.nameID, record.platformID,
                     record.platEncID, record.langID)
for record in [r for r in name.names if r.nameID == 0]:
    name.setName("A digits-only subset of IBM Plex Mono.", 10,
                 record.platformID, record.platEncID, record.langID)
font.save(sys.argv[1])
PY

if $check_only; then
    if cmp -s "$out" "$dst"; then
        echo "fonts/StageDigits-Regular.ttf matches its source"
    else
        echo "fonts/StageDigits-Regular.ttf is stale; run ${0##*/}" >&2
        exit 1
    fi
else
    cp "$out" "$dst"
    echo "wrote fonts/StageDigits-Regular.ttf ($(wc -c <"$dst" | tr -d ' ') bytes)"
fi
