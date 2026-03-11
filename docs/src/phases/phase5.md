# Phase 5: CA Early Cutoff + Multi-Tenancy Enforcement (Months 22-28)

**Goal:** CA optimization and full multi-tenancy enforcement.

**Implements:** [rio-scheduler](../components/scheduler.md) CA cutoff, [Multi-Tenancy](../multi-tenancy.md)

## Tasks

- [ ] Activate CA early cutoff in rio-scheduler
  - The store schema and gateway stubs from Phase 2c are now connected to the scheduler's per-edge cutoff logic
  - On CA build completion: compare output hash to content index
  - If match: propagate cutoff through DAG via per-edge tracking, skip downstream rebuilds
  - Track cutoff savings in metrics
- [ ] CA derivation resolution
  - Before building a CA derivation that depends on other CA derivations, the derivation must be "resolved": rewrite `inputDrvs` to replace placeholder output paths with actual realized output paths from the `realisations` table
  - The scheduler performs resolution after all input CA derivations are built and their realisations are recorded
  - Resolution produces a "resolved derivation" with concrete input paths, which is what the worker actually builds
  - **Note:** `wopQueryRealisation` (43) and `wopRegisterDrvOutput` (42) are already implemented as working read/write operations in [Phase 2c](./phase2c.md). This phase connects them to the scheduler's per-edge cutoff logic and adds derivation resolution on top.
- [ ] Multi-tenant isolation **enforcement**
  - Resource quota enforcement: reject `SubmitBuild` when tenant's `path_tenants` sum exceeds `gc_max_store_bytes` (Phase 4b ships accounting only)
  - Per-tenant signing keys (ed25519 per tenant; Phase 4 signs all narinfo with a single cluster key)
  - JWT issuance/verification for tenant identity (Phase 4 uses SSH-key-comment → `tenants.tenant_name` lookup)
  - `FindMissingChunks` per-tenant scoping (optional, at the cost of dedup savings) — requires `chunks.tenant_id` column
- [ ] NAR chunk transfer optimization: only transfer missing chunks when populating worker stores
  - `PutChunk` RPC + refcount policy for standalone chunks (grace TTL before GC)
  - Client-side chunker in `rio-worker` so uploads send only missing chunks
- [ ] Live preemption / migration — mid-build `ResourceUsage` streaming via `ProgressUpdate`, worker-side checkpoint, scheduler detects "about to OOM, move it"
- [ ] Atomic multi-output registration — all-or-nothing semantics across a derivation's outputs; currently partial registration is possible on upload failure
- [ ] Chaos testing harness (toxiproxy or equivalent for network fault injection)
- [ ] Web dashboard (TypeScript SPA)
  - Build list with status/timing/DAG visualization
  - Worker utilization graphs, cache hit rate analytics
  - Build log viewer (via gRPC-Web streaming)
  - gRPC-Web proxy (Envoy sidecar or tonic-web)

## Future Work

The following items are separate projects with their own roadmaps:

- **Work-stealing:** idle workers request work from overloaded workers' queues
- **Speculative execution:** critical-path builds on idle workers
- **Staggered scheduling:** delay dispatch to newly-registered workers until prefetch-warm (revisit on production cold-start data)
- **Nix multi-version compatibility matrix + `cargo-mutants`:** CI infrastructure expansion

## Milestone

CA early cutoff skips downstream builds for unchanged outputs; tenant isolation enforced.
