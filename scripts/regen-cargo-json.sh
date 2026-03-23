#!/usr/bin/env bash
# Regenerate Cargo.json after Cargo.lock changes (new deps, new crates).
# crate2nix's JSON output mode omits devDependencies — inject-dev-deps.py
# post-processes from cargo metadata to add them back.

set -euo pipefail
cd "$(dirname "$(realpath "$0")")/.."

echo "=== crate2nix generate ==="
nix develop -c bash -c 'crate2nix generate --format json -o Cargo.json'

echo "=== inject-dev-deps.py ==="
nix develop -c python3 scripts/inject-dev-deps.py Cargo.json

echo "=== done — commit Cargo.json ==="
git add Cargo.json
git diff --cached --stat Cargo.json
