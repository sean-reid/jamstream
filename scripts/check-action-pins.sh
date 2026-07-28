#!/bin/sh
# Every action outside the github-owned actions/* namespace must be pinned to a
# 40-hex commit, not to a tag or a branch.
#
# This exists because ci.yml carried a comment asserting exactly that while
# EmbarkStudios/cargo-deny-action and taiki-e/install-action sat on @v2, one of
# them inside the required lint job. The comment is what made it hard to see. A
# comment cannot notice drift; this can.
#
# actions/* are exempt deliberately: they are published by the same account
# that runs the runner, so pinning them buys nothing that owning the runner
# does not already give away. dtolnay/rust-toolchain is pinned to a commit on
# master like everything else and needs no exemption.
#
# Run from anywhere. Exits nonzero listing every unpinned action.
set -eu

cd "$(dirname "$0")/.."
DIR=.github/workflows

# "<file> <owner/repo@ref>" per `uses:`, trailing comments dropped.
uses_lines() {
  for file in "$DIR"/*.yml; do
    [ -f "$file" ] || continue
    sed -n "s|^[[:space:]]*-\{0,1\}[[:space:]]*uses:[[:space:]]*\([^[:space:]#]*\).*|$file \1|p" "$file"
  done
}

# A parse that reads nothing would pass everything, so the count is the check
# on the check. The repo has had more than thirty `uses:` lines since CI grew
# past one workflow.
total=$(uses_lines | grep -c . || true)
if [ "$total" -lt 10 ]; then
  echo "only $total 'uses:' lines found under $DIR; the parse or the path moved" >&2
  exit 1
fi

# A file rather than a command substitution around the loop: bash 3.2, which
# is what /bin/sh is on macOS, misparses a `case` inside `$( )`.
BAD=$(mktemp)
trap 'rm -f "$BAD"' EXIT INT TERM

uses_lines | while read -r file use; do
  case "$use" in
    # First-party, or a local/docker action with no ref to pin.
    actions/* | ./* | docker://*) continue ;;
    *@*) ;;
    *)
      echo "$file: 'uses: $use' has no @ref, so nothing is pinned"
      continue
      ;;
  esac
  ref=${use#*@}
  if printf '%s' "$ref" | grep -qE '^[0-9a-f]{40}$'; then
    continue
  fi
  echo "$file: $use is on a mutable ref"
done > "$BAD"

if [ -s "$BAD" ]; then
  cat "$BAD" >&2
  echo "Pin each to a 40-hex commit, version in a trailing comment. See the policy comment in ci.yml." >&2
  exit 1
fi

echo "all $total actions are pinned or first-party actions/*"
