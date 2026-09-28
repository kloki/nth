#!/usr/bin/env bash
# Clone or update the pinned opencode reference at <repo>/refs/opencode.
#
#   sync-ref.sh            clone if missing, otherwise stay on the current pin
#   sync-ref.sh --repin    fetch and move the pin to the latest default branch
#
# Prints the pinned commit SHA on the last line.
set -euo pipefail

REPO_URL="https://github.com/anomalyco/opencode"
ROOT="$(git rev-parse --show-toplevel)"
REF="$ROOT/refs/opencode"

if [[ ! -d "$REF/.git" ]]; then
  echo "→ cloning $REPO_URL into refs/opencode" >&2
  git clone --quiet "$REPO_URL" "$REF"
elif [[ "${1:-}" == "--repin" ]]; then
  echo "→ fetching and re-pinning to latest default branch" >&2
  git -C "$REF" fetch --quiet origin
  git -C "$REF" checkout --quiet --detach origin/HEAD
else
  echo "✓ reference exists, keeping current pin" >&2
fi

git -C "$REF" rev-parse HEAD
