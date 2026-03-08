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
- `FindMissingChunks` MUST be called before uploading to avoid redundant S3 PUTs
- **S3 backend requirements:** Strong read-after-write consistency is required. AWS S3 provides this natively. Non-AWS S3-compatible backends (MinIO, Ceph RADOS GW) must be validated for consistency.

## Chunk Lifecycle

Chunk refcounts track how many manifests reference each chunk. The `chunks` table schema:

| Column | Type | Description |
|--------|------|-------------|
| `blake3_hash` | `BYTEA PRIMARY KEY` | BLAKE3 digest of chunk content |
| `refcount` | `INTEGER NOT NULL DEFAULT 0` | Number of manifests referencing this chunk |
| `size` | `BIGINT` | Chunk size in bytes |
| `created_at` | `TIMESTAMPTZ` | Insertion timestamp |
| `deleted` | `BOOLEAN NOT NULL DEFAULT FALSE` | Soft-delete flag (set by GC sweep) |

r[store.chunk.refcount-txn]
**Refcount increment:** In the same PostgreSQL transaction that writes `manifest_data` (step 2 of PutPath). Uses `INSERT ... ON CONFLICT (blake3_hash) DO UPDATE SET refcount = chunks.refcount + 1` — a single UPSERT over the full chunk list via `UNNEST`. PostgreSQL's conflict resolution serializes INSERT vs UPDATE per-row, so concurrent `PutPath` calls with overlapping chunk lists both increment correctly without explicit locking.

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
**Authorization:** Worker `PutPath` calls must include a valid assignment token (HMAC-SHA256, issued by the scheduler). The store verifies the token signature and checks that the output path matches `expected_output_paths`. See [Security: assignment tokens](../security.md#boundary-2-gatewayworker--internal-services-grpc).

> **Phase 2a deferral:** Assignment token HMAC signing and verification are deferred to Phase 3 (with leader election). In Phase 2a, tokens are opaque strings (`{worker_id}-{drv_hash}-{generation}`) and the store does not verify them. This matches the `types.proto` comment on `WorkAssignment.assignment_token`.

1. Write manifest to PostgreSQL with `status='uploading'` (includes chunk list and chunk refcount increments --- this protects chunks from GC sweep immediately)
2. Upload all chunks to S3 (call `FindMissingChunks` first to skip existing)
3. Verify all chunks present in S3 (via HeadObject)
4. Independently compute SHA-256 of the reassembled NAR and verify it matches the declared hash
5. Flip manifest status to `'complete'` in a single PG transaction (also writes narinfo, references, content index entries)

On failure at any step, the orphan scanner reclaims stale `'uploading'` records after a configurable timeout (default: 2 hours). The chunk list in the stale manifest is used to decrement refcounts for referenced chunks. Only chunks whose refcount drops to 0 become eligible for S3 deletion via the `pending_s3_deletes` table. No full S3 enumeration needed.

r[store.put.idempotent]
**Idempotency:** If `PutPath` is called for a store path that already has a `'complete'` manifest, the call returns success immediately without re-uploading. This makes concurrent uploads of the same path safe.

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
- **Authentication:** Bearer token or `netrc`-compatible authentication for private caches. Nix supports `netrc-file` and `access-tokens` settings for HTTP cache auth.

## Signing Key Management

r[store.signing.fingerprint]
- Per-instance signing key stored in a Kubernetes Secret (recommend KMS/Vault for production)
- Signatures are computed at `PutPath` time --- the binary cache server does not need private key access at serve time
- Narinfo `Sig:` field format: `<key-name>:<base64-ed25519-signature>` (compatible with `nix.settings.trusted-public-keys`)
- Signed message: canonical fingerprint `1;<store-path>;sha256:<nar-hash-nixbase32>;<nar-size>;<sorted-refs-comma-sep>` — semicolon separator, `1;` version prefix, `sha256:` algorithm tag, references are full paths (not basenames) joined by comma. Matches Nix's `ValidPathInfo::fingerprint()` in `path-info.cc`. See `fingerprint()` in `rio-nix/src/narinfo.rs`.
- Multi-tenant: each tenant can have their own signing key for their paths

### Key Rotation

1. Generate a new ed25519 signing key
2. Add the new public key to all clients' `trusted-public-keys` configuration
3. New paths are signed with the new key immediately
4. Existing active paths are re-signed during GC (mark phase signs reachable paths with the new key)
5. After a grace period (default: 30 days), remove the old key from `trusted-public-keys`

### Realisation Signing (Phase 5)

CA `Realisation` objects carry their own ed25519 signatures over the tuple `(drv_hash, output_name, output_path, nar_hash)`. This provides integrity for content-addressed output mappings independently of narinfo signatures.

## Two-Phase Garbage Collection

r[store.gc.two-phase]
- **Phase 1 (Mark):** Identify paths unreachable from GC roots. GC roots include: active builds, queued builds, pinned paths, paths with active binary cache serving sessions, paths in `'uploading'` manifests, `wopAddTempRoot` temp roots from gateways.
- **Grace period:** Wait T hours (configurable, default 24h). Configurable per-invocation via `GCRequest`.
- **Phase 2 (Sweep):** Re-read chunk refcounts at sweep time (NOT from a mark-phase snapshot). Use `UPDATE chunks SET deleted=true WHERE blake3_hash=$1 AND refcount=0` atomically in PostgreSQL before issuing S3 DELETEs. This statement MUST execute under `SERIALIZABLE` isolation level or use explicit row-level locking (`SELECT ... FOR UPDATE` on the chunk row, then conditional `UPDATE`) to prevent a TOCTOU race where a concurrent `PutPath` increments the refcount between the read and the update. Delete path metadata from PostgreSQL first (making paths invisible), then enqueue S3 deletions via `pending_s3_deletes` (async/batched). Also clean up content index entries for swept paths.

**Orphan cleanup:** Stale `'uploading'` manifests are reclaimed after a configurable timeout (default: 2 hours). Their chunk lists are used to decrement refcounts for referenced chunks; only chunks whose refcount drops to 0 are eligible for deletion via `pending_s3_deletes`. No full S3 enumeration needed. A weekly full orphan scan remains as a safety net for any leaked chunks not covered by manifest-based cleanup.

Configurable retention policies per tenant/project. Supports dry-run mode via `GCRequest.dry_run`.

## Crash-Safe S3 Deletion (`pending_s3_deletes`)

r[store.gc.pending-deletes]
S3 deletes are not transactional with PostgreSQL. To prevent data leaks (chunks removed from PG but never deleted from S3) or premature deletes on crash, rio-store uses a transactional outbox pattern:

1. In the same PostgreSQL transaction that marks chunks as `deleted=true` (GC sweep) or decrements refcounts to 0 (orphan cleanup), write the corresponding S3 keys to the `pending_s3_deletes` table.
2. A background worker polls `pending_s3_deletes`, issues S3 DELETE requests, and removes rows on success.
3. On crash/restart, unprocessed rows are retried automatically --- S3 DELETE is idempotent.
4. The worker uses exponential backoff with jitter for transient S3 errors. Rows exceeding a max retry count (default: 10) are logged as alerts for manual investigation.

## PostgreSQL Schema

Pseudo-DDL for all store tables. `narinfo` and `manifests` are split to avoid TOAST write amplification when updating manifest status.

```sql
CREATE TABLE narinfo (
    store_path_hash    BYTEA PRIMARY KEY,
    store_path         TEXT NOT NULL,
    deriver            TEXT,
    nar_hash           BYTEA NOT NULL,          -- SHA-256
    nar_size           BIGINT NOT NULL,
    references         TEXT[] NOT NULL DEFAULT '{}',
    signatures         TEXT[] NOT NULL DEFAULT '{}',
    ca                 TEXT,                    -- content address (empty string for input-addressed)
    tenant_id          UUID,                    -- nullable: multi-tenancy deferred to Phase 4
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    registration_time  BIGINT NOT NULL DEFAULT 0,
    ultimate           BOOLEAN NOT NULL DEFAULT FALSE
);

> The `manifests` + `manifest_data` + `chunks` tables are the active schema as of Phase 2c (migration `002_store.sql` dropped the Phase 2a `nar_blobs` table). Small NARs (< 256 KiB) store inline in `manifests.inline_blob`; larger NARs are FastCDC-chunked with BLAKE3 dedup. ChunkBackend is constructed from config (`ChunkBackendKind` enum: `Inline` / `Filesystem` / `S3`, default `Inline` for back-compat).

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
    size             BIGINT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted          BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE INDEX idx_chunks_gc ON chunks (blake3_hash)
    WHERE refcount = 0 AND deleted = FALSE;

-- Multi-tenancy deferred: tenant_id columns are nullable, not in PK (Phase 4).
CREATE TABLE content_index (
    content_hash     BYTEA NOT NULL,           -- SHA-256 output content hash
    store_path_hash  BYTEA NOT NULL
                     REFERENCES narinfo(store_path_hash) ON DELETE CASCADE,
    tenant_id        UUID,
    PRIMARY KEY (content_hash, store_path_hash)
);

-- CA derivation realisations (Phase 5: populated; proactively created)
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

> **Phase deferral (GC):** The `pending_s3_deletes` table is part of the unimplemented GC subsystem. This DDL is the target design.

```sql
CREATE TABLE pending_s3_deletes (
    id               BIGSERIAL PRIMARY KEY,
    s3_key           TEXT NOT NULL,
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

## Key Files (Phase 2a)

- `rio-store/src/grpc.rs` --- StoreService gRPC implementation (PutPath, GetPath, QueryPathInfo, FindMissingPaths)
- `rio-store/src/metadata.rs` --- narinfo + manifests persistence (PostgreSQL, rewritten for Phase 2c)
- `rio-store/src/validate.rs` --- NAR hash verification (HashingReader, NarDigest)
- `rio-store/src/backend/` --- ChunkBackend trait + S3/filesystem/memory impls
- `rio-store/src/cas.rs` --- put_chunked, ChunkCache (moka + singleflight + BLAKE3 verify), reassemble
- `rio-store/src/chunker.rs` --- FastCDC wrapper (16K/64K/256K min/avg/max)
- `rio-store/src/manifest.rs` --- Chunk manifest (de)serialization, versioned binary format
- `rio-store/src/content_index.rs` --- nar_hash → store_path reverse index (CA ContentLookup)
- `rio-store/src/realisations.rs` --- CA (drv_hash, output_name) → output_path mapping
- `rio-store/src/cache_server.rs` --- axum binary cache HTTP (narinfo + nar.zst routes)
- `rio-store/src/signing.rs` --- ed25519 narinfo signing at PutPath time

## Planned Files (Phase 3a+)

- `rio-store/src/gc.rs` --- Garbage collection (mark/grace/sweep + orphan cleanup)
