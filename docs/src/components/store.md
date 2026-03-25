# rio-store

The global artifact store. Two-layer design inspired by tvix's castore/store split.

## Layer 1: Chunk Store (content-addressed blobs)

r[store.cas.fastcdc]
- NAR archives are split into chunks using FastCDC (content-defined chunking)
- Each chunk is stored by its BLAKE3 hash
- Identical chunks across different store paths are stored once (deduplication)
- Chunks stored in S3-compatible backend (production) or filesystem (dev/test)
- Chunk size target: 64KB average, with min 16KB and max 256KB (compile-time constants in `chunker.rs`)

## Inline Storage Fast-Path

r[store.inline.threshold]
NARs below 256KB (`INLINE_THRESHOLD`, a compile-time constant) are stored directly in the `manifests.inline_blob` PostgreSQL `BYTEA` column, bypassing FastCDC chunking, S3, and manifest indirection entirely. This eliminates per-item overhead for the thousands of tiny `.drv` files found in nixpkgs closures.

Inline blobs never touch S3 — they live entirely in PostgreSQL. The inline/chunked decision is made at `PutPath` time based on NAR size; see the "Inline vs. chunked invariant" in the schema section below.

## Layer 2: Nix Metadata Store

- Maps store paths to their chunk manifests (ordered list of chunk digests)
- Stores narinfo metadata: deriver, NAR hash, NAR size, references, signatures, tenant_id
- Content-addressed index: maps output content hash -> store path (for CA early cutoff)
- Input-addressed index: maps derivation hash -> output store paths (traditional lookups)
- Stored in PostgreSQL (shared with scheduler for query efficiency, separate schema)

## Hash Domain Separation

r[store.hash.domain-sep]
Two hash algorithms are used, in strictly separated domains:

| Context | Hash Algorithm | Example |
|---------|---------------|---------|
| NAR hash in narinfo | SHA-256 | `sha256:1b3a...` |
| Store path computation | SHA-256 | `/nix/store/{hash}-name` |
| narinfo signature | SHA-256 | signed over fingerprint |
| CA output hash | SHA-256 | `ContentLookup` key |
| Chunk storage key | BLAKE3 | `chunks/a3/a3f7...` |

`ContentLookupRequest` takes a SHA-256 NAR hash, never BLAKE3. These domains must never be confused. Inline blobs are not separately keyed — they are stored by `store_path_hash` in the `manifests` table alongside their narinfo.

r[store.content.self-exclude]
The `ContentLookup` RPC MUST accept an optional `exclude_store_path` filter. When set, the lookup returns a match ONLY if the content exists under a DIFFERENT store path. This prevents a just-uploaded output from matching its own `content_index` row during the CA cutoff-compare (which would falsely report "seen before" on the first-ever build, cascading `Skipped` to downstream builds that should run).

## Content Integrity Verification

r[store.integrity.verify-on-put]
- **On PutPath:** The store independently computes SHA-256 over the uploaded NAR stream and verifies it matches the declared `NarHash`. Rejects the upload on mismatch. This prevents corrupted or tampered uploads from being signed and served.

