#!/bin/sh
# Propagates JamStream's palette out of its single source of truth,
# crates/client/src/theme.rs, into every consumer that cannot read Rust.
#
# A palette value is edited in exactly one place: the DARK and LIGHT
# consts in crates/client/src/theme.rs. This script regenerates:
#
#   site/theme/css/palette.css   every palette entry as a CSS custom
#       property, in hex and as an "r, g, b" triple for rgba() tints.
#       Both palettes are emitted; the dark set is aliased to the
#       unprefixed --js-* names the docs theme actually uses, so a future
#       light docs theme only has to flip those aliases.
#       site/theme/css/variables.css references these and hardcodes no
#       palette hex of its own.
#   site/theme/index.hbs         the <meta name="theme-color"> value,
#       which is the window surface and cannot be a CSS variable.
#   crates/client/assets/icon/jamstream.svg   the fill attributes only.
#       Geometry is untouched, byte for byte.
#
# Modes:
#   (no args)  regenerate in place. Idempotent: a second run is a no-op.
#   --check    regenerate into a temp dir and compare; exits 1 naming
#              every file that drifted. This is what CI runs, so a
#              palette change that is not propagated fails the build.
#
# COUPLING TO theme.rs (the fragile part, deliberately kept dumb): the
# parse is an awk over the `pub const DARK: Palette = Palette { ... };`
# and LIGHT blocks, matching lines spelled exactly
# `name: Color32::from_rgb(0x.., 0x.., 0x..),` (what rustfmt produces).
# Anything computed, aliased, or reformatted is invisible to it, so
# theme.rs carries a comment saying so. The parse fails loudly rather
# than silently emitting a short palette: a block that is missing or that
# yields the wrong number of entries is an error.
#
# COUPLING TO THE ICON: the SVG stays the design source of record. This
# script only normalizes each fill to the palette entry for that shape's
# role, and it recognizes the role from the fill already there rather
# than from a coordinate table, so which segments are lit stays a
# property of the SVG and not of this script:
#   the first <rect> (the squircle plate)  -> surface0
#   an amber fill (the lit peak segment)   -> meter_amber, the one color
#                                             the icon keeps as its own
#                                             because it depicts the
#                                             meter widget
#   any near-white fill (a lit segment)    -> text_primary
#   any near-black fill (an unlit segment) -> surface2
# A fill that fits none of those windows is an error naming the fill;
# adding a shape in a new role means teaching classify_fill about it.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
THEME_RS="$ROOT/crates/client/src/theme.rs"
PALETTE_CSS="$ROOT/site/theme/css/palette.css"
INDEX_HBS="$ROOT/site/theme/index.hbs"
ICON_SVG="$ROOT/crates/client/assets/icon/jamstream.svg"

# Every field of Palette, so a new entry that the generator would
# silently drop is caught here instead of in a code review.
EXPECTED_ENTRIES=12

MODE=generate
case "${1:-}" in
  '') ;;
  --check) MODE=check ;;
  *)
    echo "usage: $0 [--check]" >&2
    exit 2
    ;;
esac

if [ ! -f "$THEME_RS" ]; then
  echo "render-palette: source of truth not found: $THEME_RS" >&2
  exit 1
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

# parse_palette <DARK|LIGHT> -> "name hex r g b" per entry, struct order.
parse_palette() {
  awk -v want="$1" '
    function hexval(s,   i, c, v, d) {
      v = 0
      for (i = 1; i <= length(s); i++) {
        c = tolower(substr(s, i, 1))
        d = index("0123456789abcdef", c) - 1
        if (d < 0) { return -1 }
        v = v * 16 + d
      }
      return v
    }
    $0 == "pub const " want ": Palette = Palette {" { inblock = 1; next }
    inblock && $0 == "};" { inblock = 0 }
    inblock && /Color32::from_rgb\(/ {
      colon = index($0, ":")
      name = substr($0, 1, colon - 1)
      gsub(/[ \t]/, "", name)
      if (split($0, part, /0x/) != 4) { next }
      r = substr(part[2], 1, 2)
      g = substr(part[3], 1, 2)
      b = substr(part[4], 1, 2)
      if (hexval(r) < 0 || hexval(g) < 0 || hexval(b) < 0) { next }
      printf "%s %s%s%s %d %d %d\n", name, r, g, b, hexval(r), hexval(g), hexval(b)
    }
  ' "$THEME_RS"
}

for PALETTE in dark light; do
  UPPER=$(printf '%s' "$PALETTE" | tr '[:lower:]' '[:upper:]')
  parse_palette "$UPPER" > "$TMP/$PALETTE"
  COUNT=$(awk 'END { print NR }' "$TMP/$PALETTE")
  if [ "$COUNT" -ne "$EXPECTED_ENTRIES" ]; then
    echo "render-palette: parsed $COUNT of $EXPECTED_ENTRIES entries from the" \
      "$UPPER palette in $THEME_RS; the format this script parses has" \
      "changed (see the comment above the palettes)" >&2
    exit 1
  fi
done

# field <table> <entry> <column> -> one parsed column, or fail loudly.
field() {
  awk -v key="$2" -v col="$3" '
    $1 == key { print $col; found = 1 }
    END { if (!found) { exit 1 } }
  ' "$1" || {
    echo "render-palette: no '$2' entry in the palette parsed from $THEME_RS" >&2
    exit 1
  }
}

