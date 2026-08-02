#!/bin/sh
# Guards the CLI reference pages against drift, in both directions.
#
# It used to hardcode the seven command names and throw away the output of
# `jamstream help`, so an eighth subcommand needed no page and drew no
# complaint, and it greped long flags only, so positionals and the value
# domains behind --provider, --retention and `completions <shell>` could drift
# freely. Everything it looks at now comes out of --help.
#
# What it checks:
#   commands      every subcommand `jamstream --help` lists has a page under
#                 src/cli/ and a line in src/SUMMARY.md, and every page there
#                 is a subcommand. Nested subcommands are documented on their
#                 parent's page, so they fold into it rather than needing one
#                 of their own.
#   long flags    every --flag in a command's help (its own and its
#                 subcommands') appears on the page, and every --flag on the
#                 page still exists in that help.
#   positionals   every name in an `Arguments:` block appears on the page.
#   value domains every `[possible values: ...]` entry, and every value in a
#                 "one of a, b, or c" list in a help description, appears on
#                 the page. This is the half that stopped --provider names and
#                 --retention values drifting.
#   the guides    every --flag mentioned in any other page under src/ exists
#                 somewhere in the CLI. A guide need not mention every flag,
#                 but it must not invent one. Flags belonging to other tools
#                 are listed in FOREIGN_FLAGS below, and an exemption that the
#                 CLI has since grown is itself an error.
#
# Several counts are asserted along the way. They are the check on the check:
# every extraction here is a grep over help text, and a reworded help string
# that silently matches nothing is the one way this could go quiet.
#
# Run from anywhere; builds the CLI unless JAMSTREAM_BIN points at one. Exits
# nonzero listing all the drift it found, not just the first.
set -eu
cd "$(dirname "$0")"

if [ -n "${JAMSTREAM_BIN:-}" ]; then
  BIN="$JAMSTREAM_BIN"
else
  (cd .. && cargo build -q -p jamstream-cli)
  BIN=../target/debug/jamstream
fi

# Long flags the docs mention that belong to another tool, so the CLI cannot be
# asked about them: brew's --cask, shasum's --check and --ignore-missing,
# cargo's --path, and the install and uninstall scripts' own --purge, --tag and
# --with-server.
FOREIGN_FLAGS='--cask --check --ignore-missing --path --purge --tag --with-server'

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

status=0
fail() {
  echo "drift: $*" >&2
  status=1
}

# Subcommands of a command path, `help` dropped. No arguments means the root.
subcommands() {
  "$BIN" "$@" --help | awk '
    /^Commands:/ { inblock = 1; next }
    inblock && /^[^ ]/ { inblock = 0 }
    inblock && /^  [a-z]/ { print $1 }
  ' | grep -vx help || true
}

# A command's help plus every nested subcommand's, since one page documents the
# whole subtree it heads.
help_tree() {
  "$BIN" "$@" --help
  for sub in $(subcommands "$@"); do
    help_tree "$@" "$sub"
  done
}

flags_in() {
  grep -oE -- '--[a-z][a-z0-9-]*' "$1" | grep -vx -- --help | sort -u || true
}

# ------------------------------------------------------------ the command set

COMMANDS=$(subcommands | tr '\n' ' ')
COUNT=$(printf '%s' "$COMMANDS" | wc -w | tr -d ' ')
if [ "$COUNT" -lt 7 ]; then
  echo "only $COUNT subcommands parsed out of 'jamstream --help'; the help layout changed and this script has gone blind" >&2
  exit 1
fi
echo "commands: $COMMANDS"

for cmd in $COMMANDS; do
  [ -f "src/cli/$cmd.md" ] ||
    fail "jamstream has a '$cmd' subcommand and there is no src/cli/$cmd.md"
  grep -qF "cli/$cmd.md" src/SUMMARY.md ||
    fail "src/cli/$cmd.md is not listed in src/SUMMARY.md, so the book does not link to it"
done

