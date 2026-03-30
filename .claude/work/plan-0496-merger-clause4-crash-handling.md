# Plan 496: Merger step 4.5 — clause4-check crash handling

[P0488](plan-0488-merger-clause4-wiring.md) wired the clause-4 fast-path gate at [`rio-impl-merger.md:83-96`](../agents/rio-impl-merger.md). The decision table covers `SKIP | RUN_FULL | HALT`, but there is no branch for `clause4-check` **crashing** (uncaught exception → nonzero exit, empty/malformed stdout). When that happens:

1. Step 4 already landed the ff-merge — `$TGT` is advanced, integration lock is held
2. `verdict=$(... clause4-check ...)` captures empty/garbage
3. `decision=$(jq -r .decision <<<"$verdict")` → `jq` error or `null`
4. No table row matches → the merger script falls through undefined

The state is "ff landed, lock held, no verdict" — a half-advance. Recovery requires manual `git reset --hard <pre_merge>` + lock release. The window is narrow (crash must happen between step-4 ff and step-5 CI gate), but any Python exception in `merge.clause4_check()` — bad `nix eval` output parse, `git diff` subprocess failure, filesystem race on `last-green-ci-hash` — lands here.

**Fix shape:** fail-safe toward `RUN_FULL`. A crash in the fast-path optimizer should never skip CI; it should degrade to "run the full gate". Catch the exception at the CLI dispatch boundary, emit a `RUN_FULL` verdict with `reason="clause4-check crashed: <exc>"`, exit 0. The merger sees a normal `RUN_FULL` and proceeds to step 5.

## Entry criteria

- [P0488](plan-0488-merger-clause4-wiring.md) merged (`clause4-check` CLI subcommand + `FastPathVerdict` model exist)

## Tasks

### T1 — `fix(tooling):` wrap clause4_check dispatch in try/except → RUN_FULL on error

MODIFY [`.claude/lib/onibus/cli.py:340-347`](../lib/onibus/cli.py). Wrap the `merge.clause4_check(args.base)` call:

```python
if c == "clause4-check":
    if args.schema:
        _schema_exit(_MODELS["FastPathVerdict"])
    try:
        v = merge.clause4_check(args.base)
    except Exception as e:
        # Fail-safe: crash in the optimizer degrades to full CI, never
        # skips it. The merger sees a normal RUN_FULL and proceeds to
        # step 5. reason carries the exception for the merge report.
        v = FastPathVerdict(
            decision="RUN_FULL",
            reason=f"clause4-check crashed: {type(e).__name__}: {e}",
        )
    _emit(v)
    return 3 if v.decision == "HALT" else 0
```

The `return 3` change is [P0304](plan-0304-trivial-batch-p0222-harness.md) (the batch task)'s HALT exit-code fix — coordinate at dispatch: if P0304 HALT-exit-code task landed first, this edit preserves the `3`; if this lands first, P0304 HALT-exit-code task is a no-op on this line.

### T2 — `fix(tooling):` merger step 4.5 — explicit unknown-decision → RUN_FULL fallthrough

MODIFY [`.claude/agents/rio-impl-merger.md:86-96`](../agents/rio-impl-merger.md). Add a fourth table row + a defensive bash guard after the `jq` extraction:

```bash
verdict=$(.claude/bin/onibus merge clause4-check "<pre_merge>")
decision=$(jq -r '.decision // "RUN_FULL"' <<<"$verdict" 2>/dev/null || echo RUN_FULL)
```

The `// "RUN_FULL"` jq default + `|| echo RUN_FULL` double-guard means: malformed JSON → `RUN_FULL`, missing `.decision` key → `RUN_FULL`, empty stdout → `RUN_FULL`. Add a table row:

| `decision` | Action |
|---|---|
| *anything else* | Treat as `RUN_FULL`. `clause4-check` crashed or emitted an unknown verdict. Log `.reason` (if present) to `failure_detail` and proceed to step 5. Never skip CI on an unrecognized verdict. |

