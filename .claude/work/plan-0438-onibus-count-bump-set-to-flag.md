# Plan 438: onibus count-bump — positional set_to → `--set-to` flag

bughunter correctness finding at mc=254. [`cli.py:546`](../../.claude/lib/onibus/cli.py) defines `count-bump` with a **positional** `set_to` int, but [`merge.py`](../../.claude/lib/onibus/merge.py) `count_bump` docstring (and [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md)'s usage at `:223`, [P0323](plan-0323-mergesha-pydantic-model.md) at `:25`) documents it as `--set-to N` flag syntax.

**Footgun hit in production:** chain-merger-2 at mc=254 ran `onibus merge count-bump 393` intending to pass a plan number — but `393` was consumed as `set_to`, rewinding `merge-count.txt` to 393. The adjacent subcommands at [`cli.py:544,547`](../../.claude/lib/onibus/cli.py) (`queue-consume`, `dag-flip`) both take `plan` as a positional int — identical CLI shape, opposite semantics. A bare positional int on `count-bump` is a silent-rewind trap.

**Root fix, not workaround:** change the positional to `--set-to` so `count-bump 393` argparse-errors instead of silently rewinding. The coordinator followup at mc=254 covered only prompt-template editing — that's treating the symptom. mc-rewind breaks `_cadence_range` (start_mc goes negative or gaps) and confuses merge-shas dedup.

## Tasks

### T1 — `fix(harness):` cli.py count-bump — positional set_to → --set-to flag

MODIFY [`.claude/lib/onibus/cli.py`](../../.claude/lib/onibus/cli.py) at `:546`:

```python
# BEFORE
sp = g.add_parser("count-bump"); sp.add_argument("set_to", type=int, nargs="?")

# AFTER
sp = g.add_parser("count-bump"); sp.add_argument("--set-to", dest="set_to", type=int, default=None)
```

Verify [`cli.py:308`](../../.claude/lib/onibus/cli.py) `print(merge.count_bump(args.set_to))` still works — `dest="set_to"` keeps the attribute name unchanged.

### T2 — `test(harness):` count-bump positional rejected, --set-to accepted

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py) — add near existing `count_bump` tests:

```python
def test_count_bump_positional_rejected():
    """Bare positional int argparse-errors (was silent mc-rewind pre-P0438)."""
    rc = subprocess.run(
        [ONIBUS, "merge", "count-bump", "393"],
        capture_output=True, text=True,
    ).returncode
    assert rc != 0, "count-bump 393 should argparse-error, not set mc=393"

def test_count_bump_set_to_flag_works():
    # --set-to N still functions for explicit rewind/bootstrap
    ...
```

### T3 — `docs(harness):` sweep stale `count-bump <N>` references

MODIFY any `.claude/` doc that shows bare `count-bump N` syntax. Grep at dispatch:

```bash
grep -rn 'count-bump [0-9]\|count-bump <' .claude/ --include='*.md'
```

Known-correct references that should **stay**: [P0306:223](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) and [P0323:25](plan-0323-mergesha-pydantic-model.md) already use `--set-to N` syntax. The merger agent at [`rio-impl-merger.md:138`](../../.claude/agents/rio-impl-merger.md) says count-bump is "owned by dag-flip" — no bare `count-bump N` call there. If grep finds zero hits, T3 is a no-op.

## Exit criteria

- `/nbr .#ci` green (or `cd .claude/lib/onibus && python3 -m pytest` if not in nix check)
- `onibus merge count-bump 393` exits non-zero with argparse "unrecognized arguments" (or similar)
- `onibus merge count-bump --set-to 393` succeeds and sets mc=393
- `onibus merge count-bump` (no args) still increments by 1
- `grep 'add_argument("set_to"' .claude/lib/onibus/cli.py` → 0 hits (positional gone)

## Tracey

No marker changes. Harness tooling is not spec-covered (no `harness.*` domain in `TRACEY_DOMAINS`).

## Files

```json files
[
  {"path": ".claude/lib/onibus/cli.py", "action": "MODIFY", "note": "T1: :546 positional→--set-to flag (1-line)"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T2: +test_count_bump_positional_rejected + flag-works tests"}
]
```

```
.claude/lib/onibus/
└── cli.py                    # T1: :546 one-line flag change
.claude/lib/
└── test_scripts.py           # T2: +2 tests
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [420, 421, 427], "note": "bughunter-mc254 correctness (discovered_from=bughunter). No hard-deps — cli.py:546 exists since P0306-era. Soft-dep P0420 (count_bump TOCTOU — touches merge.py count_bump body, this touches cli.py argparse only; non-overlapping). Soft-dep P0421 (merge.py rename-cluster extract — count_bump stays in merge.py, unaffected). Soft-dep P0427 (chain-merger FIFO loop — the agent that HIT this footgun; P0427 adds chain-merger loop logic, this fixes the CLI it calls). cli.py low-traffic. Absorbs coordinator followup mc=254 (prompt-template edit) — T3 sweeps any remaining doc refs but the root fix makes prompt edits unnecessary."}
```

**Depends on:** none — `cli.py:546` exists since P0306-era.
**Conflicts with:** [`cli.py`](../../.claude/lib/onibus/cli.py) — [P0418](plan-0418-onibus-rename-canonicalization-hardening.md) touched `models.py`+`cli.py` for validators; `:546` is argparse setup far from P0418's validator wiring. Non-overlapping.
