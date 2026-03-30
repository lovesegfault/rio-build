# Plan 983457302: Wire clause4_check + record_green_ci_hash into merger workflow

[P0479](plan-0479-merger-clause4-fastpath-harden.md) shipped
`clause4_check()` and `record_green_ci_hash()` at
[`.claude/lib/onibus/merge.py:537,577`](../lib/onibus/merge.py) — but the
merger skill/agent never invokes them. `grep clause4 .claude/skills/merge-impl.md
.claude/agents/rio-impl-merger.md` returns zero hits. The hardening is
inert: the fast-path gate exists, nobody calls it.

What the wiring should look like: post-ff-merge, pre-`.#ci`, the merger
calls `onibus merge clause4-check <pre-merge-ref>`. On `SKIP` verdict →
skip `.#ci` entirely (hash-identical to last green, nothing observable
changed). On `RUN_FULL` → run `.#ci` as before. On `HALT` → the function
already calls `halt_queue()` internally; merger should report and exit
non-zero. Post-green-`.#ci`, merger calls `onibus merge record-green` so
the NEXT clause4-check has a baseline.

The `clause4_check` CLI subcommand may not exist yet (the functions do;
check `onibus merge --help` at dispatch). If missing, add a thin
`@app.command` wrapper in `merge.py`.

## Entry criteria

- [P0479](plan-0479-merger-clause4-fastpath-harden.md) merged (`clause4_check` + `record_green_ci_hash` functions exist)

## Tasks

### T1 — `feat(tooling):` onibus merge clause4-check + record-green CLI subcommands

MODIFY [`.claude/lib/onibus/merge.py`](../lib/onibus/merge.py). If
`clause4_check` and `record_green_ci_hash` aren't already `@app.command`
wrapped, add:

```python
@app.command("clause4-check")
def clause4_check_cmd(base: str):
    """Fast-path gate: SKIP | RUN_FULL | HALT verdict."""
    v = clause4_check(base)
    print(json.dumps(asdict(v)))
    sys.exit(0 if v.decision != "HALT" else 1)

@app.command("record-green")
def record_green_cmd():
    """Record current .#ci drv-hash as last-known-green."""
    h = record_green_ci_hash()
    print(h or "eval-failed")
```

Check the existing `@app.command` pattern in `merge.py` for consistency
(typer vs click vs argparse — match whatever's there).

### T2 — `feat(skill):` merge-impl.md — invoke clause4-check pre-.#ci

MODIFY [`.claude/skills/merge-impl.md`](../skills/merge-impl.md). After
the ff-merge step, before the `.#ci` gate, insert:

```bash
verdict=$(.claude/bin/onibus merge clause4-check "$PRE_MERGE_REF" | jq -r .decision)
case "$verdict" in
  SKIP) echo "clause-4 SKIP: hash-identical to last green, skipping .#ci" ;;
  HALT) echo "clause-4 HALT: new tests red, queue halted"; exit 1 ;;
  *)    /nixbuild .#ci || exit 1 ;;  # RUN_FULL or unknown → full gate
esac
```

After a green `.#ci` (or a SKIP), insert:

```bash
.claude/bin/onibus merge record-green
```

### T3 — `feat(agent):` rio-impl-merger.md — document clause4 step

MODIFY [`.claude/agents/rio-impl-merger.md`](../agents/rio-impl-merger.md).
Add a section documenting the clause-4 fast-path: what SKIP/RUN_FULL/HALT
mean, when record-green fires, and that a HALT writes the queue-halted
sentinel (coordinator must `onibus merge clear-halt` after root-causing).

### T4 — `test(tooling):` clause4 CLI roundtrip — SKIP/RUN_FULL/HALT verdicts

MODIFY [`.claude/lib/test_scripts.py`](../lib/test_scripts.py). Add tests
that exercise `clause4_check_cmd` via subprocess:
- docs-only diff + matching last-green-hash → SKIP
- rio-*/src change → RUN_FULL
- new `#[test]` attr that fails → HALT (and `queue_halted()` returns non-None)

Use a temp git repo fixture or mock `_ci_drv_hash`/`_diff_files`.

## Exit criteria

- `onibus merge clause4-check <ref>` emits JSON with `decision` ∈ {SKIP, RUN_FULL, HALT}
- `onibus merge record-green` writes `_LAST_GREEN_HASH` file
- `grep clause4 .claude/skills/merge-impl.md` → ≥2 hits (pre-gate check + post-green record)
- `grep clause4 .claude/agents/rio-impl-merger.md` → ≥1 hit (documented)
- `pytest .claude/lib/test_scripts.py -k clause4` → green
- End-to-end: a docs-only merge through `/merge-impl` emits "clause-4 SKIP" and does NOT run `.#ci`

## Tracey

No domain markers — tooling-only (`.claude/` is excluded from tracey scan
via `fileset.difference` at [`flake.nix:642`](../../flake.nix)).

## Files

```json files
[
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T1: @app.command wrappers for clause4-check + record-green"},
  {"path": ".claude/skills/merge-impl.md", "action": "MODIFY", "note": "T2: invoke clause4-check pre-.#ci, record-green post"},
  {"path": ".claude/agents/rio-impl-merger.md", "action": "MODIFY", "note": "T3: document clause4 step + HALT semantics"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T4: clause4 CLI roundtrip tests"}
]
```

```
.claude/
├── lib/onibus/merge.py       # T1: CLI wrappers
├── lib/test_scripts.py       # T4: clause4 tests
├── skills/merge-impl.md      # T2: wiring
└── agents/rio-impl-merger.md # T3: docs
```

## Dependencies

```json deps
{"deps": [479], "soft_deps": [304], "note": "P0479 shipped the functions; this wires them. Soft-dep P0304-T261 (also touches merge.py _next_real — different section :505-513 vs :537+, non-overlapping). discovered_from=479."}
```

**Depends on:** [P0479](plan-0479-merger-clause4-fastpath-harden.md) — `clause4_check` + `record_green_ci_hash` exist.
**Conflicts with:** [`.claude/lib/onibus/merge.py`](../lib/onibus/merge.py) touched by P0304-T261 (`_next_real` at `:505-513`) and this plan's T983457302 in the P0304 batch (`clear_halt` at `:492-503`) — all non-overlapping sections. `test_scripts.py` is append-only test additions, low collision.
