# Plan 0156: Admin RPC suite — ListTenants/CreateTenant/ListWorkers/ListBuilds + store_size refresh

## Design

Four admin RPCs from stub → implementation, plus the `store_size_bytes` background refresh. All proto additions are additive; the handlers replace `Unimplemented` stubs that had existed since phase 3a.

**ListTenants/CreateTenant** (`fcb6f26`): `admin.proto` RPCs + `TenantInfo` type (includes `has_cache_token` bool — never leaks the token itself). Db helpers `list_tenants()` and `create_tenant(name, retention, max_bytes, cache_token)` where conflict (tenant_name OR cache_token) → `None` → handler returns `AlreadyExists`. `TenantInfo.created_at` initially `None` — PG's `::text` format isn't RFC3339 and there was no chrono dep. Round 2 (`ffdec1d`) switched to `extract(epoch)` cast.

**WorkerState tracking** (`56e7c91`): `HeartbeatRequest.resources` (proto field 3) existed but was NEVER threaded through — the gRPC handler didn't extract it, `ActorCommand::Heartbeat` didn't carry it, `handle_heartbeat` didn't receive it. End-to-end plumbing: `connected_since: Instant` set in `new()`, `last_resources: Option<ResourceUsage>` stored by `handle_heartbeat` (not clobbered with `None`).

**ListWorkers** (`3ff9037`): `ActorCommand::ListWorkers{reply}` via `send_unchecked` (dashboard needs a reading even under saturation). O(workers) scan maps `WorkerState` → `WorkerSnapshot` (internal, `Instant` fields). `admin/workers.rs` filters by status (alive/draining/connecting), converts `Instant` → `SystemTime` via `SystemTime::now() - elapsed`.

**ListBuilds + store_size** (`1f37efc`): `admin/builds.rs` direct PG query with `LIMIT/OFFSET` pagination. `cached_derivations` heuristic = "completed with no assignment row" (cache-hit transitions directly to Completed at merge time). New `spawn_store_size_refresh`: 60s background loop polling `SELECT COALESCE(SUM(nar_size), 0) FROM narinfo` → `Arc<AtomicU64>`. Scheduler already has the shared PG pool (`scheduler_live_pins` cross-layer precedent). `ClusterStatus` reads the atomic — stays fast for the autoscaler's 30s poll.

## Files

```json files
[
  {"path": "rio-proto/proto/admin.proto", "action": "MODIFY", "note": "ListTenants, CreateTenant, ListWorkers, ListBuilds RPCs"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "TenantInfo, WorkerInfo, BuildListRow, ListTenantsResponse, CreateTenantRequest/Response"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "list_tenants, create_tenant, TenantRow"},
  {"path": "rio-scheduler/src/state/worker.rs", "action": "MODIFY", "note": "WorkerState.connected_since + last_resources"},
  {"path": "rio-scheduler/src/actor/command.rs", "action": "MODIFY", "note": "ActorCommand::Heartbeat resources field; ActorCommand::ListWorkers; WorkerSnapshot"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "handle_heartbeat stores last_resources"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "heartbeat handler extracts req.resources"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "list_tenants/create_tenant/list_workers/list_builds handlers; store_size Arc<AtomicU64>"},
  {"path": "rio-scheduler/src/admin/workers.rs", "action": "NEW", "note": "list_workers: actor query + status filter + Instant→SystemTime"},
  {"path": "rio-scheduler/src/admin/builds.rs", "action": "NEW", "note": "list_builds PG query + spawn_store_size_refresh"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "spawn_store_size_refresh before AdminService construction"}
]
```

## Tracey

- `r[impl sched.admin.list-workers]` + `r[verify sched.admin.list-workers]` — `3ff9037`
- `r[impl sched.admin.list-builds]` + `r[verify sched.admin.list-builds]` — `1f37efc`
- `r[impl sched.admin.list-tenants]` + `r[verify sched.admin.list-tenants]` — added retroactively in `a89c7d0` (round 3)
- `r[impl sched.admin.create-tenant]` + `r[verify sched.admin.create-tenant]` — added retroactively in `a89c7d0` (round 3)

8 marker annotations (4 in-cluster, 4 retroactive round 3).

## Entry

- Depends on P0153: tenants table (list/create query it)
- Depends on P0155: ClearPoison precedent (same ActorCommand pattern)

## Exit

Merged as `fcb6f26`, `56e7c91`, `3ff9037`, `1f37efc` (4 commits). `.#ci` green. Tests: two workers (one draining) filter assertion; ListBuilds pagination. Refinements in rounds 2-6 (`929261b`, `ffdec1d`, `c01cceb`, `ef16ff7`, `8842220`, `e6ec34f`, `b67cc16`).
