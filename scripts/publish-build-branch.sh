#!/usr/bin/env bash
# Publish a built binary to the `build` branch.
# usage: publish-build-branch.sh <path-to-binary> <name-on-branch>
# env: GH_TOKEN, GITHUB_REPOSITORY
set -euo pipefail

BIN=${1:?binary path required}
NAME=${2:?name on branch required}
SHA=${SHA:-$(git rev-parse HEAD)}

cp "$BIN" "/tmp/$NAME"
AUTH="https://x-access-token:${GH_TOKEN}@github.com/${GITHUB_REPOSITORY}.git"
PUB="$(mktemp -d)"

if git clone --depth 1 --branch build "$AUTH" "$PUB" 2>/dev/null; then
  echo "build branch exists, updating"
else
  git -C "$PUB" init -q -b build
  git -C "$PUB" remote add origin "$AUTH"
fi

cp "/tmp/$NAME" "$PUB/"
git -C "$PUB" add -A
if git -C "$PUB" diff --cached --quiet; then
  echo "binary unchanged, nothing to publish"
  exit 0
fi

git -C "$PUB" -c user.name='github-actions[bot]' \
  -c user.email='41898282+github-actions[bot]@users.noreply.github.com' \
  commit -qm "build: $NAME @ $SHA"

for attempt in 1 2 3 4 5; do
  if git -C "$PUB" push -q origin HEAD:build; then
    echo "published $NAME to build branch"
    exit 0
  fi
  echo "push attempt $attempt rejected, rebasing on remote"
  git -C "$PUB" fetch -q origin build
  git -C "$PUB" rebase -q FETCH_HEAD || { git -C "$PUB" rebase --abort; exit 1; }
  sleep 3
done

echo "could not publish $NAME" >&2
exit 1
