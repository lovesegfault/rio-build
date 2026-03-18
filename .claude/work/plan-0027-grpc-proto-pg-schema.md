# Plan 0027: gRPC service definitions + PostgreSQL schema

## Context

With crates scaffolded (P0026), the services need contracts. This plan defines all five gRPC services (Scheduler, Worker, Store, Chunk, Admin) as protobuf and the PostgreSQL schema for scheduler state + store metadata. ~50 message types, two migration files. This is the interface layer — everything downstream speaks these protos.

Phase 2a deviation from the design spec: `tenant_id` columns exist in the schema but are nullable/unused. Multi-tenancy is a phase 4 concern; the columns are forward-compat placeholders.

## Commits

- `e2eabd3` — feat(rio-proto): define gRPC services and PostgreSQL schema for Phase 2a

Single commit. 987 insertions, 12 files.

## Files

```json files
[
  {"path": "rio-proto/proto/scheduler.proto", "action": "NEW", "note": "SchedulerService: SubmitBuild, WatchBuild, QueryBuildStatus, CancelBuild"},
  {"path": "rio-proto/proto/worker.proto", "action": "NEW", "note": "WorkerService: BuildExecution (bidi stream), Heartbeat"},
  {"path": "rio-proto/proto/store.proto", "action": "NEW", "note": "StoreService: PutPath, GetPath, QueryPathInfo, FindMissingPaths, ContentLookup (stub)"},
  {"path": "rio-proto/proto/admin.proto", "action": "NEW", "note": "AdminService: GetBuildLogs, TriggerGC, DrainWorker, ClearPoison, + 3 more"},
  {"path": "rio-proto/proto/types.proto", "action": "NEW", "note": "~50 message types: DerivationNode, WorkAssignment{assignment_token}, HeartbeatRequest{system,supported_features,max_builds}, BloomHashAlgorithm enum, BuildResultStatus enum"},
  {"path": "rio-proto/build.rs", "action": "MODIFY", "note": "tonic_build: compile all 5 proto files"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "re-export generated modules"},
  {"path": "migrations/001_scheduler_tables.sql", "action": "NEW", "note": "builds, derivations, derivation_edges, build_derivations, assignments{generation}, build_history; priority_class CHECK constraint; tenant_id nullable"},
  {"path": "migrations/002_store_tables.sql", "action": "NEW", "note": "narinfo, nar_blobs (full NARs, no chunking); 'references' quoted (PG reserved word — discovered in P0032)"}
]
```

## Design

**Proto surface:** `SchedulerService` is the build-request entrypoint — `SubmitBuild` takes a DAG of `DerivationNode`s + edges, returns a build ID; `WatchBuild` server-streams `BuildEvent`s. `WorkerService.BuildExecution` is the bidirectional stream workers hold open: server sends `WorkAssignment`, client sends `CompletionReport`. `Heartbeat` is separate (RPC, not stream message) so it survives stream reconnects. `StoreService` is CRUD on NARs keyed by store path. `ChunkService` is entirely stubs — CAS chunking is a later-phase optimization. `AdminService` has the operational surface (drain worker, clear poison, GC trigger).

The `assignment_token` field on `WorkAssignment` is documented as HMAC-signed but P0034 corrects this — it's a plain UUID in phase 2a. `BuildResultStatus` enum maps 1:1 to Nix's `BuildStatus` (Built=0, Substituted=1, ... DependencyFailed=10).

**PostgreSQL schema:** migration 001 is the scheduler's persistence. `derivations` keyed by `drv_hash` (not path — multiple paths can hash identically under content-addressed derivations). `derivation_edges` has an `is_cutoff` boolean for future CA early-cutoff. `assignments` carries a `generation` column for phase 3a leader-election fencing. Migration 002 is simpler: `narinfo` (store path → metadata) + `nar_blobs` (blob key → status, for write-ahead upload tracking). No chunking in 2a — full NARs only.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Proto spec text in `docs/src/components/proto.md` was later retro-tagged with `r[proto.scheduler.*]`, `r[proto.worker.*]`, `r[proto.store.*]` markers.

## Outcome

Merged as `e2eabd3`. Protos compile via tonic_build; migrations are syntax-valid but not yet run (sqlx migrate!() not wired until P0032 — the `"references"` reserved-word bug lay dormant until then).
