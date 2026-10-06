#!/usr/bin/env bash
# Build Portman.dmg locally (unsigned), for the host architecture or, with
# `--universal`, for Apple Silicon + Intel. Prints the path to the .dmg.
set -euo pipefail
cd "$(dirname "$0")/../app"

args=(--bundles dmg)
if [ "${1:-}" = "--universal" ]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  args+=(--target universal-apple-darwin)
fi

[ -d node_modules ] || npm ci
# CI=true skips the Finder AppleScript that lays out the DMG window, which
# otherwise needs Automation permission and pops Finder up.
CI=true npx tauri build "${args[@]}"

find ../target -path '*/bundle/dmg/*.dmg' -newer package.json -print
