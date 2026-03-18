# Plan 0244: Doc-sync sweep + TODO retags + phase [x]

phase4c.md:66-85 — **phase closeout.** Four mechanical sweeps: (1) residual deferral-block removal (implementing plans already closed their own — this catches 4b-implemented + already-done-in-prior-phase residuals); (2) `cgroup.rs:301` comment improvement (keep `TODO(phase5)` tag per A5); (3) `admin/builds.rs:22` retag `TODO(phase4c)` → `TODO(phase5)` per A4 (cursor pagination deferred); (4) `phase4{a,b,c}.md` all `[ ]` → `[x]`.

**Line numbers in phase doc are STALE** (GT9 verified: observability.md drift from `:116,166,269` to `:117,:185,:303`). **Grep for content strings, not line numbers:**

```bash
grep -rn 'Phase 4 deferral\|Not implemented (Phase 4)\|(Phase 4+)\|deferred to Phase 4' docs/src/
```

Each hit needs a decision: (a) closed by 4b/4c → remove; (b) actually Phase 5 → retag; (c) legitimate unrelated mention → leave.

## Entry criteria

All of P0220-P0243 merged. Practically: deps on leaf-tips [P0237, P0239, P0241, P0243, P0238, P0240, P0242, P0221, P0222, P0224, P0225, P0226] — transitive closure covers everything.

## Tasks

### T1 — `docs:` residual deferral-block sweep

For each file, grep for the deferral text and verify the implementing plan closed it. Remaining hits are either (a) 4b-implemented deferrals the 4b plan forgot to close, or (b) already-done-in-prior-phase that never got cleaned up.

