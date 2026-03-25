# Plan 0446: merge.py agent_start records unpadded path — archive-agents wipes live in-flight rows

Coordinator-filed correctness finding at [`.claude/lib/onibus/merge.py:357`](../../.claude/lib/onibus/merge.py). `agent_start()` records the worktree path as unpadded `p{N}` (e.g., `p439`), but the actual implementer worktrees use zero-padded `p{N:04d}` (e.g., `p0439`). When `archive_agents()` runs its path-existence check, `Path("p439").is_dir()` returns `False` (the directory is `p0439`), so the row gets archived as stale — even though the agent is actively running.

**Observed impact (2026-03-25 wave-1):** all 9 impl rows + verify/review rows recorded in the session were dropped by the first `archive-agents` run. The coordinator lost track of every in-flight agent.

One-line fix: either pad in `agent_start()` (change `f"p{N}"` → `f"p{N:04d}"`) or make `archive_agents()` check both forms. Prefer padding at source — it matches the worktree naming convention everywhere else.

## Entry criteria

- [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) merged (introduced `archive_agents` + the worktree-existence check this bug lives in)

## Tasks

### T1 — `fix(harness):` merge.py agent_start — pad plan number to 4 digits

At [`merge.py:357`](../../.claude/lib/onibus/merge.py), change the worktree path format from `p{N}` to `p{N:04d}`:

```python
# before
worktree = f"p{plan}"
# after
worktree = f"p{plan:04d}"
```

Grep for other unpadded `p{` f-strings in `merge.py` and sibling onibus modules — the padding convention should be consistent everywhere a worktree path is constructed.

### T2 — `test(harness):` regression test — agent_start path matches worktree convention

Add to [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py): `test_agent_start_pads_worktree_path` — call `agent_start(plan=439, ...)`, read back the `AgentRow.worktree` field, assert it equals `p0439` not `p439`. Test MUST fail on pre-fix code.

Optionally add `test_archive_agents_preserves_live_padded_worktree` — create a temp `p0439` directory, record an agent row, run `archive_agents()`, assert the row survives.

## Exit criteria

- `/nbr .#ci` green (harness-only change — `.#ci` exercises onibus via pre-commit)
- `pytest .claude/lib/test_scripts.py -k 'agent_start_pads\|archive_agents_preserves'` → passed
- `grep 'f"p{.*:04d}"' .claude/lib/onibus/merge.py` → ≥1 hit at the `agent_start` worktree construction

## Tracey

No tracey marker — harness tooling below spec granularity.

## Files

```json files
[
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T1: :357 pad worktree path p{N} → p{N:04d}"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T2: +test_agent_start_pads_worktree_path regression"}
]
```

```
.claude/lib/
├── onibus/
│   └── merge.py           # T1: :357 padding fix
└── test_scripts.py        # T2: regression test
```

## Dependencies

```json deps
{"deps": [306], "soft_deps": [], "note": "P0306 introduced archive_agents + the path-existence check. One-char fix but high impact — every wave loses all agent tracking until fixed."}
```

**Depends on:** [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) — the `archive_agents` worktree-check landed there.
**Conflicts with:** `merge.py` is harness-hot — [P0448](plan-0448-onibus-collisions-claude-work-filter.md) touches `collisions.py` (different file). P0304-T10 touches `merge.py:413` (crashed-writer edge case) — `:357` vs `:413`, non-overlapping hunks.
