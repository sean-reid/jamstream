#!/bin/sh
# The download page must agree with packaging/README.md about which package
# channels are live, and the two strings a user actually types must match
# their single sources: the brew tap path (defined in packaging/README.md,
# echoed by the rendered manifests) and the scoop bucket URL (release.yml's
# BUCKET_REPO, echoed by the README's scoop section).
#
# This exists because site/src/download.md said no package manager was live
# while the Homebrew tap had been shipping auto-updated formulas for weeks,
# and it stayed wrong through the Scoop launch (#362). A channel is live
# exactly when its section in packaging/README.md opens with "Live:".
#
# Run from anywhere. Exits nonzero naming every file that has to change.
set -eu

cd "$(dirname "$0")/.."

README=packaging/README.md
PAGE=site/src/download.md
RELEASE=.github/workflows/release.yml

status=0
complain() {
  echo "$1" >&2
  status=1
}

# First non-blank line of a channel's "## <name>" section in the README.
section_opening() {
  awk -v h="## $1" '
    $0 == h { insec = 1; next }
    insec && /^## / { exit }
    insec && NF { print; exit }
  ' "$README"
}

is_live() {
  case "$(section_opening "$1")" in
    Live:*) return 0 ;;
  esac
  return 1
}

# The "## Package managers" section of the download page, heading to next
# heading. A check that reads nothing would pass everything, so an empty
# section is a failure, not a pass.
PM=$(awk '
  /^## Package managers/ { insec = 1; next }
  insec && /^## / { exit }
  insec { print }
' "$PAGE")
if [ -z "$PM" ]; then
  echo "$PAGE has no '## Package managers' section; this check reads it, so teach the script where it moved" >&2
  exit 1
fi

# An install command in the package-managers section, not a prose mention:
# "winget and the AUR are planned" must not count as documenting either.
documented() {
  case "$1" in
    Homebrew) printf '%s\n' "$PM" | grep -q 'brew install' ;;
    Scoop) printf '%s\n' "$PM" | grep -qE 'scoop (install|bucket add)' ;;
    winget) printf '%s\n' "$PM" | grep -q 'winget install' ;;
    AUR) printf '%s\n' "$PM" | grep -qE 'aur\.archlinux\.org|makepkg|yay -S|paru -S' ;;
  esac
}

for ch in Homebrew winget AUR Scoop; do
  if [ -z "$(section_opening "$ch")" ]; then
    complain "$README has no '## $ch' section any more; teach scripts/check-channel-claims.sh the new shape"
    continue
  fi
  if is_live "$ch"; then
    documented "$ch" ||
      complain "$README marks $ch live but $PAGE's package-managers section has no $ch install command; add it to $PAGE"
  else
    if documented "$ch"; then
      complain "$PAGE documents a $ch install but $README does not mark $ch live; fix whichever of the two is wrong"
    fi
  fi
done

# The tap path. Defined once in the README ('brew tap <owner/repo>'),
# echoed by the manifests' install comments, typed by users from the page.
TAP=$(sed -n 's/.*brew tap \([A-Za-z0-9._-]*\/[A-Za-z0-9._-]*\).*/\1/p' "$README" | sort -u)
if [ "$(printf '%s\n' "$TAP" | grep -c .)" -ne 1 ]; then
  complain "$README no longer names the tap in exactly one 'brew tap <owner/repo>' (found: ${TAP:-nothing}); this check pins $PAGE to it"
else
  MANIFEST_TAP=$(sed -n 's|^# Install: brew install \(--cask \)\{0,1\}\([A-Za-z0-9._-]*/[A-Za-z0-9._-]*\)/.*|\2|p' \
    packaging/homebrew/Casks/*.rb packaging/homebrew/Formula/*.rb | sort -u)
  [ "$MANIFEST_TAP" = "$TAP" ] ||
    complain "the rendered manifests' install comments use '${MANIFEST_TAP:-no tap}' but $README taps '$TAP'; scripts/render-packaging.sh and the README have drifted"
  if is_live Homebrew; then
    PAGE_TAP=$(printf '%s\n' "$PM" | sed -n 's|.*brew install \(--cask \)\{0,1\}\([A-Za-z0-9._-]*/[A-Za-z0-9._-]*\)/.*|\2|p' | sort -u)
    [ "$PAGE_TAP" = "$TAP" ] ||
      complain "$PAGE's brew commands install from '${PAGE_TAP:-no tap at all}' but the tap is '$TAP'; fix $PAGE"
  fi
fi

# The bucket URL. release.yml's BUCKET_REPO is what the release actually
# pushes to, so it is the source; the README and the page echo it.
BUCKET_REPO=$(sed -n 's/^[[:space:]]*BUCKET_REPO:[[:space:]]*\([^[:space:]]*\).*/\1/p' "$RELEASE" | sort -u)
if [ "$(printf '%s\n' "$BUCKET_REPO" | grep -c .)" -ne 1 ]; then
  complain "$RELEASE no longer sets BUCKET_REPO exactly once (found: ${BUCKET_REPO:-nothing}); this check pins $PAGE to it"
else
  BUCKET_URL="https://github.com/$BUCKET_REPO"
  grep -qF "$BUCKET_URL" "$README" ||
    complain "$README's scoop section does not name $BUCKET_URL, the bucket $RELEASE pushes to; the two have drifted"
  if is_live Scoop; then
    PAGE_BUCKET=$(printf '%s\n' "$PM" | sed -n 's/.*scoop bucket add [^[:space:]]\{1,\} \([^[:space:]`]\{1,\}\).*/\1/p' | sort -u)
    [ "$PAGE_BUCKET" = "$BUCKET_URL" ] ||
      complain "$PAGE adds the bucket from '${PAGE_BUCKET:-no URL}' but $RELEASE pushes to $BUCKET_URL; fix $PAGE"
  fi
fi

if [ "$status" -ne 0 ]; then
  exit "$status"
fi
echo "the download page agrees with $README: channel claims, tap $TAP, bucket https://github.com/$BUCKET_REPO"
