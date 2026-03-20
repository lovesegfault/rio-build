# Plan 0417: dag_flip already-done — double-bump on re-invocation

rev-p414 correctness. [P0414](plan-0414-merger-amend-branch-ref-fix.md)'s `dag_flip` at [`merge.py:159-233`](../../.claude/lib/onibus/merge.py) added an already-done fast-path at `:201-206`: if `set_status` wrote an identical tree (dag row was pre-flipped), skip amend, return `amend_sha="already-done"`, and `count_bump()` unconditionally.

The unconditional bump conflates two re-entry scenarios:

| Case | Scenario | dag.jsonl pre-call | merge-shas.jsonl pre-call | Correct action |
|---|---|---|---|---|
| **(a)** | Coordinator fast-path: ff + `set_status DONE` directly, never ran `dag_flip` | `status="DONE"` | no row for this merge | **bump** — merge happened, mc never incremented |
| **(b)** | Merger ran `dag_flip` → crashed AFTER `count_bump()` → coordinator re-invoked `dag_flip` for same plan | `status="DONE"` | **row already present** (mc=N recorded at prior run) | **skip** — bumping again → mc off-by-one for all future cadence-range computations |

Case (b) is the exact bug [P0414](plan-0414-merger-amend-branch-ref-fix.md) was written to prevent at the `count_bump`-runs-after-`amend` ordering layer (the [P0319](plan-0319-merge-shas-pre-amend-dangling.md) lesson at `:225-228`). P0414 fixed the within-run ordering correctly; the already-done path reintroduces the double-bump at the re-invocation boundary — IRONIC given the plan's purpose.

The test at [`test_scripts.py:2167-2190`](../../.claude/lib/test_scripts.py) (`test_dag_flip_already_done_noop`) asserts `mc==1` on a fresh already-done call — it tests case (a), blesses wrong (b). No test calls `dag_flip` twice for the same plan.

**Fix:** add `plan: int | None` to `MergeSha` model. `count_bump()` already writes `MergeSha` rows; have it record the `plan_num` passed through from `dag_flip`. In the already-done branch, scan `merge-shas.jsonl` for a row with `plan == plan_num`: if present, case (b) — return that row's `mc` without bumping. If absent, case (a) — bump normally.

This also serves the cadence-range computation: `_cadence_range()` at `:236-266` indexes merge-shas.jsonl by `mc`; having `plan` in each row doesn't affect it (still `{r.mc: r.sha}` dict-comprehension at `:262`). Purely additive schema change.

## Entry criteria

- [P0414](plan-0414-merger-amend-branch-ref-fix.md) merged (`dag_flip` compound exists at [`merge.py:159`](../../.claude/lib/onibus/merge.py); `DagFlipResult` model at [`models.py:488`](../../.claude/lib/onibus/models.py))

## Tasks

### T1 — `fix(harness):` MergeSha — add optional plan field

MODIFY [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py) at `:285-298` (`MergeSha` class):

```python
class MergeSha(BaseModel):
    """One row in state/merge-shas.jsonl — merge-count → integration-branch
    tip at that merge. Written by count_bump() AFTER the merger's amend
    (P0319 fix @ 8a1ed8cd — before the fix, pre-amend SHAs dangled).
    Read by _cadence_range() to compute git ranges for consolidator/
    bughunter agents. Last-row-per-mc wins (count-bump --set-to can
    re-record the same mc with a different tip after a reset).

    `plan` added by P0417: identifies which plan's merge caused
    this bump. Used by dag_flip's already-done path to distinguish
    coordinator-fast-path-never-bumped (no row for plan → bump) from
    merger-crashed-post-bump-re-invoked (row exists → skip).
    Optional — older rows have `plan=None`; `--set-to` manual rewinds
    also write `plan=None` (no single plan corresponds to a rewind)."""
    mc: int = Field(ge=0, description="merge-count — value AFTER this bump")
    sha: str = Field(
        pattern=r"^[0-9a-f]{8,40}$",
        description="git rev-parse <integration-branch> — full 40-hex or "
        "abbreviated ≥8-hex. Same pattern as Mitigation.landed_sha.",
    )
    ts: datetime = Field(description="UTC timestamp of the bump")
    plan: int | None = Field(
        default=None,
        description="plan number whose merge caused this bump; None for "
        "older rows pre-P0417 or for --set-to manual rewinds",
    )
```

