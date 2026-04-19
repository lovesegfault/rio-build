# Custom pre-commit hooks (the writeShellScript ones — standard
# `*.enable = true` toggles stay inline in flake.nix).
#
# Returned attrset is merged into pre-commit.settings.hooks.
{
  pkgs,
  crate2nixCli,
}:
{
  # Reject commits containing cargo-mutants dirty markers.
  # `cargo xtask mutants` mutates source in-place; if it crashes or is
  # interrupted, the mutated line (with `/* ~ changed by
  # cargo-mutants ~ */`) survives in the worktree. A blind
  # commit then ships a mutant. The marker string is reliable
  # — cargo-mutants always wraps its mutations with it.
  check-mutants-marker = {
    enable = true;
    name = "check-mutants-marker";
    entry = toString (
      pkgs.writeShellScript "check-mutants-marker" ''
        # Marker verified for cargo-mutants 26.2.0 —
        # MUTATION_MARKER_COMMENT const at src/mutate.rs
        # upstream. If a cargo-mutants bump changes the
        # marker string, this hook SILENTLY passes —
        # re-verify on major-version bumps.
        # Only scan .rs files (cargo-mutants only touches Rust).
        # grep -l for file-list, exit 1 if any match.
        if git diff --cached --name-only -- '*.rs' \
           | xargs -r grep -l 'changed by cargo-mutants' 2>/dev/null \
           | grep -q .; then
          echo 'error: cargo-mutants marker found in staged .rs files'
          echo 'cargo-mutants left a dirty mutation — `git checkout -- <file>` to revert'
          git diff --cached --name-only -- '*.rs' \
            | xargs -r grep -l 'changed by cargo-mutants' 2>/dev/null
          exit 1
        fi
      ''
    );
    files = "\\.rs$";
    language = "system";
    pass_filenames = false;
  };

  # Reject commits that change a query! SQL string without
  # regenerating .sqlx/. With SQLX_OFFLINE=true, any query!
  # whose SQL hash no longer matches a .sqlx/*.json file
  # fails to compile — so `cargo check` on the crates that
  # use query! is the definitive staleness check. ~5s
  # incremental. Fires only on .rs changes to skip docs-only
  # commits. CI (`.#ci`) catches the same failure via the
  # clippy/nextest builds, so this hook is dev-ergonomics:
  # fail at commit time instead of 10min later.
  sqlx-prepare-check = {
    enable = true;
    name = "sqlx-prepare-check";
    entry = toString (
      pkgs.writeShellScript "sqlx-prepare-check" ''
        # Only check if any staged .rs file touches a query! macro.
        # Otherwise this is a no-op (e.g. pure-refactor commits
        # that don't change SQL).
        if git diff --cached --name-only -- '*.rs' \
           | xargs -r grep -l 'query!\|query_as!\|query_scalar!' \
           | grep -q .; then
          SQLX_OFFLINE=true cargo check --quiet -p rio-scheduler -p rio-store \
            || { echo 'sqlx query cache stale — run `cargo xtask regen sqlx`'; exit 1; }
        fi
      ''
    );
    files = "\\.rs$";
    language = "system";
    pass_filenames = false;
  };

  # Reject commits that change Cargo.toml/Cargo.lock without
  # regenerating Cargo.json. crate2nix reads Cargo.lock to
  # produce the per-crate build graph; a stale Cargo.json
  # means nix builds use the OLD dep set while cargo uses
  # the new one — silent divergence until a nix-only build
  # fails with "crate foo not found". File-gated on
  # Cargo.toml/Cargo.lock so unrelated commits don't pay
  # the ~10s regeneration cost.
  crate2nix-check = {
    enable = true;
    name = "crate2nix-check";
    entry = toString (
      pkgs.writeShellScript "crate2nix-check" ''
        set -euo pipefail
        # Gate on staged Cargo.{toml,lock}. In the hermetic
        # check derivation (pre-commit run --all-files on a
        # clean checkout), nothing is staged → no-op. This
        # also keeps the hook off the hot path for commits
        # that don't touch the dep graph.
        if ! git diff --cached --name-only \
           | grep -qE '(^|/)Cargo\.(toml|lock)$'; then
          exit 0
        fi
        tmp=$(mktemp -d)
        # Snapshot Cargo.lock — `cargo metadata` inside
        # crate2nix can bump transitive deps if the local
        # cache is cold. Restore via TRAP so the check
        # has no side effects even if crate2nix fails
        # under set -e (the old trap only rm'd the
        # snapshot, leaving a mutated Cargo.lock).
        cp Cargo.lock "$tmp/Cargo.lock.orig"
        trap '[ -f "$tmp/Cargo.lock.orig" ] && cp "$tmp/Cargo.lock.orig" Cargo.lock; rm -rf "$tmp"; rm -f Cargo.json.check' EXIT
        # Generate in workspace root — crate2nix emits path
        # fields relative to the output file's directory, so
        # -o $tmp/... would produce ../../root/... paths that
        # never match the committed Cargo.json.
        ${crate2nixCli}/bin/crate2nix generate --format json -o Cargo.json.check
        echo >> Cargo.json.check  # match end-of-file-fixer
        if ! diff -q Cargo.json Cargo.json.check >/dev/null; then
          echo 'error: Cargo.json is stale — run `cargo xtask regen cargo-json`'
          exit 1
        fi
      ''
    );
    files = "(^|/)Cargo\\.(toml|lock)$";
    language = "system";
    pass_filenames = false;
  };

  # Reject commits that change Cargo.toml/Cargo.lock without
  # regenerating workspace-hack. A stale workspace-hack means
  # per-package builds use a different feature set than the
  # workspace build → cache thrash. `hakari verify` is fast
  # (metadata-only, no compile).
  hakari-check = {
    enable = true;
    name = "hakari-check";
    entry = toString (
      pkgs.writeShellScript "hakari-check" ''
        set -euo pipefail
        if ! git diff --cached --name-only \
           | grep -qE '(^|/)Cargo\.(toml|lock)$'; then
          exit 0
        fi
        ${pkgs.cargo-hakari}/bin/cargo-hakari hakari verify 2>/dev/null || {
          echo 'error: workspace-hack is stale — run `cargo xtask regen hakari`'
          exit 1
        }
      ''
    );
    files = "(^|/)Cargo\\.(toml|lock)$";
    language = "system";
    pass_filenames = false;
  };
}
