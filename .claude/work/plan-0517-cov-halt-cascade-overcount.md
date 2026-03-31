# Plan 517: coverage-halt heuristic over-counts derivation cascade from ONE scenario

**Supersedes [P0304-T518](plan-0304-trivial-batch-p0222-harness.md).** That batch item was filed after mc=56 (observability SIGTERM, ONE halt). Since then two MORE false-positive halts fired: mc=51 (fetcher-split SIGTERM) and mc=67 (lifecycle-core timing). Three halts in one session — every one the same structural bug. Batch-trivial was wrong triage: this disrupts the coordinator's run loop every time it fires, and it fires on every single coverage-mode scenario fail.

The bug: `_COV_SCENARIO_FAIL_RE` at [`merge.py:528-532`](../../.claude/lib/onibus/merge.py) matches both `vm-test-run-<X>.drv` and `rio-cov-<X>.drv`. When ONE coverage-mode VM test fails, nix cascades the failure through its dependents: the per-test lcov wrapper (`rio-cov-vm-<scenario>-<fixture>`, from [`coverage.nix:76`](../../nix/coverage.nix) `mkPerTestLcov`) and the aggregate (`rio-cov-vm-total`, from [`coverage.nix:125`](../../nix/coverage.nix)). Three `Cannot build` lines → three distinct regex captures → `set()` dedup at [`merge.py:549`](../../.claude/lib/onibus/merge.py) sees three distinct strings → `len()==3 >= _COV_INFRA_THRESHOLD` → `halt_queue()`.

Evidence from `/tmp/rio-dev/rio-sprint-1-merge-183.log` (mc=67, P0512 coverage):

```
error: Cannot build '/nix/store/ibc4j3skizklpd3xgg0a6x402lj9ydh8-vm-test-run-rio-lifecycle-core.drv'.
error: Cannot build '/nix/store/rpwf6f9rb5y1nvsws0ahr50qy0rk6zqz-rio-cov-vm-lifecycle-core-k3s.drv'.
error: Cannot build '/nix/store/pr29yjzvf2dzg0m1vcaj9kk48gdgqaxy-rio-cov-vm-total.drv'.
```

Regex captures: `rio-lifecycle-core`, `vm-lifecycle-core-k3s`, `vm-total`. Three strings, one root cause. Subsequent coverage (merge-185, P0513) green — one-off timing, not pipeline break. But the queue halted.

The existing test at [`test_scripts.py:986-997`](../../.claude/lib/test_scripts.py) asserts dedup works for `vm-test-run-protocol-warm` + `rio-cov-protocol-warm` → both capture `protocol-warm`. That naming is NOT what coverage.nix produces: the per-test lcov is `rio-cov-${name}` where `${name}` is the `vmTestsCov` attrset key — `vm-<scenario>-<fixture>`, NOT bare `<scenario>`. The test's expectation was wrong from day one; it tested a naming scheme that doesn't exist.

P0304-T518's proposed fix ("drop `rio-cov` alternation, count only `vm-test-run-`") is correct and simpler than the followup's suggestion (strip/normalize). Cascade products are NEVER independent evidence of a pipeline break — they fail BECAUSE a dependency failed. Root VM tests (`vm-test-run-*`) are the only direct evidence.

## Tasks

### T1 — `fix(tooling):` narrow `_COV_SCENARIO_FAIL_RE` to root VM tests only

At [`merge.py:528-532`](../../.claude/lib/onibus/merge.py), drop the `(?:vm-test-run|rio-cov)` alternation:

```python
# ≥3 distinct root scenarios. rio-cov-* drvs are cascade products
# of the per-test lcov pipeline (coverage.nix:76 mkPerTestLcov + :125
# vmLcov aggregate): ONE vm-test-run failure cascades to N rio-cov
# failures. Three false-positive halts (mc=51 fetcher-split, mc=56
# observability, mc=67 lifecycle-core — all SIGTERM flakes or timing,
# all subsequently green) before this was narrowed. Real pipeline
# breaks fail multiple vm-test-run-* drvs directly (the PSA break
# failed all of them).
_COV_SCENARIO_FAIL_RE = re.compile(
    r"^error: Cannot build '/nix/store/[a-z0-9]+-"
    r"vm-test-run-([\w-]+)\.drv'",
    re.MULTILINE,
)
```

Also strip the `rio-` prefix from captures (optional normalization — `vm-test-run-rio-lifecycle-core` captures as `rio-lifecycle-core`; stripping `rio-` gives the scenario slug `lifecycle-core` that matches what humans say). At [`merge.py:549`](../../.claude/lib/onibus/merge.py):

```python
scenarios = sorted({m.removeprefix("rio-") for m in _COV_SCENARIO_FAIL_RE.findall(log)})
```

The `removeprefix` is cosmetic (the halt message reads better) but also future-proofs against non-`rio-` prefixed VM tests if any get added.

### T2 — `test(tooling):` replace the wrong-naming-scheme dedup test with real corpus

[`test_scripts.py:986-997`](../../.claude/lib/test_scripts.py) tests `vm-test-run-protocol-warm` + `rio-cov-protocol-warm` → one name. That naming doesn't exist; the test proved nothing. Replace with the REAL cascade from merge-183.log:

