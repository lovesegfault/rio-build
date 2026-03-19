#!/usr/bin/env bash
# Format only the edited file (not the whole tree) — under 10 parallel
# worktrees each doing edits, full-tree treefmt is redundant work.
# PostToolUse hook receives tool input as JSON on stdin.

file_path=$(jq -r '.tool_input.file_path // empty' 2>/dev/null)
if [[ -n "$file_path" && -f "$file_path" ]] && command -v nix &>/dev/null; then
  nix fmt -- "$file_path" 2>/dev/null
fi
