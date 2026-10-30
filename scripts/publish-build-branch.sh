#!/usr/bin/env bash
# Publish the built binaries to the `build` branch as ONE commit.
#
# The branch is a download shelf, not history: every publish replaces it with a single
# parentless commit, so `git log build` never grows past one entry.
#
# usage: publish-build-branch.sh <dir-with-binaries> [source-sha]
# env: GH_TOKEN, GITHUB_REPOSITORY
set -euo pipefail

DIST=${1:?directory with binaries required}
SHA=${2:-${SHA:-$(git rev-parse HEAD)}}
AUTH="https://x-access-token:${GH_TOKEN}@github.com/${GITHUB_REPOSITORY}.git"

# absolute, because the script changes directory below
DIST="$(cd "$DIST" && pwd)"
mapfile -t FILES < <(find "$DIST" -type f -name 'Norikimika-Bot-*' | sort)
if [ ${#FILES[@]} -ne 3 ]; then
  echo "expected 3 binaries under $DIST, found ${#FILES[@]}" >&2
  find "$DIST" -maxdepth 3 -print >&2
  exit 1
fi

PUB="$(mktemp -d)"
cd "$PUB"
git init -q -b build
git config user.name 'github-actions[bot]'
git config user.email '41898282+github-actions[bot]@users.noreply.github.com'
cp "${FILES[@]}" .
git add -A

git remote add origin "$AUTH"
if git fetch -q --depth 1 origin build 2>/dev/null; then
  if [ "$(git rev-parse FETCH_HEAD^{tree})" = "$(git write-tree)" ]; then
    echo "binaries unchanged, nothing to publish"
    exit 0
  fi
fi

git commit -qm "build: Norikimika-Bot @ $SHA"

for attempt in 1 2 3 4 5; do
  if git push -q --force origin HEAD:build; then
    echo "published 3 binaries to build as a single commit ($SHA)"
    exit 0
  fi
  echo "push attempt $attempt rejected, retrying"
  sleep $((attempt * 3))
done

echo "could not publish to build" >&2
exit 1
