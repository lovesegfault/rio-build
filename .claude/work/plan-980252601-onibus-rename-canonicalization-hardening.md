# Plan 980252601: onibus rename_unassigned + plan-ID canonicalization hardening

bughunt-mc231 + consol-mc230. Four compounding defects in the docs-writer → `rename_unassigned` → `dag_flip` chain at [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py), all independently reproduced. docs-993168 merger worked around three of them manually at [`99d3912f`](https://github.com/search?q=99d3912f&type=commits); the fourth ([P0414](plan-0414-merger-amend-branch-ref-fix.md) queue-consume unpadded) is **live and silent** — every `dag_flip`-driven merge going forward leaves its `MergeQueueRow` behind.

[P0401](plan-0401-docs-writer-lazy-t-assignment.md) closed the T-placeholder loop **for writers that emit the spec'd 9-digit `T9<runid><NN>` format**. docs-103180 (mc=225) proved the auto-rename works at 9-digit. But docs-993168 + docs-654701 (mc=231) both emitted **11-digit** `T<9-digit-P-placeholder><seq>` tokens — a writer-side format divergence from [`SKILL.md:41`](../../.claude/skills/plan/SKILL.md). That surface-level mismatch exposed three latent gaps that bite **even at 9-digit**:

| # | Gap | merge.py | Format-dependent? |
|---|---|---|---|
| A | `_T_PLACEHOLDER_RE` matches exactly 9 digits; writer 11-digit → regex misses → T-rewrite skips | [`:376`](../../.claude/lib/onibus/merge.py) | 11-digit only |
| B | `_rewrite_t_placeholders` scans `batch_docs` only; placeholder plan-docs (`plan-9ddddddNN-*.md`) cross-reference T-tokens → T-rewrite never touches them → P-rewrite's bare `str.replace(placeholder, padded)` **corrupts the T-substring** | [`:465`](../../.claude/lib/onibus/merge.py) + [`:528`](../../.claude/lib/onibus/merge.py) | **NO** — P/T sequence IDs both start at `01`, so `T980252601` substring-collides with `P980252601` under bare replace. [`merge.py:568-575`](../../.claude/lib/onibus/merge.py) comment **knows** this but defends `batch_docs` only. |
| C | P-rewrite branches on `rel.endswith(".jsonl")` only; `.md` gets `f"{assigned:04d}"` zero-padded → the `json deps` fence inside a placeholder `.md` becomes `"soft_deps": [307, 0416]` → `json.loads` `JSONDecodeError` | [`:525-528`](../../.claude/lib/onibus/merge.py) | **NO** — any writer that puts a bare-int placeholder in a `.md` deps fence hits this. |
| D | `dag_flip` calls `queue_consume(f"P{plan_num}")` (unpadded `"P414"`) but `MergeQueueRow.plan` is stored zero-padded (`"P0414"`); bare-str-cmp at [`:283`](../../.claude/lib/onibus/merge.py) never matches → **queue row never removed** | [`:193`](../../.claude/lib/onibus/merge.py) | N/A (plan-ID canonicalization, orthogonal to placeholders) |

Gap D also manifests at [`:296`](../../.claude/lib/onibus/merge.py) (`agent_start` prepends `P` but doesn't pad), [`:314`](../../.claude/lib/onibus/merge.py) (`agent_mark` bare str cmp), and [`cli.py:209`](../../.claude/lib/onibus/cli.py) (`agent-lookup` bare str cmp). Three distinct half-canonicalization strategies, zero `field_validator` on `AgentRow.plan:str` / `MergeQueueRow.plan:str` at [`models.py:31,65`](../../.claude/lib/onibus/models.py).

Adjacent: `_touched_batch_docs` regex [`:432`](../../.claude/lib/onibus/merge.py) hardcodes `plan-0\d{3}-` — batch docs `plan-1NNN+` become invisible to both T-rewrite and P-cross-ref rewrite at P1000. Currently P0417, so ~580 plans of headroom; zero marginal cost to fix alongside.

bughunt-mc231 verified docs-993168's manual rename left **zero residual tokens** (grep 993168/placeholder/T9993 across sprint-1 → 0 hits). The mc224-231 smell-sweep (7 merges, 21 commits, 100L net `rio-*/src/` added) was **clean**: unwrap=0, swallow=0, orphan-TODO=0, allow=0, lock-cluster=0, `#[ignore]`=0 — so no collateral-damage risk from this plan's harness touches.

## Entry criteria

- [P0401](plan-0401-docs-writer-lazy-t-assignment.md) merged (**DONE** — `_T_PLACEHOLDER_RE` + `_rewrite_t_placeholders` + `_touched_batch_docs` exist at [`merge.py:376-488`](../../.claude/lib/onibus/merge.py))
- [P0414](plan-0414-merger-amend-branch-ref-fix.md) merged (**DONE** — `dag_flip` compound at [`merge.py:193`](../../.claude/lib/onibus/merge.py) exists; this plan fixes its `queue_consume` call)

## Tasks

### T1 — `fix(harness):` canonical_plan_id helper + field_validator on AgentRow/MergeQueueRow

MODIFY [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py). Add a single canonicalization helper **before** `AgentRow` at `:30`:

```python
_DOCS_BRANCH_RE = re.compile(r"^docs-\d{6}$")
_PLAN_ID_RE = re.compile(r"^P?(\d+)$")


def canonical_plan_id(raw: int | str) -> str:
    """Normalize a plan reference to the canonical 'P0NNN' form.

    int → f"P{n:04d}". str "414"/"P414"/"P0414" → "P0414". docs-XXXXXX
    branch names pass through unchanged (writer/qa agent rows use the
    branch name, not a plan number).

    Three half-canonicalizations existed pre-P980252601: dag_flip at
    merge.py:193 did f"P{int}" (unpadded → "P414"), agent_start at
    :296 did prepend-only (no pad), and the str-cmp sites at :283/:314
    + cli.py:209 compared whatever the caller happened to pass. A
    queue_consume("P414") never matched stored "P0414" → the row
    stayed forever. This helper + field_validator on both models
    closes all four."""
    if isinstance(raw, int):
        return f"P{raw:04d}"
    s = raw.strip()
    if _DOCS_BRANCH_RE.fullmatch(s):
        return s
    m = _PLAN_ID_RE.fullmatch(s)
    if not m:
        raise ValueError(f"bad plan id {raw!r}: want int, 'P<N>', or 'docs-<6d>'")
    return f"P{int(m.group(1)):04d}"
```

Then add a `field_validator("plan", mode="before")` to **both** `AgentRow` (after `:36`) and `MergeQueueRow` (after `:73`) that calls `canonical_plan_id(v)`. `mode="before"` so it fires on construct AND on `model_validate_json` load — stale un-normalized rows in existing `agents-running.jsonl` / `merge-queue.jsonl` normalize on next read.

### T2 — `fix(harness):` merge.py dag_flip/agent_start/agent_mark — canonicalize at call site

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py). Import `canonical_plan_id` from models. Replace:

- [`:193`](../../.claude/lib/onibus/merge.py) `queue_consume(f"P{plan_num}")` → `queue_consume(canonical_plan_id(plan_num))`
- [`:296`](../../.claude/lib/onibus/merge.py) `plan=plan if plan.startswith("P") else f"P{plan}"` → `plan=canonical_plan_id(plan)` (the validator would catch it at construct, but explicit is clearer at the call site; the `re.fullmatch` at `:293` stays for worktree-path derivation)
- [`:314`](../../.claude/lib/onibus/merge.py) `if r.plan == plan` → `if r.plan == canonical_plan_id(plan)` (defensive — `r.plan` is already normalized by the validator on load; this normalizes the **caller's** arg)

MODIFY [`.claude/lib/onibus/cli.py`](../../.claude/lib/onibus/cli.py) at [`:209`](../../.claude/lib/onibus/cli.py): `if a.plan == args.plan` → `if a.plan == canonical_plan_id(args.plan)` (same defensive normalization).

### T3 — `fix(harness):` P-rewrite — anchor on P-prefix, not bare substring

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) at `_rewrite_and_rename` [`:524-528`](../../.claude/lib/onibus/merge.py). The bare `new.replace(r.placeholder, repl)` at `:528` substring-matches inside `T980252601` when `r.placeholder == "980252601"`. [`merge.py:568-575`](../../.claude/lib/onibus/merge.py) comment knows and defends `batch_docs` by running T-rewrite first — but that scan doesn't cover placeholder plan-docs themselves.

Replace the bare substring replace with **prefix-anchored** replacement. The placeholder appears in three contexts:

| Context | Example | Replace pattern |
|---|---|---|
| prose plan-ref | `P980252601`, `[P980252601](…)` | `P<placeholder>` → `P<04d>` |
| filename/link target | `plan-980252601-slug.md` | `plan-<placeholder>` → `plan-<04d>` |
| `dag.jsonl` integer | `"plan": 980252601`, `"deps": [980252601]` | bare `<placeholder>` → bare `<int>` (unpadded) |
| `json deps` fence (in `.md`) | `"soft_deps": [307, 980252601]` | bare `<placeholder>` → bare `<int>` (unpadded — **NOT** zero-padded; gap C) |

Two distinct rewrite forms, both needed in `.md`:

```python
for r in mapping:
    padded = f"{r.assigned:04d}"
    bare = str(r.assigned)
    if is_jsonl:
        # dag.jsonl: bare integer, no padding.
        new = new.replace(r.placeholder, bare)
    else:
        # .md: anchored replaces. P-prefix and plan-prefix protect
        # against T-substring collision (gap B); the JSON-context
        # regex handles the deps fence bare-int case (gap C) —
        # leading zero would break json.loads.
        new = new.replace(f"P{r.placeholder}", f"P{padded}")
        new = new.replace(f"plan-{r.placeholder}", f"plan-{padded}")
        # json-fence integer: immediately after `[`, `,`, `:`, or ` `
        # with word-boundary after. Unpadded.
        new = re.sub(
            rf"(?<=[\[,:\s]){re.escape(r.placeholder)}\b",
            bare,
            new,
        )
```

The look-behind `(?<=[\[,:\s])` excludes `P<ph>` and `plan-<ph>` (already handled) and `T<ph>` (T-rewrite handles it in batch_docs; placeholder docs shouldn't contain T-refs to their OWN P-placeholder per the skill but if they do, the T-prefix excludes it here).

After this, the [`:568-575`](../../.claude/lib/onibus/merge.py) comment's "T-rewrite FIRST" ordering becomes **belt-and-suspenders** rather than load-bearing. Keep the ordering; update the comment to say so.

### T4 — `fix(harness):` T-rewrite scope — scan placeholder plan-docs too

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) at `_rewrite_t_placeholders` [`:465`](../../.claude/lib/onibus/merge.py). The `batch_docs` scan misses placeholder plan-docs (`plan-9ddddddNN-*.md`) that cross-reference batch-doc T-tokens ("see P0304-T980252601 for …"). T3's anchored P-replace makes the corruption impossible, but the T-token **still doesn't get rewritten** — it survives as `T980252601` pointing at nothing.

Extend the scan to include the placeholder-doc set (same `mapping`-derived paths as `_rewrite_and_rename`'s `touched` at [`:512-514`](../../.claude/lib/onibus/merge.py)):

```python
def _rewrite_t_placeholders(
    worktree: Path, tgt: str, batch_docs: list[str],
    placeholder_docs: list[str],  # NEW: plan-9ddddddNN-*.md from mapping
) -> dict[str, dict[str, int]]:
    # Per-batch-doc sequences stay per-batch-doc. placeholder_docs are
    # scanned for T-tokens and rewritten USING batch_docs' mappings
    # (a T-ref in a placeholder doc points at a batch-doc task, so the
    # assignment is whatever that batch doc got).
    found = _find_t_placeholders(worktree, batch_docs)
    # … existing per-doc assignment …

    # Second pass: rewrite T-refs inside placeholder docs using the
    # assignments just computed. One T-token can only point at one
    # batch doc (writer emits T<runid><NN> where <runid><NN> is
    # unique within the writer run), so union the mappings.
    all_t: dict[str, int] = {}
    for mapping in result.values():
        all_t.update(mapping)
    for rel in placeholder_docs:
        p = worktree / rel
        text = p.read_text()
        new = text
        for ph, assigned in all_t.items():
            new = new.replace(f"T{ph}", f"T{assigned}")
        if new != text:
            atomic_write_text(p, new)
    return result
```

Update `rename_unassigned` at [`:576`](../../.claude/lib/onibus/merge.py) to compute `placeholder_docs` from `placeholders` (the `_find_placeholders` result at `:578`) **before** calling `_rewrite_t_placeholders`, and pass it in. This means hoisting the `placeholders = _find_placeholders(worktree)` call above the T-rewrite — sequence becomes: find-batch-docs, find-placeholder-docs, T-rewrite (both sets), P-rewrite+rename.

### T5 — `fix(harness):` widen _touched_batch_docs regex — plan-1NNN headroom

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) at [`:432`](../../.claude/lib/onibus/merge.py). `r"\.claude/work/plan-0\d{3}-"` → `r"\.claude/work/plan-\d{4}-"`. This matches all real 4-digit plan docs (0001-9999) without the 0-prefix assumption, and stays disjoint from 9-digit placeholder filenames.

Also: `_T_PLACEHOLDER_RE` at [`:376`](../../.claude/lib/onibus/merge.py) — widen from `r"\bT(9\d{8})\b"` to `r"\bT(9\d{8,10})\b"` (accepts 9-11 digits starting with 9). This catches writer-side 11-digit format-divergence defensively; the CANONICAL form is still 9-digit per [`SKILL.md:41`](../../.claude/skills/plan/SKILL.md). Add a comment that the regex is permissive on purpose — writer bugs shouldn't strand tokens.

### T6 — `test(harness):` test_dag_flip_consumes_queue_row + placeholder-doc-T-ref + deps-fence-json regression

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py). Three new tests:

**(a) `test_dag_flip_consumes_queue_row`** — after `test_dag_flip_moves_integration_branch_ref` at [`:2081`](../../.claude/lib/test_scripts.py). Seed a `MergeQueueRow(plan="P0099", ...)` before `dag_flip(99)`, assert `queue_consumed == 1` in the result AND the jsonl file has zero matching rows post-flip. The existing P0414 regression test at `:2058+` seeds NO queue row so `queue_consumed:0` was untested.

**(b) `test_rename_placeholder_doc_t_crossref`** — after `test_rename_unassigned_batch_doc_p_crossref` at [`:1495`](../../.claude/lib/test_scripts.py). Seed a placeholder plan-doc that contains `P0304-T912345601` (cross-ref to a batch-doc T-token). Seed P0304 with `### T912345601 — …`. After `rename_unassigned`, assert the placeholder doc's T-ref matches P0304's assigned T-number (not corrupted to `T<padded>01`, not stale `T912345601`).

**(c) `test_rename_deps_fence_leading_zero`** — in the same section. Seed a placeholder plan-doc whose `json deps` fence contains `"soft_deps": [307, 912345602]` (bare-int placeholder). After `rename_unassigned`, assert `json.loads` of the deps-fence body succeeds AND the value is a bare int (no leading zero). Reproduces bughunt-mc231's `json.loads('[307, 0416]')` rejection.

### T7 — `docs(harness):` rio-planner.md — reinforce 9-digit T-placeholder format

MODIFY [`.claude/agents/rio-planner.md`](../../.claude/agents/rio-planner.md). The skill already documents `T9<runid><NN>` at the Step-1 section, but two writers (docs-993168, docs-654701) emitted 11-digit `T<P-placeholder><seq>`. Add an explicit **wrong-form** example next to the right-form at Step 1:

```markdown
**T-placeholders (batch appends):** same 9-digit scheme with `T` prefix.
First batch-append T-item is `T9<runid>01`.

RIGHT: `T980252601` (9 digits: `9` + `802526` runid + `01` seq)
WRONG: `T98025260101` (11 digits: reused the P-placeholder as prefix)

The 11-digit form misses `_T_PLACEHOLDER_RE` and strands tokens. Use
the 6-digit runid directly, not your P-placeholder.
```

Place after wherever Step-1's placeholder section currently ends (grep `Step 1` / `placeholder` at dispatch for the exact anchor; [P0304-T28](plan-0304-trivial-batch-p0222-harness.md) added prior leading-zero guidance nearby at `:125+`).

## Exit criteria

- `python3 -c "from onibus.models import canonical_plan_id; assert canonical_plan_id(414)=='P0414'; assert canonical_plan_id('414')=='P0414'; assert canonical_plan_id('P414')=='P0414'; assert canonical_plan_id('P0414')=='P0414'; assert canonical_plan_id('docs-802526')=='docs-802526'"` — all five forms canonicalize
- `python3 -c "from onibus.models import AgentRow; r=AgentRow.model_validate({'plan':'414','role':'impl','status':'running'}); assert r.plan=='P0414'"` — field_validator normalizes on construct
- `python3 -c "from onibus.models import MergeQueueRow; r=MergeQueueRow.model_validate_json('{\"plan\":\"P414\",\"worktree\":\"/x\",\"verdict\":\"PASS\",\"commit\":\"abc\"}'); assert r.plan=='P0414'"` — field_validator normalizes on load
- `grep 'canonical_plan_id' .claude/lib/onibus/merge.py .claude/lib/onibus/cli.py` → ≥4 hits (dag_flip:193, agent_start:296, agent_mark:314, cli agent-lookup:209)
- `grep 'f"P{plan_num}"' .claude/lib/onibus/merge.py` → 0 hits (unpadded construct gone)
- `python3 -c "import json; json.loads('[307, 0416]')"` → `JSONDecodeError` (confirms the bug class reproduces; this is the **negative-control**, not a criterion for the fix)
- `grep 'plan-0\\\\d{3}' .claude/lib/onibus/merge.py` → 0 hits (regex ceiling lifted)
- `grep 'plan-\\\\d{4}' .claude/lib/onibus/merge.py` → ≥1 hit (T5 widened form present at `:432`)
- `grep '9\\\\d{8,10}' .claude/lib/onibus/merge.py` → ≥1 hit (T5: `_T_PLACEHOLDER_RE` accepts 9-11 digits)
- `pytest .claude/lib/test_scripts.py::test_dag_flip_consumes_queue_row` — pass (T6a)
- `pytest .claude/lib/test_scripts.py::test_rename_placeholder_doc_t_crossref` — pass (T6b)
- `pytest .claude/lib/test_scripts.py::test_rename_deps_fence_leading_zero` — pass (T6c)
- **Mutation:** revert T3's anchored replace back to bare `str.replace` → T6b fails with corrupted `T<padded>01` in placeholder doc. Proves anchoring is load-bearing.
- `grep 'WRONG:.*11 digits\|11-digit form misses' .claude/agents/rio-planner.md` → ≥1 hit (T7 wrong-form example landed)
- `grep 'queue_consumed' .claude/lib/onibus/merge.py` → still ≥1 hit in `DagFlipResult` construction (T1/T2 didn't accidentally drop the field)
- `/nixbuild .#ci` green (or clause-4c `.#checks.x86_64-linux.nextest` standalone — this plan touches `.claude/` only, tracey-validate drv does change but behavioral-identical per [P0319](plan-0319-tracey-validate-fileset-exclude-claude.md) precedent)

## Tracey

No new markers. Harness tooling — not spec-behavior. The rename_unassigned mechanism serves the docs-writer → merger pipeline; tracey tracks `rio-*` src behavior.

## Files

```json files
[
  {"path": ".claude/lib/onibus/models.py", "action": "MODIFY", "note": "T1: +canonical_plan_id helper before :30, +field_validator('plan', mode='before') on AgentRow+MergeQueueRow. Soft-conflict P0304-T191 (DagFlipResult +ref_forced field at :488-509 — additive, non-overlapping)"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T2: canonicalize at :193/:296/:314. T3: anchored P-replace at :524-528. T4: T-rewrite scans placeholder docs (:465 sig + :576 call-site hoist). T5: regex widen :376+:432. Soft-conflict P0304-T191 (:217-223 update-ref fallback — non-overlapping), P0417 (:201-206 already-done branch — non-overlapping)"},
  {"path": ".claude/lib/onibus/cli.py", "action": "MODIFY", "note": "T2: agent-lookup :209 canonicalize args.plan"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T6: +test_dag_flip_consumes_queue_row after :2081, +test_rename_placeholder_doc_t_crossref + test_rename_deps_fence_leading_zero after :1495"},
  {"path": ".claude/agents/rio-planner.md", "action": "MODIFY", "note": "T7: +wrong-form 11-digit example at Step-1 placeholder section (near P0304-T28's :125 leading-zero guidance; session-cached, lands next worktree-add)"}
]
```

```
.claude/lib/onibus/
├── models.py        # T1: canonical_plan_id + 2× field_validator
├── merge.py         # T2-T5: canonicalize + anchor + scope + widen
└── cli.py           # T2: agent-lookup canonicalize
.claude/lib/test_scripts.py      # T6: 3 regression tests
.claude/agents/rio-planner.md    # T7: wrong-form example
```

## Dependencies

```json deps
{"deps": [401, 414], "soft_deps": [417, 304], "note": "P0401 DONE — T-placeholder rewrite infrastructure exists (this plan extends scope + anchors). P0414 DONE — dag_flip exists (this plan fixes its unpadded queue_consume). Soft-dep P0417 (dag_flip already-done branch also edits merge.py:201-206 — non-overlapping hunks with T3/T4's :465-576 changes; both touch DagFlipResult construction but different fields; rebase-clean either order). Soft-dep P0304-T191 (update-ref fallback stderr at merge.py:217-223 + models.py DagFlipResult.ref_forced — non-overlapping with T1's validator additions at :31/:65; rebase-clean)."}
```

**Depends on:** [P0401](plan-0401-docs-writer-lazy-t-assignment.md) — `_T_PLACEHOLDER_RE` + `_rewrite_t_placeholders` + `_touched_batch_docs` infrastructure. [P0414](plan-0414-merger-amend-branch-ref-fix.md) — `dag_flip` compound with the buggy `queue_consume(f"P{plan_num}")` at `:193`.

**Conflicts with:** `.claude/lib/onibus/merge.py` — three in-flight touchers: [P0417](plan-0417-dag-flip-already-done-double-bump.md) (`:201-206` already-done branch, `DagFlipResult` amend_sha docstring), [P0304](plan-0304-trivial-batch-p0222-harness.md)-T191 (`:217-223` update-ref fallback + `DagFlipResult.ref_forced` field), and this plan (`:193`, `:376`, `:432`, `:465-488`, `:524-528`, `:568-576`). All non-overlapping hunks; rebase-clean. `.claude/lib/test_scripts.py` — P0417 adds `test_dag_flip_already_done_no_double_bump` near `:2184`; this plan adds `test_dag_flip_consumes_queue_row` in the same `dag_flip_repo` fixture section — both additive, sequence-independent. `.claude/agents/rio-planner.md` session-cached (lands on next worktree-add).