Backward-compatible: pydantic populates `plan=None` for existing rows that lack the field. No migration needed.

### T2 — `fix(harness):` count_bump — accept plan kwarg, record in MergeSha row

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) `count_bump` at `:123-156`:

```python
def count_bump(set_to: int | None = None, *, plan: int | None = None) -> int:
    """Cadence counter: mod 5 → consolidator, mod 7 → bughunter.

    Also records the integration-branch tip SHA at this merge-count to
    merge-shas.jsonl — _cadence_range() indexes by merge-count, not
    commit-count. <...existing docstring...>

    `plan` kwarg (P0417): if provided, recorded in the MergeSha row
    alongside mc+sha+ts. dag_flip passes this so the already-done path
    can check "did a prior dag_flip for this plan already bump?"
    (case-(b) re-invocation → skip; case-(a) coord-fast-path → bump)."""
    ...
    if tip:
        append_jsonl(sha_file, MergeSha(
            mc=new, sha=tip, ts=datetime.now(timezone.utc), plan=plan,
        ))
    return new
```

MODIFY `dag_flip` at `:229` — normal-path count-bump passes plan:

```python
# count-bump MUST run AFTER amend — it records rev-parse
# INTEGRATION_BRANCH in merge-shas.jsonl. Pre-amend that SHA is
# orphaned (reflog-only). This ordering was the P0319 fix; dag_flip
# keeps it correct by construction. P0417: pass plan so the
# already-done re-invocation check can find this row.
mc = count_bump(plan=plan_num)
```

### T3 — `fix(harness):` dag_flip already-done — check merge-shas before bump

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) already-done branch at `:201-206`:

```python
staged = git_try("diff", "--cached", "--name-only", cwd=REPO_ROOT) or ""
if not staged.strip():
    # already-done: dag row was pre-flipped. Two cases:
    #   (a) coord-fast-path — ff + set_status DONE directly, never ran
    #       dag_flip → merge-shas.jsonl has no row for this plan →
    #       bump (merge happened, mc never incremented).
    #   (b) merger crashed AFTER count_bump → re-invoked → row exists
    #       → skip (bumping again = mc off-by-one forever; the exact
    #       P0221-class double-bump P0414 was meant to prevent).
    # Distinguish by scanning merge-shas.jsonl for plan==plan_num.
    sha_file = STATE_DIR / "merge-shas.jsonl"
    prior_mc: int | None = None
    if sha_file.exists():
        for row in read_jsonl(sha_file, MergeSha):
            if row.plan == plan_num:
                prior_mc = row.mc
                break
    if prior_mc is not None:
        # case (b): already bumped at prior run
        return DagFlipResult(
            plan=plan_num, amend_sha="already-done",
            mc=prior_mc, unblocked=unblocked, queue_consumed=consumed,
        )
    # case (a): never bumped — bump now
    return DagFlipResult(
        plan=plan_num, amend_sha="already-done",
        mc=count_bump(plan=plan_num), unblocked=unblocked,
        queue_consumed=consumed,
    )
```

Add `read_jsonl` + `MergeSha` to `dag_flip`'s imports if not already visible from the file-level imports at `:30`.

### T4 — `test(harness):` already-done re-invocation — idempotent mc

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py). Rename `test_dag_flip_already_done_noop` → `test_dag_flip_already_done_case_a_coord_fastpath` (clarifies it tests coord-fast-path-never-bumped, not the general re-invocation case). Body unchanged (asserts `mc==1` on first already-done call — correct for case (a)).

ADD new test after `:2190`:

