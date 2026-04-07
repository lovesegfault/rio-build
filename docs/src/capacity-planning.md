# Capacity Planning

This page provides resource sizing guidance for rio-build deployments. All estimates are approximate and should be validated against actual workload data.

## PostgreSQL Storage

| Data Type | Size Per Record | Notes |
|-----------|----------------|-------|
| Derivation (scheduler) | ~1 KB | Includes metadata, edges, assignments |
| narinfo (store) | ~500 bytes | Includes references, signatures |
| Chunk manifest (store) | ~200 bytes | List of (BLAKE3 hash, size) pairs per NAR |
| Build history (scheduler) | ~200 bytes | EMA duration, resource usage per pname/system |

**Worked example --- nixpkgs full rebuild:**
- ~60,000 derivations = ~60 MB scheduler state
- ~80,000 store paths = ~40 MB store metadata + ~16 MB chunk manifests
- Total: ~116 MB active data (plus indexes)

**Recommendation:** Start with 10 GB allocated to PostgreSQL. A single nixpkgs rebuild cycle adds ~120 MB; with GC, steady-state usage plateaus. Monitor `pg_database_size()` and alert at 80% capacity.

## S3 (Object Storage)

| Metric | Estimate | Notes |
|--------|----------|-------|
| nixpkgs full closure (uncompressed NARs) | ~200 GB | All packages for one system |
| With FastCDC dedup | ~100-140 GB | 30-50% chunk dedup savings |
| Inline paths (< 256 KB) | ~60% by count, ~5% by size | Stored as single blobs, no chunking overhead |
| Average chunk size | 64 KB | FastCDC target (min 16 KB, max 256 KB) |
| Incremental rebuild delta | ~5-20 GB | Depends on what changed since last build |

**Recommendation:** Start with 500 GB. Enable S3 lifecycle rules to transition old chunks to infrequent access storage after 90 days. The store's two-phase GC reclaims unreachable chunks.

## Workers

### Sizing Per Worker

One build per pod (P0537). Size the pod for the build, not for a slot count.

| Resource | Recommendation | Notes |
|----------|---------------|-------|
| CPU | 4 vCPU minimum | The build's CPU; Nix's `enableParallelBuilding` uses what's available |
| Memory | 8 GB minimum | Nix sandbox + overlay + FUSE daemon overhead |
| Local SSD (FUSE cache) | 100 GB | Covers ~50% of nixpkgs closure; larger = better hit rate |
| Instance type (AWS) | `m6id.xlarge` (small/medium) | 4 vCPU, 16 GB, 237 GB NVMe |
| Instance type (AWS, large builds) | `c6id.2xlarge` | 8 vCPU, 16 GB, 474 GB NVMe |

### Fleet Sizing

| Metric | Formula | Notes |
|--------|---------|-------|
| Concurrent builds | `workers` | One build per pod |
| Throughput (small builds, ~30s avg) | `workers * 120/hr` | ~120 derivations/hr per worker |
| Throughput (mixed, ~5min avg) | `workers * 12/hr` | ~12 derivations/hr per worker |

**Worked example --- nixpkgs full rebuild (60K derivations):**
- 40 workers = 40 concurrent builds
- With 30s average build time: ~4,800 derivations/hour = ~12.5 hours total
- With 5min average (including large packages): ~480 derivations/hour = ~125 hours total
- Reality is bimodal: most builds are seconds, a few are hours. Expect 15-25 hours for a full nixpkgs rebuild on 40 workers.

**With size-class routing:**
- Small pool (10 workers, 8 concurrent each): handles 90% of builds (short-lived)
- Large pool (2 workers, 2 concurrent each): handles 10% of builds (GCC, LLVM, Firefox)
- Better utilization: small workers aren't blocked by multi-hour builds

The `BuilderPoolSet` CRD wraps this: one BPS defines all size classes declaratively, spawns one child `BuilderPool` per class (ownerReference → cascade delete), and surfaces per-class `effective_cutoff_secs` + `queued` in `.status.classes[]`. See [controller component spec](components/controller.md) for the reconciler flow.

## Gateway and Scheduler

| Component | Replicas | CPU | Memory | Notes |
|-----------|----------|-----|--------|-------|
| Gateway | 2-3 | 1 vCPU | 1 GB | Scales with concurrent SSH connections (~1 KB per connection) |
| Scheduler | 1 active + 1 standby | 2 vCPU | 4 GB | In-memory DAG: ~8 bytes/node + ~16 bytes/edge. 60K-node DAG ≈ 50-100 MB |
| Store | 2-3 | 2 vCPU | 4 GB | LRU chunk cache: configured via `chunk_cache_capacity_bytes` (default 2 GB) |
| Controller | 1 | 0.5 vCPU | 256 MB | Lightweight; mostly waiting for reconcile intervals |

## Monitoring Thresholds

