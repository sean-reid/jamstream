#!/bin/sh
# The two pinned media artifacts must still be downloadable, and the ffmpeg
# pin must name a tag upstream keeps.
#
# BtbN publishes an autobuild every day and deletes the daily ones after about
# a fortnight, keeping the last build of each month for years. A mid-month tag
# therefore stops resolving, and because a session VM fetches this URL at boot,
# the first thing anyone notices is a session with no encoder. A pin that has
# rotted is a broken product, not a broken build, so this runs nightly.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PINS="$ROOT/crates/cloud/data/media_artifacts.json"
status=0

# The last day of the month the tag names, counted out here rather than asked
# of anything: runner images carry no cal, and the date that takes a relative
# expression is GNU's alone, so neither survives as a dependency of a check
# whose whole job is to still work in a year.
last_day_of() {
  case $2 in
  1 | 3 | 5 | 7 | 8 | 10 | 12) echo 31 ;;
  4 | 6 | 9 | 11) echo 30 ;;
  2)
    if [ $(($1 % 4)) -eq 0 ] && { [ $(($1 % 100)) -ne 0 ] || [ $(($1 % 400)) -eq 0 ]; }; then
      echo 29
    else
      echo 28
    fi
    ;;
  *)
    echo "the ffmpeg pin names month $2, which is not a month" >&2
    exit 1
    ;;
  esac
}

TAG=$(sed -n 's/.*autobuild-\([0-9][0-9-]*\).*/\1/p' "$PINS" | head -1)
if [ -z "$TAG" ]; then
  echo "no autobuild tag found in $PINS; has the ffmpeg source line changed shape?" >&2
  exit 1
fi
YEAR=$(printf '%s' "$TAG" | cut -d- -f1)
# Leading zeros come off before any arithmetic: 08 and 09 are not octal, and a
# shell that reads them as octal fails the check on two months of every year.
MONTH=$(printf '%s' "$TAG" | cut -d- -f2)
MONTH=${MONTH#0}
DAY=$(printf '%s' "$TAG" | cut -d- -f3)
DAY=${DAY#0}
LAST=$(last_day_of "$YEAR" "$MONTH")
if [ "$DAY" -ne "$LAST" ]; then
  echo "the ffmpeg pin names autobuild-$TAG, day $DAY of a month ending on $LAST." >&2
  echo "Only the last autobuild of a month survives upstream; pick that one." >&2
  status=1
else
  echo "ok    ffmpeg pins autobuild-$TAG, the last build of that month"
fi

for url in $(jq -r '.ffmpeg.targets[].url, .mediamtx.targets[].url' "$PINS"); do
  code=$(curl -sSL -o /dev/null -w '%{http_code}' --max-time 60 "$url" || echo 000)
  if [ "$code" = 200 ]; then
    echo "ok    $code ${url##*/}"
  else
    echo "$code for $url" >&2
    echo "A VM booting a broadcast session downloads this; it has to resolve." >&2
    status=1
  fi
done

exit $status