```python
def test_dag_flip_already_done_case_b_reinvoke_idempotent(dag_flip_repo):
    """P0417 regression (IRONIC — P0414 fixed within-run double-bump,
    introduced re-invoke double-bump): merger crashes AFTER count_bump →
    coordinator re-invokes dag_flip for same plan → must NOT bump again.

    Simulates: seed dag with UNIMPL, call dag_flip(99) once (normal path,
    amend + count_bump → mc=1), then call dag_flip(99) again. Second call
    hits already-done (dag is DONE, nothing staged) → scans merge-shas
    → finds plan=99 row from first call → returns mc=1 unchanged.

    Pre-fix, second call would count_bump() → mc=2 → all future
    _cadence_range() windows off-by-one."""
    from onibus.jsonl import write_jsonl
    from onibus.merge import dag_flip

    repo, dag_path = dag_flip_repo
    write_jsonl(dag_path, [PlanRow(plan=99, title="x", status="UNIMPL")])
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", "feat: seed", "--no-verify")

    # First invocation — normal path (amend + count_bump → mc=1)
    r1 = dag_flip(99)
    assert r1.amend_sha != "already-done"
    assert r1.mc == 1

    # Second invocation — already-done path; merge-shas has plan=99 row
    # from r1 → returns that row's mc, no bump.
    r2 = dag_flip(99)
    assert r2.amend_sha == "already-done"
    assert r2.mc == 1, (
        f"re-invocation must NOT double-bump; got mc={r2.mc} "
        f"(P0221-class bug P0414 was meant to prevent)"
    )

    # Count file confirms: still 1.
    count_file = repo / ".claude" / "state" / "merge-count.txt"
    assert int(count_file.read_text().strip()) == 1


def test_dag_flip_already_done_case_a_preserves_old_rows(dag_flip_repo):
    """Case (a) on a repo with older merge-shas rows (plan=None from
    pre-P0417 or --set-to rewinds): plan=None rows must NOT match
    plan=99 → still bumps. Regression guard for over-eager match."""
    from onibus.jsonl import write_jsonl, append_jsonl
    from onibus.merge import dag_flip
    from onibus.models import MergeSha
    from datetime import datetime, timezone

    repo, dag_path = dag_flip_repo
    write_jsonl(dag_path, [PlanRow(plan=99, title="x", status="DONE")])
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", "feat: pre-flipped", "--no-verify")
    pre = _git(repo, "rev-parse", "HEAD")

    # Seed merge-shas with an older plan=None row (mc=1, different context).
    sha_file = repo / ".claude" / "state" / "merge-shas.jsonl"
    sha_file.parent.mkdir(parents=True, exist_ok=True)
    # Also seed count-file so count_bump reads cur=1 → bumps to 2.
    (repo / ".claude" / "state" / "merge-count.txt").write_text("1\n")
    append_jsonl(sha_file, MergeSha(
        mc=1, sha=pre, ts=datetime.now(timezone.utc), plan=None,
    ))

    result = dag_flip(99)
    assert result.amend_sha == "already-done"
    assert result.mc == 2, (
        f"plan=None rows must not match plan=99; case (a) should still "
        f"bump. Got mc={result.mc}"
    )
```

### T5 — `fix(harness):` DagFlipResult — document already-done bifurcation

MODIFY [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py) `DagFlipResult.amend_sha` field description at `:497-500`:

```python
amend_sha: str = Field(
    description="post-amend HEAD (short), or 'already-done' if dag was "
    "pre-flipped. already-done covers two cases (P0417): "
    "(a) coord-fast-path-never-bumped → mc freshly incremented; "
    "(b) merger-crashed-post-bump-re-invoked → mc from prior MergeSha "
    "row (NOT re-bumped). Caller can't distinguish from this field "
    "alone — check mc against merge-count.txt if that matters."
)
```

## Exit criteria

