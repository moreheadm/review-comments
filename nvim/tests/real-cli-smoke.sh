#!/usr/bin/env bash
set -euo pipefail
PLUGIN_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
REPO=$(mktemp -d)
trap 'rm -rf "$REPO"' EXIT
mkdir -p "$REPO/src"
jj git init --colocate "$REPO" >/dev/null
git -C "$REPO" config user.name "rv nvim smoke"
git -C "$REPO" config user.email "rv-nvim-smoke@example.invalid"
printf "return 1\nreturn 2\nreturn 3\n" > "$REPO/src/review.lua"
git -C "$REPO" add src/review.lua
git -C "$REPO" commit -m fixture >/dev/null
export RV_PLUGIN_ROOT="$PLUGIN_ROOT"
export RV_REAL_REPO="$REPO"
export RV_REAL_CLI=${RV_CLI:-/home/max/prog/devtools/review-comments/cli/target/debug/rv}
if [[ ! -x "$RV_REAL_CLI" ]]; then
  echo "rv CLI not found: $RV_REAL_CLI" >&2
  exit 1
fi
cd "$PLUGIN_ROOT"
nvim --headless -u NONE -c 'luafile tests/real-cli-smoke.lua' -c 'qa!'
