#!/usr/bin/env bash
# Regenerate Cargo.json after Cargo.lock changes (new deps, new crates).
# Since crate2nix PR #453, the JSON output natively includes
# devDependencies for workspace members — no post-processing needed.

set -euo pipefail
cd "$(dirname "$(realpath "$0")")/.."

echo "=== crate2nix generate ==="
nix develop -c bash -c 'crate2nix generate --format json -o Cargo.json'
# crate2nix doesn't emit a trailing newline; end-of-file-fixer requires one.
echo >> Cargo.json

echo "=== done — commit Cargo.json ==="
git add Cargo.json
git diff --cached --stat Cargo.json
