# ADR-023: Tiered chunk backend — per-AZ S3 Express read-through cache

Status: Accepted

---

## Context

`ChunkBackend` is rio-store's content-addressed blob storage abstraction. Pre-ADR-023 it was a single `S3ChunkBackend` over one regional S3 standard bucket. Every replica `get()` is one S3 GET unless absorbed by the per-replica moka L1; horizontally scaling rio-store from N → M replicas multiplies cold-miss S3 GET cost by M/N because each replica's moka starts empty. Build wall-clock is dominated by the first build of a closure on each node — every chunk is a cold miss for that node, and S3-standard GET p50 in-region is ~5–15 ms with a long tail under cross-AZ contention.

## Decision

Introduce `TieredChunkBackend` with two `S3ChunkBackend` tiers:

- **`remote`** — the regional S3 standard bucket. **Authoritative.** Every chunk written by any replica lands here. Unbounded.
- **`local`** — one **S3 Express One Zone directory bucket per availability zone**, addressed via the [zonal endpoint](https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-express-Endpoints.html) `https://s3express-{az_id}.{region}.amazonaws.com`. Stateless, disposable, **read-through cache only**.

The tier semantics:

- `put(digest, bytes)` writes to **`remote` only**. Express is never written by the upload path.
- `get(digest)` tries `local`; on miss, reads `remote` **and writes through to `local`**. Read-through is the only way Express fills.
- `local = None` (no Express bucket for this AZ, or `kind = s3`) degrades to pass-through to `remote` — behaviorally identical to the pre-ADR-023 backend.

Each rio-store pod resolves its node's AZ-ID at startup (`topology.kubernetes.io/zone` downward-API → IMDS `placement/availability-zone-id`) and picks the matching Express bucket from helm-supplied config. A pod scheduled to an AZ with no Express bucket (Express is not available in every AZ-ID) runs with `local = None`.

r[infra.express.cache-tier]

The cache tier MUST satisfy: (1) S3 standard is authoritative for every chunk byte — losing or evicting any Express bucket is never data loss; (2) any single Express-bucket or cache-tier-AZ outage degrades only that AZ's replicas to direct S3-standard reads (slower, not down) and leaves other AZs unaffected; (3) `store.chunkBackend.kind` is a single helm value and flipping it from `tiered` back to `s3` is instant and lossless (no migration, no draining, no cache invalidation — Express becomes inert orphan state that the lifecycle policy ages out).

The bounded-cache invariant (`r[infra.express.bounded-eviction]`, [Design Overview §9](./022-design-overview.md#9-tiered-chunk-backend)) keeps each Express bucket a working set, not a mirror: an hourly per-AZ leader-elected sweep deletes oldest-by-`LastModified` objects past a high-watermark down to a low-watermark of `chunk_backend.express.target_bytes` (default 8 TiB). Because Express is filled solely by read-through, `LastModified` tracks the last time any replica in that AZ cold-missed the chunk — moka-hot chunks have stale `LastModified` and are correct to evict (replicas reading them never reach Express). An age-based S3 Lifecycle expiration (30 days) is a defense-in-depth ceiling — directory buckets [support only age-based expiration](https://docs.aws.amazon.com/AmazonS3/latest/userguide/directory-buckets-objects-lifecycle.html), not size targets, so the application sweep is authoritative for the byte budget.

## What this is not

- **Not a CDN.** Express is per-AZ, not per-region or global. Cross-region deployment would need object-store cross-region replication and a globally-consistent metadata store; the cache tier being stateless and metadata-agnostic means it does not preclude that, but cross-region is out of scope here.
- **Not durable.** Express buckets can be deleted at any time. The eviction sweeper, the S3 lifecycle policy, and `tofu destroy` are all valid ways to lose every Express object, and none of them is an outage.
- **Not a builder cache.** Express is a rio-store-internal tier. Builders never see it — they call `GetChunks` on rio-store; the tiering is invisible behind that RPC.
- **Not multi-writer.** PostgreSQL `chunk_refs` is the single-writer arbiter of chunk metadata, single-region. The cache tier carries no metadata.
- **No DRA** (Kubernetes Dynamic Resource Allocation). Express addressing is config, not a scheduled resource.

## Considered alternatives

**FSx for Lustre per AZ.** The original ADR-022 draft used one FSx Lustre filesystem per AZ as `local`, mounted via the `aws-fsx-csi-driver` PVC. Dropped: `ChunkBackend::{get,put}(digest)` is an object API on whole 16–256 KiB blobs — FSx's POSIX surface is unused, while costing a Lustre kernel module on the NixOS AMI (out-of-tree, kernel-version-pinned, kills the "stock-on" Kconfig story), a CSI driver, per-AZ PVC zone-pinning, and a 1.2 TiB ≈ $175/mo/AZ capacity floor. Express reuses the existing `S3ChunkBackend` code path and provisions one `aws_s3_directory_bucket` per AZ. The trade is higher per-chunk read p50 at L2 (~4.5 ms vs sub-ms; behind the moka L1 on a path that already tolerates S3-standard fallback). Per-request cost crosses over with FSx storage cost at roughly 4 K sustained GET/s — far above the design point.

**Warm-on-put.** Writing chunks to Express at `PutPath` time (in addition to the read-through fill) was evaluated and dropped. Freshly-written chunks are moka-hot in the writing replica; the first cross-replica GET in the same AZ pays one S3-standard RTT and fills Express via read-through; subsequent reads hit Express. Warm-on-put would have added ~7 ms PUT latency per chunk for a benefit moka already provides locally and read-through provides cross-replica.

## Deployment prerequisite

S3 Express One Zone is available only in specific AZ-IDs. EKS subnets must land in supported AZ-IDs — verify via `aws ec2 describe-availability-zones --query 'AvailabilityZones[].[ZoneName,ZoneId]'` (the letter suffix `us-east-1a` is account-randomized; the `use1-azN` ID is physical). `infra/eks/variables.tf:express_az_ids` is the intersection of subnet zone-ids with the Express-supported set; an empty list disables the cache tier cluster-wide. See [Design Overview §9](./022-design-overview.md#9-tiered-chunk-backend) for the supported-AZ list snapshot.

## Rollback

`helm upgrade rio infra/helm/rio-build --reuse-values --set store.chunkBackend.kind=s3` — instant, lossless, no migration. Orphaned Express objects age out via the lifecycle policy.

## Cross-references

- [Design Overview §9 — Tiered chunk backend](./022-design-overview.md#9-tiered-chunk-backend) — tier semantics, AZ-ID list, measured latency, bounded eviction
- [Design Overview §10 — Storage tiers](./022-design-overview.md#10-storage-tiers-and-the-binary-cache-compatibility-layer) — what each tier holds, loss semantics
- [Implementation plan P0548/P0553/P0554/P0555/P0585](./022-implementation-plan.md) — `TieredChunkBackend`, terraform, helm, VM test, eviction sweeper
- [`docs/src/components/store.md` §"Tiered chunk backend"](../components/store.md) — `r[store.backend.tiered-get-fallback]`, `r[store.backend.tiered-put-remote-first]`
