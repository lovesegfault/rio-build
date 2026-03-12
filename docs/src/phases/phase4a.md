# Phase 4a: Observability + Multi-Tenancy Foundation (Months 17-18)

**Goal:** Close observability gaps, fix live metric bugs, wire tenant propagation from SSH key comment through to PostgreSQL, implement admin RPC stubs.

**Implements:** [Observability](../observability.md) (traceparent through `WorkAssignment`, transfer volume + utilization metrics), [Multi-Tenancy](../multi-tenancy.md) (D1: tenants table + scheduler-side resolution), [Error Taxonomy](../errors.md) (poison persistence + `ClearPoison`)

## Tasks

### Observability completion pass

- [x] **Critical: fix `cancel_signals` metric name mismatch** — `actor/build.rs:121` and `actor/worker.rs:254` emit `rio_scheduler_cancel_signals_sent_total`, but `lib.rs:179` registers `rio_scheduler_cancel_signals_total`. The documented metric has always been zero.
- [x] **Traceparent through `WorkAssignment`** — add `string traceparent = 9` proto field; scheduler injects current span's W3C traceparent at dispatch (`actor/dispatch.rs`); worker parses via `TraceContextPropagator::extract` and wraps the spawned build task with `.instrument(span)`. This closes the SSH-channel gap: ssh-ng has no gRPC metadata, so per-assignment traceparent travels in the payload instead.
- [x] `inject_current` in `rio-proto/src/client.rs` helpers (`query_path_info_opt`, `get_path_nar`) — currently these client helpers do NOT inject, so store-side spans are orphaned.
- [x] `link_parent` missing from 7 handlers: scheduler `cluster_status`/`trigger_gc`/`drain_worker`; store `content_lookup`/`trigger_gc`/`pin_path`/`unpin_path`.
- [x] `#[instrument]` on `spawn_build_task` and `handle_prefetch_hint` (`rio-worker/src/runtime.rs`).
- [x] `inject_current` on scheduler→store calls in `actor/merge.rs` (`find_missing_paths`) and `actor/recovery.rs`.
- [x] **Transfer-volume metrics**: `rio_store_put_path_bytes_total` + `rio_store_get_path_bytes_total` counters (by `nar_size` / `expected_size`).
- [x] **Worker utilization gauges**: `rio_worker_cpu_fraction` + `rio_worker_memory_fraction` via a cgroup-polling background task (`spawn_utilization_reporter` in `rio-worker/src/cgroup.rs`).
- [x] Emit `trace_id` to the Nix client via `STDERR_NEXT` at SubmitBuild time — gives operators a grep handle for Tempo.
- [x] Re-examine `nix/tests/phase2b.nix:272` span-linking assertion with the new per-assignment traceparent flow — gateway ssh-ng handler creates its own root span (no incoming traceparent from SSH), but the scheduler RPC should now be a child. Update the assertion to verify gateway→scheduler→worker trace continuity via Tempo `/api/traces/{traceID}` span count ≥3.

### Multi-tenancy foundation (D1: scheduler-side resolution)

