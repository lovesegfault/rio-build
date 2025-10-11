#!/usr/bin/env bash
echo '{"async":true,"asyncTimeout":60000}'

set -e

# Silent helper function
silent_check() {
  "$@" &>/dev/null
}

# Check if we're in a Nix environment
if [ -z "$IN_NIX_SHELL" ] && command -v nix &> /dev/null; then
  echo "💡 Not in Nix shell. Run 'nix develop' or 'direnv allow' to enter dev environment"
fi

# Check if Cargo.lock is up to date with Cargo.toml
if [ -f "Cargo.toml" ]; then
  if [ ! -f "Cargo.lock" ] || [ "Cargo.toml" -nt "Cargo.lock" ]; then
    echo "📦 Cargo.lock is outdated, running cargo check to update..."
    if ! cargo check --quiet 2>/dev/null; then
      echo "⚠️  cargo check failed. Dependencies may need attention."
      echo "   Run: cargo check"
    fi
  fi
fi

# Quick project health check
if [ -f "Cargo.toml" ] && command -v cargo &> /dev/null; then
  # Check if the project builds (cached, so fast on subsequent runs)
  if ! silent_check cargo check; then
    echo "⚠️  Project has build errors. Run: cargo check"
  fi
fi

# Install pre-commit hooks if not already installed
if [ -f "flake.nix" ] && [ ! -f ".git/hooks/pre-commit" ]; then
  echo "🪝 Pre-commit hooks not installed. They'll auto-install when you enter nix develop"
fi

echo "✅ Environment ready for development"
echo ""
echo "Quick commands:"
echo "  cargo build    - Build the project"
echo "  cargo test     - Run tests"
echo "  cargo clippy   - Run linter"
echo "  nix fmt        - Format all files"
