#!/bin/sh
# Classifies the diff between two commits as `docs` or `code` for ci.yml's
# changes job, which skips the heavy jobs when a change is docs-only.
#
# Docs means the change cannot alter what the build jobs prove: markdown
# under site/, markdown at the repository root, and the committed images
# the site embeds. Everything else is code, deliberately including
# site/src/install.sh and its siblings (the site serves them as real
# installers and docs-check executes them), the workflows, this script,
# and any path these patterns have never seen: unknown runs the full
# matrix rather than guessing.
#
# Usage: classify-diff.sh <base> <head>
# The verdict is the only stdout line; per-file detail goes to stderr.
set -eu

cd "$(dirname "$0")/.."

classify_one() {
  case "$1" in
    # Executable product surface living beside the markdown.
    site/src/*.sh | site/src/*.ps1) echo code ;;
    site/src/images/*.png | site/src/images/*.svg | site/src/images/*.ico | \
      site/src/images/*.gif | site/src/images/*.jpg | site/src/images/*.webp) echo docs ;;
    site/*.md) echo docs ;;
    # Anything else in a directory, then markdown at the root. `*` in a
    # case pattern matches `/`, which is what lets site/*.md cover the
    # whole tree under site/ and makes this ordering load bearing.
    */*) echo code ;;
    *.md) echo docs ;;
    *) echo code ;;
  esac
}

# A pattern edit that misfiles the installer scripts or the crates would
# skip the build on exactly the change that needed it, so the rules prove
# themselves against known paths before touching the real diff.
selftest() {
  got=$(classify_one "$1")
  if [ "$got" != "$2" ]; then
    echo "self-test: classify_one $1 said $got, wanted $2; fix scripts/classify-diff.sh" >&2
    exit 1
  fi
}
selftest site/src/download.md docs
selftest site/src/cli/index.md docs
selftest site/src/images/session.png docs
selftest README.md docs
selftest site/src/install.sh code
selftest site/src/uninstall.ps1 code
selftest site/copy-previews.sh code
selftest scripts/classify-diff.sh code
selftest .github/workflows/ci.yml code
selftest crates/cli/src/main.rs code
selftest Cargo.lock code
selftest .config/nextest.toml code
selftest .gitattributes code
selftest packaging/README.md code

if [ $# -ne 2 ]; then
  echo "usage: $0 <base> <head>" >&2
  exit 2
fi

files=$(git diff --name-only "$1" "$2")
if [ -z "$files" ]; then
  echo "empty diff between $1 and $2; classifying as code" >&2
  echo code
  exit 0
fi

verdict=docs
while IFS= read -r f; do
  [ -n "$f" ] || continue
  kind=$(classify_one "$f")
  printf '%-4s  %s\n' "$kind" "$f" >&2
  [ "$kind" = docs ] || verdict=code
done <<EOF
$files
EOF

echo "$verdict"