| File | Text to find | Resolution |
|---|---|---|
| [`errors.md:97`](../../docs/src/errors.md) | "Not implemented (Phase 4)" re: per-build timeout | 4b P0214 implements → verify merge, remove line |
| [`multi-tenancy.md`](../../docs/src/multi-tenancy.md) `:70,:111,:114` | "Phase 4" tenant deferrals | 4b P0206/P0207 → verify, remove |
| [`observability.md`](../../docs/src/observability.md) | `'(Phase 4+)'` | Grep fresh (line# drifted per GT9); decide per hit |
| [`scheduler.md`](../../docs/src/components/scheduler.md) `:384,:392,:398,:413` | tenant FK deferrals | 4a/4b implemented → verify, remove |
| [`worker.md`](../../docs/src/components/worker.md) `:190,:194` | NAR scanner deferrals | 4b implements → verify, remove |
| [`challenges.md`](../../docs/src/challenges.md) `:73,:100` | FUSE breaker deferrals (`:138` done by P0234) | 4b P0209 implements → verify, remove |
| [`failure-modes.md`](../../docs/src/failure-modes.md) `:52` | FUSE deferral | 4b → verify, remove |
| [`verification.md`](../../docs/src/verification.md) `:10,:83,:94,:68-73` | Phase 4 status | Consolidate: mark all 4{a,b,c} items done |

**Process per hit:**
1. `git log --all --oneline -S'<deferral text>'` — find the commit that should have removed it
2. If found: the implementing plan forgot; remove the line here
3. If not found: either (a) the text was never tracked and is genuinely stale, or (b) it's legitimately Phase 5 — retag

### T2 — `docs(worker):` cgroup.rs comment improvement (keep TODO(phase5))

MODIFY [`rio-worker/src/cgroup.rs:301`](../../rio-worker/src/cgroup.rs) — per A5+GT10, the TODO is already `TODO(phase5)`. Improve the comment to explain WHY it's phase5 and what would test it:

```rust
// TODO(phase5): sibling-cgroup cleanup on daemon restart.
// UNREACHABLE under `privileged: true` — the sibling cgroup only
// exists when the daemon runs in a separate cgroup from the worker,
// which requires device-plugin mode (ADR-012). When ADR-012's
// device-plugin VM test lands, THAT test covers this path.
// No #[ignore] stub here: testing unreachable code is noise.
```

No `#[ignore]` test stub per A5 — code is unreachable under current deployment; testing it is noise.

### T3 — `refactor(scheduler):` admin/builds.rs retag

MODIFY [`rio-scheduler/src/admin/builds.rs:22`](../../rio-scheduler/src/admin/builds.rs) — per A4 (cursor pagination deferred):

```rust
// Before: TODO(phase4c): cursor pagination
// After:  TODO(phase5): cursor pagination — needs proto field + keyset query.
//         Deferred from 4c per A4: not in phase4c.md task list.
```

### T4 — `docs:` contributing.md phase retag

MODIFY [`docs/src/contributing.md:85`](../../docs/src/contributing.md) — retag any phase4 mention → phase5.

### T5 — `docs:` phase4{a,b,c}.md mark all [x]

MODIFY `docs/src/phases/phase4a.md`, `phase4b.md`, `phase4c.md` — every `- [ ]` → `- [x]`. Sed is fine here:

```bash
sed -i 's/- \[ \]/- [x]/g' docs/src/phases/phase4{a,b,c}.md
```

Verify manually: `grep '\[ \]' docs/src/phases/phase4*.md` → empty.

### T6 — `test:` final tracey validate

```bash
nix develop -c tracey query validate
```

MUST show 0 errors. The 8 new 4c markers are all in place (P0223, P0224, P0226, P0229, P0230, P0233, P0234 × 2).

`ctrl.wps.reconcile` may show 1 verify (P0239 VM only) or 2 (if P0233 also added a unit-level verify). Either is fine.

## Exit criteria

- `/nbr .#ci` green
- `grep -r 'Phase 4 deferral\|Not implemented (Phase 4)\|(Phase 4+)' docs/src/` → only legitimate Phase 5 mentions remain
- `grep 'TODO(phase4' rio-*/src/` → empty (all retagged or resolved)
- `grep '\[ \]' docs/src/phases/phase4*.md` → empty
- `tracey query validate` → 0 errors
- `tracey query status` → all 8 phase4c markers show impl+verify

## Tracey

No new markers. This plan RUNS `tracey query validate` as its exit gate — verifies all markers added by P0223/P0224/P0226/P0229/P0230/P0233/P0234 are clean.

## Files

```json files
[
  {"path": "docs/src/errors.md", "action": "MODIFY", "note": "T1: remove :97 per-build-timeout deferral (P0214 implemented)"},
  {"path": "docs/src/multi-tenancy.md", "action": "MODIFY", "note": "T1: remove :70,:111,:114 tenant deferrals (4b implemented)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T1: grep '(Phase 4+)' fresh — remove/retag per hit"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T1: remove :384,:392,:398,:413 tenant-FK deferrals"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "T1: remove :190,:194 NAR-scanner deferrals"},
  {"path": "docs/src/challenges.md", "action": "MODIFY", "note": "T1: remove :73,:100 FUSE-breaker deferrals"},
  {"path": "docs/src/failure-modes.md", "action": "MODIFY", "note": "T1: remove :52 FUSE deferral"},
  {"path": "docs/src/verification.md", "action": "MODIFY", "note": "T1: consolidate :10,:83,:94,:68-73 Phase 4 status"},
  {"path": "docs/src/contributing.md", "action": "MODIFY", "note": "T4: retag :85 phase4→phase5"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "T2: improve :301 comment (keep TODO(phase5) tag; explain unreachable + ADR-012)"},
  {"path": "rio-scheduler/src/admin/builds.rs", "action": "MODIFY", "note": "T3: retag :22 TODO(phase4c)→TODO(phase5)"},
  {"path": "docs/src/phases/phase4a.md", "action": "MODIFY", "note": "T5: all [ ]→[x]"},
  {"path": "docs/src/phases/phase4b.md", "action": "MODIFY", "note": "T5: all [ ]→[x]"},
  {"path": "docs/src/phases/phase4c.md", "action": "MODIFY", "note": "T5: all [ ]→[x]"}
]
```

```
docs/src/
├── errors.md, multi-tenancy.md, observability.md, challenges.md,
│   failure-modes.md, verification.md, contributing.md   # T1,T4: deferral sweep
├── components/{scheduler,worker}.md                     # T1
└── phases/phase4{a,b,c}.md                              # T5: [x]
rio-worker/src/cgroup.rs                                 # T2: comment only
rio-scheduler/src/admin/builds.rs                        # T3: 1-line retag
```

## Dependencies

```json deps
{"deps": [237, 239, 241, 243, 238, 240, 242, 221, 222, 224, 225, 226], "soft_deps": [], "note": "PHASE CLOSEOUT — LAST plan. Deps on all leaf-tips (transitive covers P0220-P0243). Line numbers in phase doc are STALE — grep content strings."}
```

**Depends on:** ALL of P0220-P0243. Listed deps are the leaf-tips; transitive closure covers the rest.
**Conflicts with:** none — LAST plan, everything else merged.

**Hidden check at dispatch:** for each deferral-block hit, `git log --all --oneline -S'<text>'` confirms whether the implementing plan removed it. Don't blindly delete — some "Phase 4" mentions are legitimate historical context (e.g., "this was added in Phase 4") not deferrals.
