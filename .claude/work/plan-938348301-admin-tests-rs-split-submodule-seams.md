# Plan 938348301: admin/tests.rs split along P0383 submodule seams

rev-p383 flagged [`rio-scheduler/src/admin/tests.rs`](../../rio-scheduler/src/admin/tests.rs) at **1732L** — 2× the 861L trigger that motivated [P0383](plan-0383-admin-mod-split-plus-actor-alive-dedup.md) to split `admin/mod.rs`. P0383 extracted `logs.rs`/`gc.rs`/`tenants.rs` (new) alongside pre-existing `builds.rs`/`workers.rs`/`graph.rs`/`sizeclass.rs`; the monolithic tests module stayed monolithic. Tests that exercise `logs::get_build_logs` sit 1000+ lines from tests that exercise `graph::get_build_graph`, with zero structural seam to navigate.

Same E0063-magnet dynamic as [P0380](plan-0380-controller-workerpoolspec-test-fixture-dedup.md)'s fixture collapse: when a new RPC handler lands in e.g. `admin/tenants.rs`, the implementer must scroll ~750L into `tests.rs` to find where tenant tests cluster (`:758-914`). Grep works, but grep answers "where is the first hit" not "what's the full test surface for this submodule". The submodule seams exist in src; mirroring them in tests makes the test boundary discoverable at the filesystem layer.

**Related trajectory:** [`grpc/tests.rs`](../../rio-scheduler/src/grpc/tests.rs) is **1682L** (nearly identical size) post-[P0356](plan-0356-split-scheduler-grpc-service-impls.md). Not in scope here (rev-p383 flagged admin specifically), but the same split pattern applies — noted as a soft-dep for a future plan if the trajectory continues.

## Entry criteria

- [P0383](plan-0383-admin-mod-split-plus-actor-alive-dedup.md) merged — provides the `admin/{logs,gc,tenants}.rs` submodules this split mirrors + the `super::logs::{extract_drv_hash, gunzip_and_chunk}` import at [`tests.rs:1`](../../rio-scheduler/src/admin/tests.rs) (the logs tests already depend on post-split paths)

## Tasks

### T1 — `refactor(scheduler):` admin/tests/mod.rs — shared setup + test-plumbing

Convert `admin/tests.rs` → `admin/tests/mod.rs`. Keep the shared fixtures:

- `setup_svc` + `setup_svc_default` (`:24-60`, ~40L)
- `mk_batch` + `collect_stream` (`:62-75`, ~15L) — `mk_batch` is logs-specific but `collect_stream` is used by any streaming-RPC test; keep both here OR move `mk_batch` to T2's `logs_tests.rs`. Prefer: `collect_stream` stays shared, `mk_batch` moves.
- `seed_drv` + `seed_build` + `link` + `edge` (`:1404-1453`, ~50L) — graph test helpers. These are `async fn`s that seed PG rows; used by `get_build_graph_*` tests only. Move to T5's `graph_tests.rs`.

Post-extraction, `tests/mod.rs` retains:
- `use super::*;` plumbing + shared imports
- `setup_svc` / `setup_svc_default` / `collect_stream`
- Tests for handlers that **remain inline** in `admin/mod.rs` post-P0383: `cluster_status_*` (`:358-507`, 4 tests ~150L), `drain_worker_*` (`:508-611,:661-754`, 4 tests ~200L), `admin_rpcs_are_wired` (`:253-306`, ~55L smoke test), `test_clear_poison_*` (`:1195-1347`, 2 tests ~155L)
- `mod logs_tests; mod gc_tests; mod tenants_tests; mod builds_tests; mod graph_tests;` declarations

Target: ~600L (from 1732L).

### T2 — `refactor(scheduler):` admin/tests/logs_tests.rs — GetBuildLogs chain

Extract from `tests.rs`:

