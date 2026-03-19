# Plan 0323: MergeSha pydantic model — merge-shas.jsonl is the lone raw-dict holdout

**Consolidator mc-window finding.** [`merge-shas.jsonl`](../../.claude/state/merge-shas.jsonl) is the only state file in [`state/`](../../.claude/state/) without a pydantic model. [`merge.py:146-150`](../../.claude/lib/onibus/merge.py) writes with raw `json.dumps`; [`:181-185`](../../.claude/lib/onibus/merge.py) reads with raw `json.loads` in a loop. This is the **same stringly-typed boundary** that [`jsonl.py:1-7`](../../.claude/lib/onibus/jsonl.py) docstring calls out as "the COV_EXIT= bug" root cause: producer and consumer agree on field names by convention, not by schema.

Three data points in the consolidator window that argue for the model:

1. **Same window, same file shape, opposite choice.** [P0317](plan-0317-excusable-vm-regex-knownflake-schema.md) (mc30) added [`Mitigation(BaseModel)`](../../.claude/lib/onibus/models.py) at `:138-146` with `landed_sha: str = Field(pattern=r"^[0-9a-f]{8,40}$")`. A `MergeSha` model is the same ~8 lines with the same sha-pattern field.
2. **41 models already exist.** `grep -c 'class .*(BaseModel)' models.py` → 41. `merge-shas.jsonl` is the lone holdout.
3. **Two write-path bugs in this file already.** [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) created the file (mc27); [P0319](plan-0319-merge-shas-pre-amend-dangling.md) immediately found it was recording pre-amend (dangling) SHAs. A pydantic model wouldn't have caught the pre-amend bug (that's a call-order issue, not a schema issue) — but it WOULD make any future malformed row (wrong field name, non-hex sha, mc-as-string) fail loudly at write time instead of at `_cadence_range()` read time.

[`jsonl.py`](../../.claude/lib/onibus/jsonl.py) already has `append_jsonl` at `:47` and `read_jsonl` at `:36` — both generic over `BaseModel`. Swapping is mechanical.

## Tasks

### T1 — `refactor(harness):` add MergeSha model

MODIFY [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py) — add near the other state-file models (e.g., after [`CoverageResult`](../../.claude/lib/onibus/models.py) at `:274`, or near `MergeQueueRow` at `:63`):

```python
class MergeSha(BaseModel):
    """One row in state/merge-shas.jsonl — merge-count → integration-branch
    tip at that merge. Written by count_bump() AFTER the merger's amend
    (P0319 fix @ 8a1ed8cd — before the fix, pre-amend SHAs dangled).
    Read by _cadence_range() to compute git ranges for consolidator/
    bughunter agents. Last-row-per-mc wins (count-bump --set-to can
    re-record the same mc with a different tip after a reset)."""
    mc: int = Field(ge=0, description="merge-count — value AFTER this bump")
    sha: str = Field(
        pattern=r"^[0-9a-f]{8,40}$",
        description="git rev-parse <integration-branch> — full 40-hex or "
        "abbreviated ≥8-hex. Same pattern as Mitigation.landed_sha.",
    )
    ts: datetime = Field(description="UTC timestamp of the bump")
```

**The `pattern` catches:** uppercase hex (git uses lowercase), 7-char git-short (too ambiguous for a long-lived record), non-hex garbage. The `ge=0` catches negative mc (would indicate a count-file corruption). Neither has fired yet — that's the point, they're guards.

### T2 — `refactor(harness):` count_bump() — json.dumps → append_jsonl

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) at `:145-150`:

```python
# Before:
    if tip:
        with sha_file.open("a") as f:
            f.write(json.dumps({
                "mc": new, "sha": tip,
                "ts": datetime.now(timezone.utc).isoformat(),
            }) + "\n")

# After:
    if tip:
        from onibus.jsonl import append_jsonl
        from onibus.models import MergeSha
        append_jsonl(sha_file, MergeSha(
            mc=new, sha=tip, ts=datetime.now(timezone.utc),
        ))
```

