# Plan 997534801: count_bump TOCTOU — MergeSha-row BEFORE count-file

rev-p417 correctness. IRONY²: [P0417](plan-0417-dag-flip-already-done-double-bump.md) (DONE at mc=240) fixes the already-done double-bump by scanning `merge-shas.jsonl` for a row with `plan == plan_num` before bumping — but [`merge.py:148`](../../.claude/lib/onibus/merge.py) STILL writes `count-file` at `:148` THEN the `MergeSha` row at `:159-162`. A crash between (or `tip==""` at `:159` — comment at `:149-153` explicitly names this as reachable) → count bumped, MergeSha row never written → P0417's scan at [`:218-224`](../../.claude/lib/onibus/merge.py) finds nothing → case-(a) path → **bumps again**.

| Write | Line | Crash/skip here → re-scan verdict |
|---|---|---|
| `count_file.write_text(f"{new}\n")` | [`:148`](../../.claude/lib/onibus/merge.py) | mc already incremented on disk |
| `tip = subprocess.run(...)` | [`:154-158`](../../.claude/lib/onibus/merge.py) | `tip==""` → `:159` guard skips → no row |
| `if tip: append_jsonl(sha_file, MergeSha(...))` | [`:159-162`](../../.claude/lib/onibus/merge.py) | row present → P0417 scan finds it |

P0417's case-b detection only works if the MergeSha row was written. The window between count-file write and MergeSha write is exactly where P0417's fix is blind. P0417 is DONE (mc=240) — this plan closes the last structurally-reachable variant.

**Fix:** swap the order inside `count_bump()` — write MergeSha row BEFORE count-file. [`_cadence_range`](../../.claude/lib/onibus/merge.py) at `:293` already dedupes by last-row-per-mc (`{r.mc: r.sha for r in read_jsonl(...)}`), so a duplicate-mc row from a partially-applied bump is harmless: next successful bump overwrites it in the dict comprehension.

The `tip==""` case becomes: MergeSha row skipped (because no tip to record) AND count bumped anyway. That's a cadence-gap not a double-bump — `_cadence_range` returns `None` for that window, agents silently skipped, next merge recovers. Acceptable degradation vs. the current double-bump-forever failure mode.

## Entry criteria

- [P0417](plan-0417-dag-flip-already-done-double-bump.md) merged (**DONE at mc=240** — `MergeSha.plan` field + already-done scan at [`:218-224`](../../.claude/lib/onibus/merge.py) + `count_bump(plan=)` kwarg at [`:123`](../../.claude/lib/onibus/merge.py) all landed)

## Tasks

### T1 — `fix(harness):` count_bump — MergeSha row before count-file

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) `count_bump` at `:123-163`. Swap write order:

```python
def count_bump(set_to: int | None = None, *, plan: int | None = None) -> int:
    """..."""
    count_file = STATE_DIR / "merge-count.txt"
    sha_file = STATE_DIR / "merge-shas.jsonl"
    if set_to is not None:
        new = set_to
    else:
        cur = int(count_file.read_text().strip()) if count_file.exists() else 0
        new = cur + 1
    count_file.parent.mkdir(parents=True, exist_ok=True)

    # Record tip at THIS merge-count FIRST — P0417's already-done scan
    # checks merge-shas.jsonl for plan==plan_num before bumping. If
    # count-file is written first and we crash before MergeSha, the
    # scan sees no row → case-(a) → double-bump. Writing MergeSha
    # first makes the crash window a cadence-gap (agents skipped for
    # one tick) not a permanent off-by-one. _cadence_range dedupes by
    # last-row-per-mc at :262, so duplicate-mc rows are harmless.
    # tip=="" (bare rev-parse fail outside repo) → skip MergeSha but
    # still bump count — same cadence-gap degradation.
    tip = subprocess.run(
        ["git", "rev-parse", INTEGRATION_BRANCH],
        cwd=REPO_ROOT,
        capture_output=True, text=True,
    ).stdout.strip()
    if tip:
        append_jsonl(sha_file, MergeSha(
            mc=new, sha=tip, ts=datetime.now(timezone.utc), plan=plan,
        ))
    count_file.write_text(f"{new}\n")
    return new
```