T1 (CLI-side catch) and T2 (merger-side guard) are belt-and-suspenders: T1 catches Python exceptions, T2 catches the case where the subprocess itself dies (OOM-kill, signal) before Python's exception handler runs.

### T3 — `test(tooling):` clause4_check crash → RUN_FULL verdict

MODIFY [`.claude/lib/test_scripts.py`](../lib/test_scripts.py) after the existing `_clause4_fixture` helper at `:962`. Add a test that monkeypatches `merge.clause4_check` to raise, then invokes the CLI dispatch:

```python
def test_clause4_crash_degrades_to_run_full(tmp_path, monkeypatch, capsys):
    import onibus.merge as m
    monkeypatch.setattr(m, "clause4_check",
                        lambda base: (_ for _ in ()).throw(RuntimeError("boom")))
    rc = _dispatch(["merge", "clause4-check", "abc123"])
    assert rc == 0  # RUN_FULL is rc=0, not HALT's rc=3
    out = capsys.readouterr().out
    v = FastPathVerdict.model_validate_json(out)
    assert v.decision == "RUN_FULL"
    assert "crashed" in v.reason and "RuntimeError" in v.reason
```

This is the crash-path test the P0488 review flagged as missing ([P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) (the batch task) is the batch-append tracking the test-gap; this plan CLOSES it — coordinate at dispatch: if P0311 crash-path-test task landed first as a standalone test-add, this T3 is a no-op).

## Exit criteria

- `clause4-check` CLI dispatch wraps `merge.clause4_check()` in try/except → `RUN_FULL` verdict on exception
- `rio-impl-merger.md` step 4.5 has `jq -r '.decision // "RUN_FULL"'` default + `|| echo RUN_FULL` guard
- `rio-impl-merger.md` decision table has an "anything else → RUN_FULL" row
- `pytest .claude/lib/test_scripts.py -k clause4_crash` → ≥1 test green
- Manually: `monkeypatch clause4_check to raise → CLI emits valid FastPathVerdict JSON with decision=RUN_FULL`

## Tracey

No domain markers. This is tooling (`.claude/` agent protocol + CLI), which has no `docs/src/components/` spec. The clause-4 fast-path is documented inline at [`rio-impl-merger.md`](../agents/rio-impl-merger.md) and the skills, not in the design book.

## Files

```json files
[
  {"path": ".claude/lib/onibus/cli.py", "action": "MODIFY", "note": "T1: try/except wrap at :340-347 → RUN_FULL on exception"},
  {"path": ".claude/agents/rio-impl-merger.md", "action": "MODIFY", "note": "T2: jq default + fallthrough table row at :86-96"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T3: clause4_crash_degrades_to_run_full test after :962"}
]
```

```
.claude/
├── lib/
│   ├── onibus/cli.py       # T1 try/except wrap
│   └── test_scripts.py     # T3 crash-path test
└── agents/
    └── rio-impl-merger.md  # T2 jq default + table row
```

## Dependencies

```json deps
{"deps": [488], "soft_deps": [304, 311], "note": "P0488 (merging/DONE) shipped clause4-check CLI + FastPathVerdict. Soft-dep P0304 HALT-exit-code task (HALT exit-code 1→3) touches same cli.py:347 line — coordinate at dispatch. Soft-dep P0311 crash-path-test task (crash-path test-gap) is CLOSED by this plan's T3. discovered_from=488."}
```

**Depends on:** [P0488](plan-0488-merger-clause4-wiring.md) — `clause4-check` subcommand, `FastPathVerdict` model, merger step 4.5 section all arrive with it.
**Conflicts with:** `.claude/lib/onibus/cli.py:347` — [P0304](plan-0304-trivial-batch-p0222-harness.md) (the batch task) changes the same return statement (exit 1→3). Both edits are compatible; whichever lands second adjusts. `.claude/lib/test_scripts.py` — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) multiple T-items append tests; all additive, zero name collisions.