Note `ts` is now a `datetime` object, not an `.isoformat()` string — pydantic's `model_dump_json()` handles serialization. Validation fires at construction: if `tip` is somehow empty-string (the `if tip:` guard should prevent this) or contains non-hex, `MergeSha(...)` raises `ValidationError` instead of writing garbage.

**Import placement:** if `merge.py` already imports from `onibus.jsonl`/`onibus.models` at module top, add `MergeSha` to the existing import line instead of the inline import. Check at dispatch.

### T3 — `refactor(harness):` _cadence_range() — json.loads loop → read_jsonl

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) at `:180-185`:

```python
# Before:
    by_mc: dict[int, str] = {}
    for line in sha_file.read_text().splitlines():
        if not line.strip():
            continue
        row = json.loads(line)
        by_mc[row["mc"]] = row["sha"]

# After:
    from onibus.jsonl import read_jsonl
    from onibus.models import MergeSha
    by_mc: dict[int, str] = {r.mc: r.sha for r in read_jsonl(sha_file, MergeSha)}
```

`read_jsonl` already skips empty lines and `#`-prefixed comments ([`jsonl.py:42`](../../.claude/lib/onibus/jsonl.py)). The dict-comp preserves last-wins semantics (later rows overwrite earlier for the same mc) — matches the comment at [`:178-179`](../../.claude/lib/onibus/merge.py).

**Migration note:** existing `merge-shas.jsonl` rows have `ts` as an ISO-string, not a `datetime`. Pydantic's `datetime` field accepts ISO strings natively (`model_validate_json` parses them), so reading old rows works. New rows written by T2 also serialize to ISO strings via `model_dump_json()`. No file migration needed.

### T4 — `test(harness):` MergeSha roundtrip + sha pattern validation

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py) — add near the jsonl roundtrip test at `:133`:

```python
def test_mergesha_roundtrip(tmp_path: Path):
    """MergeSha writes via append_jsonl and reads via read_jsonl. The
    ts datetime roundtrips through ISO string. Last-row-per-mc-wins
    is a dict-comp property, not a model property — tested separately
    in test_cadence_range_last_mc_wins if that exists."""
    from onibus.models import MergeSha
    from onibus.jsonl import append_jsonl, read_jsonl
    from datetime import datetime, timezone

    p = tmp_path / "merge-shas.jsonl"
    row = MergeSha(mc=42, sha="deadbeef" * 5, ts=datetime.now(timezone.utc))
    append_jsonl(p, row)
    got = read_jsonl(p, MergeSha)
    assert len(got) == 1
    assert got[0].mc == 42
    assert got[0].sha == "deadbeef" * 5
    # ts roundtrips (pydantic serializes datetime → ISO, parses back)
    assert abs((got[0].ts - row.ts).total_seconds()) < 1


def test_mergesha_sha_pattern_rejects():
    """The sha Field pattern ^[0-9a-f]{8,40}$ — same as Mitigation.landed_sha.
    Catches: 7-char git-short (too ambiguous), uppercase (git is lowercase),
    non-hex, empty. Raw json.dumps didn't validate any of this; a typo'd
    sha would surface as 'fatal: bad object' in _cadence_range's git diff
    much later."""
    from onibus.models import MergeSha
    from datetime import datetime, timezone
    from pydantic import ValidationError
    import pytest

    ts = datetime.now(timezone.utc)
    # 7-char: rejected (min 8)
    with pytest.raises(ValidationError):
        MergeSha(mc=1, sha="abc1234", ts=ts)
    # uppercase: rejected
    with pytest.raises(ValidationError):
        MergeSha(mc=1, sha="DEADBEEF", ts=ts)
    # non-hex: rejected
    with pytest.raises(ValidationError):
        MergeSha(mc=1, sha="ghijklmn", ts=ts)
    # 8-char lowercase hex: accepted
    MergeSha(mc=1, sha="deadbeef", ts=ts)
    # 40-char full: accepted
    MergeSha(mc=1, sha="a" * 40, ts=ts)
    # negative mc: rejected (ge=0)
    with pytest.raises(ValidationError):
        MergeSha(mc=-1, sha="deadbeef", ts=ts)
```

**Check at dispatch:** whether `test_scripts.py` already imports `pytest` at module top or inline per-test. Match the convention.