The `set_to is not None` path (`--set-to` manual rewind) also benefits: rewinds write `plan=None` rows per P0417-T1's docstring, so the scan correctly skips them.

### T2 — `test(harness):` crash-between-writes regression

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py). Add after P0417's `test_dag_flip_already_done_case_b_reinvoke_idempotent`:

```python
def test_count_bump_crash_between_writes_not_double_bump(dag_flip_repo):
    """P997534801 regression — IRONY²: P0417's scan checks for a
    MergeSha row with plan==plan_num, but pre-fix count_bump wrote
    count-file BEFORE MergeSha. Crash between → count bumped, no row
    → scan finds nothing → case-(a) → double-bump.

    Simulates: patch append_jsonl to raise after count_file.write_text
    succeeds (pre-fix ordering) vs after MergeSha append (post-fix
    ordering). Post-fix, count-file is NOT written on the crashed
    path → re-invoke bumps from the same cur value → no off-by-one."""
    from unittest.mock import patch
    from onibus.merge import count_bump

    repo, _ = dag_flip_repo
    count_file = repo / ".claude" / "state" / "merge-count.txt"
    sha_file = repo / ".claude" / "state" / "merge-shas.jsonl"
    count_file.parent.mkdir(parents=True, exist_ok=True)
    count_file.write_text("5\n")

    # Simulate crash after MergeSha write, before count-file write.
    # Post-fix order means append_jsonl runs FIRST; patching
    # Path.write_text to raise after the parent-mkdir call catches
    # the count-file write specifically.
    with patch.object(type(count_file), "write_text",
                      side_effect=RuntimeError("simulated crash")):
        try:
            count_bump(plan=99)
        except RuntimeError:
            pass  # expected

    # MergeSha row written, count-file unchanged.
    assert int(count_file.read_text().strip()) == 5, (
        "count-file must NOT be written before MergeSha — crash leaves "
        "count unchanged, next invoke starts from same cur"
    )
    # If tip resolved, merge-shas.jsonl has the row (harmless duplicate-mc
    # that _cadence_range dedupes). Either way: no double-bump vector.
    if sha_file.exists():
        from onibus.jsonl import read_jsonl
        from onibus.models import MergeSha
        rows = list(read_jsonl(sha_file, MergeSha))
        # mc=6 recorded (the NEW value) even though count-file still says 5.
        # Next successful count_bump writes mc=6 count-file + mc=6 row;
        # _cadence_range's last-row-per-mc dict-comprehension uses the
        # later row's sha. Harmless.
        assert any(r.mc == 6 and r.plan == 99 for r in rows)
```

**Care:** `patch.object(type(count_file), "write_text", ...)` patches `PosixPath.write_text` globally for the `with` block — also affects the MergeSha path's `append_jsonl` if that routes through `Path.write_text`. Check `append_jsonl` implementation at dispatch; if it uses `open(mode="a").write`, the patch is selective. If it uses `write_text`, use `patch("onibus.merge.append_jsonl", side_effect=...)` for the MergeSha half instead and assert count-file WAS written under the OLD ordering (negative control).

### T3 — `docs(harness):` merge.py — update P0319 ordering comment

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) comment at `:255-259` (`dag_flip` normal-path — grep `count-bump MUST run AFTER amend`):

```python
# count-bump MUST run AFTER amend — it records rev-parse
# INTEGRATION_BRANCH in merge-shas.jsonl. Pre-amend that SHA is
# orphaned (reflog-only). This ordering was the P0319 fix; dag_flip
# keeps it correct by construction. P0417 passes plan so the
# already-done re-invocation check can find this row. P997534801
# reordered count_bump internally (MergeSha row BEFORE count-file)
# so crash-between-writes degrades to a cadence-gap not double-bump.
mc = count_bump(plan=plan_num)
```

