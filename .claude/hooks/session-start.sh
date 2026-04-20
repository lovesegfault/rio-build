#!/usr/bin/env bash
echo '{"async":true,"asyncTimeout":30000}'

# Resolve absolute paths for the direnv shim and pin them in
# settings.local.json. The checked-in settings.json uses relative paths
# (which only work while cwd == project root); settings.local.json overlays
# with absolutes so the shim survives `cd` and applies in subagent worktrees.
# settings.local.json is gitignored — per-machine, takes effect next session.
root="${CLAUDE_PROJECT_DIR:-$PWD}"
local_settings="$root/.claude/settings.local.json"

# Auto-allow this worktree's .envrc so the direnv shim works in fresh
# worktrees (isolation:"worktree" subagents, new `git worktree add`s).
# The .envrc is checked in and just does `use flake`, so this trusts code
# we already trust by virtue of running in this checkout.
if [[ -f "$root/.envrc" ]] && command -v direnv &>/dev/null; then
  direnv allow "$root" 2>/dev/null
fi

if command -v jq &>/dev/null; then
  base='{}'
  [[ -f "$local_settings" ]] && base="$(cat "$local_settings")"
  next="$(jq --arg be "$root/.claude/direnv-init.sh" \
             --arg zd "$root/.claude/zdotdir" \
    '.env.BASH_ENV = $be | .env.ZDOTDIR = $zd | .env.DIRENV_LOG_FORMAT = ""' \
    <<<"$base")"
  [[ "$next" != "$base" ]] && printf '%s\n' "$next" >"$local_settings"
fi

# Verify Rust toolchain is available
if ! command -v cargo &>/dev/null; then
  echo "⚠️  cargo not found. Enter the dev shell: nix develop"
fi
