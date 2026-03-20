# Plan 0401: docs-writer lazy T-number assignment — T-collision fix

consol-mc200 feature finding at [`.claude/skills/plan/SKILL.md:41`](../../.claude/skills/plan/SKILL.md). Two docs-writer runs (docs-267443 + docs-758618) appended to the same batch targets (P0304/P0311/P0295), both numbered T-items from their OWN worktree base, colliding with DIFFERENT content. QA caught it (21 cross-refs to fix); coordinator hand-renumbered via sed. Second collision this session.

Root cause: `/plan` step 3 checks batch-target `status` (UNIMPL/DONE) but not in-flight `docs-*` worktrees. Each concurrent writer sees the same last-T-number and numbers from there.

Two options per the finding:

- **(a) Serialize:** `/plan` step 3 also greps `git worktree list | grep docs-` → refuse-or-queue if another docs-* worktree exists. Simpler but adds coordinator latency (docs-writers fire every ~7 merges at current cadence-flush rate; serializing them is a throughput hit when the sink has >20 rows).
- **(b) Lazy T-assignment:** writer appends tasks with placeholder T-markers (`T9<runid><NN>`) — SAME 9-digit scheme as the existing P-number placeholders. `/merge-impl`'s `rename_unassigned` assigns real T-numbers at merge time (single string-replace, same mechanism).

Option **(b)** is the consolidator's recommendation — it matches the existing P-placeholder pattern, requires zero writer coordination, and `rename_unassigned` at [`merge.py:365`](../../.claude/lib/onibus/merge.py) already has the rewrite-pass infrastructure.

## Tasks

### T1 — `feat(harness):` merge.py — T-placeholder detection + assignment

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) at `_find_placeholders` (`:301`) and `rename_unassigned` (`:365`). Current pattern matches `9\d{8}` for P-placeholders in `plan-9<runid><NN>-*.md` filenames. Extend to ALSO find `T9<runid><NN>` markers in batch-target plan docs.

T-placeholder format: `T9<runid><NN>` where `<runid>` is 6 digits (same as P), `<NN>` is the writer's local T-sequence (01, 02, …). The `T` prefix disambiguates from P-placeholders (which are bare 9-digit).

Assignment scan: for each batch doc touched by the docs-branch (grep `git diff --name-only $TGT... -- '.claude/work/plan-0*.md'`), find `T9\d{8}` tokens, group by runid, for each group assign sequential real T-numbers starting from `max(existing-T-in-that-doc) + 1`. Single string-replace per token (same as the P-rewrite at `:352`).

```python
_T_PLACEHOLDER = re.compile(r"\bT(9\d{8})\b")

def _find_t_placeholders(worktree: Path, tgt: str) -> dict[Path, list[str]]:
    """batch-doc path → list of T9ddddddNN placeholders in insertion order."""
    touched = _git(["diff", "--name-only", f"{tgt}..."], cwd=worktree).splitlines()
    result: dict[Path, list[str]] = {}
    for f in touched:
        if not re.match(r"\.claude/work/plan-0\d{3}-", f):
            continue  # batch-target docs only (P0304/P0311/P0295 etc.)
        text = (worktree / f).read_text()
        phs = _T_PLACEHOLDER.findall(text)
        if phs:
            result[worktree / f] = list(dict.fromkeys(phs))  # dedup, keep order
    return result

def _max_existing_t(doc_path: Path, tgt: str) -> int:
    """Highest ### T<N> — header in the TGT-branch version of the doc."""
    tgt_text = _git(["show", f"{tgt}:{doc_path.relative_to(worktree)}"], ...)
    matches = re.findall(r"^### T(\d+) —", tgt_text, flags=re.M)
    return max(int(m) for m in matches) if matches else 0
```

Wire into `rename_unassigned` after the P-placeholder rewrite — T-rewrite reads the same `worktree`, scans batch-docs, assigns + replaces.

### T2 — `feat(harness):` rio-planner.md — emit T-placeholders for batch-appends

MODIFY [`.claude/agents/rio-planner.md`](../../.claude/agents/rio-planner.md) near the batch-append guidance (grep `Batch append`). The writer currently computes `next-T` from its worktree base (`grep '^### T' | tail -1`) and numbers sequentially. Change to:

> **Batch-append T-numbering:** use placeholder T-numbers `T9<runid><NN>` where `<runid>` is the same 6-digit Run ID from your prompt, `<NN>` is YOUR local sequence per-batch-doc (01, 02, …). E.g., first append to P0304 gets `T9<runid>01`, second gets `T9<runid>02`. First append to P0311 also starts at `T9<runid>01` (sequences are per-doc, not global). The merger's `rename_unassigned` assigns real sequential T-numbers at merge time — you never see the final numbers, and concurrent writers can't collide.
>
> **Cross-references:** when T-item A references T-item B in the SAME batch-append run, use the placeholder `T9<runid>NN` form. When referencing a PRE-EXISTING T-item (from TGT, not your append), use its real number (`T157`). The merger rewrites only `T9\d{8}` tokens.

Also MODIFY [`.claude/skills/plan/SKILL.md`](../../.claude/skills/plan/SKILL.md) at `:41` (the step-3 batch-check) — remove any language that encourages the writer to compute next-T from worktree-base, point at the placeholder scheme instead.

### T3 — `test(harness):` T-placeholder rewrite regression

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py) near the existing `rename_unassigned` tests (grep `placeholder`). Scenario:

