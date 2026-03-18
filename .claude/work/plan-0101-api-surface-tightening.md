# Plan 0101: API surface tightening — visibility + small extractions

## Design

Nine small refactor commits tightened module visibility and extracted cohesive helpers — the kind of cleanup that accumulates as "I'll do this later" comments. The goal: make the public API surface intentional, not incidental.

`c815722` demoted the actor-satellite modules (`critical_path`, `estimator`, `queue`, `assignment`) from `pub` to `pub(crate)` — they're used only by `actor/*` internally; `main.rs` reaches `SizeClassConfig` via a crate-level re-export. The demotion surfaced dead code: `Estimator::{len, is_empty}` were test-only (now `cfg(test)`), `ReadyQueue::is_empty` was wholly unused (deleted). `d61f62a` did the same for `rio-store`'s `pub mod` surface. `66ee825` tightened remaining visibility and gated `SchedulerGrpc::new` behind `cfg(test)` (production uses the builder).

`af8c950` extracted the worker's business logic from `lib.rs` into a `runtime` module: `build_heartbeat_request`, `BuildSpawnContext`, `spawn_build_task`, `BloomHandle` and their 5 tests moved; `lib.rs` became pure mod declarations + re-exports. `main.rs` imports unchanged via re-exports. This extraction prepared for P0108 (SIGTERM drain) which needed to extend the runtime loop without fighting a 400-line `lib.rs`.

`c3683bc` flattened tonic client type re-exports in `rio-proto`: callers write `rio_proto::StoreServiceClient` instead of `rio_proto::store::store_service_client::StoreServiceClient`. `60fa259` moved `RIO_DAEMON_TIMEOUT_SECS` from a raw env read to figment config (consistent with every other timeout). `0d06f9e`, `7d11ad3`, `03e7119` extracted small helpers (`update_narinfo`, `narinfo_cols`, `validate_row`, `node_common_fields`, `HeaderValue::from_static` for compile-time header validation) from functions that had grown past a screenful.

## Files

```json files
[
  {"path": "rio-worker/src/runtime.rs", "action": "NEW", "note": "extracted from lib.rs: build_heartbeat_request, BuildSpawnContext, spawn_build_task, BloomHandle + 5 tests"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "reduced to mod declarations + re-exports"},
  {"path": "rio-proto/src/client.rs", "action": "MODIFY", "note": "flattened re-exports: rio_proto::StoreServiceClient not the full tonic path"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "actor-satellite modules demoted to pub(crate); SizeClassConfig re-exported at crate level"},
  {"path": "rio-scheduler/src/estimator.rs", "action": "MODIFY", "note": "len/is_empty gated cfg(test)"},
  {"path": "rio-scheduler/src/queue.rs", "action": "MODIFY", "note": "ReadyQueue::is_empty deleted (wholly unused)"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "pub mod surface tightened"},
  {"path": "rio-store/src/metadata/queries.rs", "action": "MODIFY", "note": "update_narinfo + narinfo_cols + validate_row extracted"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "node_common_fields helper extracted"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "RIO_DAEMON_TIMEOUT_SECS moved to figment config"},
  {"path": "rio-common/src/grpc.rs", "action": "MODIFY", "note": "timeout constant cleanup"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). No spec requirement covers visibility — this is internal code organization.

## Entry

- Depends on P0097: dead-code removal first; surface-tightening on a smaller codebase found more leaks.

## Exit

Merged as `c815722..03e7119` (9 commits, non-contiguous). `.#ci` green at merge. Zero behavior change — `runtime.rs` is a pure move, re-exports preserve import paths. 846 tests unchanged.
