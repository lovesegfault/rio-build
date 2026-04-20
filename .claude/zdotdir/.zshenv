# zsh entry point for Claude Code's Bash-tool shells.
# Settings.json points $ZDOTDIR here; non-interactive zsh always sources
# $ZDOTDIR/.zshenv. We just delegate to the shared direnv loader.

source "${${(%):-%x}:A:h}/../direnv-init.sh"
