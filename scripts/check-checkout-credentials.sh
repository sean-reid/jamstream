#!/bin/sh
# Every actions/checkout must say out loud what happens to the job token.
#
# checkout writes the token into .git/config as an http.extraheader unless it
# is told not to, and anything that runs later in the job can read it back out
# of the workspace, build scripts in the dependency graph included. Almost
# every checkout in this repository never touches the network again after it
# runs, so `persist-credentials: false` costs nothing; the one that pushes
# says `true` and explains itself.
#
# release.yml got this treatment first and nothing stopped the other six
# workflows from keeping the default, which is how #397 happened. A comment
# cannot notice the next workflow; this can.
#
# Run from anywhere. Exits nonzero naming every checkout that decides nothing.
set -eu

cd "$(dirname "$0")/.."
DIR=.github/workflows

# A parse that reads nothing would pass everything, so the count is the check
# on the check. The repo has had more than twenty checkouts since the release
# pipeline grew past one job.
total=$(grep -c 'uses:[[:space:]]*actions/checkout@' "$DIR"/*.yml | awk -F: '{ n += $2 } END { print n + 0 }')
if [ "$total" -lt 20 ]; then
  echo "only $total checkouts found under $DIR; the parse or the path moved" >&2
  exit 1
fi

# A checkout's own `uses:` line opens the window; the next list item, the next
# file, or the end of input closes it. That holds for both shapes this repo
# writes: `- uses: actions/checkout@v7` and an `- if:` step whose uses sits on
# the following line.
BAD=$(awk '
  function close_window() {
    if (checking && !ok) {
      print file ":" line ": actions/checkout sets no persist-credentials"
    }
    checking = 0
  }
  FNR == 1 { close_window() }
  /uses:[[:space:]]*actions\/checkout@/ {
    close_window()
    checking = 1; ok = 0; line = FNR; file = FILENAME
    next
  }
  checking && /^[[:space:]]*-[[:space:]]/ { close_window() }
  checking && /persist-credentials:[[:space:]]*(true|false)/ { ok = 1 }
  END { close_window() }
' "$DIR"/*.yml)

if [ -n "$BAD" ]; then
  echo "$BAD" >&2
  echo "Set persist-credentials: false, or true with a comment saying what needs the token." >&2
  exit 1
fi

echo "all $total checkouts declare what happens to the token"
