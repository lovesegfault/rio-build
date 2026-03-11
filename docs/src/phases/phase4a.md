# Phase 4a: Observability + Multi-Tenancy Foundation (Months 17-18)

**Goal:** Close observability gaps, fix live metric bugs, wire tenant propagation from SSH key comment through to PostgreSQL, implement admin RPC stubs.

**Implements:** [Observability](../observability.md) (traceparent through `WorkAssignment`, transfer volume + utilization metrics), [Multi-Tenancy](../multi-tenancy.md) (D1: tenants table + scheduler-side resolution), [Error Taxonomy](../errors.md) (poison persistence + `ClearPoison`)

## Tasks

### Observability completion pass

- [ ] **Critical: fix `cancel_signals` metric name mismatch** — `actor/build.rs:121` and `actor/worker.rs:254` emit `rio_scheduler_cancel_signals_sent_total`, but `lib.rs:179` registers `rio_scheduler_cancel_signals_total`. The documented metric has always been zero.
- [ ] **Traceparent through `WorkAssignment`** — add `string traceparent = 9` proto field; scheduler injects current span's W3C traceparent at dispatch (`actor/dispatch.rs`); worker parses via `TraceContextPropagator::extract` and wraps the spawned build task with `.instrument(span)`. This closes the SSH-channel gap: ssh-ng has no gRPC metadata, so per-assignment traceparent travels in the payload instead.
- [ ] `inject_current` in `rio-proto/src/client.rs` helpers (`query_path_info_opt`, `get_path_nar`) — currently these client helpers do NOT inject, so store-side spans are orphaned.
- [ ] `link_parent` missing from 7 handlers: scheduler `cluster_status`/`trigger_gc`/`drain_worker`; store `content_lookup`/`trigger_gc`/`pin_path`/`unpin_path`.
- [ ] `#[instrument]` on `spawn_build_task` and `handle_prefetch_hint` (`rio-worker/src/runtime.rs`).
- [ ] `inject_current` on scheduler→store calls in `actor/merge.rs` (`find_missing_paths`) and `actor/recovery.rs`.
- [ ] **Transfer-volume metrics**: `rio_store_put_path_bytes_total` + `rio_store_get_path_bytes_total` counters (by `nar_size` / `expected_size`).
- [ ] **Worker utilization gauges**: `rio_worker_cpu_fraction` + `rio_worker_memory_fraction` via a cgroup-polling background task (`spawn_utilization_reporter` in `rio-worker/src/cgroup.rs`).
- [ ] Emit `trace_id` to the Nix client via `STDERR_NEXT` at SubmitBuild time — gives operators a grep handle for Tempo.
- [ ] Re-examine `nix/tests/phase2b.nix:272` span-linking assertion with the new per-assignment traceparent flow — gateway ssh-ng handler creates its own root span (no incoming traceparent from SSH), but the scheduler RPC should now be a child. Update the assertion to verify gateway→scheduler→worker trace continuity via Tempo `/api/traces/{traceID}` span count ≥3.

### Multi-tenancy foundation (D1: scheduler-side resolution)

- [ ] **Migration 009 Part A**: `tenants(tenant_id UUID PK, tenant_name TEXT UNIQUE, gc_retention_hours INT DEFAULT 168, gc_max_store_bytes BIGINT, created_at)`. Pre-FK `NULL`-out of any existing non-NULL `tenant_id` rows in `builds`/`derivations` (they're valid-UUID-format but reference nothing — pre-009 `parse::<Uuid>()` would have rejected non-UUIDs). Add FKs + partial indexes (`WHERE tenant_id IS NOT NULL`).
- [ ] `SchedulerDb::resolve_tenant(name: &str) -> Result<Option<Uuid>>` — PG lookup by `tenant_name`.
- [ ] Replace `parse::<Uuid>()` at `rio-scheduler/src/grpc/mod.rs:~364` with `resolve_tenant` call. Unknown tenant → `Status::invalid_argument("unknown tenant: {name}")`. Empty string → `None` (unauthenticated/single-tenant mode).
- [ ] Change `BuildInfo.tenant_id` type from `Option<String>` to `Option<Uuid>` (`rio-scheduler/src/state/build.rs:91`) — scheduler was already parsing as UUID but storing the string.
- [ ] **Gateway: capture tenant name from SSH key comment** — `PublicKey::comment()` (ssh-key crate preserves comments). Store on `ConnectionHandler`, plumb through `SessionContext.tenant_name: String`, pass to `build_submit_request` (`rio-gateway/src/translate.rs:544`).
- [ ] Add `tenant_name` to gateway log span fields (`observability.md:269` span-field row is currently marked Phase 4+).
- [ ] **Binary cache auth middleware** (`rio-store/src/cache_server/auth.rs`) — Bearer token or netrc-compatible. See `docs/src/components/store.md:156` deferral block. Token→tenant mapping via `tenants` table. Unauthenticated access must be an explicit opt-in.
- [ ] `ListTenants`/`CreateTenant` admin RPCs (`rio-proto/proto/admin.proto` + scheduler handler + db helpers).
- [ ] `x-rio-tenant` gRPC metadata helper in `rio-proto` — scheduler writes resolved UUID to metadata for downstream propagation (span fields, future per-tenant filtering).

### Admin RPCs + poison persistence

- [ ] **Migration 009 Part B**: `ALTER TABLE derivations ADD COLUMN poisoned_at TIMESTAMPTZ`. The `failed_workers TEXT[]` column already exists (migration 004) — `errors.md:75` is stale on this point and will be corrected in the Phase 4c doc-sync pass.
- [ ] Persist `poisoned_at` when `POISON_THRESHOLD` trips (`poison_and_cascade` helper). Recovery loads it. TTL expiry check compares `poisoned_at + poison_ttl` vs `now()`. `clear_poison` db helper sets it `NULL`.
- [ ] Add `WorkerState.connected_since: Instant` + `last_resources: Option<ResourceUsage>` — populated on `RegisterWorker` + heartbeat `ResourceUsage` updates.
- [ ] **Implement `ListWorkers`** (`rio-scheduler/src/admin/mod.rs:406` stub) — new `ListWorkers` ActorCommand returning snapshot of `WorkerState` map. Response includes `worker_id`, `size_class`, `draining`, `running_builds.len()`, `connected_since`, `last_resources`.
- [ ] **Implement `ListBuilds`** (`:413` stub) — PG query on `builds` with tenant filter. Paginated (`limit`, `cursor` by `submitted_at`).
- [ ] **Implement `ClearPoison`** (`:567` stub) — `ClearPoison(drv_hash)` ActorCommand → removes from in-memory `poisoned` set + `db.clear_poison(drv_hash)`.
- [ ] `ClusterStatus.store_size_bytes` via a slow-refresh background task (60s poll, `AtomicU64` read by the handler). NOT inline in the handler (`admin/mod.rs:353` TODO) — keeps `ClusterStatus` fast for the autoscaler's 30s poll.

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

`vm-phase4` first section (tenant smoke) passes: SSH key with comment `team-test` → build row has `tenant_id` matching the pre-seeded `tenants` row.
