#!/bin/sh
# Two shapes of test that measure the machine instead of the thing. Both pass
# on a quiet laptop, so review does not catch them, and both fail on a loaded
# runner as something that reads like a product bug.
#
# One: a count asserted against a wall-clock sleep, which is #458 and #459.
# The test sleeps and then asks how many things arrived, so the answer belongs
# to the runner's scheduler. A loaded macOS runner stretched a 20 ms window to
# 145 ms and delivered 58 callbacks where 8 were meant. Assert what happened
# instead: drive a fixed number of events, or wait for the condition against a
# scaled budget and stop as soon as it holds.
#
# Two: audio measured at the tail of a capture file, which is #474. A capture
# spans from before the join to after the leave, and both ends hold silence the
# backend wrote while nothing played: 1.75 s at the front and 0.75 s at the
# back on a loaded Windows runner, against a one second window. loudest_rms and
# tone_profile find the audio wherever it landed.
#
# Run with no arguments to scan the workspace. Run with paths to scan those
# files instead, which is how ci.yml proves the gate bites. Exits nonzero
# naming every test that has to change.
set -eu

cd "$(dirname "$0")/.."

# Every occurrence outside its own definition measures a position in a file
# whose length depends on how fast the machine ran. Two are deliberate: both
# read a span meant to be silent, where padding is silence too.
TAIL_FILE=crates/client/tests/live_runtime.rs
TAIL_BUDGET=2

SLEEP_SCAN=$(mktemp)
TAIL_SCAN=$(mktemp)
OUT=$(mktemp)
trap 'rm -f "$SLEEP_SCAN" "$TAIL_SCAN" "$OUT"' EXIT INT HUP TERM

# Fires on a test function that asserts an event tally against a positive
# literal in the straight-line run after a sleep. Narrow on purpose, because a
# gate that fires on innocent code is disabled by the next person: how big
# something is has nothing to do with the clock, an expected zero is a
# guarantee rather than a rate, and an #[ignore] test never runs on a runner,
# so none of the three is scanned.
cat > "$SLEEP_SCAN" <<'AWK'
function ident() { return "[A-Za-z_][A-Za-z_0-9]*" }

# A whole operand that can only be a tally of events: a dotted path ending in
# a call that takes no arguments. An intermediate parenthesis would make it an
# iterator chain, which is how a test says something about content.
function counter() { return ident() "(\\." ident() ")*\\." ident() "\\(\\)" }

function positive() { return "[1-9][0-9_]*" }

function examine(file, name, at, body,   n, st, i, j, locals, m, head) {
  scanned++
  # A length is how big something is: a hex digest is 64 characters and a
  # frame is 4 bytes a pixel however slowly the machine ran.
  gsub(/\.(len|is_empty)\(\)/, ".size", body)
  n = split(body, st, ";")
  for (i = 1; i <= n; i++) {
    if (st[i] !~ "(^|[^A-Za-z_0-9])sleep[[:space:]]*\\(") {
      continue
    }
    locals = "@@none@@"
    for (j = i + 1; j <= n; j++) {
      # A brace between the sleep and the assertion means a block closed or
      # opened, and a poll loop that stops on its own condition is the fix
      # rather than the fault.
      if (st[j] ~ /[{}]/) {
        break
      }
      head = "assert_eq![[:space:]]*\\([[:space:]]*"
      if (st[j] ~ head "(" counter() "|" locals ")[[:space:]]*,[[:space:]]*" positive() "[[:space:]]*[,)]" ||
        st[j] ~ head positive() "[[:space:]]*,[[:space:]]*(" counter() "|" locals ")[[:space:]]*[,)]") {
        print file ":" at ": " name " asserts a count against a sleep"
        return
      }
      # A counter read into a local is the same assertion over two lines.
      if (match(st[j], "let[[:space:]]+" ident() "[[:space:]]*=[[:space:]]*" counter() "[[:space:]]*$")) {
        m = substr(st[j], RSTART, RLENGTH)
        sub(/^let[[:space:]]+/, "", m)
        sub(/[[:space:]]*=.*$/, "", m)
        locals = locals "|" m
      }
    }
  }
}

function finish() {
  if (open) {
    if (is_test && !is_ignored) {
      examine(FILENAME, fn_name, fn_at, body)
    }
    open = 0
  }
  is_test = 0
  is_ignored = 0
}

FNR == 1 { finish() }

{
  # A string literal carries braces, semicolons and prose that everything
  # below would otherwise read as code.
  line = $0
  gsub(/"[^"]*"/, "\"\"", line)
}