```python
def test_rename_unassigned_t_placeholders_per_doc():
    # Batch doc P0304 on TGT has T1-T162. docs-branch appends
    # T959435401, T959435402 to P0304 and T959435401 to P0311
    # (separate per-doc sequence). TGT's P0311 has T1-T65.
    # After rename: P0304 → T163, T164. P0311 → T66.
    # Cross-ref in T959435402's body "see T959435401" → "see T163".
    ...
    assert "### T163 —" in p0304_text
    assert "### T164 —" in p0304_text
    assert "T959435401" not in p0304_text  # all placeholders rewritten
    assert "see T163" in p0304_text         # cross-ref rewritten
    assert "### T66 —" in p0311_text

def test_rename_unassigned_t_placeholders_concurrent_writers():
    # Two docs-branches both append to P0304. Branch-A uses runid 111111
    # (T911111101, T911111102), branch-B uses runid 222222 (T922222201).
    # Merge A first → A's become T163, T164. Then merge B → B's T922222201
    # becomes T165 (max-existing is now 164).
    ...
    assert "### T165 —" in p0304_after_b
```

### T4 — `docs(harness):` SKILL.md step-3 — document the concurrency model

MODIFY [`.claude/skills/plan/SKILL.md`](../../.claude/skills/plan/SKILL.md) after step 3's batch-check. Add a note:

> **Concurrency note:** multiple `/plan` invocations can be in-flight (one per followup flush). P-numbers and T-numbers both use the 9-digit placeholder scheme (P → `9<runid><NN>`, T → `T9<runid><NN>`) so writers never race on sequential numbers. The merger serializes assignment at merge time. If a batch doc appears in `git worktree list | grep docs-` (another writer is touching it), that's FINE — placeholders don't collide.

## Exit criteria

- `/nbr .#ci` green
- `grep '_T_PLACEHOLDER\|T9\\\\d{8}\|_find_t_placeholders' .claude/lib/onibus/merge.py` → ≥2 hits (T1: regex + fn)
- `grep '_max_existing_t\|max.*existing.*T' .claude/lib/onibus/merge.py` → ≥1 hit (T1: per-doc base computation)
- `grep 'T9<runid>\|T-placeholder\|placeholder T-number' .claude/agents/rio-planner.md` → ≥1 hit (T2: guidance added)
- `nix develop -c pytest .claude/lib/test_scripts.py -k 't_placeholders'` → ≥2 passed (T3)
- T3 mutation: comment out the T-rewrite pass in `rename_unassigned` → both tests fail (proves rewrite is load-bearing)
- `grep 'concurrency\|placeholders don.t collide' .claude/skills/plan/SKILL.md` → ≥1 hit (T4)

## Tracey

No tracey markers — harness/agent infrastructure, not spec'd behavior.

## Files

```json files
[
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T1: +_T_PLACEHOLDER regex, +_find_t_placeholders, +_max_existing_t; wire into rename_unassigned :365+ after P-rewrite"},
  {"path": ".claude/agents/rio-planner.md", "action": "MODIFY", "note": "T2: batch-append guidance — T9<runid>NN placeholders instead of computed next-T"},
  {"path": ".claude/skills/plan/SKILL.md", "action": "MODIFY", "note": "T2: :41 step-3 batch-check — remove next-T-from-worktree-base language. T4: concurrency-note after step 3"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T3: +test_rename_unassigned_t_placeholders_per_doc + _concurrent_writers (near existing placeholder tests)"}
]
```

```
.claude/
├── lib/
│   ├── onibus/merge.py        # T1: T-placeholder detection + rewrite
│   └── test_scripts.py        # T3: regression tests
├── agents/rio-planner.md      # T2: emit T-placeholders
└── skills/plan/SKILL.md       # T2+T4: step-3 update + concurrency note
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [325], "note": "consol-mc200 feature — 2nd T-collision this session (docs-267443 + docs-758618). Option-(b) lazy-T matches existing 9-digit P-placeholder pattern; rename_unassigned :365 already has rewrite infrastructure. Per-doc T-sequence (not global) so P0304's T959435401 and P0311's T959435401 are distinct (rewritten to different real numbers based on each doc's max-existing-T). Cross-ref rewrite covered by same string-replace (T959435401 → T163 everywhere in that doc). discovered_from=consolidator-mc200."}
```

**Depends on:** None (harness-only).
**Soft-dep:** [P0325](plan-0325-rename-unassigned-post-ff-rewrite.md) — its fix at [`merge.py:329-347`](../../.claude/lib/onibus/merge.py) derives the rewrite-set from `mapping` not `diff`; T1's T-placeholder scan uses `git diff --name-only TGT...` which is the SAME three-dot shape P0325 moved away from for P-placeholders. T1's scan is pre-ff (finds which batch-docs the writer touched), so three-dot is correct here — P0325's concern (post-ff diff is empty) doesn't apply. Document this distinction in T1's code comment.
**Conflicts with:** [`merge.py`](../../.claude/lib/onibus/merge.py) — P0304-T30 fixes the "by construction" comment at `:334-343` + adds batch-target glob at `:352+`; T1 here adds new fns after `:301` + wires at `:365+` — non-overlapping. [`test_scripts.py`](../../.claude/lib/test_scripts.py) — additive test-fns alongside P0304-T30's `batch_append_targets` test. [`rio-planner.md`](../../.claude/agents/rio-planner.md) — P0304-T28 adds no-leading-zero guidance near `:117`; T2 here edits batch-append guidance (different section).
