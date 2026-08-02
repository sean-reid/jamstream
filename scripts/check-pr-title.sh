#!/bin/sh
# The pull request title is the release note.
#
# We squash-merge, so the title becomes the commit message on main and
# release-please builds the changelog and the version bump out of it. A title
# that is not a conventional commit is not an error anywhere: the pull request
# merges, CI is green, and the change is silently absent from the next release.
# That is what happened before v0.1.2, where nine pull requests including the
# session firewall and the broadcast pipeline were dropped from the changelog
# and had to be restored by hand.
#
# The accepted types are read out of release-please-config.json rather than
# written here. A type this gate accepts and release-please has no section for
# renders nothing, and a range whose every commit is such a type is not
# released at all: release-please skips a pull request whose notes are empty.
# That is how `security` came to be refused here while two commits already in
# the history use it. Deriving the list makes the two impossible to disagree.
#
# Usage: scripts/check-pr-title.sh "<title>"
set -eu

cd "$(dirname "$0")/.."
CONFIG=release-please-config.json

# "<type> shown|hidden" per entry of the changelog-sections array. Scoped to
# that array because the extra-files block later in the file has "type" keys
# of its own.
sections() {
  awk '
    /"changelog-sections"[[:space:]]*:[[:space:]]*\[/ { inside = 1; next }
    inside && /^[[:space:]]*\]/ { inside = 0 }
    inside && /"type"/ {
      type = $0
      sub(/.*"type"[^"]*"/, "", type)
      sub(/".*/, "", type)
      print type, ($0 ~ /"hidden"[[:space:]]*:[[:space:]]*true/) ? "hidden" : "shown"
    }
  ' "$CONFIG"
}

LIST=$(sections)
# A parse that read nothing would reject every title, including the ones that
# fix it, so the count is the check on the check.
count=$(printf '%s\n' "$LIST" | grep -c . || true)
if [ "$count" -lt 10 ]; then
  echo "read $count changelog sections from $CONFIG; the parse or the file moved" >&2
  exit 1
fi

TYPES=$(printf '%s\n' "$LIST" | awk '{ print $1 }' | tr '\n' '|' | sed 's/|$//')
SHOWN=$(printf '%s\n' "$LIST" | awk '$2 == "shown" { print $1 }' | tr '\n' ' ' | sed 's/ $//')
HIDDEN=$(printf '%s\n' "$LIST" | awk '$2 == "hidden" { print $1 }' | tr '\n' ' ' | sed 's/ $//')

TITLE=${1?usage: check-pr-title.sh "<title>"}

if printf '%s' "$TITLE" | grep -qE "^($TYPES)(\([a-z0-9,./ -]+\))?!?: .+"; then
  echo "ok: $TITLE"
  exit 0
fi

cat >&2 <<MSG
PR title is not a conventional commit:

  $TITLE

Because this repo squash-merges, the title becomes the commit message on
main, and release-please builds the changelog from it. A non-conforming
title merges fine and then vanishes from the release notes, which is why
this is enforced rather than asked for.

Use: <type>[optional scope][!]: <description>

  in the release notes:  $SHOWN
  not in the notes:      $HIDDEN

Every type bumps the patch version and ! bumps the minor while this is
0.x, but a range of second-row types with no ! renders no notes, and a
release with no notes is skipped.

Example: security(protocol)!: bind the handshake cookie to the address
         fix(cloud): make the VM bootstrap fail closed
MSG
exit 1
