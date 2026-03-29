# Plan 484: TOOLING — add cov-smoke to merger .#ci (PSA break went 118 commits undetected)

The merger's `.#ci` gate does NOT include `.#coverage-full` — it's backgrounded (see [`merge.py:56`](../../.claude/lib/onibus/merge.py)) and the result is consumed by `/dag-tick` as a non-blocking `test-gap` followup. A PSA break went **118 commits** undetected because it only surfaces in VM coverage runs, and coverage-pending entries were triaged as "investigate later" test-gaps instead of queue-halts.

Three options, in order of preference:

- **(c) Fast smoke:** add one `cov-vm-*` smoke to `.#ci` itself. Picks one representative VM scenario, runs it in coverage mode, ~5min. Catches "coverage infrastructure broken" without the full 25min cost. **Preferred — ship this.**
- **(b) Cadence with queue-halt:** `.#coverage-full` runs every Nth merge; on red, writes `.claude/state/queue-halted` (same sentinel [P0479](plan-0479-merger-clause4-fastpath-harden.md) T3 introduces). Catches breaks within a merge-cycle instead of 118 commits. **Ship alongside (c) as belt-and-braces.**
- **(a) Add to merger critical path:** `.#coverage-full` (~25min) blocking every merge. Rejected: doubles merge time, serializes the queue on a mostly-green check.

Discovered via coverage-sink. Related to [P0479](plan-0479-merger-clause4-fastpath-harden.md) (Clause-4 fast-path hardening — same "merger let bad state through" class, different axis).

## Entry criteria

- [P0479](plan-0479-merger-clause4-fastpath-harden.md) merged (introduces `.claude/state/queue-halted` sentinel + `/dag-run` pre-dispatch check — option (b) here reuses both)

## Tasks

### T1 — `feat(nix):` add `.#cov-smoke` — one-scenario coverage smoke for `.#ci`

MODIFY [`nix/coverage.nix`](../../nix/coverage.nix) — add a `cov-smoke` attribute that runs `cov-vm-protocol-warm-standalone` (or whichever single scenario exercises the PSA break path) and produces a pass/fail. Wire it into `.#ci`'s constituent list in [`flake.nix`](../../flake.nix).

Target: ~5min. Picks the scenario that exercises store+scheduler+gateway together (broadest coverage-infrastructure surface per minute).

### T2 — `feat(tooling):` cadence `.#coverage-full` with queue-halt on infrastructure-red

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) — the existing backgrounded `.#coverage-full` already writes to `coverage-pending.jsonl`; extend to ALSO write `queue-halted` when the failure is infrastructure-class (all VM scenarios fail) vs test-gap (one scenario fails). Heuristic: ≥3 scenarios fail → infrastructure break → halt.

Invoke via the existing cadence mechanism (merge-count modulo, same pattern as consolidator/bughunter — every Nth merge).

### T3 — `test(tooling):` cov-smoke catches the PSA-break class

Verify T1's smoke scenario actually exercises the PSA path. Reproduce the 118-commit-old break in a branch, run `.#cov-smoke`, assert red. If the smoke doesn't catch it, pick a different scenario.

### T4 — `docs(tooling):` update project instructions coverage policy

MODIFY the top-level project instructions doc §Coverage policy — document that `.#cov-smoke` is now in `.#ci` (blocking), `.#coverage-full` is still backgrounded but now queue-halts on infrastructure-class failure (≥3 scenarios red).

## Exit criteria

- `nix build .#cov-smoke` completes in <8min
- `.#ci` constituent list includes `cov-smoke` (`ls result/ | grep cov-smoke` after `nix build .#ci`)
- `grep 'queue-halted\|coverage-full-red' .claude/lib/onibus/merge.py` → ≥2 hits (T2 sentinel write)
- T3 repro: branch with PSA break → `nix build .#cov-smoke` → red

## Tracey

No domain markers — tooling change, `.claude/` + `nix/` infra only. No `docs/src/components/` behavior described.

## Files

```json files
[
  {"path": "nix/coverage.nix", "action": "MODIFY", "note": "T1: cov-smoke attribute"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T1: wire cov-smoke into .#ci"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T2: infra-failure heuristic writes queue-halted"},
  {"path": "CLAUDE.md", "action": "MODIFY", "note": "T4: coverage policy update"}
]
```

```
nix/coverage.nix            # T1: cov-smoke attr
flake.nix                   # T1: .#ci wiring
.claude/lib/onibus/merge.py # T2: infra-failure → halt
```

## Dependencies

```json deps
{"deps": [479], "soft_deps": [], "note": "479 introduces .claude/state/queue-halted sentinel + /dag-run pre-dispatch check. T2 here writes to the same sentinel. Without 479, T2 would need to invent the halt mechanism itself."}
```

**Depends on:** [P0479](plan-0479-merger-clause4-fastpath-harden.md) — `queue-halted` sentinel + `/dag-run` check.
**Conflicts with:** [`merge.py`](../../.claude/lib/onibus/merge.py) — P0479 T1-T3 also modify. Serialize: P0479 first (introduces halt mechanism), P0484 second (extends it). [`flake.nix`](../../flake.nix) — moderate traffic; T1's `.#ci` constituent edit is a one-line append, low conflict risk.