# Resolved up front so a rename in theme.rs fails here, loudly, rather
# than inside a command substitution whose status would be discarded.
DARK_SURFACE0=$(field "$TMP/dark" surface0 2)
DARK_SURFACE2=$(field "$TMP/dark" surface2 2)
DARK_TEXT_PRIMARY=$(field "$TMP/dark" text_primary 2)
DARK_METER_AMBER=$(field "$TMP/dark" meter_amber 2)

# ---------------------------------------------------------------- palette.css

# emit_entries <dark|light>: the hex and rgb-triple pair per entry.
emit_entries() {
  while read -r name hex r g b; do
    css=$(printf '%s' "$name" | tr '_' '-')
    printf '    --js-%s-%s: #%s;\n' "$1" "$css" "$hex"
    printf '    --js-%s-%s-rgb: %s, %s, %s;\n' "$1" "$css" "$r" "$g" "$b"
  done < "$TMP/$1"
}

write_palette_css() {
  cat <<'EOF'
/* GENERATED FILE. Do not edit.
 *
 * Written by scripts/render-palette.sh from the DARK and LIGHT palettes in
 * crates/client/src/theme.rs, which is the single source of truth for
 * JamStream's colors. Change a color there and rerun the script; CI runs
 * it with --check and fails when the two have drifted apart.
 *
 * Every entry appears twice: as a hex for color and background, and as an
 * "r, g, b" triple for rgba() tints, since rgba() cannot take a hex var.
 * The docs site is dark only, so the --js-dark-* set is aliased to the
 * unprefixed --js-* names that theme/css/variables.css consumes; a light
 * docs theme would only have to repoint those aliases.
 */

:root {
EOF
  printf "    /* The app's dark palette, verbatim. */\n"
  emit_entries dark
  printf '\n'
  printf "    /* The app's light palette. Unused by the dark-only docs theme\n"
  printf '     * today; emitted so a light theme has it when it lands. */\n'
  emit_entries light
  printf '\n'
  printf '    /* The palette the docs theme is currently wired to. */\n'
  while read -r name _; do
    css=$(printf '%s' "$name" | tr '_' '-')
    printf '    --js-%s: var(--js-dark-%s);\n' "$css" "$css"
    printf '    --js-%s-rgb: var(--js-dark-%s-rgb);\n' "$css" "$css"
  done < "$TMP/dark"
  printf '}\n'
}

# ----------------------------------------------------------------- index.hbs

# The theme-color meta is the browser chrome color; it is an HTML
# attribute, so it cannot reference a CSS variable and has to be written.
write_index_hbs() {
  awk -v surface0="$DARK_SURFACE0" '
    match($0, /<meta name="theme-color" content="#[0-9a-fA-F]+">/) {
      printf "%s<meta name=\"theme-color\" content=\"#%s\">%s\n", \
        substr($0, 1, RSTART - 1), surface0, substr($0, RSTART + RLENGTH)
      hit = 1
      next
    }
    { print }
    END {
      if (!hit) {
        print "render-palette: no theme-color meta tag in index.hbs" > "/dev/stderr"
        exit 1
      }
    }
  ' "$INDEX_HBS"
}

# ------------------------------------------------------------------ the icon

write_icon_svg() {
  awk \
    -v surface0="$DARK_SURFACE0" \
    -v surface2="$DARK_SURFACE2" \
    -v text_primary="$DARK_TEXT_PRIMARY" \
    -v meter_amber="$DARK_METER_AMBER" '
    function hexval(s,   i, c, v, d) {
      v = 0
      for (i = 1; i <= length(s); i++) {
        c = tolower(substr(s, i, 1))
        d = index("0123456789abcdef", c) - 1
        if (d < 0) { return -1 }
        v = v * 16 + d
      }
      return v
    }
    # Recognizes a shape role from the color already on it, so the design
    # (which segments are lit) stays in the SVG. See the script header.
    function classify_fill(hex, first,   r, g, b, lo, hi) {
      if (first) { return surface0 }
      r = hexval(substr(hex, 1, 2))
      g = hexval(substr(hex, 3, 2))
      b = hexval(substr(hex, 5, 2))
      lo = r
      if (g < lo) { lo = g }
      if (b < lo) { lo = b }
      hi = r
      if (g > hi) { hi = g }
      if (b > hi) { hi = b }
      if (r >= 208 && g >= 128 && g <= 192 && b <= 64) { return meter_amber }
      if (lo >= 128) { return text_primary }
      if (hi <= 96) { return surface2 }
      return ""
    }
    /<rect/ {
      rects++
      if (!match($0, /fill="#[0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]"/)) {
        printf "render-palette: rect %d has no six-digit fill\n", rects > "/dev/stderr"
        exit 1
      }
      cur = substr($0, RSTART + 7, 6)
      recolored = classify_fill(cur, rects == 1)
      if (recolored == "") {
        printf "render-palette: rect %d has fill #%s, which is not a plate, a lit, an unlit, or a peak color; teach classify_fill about it\n", rects, cur > "/dev/stderr"
        exit 1
      }
      printf "%sfill=\"#%s\"%s\n", substr($0, 1, RSTART - 1), recolored, substr($0, RSTART + RLENGTH)
      next
    }
    { print }
    END {
      if (rects == 0) {
        print "render-palette: no <rect> fills found in the icon" > "/dev/stderr"
        exit 1
      }
    }
  ' "$ICON_SVG"
}

# -------------------------------------------------------------------- drive

# The branding assets land in their own change; a tree without the icon
# still gets its CSS regenerated instead of failing.
HAVE_ICON=yes
if [ ! -f "$ICON_SVG" ]; then
  HAVE_ICON=no
  echo "render-palette: no $ICON_SVG; skipping the icon" >&2
fi

write_palette_css > "$TMP/palette.css"
write_index_hbs > "$TMP/index.hbs"
if [ "$HAVE_ICON" = yes ]; then
  write_icon_svg > "$TMP/jamstream.svg"
fi

if [ "$MODE" = check ]; then
  STATUS=0
  drifted() {
    echo "render-palette: $1 is stale: it does not match what" \
      "scripts/render-palette.sh generates from the palettes in" \
      "crates/client/src/theme.rs. Run scripts/render-palette.sh (then" \
      "scripts/render-icon.sh if the icon changed) and commit the result." >&2
    STATUS=1
  }
  cmp -s "$TMP/palette.css" "$PALETTE_CSS" || drifted site/theme/css/palette.css
  cmp -s "$TMP/index.hbs" "$INDEX_HBS" || drifted site/theme/index.hbs
  if [ "$HAVE_ICON" = yes ]; then
    cmp -s "$TMP/jamstream.svg" "$ICON_SVG" ||
      drifted crates/client/assets/icon/jamstream.svg
  fi
  if [ "$STATUS" -eq 0 ]; then
    echo "palette: every generated consumer matches crates/client/src/theme.rs"
  fi
  exit "$STATUS"
fi

# install <generated> <destination> <label>
install_generated() {
  if cmp -s "$1" "$2"; then
    echo "unchanged $3"
  else
    cp "$1" "$2"
    echo "wrote $3"
  fi
}

install_generated "$TMP/palette.css" "$PALETTE_CSS" site/theme/css/palette.css
install_generated "$TMP/index.hbs" "$INDEX_HBS" site/theme/index.hbs
if [ "$HAVE_ICON" = yes ]; then
  install_generated "$TMP/jamstream.svg" "$ICON_SVG" \
    crates/client/assets/icon/jamstream.svg
  echo "note: rerun scripts/render-icon.sh to rerender the derived icon assets"
fi
