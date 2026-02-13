#!/usr/bin/env bash
echo '{"async":true,"asyncTimeout":30000}'

# Verify Rust toolchain is available
if ! command -v cargo &>/dev/null; then
  echo "⚠️  cargo not found. Enter the dev shell: nix develop"
fi

# Check if Cargo.lock is stale
if [ -f "Cargo.toml" ] && [ -f "Cargo.lock" ]; then
  if [ "Cargo.toml" -nt "Cargo.lock" ]; then
    echo "📦 Cargo.lock may be stale, updating..."
    cargo check --quiet 2>/dev/null || echo "⚠️  cargo check failed. Run: cargo check"
  fi
fi
