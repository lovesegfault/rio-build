#!/usr/bin/env bash
# Format the project after edits using treefmt via nix

if command -v nix &>/dev/null; then
  nix fmt 2>/dev/null
fi
