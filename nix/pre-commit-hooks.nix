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
  # commits. CI catches the same failure via the clippy/nextest
  # builds, so this hook is dev-ergonomics:
  # fail at commit time instead of 10min later.
  sqlx-prepare-check = {
    enable = true;
    name = "sqlx-prepare-check";
    entry = toString (
      pkgs.writeShellScript "sqlx-prepare-check" ''
        # language=system → this runs in the COMMITTING environment, not a
        # sandbox. Outside the dev shell (IDE git UIs, bare terminals) the
        # build env is incomplete (no LIBCLANG_PATH/PG_BIN/cmake) and the
        # check could only fail confusingly. Skip with a note instead —
        # CI's hermetic checks are the real enforcement. The sentinel is
        # the rio-specific RIO_DEVSHELL, not a generic tool var like
        # PROTOC: any other protobuf project's shell exports PROTOC too,
        # and passing the guard in such a foreign env reproduces exactly
        # the confusing failure the guard exists to prevent.
        if [ -z "''${RIO_DEVSHELL:-}" ]; then
          echo 'sqlx-prepare-check: not in the rio dev shell (RIO_DEVSHELL unset); skipping — CI enforces this gate' >&2
          exit 0
        fi
        # A stale shell can outlive `nix store gc`: RUSTC_WRAPPER then
        # points at a collected store path and every cargo spawn dies
        # before kache's fail-open could run — which the || branch below
        # would misread as a stale sqlx cache and block the commit with
        # the wrong message. Skip with the real reason instead.
        if [ -n "''${RUSTC_WRAPPER:-}" ] && [ ! -x "''${RUSTC_WRAPPER}" ]; then
          echo 'sqlx-prepare-check: RUSTC_WRAPPER does not resolve (stale shell after nix store gc?); skipping — re-enter the dev shell. CI enforces this gate' >&2
          exit 0
        fi
        # Re-pin the sqlx contract variable from THIS repo's toplevel:
        # the inherited value is frozen at shell entry and may belong to
        # a sibling worktree (tmux pane that cd'd over) — the check must
        # validate the staged queries against this checkout's cache.
        SQLX_OFFLINE_DIR="$(git rev-parse --show-toplevel)/.sqlx"
        export SQLX_OFFLINE_DIR
        # Only check if any staged .rs file touches a query! macro.
        # Otherwise this is a no-op (e.g. pure-refactor commits
        # that don't change SQL).
        if git diff --cached --name-only -- '*.rs' \
           | xargs -r grep -l 'query!\|query_as!\|query_scalar!' \
           | grep -q .; then
          # --all-targets: cfg(test)/tests/ query! sites are in the
          # offline cache too (regen sqlx prepares with `-- --all-targets`
          # for the LivePin contract anchors) — without it a stale
          # test-only entry sails through commit and fails in CI ~10min
          # later. Scoped to query!-touching commits, so the wider unit
          # graph only compiles when it can actually catch something.
          SQLX_OFFLINE=true cargo check --quiet --all-targets -p rio-scheduler -p rio-store -p rio-controller \
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
        trap 'rm -rf "$tmp"' EXIT
        # Three workspaces: root + the two fuzz subworkspaces
        # (each has its own Cargo.lock + Cargo.json consumed by
        # nix/fuzz.nix). crate2nix emits path fields relative to
        # the output file's directory, so -o $tmp/... would
        # produce ../../root/... paths that never match — generate
        # in-place under .check, diff, then clean up.
        for dir in . fuzz/rio-nix fuzz/rio-store; do
          # Snapshot Cargo.lock — `cargo metadata` inside
          # crate2nix can bump transitive deps if the local
          # cache is cold. Restore so the check has no side
          # effects even if crate2nix fails under set -e.
          cp "$dir/Cargo.lock" "$tmp/Cargo.lock.orig"
          ( cd "$dir" && ${crate2nixCli}/bin/crate2nix generate --format json -o Cargo.json.check )
          cp "$tmp/Cargo.lock.orig" "$dir/Cargo.lock"
          echo >> "$dir/Cargo.json.check"  # match end-of-file-fixer
          if ! diff -q "$dir/Cargo.json" "$dir/Cargo.json.check" >/dev/null; then
            rm -f "$dir/Cargo.json.check"
            echo "error: $dir/Cargo.json is stale — run \`cargo xtask regen cargo-json\`"
            exit 1
          fi
          rm -f "$dir/Cargo.json.check"
        done
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
  #
  # Like crate2nix-check above, this hook no-ops in the hermetic
  # `pre-commit run --all-files` derivation (nothing is staged) and
  # is bypassed by `--no-verify`. The hermetic backstop is
  # `checks.<system>.hakari-drift` (nix/misc-checks.nix).
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