open {
  if (line ~ closer) {
    finish()
  } else {
    body = body " " line
  }
  next
}

line ~ /^[[:space:]]*#\[[A-Za-z_:]*test(\]|\()/ { is_test = 1 }
line ~ /^[[:space:]]*#\[ignore/ { is_ignored = 1 }
line ~ /^[[:space:]]*(pub[^ ]* )?mod [A-Za-z_]/ { finish(); next }

# rustfmt puts every item at its block's indentation and closes it with a
# brace at the same column, so the body runs to the first line matching that.
line ~ /^[[:space:]]*(pub[^ ]* )?(async )?(unsafe )?fn [A-Za-z_]/ {
  match(line, /^[[:space:]]*/)
  closer = "^" substr(line, 1, RLENGTH) "}"
  match(line, /fn [A-Za-z_0-9]+/)
  fn_name = substr(line, RSTART + 3, RLENGTH - 3)
  fn_at = FNR
  body = ""
  open = 1
  next
}

# Anything that is not an attribute or a comment ends the attribute block a
# test attribute would have opened.
line ~ /[^[:space:]]/ && line !~ /^[[:space:]]*(#|\/\/)/ { is_test = 0; is_ignored = 0 }

END {
  finish()
  print "scanned " scanned
}
AWK

# A parse that read nothing would pass everything, so the count is the check
# on the check. The workspace has been past a thousand test functions since the
# client grew its own suites, and a handful of fixtures has whatever it has.
FLOOR=1200
if [ "$#" -gt 0 ]; then
  FLOOR=0
  # Files handed in are fixtures, and a fixture gets no allowance.
  TAIL_BUDGET=0
fi

if [ "$#" -gt 0 ]; then
  printf '%s\n' "$@"
else
  find crates xtask -name '*.rs' | sort
fi | xargs awk -f "$SLEEP_SCAN" > "$OUT"

SCANNED=$(sed -n 's/^scanned //p' "$OUT")
SLEEPERS=$(grep -v '^scanned ' "$OUT" || true)

if [ "$SCANNED" -lt "$FLOOR" ]; then
  echo "only $SCANNED test functions found; the parse or the tree moved" >&2
  exit 1
fi

status=0
if [ -n "$SLEEPERS" ]; then
  echo "$SLEEPERS" >&2
  echo "A sleep is not a measurement window. Assert what happened: drive a fixed number of events, or wait for the condition against a scaled budget." >&2
  status=1
fi

cat > "$TAIL_SCAN" <<'AWK'
# The definitions themselves, and whatever they call inside their own bodies,
# are the measurement; every other mention is a test taking one.
/^fn (tail|tail_rms|tail_pitch_hz)\(/ { helper = 1; next }
helper { if (/^}/) helper = 0; next }
{
  rest = $0
  while (match(rest, /(^|[^A-Za-z_0-9])tail(_rms|_pitch_hz)?\(/)) {
    print FILENAME ":" FNR ": a tail measurement reads a window that a slow join or leave fills with padding"
    rest = substr(rest, RSTART + RLENGTH)
  }
}
AWK

if [ "$#" -gt 0 ]; then
  SCOPE="the files given"
  WHERE="the files given"
  TAILS=$(awk -f "$TAIL_SCAN" "$@")
else
  SCOPE="the workspace"
  WHERE=$TAIL_FILE
  TAILS=$(awk -f "$TAIL_SCAN" "$TAIL_FILE")
fi
FOUND=$(printf '%s' "$TAILS" | grep -c . || true)

if [ "$FOUND" -gt "$TAIL_BUDGET" ]; then
  echo "$TAILS" >&2
  echo "the tail budget for $WHERE is $TAIL_BUDGET and the scan found $FOUND." >&2
  echo "Measure where the audio is: loudest_rms scans for the loudest window and tone_profile reports across blocks." >&2
  exit 1
fi

if [ "$status" -eq 0 ]; then
  echo "$SCANNED test functions under $SCOPE assert no count against a sleep"
fi

if [ "$#" -gt 0 ]; then
  exit "$status"
fi

if [ "$FOUND" -lt "$TAIL_BUDGET" ]; then
  echo "$TAIL_FILE is under its tail budget, $FOUND against $TAIL_BUDGET; lower TAIL_BUDGET in scripts/check-test-timing.sh to hold the ground"
else
  echo "$TAIL_FILE is at its tail budget of $TAIL_BUDGET"
fi
exit "$status"