```python
# mc=67 corpus: ONE lifecycle-core fail cascaded to 3 drv failures.
# vm-test-run drv is the root; rio-cov-vm-* are dependency-cascade
# products that should NOT count. P517: regex narrowed to
# vm-test-run only after three false-positive halts.
log_cascade = tmp_path / "log_cascade"
log_cascade.write_text(
    "error: Cannot build '/nix/store/ibc4j3skizklpd3xgg0a6x402lj9ydh8-"
    "vm-test-run-rio-lifecycle-core.drv'.\n"
    "error: Cannot build '/nix/store/rpwf6f9rb5y1nvsws0ahr50qy0rk6zqz-"
    "rio-cov-vm-lifecycle-core-k3s.drv'.\n"
    "error: Cannot build '/nix/store/pr29yjzvf2dzg0m1vcaj9kk48gdgqaxy-"
    "rio-cov-vm-total.drv'.\n"
    "error: Cannot build '/nix/store/mva3mppp6pl8k9ca286cflp7x7717qyc-"
    "rio-coverage-full.drv'.\n"
)
n, names = m.coverage_full_red(str(log_cascade))
assert n == 1 and names == ["lifecycle-core"], (
    f"ONE vm-test-run fail + cascade products should count as 1, "
    f"got n={n} names={names}"
)
assert m.coverage_maybe_halt(str(log_cascade)) is False
assert not (state / "queue-halted").exists()
```

Also update the existing 3-scenario test at `:968-984` — its `rio-cov-lifecycle-core` line now won't match. Replace with a third `vm-test-run-` line so it still tests the ≥3 threshold:

```python
# 3 distinct vm-test-run fails → infra break, halt.
log3 = tmp_path / "log3"
log3.write_text(
    "error: Cannot build '/nix/store/aaa111-vm-test-run-rio-protocol-warm.drv'.\n"
    "error: Cannot build '/nix/store/bbb222-vm-test-run-rio-lifecycle-core.drv'.\n"
    "error: Cannot build '/nix/store/ccc333-vm-test-run-rio-security-nonpriv.drv'.\n"
)
n, names = m.coverage_full_red(str(log3))
assert n == 3
assert names == ["lifecycle-core", "protocol-warm", "security-nonpriv"]
```

### T3 — `docs(plan):` mark P0304-T518 superseded

Edit [`plan-0304-trivial-batch-p0222-harness.md:4090`](plan-0304-trivial-batch-p0222-harness.md) T518 header → prefix with `[SUPERSEDED by P517]`. The T518 exit criterion at `:4587` and Files-fence entry at `:4995` stay (for archaeology) but the implementer skips them — the grep criterion (`grep 'rio-cov' ... → 0 hits`) will pass naturally once P517 lands.

## Exit criteria

- `grep "(?:vm-test-run|rio-cov)" .claude/lib/onibus/merge.py` → 0 hits (alternation gone)
- `grep "rio-cov" .claude/lib/onibus/merge.py | grep -v '^#\|comment'` → 0 hits in live code (comment mentions are fine)
- `nix develop -c python3 -m pytest .claude/lib/test_scripts.py::test_coverage_full_red_heuristic -v` → PASSED
- Feed merge-183.log verbatim to `coverage_full_red()` → `(1, ["lifecycle-core"])`
- `grep 'SUPERSEDED by P517' .claude/work/plan-0304-*.md` → ≥1 hit at T518 header

## Tracey

No domain markers — tooling fix, not a component behavior. The heuristic itself has no spec marker (internal coordinator mechanics).

## Files

```json files
[
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T1: _COV_SCENARIO_FAIL_RE :528-532 drop rio-cov alternation + :549 removeprefix('rio-')"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T2: :968-984 three-scenario test rewrite vm-test-run only; :986-997 cascade-dedup test replaced with merge-183 corpus"},
  {"path": ".claude/work/plan-0304-trivial-batch-p0222-harness.md", "action": "MODIFY", "note": "T3: T518 header :4090 [SUPERSEDED by P517] prefix"}
]
```

```
.claude/
├── lib/onibus/merge.py            # T1: regex narrow + prefix strip
├── lib/test_scripts.py            # T2: corpus-backed test
└── work/plan-0304-*.md            # T3: supersession marker
```

## Dependencies

```json deps
{"deps": [484], "soft_deps": [304], "note": "Supersedes P0304-T518. P0484 is DONE (introduced the heuristic + test_coverage_full_red_heuristic); discovered_from NOT in deps because this is a coordinator finding (no discovered_from int). Zero hard deps on in-flight work. Priority ~75: three halts this session is active coordinator disruption."}
```

**Depends on:** [P0484](plan-0484-merger-cov-smoke-ci.md) DONE — introduced `coverage_maybe_halt` + `_COV_SCENARIO_FAIL_RE` + the test. No in-flight deps.
**Soft-deps:** [P0304](plan-0304-trivial-batch-p0222-harness.md) — T518 is the batch-trivial version this supersedes. P0304-T521 edits `merge.py:266`+`:1038` (gpgsign); non-overlapping with `:528-549`.
**Conflicts with:** `merge.py` is not in top-20 collisions. P0295-T487 edits `merge.py:556` (docstring) — adjacent but non-overlapping. `test_scripts.py` P0295-T492 edits `:1506`, P0508 inserted at `:1508` — non-overlapping with `:946-1000`.
