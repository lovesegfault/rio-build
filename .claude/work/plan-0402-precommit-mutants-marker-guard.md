# Plan 0402: pre-commit guard — reject cargo-mutants dirty markers

rev-p373 correctness finding. The p373 worktree had a dirty cargo-mutants artifact at [`rio-nix/src/protocol/wire/mod.rs:158`](../../rio-nix/src/protocol/wire/mod.rs) — `if pad == /* ~ changed by cargo-mutants ~ */ 0 {` in the WORKING TREE (committed HEAD was correct `if pad > 0`). The merger ff-forwards committed state so this didn't ship. But the workflow trap is real: `just mutants` crashes mid-run → mutated source survives in the worktree → next blind commit ships a mutant.

No pre-commit guard exists for this. The marker `changed by cargo-mutants` is a reliable detection string (cargo-mutants always leaves it). Same tier as the existing `treefmt`/`check-merge-conflicts` hooks — 5-line grep, fails fast.

## Entry criteria

- [P0373](plan-0373-cargo-mutants-triage-wire-aterm-hmac.md) merged (cargo-mutants infrastructure exists; `just mutants` is a thing) — **DONE**

## Tasks

### T1 — `fix(nix):` pre-commit hook — reject cargo-mutants markers

MODIFY [`flake.nix`](../../flake.nix) at `:1230` (`settings.hooks = {`) — add after `check-merge-conflicts` (same deny-marker tier):

```nix
# Reject commits containing cargo-mutants dirty markers.
# `just mutants` mutates source in-place; if it crashes or is
# interrupted, the mutated line (with `/* ~ changed by cargo-mutants
# ~ */`) survives in the worktree. A blind commit then ships a
# mutant. The marker string is reliable — cargo-mutants always
# wraps its mutations with it.
check-mutants-marker = {
  enable = true;
  name = "check-mutants-marker";
  entry = toString (
    pkgs.writeShellScript "check-mutants-marker" ''
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
```

Matches the `sqlx-prepare-check` shape at `:1251-1270` (shell-script entry, `.rs` files-filter, `pass_filenames = false` so the script does its own git-diff-cached scan).

### T2 — `test(nix):` hook fires on marker, passes on clean

MODIFY [`nix/tests/scenarios/pre-commit.nix`](../../nix/tests/scenarios/pre-commit.nix) (or wherever pre-commit hooks are integration-tested — grep `sqlx-prepare-check` in `nix/tests/` at dispatch to find the right file; if none, this is a new scenario under `nix/tests/scenarios/`).

Positive + negative:

```nix
# Marker in staged file → hook rejects
machine.succeed("cd /repo && echo 'let x = /* ~ changed by cargo-mutants ~ */ 1;' > rio-nix/src/foo.rs")
machine.succeed("cd /repo && git add rio-nix/src/foo.rs")
machine.fail("cd /repo && pre-commit run check-mutants-marker --all-files")

# Marker removed → hook passes
machine.succeed("cd /repo && git checkout -- rio-nix/src/foo.rs")
machine.succeed("cd /repo && pre-commit run check-mutants-marker --all-files")
```

If no existing pre-commit integration test scenario exists, T2 simplifies to a one-shot `justfile` target:

```make
# justfile
check-mutants-marker-test:
    @echo 'let x = /* ~ changed by cargo-mutants ~ */ 1;' > /tmp/mutants-test.rs
    @! grep -q 'changed by cargo-mutants' /tmp/mutants-test.rs && exit 1 || true
    @rm /tmp/mutants-test.rs
    @echo "check-mutants-marker detection works"
```

Prefer the VM test if precedent exists; otherwise, the hook's own definition IS the test (git-hooks.nix validates the script compiles; first dev to run `just mutants` + interrupt + commit proves it live).

## Exit criteria

- `/nbr .#ci` green
- `grep 'check-mutants-marker\|changed by cargo-mutants' flake.nix` → ≥2 hits (T1: hook name + marker string)
- `nix develop -c pre-commit run check-mutants-marker --all-files` → exit 0 (clean repo passes)
- T2 positive-negative pair passes (if VM scenario route taken)

## Tracey

No tracey markers — dev-ergonomics guard, not spec'd behavior.

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "T1: +check-mutants-marker hook at :1230 settings.hooks (after check-merge-conflicts, same tier as sqlx-prepare-check pattern)"}
]
```

```
flake.nix   # T1: +check-mutants-marker hook
```

## Dependencies

```json deps
{"deps": [373], "soft_deps": [], "note": "rev-p373 correctness (workflow trap — cargo-mutants crash leaves dirty marker, blind commit ships mutant). p373 worktree HAD the dirty state (wire/mod.rs:158); merger ff'd committed HEAD so didn't ship. 5-line deny-marker grep, sqlx-prepare-check shape. flake.nix count=37 HOT but hook is one attrset add at :1230, additive."}
```

**Depends on:** [P0373](plan-0373-cargo-mutants-triage-wire-aterm-hmac.md) — cargo-mutants infra exists (DONE).
**Conflicts with:** [`flake.nix`](../../flake.nix) count=37 HOT — T1 adds one hook attrset at `:1230`. P0304-T74 edits the mutants `buildPhaseCargoCommand` at `:807-814`, P0304-T76 edits `goldenTestEnv` at a different block — all non-overlapping with `:1230`. P0311-T51 edits helm-lint yq block — different section. Any plan touching `settings.hooks` serializes, but additive attrsets auto-merge clean.
