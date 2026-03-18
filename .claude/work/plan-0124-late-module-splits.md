# Plan 0124: Late module splits — workerpool/admin/worker-main

## Design

Three files born in this phase (P0112, P0106, P0108) had grown large enough to warrant the same split treatment P0102 gave the pre-phase files. Three commits, pure moves.

`49b05e6` split `rio-controller/src/reconcilers/workerpool.rs` (~1000 LOC after P0119/P0121 fixes) → `workerpool/{mod,builders,tests}.rs`. `mod.rs` holds `reconcile()` + `error_policy()` + finalizer drain; `builders.rs` holds `build_statefulset`, `build_container`, `build_probes` (the pure constructors); `tests.rs` holds the tower-test ApiServerVerifier tests.

`dd53025` split `rio-scheduler/src/admin.rs` → `admin/{mod,tests}.rs` + extracted `ActorCommand` variants into `actor/command.rs`. The admin handler was fighting for space with the command enum; separating them made P0125's Build-CRD exercise additions cleaner.

`b04f959` extracted `config.rs`, prefetch module, and drain logic from `rio-worker/src/main.rs` — `main.rs` had grown to ~400 LOC of mixed concern. `config.rs` holds the figment config struct + CLI args; the prefetch spawn logic and SIGTERM drain moved to dedicated functions. `main.rs` is now pure wiring.

**File-path continuity note:** Plans P0106, P0108, P0111, P0112, P0116, P0119, P0121, P0122, P0123 all touch the pre-split files. Their `## Files` sections use the final post-split paths (so `collisions-regen` works against the current tree). This plan is the resolution point: the paths those plans reference exist as of this merge.

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "NEW", "note": "split: reconcile, error_policy, finalizer drain"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "NEW", "note": "split: build_statefulset, build_container, build_probes (pure constructors)"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "NEW", "note": "split: tower-test ApiServerVerifier tests"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "NEW", "note": "split: ClusterStatus + DrainWorker handlers"},
  {"path": "rio-scheduler/src/admin/tests.rs", "action": "NEW", "note": "split: admin handler tests"},
  {"path": "rio-scheduler/src/actor/command.rs", "action": "NEW", "note": "extracted: ActorCommand enum variants"},
  {"path": "rio-worker/src/config.rs", "action": "NEW", "note": "extracted: figment Config struct + CliArgs"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "reduced to pure wiring; prefetch/drain moved to dedicated fns"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). The retroactive sweep placed module-header annotations on the post-split files (same as P0102's splits).

## Entry

- Depends on P0121: `workerpool.rs` had grown from P0112's 845 LOC to ~1000 after P0119/P0121's fixes; the split happened after those stabilized.
- Depends on P0108: the SIGTERM drain logic extracted here was added by P0108.
- Depends on P0106: the admin handlers split here were added by P0106.

## Exit

Merged as `49b05e6..b04f959` (3 commits, non-contiguous — `dd53025` and `b04f959` are interleaved with P0123/P0125). `.#ci` green at merge. Zero behavior change — pure moves with `mod.rs` re-exports.