for page in src/cli/*.md; do
  name=$(basename "$page" .md)
  if [ "$name" = index ]; then
    continue
  fi
  case " $COMMANDS " in
    *" $name "*) ;;
    *) fail "$page documents a '$name' subcommand the CLI does not have" ;;
  esac
done

# ----------------------------------------------------- per command, per page

TOTAL_FLAGS=0
TOTAL_POSITIONALS=0
TOTAL_DOMAINS=0

for cmd in $COMMANDS; do
  page="src/cli/$cmd.md"
  if [ ! -f "$page" ]; then
    continue
  fi
  help_tree "$cmd" > "$TMP/help"

  flags_in "$TMP/help" > "$TMP/help-flags"
  flags_in "$page" > "$TMP/page-flags"
  # `|| true` because grep -c exits 1 on a zero count, and a simple assignment
  # takes its command substitution's status, which set -e would act on.
  # `completions` has no flags of its own at all.
  found=$(grep -c . < "$TMP/help-flags" || true)
  TOTAL_FLAGS=$((TOTAL_FLAGS + found))
  while read -r flag; do
    grep -qx -- "$flag" "$TMP/page-flags" ||
      fail "$cmd --help has $flag and $page does not mention it"
  done < "$TMP/help-flags"
  while read -r flag; do
    grep -qx -- "$flag" "$TMP/help-flags" ||
      fail "$page mentions $flag and $cmd --help does not have it"
  done < "$TMP/page-flags"

  # Positionals, from every `Arguments:` block in the subtree. Brackets are
  # dropped: a page may write SESSION or [SESSION] and both say the same
  # thing to a reader.
  awk '
    /^Arguments:/ { inblock = 1; next }
    inblock && /^[^ ]/ { inblock = 0 }
    inblock && /^  [<[]/ { gsub(/[<>[\]]/, "", $1); print $1 }
  ' "$TMP/help" | sort -u > "$TMP/positionals"
  while read -r arg; do
    TOTAL_POSITIONALS=$((TOTAL_POSITIONALS + 1))
    grep -qF -- "$arg" "$page" ||
      fail "$cmd takes a positional $arg and $page never names it"
  done < "$TMP/positionals"

  # Value domains, in the two shapes help states them: clap's own
  # `[possible values: ...]`, and a description ending in a list of literals.
  {
    grep -oE '\[possible values: [^]]*\]' "$TMP/help" |
      sed 's/\[possible values: //; s/\]//; s/, /\
/g'
    grep -oE ': [a-z0-9]+(, [a-z0-9]+)+,? or [a-z0-9]+' "$TMP/help" |
      sed 's/^: //; s/,\{0,1\} or /\
/; s/, /\
/g'
  } | sed '/^$/d' | sort -u > "$TMP/domains"
  while read -r value; do
    TOTAL_DOMAINS=$((TOTAL_DOMAINS + 1))
    grep -qF -- "$value" "$page" ||
      fail "$cmd accepts the value '$value' and $page never names it"
  done < "$TMP/domains"
done

if [ "$TOTAL_FLAGS" -lt 30 ]; then
  echo "only $TOTAL_FLAGS flags parsed out of the help output; the parse is broken" >&2
  exit 1
fi
if [ "$TOTAL_POSITIONALS" -lt 4 ]; then
  echo "only $TOTAL_POSITIONALS positionals parsed; the Arguments: block layout changed" >&2
  exit 1
fi
if [ "$TOTAL_DOMAINS" -lt 10 ]; then
  echo "only $TOTAL_DOMAINS domain values parsed; the help wording that states them changed" >&2
  exit 1
fi

# ------------------------------------------------------------- the guides

# Every flag the whole CLI has, so a guide is checked against all of them
# without this needing to know which command a flag belongs to.
for cmd in $COMMANDS; do
  help_tree "$cmd"
done > "$TMP/all-help"
flags_in "$TMP/all-help" > "$TMP/all-flags"

find src -name '*.md' -not -path 'src/cli/*' -exec cat {} + > "$TMP/guides"
flags_in "$TMP/guides" > "$TMP/guide-flags"
while read -r flag; do
  case " $FOREIGN_FLAGS " in
    *" $flag "*) continue ;;
  esac
  grep -qx -- "$flag" "$TMP/all-flags" ||
    fail "the guides mention $flag and no jamstream command has it. If it belongs to another tool, add it to FOREIGN_FLAGS in this script."
done < "$TMP/guide-flags"

# An exemption the CLI has since grown means the guides stopped being checked
# against the real thing.
for flag in $FOREIGN_FLAGS; do
  if grep -qx -- "$flag" "$TMP/all-flags"; then
    fail "$flag is in FOREIGN_FLAGS and the CLI now has it; drop the exemption"
  fi
done

if [ "$status" -eq 0 ]; then
  echo "cli docs match --help: $COUNT commands, $TOTAL_FLAGS flags," \
    "$TOTAL_POSITIONALS positionals, $TOTAL_DOMAINS domain values"
fi
exit "$status"
