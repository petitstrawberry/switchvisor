#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Keep untracked source and target/ out of Nix's flake source snapshot.
mkdir -p .cache/toolchain-flake
cp flake.nix flake.lock .cache/toolchain-flake/
if command -v nix >/dev/null 2>&1; then
    task_nix=nix
else
    task_nix=/nix/var/nix/profiles/default/bin/nix
fi
exec "$task_nix" develop "path:$PWD/.cache/toolchain-flake" "$@"
