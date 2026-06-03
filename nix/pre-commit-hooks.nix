# Custom pre-commit hooks (the writeShellScript ones — standard
# `*.enable = true` toggles stay inline in flake.nix).
#
# Returned attrset is merged into pre-commit.settings.hooks.
{
  pkgs,
  crate2nixCli,
}:
let
  # Shared guard for every hook that spawns cargo (directly or via a
  # tool that runs `cargo metadata`). Two tests, in order:
  #
  # 1. RIO_DEVSHELL sentinel: language=system hooks run in the
  #    COMMITTING environment, not a sandbox. Outside the dev shell
  #    (IDE git UIs, bare terminals) the build env is incomplete (no
  #    LIBCLANG_PATH/PG_BIN, possibly a version-divergent system cargo)
  #    and any cargo-spawning hook can only fail confusingly — or
  #    worse, emit a false "stale" verdict from the wrong toolchain.
  #    The sentinel is the rio-specific RIO_DEVSHELL, not a generic
  #    tool var like PROTOC: a foreign protobuf project's shell exports
  #    PROTOC too, and passing the guard there reproduces exactly the
  #    confusing failure the guard exists to prevent.
  # 2. Dangling RUSTC_WRAPPER: a stale shell can outlive `nix store
  #    gc`, leaving RUSTC_WRAPPER pointing at a collected store path —
  #    and cargo execs `$RUSTC_WRAPPER rustc -vV` even for
  #    `cargo metadata`, so crate2nix-check and hakari-check die
  #    exactly like compile hooks do, just with a less obvious error.
  #    PATH-aware like cargo's own resolution: a bare-name wrapper (a
  #    user-level RUSTC_WRAPPER=sccache) must be looked up, not tested
  #    as a CWD-relative pathname.
  #
  # Both skip (exit 0) with the real reason: every guarded gate has a
  # hermetic CI backstop (clippy/nextest for sqlx staleness,
  # hakari-drift, crate2nix-drift (root + fuzz variants)), so
  # fail-open here is honest.
  cargoEnvGuard = ''
    if [ -z "''${RIO_DEVSHELL:-}" ]; then
      echo "''${0##*/}: not in the rio dev shell (RIO_DEVSHELL unset); skipping — CI enforces this gate" >&2
      exit 0
    fi
    if [ -n "''${RUSTC_WRAPPER:-}" ]; then
      case "$RUSTC_WRAPPER" in
        */*) wrapper_ok=$([ -x "$RUSTC_WRAPPER" ] && echo 1 || echo 0) ;;
        *) wrapper_ok=$(command -v "$RUSTC_WRAPPER" >/dev/null 2>&1 && echo 1 || echo 0) ;;
      esac
      if [ "$wrapper_ok" = 0 ]; then
        echo "''${0##*/}: RUSTC_WRAPPER does not resolve (stale shell after nix store gc?); skipping — re-enter the dev shell. CI enforces this gate" >&2
        exit 0
      fi
      unset wrapper_ok
    fi
  '';
