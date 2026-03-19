set dotenv-load := true
set dotenv-filename := ".env.local"

# Cloud deployment modules. `just <mod>` lists recipes for that
# module; `just <mod> <recipe>` runs one. Each module file has its
# own `set shell` — settings do NOT inherit into modules, so forget
# that line in a new module and you silently lose pipefail.
mod eks 'infra/eks.just'
mod dev 'infra/dev.just'

default:
    @just --list --list-submodules

# Run mutation testing locally (slow — budget ~30-60min for the
# scoped target set in .config/mutants.toml). Results land in
# mutants.out/ under $PWD. Diff missed.txt against the last run to
# spot new survivors.
#
# `--in-place` mutates your working tree: commit or stash first.
# cargo-mutants restores the original source after each mutation,
# but a ^C mid-run can leave a mutated file behind.
#
# For the hermetic version (nix sandbox, vendored deps, pinned
# toolchain): `nix build .#mutants` — week-over-week comparable.
mutants:
    cargo mutants --in-place --no-shuffle --config .config/mutants.toml
    @echo "Caught:   $(jq '[.outcomes[] | select(.summary == \"CaughtMutant\")] | length' mutants.out/outcomes.json)"
    @echo "Missed:   $(jq '[.outcomes[] | select(.summary == \"MissedMutant\")] | length' mutants.out/outcomes.json)"
    @echo "See mutants.out/missed.txt for details"
