# Key Challenges

## 1. Nix Worker Protocol Fidelity

The ssh-ng / daemon protocol is complex, versioned, and not formally specified. We need to handle version negotiation, all required opcodes, and edge cases. Key references:

- [Snix protocol docs](https://snix.dev/docs/reference/nix-daemon-protocol/)
- [Tweag: Re-implementing the Nix protocol in Rust](https://www.tweag.io/blog/2024-04-25-nix-protocol-in-rust/)
- Nix C++ source: `worker-protocol.hh`, `daemon.cc`, `remote-store.cc`

## 2. Store Path Transfer Efficiency

Moving closures between rio-store and workers is the main bottleneck. Strategies:

- NAR streaming (don't materialize full NARs in memory)
- Chunk-level deduplication (FastCDC) for incremental transfers
- Worker affinity to minimize transfers
- Pre-fetching: scheduler sends prefetch hints to the worker's FUSE cache before assigning work
- Per-worker FUSE store with local SSD cache provides local-disk performance for hot paths without shared infrastructure

## 3. CA Early Cutoff Correctness

When a CA derivation's output matches cached content, we must correctly propagate the cutoff through the DAG. This requires careful state management in the scheduler --- a cutoff at node N means all transitive dependents of N can potentially skip rebuilding if their other inputs are also unchanged.

## 4. IFD (Import-From-Derivation)

Nix evaluation may block on build results. The gateway must handle this gracefully --- the client sends a build request mid-evaluation, and rio must prioritize these "evaluation-blocking" builds. These show up as individual `wopBuildDerivation` calls that arrive before the full DAG is known.

## 5. Worker Store Lifecycle

Workers need a functional `/nix/store`. The FUSE + overlay approach introduces complexity:

- Upper layer cleanup must be deterministic (unique per-build directory, discarded with the pod)
- The namespace ordering (FUSE mount → overlayfs → nix sandbox) must be correct; see [builder.md](components/builder.md)

**Decided approach:** Each worker runs a FUSE filesystem (`rio-fuse`) that lazily fetches store paths from rio-store. Each build gets a per-build overlayfs with **stacked lowers** (host `/nix/store` first so nix-daemon and its dependencies are reachable, FUSE mount second for lazily-fetched paths) and a per-build synthetic SQLite database in the upper layer. This avoids shared mutable state, eliminates shared PV infrastructure, and provides local-disk performance via SSD caching. See [builder.md](components/builder.md) for full details.

## 6. Failure Semantics

Nix builds can fail in many ways (build error, timeout, OOM, sandbox violation). rio must faithfully report failures back through the protocol, mapping internal failure states to the correct `BuildResult::Status` values (PermanentFailure, TransientFailure, TimedOut, etc.).

## 7. Worker Pod Security

overlayfs and the Nix sandbox both require `CAP_SYS_ADMIN` + `CAP_SYS_CHROOT`. This conflicts with PodSecurityStandards on managed Kubernetes clusters (EKS, GKE, AKS).

Mitigations:

- Dedicated node pools with relaxed pod security policies for worker pods
- Custom seccomp profiles that allow only the specific syscalls needed (mount, pivot_root)
- NetworkPolicy isolation to restrict worker pod network access

**Important:** The Nix sandbox is NOT a security boundary --- it's a purity mechanism that prevents builds from accessing paths outside their declared inputs. For multi-tenant deployments, the actual security boundary is the worker pod and node isolation provided by Kubernetes.

## 8. CAS Durability Under Partial Failure

The content-addressable store has two failure modes during writes:

- **Orphaned chunks:** Chunk upload succeeds but metadata write fails, leaving unreferenced chunks in blob storage
- **Broken manifest:** Metadata write succeeds but some chunks are missing, producing an unreadable manifest

**Solution:** Write-ahead manifest pattern --- write chunk references to a pending manifest before uploading chunks, then promote to committed after all chunks are verified. See [store.md](components/store.md) for the full write-ahead protocol.

## 9. Cold Start at Scale

When many workers start simultaneously (scale-up event), all FUSE caches are cold. Every worker needs to fetch the same common dependencies (glibc, coreutils, etc.) from rio-store, creating a thundering herd on the store's S3 backend.

**Mitigations:**
- **Implemented:** In-process LRU chunk cache on rio-store (`ChunkCache`, moka-based, default 2 GiB) reduces S3 round-trips for hot chunks
- **Implemented:** Per-derivation prefetch hints --- scheduler sends input-path prefetch to the assigned worker before dispatch, so the FUSE cache warms during the scheduling window rather than on first `read()`

> **Implemented:** staggered scheduling with cold-start prefetch warm-gate (`r[sched.assign.warm-gate]`). Relevant for ephemeral builders (every build cold-starts — `r[ctrl.pool.ephemeral]`).

## 10. PostgreSQL as Bottleneck

The scheduler and store share a PostgreSQL cluster. High-throughput builds (e.g., full nixpkgs rebuild) generate heavy write load from derivation state transitions and chunk manifest writes.

**Mitigations:**
- Connection pooling via PgBouncer
- Read replicas for dashboard queries (AdminService reads can use a read-only endpoint)
- Separate PostgreSQL instances for store vs. scheduler if write contention becomes an issue
- Async/batched writes for non-critical state (duration estimates, dashboard status) --- see [scheduler.md](components/scheduler.md#synchronous-vs-async-writes)

## 11. FUSE Cache Miss Cascading Failure

When rio-store is overloaded or degraded, FUSE cache misses on workers become slow. This creates a cascading failure loop:

1. rio-store slow -> FUSE reads block -> builds stall
2. Workers appear slow (actual_time >> estimated_time) -> scheduler considers them degraded
3. Scheduler assigns work to other workers -> those workers also hit rio-store -> amplification
4. All workers stall -> scheduler queue depth grows -> controller scales up workers -> more rio-store load

**Mitigations:**
- **FUSE fetch timeout:** `fetch_extract_insert` wraps the entire gRPC-fetch-plus-stream-drain in `GRPC_STREAM_TIMEOUT` (300s). A stalled store returns `EIO` to the build rather than blocking a FUSE thread forever. Concurrent-fetch waiters (threads that hit the same path while a fetch is already in flight) time out after 30s and return `EAGAIN` (retryable) --- the wait is defensive, since the fetcher's Drop-guard fires even on panic.
- **Scheduler cache-check circuit breaker:** the scheduler's `FindMissingPaths` cache-check trips open after 5 consecutive failures (`CacheCheckBreaker`). While open, SubmitBuild is rejected with `StoreUnavailable` instead of queueing every derivation as a cache miss. Half-open probe closes the breaker on the first success; auto-closes after 30s even without a probe.
- **Scheduler backpressure:** actor-queue-depth hysteresis (80% activate, 60% deactivate) refuses new submissions when the actor is overloaded. Not store-health-aware --- it responds to actor congestion regardless of cause.
- **Worker FUSE circuit breaker:** the worker tracks consecutive store-fetch failures; when the breaker opens, `HeartbeatRequest.store_degraded` is set and the scheduler excludes the worker from assignment via `has_capacity()`. See `r[builder.fuse.circuit-breaker]` + `r[builder.heartbeat.store-degraded]`.

## 12. Scheduler In-Memory DAG Scalability

The scheduler maintains the entire global DAG in memory via a single-owner actor model. A full nixpkgs rebuild has 50,000+ derivation nodes. Multiple concurrent nixpkgs rebuilds (e.g., from different tenants or branches) multiply this.

**Concerns:**
- Memory consumption: each derivation node carries metadata (hash, pname, system, status, priority, edges). At 50K+ nodes with edge lists, a single DAG can consume hundreds of MB.
- Actor throughput: all mutations go through a single `mpsc` channel. Critical-path recomputation across a large DAG could cause head-of-line blocking.
- DAG merge cost: merging two large DAGs requires deduplication by `drv_hash`, which is O(n) per merge.

**Mitigations:**
- Profile memory and throughput during Phase 2c benchmarks (target: 60K-node DAG in < 500MB, actor processes > 1000 ops/sec)
- Consider offloading compute-heavy operations (critical-path recomputation) to a background task with dirty-flag coalescing
- Bound individual submissions: `MAX_DAG_NODES = 1,048,576` / `MAX_DAG_EDGES = 5,242,880` --- global compile-time constants (`rio-common/src/limits.rs`), not per-tenant. SubmitBuild rejects DAGs exceeding either limit before merge.

## 13. FUSE Local I/O Performance

The FUSE daemon (`rio-fuse`) runs in userspace via the `fuser` crate. FUSE context switches between kernel and userspace could become a latency bottleneck --- even when the SSD cache is warm. This is a different risk from Challenge 11 (rio-store overload); this is about the local I/O path.

**Concerns:**
- Each file `read()` from the build sandbox crosses kernel → userspace → kernel. For builds that read thousands of small files (e.g., header-heavy C++ compilations), the overhead accumulates.

**Mitigations:**
- Benchmark FUSE read latency (p50, p99) during the Phase 1a spike under concurrent load
- Compare against direct filesystem reads to quantify overhead
- The `fuser` crate supports multi-threaded FUSE dispatch; ensure this is enabled
- **FUSE passthrough mode (Linux 6.9+):** For cached paths on local SSD, FUSE passthrough (`FUSE_PASSTHROUGH`) eliminates the `read()` context switch by handing off file descriptors to the backing files. See [rio-builder: FUSE Passthrough Mode](./components/builder.md#fuse-passthrough-mode-linux-69).
- **File handle caching:** Keep backing file handles open across reads. Passthrough only helps for `read()` on already-open files; `lookup()` and `open()` still traverse userspace. Builds that open many small files once (header-heavy C++) won't benefit from passthrough alone --- they need reduced `open()` overhead via kernel entry/attribute caching (high TTL on immutable store paths).
- If FUSE overhead exceeds 2x vs direct reads even with all mitigations, consider the bind-mount fallback

**Phase 1a spike results (EKS AL2023, kernel 6.12, c8a.xlarge):**

Standard FUSE overhead was 10-50x vs direct reads (p50, varying concurrency 1-16). FUSE passthrough (`fuser` 0.17, `FUSE_PASSTHROUGH`) showed no improvement for the open-read-close-per-file benchmark pattern because `lookup()`/`open()` dominate, not `read()`. The overhead is acceptable for the architecture (the full FUSE → overlayfs → nix-build chain works), but production `rio-fuse` must optimize the `open()` path via file handle caching and aggressive attribute/entry TTLs. See [archived benchmark data](./phases-archive/phase1a.md).

## 14. Size-Class Cold Start and Misclassification

> **Implemented:** `BuilderPoolSet` CRD (P0232), child-builder reconciler (P0233), per-class status refresh + autoscaler (P0234). See [controller.md](components/controller.md#builderpoolset). Alternative for simpler deployments: multiple independent `BuilderPool` CRs with operator-configured cutoffs in `scheduler.toml`.

With size-class routing, two related challenges arise:

**Cold start:** On a fresh deployment with no `build_history` data, all derivations use the operator-configured cutoffs or the default fallback (30s estimate). These initial cutoffs may be wildly wrong for the actual workload, leading to poor classification until sufficient data accumulates. Mitigation: allow operators to seed `build_history` from external sources (e.g., Hydra build logs, previous rio-build deployments), and use conservative initial cutoffs that over-classify into larger classes (wastes resources but avoids OOM kills).

**Misclassification cascades:** A derivation that is consistently misclassified (e.g., a build whose duration depends on network speed for FODs, or on source code changes) creates oscillation: it gets routed to a small class, exceeds the cutoff, gets bumped to large, then gets routed back to small after the EMA decays. **Implemented mitigation: penalty-overwrite** (`r[sched.classify.penalty-overwrite]`) --- when actual duration exceeds 2× the assigned class's cutoff, the scheduler overwrites `ema_duration_secs` with the observed value directly (no alpha-blend). The next `classify()` call sees the real duration and picks the right class. A fluke self-corrects via normal EMA blending on the next successful completion; no consecutive-failure counter is tracked.

**Queue imbalance:** If all ready derivations happen to be "medium" but only "small" workers are idle, derivations queue unnecessarily. Mitigation: overflow routing allows small-class derivations to spill to medium workers when the small queue is empty, but never routes large derivations downward.

## 15. Schema Migration

Database schema evolves across phases (new tables, new columns, index changes). Migrations must be:

- **Forward-compatible**: old code must tolerate new columns (use `ADD COLUMN ... DEFAULT`)
- **Versioned**: use `sqlx migrate` with numbered migration files
- **Tested**: rollback scripts for each migration, tested in CI
- **Blue-green compatible**: during deployment, both old and new code versions may run simultaneously