in
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
  # fails to compile — so `cargo check --all-targets` on the
  # crates that use query! is the definitive staleness check
  # (seconds warm; the unit-test graph of three crates when
  # cold). Fires only on .rs/.sqlx changes to skip docs-only
  # commits. CI catches the same failure via the clippy/nextest
  # builds, so this hook is dev-ergonomics:
  # fail at commit time instead of 10min later.
  sqlx-prepare-check = {
    enable = true;
    name = "sqlx-prepare-check";
    entry = toString (
      pkgs.writeShellScript "sqlx-prepare-check" ''
        ${cargoEnvGuard}
        # Re-pin the sqlx contract variable from THIS repo's toplevel:
        # the inherited value is frozen at shell entry and may belong to
        # a sibling worktree (tmux pane that cd'd over) — the check must
        # validate the staged queries against this checkout's cache.
        SQLX_OFFLINE_DIR="$(git rev-parse --show-toplevel)/.sqlx"
        export SQLX_OFFLINE_DIR
        # Trigger = a staged .rs file containing a REAL callsite-shaped
        # match: //-comment lines stripped, then the sqlx::-qualified
        # macro shape (also covers query_file!/query_unchecked!).
        # 2026-06 census: every real callsite in-tree is
        # `sqlx::`-qualified (zero `use sqlx::query…` imports), and
        # every out-of-crate match is a pure //-comment line (xtask +
        # rio-buildhash + rio-migrations doc-comments) — the previous
        # whole-file grep hard-blocked commits on exactly those.
        # Residuals, accepted: a block comment or string literal shaped
        # like `sqlx::query…!` still trips this; a future UNqualified
        # `query!(` behind a use-import under-triggers and the hook
        # silently skips — CI clippy/nextest remains the staleness
        # backstop either way (this hook is dev-ergonomics, per the
        # header above).
        real_callsite() {
          grep -Ev '^[[:space:]]*//' -- "$1" 2>/dev/null \
            | grep -Eq 'sqlx::query[a-z_]*!'
        }
        # Owning crate = nearest ancestor dir with a Cargo.toml.
        # Walking up (instead of `cut -d/ -f1`) maps nested crates to
        # the manifest that actually owns the file.
        owning_crate() {
          d=$(dirname -- "$1")
          while [ "$d" != "." ] && [ "$d" != "/" ]; do
            if [ -f "$d/Cargo.toml" ]; then
              printf '%s\n' "$d"
              return 0
            fi
            d=$(dirname -- "$d")
          done
        }
        # Derive the crate list from the staged hits instead of
        # hand-listing it: a hand list here was a third independent
        # mirror of "the set of query! crates" (alongside crate2nix.nix
        # sqlxQueryCrates and the per-crate build.rs trackers) — a
        # fourth crate gaining query! would fire the trigger, pass
        # vacuously, and fail in CI 10min later.
        crates=$(git diff --cached --name-only --diff-filter=d -- '*.rs' \
          | while IFS= read -r f; do
              real_callsite "$f" && owning_crate "$f"
            done | sort -u)
        # A staged .sqlx/ edit can stale ANY consumer of the cache, not
        # just crates whose .rs files moved — widen to every
        # track_sqlx-wired crate. Derived from the build scripts, NOT
        # `grep rio_buildhash`: rio-migrations wires track_migrations
        # (sqlx::migrate! reads migrations/, never .sqlx/) and must not
        # burn a vacuous cargo check here.
        if git diff --cached --name-only | grep -q '^\.sqlx/'; then
          crates=$(
            {
              printf '%s\n' "$crates"
              git ls-files '*build.rs' \
                | xargs -r grep -l track_sqlx 2>/dev/null \
                | xargs -r -n1 dirname
            } | sort -u | grep -v '^$'
          )
        fi
        [ -n "$crates" ] || exit 0
        args=""
        for c in $crates; do
          [ -f "$c/Cargo.toml" ] || continue
          if [ -f "$c/build.rs" ] && grep -q track_sqlx "$c/build.rs"; then
            # CHECK arm. -p takes the package name from the manifest —
            # the directory name is convention, not contract.
            args="$args -p $(awk -F'"' '/^name[[:space:]]*=/{print $2; exit}' "$c/Cargo.toml")"
          elif awk '/^\[dependencies\]/{f=1;next} /^\[/{f=0} f && /^sqlx[[:space:]]*[.=]/{found=1} END{exit !found}' "$c/Cargo.toml"; then
            # REFUSE arm: real callsites + sqlx in [dependencies] but
            # no tracker — kache would replay compiles against an
            # unobserved .sqlx. (sqlx only in [dev-dependencies] with
            # test-only query! would skip instead of refuse; no such
            # crate exists.)
            echo "sqlx-prepare-check: $c has sqlx in [dependencies] and real query! callsites but no track_sqlx() in build.rs — kache would replay compiles against an unobserved .sqlx; wire rio-buildhash::track_sqlx() (see CLAUDE.md, out-of-band macro inputs)" >&2
            exit 1
          fi
          # else: callsite-shaped text in a crate without the sqlx
          # runtime dep (string literal / block comment in a tool
          # crate) — cannot be a compiled query!, skip silently.
        done
        [ -n "$args" ] || exit 0
        # --all-targets: cfg(test)/tests/ query! sites are in the
        # offline cache too (regen sqlx prepares with `-- --all-targets`
        # for the LivePin contract anchors) — without it a stale
        # test-only entry sails through commit and fails in CI ~10min
        # later. Scoped to query!-touching commits, so the wider unit
        # graph only compiles when it can actually catch something.
        # Compiler diagnostics stay on stderr (--quiet drops only
        # cargo's status lines) — the trailer must not claim more than
        # it knows: EACCES over a kache restore, a gc'd cargo, or a
        # plain type error all land here too, and "regen sqlx" fixes
        # none of them.
        # shellcheck disable=SC2086
        SQLX_OFFLINE=true cargo check --quiet --all-targets $args \
          || { echo 'sqlx-prepare-check failed — if the errors above say `no cached data for query`, run `cargo xtask regen sqlx`; otherwise fix the reported error'; exit 1; }
      ''
    );
    files = "(\\.rs$|^\\.sqlx/)";
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
  # the ~10s regeneration cost. The hermetic backstops are
  # `checks.<system>.crate2nix-drift` for the ROOT Cargo.json and
  # `crate2nix-drift-fuzz-{rio-nix,rio-store}` for the fuzz ones
  # (all nix/misc-checks.nix) — the fuzz derivations never read
  # the fuzz Cargo.locks, so a stale json builds the old graph
  # silently; only a newly-added dep fails loudly.
  crate2nix-check = {
    enable = true;
    name = "crate2nix-check";
    entry = toString (
      pkgs.writeShellScript "crate2nix-check" ''
        set -euo pipefail
        ${cargoEnvGuard}
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
        ${cargoEnvGuard}
        if ! git diff --cached --name-only \
           | grep -qE '(^|/)Cargo\.(toml|lock)$'; then
          exit 0
        fi
        # No stderr redirect: hakari verify's diff (and any environment
        # failure, e.g. a dead cargo) must reach the committer — a
        # swallowed cause with a confident "regen hakari" trailer sends
        # people down the wrong path.
        ${pkgs.cargo-hakari}/bin/cargo-hakari hakari verify || {
          echo 'error: workspace-hack is stale — run `cargo xtask regen hakari` (if the output above shows a different failure, fix that instead)'
          exit 1
        }
      ''
    );
    files = "(^|/)Cargo\\.(toml|lock)$";
    language = "system";
    pass_filenames = false;
  };
}
