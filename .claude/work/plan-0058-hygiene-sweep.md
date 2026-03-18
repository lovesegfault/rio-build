# Plan 0058: Post-2a hygiene sweep + metric label standardization

## Design

Eight small fixes accumulated during phase-2a review that didn't block the milestone but needed addressing before building on top. Each is individually tiny; collectively they remove noise that would otherwise contaminate every diff in the rest of phase-2b.

The headline fix is metric label standardization (`7a3aead`): `observability.md` was self-contradictory — the scheduler spec said `outcome="succeeded"/"failed"`, the worker spec said `outcome="success"/"failure"`, and the SLI formulas referenced `outcome=success` for both components (which would never match the scheduler's actual emitted label). Prometheus queries needed per-component translation. Standardized on `success`/`failure` everywhere; the SLI formulas are now correct as written. This also updated the phase-2a VM test's assertion for the renamed scheduler label.

The other load-bearing fix is the scheduler O(n²) merge (`df593a3`): `MergeResult.newly_inserted` was `Vec<String>`, causing O(n) `.contains()` inside per-node loops in `persist_merge_to_db`, `handle_merge_dag`, and `dag::merge`. For a 1000-node DAG that's ~1M string comparisons inside the single-threaded actor, blocking all heartbeats/completions/dispatches during the merge. `HashSet<String>` makes every `.contains()` O(1). This commit also planted the `TODO(phase2b)` for the batch-DB-write optimization that P0061 later implements.

The rest: `WorkerState::new` constructor extraction (was inlined identically twice), eager `base_dir` creation in `FilesystemBackend::new` (fail fast, not on first write), adopt `rio_nix::nar::serialize` in tests instead of hand-rolled NAR bytes, worker TOCTOU fix (`exists()` before `read_dir()` — wasted stat + race), removal of unused `root_span` helper (all four binaries inline `info_span!` with component name as span name, not "root").

## Files

```json files
[
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "HashSet for newly_inserted; TODO(phase2b) batch-DB marker"},
  {"path": "rio-scheduler/src/dag.rs", "action": "MODIFY", "note": "HashSet adoption in merge"},
  {"path": "rio-scheduler/src/state.rs", "action": "MODIFY", "note": "WorkerState::new constructor"},
  {"path": "rio-store/src/backend/filesystem.rs", "action": "MODIFY", "note": "eager base_dir creation in ::new"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "adapt to eager-mkdir constructor"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "use rio_nix::nar::serialize"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "remove TOCTOU exists() before read_dir()"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "dedupe to_string_lossy in ENOENT hot path"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "remove vestigial #[allow(too_many_arguments)]"},
  {"path": "rio-common/src/observability.rs", "action": "MODIFY", "note": "remove unused root_span helper"},
  {"path": "rio-gateway/src/session.rs", "action": "MODIFY", "note": "metric outcome label success/failure"},
  {"path": "nix/tests/phase2a.nix", "action": "MODIFY", "note": "update VM assertion for renamed scheduler metric label"},
  {"path": "rio-scheduler/Cargo.toml", "action": "MODIFY", "note": "minor dep hygiene"},
  {"path": "rio-store/Cargo.toml", "action": "MODIFY", "note": "minor dep hygiene"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. The metric-label fix would retroactively land under `r[obs.metrics.outcome]` (standardized labels spec).

## Entry

- Depends on **P0056** (phase-2a terminal): all touched files are 2a artifacts (`actor.rs`, `fuse/mod.rs`, `upload.rs`, `phase2a.nix`).

## Exit

Merged as `df593a3..7a3aead` (8 commits). `.#ci` green at merge; `vm-phase2a` assertion updated for renamed scheduler label and passing.
