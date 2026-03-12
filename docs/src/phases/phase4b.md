# Phase 4b: GC Correctness + Operational Tooling + Defense (Months 18-20)

**Goal:** Fix the critical GC reachability bug (worker output references are always empty), automate GC scheduling, ship `rio-cli` + Helm chart, harden against abuse and store degradation.

**Implements:** [Store GC](../components/store.md#garbage-collection) (per-tenant retention + automation), [Error Taxonomy](../errors.md) (per-build timeout, hardened retry), [Failure Modes](../failure-modes.md) (FUSE circuit breaker)

> **Depends on Phase 4a:** Per-tenant GC retention needs the `tenants` table. `rio-cli` needs `ListWorkers`/`ListBuilds`/`ClearPoison`. Rate limiter is keyed on `tenant_name` from `SessionContext`.

## Tasks

### GC correctness (critical bug)

- [ ] **Critical: NAR reference scanner** — `rio-worker/src/upload.rs:223` sends `references: Vec::new()` for EVERY uploaded output. This means GC mark-and-sweep reachability is **wrong**: all non-root paths appear unreachable, and the binary cache serves inaccurate narinfo.
  - New `rio-worker/src/references.rs`: scan the NAR stream for 32-character nixbase32 store-path hashes using `aho-corasick` (add to `[workspace.dependencies]`). The automaton is built from a candidate set so only paths the build could have seen are matched — scanning for arbitrary 32-char alphabets would produce false positives on compressed/binary data.
  - **Candidate set source:** the input closure computed at `rio-worker/src/executor/mod.rs:379` via `compute_input_closure()` (QueryPathInfo BFS). This closure is currently built for the synthetic DB and dropped before the upload step. **Plumb it through:** keep the `input_paths: Vec<String>` alive past synth-DB generation and pass it as a new parameter to `upload::upload_all_outputs(store_client, upper_dir, token, &input_closure)`. Each `upload_output` then builds the aho-corasick automaton from the closure's 32-char hash prefixes.
  - Scan happens inside the `dump_path_streaming` tee: add a second writer that feeds bytes to a `ReferenceScanner` which runs the automaton incrementally. After the dump completes, `scanner.into_matches()` returns the set of store-path hashes found → map back to full paths → populate `PathInfo.references` in the trailer.
  - VM test assertion: build a derivation with known references, `SELECT references FROM narinfo WHERE store_path = '...'` → non-empty and matches expected.
- [ ] **xmax-based inserted-check in `cas.rs` upsert** (`rio-store/src/gc/drain.rs:109` TODO + X18 drain blast-radius race) — the drain task's `still_dead` re-check races against a concurrent `chunks` upsert for the same chunk within milliseconds. Add `RETURNING xmax = 0 AS inserted` to the upsert; drain uses `deleted = false AND NOT inserted` to detect a resurrect-then-redelete race.

### Per-tenant GC retention (D4: scheduler-side `path_tenants` upsert)

- [ ] **Migration 009 Part C**: `path_tenants(store_path_hash BYTEA NOT NULL, tenant_id UUID NOT NULL REFERENCES tenants ON DELETE CASCADE, first_referenced_at TIMESTAMPTZ NOT NULL DEFAULT now(), PRIMARY KEY (store_path_hash, tenant_id))` + index on `(tenant_id, first_referenced_at)` for the retention CTE. **No FK→narinfo** (follows `scheduler_live_pins` precedent at `migrations/007_live_pins.sql:15`): the scheduler writes from `built_outputs` which is post-upload so the narinfo row exists, but dropping the FK avoids coupling and lets the mark CTE's `JOIN narinfo` naturally filter any orphan rows (harmless — JOIN produces 0 rows). `tenant_id ON DELETE CASCADE`: deleting a tenant drops their retention claims → those paths fall back to the global grace floor at the next GC. **Drop `narinfo.tenant_id` column** in the same migration part — semantically broken (first-writer-wins), no reader exists, `path_tenants` is authoritative.
- [ ] `SchedulerDb::upsert_path_tenants(output_paths: &[String], tenant_ids: &[Uuid])` — batch `INSERT ... ON CONFLICT DO NOTHING` via `unnest()`. SHA-256 each path for `store_path_hash` (same as `pin_live_inputs` at `db.rs:346`). Called from `actor/completion.rs` after the `output_paths` extraction from `built_outputs` (line ~224 — these are post-upload-confirmed paths), using `output_paths` × `interested_builds.filter_map(|b| b.tenant_id)` (after 4a this is `Option<Uuid>`). If all interested builds have `tenant_id = None` (single-tenant mode), the tenant_ids vec is empty → no-op → paths fall back to global grace retention. **This correctly handles concurrent-dedup**: tenants A+B submit the same derivation → DAG merge dedupes → one execution → completion handler sees BOTH in `interested_builds` → both get rows. `Claims.tenant_id` in HMAC is NOT needed — removed from the design.
- [ ] New GC mark CTE seed (`rio-store/src/gc/mark.rs`): `UNION SELECT n.store_path FROM narinfo n JOIN path_tenants pt USING (store_path_hash) JOIN tenants t USING (tenant_id) WHERE pt.first_referenced_at > now() - make_interval(hours => t.gc_retention_hours)`. Union-of-retention-windows: path survives if ANY tenant's window covers it. Existing global grace seed stays as the floor.
- [ ] Per-tenant quota query (`rio-store/src/gc/tenant.rs`): `SELECT COALESCE(SUM(n.nar_size), 0) FROM narinfo n JOIN path_tenants pt USING (store_path_hash) WHERE pt.tenant_id = $1`. Sum-across-tenants > physical storage (dedup not reflected) — this is correct for a quota: tenant is charged for their working set, not a prorated slice. **Accounting only** in 4b; enforcement (reject SubmitBuild when over quota) is Phase 5.
- [ ] Audit `narinfo.tenant_id` readers before the drop in 009 Part C. No writer exists (store never populates it; `PutPath` doesn't carry tenant). Expected: zero readers. If any debug/test query reads it, update to `JOIN path_tenants` first.

### GC automation

- [ ] `rio-store/src/gc/mod.rs`: `run_gc` orchestration function — wraps `mark` → `sweep` → spawn `drain` in one call, with dry-run flag. `StoreAdminService.TriggerGC` already exists (3b); this consolidates the handler body.
- [ ] `rio-controller/src/gc_schedule.rs`: cron-triggered `TriggerGC` calls. Default daily at 03:00 cluster-local. Emits `rio_controller_gc_runs_total{result=success|failure}` (`observability.md:166` Phase 4+ row).
- [ ] `rio_store_gc_paths_swept_total` + `rio_store_gc_chunks_enqueued_total` counters in sweep.

### Operational tooling

- [ ] **`rio-cli` crate** — clap + tokio, talks to `AdminService` via `rio-proto::client::connect_admin`. Subcommands: `status` (ClusterStatus), `workers` (ListWorkers table), `builds` (ListBuilds with filters), `logs <build_id>` (GetBuildLogs), `gc [--dry-run]` (TriggerGC), `poison clear <drv_hash>` (ClearPoison), `tenants list|create`. Output: human-readable tables by default, `--json` flag for machine consumption.
- [ ] `rio-cli` integration smoke tests against a live scheduler (rio-test-support ephemeral PG + scheduler).
- [ ] **Helm chart** (`infra/k8s/helm/rio-build/`) derived from the Kustomize base. `values.yaml` parameterizes: image tags, PostgreSQL/S3 connection, TLS secret names, size-class cutoffs, GC schedule. Templates: Deployment/StatefulSet per component, ServiceAccount + RBAC, NetworkPolicy, PDB, Secret/ConfigMap. `helm install --dry-run` CI check.
- [ ] `rio-cli` + `dockerImages.rio-cli` added to `flake.nix` packages.

### Defensive hardening

- [ ] **Per-tenant rate limiter on SubmitBuild** (`rio-gateway/src/ratelimit.rs`) — `governor` crate (add to `[workspace.dependencies]`), keyed on `SessionContext.tenant_name` (added in 4a SSH-comment task). `None` tenant = global bucket. Default: 10 builds/min/tenant, burst 30. Over-limit → SSH-level `STDERR_ERROR` with `TooManyRequests`. Limiter held in `Arc<DefaultKeyedRateLimiter<String>>` on the gateway's shared state (one per process, not per `ConnectionHandler`).
- [ ] **Gateway connection-count cap** — hard limit on concurrent SSH sessions (default 1000). At limit → reject new connections with SSH disconnect reason `TOO_MANY_CONNECTIONS`. Handle `RESOURCE_EXHAUSTED` from scheduler (backpressure) by translating to retryable `STDERR_ERROR`.
- [ ] **FUSE circuit breaker** (`rio-worker/src/fuse/circuit.rs`) — track consecutive store-fetch failures. After N failures (default 5), trip open: FUSE `read()` returns `EIO` immediately instead of blocking on a 300s timeout. Half-open probe on the next `ensure_cached` call; auto-close after 30s.
  - **Proto change:** add `bool store_degraded = 9` to `HeartbeatRequest` (`rio-proto/proto/types.proto:321`). Worker sets it from the circuit breaker's open state on each heartbeat.
  - **Scheduler change:** add `WorkerState.store_degraded: bool` (updated in `handle_heartbeat`). `has_capacity()` returns `false` when degraded (same mechanism as `draining`) → `best_worker()` filters degraded workers out naturally. No explicit assignment.rs filter needed.
  - Closes `challenges.md:100` and `failure-modes.md:52` deferrals.
- [ ] **`maxSilentTime` enforcement** (`rio-worker/src/executor/daemon/stderr_loop.rs`) — track `last_output: Instant`. If `elapsed() > max_silent_time` (from `WorkAssignment.build_options`), `cgroup.kill` the build, report `TimedOut`. Currently plumbed through the proto but never checked (`errors.md:96` row).
- [ ] **Per-build overall timeout** — scheduler-side cancellation when `Build.spec.timeout` is exceeded (`errors.md:97` row). `BuildInfo.submitted_at: Instant` already exists (`state/build.rs:114`); `handle_tick` checks `submitted_at.elapsed() > build.options.build_timeout` (if nonzero) → cancel all non-terminal derivations for that build → `transition_build` to Failed with `TimedOut`.
- [ ] **Per-worker failure counts** — separate per-worker failure counter from the distinct-worker set. A derivation failing 5× on worker A should NOT count toward the 3-distinct-workers poison threshold. `failed_workers` stays a `HashSet` for the threshold; add `per_worker_failure_count: HashMap<WorkerId, u32>` for retry-budget decisions within one worker.

### VM test sections

- [ ] **Append to `nix/tests/phase4.nix`** (created in 4a): Section B (GC+references: build with tenant → `narinfo.references` non-empty via NAR scanner, `path_tenants` row exists, backdate `first_referenced_at` past retention + `TriggerGC` → unreferenced path swept, referenced path survives), Section C (rate-limit trip: 11 rapid SubmitBuilds from the same tenant → 11th gets `STDERR_ERROR` `TooManyRequests`), Section D (`maxSilentTime` kill: submit a derivation that sleeps silently → worker kills after timeout → `BuildResult.status == TimedOut`), Section E (`rio-cli` smoke: `status`/`workers`/`builds`/`gc --dry-run` against the live scheduler).

### Tracey markers

- [ ] Add spec `r[...]` markers + `r[impl]`/`r[verify]` annotations for 4b behaviors: `worker.refs.nar-scan`, `worker.fuse.circuit-breaker`, `worker.silence.timeout-kill`, `sched.gc.path-tenants-upsert`, `sched.timeout.per-build`, `gw.rate.per-tenant`, `ctrl.gc.cron-schedule`.

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

`nix-build-remote -- .#checks.x86_64-linux.vm-phase4` passes with Sections A–E.

`helm install --dry-run` renders without errors. `nix develop -c cargo nextest run` passes with new tests for: NAR reference scanner (seeded closure → matches only known refs, no false positives on binary noise), circuit breaker state transitions, rate limiter keyed behavior, `path_tenants` upsert batch shape.
