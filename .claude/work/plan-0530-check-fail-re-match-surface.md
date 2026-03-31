# Plan 530: `_CHECK_FAIL_RE` — known-flakes match surface for checks.* drvs

P0490 validator confirmed: T2's known-flake entry is DEAD WEIGHT. `drv_name:null` excluded from `by_drv` (build.py:254-256). `test:` key matches only nextest FAILs. `_VM_FAIL_RE` only captures `vm-test-run-*`. **No match surface for checks.* drvs** — tracey-validate, clippy, docs, coverage, fuzz, cov-smoke all unexcusable-by-known-flake today.

A tracey-validate failure produces `error: Cannot build '/nix/store/HASH-rio-tracey-validate.drv'`. `_VM_FAIL_RE` requires `vm-test-run-` between hash and name; this has `rio-` directly after hash. Disjoint.

## Tasks

### T1 — `fix(harness):` add `_CHECK_FAIL_RE` + wire into excusable()

MODIFY `.claude/lib/onibus/build.py`. After `_VM_FAIL_RE` (~:153):

```python
# Non-VM checks.* constituent failures. Same ^error: anchor + Cannot build
# shape as _VM_FAIL_RE, but the drv name is rio-<check> directly after the
# hash (no vm-test-run- prefix). Captures the full rio-<name> for consistency
# with _VM_FAIL_RE's capture of the full vm-test-run-<scenario> suffix.
#
# Disjoint from _VM_FAIL_RE: [a-z0-9]+-rio- requires rio- IMMEDIATELY after
# the hash. vm-test-run drvs have -vm-test-run- between, so they don't match
# here. And _VM_FAIL_RE's -vm-test-run- prefix excludes bare rio-<check>.
# P0490 canary: first non-VM flake entry ever attempted, discovered surface missing.
_CHECK_FAIL_RE = re.compile(
    r"^error: Cannot build '/nix/store/[a-z0-9]+-(rio-[\w-]+)\.drv'",
    re.MULTILINE,
)
```

Then at ~:228 alongside `vm_fails`:
```python
check_fails = sorted(set(_CHECK_FAIL_RE.findall(text)))  # non-VM checks.* drv names
failing = nextest_fails + vm_fails + check_fails
```

And at ~:260 in the `matched` union:
```python
| set(d for d in check_fails if d in by_drv)
```

### T2 — `fix(flake-registry):` make P0490's entry live

MODIFY `.claude/known-flakes.jsonl:15`. `"drv_name":null` → `"drv_name":"rio-tracey-validate"`. That's the capture value from `_CHECK_FAIL_RE`.

### T3 — `test(harness):` excusable recognizes checks.* drv via known-flake

NEW test in `.claude/lib/test_scripts.py` near the existing `excusable` tests. Synthetic log:

```python
log = "error: Cannot build '/nix/store/abc123xyz-rio-tracey-validate.drv'\n       Reason: builder failed with exit code 1."
```

Assert: `excusable()` returns True with `matched_flakes=["rio-tracey-validate"]` when the known-flake entry has `drv_name:"rio-tracey-validate"`, `retry:"Once"`.

**Negative case**: VM drv log + check-name entry → no match (proves disjoint). Log with `vm-test-run-rio-observability.drv` + flake entry `drv_name:"rio-tracey-validate"` → NOT excusable.

## Exit criteria

- `/nixbuild .#ci` green
- `grep '_CHECK_FAIL_RE' .claude/lib/onibus/build.py` → ≥2 hits (def + findall)
- `jq -r '.drv_name' .claude/known-flakes.jsonl | grep -c rio-tracey-validate` → 1
- `pytest .claude/lib/test_scripts.py -k check_fail_re` passes (positive + negative)
- Disjoint proof: `_CHECK_FAIL_RE.findall("...-vm-test-run-rio-foo.drv...")` → `[]` (test assertion)

## Tracey

No domain markers — harness tooling.

## Files

```json files
[
  {"path": ".claude/lib/onibus/build.py", "action": "MODIFY", "note": "T1: _CHECK_FAIL_RE regex after :153; check_fails extraction at :228; matched union at :260"},
  {"path": ".claude/known-flakes.jsonl", "action": "MODIFY", "note": "T2: :15 drv_name null→rio-tracey-validate"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T3: positive (tracey-validate matches) + negative (vm-test-run disjoint)"}
]
```

## Dependencies

```json deps
{"deps": [490], "note": "P0490 landed the dead T2 entry + the in-drv retry. This makes T2 live. P0527 (by_drv defaultdict) already handles the dict side correctly."}
```

## Risks

- **P0448 in parallel** touches `.claude/lib/test_scripts.py` (+85L tests). DIFFERENT section (collisions tests ~:3398). No collision expected; rebase around whichever lands first.
- Regex care: `[a-z0-9]+-(rio-[\w-]+)` — the `[\w-]+` is greedy. A drv named `rio-foo-bar-baz` captures `rio-foo-bar-baz`. Correct. But `rio-cov-vm-total` (aggregate) would ALSO match — verify excusable is only consulted on FAILING drvs, so aggregate-cascade doesn't re-introduce P0517's over-count in a different surface.
