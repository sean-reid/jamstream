#!/bin/sh
# Prints the newest published release tag that has finished uploading, for
# the CI jobs that install from a real release.
#
# SHA256SUMS is the last asset release.yml uploads, because it hashes all the
# others, so for the several minutes a release takes there is a published
# release with nothing in it and the installers correctly refuse it. Asking
# for the newest release turned that into a red docs-check on main during
# v0.2.0, which is not the question those jobs mean to ask: they are there to
# prove the scripts work. So a release counts only once its sums are up.
#
# sean-reid/jamstream by name rather than the checked out repository, because
# that is the repository the installers download from wherever they run.
#
# Needs gh with a token in GH_TOKEN. Run from anywhere.
set -eu

REPO=sean-reid/jamstream

tag=$(gh api "repos/$REPO/releases?per_page=30" --jq \
  'map(select(.draft == false and ([.assets[].name] | index("SHA256SUMS")))) | .[0].tag_name // empty')

if [ -z "$tag" ]; then
  echo "::error::no published $REPO release carries a SHA256SUMS asset, so there is nothing for the installer to verify against" >&2
  exit 1
fi

printf '%s\n' "$tag"
