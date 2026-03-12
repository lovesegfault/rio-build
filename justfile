set dotenv-load := true
set dotenv-filename := ".env.local"

# Cloud deployment modules. `just <mod>` lists recipes for that
# module; `just <mod> <recipe>` runs one. Each module file has its
# own `set shell` — settings do NOT inherit into modules, so forget
# that line in a new module and you silently lose pipefail.
mod eks
mod dev

default:
    @just --list --list-submodules
