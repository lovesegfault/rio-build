#!/usr/bin/env bash
echo '{"async":true,"asyncTimeout":30000}'

# Verify Rust toolchain is available
if ! command -v cargo &>/dev/null; then
  echo "⚠️  cargo not found. Enter the dev shell: nix develop"
fi
