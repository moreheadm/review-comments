#!/usr/bin/env bash
set -euo pipefail
PLUGIN_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
REPO=$(mktemp -d)
GIT_REPO=$(mktemp -d)
STATE=$(mktemp -d)
trap 'rm -rf "$REPO" "$GIT_REPO" "$STATE"' EXIT
mkdir -p "$REPO/src"
jj git init --colocate "$REPO" >/dev/null
git -C "$REPO" config user.name "rv test"
git -C "$REPO" config user.email "rv-test@example.invalid"
printf "return 1\nreturn 2\n" > "$REPO/src/example.lua"
git -C "$REPO" add src/example.lua
git -C "$REPO" commit -m fixture >/dev/null
mkdir -p "$GIT_REPO/src"
git -C "$GIT_REPO" init -q
git -C "$GIT_REPO" config user.name "rv test"
git -C "$GIT_REPO" config user.email "rv-test@example.invalid"
printf "plain git\n" > "$GIT_REPO/src/plain.txt"
: > "$GIT_REPO/src/empty.txt"
git -C "$GIT_REPO" add src/plain.txt src/empty.txt
git -C "$GIT_REPO" commit -m fixture >/dev/null
GIT_OID=$(git -C "$GIT_REPO" rev-parse HEAD)
OID=$(jj -R "$REPO" log -r @ --no-graph -T 'commit_id ++ "\n"' | tr -d '\n')
if [[ ! "$OID" =~ ^[0-9a-f]{40}$ && ! "$OID" =~ ^[0-9a-f]{64}$ ]]; then
  echo "Could not resolve full jj fixture ID: $OID" >&2
  exit 1
fi
export RV_PLUGIN_ROOT="$PLUGIN_ROOT"
export RV_TEST_REPO="$REPO"
export RV_GIT_TEST_REPO="$GIT_REPO"
export RV_GIT_TEST_OID="$GIT_OID"
export RV_FAKE_RV="$PLUGIN_ROOT/tests/fake-rv"
export RV_FAKE_STATE="$STATE"
export RV_FAKE_LOG="$STATE/commits.log"
export RV_FAKE_COMMIT_OID="$OID"
export RV_FAKE_REVIEW_OID=eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
cd "$PLUGIN_ROOT"
nvim --headless -u NONE -c 'luafile tests/headless.lua' -c 'qa!'
