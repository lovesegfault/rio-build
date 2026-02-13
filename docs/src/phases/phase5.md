# Phase 5: CA Early Cutoff + Multi-Tenancy (Months 19-24)

**Goal:** CA optimization and multi-tenancy.

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
- [ ] Multi-tenant isolation
  - Namespace-scoped builds with resource quotas
  - Per-tenant store partitioning (shared deduplication, separate access control)
  - SSH key -> tenant mapping in gateway
- [ ] NAR chunk transfer optimization: only transfer missing chunks when populating worker stores
- [ ] Web dashboard (TypeScript SPA)
  - Build list with status/timing/DAG visualization
  - Worker utilization graphs, cache hit rate analytics
  - Build log viewer (via gRPC-Web streaming)
  - gRPC-Web proxy (Envoy sidecar or tonic-web)

## Future Work

The following items are separate projects with their own roadmaps:

- **Work-stealing:** idle workers request work from overloaded workers' queues
- **Speculative execution:** critical-path builds on idle workers

## Milestone

CA early cutoff skips downstream builds for unchanged outputs; tenant isolation enforced.
