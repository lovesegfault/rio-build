# Phase 4: Production Hardness (Months 17-22)

**Goal:** Observability completion, multi-tenancy propagation, GC correctness, operational tooling, adaptive scheduling, and production-grade VM test coverage.

Phase 4 is primarily additive: new background tasks (GC cron, `CutoffRebalancer`, store-size refresher, FUSE circuit breaker), new gRPC surface (`ListWorkers`, `ListBuilds`, `ClearPoison`, `GetSizeClassStatus`, `ListTenants`), new crates (`rio-cli`, `rio-bench`), a new CRD (`WorkerPoolSet` with child-`WorkerPool` ownership), and migration `009`. No existing state machines change shape.

Phase 4 is split into three sub-phases to manage scope and dependency ordering:

- **[Phase 4a](./phase4a.md)** (Months 17-18): Observability completion + multi-tenancy foundation — critical metric-name bug, traceparent through `WorkAssignment`, tenants table + SSH-key→tenant propagation, admin RPCs, poison persistence.
- **[Phase 4b](./phase4b.md)** (Months 18-20): GC correctness + operational tooling + defensive hardening — **critical: worker output references are currently always empty → GC reachability is wrong**. NAR reference scanner, per-tenant GC retention, GC automation, `rio-cli`, Helm chart, rate limiting, FUSE circuit breaker, `maxSilentTime` enforcement.
- **[Phase 4c](./phase4c.md)** (Months 20-22): Adaptive scheduling + validation + polish — SITA-E `CutoffRebalancer`, `WorkerPoolSet` CRD, VM tests for PDB/NetPol/FOD-proxy, Grafana dashboards, `rio-bench`, custom seccomp, doc sync.

See the individual sub-phase docs for detailed task lists and milestones.

## Key Architecture Decisions (resolved during planning)

| # | Decision | Rationale |
|---|---|---|
| D1 | Tenants: UUID + lookup table, **scheduler-side** name→UUID resolution | Gateway stays PostgreSQL-free → preserves stateless-N-replica HA. Adding a tenant = one `INSERT` + an `authorized_keys` line, no gateway restart. |
| D2 | SITA-E input: new `build_samples` table (raw durations), not `build_history` | `build_history` stores EMA only. SITA-E needs the empirical CDF → raw samples. |
| D3 | Full `WorkerPoolSet` CRD: reconciler creates/owns child `WorkerPool` CRs | Per-class autoscaling via new `GetSizeClassStatus` RPC. `PoolTemplate` nested struct (subset of `WorkerPoolSpec`). |
| D4 | Per-tenant GC via `path_tenants` junction table, upserted by **scheduler** in `handle_completion` | Correctly handles concurrent-dedup: tenants A+B submit same derivation → DAG merge dedupes → one execution → completion handler sees BOTH in `interested_builds` → both get `path_tenants` rows. Store stays tenant-unaware. `Claims.tenant_id` in HMAC is NOT needed. |
| D5 | Load testing: criterion benches + VM scaled-DAG smoke | Hydra comparison = documented manual procedure, not automated. |
| D6 | VM tests E (PDB/NetPol) + D (FOD proxy) in scope | NetPol ingress filtering **skipped** — scheduler/store aren't pods in the test topology. |

## Migration 009

All Phase 4 schema changes land in a single `migrations/009_phase4.sql`, appended across sub-phases:

| Part | Sub-phase | Content |
|---|---|---|
| A | 4a | `tenants` table + FKs + indexes + pre-FK `NULL`-out of existing orphan `tenant_id` rows |
| B | 4a | `derivations.poisoned_at TIMESTAMPTZ` |
| C | 4b | `path_tenants` junction (many-to-many, union-of-retention-windows) |
| D | 4c | `build_samples` (raw durations for SITA-E CDF) |

## Deferred to Phase 5

Explicitly out of Phase 4 scope:

| Item | Reason |
|---|---|
| `PutChunk` RPC | No client-side chunker exists yet. Needs a refcount policy for standalone chunks (grace TTL before GC). |
| Live preemption/migration | Needs worker-side checkpoint + mid-build `ResourceUsage` streaming via `ProgressUpdate`. |
| `rio-dashboard` web SPA | `rio-cli` + Grafana cover the operator workflow. SPA is a separate project. |
| Chaos testing harness (toxiproxy fault injection) | VM test topology already covers crash/reconnect/failover. Fault injection is a distinct harness. |
| Per-tenant resource quota **enforcement** | Quota **accounting** (sum of `nar_size` via `path_tenants`) ships in 4b. Enforcement (reject SubmitBuild when over quota) is Phase 5. |
| Per-tenant signing keys + JWT tenant tokens | Single cluster-wide ed25519 key signs all narinfo today. Per-tenant keys are Phase 5 with full multi-tenant isolation. |
| `FindMissingChunks` per-tenant scoping | Chunk table doesn't carry `tenant_id`. Cross-tenant build-activity leakage via chunk probing is documented as an accepted risk. |
| NetworkPolicy ingress filtering VM test | Scheduler/store are systemd services on the `control` VM, not pods — `podSelector` matches nothing. |
| Atomic multi-output registration | Partially-registered derivations after upload failure are cleaned up by the next successful rebuild. Complexity not justified yet. |
| Staggered scheduling (delay dispatch to cold workers until prefetch-warm) | In-process chunk cache + per-derivation prefetch absorb most thundering-herd. Revisit if mass cold-start (all pools scaling from 0) spikes S3 in production. |
| Nix multi-version compatibility matrix + `cargo-mutants` | Testing infrastructure expansion; separate track. |

## Milestone

All three sub-phase milestones pass. `ci-fast` aggregate includes `vm-phase4` and `vm-phase4-fod`. Grafana dashboards render against a live cluster. `rio-cli status/workers/builds/gc` works against a Helm-deployed cluster.