- `/nixbuild .#ci` green (or clause-4c on VM-flake)
- `grep 'plan: int | None' .claude/lib/onibus/models.py` → ≥1 hit in `MergeSha` body (T1)
- `python3 -c "from onibus.models import MergeSha; MergeSha(mc=1, sha='abcd1234', ts='2026-01-01T00:00:00Z')"` — no ValidationError (T1: plan defaults to None, backward-compat)
- `python3 -c "import json; from onibus.models import MergeSha; MergeSha.model_validate(json.loads('{\"mc\":1,\"sha\":\"abcd1234\",\"ts\":\"2026-01-01T00:00:00Z\"}'))"` — no ValidationError (T1: older rows without plan field parse)
- `grep 'plan=plan' .claude/lib/onibus/merge.py` → ≥2 hits (T2: count_bump(plan=…) kwarg pass-through + dag_flip `:229` call)
- `grep 'prior_mc\|row.plan == plan_num' .claude/lib/onibus/merge.py` → ≥2 hits (T3: scan logic in already-done branch)
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'dag_flip'` → ≥5 passed (T4: 3 P0414 + 2 new case-b/case-a tests; rename case-a test accounted)
- **T4 mutation:** revert T3 (restore unconditional `count_bump()` in already-done branch) → `test_dag_flip_already_done_case_b_reinvoke_idempotent` FAILS with `mc==2` assertion error
- `grep 'case (a)\|case (b)\|coord-fast-path\|crashed-post-bump' .claude/lib/onibus/models.py` → ≥1 hit in DagFlipResult (T5: docstring bifurcation)

## Tracey

No markers. Harness-tooling correctness; not spec-behavior.

## Files

```json files
[
  {"path": ".claude/lib/onibus/models.py", "action": "MODIFY", "note": "T1: MergeSha +plan field :285-298 (optional, backward-compat). T5: DagFlipResult amend_sha docstring :497-500 bifurcation"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T2: count_bump +plan kwarg :123-156, pass to MergeSha row + dag_flip :229 pass plan_num. T3: already-done branch :201-206 → scan merge-shas.jsonl for plan==plan_num, skip bump if found"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T4: rename test_dag_flip_already_done_noop → _case_a_coord_fastpath; +test_dag_flip_already_done_case_b_reinvoke_idempotent + _case_a_preserves_old_rows after :2190"}
]
```

```
.claude/lib/onibus/
├── models.py             # T1+T5: MergeSha+plan; DagFlipResult docstring
└── merge.py              # T2+T3: count_bump(plan=), already-done scan
.claude/lib/
└── test_scripts.py       # T4: case-b idempotency + case-a old-rows
```

## Dependencies

```json deps
{"deps": [414], "soft_deps": [319, 306, 221, 323], "note": "HARD-dep P0414 (dag_flip compound + DagFlipResult + test_dag_flip_already_done_noop all arrive with it; discovered_from=414). Soft-dep P0319 (DONE — amend-THEN-count_bump ordering; T2's dag_flip :229 pass carries that discipline forward). Soft-dep P0306 (DONE — merge-shas.jsonl populate via MergeSha model; T1 extends the model). Soft-dep P0221 (DONE — the double-bump lesson this plan prevents re-entering). Soft-dep P0323 (MergeSha pydantic model — T1 adds field to it). IRONIC DEPENDENCY: P0414's commit message references P0221's double-bump as the thing it prevents; P0414's already-done path reintroduces a variant of it at a different boundary (re-invocation not within-run). This plan closes that loop."}
```

**Depends on:** [P0414](plan-0414-merger-amend-branch-ref-fix.md) — `dag_flip`, `DagFlipResult`, `test_dag_flip_already_done_noop` must exist.

**Conflicts with:** [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) — [P0304](plan-0304-trivial-batch-p0222-harness.md)-T191 adds a `print(…, file=sys.stderr)` at `:222-223` (update-ref fallback); T3 here edits `:201-206`; non-overlapping hunks. [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py) — [P0304](plan-0304-trivial-batch-p0222-harness.md)-T191 may add a `ref_forced: bool` field to `DagFlipResult` at `:488-509`; T5 edits `:497-500` (docstring); mild overlap — additive field-add + docstring-edit in same class, rebase-clean either order. [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py) — T4 renames + adds tests after `:2190`; P0414 added the `:2061-2190` block; additive, no conflict.