| Metric | Warning | Critical |
|--------|---------|----------|
| PG connection pool utilization | > 70% | > 90% |
| S3 request rate (429 errors) | > 0 sustained | > 10/min |
| Worker queue depth | > 2x worker count | > 5x worker count |
| Scheduler actor queue depth | > 50% capacity (5,000) | > 80% capacity (8,000) |
| FUSE cache hit rate | < 80% | < 50% |
| Build failure rate | > 5% | > 15% |

## Benchmarking against Hydra

The `rio-bench` crate provides criterion benchmarks for `SubmitBuild` latency and dispatch throughput:

```bash
nix run .#bench                                  # all benchmarks, full timing
nix run .#bench -- --bench submit_build          # latency only
nix run .#bench -- --bench dispatch              # throughput only
nix run .#bench -- -- --test                     # smoke (1 iteration, no timing)
```

HTML reports land at `target/criterion/<group>/<param>/report/index.html`. Criterion tracks history across runs in `target/criterion/` — delete it to reset the regression baseline.

The phase 4c target is **p99 SubmitBuild latency < 50 ms at N=100 nodes** on the `submit_build/linear/100` benchmark. This is a target, not a gate; the numbers characterize behavior, they don't block merges.

### Setup

For a fair comparison against `hydra-queue-runner`:

| Axis | Requirement | Why it matters |
|---|---|---|
| Hardware | Identical instance type for both systems | PostgreSQL insert latency is storage-bound; comparing `m6id.xlarge` against `r6id.2xlarge` measures disks, not schedulers. |
| PostgreSQL | Same PG major version, same `shared_buffers`/`work_mem`, both local (not RDS) | Network round-trip to RDS dominates at small N. Run PG co-located with the scheduler under test. |
| Workload | Same nixpkgs subset, same commit | The DAG shape (depth, fan-out) is the primary cost driver. A linear chain vs. a wide tree at identical node count stress different code paths. |
| Warm/cold | Decide up front and be consistent | Hydra caches `.drv` parsing; rio caches nothing across builds. Either run both cold or warm both up with the same DAG first. |

### Metrics to compare

| rio-build | Hydra equivalent | What it measures |
|---|---|---|
| `submit_build/linear/N` latency | enqueue latency: `hydra-eval-jobs` submit → row visible in `buildsteps` | DAG ingest + DB write. At N=100, rio does one gRPC call + batch insert; Hydra does one row per step. |
| `dispatch/binary_tree_drain/depth` throughput (elements/s) | `hydra-queue-runner` log delta: first `building` → last `finished`, with a no-op builder | Ready-scan + assignment-send + completion-handle loop. |
| Time-to-first-dispatch (not yet benchmarked) | `hydra-queue-runner` wakeup latency after `builds` insert | Cold-start responsiveness. Hydra polls `builds` on an interval; rio-scheduler's actor reacts immediately on MergeDag. |

### Expected differences

The two systems place work differently across the pipeline, so identical top-line numbers would be coincidence:

- **DAG reconstruction location.** rio-build parses `.drv` files gateway-side and sends a pre-materialized node/edge list over gRPC. Hydra's `hydra-queue-runner` parses derivations from its own Nix store on the scheduler host. rio's SubmitBuild latency therefore *excludes* parse time; an apples-to-apples measurement starts the clock at "client sends bytes" and stops at "worker receives assignment" — which spans gateway + scheduler for rio, but queue-runner only for Hydra.

- **Batch vs. incremental insert.** rio inserts the full DAG in one `SubmitBuild`. Hydra discovers derivations incrementally as `hydra-eval-jobs` emits them. At large N, rio pays one big insert; Hydra amortizes over the evaluation window. Compare rio's `linear/1000` latency against Hydra's *total* time-to-all-rows-visible, not time-to-first-row.

- **Concurrency model.** `hydra-queue-runner` is single-threaded; rio-scheduler is async but single-actor (one `DagActor` task owns all mutable state). Throughput comparison is meaningful: both are serialization points. Latency comparison needs care — rio's async runtime interleaves gRPC accept, actor processing, and DB writes, so tail latency depends on what else the runtime is scheduling at the moment. Run latency benches with the actor queue empty, not under concurrent load, unless you're specifically measuring contention.

### Caveats

- **DB accumulation.** `rio-bench` uses one ephemeral PostgreSQL for the whole criterion run. Rows accumulate across iterations; at criterion's default sample count (~100) with N=1000, that's ~100K `derivations` rows by the last sample. Later iterations see a bigger DB. For a clean isolated measurement, use `--sample-size 10` or drop N. For characterizing a long-running scheduler, the accumulation is arguably more honest.

- **gRPC overhead in `submit_build`.** The latency benchmark drives the scheduler over real TCP gRPC. That's correct for production characterization (clients are remote) but adds ~1-2 ms of per-call overhead that isn't present in the dispatch benchmark, which drives the actor directly. Don't subtract numbers across the two benchmark groups.

- **No real builds.** Both benchmarks use synthetic DAGs with instant (or no) execution. They measure the scheduler's control plane, not end-to-end build time. For throughput-of-real-builds, use the worker-fleet formula in [Fleet Sizing](#fleet-sizing) above.
