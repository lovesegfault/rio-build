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

**Recommendation:** Start with 500 GB. Enable S3 lifecycle rules to transition old chunks to infrequent access storage after 90 days. The store's GC (Phase 4) reclaims unreachable chunks.

## Workers

### Sizing Per Worker

| Resource | Recommendation | Notes |
|----------|---------------|-------|
| CPU | 4 vCPU minimum | Supports `maxConcurrentBuilds=4` (1 vCPU per build slot) |
| Memory | 8 GB minimum | Nix sandbox + overlay + FUSE daemon overhead |
| Local SSD (FUSE cache) | 100 GB | Covers ~50% of nixpkgs closure; larger = better hit rate |
| Instance type (AWS) | `m6id.xlarge` (small/medium) | 4 vCPU, 16 GB, 237 GB NVMe |
| Instance type (AWS, large builds) | `c6id.2xlarge` | 8 vCPU, 16 GB, 474 GB NVMe |

### Fleet Sizing

| Metric | Formula | Notes |
|--------|---------|-------|
| Concurrent builds | `workers * maxConcurrentBuilds` | Default: 4 builds per worker |
| Throughput (small builds, ~30s avg) | `concurrent_builds * 120/hr` | ~480 derivations/hr per worker |
| Throughput (mixed, ~5min avg) | `concurrent_builds * 12/hr` | ~48 derivations/hr per worker |

**Worked example --- nixpkgs full rebuild (60K derivations):**
- 10 workers, `maxConcurrentBuilds=4` = 40 concurrent builds
- With 30s average build time: ~4,800 derivations/hour = ~12.5 hours total
- With 5min average (including large packages): ~480 derivations/hour = ~125 hours total
- Reality is bimodal: most builds are seconds, a few are hours. Expect 15-25 hours for a full nixpkgs rebuild on 10 workers.

**With size-class routing:**
- Small pool (10 workers, 8 concurrent each): handles 90% of builds (short-lived)
- Large pool (2 workers, 2 concurrent each): handles 10% of builds (GCC, LLVM, Firefox)
- Better utilization: small workers aren't blocked by multi-hour builds

> **Phase 4 deferral — `WorkerPoolSet` CRD:** currently size-class routing is configured per `WorkerPool` via the scheduler's size-class cutoff table (duration + memory thresholds). A `WorkerPoolSet` CRD that groups pools and lets the controller manage cutoffs declaratively is deferred to Phase 4. Until then, deploy multiple independent `WorkerPool` CRs and configure cutoffs in `scheduler.toml`.

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