## Exit criteria

- `/nixbuild .#ci` green (or clause-4c — this plan touches `.claude/` only; tracey-validate drv behavioral-identical per [P0319](plan-0319-merge-shas-pre-amend-dangling.md) precedent)
- `python3 -c "import re; src=open('.claude/lib/onibus/merge.py').read(); body=re.search(r'def count_bump.*?(?=\ndef )', src, re.S).group(0); assert body.index('append_jsonl') < body.index('count_file.write_text'), 'MergeSha before count-file'"` — no AssertionError (T1: write-order swapped)
- `grep 'P0417\|P997534801\|cadence-gap' .claude/lib/onibus/merge.py` → ≥2 hits in `count_bump` body + ≥1 hit at `dag_flip :225` region (T1+T3: comments explain the ordering constraint)
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'crash_between_writes'` → 1 passed (T2)
- **T2 mutation:** revert T1 (restore count-file-first order) → `test_count_bump_crash_between_writes_not_double_bump` FAILS with `count-file must NOT be written before MergeSha` assertion. Proves ordering is load-bearing.
- P0417's existing `test_dag_flip_already_done_case_b_reinvoke_idempotent` still passes (T1 doesn't regress the case-b scan)

## Tracey

No markers. Harness-tooling correctness; not spec-behavior.

## Files

```json files
[
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T1: count_bump :123-163 — swap MergeSha append_jsonl before count_file.write_text. T3: :255-259 dag_flip comment — append P997534801 ordering note"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T2: +test_count_bump_crash_between_writes_not_double_bump after P0417's case_b test"}
]
```

```
.claude/lib/onibus/
└── merge.py              # T1+T3: count_bump write-order swap + comment
.claude/lib/
└── test_scripts.py       # T2: crash-between-writes regression
```

## Dependencies

```json deps
{"deps": [417], "soft_deps": [418, 414, 319, 304], "note": "HARD-dep P0417 (DONE at mc=240 — MergeSha.plan field + already-done scan + count_bump(plan=) kwarg landed; discovered_from=417). Soft-dep P0418 (UNIMPL — also touches merge.py for canonicalization: T2 at queue_consume :200, agent_start, agent_mark; T3/T4/T5 at :376-576 rename cluster — all non-overlapping with this plan's :123-163 count_bump body; rebase-clean). Soft-dep P0414 (DONE — dag_flip compound exists; T3 edits its :255-259 comment). Soft-dep P0319 (DONE — amend-THEN-count_bump ordering that T3's comment references). Soft-dep P0304-T191 (update-ref fallback at :247-253 — non-overlapping). IRONY²-CHAIN COMPLETE: P0414 fixed within-run double-bump → P0417 fixed re-invoke double-bump at already-done path → this plan fixes crash-between-writes double-bump inside count_bump itself. Each fix's window is narrower than the previous; this is the last structurally-reachable variant (MergeSha-row-THEN-count-file makes the scan's evidence exist before the observable state changes)."}
```

**Depends on:** [P0417](plan-0417-dag-flip-already-done-double-bump.md) — `MergeSha.plan` field, already-done scan logic, `count_bump(plan=)` kwarg.

**Conflicts with:** [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) — [P0418](plan-0418-onibus-rename-canonicalization-hardening.md) T2-T5 touch `:193`, `:296`, `:314`, `:376-576` (canonicalization + rename cluster); this plan touches `:123-156` (count_bump body) + `:225-228` (comment). Non-overlapping hunks. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T191 touches `:217-223` (update-ref fallback stderr). Also non-overlapping. [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py) — P0417-T4 and P0418-T6 add tests in the `dag_flip_repo` fixture section; T2 here adds one more. All additive, different test-fn names.