r[store.integrity.verify-on-get]
- **On chunk read (S3 or cache):** Every chunk fetched from S3 or the in-process LRU cache is BLAKE3-verified against its manifest-declared digest. Corrupt chunks are re-fetched (from S3 on cache corruption) or flagged as an error.
- **On inline blob read:** The inline NAR is served directly from PostgreSQL. On `GetPath`, the client can verify the SHA-256 against `narinfo.nar_hash` (the store does not re-hash on every read; integrity is guaranteed by verify-on-put and PostgreSQL's own storage guarantees).

## Chunk Manifest Format

r[store.manifest.format]
Each manifest is stored as a single PostgreSQL row with a `bytea` column containing serialized `(BLAKE3_digest, chunk_size)` pairs --- not one row per chunk. This reduces the INSERT count per `PutPath` from N to 1. Manifests are keyed by store path hash. The serialization format includes a version byte prefix for future evolution.

## Chunk Storage

- **S3 key schema:** `chunks/{first-2-hex-chars}/{full-blake3-hex}` (prefix-partitioned to avoid S3 hotspots)
- Chunks are stored uncompressed in S3 to maximize dedup across packages
- NAR streams are compressed on-the-fly (zstd) when serving via the binary cache HTTP endpoint
- Dedup during `PutPath` uses a `refcount == 1` heuristic: after the refcount UPSERT, chunks whose refcount is exactly 1 were newly inserted by this upload and need to go to S3; chunks with refcount > 1 already existed. This has a small TOCTOU window (two concurrent uploaders of the same chunk may both skip upload), but the race is harmless — `GetPath` BLAKE3-verifies every chunk and surfaces `NotFound` as a clear error, not silent corruption. A separate `FindMissingChunks` gRPC batch-query exists for external callers (e.g., worker-side pre-upload checks).
- **S3 backend requirements:** Strong read-after-write consistency is required. AWS S3 provides this natively. Non-AWS S3-compatible backends (MinIO, Ceph RADOS GW) must be validated for consistency.

## Chunk Lifecycle

Chunk refcounts track how many manifests reference each chunk. The `chunks` table schema:

| Column | Type | Description |
|--------|------|-------------|
| `blake3_hash` | `BYTEA PRIMARY KEY` | BLAKE3 digest of chunk content |
| `refcount` | `INTEGER NOT NULL DEFAULT 0` | Number of manifests referencing this chunk |
| `size` | `BIGINT NOT NULL` | Chunk size in bytes |
| `created_at` | `TIMESTAMPTZ` | Insertion timestamp |
| `deleted` | `BOOLEAN NOT NULL DEFAULT FALSE` | Soft-delete flag (set by GC sweep) |

r[store.chunk.refcount-txn]
**Refcount increment:** In the same PostgreSQL transaction that writes `manifest_data` (step 2 of PutPath). Uses `INSERT ... ON CONFLICT (blake3_hash) DO UPDATE SET refcount = chunks.refcount + 1` — a single UPSERT over the full chunk list via `UNNEST`. PostgreSQL's conflict resolution serializes INSERT vs UPDATE per-row, so concurrent `PutPath` calls with overlapping chunk lists both increment correctly without explicit locking.

r[store.chunk.put-standalone]
The `PutChunk` RPC MUST accept chunks independent of any NAR manifest. A chunk with no manifest reference is held for the grace-TTL before GC eligibility. (Sibling to `r[store.chunk.refcount-txn]`.)

r[store.chunk.grace-ttl]
Chunks with zero manifest references AND `created_at < now() - grace_seconds` are GC-eligible. The grace period prevents a race where a worker's `PutChunk` arrives before its `PutPath` manifest. (Sibling to `r[store.chunk.refcount-txn]`.)

r[store.chunk.tenant-scoped]
`FindMissingChunks` is tenant-scoped via the `chunk_tenants` junction: a chunk is reported present to tenant X IFF a `(blake3_hash, tenant_id=X)` row exists. GC MUST delete junction rows in the same transaction as chunk soft-delete (the `chunks.blake3_hash` FK has `ON DELETE CASCADE`, but chunks are soft-deleted — `UPDATE SET deleted=TRUE`, never `DELETE` — so CASCADE never fires). A stale junction row for a tombstoned chunk would report false-present, causing the worker to skip `PutChunk` for a chunk whose S3 object is already deleted.

**Refcount decrement:** In the same PostgreSQL transaction that deletes a manifest (orphan cleanup of stale `'uploading'` manifests, or GC sweep of unreachable `'complete'` manifests). Uses `UPDATE chunks SET refcount = refcount - 1 WHERE blake3_hash = ANY($1)` — atomic batch decrement.

Chunks with `refcount = 0` are not immediately deleted from S3; they become eligible for GC sweep, which soft-deletes them and enqueues S3 key deletion via the `pending_s3_deletes` table.

## Key Operations

| Operation | Description |
|-----------|-------------|
| `PutPath(narinfo, nar_stream)` | Chunk the NAR, verify NAR hash, deduplicate chunks, store metadata |
| `GetPath(store_path)` | Return narinfo + reconstruct NAR from verified chunks |
| `QueryPathInfo(store_path)` | Return narinfo only |
| `FindMissingPaths(paths)` | Batch validity check (like REAPI's FindMissingBlobs) |
| `ContentLookup(content_hash)` | Find existing store path with matching content (CA cutoff) |

### Write-Ahead Manifest Pattern (PutPath flow)

r[store.put.wal-manifest]
**Authorization:** Worker `PutPath` calls include an HMAC-SHA256-signed assignment token in the `x-rio-assignment-token` gRPC metadata header. The store verifies the token signature, checks expiry, and rejects uploads whose `store_path` is not in `claims.expected_outputs`. Callers presenting a client certificate with `CN=rio-gateway` bypass the HMAC check (the gateway handles `nix copy --to` and has no assignment). See [Security: assignment tokens](../security.md#boundary-2-gatewayworker--internal-services-grpc).

r[store.hmac.san-bypass]
The HMAC bypass check accepts a client certificate whose CN **or** any SAN `DNSName` entry matches the configured allowlist (`hmac_bypass_cns`, default `["rio-gateway"]`). SAN matching enables cert-manager-issued certificates that place identity in SAN extensions rather than CN. The check is CN-first, SAN-second; either match grants bypass.

1. **Idempotency check + `'uploading'` placeholder:** If a `'complete'` manifest already exists for this path, return success immediately (fast-path no-op). Otherwise, insert an `'uploading'` placeholder row in `manifests` as an idempotency lock --- this PG write happens **before** the NAR is buffered or verified.
2. **Buffer + verify:** Accumulate the streamed NAR chunks into a buffer, then compute SHA-256 over the buffered bytes and verify against the declared `NarHash`. On mismatch, delete the placeholder row and reject.
3. **Write-ahead manifest (PG):** Chunk the buffered NAR with FastCDC, then in a single PostgreSQL transaction: write `manifest_data` (serialized chunk list) and UPSERT chunk refcounts. This protects chunks from GC sweep immediately — even if the upload crashes after this point, orphan cleanup will find the stale `'uploading'` row and decrement refcounts correctly.
4. **Dedup check:** Query `chunks.refcount` for all chunks in the manifest. Chunks with `refcount == 1` were newly inserted by step 3 and need S3 upload; chunks with `refcount > 1` already existed before this upload.
5. **Upload new chunks:** Parallel S3 PUTs (8-wide) for the `refcount == 1` subset. No post-upload HeadObject verification — integrity is guaranteed by BLAKE3 verification on every read (see `store.integrity.verify-on-get`).
6. **Complete:** Flip manifest status to `'complete'` in a single PG transaction (also fills real narinfo fields, references, content index entries).

**On graceful error (steps 4–6 return `Err`):** `put_chunked` rolls back refcounts and deletes the `'uploading'` placeholder before returning. Chunks already uploaded to S3 are **not** deleted (GC sweep's responsibility — deleting now would race with a concurrent uploader that just incremented the same chunk).

**On crash (process dies between steps 3 and 6):** the orphan scanner reclaims stale `'uploading'` records after a configurable timeout (default: 2 hours). The chunk list in `manifest_data` is used to decrement refcounts; only chunks whose refcount drops to 0 become eligible for S3 deletion via `pending_s3_deletes`. No full S3 enumeration needed.

r[store.put.idempotent]
**Idempotency:** If `PutPath` is called for a store path that already has a `'complete'` manifest, the call returns success immediately without re-uploading. This makes concurrent uploads of the same path safe.

r[store.atomic.multi-output]
Multi-output derivation registration MUST be atomic at the DB level: all output rows commit in one transaction, or none do. Blob-store writes are NOT rolled back (orphaned blobs are refcount-zero and GC-eligible on the next sweep). The bound is ≤1 NAR-size per failure.

## NAR Reassembly

r[store.nar.reassembly]
- Load the full manifest into memory (list of chunk digests --- even a 10GB NAR is only ~5MB of manifest)
- Parallel chunk prefetch with a sliding window (K=8 concurrent fetches via `futures::stream::buffered()`, which preserves chunk ordering)
- In-process LRU chunk cache (configurable, default 2GB) to avoid repeated S3 round-trips for hot chunks
- BLAKE3-verify every chunk on read (see Content Integrity Verification above)
- Stream the reassembled NAR to the client without materializing the full NAR in memory

## Request Coalescing (Singleflight)

r[store.singleflight]
When multiple concurrent requests need the same chunk from S3 (common during cold starts or thundering herd scenarios), rio-store coalesces them into a single in-flight fetch using a singleflight pattern:

- A `DashMap<[u8; 32], Shared<BoxFuture<'static, Option<Bytes>>>>` tracks in-flight S3 GETs (the fetch is spawned as a tokio task and its `JoinHandle` is mapped to `Option<Bytes>` before `.shared()`, since `JoinError` is not `Clone`)
- First request for chunk X spawns the fetch task and inserts the shared future
- Subsequent requests for chunk X `.await` the existing shared future instead of issuing duplicate S3 GETs
- On completion (success or failure), the entry is removed from the map
- Failed fetches (including task panics) resolve to `None`, and removal from the map means the next request retries cleanly

This is critical for cold start thundering herd: when many builds start simultaneously and request overlapping closures, without coalescing S3 would see O(N*M) GET requests instead of O(M) where N is concurrent builds and M is unique chunks.

## Binary Cache HTTP Server

r[store.http.narinfo]
rio-store serves the standard Nix binary cache protocol so Nix clients can use it as a substituter directly (e.g., `substituters = https://rio-cache.example.com`).

| Endpoint | Description |
|----------|-------------|
| `/nix-cache-info` | Required by Nix clients. Returns `StoreDir`, `WantMassQuery`, `Priority`. |
| `/<hash>.narinfo` | Path metadata (narinfo format with signatures) |
| `/nar/<hash>.nar.zst` | NAR content (zstd-compressed, reassembled from chunks) |

- Served by an embedded axum server in the same rio-store process (consider separate scaling for high-traffic caches)
- Narinfo signatures are pre-computed at `PutPath` time and stored in metadata
- `Cache-Control: public, max-age=31536000, immutable` for NAR and narinfo (content-addressed = immutable)
- `ETag` headers derived from NAR hash for conditional requests
- Compression: serves `.nar.zst` only (target Nix 2.20+ fully supports zstd; `.nar.xz` is not supported to avoid CPU-expensive on-the-fly XZ compression)
- **FileHash/FileSize omission:** narinfo responses omit the optional `FileHash` and `FileSize` fields because NARs are compressed on-the-fly from chunks (no pre-compressed file exists on disk or in S3). Nix falls back to `NarHash`/`NarSize` verification after decompression, which rio-store already guarantees via its content integrity checks.
- `HEAD` requests for narinfo are handled efficiently (metadata lookup, no NAR reassembly)
- **Authentication:** Bearer token authentication for private caches. Nix supports `netrc-file` and `access-tokens` settings for HTTP cache auth.

r[store.cache.auth-bearer]
Binary cache authentication uses per-tenant Bearer tokens mapped via the `tenants.cache_token` column. The auth middleware queries `SELECT tenant_name FROM tenants WHERE cache_token = $1` on each request; a valid token authenticates as that tenant. Unknown/missing token → `401 Unauthorized` with `WWW-Authenticate: Bearer`. Unauthenticated access requires explicit opt-in via `cache_allow_unauthenticated = true` in the store config (default `false` — fail loud). When auth is required but no tenants have tokens configured (misconfigured deployment), requests return `503 Service Unavailable` with a descriptive message so operators notice immediately.

## Signing Key Management

r[store.signing.fingerprint]
- Per-instance signing key stored in a Kubernetes Secret (recommend KMS/Vault for production)
- Signatures are computed at `PutPath` time --- the binary cache server does not need private key access at serve time
- Narinfo `Sig:` field format: `<key-name>:<base64-ed25519-signature>` (compatible with `nix.settings.trusted-public-keys`)
- Signed message: canonical fingerprint `1;<store-path>;sha256:<nar-hash-nixbase32>;<nar-size>;<sorted-refs-comma-sep>` — semicolon separator, `1;` version prefix, `sha256:` algorithm tag, references are full paths (not basenames) joined by comma. Matches Nix's `ValidPathInfo::fingerprint()` in `path-info.cc`. See `fingerprint()` in `rio-nix/src/narinfo.rs`.
- Multi-tenant: each tenant can have their own signing key for their paths

r[store.signing.empty-refs-warn]
When signing a non-CA path with zero references, the store MUST emit a warning. Non-leaf derivations with empty references indicate the worker's reference scanner missed deps.

r[store.tenant.sign-key]
narinfo signing MUST use the tenant's active signing key from `tenant_keys` when present, falling back to the cluster key otherwise. A tenant with its own key produces narinfo that `nix store verify --trusted-public-keys tenant:<pk>` accepts for that tenant's paths only.

r[store.tenant.narinfo-filter]
Authenticated narinfo requests MUST filter results by `path_tenants.tenant_id = auth.tenant_id`. Anonymous (unauthenticated) requests return unfiltered results for backward compatibility.

### Key Rotation

1. Generate a new ed25519 signing key
2. Add the new public key to all clients' `trusted-public-keys` configuration
3. New paths are signed with the new key immediately
4. Existing active paths are re-signed during GC (mark phase signs reachable paths with the new key)
5. After a grace period (default: 30 days), remove the old key from `trusted-public-keys`

### Realisation Signing

CA `Realisation` objects carry their own ed25519 signatures over the tuple `(drv_hash, output_name, output_path, nar_hash)`. This provides integrity for content-addressed output mappings independently of narinfo signatures.

## Two-Phase Garbage Collection

r[store.gc.two-phase]
- **Phase 1 (Mark):** Identify paths unreachable from GC roots via a recursive CTE over `narinfo."references"`. GC root seeds: explicit pins in the `gc_roots` table, auto-pinned live-build inputs in the `scheduler_live_pins` table, manifests with `status='uploading'` (in-flight PutPath), paths with `created_at > now() - grace_hours` (recent uploads), and `extra_roots` passed from the scheduler's live-build output paths (`ActorCommand::GcRoots`). Mark takes an **exclusive** advisory lock (`GC_MARK_LOCK_ID = 0x724F47430002`); PutPath takes the same lock **shared** around its placeholder-insert → complete-manifest window. This blocks PutPath for ~1s (CTE duration) during mark.
- **Grace period:** Configurable per-invocation via `GcRequest.grace_period_hours` (default **2h**). Protects paths uploaded shortly before GC that builds haven't referenced yet.
- **Phase 2 (Sweep):** Re-read chunk refcounts at sweep time (NOT from a mark-phase snapshot). Per unreachable path, in batched transactions: `SELECT chunk_list ... FOR UPDATE OF m` locks the **manifest** row (not chunk rows), then sweep **re-checks references** (GIN-indexed) --- if a PutPath completed between mark and sweep with a reference to this path, the delete is skipped (`rio_store_gc_path_resurrected_total` metric). DELETE narinfo (CASCADE), decrement chunk refcounts, mark `refcount=0` chunks deleted, enqueue S3 keys to `pending_s3_deletes` --- all in the same PG transaction. Sweep does NOT hold the mark advisory lock.

**Orphan cleanup:** Stale `'uploading'` manifests are reclaimed after a configurable timeout (default: 2 hours). Their chunk lists are used to decrement refcounts for referenced chunks; only chunks whose refcount drops to 0 are eligible for deletion via `pending_s3_deletes`. No full S3 enumeration needed. A weekly full orphan scan remains as a safety net for any leaked chunks not covered by manifest-based cleanup.

r[store.gc.tenant-retention]
A store path survives GC if *any* tenant that has referenced it still
has the path inside its retention window. This is the 6th UNION arm in
the mark phase CTE (after roots, uploading-manifests, global grace,
extra-roots, and scheduler-live-pins): it joins `path_tenants` against
`tenants.gc_retention_hours` — `WHERE pt.first_referenced_at > now() -
make_interval(hours => t.gc_retention_hours)`. Union-of-retention
semantics: the most generous tenant wins. The global grace period
(`narinfo.created_at` window) is a floor; tenant retention extends it
but never shortens it. An empty `path_tenants` table makes this arm a
no-op (0 rows contributed).

r[store.gc.tenant-quota]
Per-tenant store accounting sums `narinfo.nar_size` over all paths
the tenant has referenced (`JOIN path_tenants USING (store_path_hash)
WHERE tenant_id = $1`). This is the accounting query; enforcement is
the sibling `r[store.gc.tenant-quota-enforce]` below (gateway rejects
SubmitBuild over quota).

r[store.gc.tenant-quota-enforce]
The gateway MUST reject `SubmitBuild` with `STDERR_ERROR` when `tenant_store_bytes(tenant_id)` exceeds `tenants.gc_max_store_bytes`. Enforcement is eventually-consistent — `tenant_store_bytes` may be cached with ≤30s TTL. The connection stays open; the user can retry after GC. (Sibling to `r[store.gc.tenant-quota]` — distinguishes enforcement from accounting.)

Supports dry-run mode via `GCRequest.dry_run`.

r[store.gc.empty-refs-gate]
Before the mark phase, GC MUST check the ratio of sweep-eligible paths with empty references. If >10%, refuse with `FailedPrecondition` unless `force=true`. This prevents mass deletion when the worker's reference scanner is broken.

r[store.cas.upsert-inserted]
The chunk-upsert batch INSERT returns per-row `(refcount = 1) AS inserted`
so the caller knows which blake3 hashes need upload to backend. This
replaces the racy re-query: previously `do_upload` re-SELECTed
`refcount` after the upsert, but a concurrent PutPath could bump
refcount between upsert and re-SELECT. `refcount = 1` is atomic with
the INSERT. It is also SAFER than `xmax = 0`: a chunk at refcount=0
(soft-deleted, awaiting GC drain) that gets resurrected by upsert has
`xmax != 0` (CONFLICT fired) but `refcount = 1` — the `xmax` approach
would skip upload, but S3 may have already deleted the object.

## Crash-Safe S3 Deletion (`pending_s3_deletes`)

r[store.gc.pending-deletes]
S3 deletes are not transactional with PostgreSQL. To prevent data leaks (chunks removed from PG but never deleted from S3) or premature deletes on crash, rio-store uses a transactional outbox pattern:

1. In the same PostgreSQL transaction that marks chunks as `deleted=true` (GC sweep) or decrements refcounts to 0 (orphan cleanup), write the corresponding S3 keys **and `blake3_hash`** to the `pending_s3_deletes` table.
2. A background drain task polls `pending_s3_deletes` on a **fixed 30s interval** (`DRAIN_INTERVAL`, not exponential --- S3 DELETE failures are rare and transient; queueing absorbs bursts). Before issuing each S3 DELETE, drain **re-checks the chunk state by `blake3_hash`** (TOCTOU guard: if a concurrent PutPath re-incremented the refcount since sweep enqueued the key, skip the delete and remove the row --- `rio_store_gc_chunk_resurrected_total` metric). On S3 success, the row is removed; on failure, `attempts` is incremented.
3. On crash/restart, unprocessed rows are retried automatically --- S3 DELETE is idempotent.
4. Rows exceeding max retry count (default: 10) remain in the table for alerting (`rio_store_s3_deletes_stuck` gauge).

**GC-vs-GC serialization:** `TriggerGC` takes a non-blocking advisory lock (`GC_LOCK_ID = 0x724F47430001`) via `pg_try_advisory_lock`. A second concurrent `TriggerGC` gets "already running" and returns early.

## PostgreSQL Schema

Pseudo-DDL for all store tables. `narinfo` and `manifests` are split to avoid TOAST write amplification when updating manifest status.

```sql
CREATE TABLE narinfo (
    store_path_hash    BYTEA PRIMARY KEY,
    store_path         TEXT NOT NULL,
    deriver            TEXT,
    nar_hash           BYTEA NOT NULL,          -- SHA-256
    nar_size           BIGINT NOT NULL,
    "references"       TEXT[] NOT NULL DEFAULT '{}',  -- quoted: PG reserved keyword
    signatures         TEXT[] NOT NULL DEFAULT '{}',
    ca                 TEXT,                    -- content address (empty string for input-addressed)
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    registration_time  BIGINT NOT NULL DEFAULT 0,
    ultimate           BOOLEAN NOT NULL DEFAULT FALSE
);
```

> The `manifests` + `manifest_data` + `chunks` tables are the active schema as of Phase 2c (migration `002_store.sql` dropped the Phase 2a `nar_blobs` table). Small NARs (< 256 KiB) store inline in `manifests.inline_blob`; larger NARs are FastCDC-chunked with BLAKE3 dedup. ChunkBackend is constructed from config (`ChunkBackendKind` enum: `Inline` / `Filesystem` / `S3`, default `Inline` for back-compat).

```sql
CREATE TABLE manifests (
    store_path_hash  BYTEA PRIMARY KEY
                     REFERENCES narinfo(store_path_hash),
    status           TEXT NOT NULL DEFAULT 'uploading'
                     CHECK (status IN ('uploading', 'complete')),
    inline_blob      BYTEA,                   -- non-NULL ⇒ inline storage fast-path
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Split from manifests: chunk_list is large (bytea, often TOASTed).
-- Keeping it in a separate table avoids rewriting the TOAST pointer
-- every time manifests.status is updated.
CREATE TABLE manifest_data (
    store_path_hash  BYTEA PRIMARY KEY
                     REFERENCES manifests(store_path_hash),
    chunk_list       BYTEA NOT NULL            -- versioned serialization of
                                               -- (BLAKE3_digest, chunk_size) pairs
);
```

**Inline vs. chunked invariant:** If `manifests.inline_blob IS NOT NULL`, no corresponding `manifest_data` row exists — the NAR content is stored entirely in the `inline_blob` field. Conversely, if `inline_blob IS NULL`, a `manifest_data` row MUST exist with a valid `chunk_list`. Code must check `inline_blob` first; only if it is NULL should `manifest_data` be queried. The `manifest_data` foreign key does not require a row to exist (no `ON DELETE` action forces creation).

```sql
CREATE TABLE chunks (
    blake3_hash      BYTEA PRIMARY KEY,
    refcount         INTEGER NOT NULL DEFAULT 0,
    size             BIGINT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted          BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE INDEX idx_chunks_gc ON chunks (blake3_hash)
    WHERE refcount = 0 AND deleted = FALSE;

-- Per-tenant path visibility is in the path_tenants junction (migration 012),
-- not a per-column tenant_id. content_index.tenant_id remains nullable and
-- unused (migration 002 predates the junction-table design).
CREATE TABLE content_index (
    content_hash     BYTEA NOT NULL,           -- SHA-256 output content hash
    store_path_hash  BYTEA NOT NULL
                     REFERENCES narinfo(store_path_hash) ON DELETE CASCADE,
    tenant_id        UUID,
    PRIMARY KEY (content_hash, store_path_hash)
);

-- CA derivation realisations (populated on CA build completion)
CREATE TABLE realisations (
    drv_hash         BYTEA NOT NULL,              -- modular derivation hash
    output_name      TEXT NOT NULL,                -- e.g. 'out', 'dev'
    output_path      TEXT NOT NULL,
    output_hash      BYTEA NOT NULL,              -- SHA-256 content hash of output NAR
    signatures       TEXT[] NOT NULL DEFAULT '{}', -- ed25519 realisation signatures
    tenant_id        UUID,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (drv_hash, output_name)
);
CREATE INDEX realisations_output_idx ON realisations (output_path);
```

> GC tables (`gc_roots`, `scheduler_live_pins`, `pending_s3_deletes`) are added by `migrations/006_gc_safety.sql`. See the `rio-store/src/gc/` module for the mark/sweep/drain implementation.

```sql
CREATE TABLE gc_roots (
    store_path_hash  BYTEA PRIMARY KEY
                     REFERENCES narinfo(store_path_hash),
    source           TEXT NOT NULL,               -- 'admin' / 'test' / etc
    pinned_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Scheduler auto-pins input closures of dispatched derivations.
-- Unpinned on completion. NOT FK'd to narinfo (input paths may not
-- be in the local store yet — they'll arrive via worker upload).
CREATE TABLE scheduler_live_pins (
    store_path_hash  BYTEA NOT NULL,
    drv_hash         TEXT NOT NULL,
    pinned_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (store_path_hash, drv_hash)
);

CREATE TABLE pending_s3_deletes (
    id               BIGSERIAL PRIMARY KEY,
    s3_key           TEXT NOT NULL,
    blake3_hash      BYTEA,                       -- drain re-checks chunk state
                                                   -- before S3 DELETE (TOCTOU guard)
    enqueued_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    attempts         INTEGER NOT NULL DEFAULT 0,
    last_error       TEXT
);
CREATE INDEX idx_pending_s3_deletes_drain
    ON pending_s3_deletes (enqueued_at)
    WHERE attempts < 10;
```

## Design References (no code dependencies)

- tvix `castore` protobuf definitions (MIT-licensed): inform our CAS gRPC API design
- tvix `store` protobuf definitions (MIT-licensed): inform our PathInfo API design
- Attic's FastCDC-based chunking approach: reference for chunk size tuning and dedup strategy
- NAR and narinfo formats: implemented from scratch in `rio-nix`

## Key Files (Phase 3a)

- `rio-store/src/grpc/` --- StoreService gRPC implementation
  - `mod.rs` — service struct + shared state
  - `put_path.rs` — PutPath handler (buffer, verify, branch inline/chunked)
  - `get_path.rs` — GetPath handler (manifest load, parallel reassembly stream)
  - `chunk.rs` — FindMissingChunks batch query RPC
- `rio-store/src/metadata/` --- narinfo + manifest persistence (PostgreSQL)
  - `mod.rs` — re-exports + shared types
  - `inline.rs` — inline-blob fast path (write `manifests.inline_blob` BYTEA)
  - `chunked.rs` — chunked-path manifest + refcount UPSERT
  - `queries.rs` — narinfo SELECT/UPDATE, QueryPathInfo, FindMissingPaths
- `rio-store/src/validate.rs` --- NAR hash verification (HashingReader, NarDigest, validate_nar_digest)
- `rio-store/src/backend/` --- ChunkBackend trait + S3/filesystem/memory impls
- `rio-store/src/cas.rs` --- put_chunked orchestration, ChunkCache (moka LRU + singleflight + BLAKE3 verify)
- `rio-store/src/chunker.rs` --- FastCDC wrapper (16K/64K/256K min/avg/max)
- `rio-store/src/manifest.rs` --- Chunk manifest (de)serialization, versioned binary format
- `rio-store/src/content_index.rs` --- nar_hash → store_path reverse index (CA ContentLookup)
- `rio-store/src/realisations.rs` --- CA (drv_hash, output_name) → output_path mapping
- `rio-store/src/cache_server/` --- axum binary cache HTTP (narinfo + nar.zst routes)
- `rio-store/src/signing.rs` --- ed25519 narinfo signing at PutPath time

## GC Files

- `rio-store/src/gc/mod.rs` --- GC lock constants (`GC_LOCK_ID`, `GC_MARK_LOCK_ID`), `GcStats`, `run_gc` orchestration
- `rio-store/src/gc/mark.rs` --- `compute_unreachable` recursive CTE (seeds: gc_roots, scheduler_live_pins, uploading manifests, grace-period, extra_roots)
- `rio-store/src/gc/sweep.rs` --- Per-path batched delete with `FOR UPDATE OF m` + reference re-check
- `rio-store/src/gc/drain.rs` --- Background S3 delete drain (30s interval, blake3_hash re-check)
- `rio-store/src/gc/orphan.rs` --- Stale `'uploading'` manifest cleanup
- `rio-store/src/grpc/admin.rs` --- `StoreAdminService` (TriggerGC, PinPath, UnpinPath)
