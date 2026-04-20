# Sourced by non-interactive shells spawned by Claude Code's Bash tool:
#   - bash: via $BASH_ENV
#   - zsh:  via $ZDOTDIR/.zshenv -> this file
# Loads the cwd's direnv environment so dev-shell tools (cargo, tracey,
# protoc, ...) are available without an explicit `nix develop -c` wrapper.
#
# Degrades silently if direnv is missing or the .envrc is not `direnv allow`ed.

[ -n "${_RIO_DIRENV_LOADED:-}" ] && return 0
export _RIO_DIRENV_LOADED=1

if command -v direnv >/dev/null 2>&1; then
  # `direnv export bash` emits plain `export K='v';` lines that zsh parses
  # identically, so this one invocation serves both shells.
  eval "$(direnv export bash 2>/dev/null)"
fi
