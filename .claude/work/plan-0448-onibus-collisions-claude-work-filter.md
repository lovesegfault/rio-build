# Plan 0448: onibus collisions check misses .claude/work/ overlaps — filter or bug?

Coordinator-filed finding at [`.claude/lib/onibus/collisions.py`](../../.claude/lib/onibus/collisions.py). `onibus collisions check 295` did NOT report the overlap between P0295 and P0437 on [`.claude/work/plan-0304-trivial-batch-p0222-harness.md`](../../.claude/work/plan-0304-trivial-batch-p0222-harness.md). Caught manually via `git diff`. Low-impact (caught manually) but undermines the collision-safety guarantee the coordinator relies on.

**Two hypotheses:**

1. **Intentional filter:** `.claude/work/plan-*.md` files may be deliberately excluded from collision checking — batch-append plans (P0295/P0304/P0311) are DESIGNED to be edited by many plans concurrently, and reporting every batch-append as a collision would be noise. If so, the filter needs documenting.
2. **Bug introduced by P0306:** [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md)'s new `check_vs_running` verify-phase filter may have a path-subset bug that accidentally excludes `.claude/work/` paths.

This plan is investigation-first: determine which hypothesis is correct, then either document (hypothesis 1) or fix (hypothesis 2).

## Entry criteria

- [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) merged (introduced `check_vs_running` — the suspected regression source)

## Tasks

### T1 — `fix(harness):` collisions.py — investigate .claude/work/ filter behavior

Read [`collisions.py`](../../.claude/lib/onibus/collisions.py) and trace the path-filter logic. Determine:

- Is there an explicit `.claude/work/` exclusion? (grep for `work/` or `plan-` in filter predicates)
- Did P0306's `check_vs_running` change the filter semantics?
- Reproduce: `onibus collisions check 295` with P0437's files-fence including `plan-0304-*.md` — does it report?

**If intentional filter:** add a doc-comment at the filter site explaining WHY `.claude/work/` is excluded (batch-append noise suppression). Add a note to the `/dag-run` skill doc that batch-doc collisions are NOT caught by `collisions check` and must be eyeballed.

**If bug:** fix the filter to include `.claude/work/plan-*.md` paths. The noise concern can be addressed by only flagging when BOTH plans target the SAME batch-doc T-range (harder) or by accepting the noise (simpler — coordinator can ignore expected batch-append collisions).

### T2 — `test(harness):` collisions check covers .claude/work/ paths

Add to [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py): `test_collisions_check_includes_claude_work` — two mock plan files-fences both listing `.claude/work/plan-0304-*.md`, assert `collisions check` reports the overlap (or, if the filter is intentional, assert it does NOT report and the doc-comment exists).

## Exit criteria

- `/nbr .#ci` green
- Investigation conclusion documented in this plan doc's `## Outcome` section (added at impl time)
- Either: `grep 'claude/work.*excluded\|batch-append noise' .claude/lib/onibus/collisions.py` → ≥1 hit (documented filter), OR `onibus collisions check <N>` reports `.claude/work/` overlaps (bug fixed)
- `pytest .claude/lib/test_scripts.py -k collisions_check_includes` → passed

## Tracey

No tracey marker — harness tooling.

## Files

```json files
[
  {"path": ".claude/lib/onibus/collisions.py", "action": "MODIFY", "note": "T1: either doc-comment the filter OR fix path-inclusion for .claude/work/"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T2: +test_collisions_check_includes_claude_work"}
]
```

```
.claude/lib/
├── onibus/
│   └── collisions.py      # T1: filter doc or fix
└── test_scripts.py        # T2: regression test
```

## Dependencies

```json deps
{"deps": [306], "soft_deps": [], "note": "P0306 introduced check_vs_running — suspected regression source. Investigation-first plan."}
```

**Depends on:** [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) — the `check_vs_running` filter change.
**Conflicts with:** `collisions.py` is low-traffic. `test_scripts.py` shared with [P0446](plan-0446-merge-agent-start-path-padding.md) and [P0447](plan-0447-onibus-flake-excusable-nixbuild-patterns.md) — all append-only test additions, rebase-clean.

## Outcome

**BUG (hypothesis 2 — but predates P0306).** Not an intentional filter.

`check_vs_running` builds an asymmetric comparison:
- **this-side** (`this_files`, line 142): plan fence → `PlanFile.model_validate` → pattern accepts `.claude/`, `nix/`, `docs/`, `migrations/`, `rio-*/tests/`, etc.
- **their-side** (`their`, line 156): `diff_src_files(wt)` → `^rio-[a-z-]+/src/.*\.rs$` filter → rust-src ONLY

Intersection `this_set & their` is structurally incapable of containing non-rust-src paths. P0295's fence lists `.claude/work/plan-0304-*.md`; P0437's worktree diff includes it; `diff_src_files` drops it from `their`; collision never reported.

The `^rio-*/src/*.rs$` filter has been in `diff_src_files` since `_lib.py` (pre-onibus, before 504dad2b). P0306 (32e4239d) only added `_verify_phase_plans()` frozen-role skip — red herring.

**Fix:** renamed `diff_src_files` → `diff_files`, dropped the filter. `this_set` is already `PlanFile`-scoped (fence) or rust-src-scoped (grep fallback), so the intersection does the scoping — no their-side filter needed. Bonus: also catches previously-missed collisions on `nix/tests/`, `docs/src/`, `migrations/`, `rio-*/tests/`.

Noise concern: batch-append docs (P0304/P0311) will now show as colliding when multiple in-flight plans target them. Accepted — coordinator knows to ignore expected batch-append overlaps. T-range disambiguation deferred.
