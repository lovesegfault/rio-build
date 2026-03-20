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

# Regenerate .sqlx/ offline query cache. Required after any query!(...)
# macro edit or migration change. Spins up an ephemeral PG (same
# initdb+postgres bootstrap as rio-test-support::TestDb), runs
# migrations, runs `cargo sqlx prepare --workspace`.
#
# Output: .sqlx/query-<hash>.json per query! callsite — commit these.
# Old orphaned JSON files (from deleted/changed queries) are cleared
# by prepare itself.
#
# Budget: ~15s (initdb 2s + migrate 1s + prepare/compile ~10s).
sqlx-prepare:
    #!/usr/bin/env bash
    set -euo pipefail
    PGDATA=$(mktemp -d)
    PGPORT=$(shuf -i 49152-65535 -n 1)
    trap 'pg_ctl -D "$PGDATA" stop -m immediate 2>/dev/null || true; rm -rf "$PGDATA"' EXIT
    initdb -D "$PGDATA" -A trust --no-sync >/dev/null
    pg_ctl -D "$PGDATA" -o "-p $PGPORT -k $PGDATA" -l "$PGDATA/pg.log" start -w >/dev/null
    export DATABASE_URL="postgres://${USER:-postgres}@localhost:$PGPORT/postgres?host=$PGDATA"
    # Devshell sets SQLX_OFFLINE=true globally so cargo build/check
    # works without PG; unset here so prepare actually hits the DB.
    unset SQLX_OFFLINE
    cargo sqlx migrate run --source migrations
    # --check first: exits 0 if cache is current (no-op); non-zero
    # if stale → fall through to regenerate.
    cargo sqlx prepare --workspace --check 2>/dev/null \
        || cargo sqlx prepare --workspace
    echo "sqlx cache: $(ls .sqlx/ 2>/dev/null | wc -l) queries"
