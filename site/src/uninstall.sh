#!/bin/sh
# JamStream CLI uninstaller, the pair of install.sh. Served by the site at
# /uninstall.sh the same way:
#
#   curl -fsSL https://sean-reid.github.io/jamstream/uninstall.sh | sh
#
# Removes the jamstream CLI (and jamstreamd, if --with-server installed it)
# from wherever install.sh put them. Session data and credentials are kept
# unless asked for, because they are what let a reinstall find a session
# that is still running somewhere.
#
# Options and environment:
#   --purge                 also delete the JamStream data directory
#   --yes                   do not ask before removing anything
#   JAMSTREAM_INSTALL_DIR   look here as well as the default directories
#
# POSIX sh. Checked by shellcheck in CI (.github/workflows/docs-check.yml).

set -eu

say() { printf '%s\n' "$1"; }
fail() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

usage() {
  say "usage: uninstall.sh [--purge] [--yes]"
  say ""
  say "Removes the jamstream CLI installed by install.sh."
  say "  --purge   also delete the JamStream data directory"
  say "  --yes     do not ask before removing anything"
  say ""
  say "Set JAMSTREAM_INSTALL_DIR if you installed somewhere custom."
}

purge=0
assume_yes=0
for arg in "$@"; do
  case "$arg" in
    --purge) purge=1 ;;
    --yes) assume_yes=1 ;;
    -h|--help)
      usage
      exit 0
      ;;
    *) fail "unknown option: $arg (this script takes --purge and --yes only)" ;;
  esac
done

# Where to look. install.sh installs to exactly one directory, so an
# explicit JAMSTREAM_INSTALL_DIR is searched alone rather than alongside
# the defaults; anything else risks removing a binary the caller did not
# mean, which is exactly what an early version of this script did.
if [ -n "${JAMSTREAM_INSTALL_DIR:-}" ]; then
  candidates="$JAMSTREAM_INSTALL_DIR"
else
  candidates="/usr/local/bin ${HOME}/.local/bin"
fi

found=""
for dir in $candidates; do
  [ -n "$dir" ] || continue
  for name in jamstream jamstreamd; do
    bin="$dir/$name"
    [ -f "$bin" ] || continue
    case "$(readlink "$bin" 2>/dev/null || true)" in
      *Cellar*|*homebrew*)
        say "$bin belongs to Homebrew; run: brew uninstall jamstream-cli"
        continue
        ;;
    esac
    found="$found $bin"
  done
done

if [ -z "$found" ]; then
  say "nothing to remove: no jamstream binary in $candidates"
else
  # A session that is still running keeps costing money or holding a port
  # after the binary that can end it is gone. jamstream end or jamstream
  # sweep first, then uninstall.
  first=$(printf '%s' "$found" | awk '{print $1}')
  # status --json pretty-prints, so the match tolerates whitespace.
  if "$first" status --json 2>/dev/null | grep -Eq '"status":[[:space:]]*"running"'; then
    say "a session is still running (jamstream status):"
    "$first" status 2>/dev/null || true
    if [ "$assume_yes" -ne 1 ]; then
      fail "end it first (jamstream end --last), sweep strays (jamstream sweep), or rerun with --yes to remove the binary anyway"
    fi
    say "continuing anyway (--yes); the session keeps running until its own timers end it"
  fi

  for bin in $found; do
    if [ -w "$bin" ] || [ -w "$(dirname "$bin")" ]; then
      rm "$bin"
      say "removed $bin"
    else
      fail "$bin is not writable; rerun with sudo"
    fi
  done
fi

# Data: session records under the platform data directory. Credentials live
# in the OS keychain, which a shell script should not reach into; say where
# they are instead.
case "$(uname -s)" in
  Darwin) data_dir="${HOME}/Library/Application Support/jamstream" ;;
  *) data_dir="${XDG_DATA_HOME:-${HOME}/.local/share}/jamstream" ;;
esac

if [ "$purge" -eq 1 ]; then
  if [ -d "$data_dir" ]; then
    rm -rf "$data_dir"
    say "removed $data_dir"
  else
    say "no data directory at $data_dir"
  fi
else
  [ -d "$data_dir" ] && say "kept session data at $data_dir (rerun with --purge to delete it)"
fi

say "Cloud credentials, if you saved any, are in your OS keychain: search for jamstream and delete the entries."
say "The desktop app, if installed, is separate: on macOS drag JamStream out of /Applications."
