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

# The last day of the month the tag names, without a date library: ask cal for
# the month and take its final number.
last_day_of() {
  cal "$2" "$1" | awk 'NF { last = $NF } END { print last }'
}

TAG=$(sed -n 's/.*autobuild-\([0-9][0-9-]*\).*/\1/p' "$PINS" | head -1)
if [ -z "$TAG" ]; then
  echo "no autobuild tag found in $PINS; has the ffmpeg source line changed shape?" >&2
  exit 1
fi
YEAR=$(printf '%s' "$TAG" | cut -d- -f1)
MONTH=$(printf '%s' "$TAG" | cut -d- -f2)
DAY=$(printf '%s' "$TAG" | cut -d- -f3)
LAST=$(last_day_of "$YEAR" "$MONTH")
if [ "$((DAY))" -ne "$((LAST))" ]; then
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
