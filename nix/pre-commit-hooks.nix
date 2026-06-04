# Custom pre-commit hooks (the writeShellScript ones — standard
# `*.enable = true` toggles stay inline in flake.nix).
#
# Returned attrset is merged into pre-commit.settings.hooks.
{
  pkgs,
  crate2nixCli,
}:
let
  inherit (pkgs) lib;

  # Local crates whose RESOLVED dependency graph includes sqlx —
  # membership for sqlx-prepare-check's REFUSE arm, derived from the
  # committed Cargo.json instead of the previous [dependencies]-section
  # awk over Cargo.toml. The awk was form-sensitive: blind to dotted
  # tables (`[dependencies.sqlx]` + `workspace = true` fails its
  # `/^\[dependencies\]/` header match) and to
  # `[target."cfg(...)".dependencies]` — both of which crate2nix folds
  # into the resolved `dependencies` array, so this list sees them by
  # construction. devDependencies are deliberately excluded (test-only
  # query! in a dev-dep-only crate skips, as before); `optional` deps
  # are included — conservative-correct: a feature-gated query! still
  # needs the tracker. 2026-06 census: rio-builder rio-controller
  # rio-migrations rio-scheduler rio-store rio-test-support xtask.
  resolvedCargoJson = builtins.fromJSON (builtins.readFile ../Cargo.json);
  sqlxDepCrates = lib.naturalSort (
    lib.unique (
      map (c: c.crateName) (
        lib.filter (
          c: (c.source.type or "") == "local" && lib.any (d: (d.name or "") == "sqlx") (c.dependencies or [ ])
        ) (lib.attrValues resolvedCargoJson.crates)
      )
    )
  );
  # dir → crateName map for sqlx-prepare-check, baked from the same
  # committed Cargo.json. owning_crate() yields a DIRECTORY while
  # sqlxDepCrates above holds crate NAMES (and cargo -p takes the
  # package name) — today the two coincide because every local crate
  # has source.path == crateName, but that is convention, not
  # contract: a nested crate (a/b/foo) or a dir≠name crate would make
  # a dir-vs-name comparison silently fail open. Every local crate
  # record carries source.path, so the real mapping is derivable;
  # rendered as a case function (sorted for a stable store path —
  # dirs/names are [A-Za-z0-9_./-], no shell-quoting hazards).
  localDirToName = lib.sort (a: b: a.dir < b.dir) (
    map (c: {
      dir = c.source.path;
      name = c.crateName;
    }) (lib.filter (c: (c.source.type or "") == "local") (lib.attrValues resolvedCargoJson.crates))
  );
  crateForDirFn = ''
    crate_for_dir() {
      case "$1" in
        ${lib.concatMapStringsSep "\n    " (
          e: "'${e.dir}') printf '%s\\n' '${e.name}' ;;"
        ) localDirToName}
        *) return 1 ;;
      esac
    }
  '';
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
        # `//`-to-EOL is stripped BEFORE matching, so both full-line
        # and trailing inline comments (`foo(); // see sqlx::query!`)
        # are invisible — the latter would otherwise refuse spuriously
        # in a tracker-less sqlx crate.
        #
        # Residuals, accepted, by FAIL DIRECTION:
        # - fail OPEN (skip, never block): a `//` inside a string
        #   literal on a real callsite line hides that line; a future
        #   UNqualified `query!(` behind a use-import under-triggers;
        #   and the sqlxDepCrates membership list below is BAKED into
        #   this store script at config eval — a crate gaining sqlx +
        #   its first query! in the same commit passes vacuously until
        #   a devshell re-entry relinks the config. CI clippy/nextest
        #   remains the staleness backstop for all of these (this hook
        #   is dev-ergonomics, per the header above).
        # - fail CLOSED (block, remediation in the REFUSE message): a
        #   block comment or string literal shaped like `sqlx::query…!`
        #   in a crate that IS in sqlxDepCrates but wires no tracker —
        #   the //-strip cannot see block comments or strings, so the
        #   REFUSE arm fires on prose there. (In tracker-WIRED crates
        #   the same shape just costs a vacuous cargo check; in crates
        #   without the sqlx dep it skips silently.)
        real_callsite() {
          sed 's@//.*@@' -- "$1" 2>/dev/null \
            | grep -Eq 'sqlx::query[a-z_]*!'
        }
        # Tracker wiring = a CALL-shaped `track_sqlx(` with //-comments
        # stripped — same discipline as real_callsite, so a doc-comment
        # mention of track_sqlx() in a build.rs cannot count as wiring
        # (the bare-substring grep this replaces would have). SHARED
        # call-shape pattern `track_sqlx[[:space:]]*\(` — keep in
        # lockstep with hasTrackerCall in nix/crate2nix.nix (the eval
        # side of the same scan): both tolerate whitespace before the
        # paren, so `track_sqlx ();` counts as wired at commit AND at
        # eval. The tracker⟺consumer pairing itself (build.rs call ⟺
        # lib.rs/main.rs env!("RIO_SQLX_HASH") read) is enforced at
        # EVAL by nix/crate2nix.nix's pairing asserts on every flake
        # eval — not here, where it only ran on query!-touching
        # commits.
        #
        # Build-script path: every arm below assumes the conventional
        # `<dir>/build.rs` (the implicit cargo default; all local
        # crates today — none sets `[package] build`). The nix-side
        # scan consults the explicit `build` field Cargo.json records
        # when a manifest sets one (hasTrackerCall's caller in
        # nix/crate2nix.nix); here the assumption fails by direction:
        # a local crate with a custom build path fails CLOSED in the
        # CHECK/REFUSE arms (no build.rs found → REFUSE if it is in
        # sqlxDepCrates, with this message in hand) and OPEN in the
        # widen arm (`git ls-files '*build.rs'` misses it; CI
        # clippy/nextest backstops).
        tracker_wired() {
          sed 's@//.*@@' -- "$1" 2>/dev/null \
            | grep -q 'track_sqlx[[:space:]]*('
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
        # dir → crateName, baked from Cargo.json local crates'
        # {source.path → crateName} (see localDirToName in this file's
        # let block). Returns 1 for a dir with no committed Cargo.json
        # entry.
        ${crateForDirFn}
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
        # --no-renames: under rename detection a `git mv` OUT of
        # .sqlx/ lists only the destination, so the widen would miss
        # exactly the cache-entry-removal it exists for; --no-renames
        # decomposes the rename to D+A and the deleted .sqlx/ path
        # matches. (-q is safe here — this script sets no pipefail, so
        # the pipeline takes grep's status.)
        if git diff --cached --no-renames --name-only | grep -q '^\.sqlx/'; then
          crates=$(
            {
              printf '%s\n' "$crates"
              git ls-files '*build.rs' \
                | while IFS= read -r b; do
                    tracker_wired "$b" && dirname -- "$b"
                  done
            } | sort -u | grep -v '^$'
          )
        fi
        [ -n "$crates" ] || exit 0
        args=""
        for c in $crates; do
          [ -f "$c/Cargo.toml" ] || continue
          # Map the directory to its package name. Unmapped dir =
          # crate added without `cargo xtask regen cargo-json` (or a
          # config relink lag) — fail OPEN with a one-line note, same
          # direction as the baked-census residual documented above;
          # CI clippy/nextest is the staleness backstop.
          name=$(crate_for_dir "$c") || {
            echo "sqlx-prepare-check: $c has no entry in the committed Cargo.json (crate added without \`cargo xtask regen cargo-json\`?) — skipping; CI clippy/nextest is the staleness backstop" >&2
            continue
          }
          if [ -f "$c/build.rs" ] && tracker_wired "$c/build.rs"; then
            # CHECK arm. (The consumer half of the tracker contract —
            # the lib.rs/main.rs env!("RIO_SQLX_HASH") read — is
            # asserted at eval by nix/crate2nix.nix, not re-checked
            # here.) -p takes the package name, which IS the mapped
            # crateName from Cargo.json — the directory name is
            # convention, not contract.
            args="$args -p $name"
          else
            # REFUSE arm: real callsite shape + sqlx in the crate's
            # RESOLVED [dependencies] (sqlxDepCrates, baked from
            # Cargo.json at config eval — see the membership comment in
            # this file's let block) but no tracker — kache would
            # replay compiles against an unobserved .sqlx. dev-only
            # sqlx consumers are not in the list and skip instead of
            # refuse, as before. Membership tests the mapped crateName
            # against the crateName list — type-aligned by
            # construction (both sides come from Cargo.json).
            case " ${toString sqlxDepCrates} " in
              *" $name "*)
                echo "sqlx-prepare-check: $c (crate $name) has sqlx in its resolved [dependencies] (Cargo.json) and a real query! callsite shape but no track_sqlx() in build.rs — kache would replay compiles against an unobserved .sqlx; wire rio-buildhash::track_sqlx() (see CLAUDE.md, out-of-band macro inputs). If the only match is inside a block comment or string literal (the //-strip can't see those), break the \`sqlx::query\` token — e.g. split the string — or wire the tracker anyway" >&2
                exit 1
                ;;
            esac
            # No sqlx in the resolved runtime deps: callsite-shaped
            # text in a tool crate (string literal / block comment) —
            # cannot be a compiled query!, skip silently.
          fi
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
    # The framework's staged-file list uses --diff-filter=ACMRTUXB
    # ("everything except D"), so a deletion-ONLY .sqlx commit matches
    # `files` yet yields zero filenames and the hook is skipped before
    # its own deletion-aware `git diff --cached` widen arm can run — a
    # deleted cache entry still referenced by a query! is exactly the
    # staleness this hook pre-empts. always_run fires it on every
    # commit instead; the trigger derivation above exits 0 in
    # milliseconds when nothing sqlx-shaped is staged. (`files` is kept
    # as documentation of the trigger surface.)
    always_run = true;
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
  # `crate2nix-drift-fuzz-<ws>` for the fuzz ones (all
  # nix/misc-checks.nix, derived from the eval-time fuzzWorkspaces
  # list) — the fuzz derivations never read
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
        # that don't touch the dep graph. The UNFILTERED
        # `git diff --cached` here sees deletions too — it is
        # always_run below that stops the framework's
        # deletion-blind staged list from skipping the hook
        # before this gate runs.
        #
        # SIGPIPE-safe shape (the "stdenv pipefail SIGPIPE" class in
        # .claude/rules/ci-failure-patterns.md): `git diff | grep -q`
        # under pipefail lets grep's first-match exit SIGPIPE git →
        # pipeline status 141 → the `!` inverts it → the hook silently
        # SKIPS on exactly a commit that staged a manifest (needs a
        # >pipe-buffer staged list, so rare — but fail-open). Capture
        # to a var first; grep then owns the status alone.
        # --no-renames: under rename detection a renamed Cargo.lock
        # lists only its destination, hiding the deletion-shaped half
        # from the pattern.
        staged=$(git diff --cached --no-renames --name-only)
        if ! grep -qE '(^|/)Cargo\.(toml|lock)$' <<<"$staged"; then
          exit 0
        fi
        tmp=$(mktemp -d)
        # Single EXIT-trap convergence, registered BEFORE the first
        # backup is taken: restore_all() copies every backed-up
        # Cargo.lock back, removes every partial Cargo.json.check,
        # then removes $tmp. Success, drift exit 1, failed-generate
        # exit 1, set -e aborts, and SIGINT/SIGTERM (bash runs the
        # EXIT trap on fatal-signal termination) all converge here —
        # no path can delete the only backup before restoring it.
        # Residual: SIGKILL/OOM-kill strands the mutated lock —
        # unavoidable for any in-shell scheme.
        n=0
        restore_all() {
          i=0
          while [ "$i" -lt "$n" ]; do
            dest=$(cat "$tmp/$i.dest")
            cp "$tmp/$i.lock" "$dest/Cargo.lock"
            rm -f "$dest/Cargo.json.check"
            i=$((i + 1))
          done
          rm -rf "$tmp"
        }
        trap restore_all EXIT
        # Workspaces: root + the fuzz subworkspaces (each has its
        # own Cargo.lock + Cargo.json consumed by nix/fuzz.nix),
        # discovered at RUN TIME by the glob — a brand-new fuzz
        # workspace's first commit is gated here without waiting for
        # a devshell config relink (the previous eval-baked list was
        # addition-blind); the -f guard covers the removal direction
        # and bash's unmatched-glob-stays-literal default falls
        # through it harmlessly. The hermetic crate2nix-drift-fuzz-<ws>
        # backstop stays eval-derived from filesets.nix fuzzWorkspaces
        # — fine, flake eval requires the json tracked anyway. The
        # asymmetry with sqlxDepCrates (still eval-baked above) is
        # principled: a lock list is filesystem-derivable at hook
        # runtime; a resolved-dependency census requires parsing
        # Cargo.json at eval and cannot be derived by the shell.
        # crate2nix emits path fields relative
        # to the output file's directory, so -o $tmp/... would
        # produce ../../root/... paths that never match — generate
        # in-place under .check, diff, then clean up (deferred to the
        # EXIT trap).
        for dir in . fuzz/*/; do
          dir=''${dir%/}
          [ -f "$dir/Cargo.lock" ] || continue
          # Snapshot Cargo.lock — `cargo metadata` inside
          # crate2nix can bump transitive deps if the local
          # cache is cold. Per-dir backup + dest record; every
          # restore happens in restore_all at EXIT (the locks are
          # independent files and the drift diff below reads only
          # Cargo.json[.check], never a lock, so deferring is sound).
          cp "$dir/Cargo.lock" "$tmp/$n.lock"
          printf '%s\n' "$dir" > "$tmp/$n.dest"
          n=$((n + 1))
          if ! ( cd "$dir" && ${crate2nixCli}/bin/crate2nix generate --format json -o Cargo.json.check ); then
            exit 1
          fi
          # Newline-terminate ONLY if crate2nix didn't — mirrors
          # xtask/src/regen/cargo_json.rs's conditional append, so
          # a future crate2nix that emits its own trailing newline
          # can't double-append on the check side and spuriously
          # report drift.
          [ -z "$(tail -c1 "$dir/Cargo.json.check")" ] || echo >> "$dir/Cargo.json.check"
          if ! diff -q "$dir/Cargo.json" "$dir/Cargo.json.check" >/dev/null; then
            echo "error: $dir/Cargo.json is stale — run \`cargo xtask regen cargo-json\`"
            exit 1
          fi
        done
      ''
    );
    files = "(^|/)Cargo\\.(toml|lock)$";
    # Framework gap (same as sqlx-prepare-check): the staged-file list
    # is --diff-filter=ACMRTUXB, no deletions — and a deletion-only
    # Cargo.{toml,lock} commit is reachable here (fuzz workspaces live
    # outside the root workspace; removing one deletes its lock
    # without touching the root lock). The inner unfiltered git-diff
    # gate above is deletion-aware and fast-exits for everything else.
    always_run = true;
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
        # Same SIGPIPE-safe gate shape as crate2nix-check (pipefail is
        # on here too — see the class comment there), and --no-renames
        # for the same rename-decomposition reason.
        staged=$(git diff --cached --no-renames --name-only)
        if ! grep -qE '(^|/)Cargo\.(toml|lock)$' <<<"$staged"; then
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
    # Framework gap (same shape as crate2nix-check): a deletion-only
    # Cargo.{toml,lock} commit is invisible to the framework's
    # --diff-filter=ACMRTUXB staged list; the inner unfiltered
    # git-diff gate is deletion-aware and fast-exits otherwise.
    always_run = true;
    language = "system";
    pass_filenames = false;
  };
}
