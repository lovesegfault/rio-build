# Phase 4b: GC Correctness + Operational Tooling + Defense (Months 18-20)

**Goal:** Fix the critical GC reachability bug (worker output references are always empty), automate GC scheduling, ship `rio-cli` + Helm chart, harden against abuse and store degradation.

**Implements:** [Store GC](../components/store.md#garbage-collection) (per-tenant retention + automation), [Error Taxonomy](../errors.md) (per-build timeout, hardened retry), [Failure Modes](../failure-modes.md) (FUSE circuit breaker)

> **Depends on Phase 4a:** Per-tenant GC retention needs the `tenants` table. `rio-cli` needs `ListWorkers`/`ListBuilds`/`ClearPoison`. Rate limiter is keyed on `tenant_name` from `SessionContext`.

## Tasks

### GC correctness (critical bug)

- [ ] **Critical: NAR reference scanner** — `rio-worker/src/upload.rs:223` sends `references: Vec::new()` for EVERY uploaded output. This means GC mark-and-sweep reachability is **wrong**: all non-root paths appear unreachable, and the binary cache serves inaccurate narinfo.
  - New `rio-worker/src/references.rs`: scan the NAR stream for 32-character nixbase32 store-path hashes using `aho-corasick`. Candidate set comes from `WorkAssignment.input_paths` (only scan for paths the build could have seen; scanning for arbitrary 32-char alphabets produces false positives on compressed/binary data).
  - Wire into `upload.rs`: scan each output NAR during the dump-path tee stream → populate `PathInfo.references` before `PutPath`.
  - VM test assertion: build a derivation with known references, `SELECT references FROM narinfo WHERE store_path = '...'` → non-empty and matches expected.
- [ ] **xmax-based inserted-check in `cas.rs` upsert** (`rio-store/src/gc/drain.rs:109` TODO + X18 drain blast-radius race) — the drain task's `still_dead` re-check races against a concurrent `chunks` upsert for the same chunk within milliseconds. Add `RETURNING xmax = 0 AS inserted` to the upsert; drain uses `deleted = false AND NOT inserted` to detect a resurrect-then-redelete race.

### Per-tenant GC retention (D4: scheduler-side `path_tenants` upsert)

- [ ] **Migration 009 Part C**: `path_tenants(store_path_hash BYTEA FK→narinfo ON DELETE CASCADE, tenant_id UUID FK→tenants, first_referenced_at TIMESTAMPTZ, PK(store_path_hash, tenant_id))` + retention index.
- [ ] `SchedulerDb::upsert_path_tenants(output_paths: &[String], tenant_ids: &[Uuid])` — batch `INSERT ... ON CONFLICT DO NOTHING` via `unnest()`. Called from `actor/completion.rs` in `handle_completion` after successful upload, using `completion_report.output_paths` × `interested_builds.filter_map(|b| b.tenant_id)`. **This correctly handles concurrent-dedup**: tenants A+B submit the same derivation → DAG merge dedupes → one execution → completion handler sees BOTH in `interested_builds` → both get rows. `Claims.tenant_id` in HMAC is NOT needed — removed from the design.
- [ ] New GC mark CTE seed (`rio-store/src/gc/mark.rs`): `UNION SELECT n.store_path FROM narinfo n JOIN path_tenants pt USING (store_path_hash) JOIN tenants t USING (tenant_id) WHERE pt.first_referenced_at > now() - make_interval(hours => t.gc_retention_hours)`. Union-of-retention-windows: path survives if ANY tenant's window covers it. Existing global grace seed stays as the floor.
- [ ] Per-tenant quota query (`rio-store/src/gc/tenant.rs`): `SELECT COALESCE(SUM(n.nar_size), 0) FROM narinfo n JOIN path_tenants pt USING (store_path_hash) WHERE pt.tenant_id = $1`. Sum-across-tenants > physical storage (dedup not reflected) — this is correct for a quota: tenant is charged for their working set, not a prorated slice. **Accounting only** in 4b; enforcement (reject SubmitBuild when over quota) is Phase 5.
- [ ] `narinfo.tenant_id` column stays nullable/unused — semantically broken (first-writer-wins). `path_tenants` junction is the authoritative source. May be dropped in a future migration.

### GC automation

- [ ] `rio-store/src/gc/mod.rs`: `run_gc` orchestration function — wraps `mark` → `sweep` → spawn `drain` in one call, with dry-run flag. `StoreAdminService.TriggerGC` already exists (3b); this consolidates the handler body.
- [ ] `rio-controller/src/gc_schedule.rs`: cron-triggered `TriggerGC` calls. Default daily at 03:00 cluster-local. Emits `rio_controller_gc_runs_total{result=success|failure}` (`observability.md:166` Phase 4+ row).
- [ ] `rio_store_gc_paths_swept_total` + `rio_store_gc_chunks_enqueued_total` counters in sweep.

### Operational tooling

- [ ] **`rio-cli` crate** — clap + tokio, talks to `AdminService` via `rio-proto::client::connect_admin`. Subcommands: `status` (ClusterStatus), `workers` (ListWorkers table), `builds` (ListBuilds with filters), `logs <build_id>` (GetBuildLogs), `gc [--dry-run]` (TriggerGC), `poison clear <drv_hash>` (ClearPoison), `tenants list|create`. Output: human-readable tables by default, `--json` flag for machine consumption.
- [ ] `rio-cli` integration smoke tests against a live scheduler (rio-test-support ephemeral PG + scheduler).
- [ ] **Helm chart** (`deploy/helm/rio-build/`) derived from the Kustomize base. `values.yaml` parameterizes: image tags, PostgreSQL/S3 connection, TLS secret names, size-class cutoffs, GC schedule. Templates: Deployment/StatefulSet per component, ServiceAccount + RBAC, NetworkPolicy, PDB, Secret/ConfigMap. `helm install --dry-run` CI check.
- [ ] `rio-cli` + `dockerImages.rio-cli` added to `flake.nix` packages.

### Defensive hardening

- [ ] **Per-tenant rate limiter on SubmitBuild** (`rio-gateway/src/ratelimit.rs`) — `governor` crate, keyed on `SessionContext.tenant_name`. Default: 10 builds/min/tenant, burst 30. Over-limit → SSH-level `STDERR_ERROR` with `TooManyRequests`.
- [ ] **Gateway connection-count cap** — hard limit on concurrent SSH sessions (default 1000). At limit → reject new connections with SSH disconnect reason `TOO_MANY_CONNECTIONS`. Handle `RESOURCE_EXHAUSTED` from scheduler (backpressure) by translating to retryable `STDERR_ERROR`.
- [ ] **FUSE circuit breaker** (`rio-worker/src/fuse/circuit.rs`) — track consecutive store-fetch failures. After N failures (default 5), trip open: FUSE `read()` returns `EIO` immediately instead of blocking on a 300s timeout. Half-open probe on the next `ensure_cached` call; auto-close after 30s. Worker heartbeat reports `store_degraded: bool` → scheduler stops dispatching to degraded workers (closes `challenges.md:100` deferral).
- [ ] **`maxSilentTime` enforcement** (`rio-worker/src/executor/daemon/stderr_loop.rs`) — track `last_output: Instant`. If `elapsed() > max_silent_time` (from `WorkAssignment.build_options`), `cgroup.kill` the build, report `TimedOut`. Currently plumbed through the proto but never checked (`errors.md:96` row).
- [ ] **Per-build overall timeout** — scheduler-side cancellation when `Build.spec.timeout` is exceeded (`errors.md:97` row). Add `BuildInfo.submitted_at: Instant`; `handle_tick` checks `elapsed() > timeout` → `CancelBuild` for all interested derivations → `transition_build` to Failed with `TimedOut`.
- [ ] **Per-worker failure counts** — separate per-worker failure counter from the distinct-worker set. A derivation failing 5× on worker A should NOT count toward the 3-distinct-workers poison threshold. `failed_workers` stays a `HashSet` for the threshold; add `per_worker_failure_count: HashMap<WorkerId, u32>` for retry-budget decisions within one worker.

## Carried-forward TODOs (resolved)

| Location | What | Resolution |
|---|---|---|
| `rio-worker/src/upload.rs:223` | `references: Vec::new()` | NAR scanner populates real references |
| `rio-store/src/gc/drain.rs:109` | xmax inserted-check | RETURNING clause closes the race |
| (X18, undocumented in code) | Drain blast-radius narrow race | Same fix as `drain.rs:109` xmax |
| `docs/src/errors.md:96` | `maxSilentTime` not enforced | `stderr_loop` tracks output cadence |
| `docs/src/errors.md:97` | Per-build overall timeout | `handle_tick` check on `submitted_at` |
| `docs/src/observability.md:166` | `rio_controller_gc_runs_total` Phase 4+ | Emitted by GC schedule reconciler |
| `docs/src/challenges.md:100` | Worker store-degraded heartbeat | FUSE circuit breaker + heartbeat flag |
| `docs/src/failure-modes.md:52` | FUSE timeout + breaker | Circuit breaker + `EIO` fast-fail |

## Milestone

`vm-phase4` GC section passes: build with tenant → `narinfo.references` non-empty, `path_tenants` row exists, backdate past retention + run GC → unreferenced path swept, referenced path survives.

`rio-cli status/workers/builds/gc` works against the `vm-phase4` scheduler. `helm install --dry-run` renders without errors against the `deploy/base/` manifests.