## Exit criteria

- `/nbr .#ci` green
- `grep 'class MergeSha(BaseModel)' .claude/lib/onibus/models.py` → 1 hit (T1)
- `grep -c 'class .*(BaseModel)' .claude/lib/onibus/models.py` → 42 (was 41; T1 adds one)
- `grep 'json.dumps\|json.loads' .claude/lib/onibus/merge.py` — if any hits remain, they're NOT in `count_bump` or `_cadence_range` (T2+T3: raw dict I/O replaced). Likely zero hits total (these were the only raw-json sites in merge.py), but other functions may use json legitimately — check at dispatch.
- `grep 'MergeSha' .claude/lib/onibus/merge.py` → ≥2 hits (T2+T3: both write and read paths use the model)
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'mergesha'` → 2 passed (T4)
- **Backward-compat check:** existing `state/merge-shas.jsonl` rows (written pre-T2 with `"ts": "<iso-string>"`) parse successfully via `read_jsonl(sha_file, MergeSha)`. Verify by running `_cadence_range()` against the live file — should return a valid range, not raise ValidationError. (Pydantic's datetime parser accepts ISO strings.)
- **Precondition self-check:** `test_mergesha_roundtrip` asserts `len(got) == 1` BEFORE asserting field values — if `append_jsonl` or `read_jsonl` silently no-ops, the test fails on the length check instead of an IndexError on `got[0]`.

## Tracey

No markers. Harness code — state-file schema is tooling, not spec'd behavior.

## Files

```json files
[
  {"path": ".claude/lib/onibus/models.py", "action": "MODIFY", "note": "T1: +MergeSha(BaseModel) ~10 lines, near CoverageResult :274 or MergeQueueRow :63"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T2: count_bump :145-150 json.dumps→append_jsonl(MergeSha(...)). T3: _cadence_range :180-185 json.loads loop→read_jsonl dict-comp"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T4: +test_mergesha_roundtrip + test_mergesha_sha_pattern_rejects (near jsonl roundtrip :133)"}
]
```

```
.claude/lib/onibus/
├── models.py                     # T1: +MergeSha
└── merge.py                      # T2+T3: json.dumps/loads → append_jsonl/read_jsonl
.claude/lib/
└── test_scripts.py               # T4: roundtrip + pattern tests
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [304, 319, 311, 0322], "note": "No hard deps — P0306 (created merge-shas.jsonl) DONE, P0319 T1/T2 (reorder amend-before-bump + backfill) DONE @ 8a1ed8cd. Consolidator origin. Soft-conflict P0304 T17 (also touches merge.py — _LEASE_SECS bump at :57; T2/T3 here touch :145-150 and :180-185; different functions, 90+ lines apart). Soft-conflict P0304 T1 (models.py PlanFile regex at :322; T1 here adds a class near :63 or :274; different sections). Soft-conflict P0319 T3 (net-new amend-after-bump regression test — test_onibus_dag.py or test_scripts.py; different test concern). Soft-conflict P0311 T9 + P0322 T3 (both also add tests to test_scripts.py — all additive, different names). All low-risk."}
```

**Depends on:** None. The state file and both code paths exist as of sprint-1. P0319's fix at [`8a1ed8cd`](https://github.com/search?q=8a1ed8cd&type=commits) reordered the merger agent's call sequence — didn't touch `merge.py` code, so no conflict.

**Conflicts with:**
- [`merge.py`](../../.claude/lib/onibus/merge.py) — [P0304](plan-0304-trivial-batch-p0222-harness.md) T17 bumps `_LEASE_SECS` at `:57`. T2/T3 here touch `:145-150` and `:180-185`. 90+ lines apart; zero textual conflict.
- [`models.py`](../../.claude/lib/onibus/models.py) — [P0304](plan-0304-trivial-batch-p0222-harness.md) T1 edits `PlanFile.path` regex at `:322`. T1 here adds a new class near `:63` or `:274`. Different sections.
- [`test_scripts.py`](../../.claude/lib/test_scripts.py) — three plans appending tests this docs run (P0311 T9, P0322 T3, this T4). All add new `def test_...` functions. Zero name collisions.
