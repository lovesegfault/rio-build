# Plan 534: `_CHECK_FAIL_RE` aggregate filter — restore known-flake saves for VM cascades

P0530 REGRESSION. `_CHECK_FAIL_RE` captures `rio-ci` (the aggregate drv) when ANY constituent fails and cascades. Before P0530: `vm-test-run-rio-observability` fails → `rio-ci` cascade-fails → nothing extracts `rio-ci` → `failing=[rio-observability]` len=1 → known-flake saves. After P0530: `_CHECK_FAIL_RE` extracts `rio-ci` → `failing=[rio-observability, rio-ci]` len=2 → build.py:300 rejects before by_drv → **known-flake doesn't save you anymore.**

**Discovered by P0532 implementer:** CI iter1 hit `rio-observability` known-flake (P0509's entry), `onibus flake excusable` returned `excusable: false` citing "2 failures." Retry worked anyway, but the machinery was BROKEN. P0509's known-flake saved P0445 (pre-P0530); did NOT save P0532 (post-P0530).

P0530's validator noted the `len>1` rejection path — but framed it as SAFETY ("no false excuse of the aggregate"). It's ALSO a regression: the rejection blocks TRUE excuse of the REAL fail.

The bughunter mc=91 showed build.py:306's `in vm_fails` gate is DEAD (`_TCG_MARKERS=()`). So :300's `len>1` is the ONLY rejection, and it fires on aggregate cascades now.

## Tasks

### T1 — `fix(harness):` filter aggregate drvs from check_fails

MODIFY `.claude/lib/onibus/build.py`. After `_CHECK_FAIL_RE` definition (~:176), add:

```python
# Aggregate drvs (linkFarm/symlinkJoin targets that cascade-fail when
# any constituent fails). These are NEVER independent evidence — they
# fail BECAUSE a dependency failed. P0534: P0530's _CHECK_FAIL_RE
# matches rio-ci, which puts it in check_fails → len(failing)>1 at
# :300 rejects → known-flake can't save. Before P0530 nothing extracted
# rio-ci; len=1; flake saved. P0532 hit this (iter1 excusable=false).
_AGGREGATE_DRVS = frozenset({"rio-ci", "rio-cov-vm-total", "rio-coverage-full"})
```

Then at ~:252 where `check_fails` is extracted:

```python
check_fails = sorted(set(_CHECK_FAIL_RE.findall(text)) - _AGGREGATE_DRVS)
```

This is a set-subtract before sort. Aggregates are filtered at extraction time, before `failing` is built, before :300's len-check sees them.

### T2 — `test(harness):` P0532's scenario — VM flake + aggregate cascade → saves

MODIFY `.claude/lib/test_scripts.py`. New test case near P0530's `test_check_fail_re_matches_non_vm_drv`. Synthetic log mimicking P0532 iter1:

```python
log = (
    "error: Cannot build '/nix/store/abc-vm-test-run-rio-observability.drv'\n"
    "       Reason: builder failed with exit code 1.\n"
    "error: Cannot build '/nix/store/def-rio-ci.drv'\n"
    "       Reason: 1 dependency failed.\n"
)
```

With known-flake entry `drv_name:"rio-observability"` `retry:"Once"`. Assert: `excusable()` returns `True`, `matched_flakes=["rio-observability"]`, `len(failing)==1` (rio-ci filtered).

**Negative case:** aggregate as the ONLY fail (impossible in practice but proves the filter). `rio-ci` alone → `check_fails=[]` after filter → `failing=[]` → no-fails branch (whatever that does — verify it doesn't falsely excuse).

**Mutation-check exit criterion:** delete `"rio-ci"` from `_AGGREGATE_DRVS` → test FAILS with `excusable=False` (len>1 rejection, pre-P0534 behavior restored).

## Exit criteria

- `/nixbuild .#ci` green
- `grep '_AGGREGATE_DRVS' .claude/lib/onibus/build.py` → ≥2 hits (frozenset def + subtract)
- `pytest .claude/lib/test_scripts.py -k aggregate` passes (positive + negative)
- Mutation: delete `"rio-ci"` from frozenset → test FAILS (proves the filter is load-bearing)
- P0530's existing `test_check_fail_re_matches_non_vm_drv` still passes (tracey-validate isn't an aggregate, not filtered)

## Tracey

No domain markers — harness tooling.

## Files

```json files
[
  {"path": ".claude/lib/onibus/build.py", "action": "MODIFY", "note": "T1: _AGGREGATE_DRVS frozenset after :176; set-subtract at check_fails extraction ~:252"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T2: VM-flake+aggregate-cascade positive (P0532 scenario); aggregate-only negative; mutation-check"}
]
```

## Dependencies

```json deps
{"deps": [530], "note": "P0530 introduced the regression. P0532 discovered it. Fix restores pre-P0530 known-flake saves for VM cascades while KEEPING P0530's actual goal (non-VM checks.* drvs like rio-tracey-validate are still extractable)."}
```

## Risks

- **Aggregate enumeration completeness** — are there OTHER aggregate drvs? `rio-ci` is the main one. `rio-cov-vm-total` and `rio-coverage-full` are coverage-side aggregates. Check: `grep -E 'linkFarm|symlinkJoin|buildEnv' flake.nix nix/` for other cascade-prone derivations. Missing one means the regression persists for that aggregate's cascades.
- **P0532 in parallel** touches `.claude/lib/test_scripts.py` (~:1693 P0530 tests). P0534-T2 lands near the same section. Diff-section but close. Rebase around whichever merges first.
- **P0530's :169 comment is now stale** — it says "excusable() is consulted only on .rc != 0 so the aggregate won't be falsely excused" — true but misses that it ALSO isn't TRULY excused. T1 should update that comment to reference `_AGGREGATE_DRVS` as the filter mechanism.
