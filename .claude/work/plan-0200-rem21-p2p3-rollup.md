# Plan 0200: Rem-21 — P2/P3 rollup: 6 batches (A scheduler, B worker, C store, D common, E config, F docs)

## Design

~45 P2/P3 findings batched by component for review+test locality. Each batch is one commit.

**Batch A (`58df18f`, scheduler):** `failed_workers` dedup (was appending duplicates on reassign); tenant name trim in one more site; `reconcile_errors_total` metric registration; `spawn_monitored` for one missed `tokio::spawn`.

**Batch B (`647c86e`, worker):** reject unfiltered paths (candidate set must be complete before scan); `fod_flag` from drv's `outputHashMode` not heuristic.

**Batch C (`b8639ab`, store):** pre-register drain gauges (items 1-3 already done in rem-19; this is the remainder).

**Batch D (`58a29e8`, common):** redact `rfind` (secrets-in-URL leak); otel empty/nan guards; log-format case-insensitive.

**Batch E (`ac8dda1`, config):** worker knobs in `WorkerPoolSpec`; zero-interval guard (panic on 0s poll); lease via figment.

**Batch F (`946ec32`, docs):** `proto.md` sync; remediation report corrections.

**Rider (`b70b9dc`):** one missed item from Batch A.

**Partial landing — §Remainder items deferred:**
- `ctrl-connect-store-outside-main` (held for rem-03) — NOT landed; `connect_store` still per-reconcile at `rio-controller/src/reconcilers/build.rs:220`.
- `pg-dual-migrate-race` (held for rem-12) — NOT landed; no `skip_migrations` flag found.
- `pg-zero-compile-time-checked-queries` — `TODO(phase4b)` at `db.rs:9` (same as rem-12 §6).
- `worker.fuse.passthrough` `#[ignore]` r[verify] stub — NOT found; still UNTESTED at phase-4a.
- P3 code-quality items (module splits, `.unwrap()`→`.expect()` sweep, `describe_all!` macro) — `TODO(phase4b)`.

**Cross-references that landed ELSEWHERE (not in this plan's commits):**
- `gw-temp-roots-unbounded` (held for rem-07) → landed in **rem-18** (`9ef0fdc`, P0197): `temp_roots` HashSet deleted entirely.
- `gw-submit-build-bare-question-mark-no-stderr` (held for rem-07) → landed in **rem-20** (`4775759`, P0199): `STDERR_NEXT` before `return Err`.

Remediation doc: `docs/src/remediations/phase4a/21-p2-p3-rollup.md` (287 lines, shortest — index + §Remainder).

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "failed_workers dedup"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "spawn_monitored for missed spawn"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "tenant trim; reconcile metric"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "tenant trim rider"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "reconcile_errors_total register"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "reject unfiltered paths; fod_flag from drv"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "pre-register drain gauges"},
  {"path": "rio-common/src/config.rs", "action": "MODIFY", "note": "redact rfind; log-format case-insensitive"},
  {"path": "rio-common/src/observability.rs", "action": "MODIFY", "note": "otel empty/nan guards"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "redact helper"},
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "MODIFY", "note": "worker knobs in WorkerPoolSpec"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "zero-interval guard"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "knob plumbing"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "knob tests"},
  {"path": "rio-scheduler/src/lease/mod.rs", "action": "MODIFY", "note": "lease config via figment"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "lease figment wiring"},
  {"path": "rio-worker/src/config.rs", "action": "MODIFY", "note": "config validation"},
  {"path": "infra/helm/crds/workerpools.rio.build.yaml", "action": "MODIFY", "note": "WorkerPoolSpec schema"},
  {"path": "docs/src/components/proto.md", "action": "MODIFY", "note": "sync"},
  {"path": "docs/src/remediations/phase4a.md", "action": "MODIFY", "note": "corrections"}
]
```

## Tracey

No new markers — P2/P3 cleanup, no new spec behaviors.

## Entry

- Depends on P0180..P0199: all prior remediations (rollup sweeps what they left)

## Exit

Merged as `1fcd30c` (plan doc) + 7 batch commits: `58df18f`, `647c86e`, `b8639ab`, `58a29e8`, `ac8dda1`, `946ec32`, `b70b9dc`. `.#ci` green. **Partial:** §Remainder deferred as documented; 7× `TODO(phase4b)` comments track.
