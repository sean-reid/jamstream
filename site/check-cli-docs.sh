#!/bin/sh
# Guards the CLI reference pages against drift, in both directions:
# every long flag in `jamstream <cmd> --help` must appear in
# site/src/cli/<cmd>.md, and every long flag mentioned in that page must
# still exist in the help output. Run from anywhere; builds the CLI unless
# JAMSTREAM_BIN points at one. Exits nonzero listing all drift found.
set -eu
cd "$(dirname "$0")"

if [ -n "${JAMSTREAM_BIN:-}" ]; then
  BIN="$JAMSTREAM_BIN"
else
  (cd .. && cargo build -q -p jamstream-cli)
  BIN=../target/debug/jamstream
fi

"$BIN" help >/dev/null

status=0
for cmd in host status end sweep join recordings completions; do
  page="src/cli/$cmd.md"
  [ -f "$page" ] || { echo "missing page: $page" >&2; status=1; continue; }
  help_out="$("$BIN" "$cmd" --help)"
  # A command with subcommands documents them on the same page, so its flag
  # set is the union of its own and theirs.
  case "$cmd" in
    recordings)
      help_out="$help_out
$("$BIN" recordings get --help)"
      ;;
  esac
  help_flags="$(printf '%s' "$help_out" | grep -oE -- '--[a-z][a-z0-9-]*' | grep -vx -- --help | sort -u)"
  page_flags="$(grep -oE -- '--[a-z][a-z0-9-]*' "$page" | grep -vx -- --help | sort -u || true)"
  for f in $help_flags; do
    printf '%s\n' "$page_flags" | grep -qx -- "$f" \
      || { echo "drift: $cmd --help has $f but $page does not mention it" >&2; status=1; }
  done
  for f in $page_flags; do
    printf '%s\n' "$help_flags" | grep -qx -- "$f" \
      || { echo "drift: $page mentions $f but $cmd --help does not have it" >&2; status=1; }
  done
done
[ $status -eq 0 ] && echo "cli docs match --help output"
exit $status