- [x] **Migration 009 Part A**: `tenants(tenant_id UUID PK, tenant_name TEXT UNIQUE, gc_retention_hours INT DEFAULT 168, gc_max_store_bytes BIGINT, created_at)`. Pre-FK `NULL`-out of any existing non-NULL `tenant_id` rows in `builds`/`derivations` (they're valid-UUID-format but reference nothing — pre-009 `parse::<Uuid>()` would have rejected non-UUIDs). Add FKs + partial indexes (`WHERE tenant_id IS NOT NULL`).
- [x] `SchedulerDb::resolve_tenant(name: &str) -> Result<Option<Uuid>>` — PG lookup by `tenant_name`.
- [x] Replace `parse::<Uuid>()` at `rio-scheduler/src/grpc/mod.rs:~364` with `resolve_tenant` call. Unknown tenant → `Status::invalid_argument("unknown tenant: {name}")`. Empty string → `None` (unauthenticated/single-tenant mode).
- [x] Change `BuildInfo.tenant_id` type from `Option<String>` to `Option<Uuid>` (`rio-scheduler/src/state/build.rs:91`) — scheduler was already parsing as UUID but storing the string.
- [x] **Gateway: capture tenant name from SSH key comment** — the comment lives in the **server-side `authorized_keys` entry**, NOT the client's key (the client sends raw key data only). Change `auth_publickey` (`rio-gateway/src/server.rs:205`) from `.any(|authorized| authorized.key_data() == key.key_data())` to `.find(...)` and call `.comment()` on the **matched authorized entry**. Store as `ConnectionHandler.tenant_name: Option<String>` (new field). Plumb through `channel_open_session` → `session::run()` spawn → `SessionContext::new()` (new `tenant_name: Option<String>` parameter + field) → `build_submit_request` (`rio-gateway/src/translate.rs:544`). Empty comment = `None` = single-tenant mode.
- [x] Add `tenant_name` to gateway log span fields (`observability.md:269` span-field row is currently marked Phase 4+).
- [x] **Binary cache auth middleware** (`rio-store/src/cache_server/auth.rs`) — Bearer token or netrc-compatible. See `docs/src/components/store.md:156` deferral block. Token→tenant mapping via `tenants` table. Unauthenticated access must be an explicit opt-in.
- [x] `ListTenants`/`CreateTenant` admin RPCs (`rio-proto/proto/admin.proto` + scheduler handler + db helpers).
- [x] `tenant_id` span field on scheduler handlers — once `resolve_tenant` returns a UUID, attach it to the `SubmitBuild`/`WatchBuild`/`CancelBuild` handler spans (`observability.md:269`). No gRPC metadata helper needed in Phase 4 (no downstream consumer — store stays tenant-unaware per D4; worker doesn't need tenant for execution).

### Admin RPCs + poison persistence

- [x] **Migration 009 Part B**: `ALTER TABLE derivations ADD COLUMN poisoned_at TIMESTAMPTZ`. The `failed_workers TEXT[]` column already exists (migration 004) — `errors.md:75` is stale on this point and will be corrected in the Phase 4c doc-sync pass.
- [x] Persist `poisoned_at` when `POISON_THRESHOLD` trips (`poison_and_cascade` helper). Recovery loads it. TTL expiry check compares `poisoned_at + poison_ttl` vs `now()`. `clear_poison` db helper sets it `NULL`.
- [x] Add `WorkerState.connected_since: Instant` + `last_resources: Option<ResourceUsage>` — populated on `RegisterWorker` + heartbeat `ResourceUsage` updates.
- [x] **Implement `ListWorkers`** (`rio-scheduler/src/admin/mod.rs:406` stub) — new `ListWorkers` ActorCommand returning snapshot of `WorkerState` map. Response includes `worker_id`, `size_class`, `draining`, `running_builds.len()`, `connected_since`, `last_resources`.
- [x] **Implement `ListBuilds`** (`:413` stub) — PG query on `builds` with tenant filter. Paginated (`limit`, `cursor` by `submitted_at`).
- [x] **Implement `ClearPoison`** (`:567` stub) — `ClearPoison(drv_hash)` ActorCommand → removes from in-memory `poisoned` set + `db.clear_poison(drv_hash)`.
- [x] `ClusterStatus.store_size_bytes` via a slow-refresh background task (60s poll, `AtomicU64` read by the handler). NOT inline in the handler (`admin/mod.rs:353` TODO) — keeps `ClusterStatus` fast for the autoscaler's 30s poll. **Source: direct PG query** (`SELECT COALESCE(SUM(nar_size), 0) FROM narinfo`) — scheduler already has a pool to the shared DB; no new store RPC needed. Follows the `scheduler_live_pins` cross-layer precedent.

### VM test scaffolding

- [x] **Create `nix/tests/phase4.nix`** with Section A only (tenant smoke). Register as `vm-phase4` in `flake.nix` (`withMinCpu 4`, `k3sArgs` — same shape as `vm-phase3a`). Later sub-phases append sections B–J (see section map in `phase4.md`). Section A: SSH key with comment `team-test` → `SubmitBuild` → PG `builds` row has `tenant_id` matching a pre-seeded `tenants` row; second key with no comment → `tenant_id IS NULL`.

### Tracey markers

- [x] Add spec `r[...]` markers + `r[impl]`/`r[verify]` annotations for 4a behaviors: `sched.tenant.resolve`, `gw.auth.tenant-from-key-comment`, `store.cache.auth-bearer`, `sched.admin.list-workers`, `sched.admin.list-builds`, `sched.admin.clear-poison`, `sched.poison.ttl-persist`, `sched.trace.assignment-traceparent`.

## Carried-forward TODOs (resolved)

| Location | What | Resolution |
|---|---|---|
| `rio-gateway/src/translate.rs:544` | `tenant_id: String::new()` | Gateway captures from SSH key comment |
| `rio-scheduler/src/admin/mod.rs:406,413,567` | `ListWorkers`/`ListBuilds`/`ClearPoison` stubs | Implemented |
| `rio-scheduler/src/admin/mod.rs:353` | `store_size_bytes` | Background refresh task |
| `rio-controller/src/crds/build.rs:65` | Tenant enforcement | Controller→scheduler passthrough already sends tenant name; scheduler now resolves it |
| `docs/src/components/store.md:156` | Binary cache auth | Bearer/netrc middleware |
| `docs/src/components/scheduler.md:413` | `derivations_tenant_idx` | Migration 009 Part A |
| `nix/tests/phase2b.nix:272` | Span-linking re-examine | Updated assertion with new traceparent flow |

## Milestone

`nix develop -c cargo nextest run` passes with new tests for: metric name (assert the right counter increments), tenant resolution (known name → UUID row, unknown → `InvalidArgument`, empty → `None`), `ListWorkers` snapshot shape, `ClearPoison` clears both in-mem and PG.

`nix-build-remote -- .#checks.x86_64-linux.vm-phase4` passes (Section A: tenant smoke).

**Status: COMPLETE (round 3)** — 52 commits on `phase-4a-dev`. 1139 nextest tests (+12), tracey 143 rules (143 covered, 120 verified — up from 141/116).

Round 2 (08f2058..875aa97, +9 commits) fixed: `set_poisoned_at`/`clear_poison` BYTEA-vs-TEXT binding bug that silently broke poison persistence, missing `WorkerInfo.size_class` + `ListBuilds` tenant filter proto fields, `tenant_id` span field on SubmitBuild, observability gaps (TriggerGC inject_current, backstop cancel_signals), `TenantInfo.created_at` now populated via epoch extract. Added `r[verify]` tests for ClearPoison happy path + poison recovery.

Round 3 (6ee4f10..3abfc95, +15 commits) found a **critical trace-chain break**: span context does NOT cross the mpsc channel to the actor task, so `dispatch.rs` calling `current_traceparent()` captured an orphan actor span — the worker received a valid traceparent but it belonged to a disjoint trace. The phase4a design goal (*"gateway→scheduler→worker trace continuity"*) was never actually achieved; the VM assertion had been watered down to "trace exists" after three failed loosening attempts. **Fix:** carry traceparent as plain data through `MergeDagRequest` → `DerivationState` → `WorkAssignment`, so dispatch embeds the submitter's trace regardless of dispatch path (immediate/deferred/post-recovery). First submitter wins on dedup. Restored phase2b span-count ≥3 VM assertion — **verified passing**. Also found an **empty-bearer-token auth bypass**: `"Bearer "` (trailing space) → `strip_prefix` yields `Some("")` → matched `WHERE cache_token = ''` if operator mistakenly created such a tenant; `CreateTenant` accepted it; `has_cache_token` hid it (IS NOT NULL). Fixed both sides. Also fixed: `handle_reconcile_assignments` missing `#[instrument]` (inject_current was a no-op), `from_poisoned_row` not guarding `Duration::from_secs_f64(+inf)` panic, bare `tokio::spawn` for cgroup reporter (→ `spawn_monitored`), consolidated duplicated tenant-resolve blocks, split vestigial heartbeat tuple param, added verify tests for `obs.metric.worker-util` + `obs.metric.transfer-volume` + `obs.trace.w3c-traceparent`, added `sched.admin.{list,create}-tenant` spec markers, filled minor test gaps.