- `get_build_logs_from_ring_buffer` (`:78`)
- `get_build_logs_since_line_filters` (`:109`)
- `get_build_logs_from_s3_fallback` (`:135`)
- `get_build_logs_not_found_in_either` (`:216`)
- `get_build_logs_empty_drv_path_invalid` (`:235`)
- `extract_drv_hash_strips_store_prefix` (`:308`)
- `gunzip_and_chunk_roundtrip` (`:314`)
- `gunzip_and_chunk_since_filtering` (`:336`)
- `test_get_build_logs_invalid_uuid` (`:613`)
- `mk_batch` helper (moved from T1's shared block)

~310L total. Imports: `use super::*;` (pulls `setup_svc`, `collect_stream` from `mod.rs`) + `use crate::admin::logs::{extract_drv_hash, gunzip_and_chunk};` (already at `tests.rs:1` — move with the tests).

### T3 — `refactor(scheduler):` admin/tests/gc_tests.rs — TriggerGC chain

Extract:

- `test_trigger_gc_store_unreachable` (`:641`)
- `trigger_gc_forward_exits_on_shutdown` (`:1349`)

~75L total. The second test carries the `r[verify sched.admin.gc-shutdown-aware]` annotation if one exists — check at dispatch with `grep 'r\[verify' rio-scheduler/src/admin/tests.rs` around `:1348`. If present, the annotation moves with the test.

### T4 — `refactor(scheduler):` admin/tests/tenants_tests.rs — Create/List chain

Extract:

- `test_create_and_list_tenants` (`:758`) — carries `r[verify sched.admin.list-tenants]` + `r[verify sched.admin.create-tenant]` at `:755-756`

~160L total. Annotations move with the test fn (standalone comments immediately above `#[tokio::test]`).

### T5 — `refactor(scheduler):` admin/tests/graph_tests.rs — GetBuildGraph chain

Extract the largest standalone cluster (~330L):

- Helpers: `seed_drv` (`:1404`), `seed_build` (`:1425`), `link` (`:1434`), `edge` (`:1443`) — PG-seeding `async fn`s used only by graph tests
- `get_build_graph_basic_shape` (`:1456`)
- `get_build_graph_subgraph_scoping` (`:1527`)
- `get_build_graph_truncation` (`:1592`) — carries `r[verify dash.graph.degrade-threshold]` at `:1620`
- `get_build_graph_truncated_no_dangling_edges` (`:1635`)
- `get_build_graph_bad_uuid` (`:1703`)
- `get_build_graph_unknown_build_empty` (`:1716`)

### T6 — `refactor(scheduler):` admin/tests/builds_tests.rs — ListBuilds chain + workers

Extract:

- `test_list_builds_filter_and_pagination` (`:917`) — carries `r[verify sched.admin.list-builds]` at `:915`
- `test_list_builds_cross_tenant_isolation` (`:1039`) — carries `r[verify sched.admin.list-builds]` at `:1037` (second verify site for same marker — tracey accepts multiple verify sites)
- `test_list_workers_with_filter` (`:1125`) — carries `r[verify sched.admin.list-workers]` at `:1123`

~275L. Naming: `builds_tests.rs` carries the `list_workers` test too (75L, small to warrant its own file); alternatively `list_tests.rs`. Implementer's call — both cover list-RPCs.

## Exit criteria

- `/nixbuild .#ci` green (or nextest-standalone clause-4c)
- `wc -l rio-scheduler/src/admin/tests/mod.rs` → ≤650L (down from 1732L monolith)
- `test -d rio-scheduler/src/admin/tests && test -f rio-scheduler/src/admin/tests/mod.rs` — directory conversion landed
- `ls rio-scheduler/src/admin/tests/*.rs | wc -l` → ≥6 (mod.rs + logs/gc/tenants/graph/builds)
- `test ! -f rio-scheduler/src/admin/tests.rs` — old monolith file gone (or empty re-export stub if cargo layout requires)
- `cargo nextest run -p rio-scheduler admin::` → same test count pre/post split (currently ~30 `#[tokio::test]` + ~3 `#[test]` in admin tests — count at dispatch with `grep -c '#\[tokio::test\]\|#\[test\]' rio-scheduler/src/admin/tests.rs` on the pre-split file)
- `grep -r 'r\[verify sched.admin\|r\[verify dash.graph' rio-scheduler/src/admin/tests/` → ≥6 hits (all tracey annotations moved with their tests: `list-tenants`, `create-tenant`, `list-builds`×2, `list-workers`, `clear-poison`, `degrade-threshold`)
- `nix develop -c tracey query rule sched.admin.list-tenants` → shows ≥1 verify site post-split (annotation move didn't break tracey scan)

## Tracey

References existing markers (all `r[verify]` annotations MOVE, none are added/removed):

- `r[sched.admin.list-tenants]` + `r[sched.admin.create-tenant]` — `:755-756` → `tenants_tests.rs`
- `r[sched.admin.list-builds]` — `:915` + `:1037` → `builds_tests.rs` (2 verify sites)
- `r[sched.admin.list-workers]` — `:1123` → `builds_tests.rs`
- `r[sched.admin.clear-poison]` — `:1191` → stays in `tests/mod.rs` (clear_poison remains inline in `admin/mod.rs` per P0383-T5)
- `r[dash.graph.degrade-threshold]` — `:1620` → `graph_tests.rs`

No new markers. Pure file reorganization along existing submodule seams.

## Files

```json files
[
  {"path": "rio-scheduler/src/admin/tests/mod.rs", "action": "NEW", "note": "T1: shared setup_svc/setup_svc_default/collect_stream + cluster_status/drain_worker/admin_rpcs_are_wired/clear_poison tests (~600L); mod decls for T2-T6 submodules"},
  {"path": "rio-scheduler/src/admin/tests/logs_tests.rs", "action": "NEW", "note": "T2: get_build_logs_* (5) + extract_drv_hash + gunzip_and_chunk_* (2) + mk_batch helper (~310L from :78-356,:612-639)"},
  {"path": "rio-scheduler/src/admin/tests/gc_tests.rs", "action": "NEW", "note": "T3: trigger_gc_store_unreachable + trigger_gc_forward_exits_on_shutdown (~75L from :640-660,:1348-1402)"},
  {"path": "rio-scheduler/src/admin/tests/tenants_tests.rs", "action": "NEW", "note": "T4: test_create_and_list_tenants + r[verify sched.admin.list-tenants/create-tenant] annotations (~160L from :755-914)"},
  {"path": "rio-scheduler/src/admin/tests/graph_tests.rs", "action": "NEW", "note": "T5: seed_drv/seed_build/link/edge helpers + get_build_graph_* (6) + r[verify dash.graph.degrade-threshold] (~330L from :1404-1732)"},
  {"path": "rio-scheduler/src/admin/tests/builds_tests.rs", "action": "NEW", "note": "T6: list_builds_* (2) + list_workers_with_filter + r[verify sched.admin.list-builds/list-workers] annotations (~275L from :915-1190)"},
  {"path": "rio-scheduler/src/admin/tests.rs", "action": "DELETE", "note": "T1: monolith replaced by tests/ directory (git mv tests.rs tests/mod.rs then extract)"}
]
```

```
rio-scheduler/src/admin/
├── mod.rs                   # (unchanged — P0383 already split to 397L)
├── tests.rs                 # DELETE — becomes tests/
└── tests/
    ├── mod.rs               # T1: ~600L shared setup + inline-handler tests
    ├── logs_tests.rs        # T2: ~310L GetBuildLogs
    ├── gc_tests.rs          # T3: ~75L TriggerGC
    ├── tenants_tests.rs     # T4: ~160L Create/ListTenants
    ├── graph_tests.rs       # T5: ~330L GetBuildGraph
    └── builds_tests.rs      # T6: ~275L ListBuilds/ListWorkers
```

## Dependencies

```json deps
{"deps": [383], "soft_deps": [271, 311, 370], "note": "discovered_from=rev-p383. P0383 (DONE) created the admin/{logs,gc,tenants}.rs submodule seams this split mirrors; tests.rs:1 already imports super::logs::{extract_drv_hash, gunzip_and_chunk} — P0383's extraction is a hard prereq. Soft-dep P0271 (cursor pagination — adds ListBuilds tests; if dispatched after this split, new tests land in builds_tests.rs directly instead of appending to the 1732L monolith; if dispatched before, T6 carries P0271's new tests forward). Soft-dep P0311-T1/T2/T3 (MockAdmin vec![] assertions in admin tests — touches same test fns T6 extracts; sequence-independent, both additive to test bodies). Soft-dep P0370 (spawn_periodic — touches spawn_store_size_refresh which is in admin/gc.rs; T3 here touches gc_tests.rs not gc.rs, non-overlapping). NOTE: grpc/tests.rs is 1682L (same trajectory, P0356 parity candidate) — out of scope here but flagged for followups-sink if trajectory continues."}
```

**Depends on:** [P0383](plan-0383-admin-mod-split-plus-actor-alive-dedup.md) — DONE; provides `admin/{logs,gc,tenants}.rs` + `actor_guards.rs`. The `tests.rs:1` import of `super::logs::*` proves the hard dep.

**Conflicts with:** `admin/tests.rs` is low-collision (most plans touch `admin/mod.rs` not the tests file). [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T1/T2/T3 add assertions to `admin::tests` fns — the fns T6 extracts. If P0311 dispatches first, its test-body edits carry forward into `builds_tests.rs`; if this plan dispatches first, P0311's edits target the new file. Rebase-clean either order (test-body interior vs file-structure exterior). `admin/mod.rs:397` mod declaration (`mod tests;`) stays unchanged — Rust treats `tests.rs` and `tests/mod.rs` identically.
