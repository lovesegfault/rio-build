# rio-store

The global artifact store. Two-layer design inspired by tvix's castore/store split.

## Layer 1: Chunk Store (content-addressed blobs)

r[store.cas.fastcdc]
- NAR archives are split into chunks using FastCDC (content-defined chunking)
- Each chunk is stored by its BLAKE3 hash
- Identical chunks across different store paths are stored once (deduplication)
- Chunks stored in S3-compatible backend (production) or filesystem (dev/test)
- Chunk size target: 64KB average, with min 16KB and max 256KB (compile-time constants in `chunker.rs`)

r[store.cas.upload-bounded]
Chunk uploads within a single `put_chunked` call MUST be bounded to `RIO_CHUNK_UPLOAD_MAX_CONCURRENT` (default 32) concurrent S3 operations. Unbounded fan-out on large NARs (>1000 chunks) saturates the aws-sdk connection pool and produces `DispatchFailure` errors.

r[store.cas.s3-retry]
The S3 client MUST be configured with `RIO_S3_MAX_ATTEMPTS` (default 10) retry attempts and stalled-stream protection disabled. S3-compatible backends (rustfs, MinIO) recycle idle connections more aggressively than AWS S3; a pooled connection closed server-side surfaces as transient `DispatchFailure`. The aws-sdk default of 3 attempts exhausts under connection-churn bursts. Stalled-stream protection false-positives on small chunks against local backends where upload completes faster than the throughput monitor's baseline window.

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
| CA output hash | SHA-256 | `realisations.output_hash` |
| Chunk storage key | BLAKE3 | `chunks/a3/a3f7...` |

These domains must never be confused. Inline blobs are not separately keyed — they are stored by `store_path_hash` in the `manifests` table alongside their narinfo.

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
- Dedup during `PutPath` uses a `refcount == 1` heuristic: after the refcount UPSERT, chunks whose refcount is exactly 1 were newly inserted by this upload and need to go to S3; chunks with refcount > 1 already existed. This has a small TOCTOU window (two concurrent uploaders of the same chunk may both skip upload), but the race is harmless — `GetPath` BLAKE3-verifies every chunk and surfaces `NotFound` as a clear error, not silent corruption. A separate `FindMissingChunks` gRPC batch-query exists for external callers (e.g., executor-side pre-upload checks).
- **S3 backend requirements:** Strong read-after-write consistency is required. AWS S3 provides this natively. Non-AWS S3-compatible backends (MinIO, Ceph RADOS GW) must be validated for consistency.

r[store.backend.filesystem]
For dev/single-node deployments, `FilesystemChunkBackend` stores chunks on local disk under `{base_dir}/chunks/{aa}/{full-blake3-hex}` --- the same two-hex-prefix fanout as the S3 key schema (so switching backends doesn't surprise operators, and per-directory file counts stay bounded). All 256 `{aa}/` subdirs are pre-created at construction (~256 mkdir calls, ~1ms) so `put()` never check-then-mkdirs on the hot path. Writes are crash-safe via temp + `fsync` + `rename` + parent-dir-`fsync`: a crash between `put()` returning and `complete_manifest()` committing must not leave the manifest claiming a chunk that's zero-length or absent. The temp file goes in the same `{aa}/` subdir (rename atomicity needs same filesystem) with a random suffix so concurrent puts of the same content-addressed chunk don't race on the same temp name.

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

r[store.chunk.lock-order]
All batch row-locking statements keyed on `blake3_hash` (`UPDATE chunks ... WHERE blake3_hash = ANY($1)`, `INSERT ... ON CONFLICT` with UNNEST over hash arrays) MUST bind a sorted input array. PostgreSQL acquires row locks in ANY()/UNNEST scan order; unsorted overlapping sets across concurrent transactions create circular lock-wait → SQLSTATE 40P01. Sorting makes lock-acquisition order deterministic across all writers. Note: a RETURNING set is NOT in input-array order — re-sort before passing to downstream ANY() statements. A single defensive retry on 40P01 is permitted (index-page splits can still deadlock under extreme contention); unbounded retry is NOT permitted (masks real lock-order bugs).

r[store.chunk.grace-ttl]
Chunks with zero manifest references AND `created_at < now() - grace_seconds` are GC-eligible. The grace period covers transient refcount-zero windows inside the server-side `PutPath` flow — e.g., a failed upload's rollback (`reap_one`) decrements a chunk to zero while a concurrent `PutPath` that already saw the chunk as present and skipped re-upload has not yet committed its increment. (Sibling to `r[store.chunk.refcount-txn]`.)

**Refcount decrement:** In the same PostgreSQL transaction that deletes a manifest (orphan cleanup of stale `'uploading'` manifests, or GC sweep of unreachable `'complete'` manifests). Uses `UPDATE chunks SET refcount = refcount - 1 WHERE blake3_hash = ANY($1)` — atomic batch decrement.

Chunks with `refcount = 0` are not immediately deleted from S3; they become eligible for GC sweep, which soft-deletes them and enqueues S3 key deletion via the `pending_s3_deletes` table.

## Key Operations

| Operation | Description |
|-----------|-------------|
| `PutPath(narinfo, nar_stream)` | Chunk the NAR, verify NAR hash, deduplicate chunks, store metadata |
| `GetPath(store_path)` | Return narinfo + reconstruct NAR from verified chunks |
| `QueryPathInfo(store_path)` | Return narinfo only |
| `BatchQueryPathInfo(paths)` | Batch narinfo lookup, one PG round-trip (`r[store.api.batch-query]`) |
| `BatchGetManifest(paths)` | Batch (narinfo, manifest) lookup, 1 PG round-trip (`r[store.api.batch-manifest]`) |
| `FindMissingPaths(paths)` | Batch validity check (like REAPI's FindMissingBlobs) |
| `QueryPathFromHashPart(hash_part)` | Resolve full store path from 32-char nixbase32 hash prefix (`r[store.api.hash-part]`) |
| `AddSignatures(store_path, sigs)` | Append ed25519 signatures to existing narinfo (`r[store.api.add-signatures]`) |
| `RegisterRealisation` / `QueryRealisation` | CA derivation output mapping (`r[store.realisation.register]`, `r[store.realisation.query]`) |

### Batch Query RPCs

r[store.api.batch-query]
`BatchQueryPathInfo` returns `(store_path, Option<PathInfo>)` for many paths in ONE PostgreSQL round-trip. Local-only --- it does NOT trigger upstream substitution and does NOT apply the cross-tenant signature-visibility gate (both add per-path round-trips, defeating the batch). The request is bounded by `max_batch_paths` (default `DEFAULT_MAX_BATCH_PATHS`, configurable via `RIO_MAX_BATCH_PATHS`); over-cap returns `INVALID_ARGUMENT` naming the env var. Every path is `validate_store_path`-checked before PG. I-110: builder closure-BFS (`compute_input_closure`) is the only current caller; the per-path → batch swap was the 130× scale unlock.

r[store.api.batch-manifest]
`BatchGetManifest` returns `(store_path, Option<ManifestHint>)` for many paths in ONE PostgreSQL round-trip (`LEFT JOIN manifest_data`). A `ManifestHint` carries the full `PathInfo` plus either `inline_blob` or the `(blake3_hash, size)` chunk list. Same local-only / DoS-bound / validation rules as `r[store.api.batch-query]`. I-110c: the builder issues this once per build (`r[builder.warmgate.manifest-prime]`) so each subsequent `GetPath` can supply `manifest_hint` and skip both PG lookups.

r[store.api.hash-part]
`QueryPathFromHashPart` resolves a full store path from its 32-char nixbase32 hash prefix (the 20-byte `compressHash` output). The hash part MUST be exactly 32 chars and MUST decode as nixbase32 --- both checked BEFORE the PG query. The decoded bytes are discarded; the decode is purely a validator that blocks LIKE-injection (the lookup builds `'/nix/store/{hash}-%'`, and nixbase32's alphabet contains neither `%` nor `_`). Returns `NOT_FOUND` if no matching `'complete'` narinfo exists. Backs the gateway's `wopQueryPathFromHashPart`.

r[store.api.add-signatures]
`AddSignatures` appends ed25519 signature strings to an existing `'complete'` narinfo (`array_cat`, deduped). The signatures list is bounded by `MAX_SIGNATURES`. An empty list is a no-op (NOT an error --- `nix store sign` with no configured key legitimately produces it). Returns `NOT_FOUND` if the path has no `'complete'` narinfo. The store does NOT verify the signatures --- that's the consumer's job at `narinfo` read time. Backs the gateway's `wopAddSignatures`.

### Write-Ahead Manifest Pattern (PutPath flow)

r[store.put.wal-manifest]
**Authorization:** Executor `PutPath` calls include an HMAC-SHA256-signed assignment token in the `x-rio-assignment-token` gRPC metadata header. The store verifies the token signature, checks expiry, and rejects uploads whose `store_path` is not in `claims.expected_outputs`. The gateway (which handles `nix copy --to` and has no assignment) bypasses the assignment-token check via a service token — see `r[sec.authz.service-token]`. See [Security: assignment tokens](../security.md#boundary-2-gatewayexecutor--internal-services-grpc).

r[sec.authz.ca-path-derived]
For floating-CA derivations (`AssignmentClaims.is_ca = true`), `expected_outputs` is unknown at dispatch time. Instead of skipping authorization, the store recomputes the CA store path **server-side** from the SHA-256 it computed over the buffered NAR (via `StorePath::make_fixed_output(name, nar_hash, recursive=true, refs)`) and rejects with `PERMISSION_DENIED` if it does not match the uploaded `store_path`. A worker holding an `is_ca=true` token therefore cannot upload to any path other than the content-derived path of the NAR it actually sent.

r[sec.authz.service-token]
Trusted control-plane callers present an `x-rio-service-token` header: an HMAC-SHA256-signed `ServiceClaims { caller, expiry_unix }` keyed with `RIO_SERVICE_HMAC_KEY_PATH` (a separate secret from the assignment-token key). The store verifies signature and expiry, then checks `caller ∈ service_bypass_callers` (default `["rio-gateway"]`). A valid service token bypasses the assignment-token check — the gateway mints one per `PutPath` with a 60-second expiry. Transport-agnostic: works over plaintext-on-WireGuard with no TLS dependency.

1. **Idempotency check + `'uploading'` placeholder:** If a `'complete'` manifest already exists for this path, return success immediately (fast-path no-op). Otherwise, insert an `'uploading'` placeholder row in `manifests` as an idempotency lock --- this PG write happens **before** the NAR is buffered or verified.
2. **Buffer + verify:** Accumulate the streamed NAR chunks into a buffer, then compute SHA-256 over the buffered bytes and verify against the declared `NarHash`. On mismatch, delete the placeholder row and reject.
3. **Write-ahead manifest (PG):** Chunk the buffered NAR with FastCDC, then in a single PostgreSQL transaction: write `manifest_data` (serialized chunk list) and UPSERT chunk refcounts. This protects chunks from GC sweep immediately — even if the upload crashes after this point, orphan cleanup will find the stale `'uploading'` row and decrement refcounts correctly.
4. **Dedup check:** Query `chunks.refcount` for all chunks in the manifest. Chunks with `refcount == 1` were newly inserted by step 3 and need S3 upload; chunks with `refcount > 1` already existed before this upload.
5. **Upload new chunks:** Parallel S3 PUTs (8-wide) for the `refcount == 1` subset. No post-upload HeadObject verification — integrity is guaranteed by BLAKE3 verification on every read (see `store.integrity.verify-on-get`).
6. **Complete:** Flip manifest status to `'complete'` in a single PG transaction (also fills real narinfo fields, references, content index entries).

**On graceful error (steps 4–6 return `Err`):** `put_chunked` rolls back refcounts and deletes the `'uploading'` placeholder before returning. Chunks already uploaded to S3 are **not** deleted (GC sweep's responsibility — deleting now would race with a concurrent uploader that just incremented the same chunk).

**On crash (process dies between steps 3 and 6):** the orphan scanner reclaims stale `'uploading'` records after a compile-time threshold (`STALE_THRESHOLD`, 15 minutes — was 2 hours; tightened because substitution made stale placeholders a hot-path blocker, see `r[store.substitute.stale-reclaim]`). The chunk list in `manifest_data` is used to decrement refcounts; only chunks whose refcount drops to 0 become eligible for S3 deletion via `pending_s3_deletes`. No full S3 enumeration needed.

r[store.gc.orphan-heartbeat]
Uploaders MUST heartbeat `manifests.updated_at` during long-running chunk uploads (interval: ≤30s or ≤64 chunks, whichever first) so the orphan scanner's stale-threshold check distinguishes in-progress uploads from crashed ones. Without heartbeat, `updated_at` reflects insert time — a 16-minute upload over 50Mbps would be reaped at the 15-minute mark.

r[store.put.idempotent]
**Idempotency:** If `PutPath` is called for a store path that already has a `'complete'` manifest, the call returns success immediately without re-uploading. This makes concurrent uploads of the same path safe.

r[store.put.placeholder-refs]
The `'uploading'` placeholder narinfo MUST carry `references` from the instant it commits (same INSERT, same transaction as the `manifests` row). PutPath does NOT take any GC-related advisory lock. The placeholder's references are what protect its closure from GC: either mark's CTE seed (b) walks them (placeholder committed before mark's snapshot), or sweep's per-path re-check sees them (`r[store.gc.sweep-recheck]`). Rationale: I-192 — the previous `GC_MARK_LOCK_ID` advisory lock was redundant with the re-check, and surfaced `Aborted` to `nix copy` under mark-CTE pressure (I-168).

r[store.atomic.multi-output]
Multi-output derivation registration MUST be atomic at the DB level: all output rows commit in one transaction, or none do. Blob-store writes are NOT rolled back (orphaned blobs are refcount-zero and GC-eligible on the next sweep). The bound is ≤1 NAR-size per failure.

r[store.put.nar-bytes-budget+2]
A process-global `tokio::sync::Semaphore` (default `8 × MAX_NAR_SIZE` = 32 GiB; configurable via `nar_buffer_budget_bytes`) bounds in-flight NAR bytes across ALL concurrent `PutPath` handlers. Each handler `acquire_many(chunk.len().max(MIN_NAR_CHUNK_CHARGE))` BEFORE extending its `nar_data: Vec<u8>`; permits are held in a `Vec<SemaphorePermit>` and released on handler drop (any exit path). When the budget is exhausted, the `await` backpressures the client via gRPC flow control instead of OOMing the process (10 × 4 GiB concurrent uploads = 40 GiB RSS otherwise). The per-request `MAX_NAR_SIZE` check uses `>=` so a single chunk of exactly 2³² bytes is rejected before it reaches `acquire_many(0)` and silently bypasses the budget. Empty `NarChunk` messages are rejected with `InvalidArgument`; tiny chunks are charged a floor of `MIN_NAR_CHUNK_CHARGE` (256) bytes so per-permit tracking overhead is itself bounded by the budget. `PutPathBatch` cumulative NAR bytes across all outputs are capped at `MAX_NAR_SIZE`; oversize batches return `FailedPrecondition` and the builder falls back to per-output `PutPath` (which has no cumulative cap, only per-output 4 GiB), so a single batch can never self-deadlock on permits it holds. NOT shared with GetPath's chunk cache (moka-bounded separately).

r[store.put.drop-cleanup+2]
Once `PutPath` or `PutPathBatch` owns an `'uploading'` placeholder, it arms a `PlaceholderGuard` that (a) heartbeats `manifests.updated_at` every 30s while held, so `r[store.put.stale-reclaim]`'s `reap_one(SUBSTITUTE_STALE_THRESHOLD)` never reaps a live owner during a long ingest/stage (6 GB at 50 Mbps ≈ 16 min); and (b) on Drop, spawns `gc::orphan::reap_one(store_path_hash)`. This covers the handler future being DROPPED --- tonic aborts the task when the client `RST_STREAM`s (builder killed mid-upload) --- which the explicit `abort_upload` calls on `return Err` paths do NOT cover. `reap_one` filters `status='uploading'` so firing after an explicit `abort_upload` is a harmless no-op; firing after `upgrade_manifest_to_chunked` correctly decrements the chunk refcounts the inline-only delete would leak. The guard is defused only on success. I-125a: pre-fix, a phantom-drained builder leaked the placeholder and the next uploader for the same path got `Aborted: concurrent PutPath` until the orphan scanner reaped it (15 min); the builder side polls for that case per `r[builder.upload.aborted-poll]`.

## NAR Reassembly

r[store.nar.reassembly]
- Load the full manifest into memory (list of chunk digests --- even a 10GB NAR is only ~5MB of manifest)
- Parallel chunk prefetch with a sliding window (K=8 concurrent fetches via `futures::stream::buffered()`, which preserves chunk ordering)
- In-process LRU chunk cache (configurable, default 2GB) to avoid repeated S3 round-trips for hot chunks
- BLAKE3-verify every chunk on read (see Content Integrity Verification above)
- Stream the reassembled NAR to the client without materializing the full NAR in memory

r[store.get.manifest-hint]
When `GetPathRequest.manifest_hint` is set with a non-null `info`, `GetPath` bypasses BOTH PG lookups (`query_path_info` and `get_manifest`) and streams directly from the supplied `(PathInfo, chunk-list-or-inline-blob)`. The hint MUST be for the requested path (`hint.info.store_path == req.store_path`, else `INVALID_ARGUMENT`) and structurally well-formed (32-byte chunk hashes, valid `PathInfo`); a hint with `info=None` falls through to PG. Safety: the post-stream whole-NAR SHA-256 verify checks the reassembled bytes against `hint.info.nar_hash`, so a stale or forged hint surfaces as `DATA_LOSS` exactly like a corrupt `manifest_data` row would; chunks are content-addressed and BLAKE3-verified in `get_verified`, so a hint cannot read chunks the client doesn't already know the hash of. I-110c: with `r[store.api.batch-manifest]` priming the builder, this collapses ~2 PG hits/input to zero on the JIT-fetch hot path.

r[store.get.size-sanity-check]
Before streaming, `GetPath` MUST verify the manifest's summed size (inline blob length, or sum of chunk sizes) equals `narinfo.nar_size`. A mismatch indicates manifest/narinfo drift — PutPath wrote inconsistent state, or the DB was manually modified. The store MUST return `DATA_LOSS` without streaming any NAR bytes. This is a fail-fast over the post-stream integrity check, which would only catch the drift after the client received (and wasted bandwidth on) a corrupt NAR.

## Request Coalescing (Singleflight)

r[store.singleflight]
When multiple concurrent requests need the same chunk from S3 (common during cold starts or thundering herd scenarios), rio-store coalesces them into a single in-flight fetch using a singleflight pattern:

- A `DashMap<[u8; 32], Shared<BoxFuture<'static, Option<Bytes>>>>` tracks in-flight S3 GETs (the fetch is spawned as a tokio task and its `JoinHandle` is mapped to `Option<Bytes>` before `.shared()`, since `JoinError` is not `Clone`)
- First request for chunk X spawns the fetch task and inserts the shared future
- Subsequent requests for chunk X `.await` the existing shared future instead of issuing duplicate S3 GETs
- On completion (success or failure), the entry is removed from the map
- Failed fetches (including task panics) resolve to `None`, and removal from the map means the next request retries cleanly

This is critical for cold start thundering herd: when many builds start simultaneously and request overlapping closures, without coalescing S3 would see O(N*M) GET requests instead of O(M) where N is concurrent builds and M is unique chunks.

## Signing Key Management

r[store.signing.fingerprint]
- Per-instance signing key stored in a Kubernetes Secret (recommend KMS/Vault for production)
- Signatures are computed at `PutPath` time --- read-path consumers (gRPC `QueryPathInfo`, gateway narinfo responses) do not need private key access at serve time
- Narinfo `Sig:` field format: `<key-name>:<base64-ed25519-signature>` (compatible with `nix.settings.trusted-public-keys`)
- Signed message: canonical fingerprint `1;<store-path>;sha256:<nar-hash-nixbase32>;<nar-size>;<sorted-refs-comma-sep>` — semicolon separator, `1;` version prefix, `sha256:` algorithm tag, references are full paths (not basenames) joined by comma. Matches Nix's `ValidPathInfo::fingerprint()` in `path-info.cc`. See `fingerprint()` in `rio-nix/src/narinfo.rs`.
- Multi-tenant: each tenant can have their own signing key for their paths

r[store.signing.empty-refs-warn]
When signing a non-CA path with zero references, the store MUST emit a warning. Non-leaf derivations with empty references indicate the executor's reference scanner missed deps.

r[store.tenant.sign-key]
narinfo signing MUST use the tenant's active signing key from `tenant_keys` when present, falling back to the cluster key otherwise. A tenant with its own key produces narinfo that `nix store verify --trusted-public-keys tenant:<pk>` accepts for that tenant's paths only.

r[store.tenant.narinfo-filter]
Authenticated narinfo requests MUST filter results by `path_tenants.tenant_id = auth.tenant_id`. Anonymous (unauthenticated) requests return unfiltered results for backward compatibility.

### Key Rotation

1. Generate a new ed25519 signing key
2. Add the new public key to all clients' `trusted-public-keys` configuration
3. New paths are signed with the new key immediately
4. Prior cluster public keys stay in the trusted set via `cluster_key_history`; no re-sign needed while the history row exists (see `r[store.key.rotation-cluster-history]`)
5. After a grace period (default: 30 days), remove the old key from `trusted-public-keys` and delete its `cluster_key_history` row

r[store.key.rotation-cluster-history]
The cluster signing key MAY be rotated. Prior cluster public keys MUST remain in the trusted set for `sig_visibility_gate` verification until the grace period expires — otherwise paths signed under the old key become invisible to cross-tenant reads when `path_tenants` row count hits zero (CASCADE on tenant deletion). Prior keys are loaded from `cluster_key_history` alongside the active `Signer`.

> **Future work:** cluster key rotation history and per-tenant signing keys are intended to be manageable via a `rio-cli keys` subcommand (validating `name:base64(32-byte-ed25519-pubkey)` format before INSERT; retirement sets `retired_at` to preserve the audit trail). Until that lands, manual `psql` is the workflow — load-time checks (`r[store.key.rotation-cluster-history]`) catch malformed rows regardless. See `// TODO:` in `rio-store/src/grpc/admin.rs`.

### Realisations

r[store.realisation.register]
`RegisterRealisation` inserts a CA derivation realisation row `(drv_hash, output_name) → (output_path, output_hash, signatures)` into the `realisations` table. `drv_hash` is the modular derivation hash (`hashDerivationModulo`) --- it depends only on the derivation's fixed attributes, NOT on output paths, so two CA derivations with identical inputs hash the same. The insert is `ON CONFLICT (drv_hash, output_name) DO NOTHING` (idempotent --- CA derivations are deterministic, so a duplicate insert means "already knew that"). `drv_hash`/`output_hash` MUST be 32 bytes; the gRPC layer validates and converts to `[u8; 32]` at the trust boundary. Backs the gateway's `wopRegisterDrvOutput`.

r[store.realisation.query]
`QueryRealisation` returns the realisation row for `(drv_hash, output_name)`, or `NOT_FOUND` if no row exists. `NOT_FOUND` means cache miss, not error --- the gateway maps it to an empty-set wire response for `wopQueryRealisation`. DB-egress validation re-checks hash lengths so a row written directly via psql can't poison the response.

r[store.realisation.gc-sweep]
GC sweep MUST `DELETE FROM realisations WHERE output_path = $swept_path` in the same transaction as the `narinfo` DELETE. The `realisations` table has NO foreign key to `narinfo` (migration 002) so CASCADE does not cover it; without explicit cleanup, stale rows would point to swept paths and `QueryRealisation` would claim a CA cache hit for an output no longer in the store. The `realisations_output_idx` index makes the per-path DELETE fast.

CA `Realisation` objects carry their own ed25519 signatures over the tuple `(drv_hash, output_name, output_path, nar_hash)`. This provides integrity for content-addressed output mappings independently of narinfo signatures.

## Upstream Cache Substitution

r[store.substitute.upstream]
rio-store MAY be configured with per-tenant upstream binary caches (`tenant_upstreams` table). On `QueryPathInfo`/`GetPath` miss, the store queries each upstream in priority order (`ORDER BY priority ASC`), fetches the narinfo, verifies at least one `Sig:` line against the tenant's `trusted_keys`, and if valid ingests the NAR via the same chunked-CAS path as `PutPath`. Substitution is synchronous (block-and-fetch): the originating RPC waits for ingest to complete.

r[store.substitute.sig-mode]
Per-upstream `sig_mode` controls post-substitution signature storage: `keep` stores the upstream's `Sig:` lines unchanged; `add` stores upstream sigs plus a fresh signature from the tenant's active key (or cluster key); `replace` discards upstream sigs and stores only the rio-generated signature.

r[store.substitute.tenant-sig-visibility]
A substituted path is cross-tenant visible only by signature: tenant B's `QueryPathInfo` for a path substituted by tenant A returns the path IFF at least one of the stored `narinfo.signatures` verifies against a key in tenant B's `trusted_keys` (union of B's upstream `trusted_keys` arrays). This prevents tenant A from poisoning tenant B's store by substituting from a cache B doesn't trust.

r[store.substitute.probe-bounded+2]
`check_available` (the HEAD-only probe feeding `FindMissingPathsResponse.substitutable_paths`) MUST bound its upstream load. Per-path probe results (positive and negative) are cached for 1h (cap 100k entries) so overlapping `FindMissingPaths` for the same closure don't re-probe; probe results for paths where no upstream returned 2xx and at least one returned a non-404 error are NOT cached (a transient 503 must not pin the path to "miss" for 1h). For uncached paths, concurrency is gated on each upstream's `/nix-cache-info` (also cached, 1h TTL): 128 concurrent HEADs if every upstream advertises `WantMassQuery: 1`, else 8. At most 4096 uncached paths are probed per call; oversized batches are truncated to the first 4096 (subsequent calls see those as cached and probe the next slice, so repeated calls converge to full coverage). The field is a scheduler optimization hint, and per-derivation substitutability is rediscovered at dispatch time, so truncation is conservative rather than a correctness issue.

r[store.substitute.singleflight+2]
`try_substitute` is wrapped in a moka `Cache<(tenant_id, store_path), Option<Arc<ValidatedPathInfo>>>` with 30s TTL and 10 000-entry cap. moka's `try_get_with` coalesces N concurrent callers for the same key into one `do_substitute` call. This is a singleflight coalescer, not a PathInfo cache (the narinfo table IS the cache) --- 30s is long enough to coalesce a burst of `GetPath`s for the same path from N workers, short enough that a substitution-miss doesn't stay stale. `try_get_with` so transient errors propagate to every coalesced waiter without being cached; `Ok(None)` (definitive miss) IS cached. The narinfo/`nix-cache-info`/HEAD requests have a 30s per-request timeout so a hung upstream can't wedge the singleflight slot forever; the NAR GET is bounded only by the `MAX_NAR_SIZE` decompressed cap and the 5-minute stale-reclaim.

r[store.substitute.untrusted-upstream]
`tenant_upstreams` rows (URL and `trusted_keys`) are tenant-supplied via `AddUpstream`, and the substituter is process-global, so one tenant's hostile upstream MUST NOT be able to OOM or stall rio-store for all tenants. Every upstream-supplied body is size-capped: narinfo bodies at `MAX_NARINFO_BYTES` (1 MiB --- sized for `MAX_REFERENCES` basenames), `/nix-cache-info` bodies at `MAX_CACHE_INFO_BYTES` (4 KiB), and decompressed NARs at `MAX_NAR_SIZE` (4 GiB) --- the decompressed cap is applied AFTER the decoder so a zstd bomb is bounded regardless of what `NarSize` claimed. The narinfo/`nix-cache-info`/HEAD requests carry a 30s per-request timeout. The NAR SHA-256 digest runs on `spawn_blocking` so a multi-GB hash does not stall a tokio worker.

r[store.substitute.identity-check]
The parsed narinfo's `StorePath:` MUST equal the requested store path; mismatch is rejected before signature verification. Signature verification proves the upstream signed *that* narinfo, not that it answers `{hash_part}.narinfo` --- a valid-signed narinfo for path A served at `B.narinfo` would otherwise ingest A and return it from `QueryPathInfo(B)`.

r[store.substitute.stale-reclaim]
When `try_substitute` finds an existing `'uploading'` placeholder for the requested path, it MUST check the placeholder's age. If older than `SUBSTITUTE_STALE_THRESHOLD` (5 minutes), the substituter reclaims the placeholder (DELETE + re-INSERT) and proceeds with the fetch. A young placeholder indicates a live concurrent uploader and returns a miss. This prevents a crashed substitution from blocking the path for the full orphan-scanner interval (15 minutes). The `rio_store_substitute_stale_reclaimed_total` counter tracks reclaim events.

r[store.put.stale-reclaim]
`PutPath` and `PutPathBatch` MUST apply the same stale-reclaim as `r[store.substitute.stale-reclaim]` when `insert_manifest_uploading` reports a pre-existing placeholder: reap it via `gc::orphan::reap_one` with the same `SUBSTITUTE_STALE_THRESHOLD`, then retry the insert once. A fetcher that died mid-upload (storage eviction, OOM) otherwise blocks the next attempt with `Aborted("concurrent PutPath in progress")` until the orphan scanner's 15-minute sweep. The `rio_store_putpath_stale_reclaimed_total` counter tracks reclaim events; sustained high alongside `rio_scheduler_resource_floor_promotions_total{kind="fod"}` indicates under-sized fetcher pods (I-207/I-208).

## Two-Phase Garbage Collection

r[store.gc.two-phase]
- **Phase 1 (Mark):** Identify paths unreachable from GC roots via a recursive CTE over `narinfo."references"`. GC root seeds: auto-pinned live-build inputs in the `scheduler_live_pins` table, manifests with `status='uploading'` (in-flight PutPath), paths with `created_at > now() - grace_hours` (recent uploads), per-tenant retention windows, and `extra_roots` passed from the scheduler's live-build output paths (`ActorCommand::GcRoots`). Mark takes NO lock against PutPath (I-192); PutPath runs freely throughout. The CTE's MVCC snapshot is a point-in-time view — placeholders that commit after it are caught by sweep's re-check.
- **Grace period:** Configurable per-invocation via `GcRequest.grace_period_hours` (default **2h**). Protects paths uploaded shortly before GC that builds haven't referenced yet.
- **Phase 2 (Sweep):** Re-read chunk refcounts at sweep time (NOT from a mark-phase snapshot). Per unreachable path, in batched transactions: `SELECT chunk_list ... FOR UPDATE OF m` locks the **manifest** row (not chunk rows), then sweep **re-checks references** (`r[store.gc.sweep-recheck]`). DELETE narinfo (CASCADE), decrement chunk refcounts, mark `refcount=0` chunks deleted, enqueue S3 keys to `pending_s3_deletes` --- all in the same PG transaction.

r[store.gc.sweep-recheck]
Sweep MUST, before deleting each candidate path Q, re-check `EXISTS(SELECT 1 FROM narinfo n WHERE n."references" @> ARRAY[Q] AND n ∉ sweep_unreachable)` with a fresh READ-COMMITTED snapshot. The scan covers ALL narinfo rows including `status='uploading'` placeholders (which carry references from insert per `r[store.put.placeholder-refs]`). On match, sweep MUST skip Q and increment `rio_store_gc_path_resurrected_total`. This re-check is the sole load-bearing mark-vs-PutPath concurrency guard: a PutPath that commits a placeholder referencing Q at any point before the re-check resurrects Q regardless of mark's snapshot timing. A μs-scale TOCTOU between re-check and DELETE remains; the grace period makes it operationally negligible (Q being swept means it's >grace old with zero referrers).

**Orphan cleanup:** Stale `'uploading'` manifests are reclaimed after a compile-time threshold (`STALE_THRESHOLD`, 15 minutes). Their chunk lists are used to decrement refcounts for referenced chunks; only chunks whose refcount drops to 0 are eligible for deletion via `pending_s3_deletes`. No full S3 enumeration needed. A weekly full orphan scan remains as a safety net for any leaked chunks not covered by manifest-based cleanup.

r[store.gc.sweep-path-tenants]
Sweep MUST delete `path_tenants` rows for each swept `store_path_hash`
in the same transaction as the `narinfo` DELETE. `path_tenants` has no
FK CASCADE to `narinfo` (migration 012); without explicit cleanup,
orphaned rows survive the sweep and grant wrong-tenant visibility when
a different tenant later re-uploads the same store path (the stale row
still JOINs in the `r[store.gc.tenant-retention]` CTE arm).

r[store.gc.sweep-cycle-reclaim]
The sweep-phase reference re-check MUST exclude referrers that are
themselves in the current unreachable batch. Without this exclusion,
mutual-reference cycles (A→B, B→A) and self-references (A→A) are never
swept: the re-check sees an intra-batch referrer and skips both paths
forever. The exclusion is a `NOT EXISTS` anti-join against a temp table
populated once at sweep start with the full unreachable set — O(N) wire
bytes and an index probe per row, vs. O(N²) for a per-path array bind.

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

r[store.gc.serialize-lock]
`run_gc` serializes against itself via `pg_try_advisory_lock(GC_LOCK_ID)` on a dedicated session-scoped pool connection. If the lock is held, `run_gc` returns `Ok(None)` immediately ("already running") --- two concurrent sweeps would not corrupt anything but waste work and produce misleading stats. The lock is explicitly released via `pg_advisory_unlock` on every exit path (a scopeguard backs the explicit calls). The constant `GC_LOCK_ID = 0x724F47430001` is arbitrary; it just must not collide with other advisory locks in the schema (`r[store.db.migrate-try-lock]` uses a different ID).

r[store.gc.dry-run]
`GcRequest.dry_run=true` runs mark + sweep with full stats computation but the sweep transaction is `ROLLBACK`ed instead of committed. The operator sees "would delete N paths, free M bytes" without touching narinfo, chunk refcounts, or `pending_s3_deletes`. The final progress message's `current_path` reads `"dry-run: no paths actually deleted"`. `rio_store_gc_path_swept_total` is NOT incremented on dry-run.

r[store.gc.shutdown-abort]
`sweep` checks the shutdown token between batches (NOT mid-transaction --- a partial batch ROLLBACKs cleanly via tx drop). On cancellation it returns `SweepAbort::Shutdown`; `run_gc` releases `GC_LOCK_ID` and returns `Status::aborted("GC aborted: process shutting down")`. `VerifyChunks` likewise checks the token between PG batches and sends `Aborted` on the progress stream.

r[store.cas.upsert-inserted+2]
The chunk-upsert batch INSERT returns per-row `(uploaded_at IS NULL) AS
needs_upload` so the caller knows which blake3 hashes need upload to
backend. The predicate is atomic with the upsert (no re-query window)
and is keyed on confirmed backend presence rather than refcount: a
chunk whose first uploader was killed mid-PUT has refcount≥1 but
`uploaded_at IS NULL`, so the next PutPath re-uploads instead of
skipping into permanent data loss.

r[store.cas.chunk-upload-committed]
`chunks.uploaded_at` is non-NULL iff a `ChunkBackend::put` for that
hash has been observed to succeed. `cas::put_chunked` MUST set it (via
`mark_chunks_uploaded`) only after `do_upload` returns Ok, and GC paths
MUST clear it back to NULL when they mark the chunk `deleted=true`.
Concurrent uploaders racing on a NULL `uploaded_at` all upload (S3
PutObject is idempotent for identical content); the first to reach
`mark_chunks_uploaded` wins the timestamp.

## Crash-Safe S3 Deletion (`pending_s3_deletes`)

r[store.gc.pending-deletes]
S3 deletes are not transactional with PostgreSQL. To prevent data leaks (chunks removed from PG but never deleted from S3) or premature deletes on crash, rio-store uses a transactional outbox pattern:

1. In the same PostgreSQL transaction that marks chunks as `deleted=true` (GC sweep) or decrements refcounts to 0 (orphan cleanup), write the corresponding S3 keys **and `blake3_hash`** to the `pending_s3_deletes` table.
2. A background drain task polls `pending_s3_deletes` on a **fixed 30s interval** (`DRAIN_INTERVAL`, not exponential --- S3 DELETE failures are rare and transient; queueing absorbs bursts). Before issuing each S3 DELETE, drain **re-checks the chunk state by `blake3_hash`** (TOCTOU guard: if a concurrent PutPath re-incremented the refcount since sweep enqueued the key, skip the delete and remove the row --- `rio_store_gc_chunk_resurrected_total` metric). On S3 success, the row is removed; on failure, `attempts` is incremented.
3. On crash/restart, unprocessed rows are retried automatically --- S3 DELETE is idempotent.
4. Rows exceeding max retry count (default: 10) remain in the table for alerting (`rio_store_s3_deletes_stuck` gauge).

**GC-vs-GC serialization:** see `r[store.gc.serialize-lock]`.

## Admin RPCs

r[store.admin.verify-chunks]
`StoreAdminService.VerifyChunks` server-streams `VerifyChunksProgress{scanned, missing, missing_hashes, is_complete}` while keyset-paginating `chunks WHERE deleted=FALSE AND blake3_hash > $cursor ORDER BY blake3_hash LIMIT batch_size` and calling `ChunkBackend.exists_batch` per page. Keyset (NOT OFFSET) so a 100k-chunk store is O(N) overall. `batch_size=0` → default; clamped at `VERIFY_BATCH_MAX`. `deleted=TRUE` rows are skipped (awaiting S3-delete drain --- presence is undefined); `refcount=0` IS verified (could be a mid-upload row in grace TTL --- the object SHOULD exist once `uploaded_at` is set). Returns `FAILED_PRECONDITION` for inline-only stores (no chunk backend). Read-only --- no `--repair` (deleting the PG row would be wrong if the object is recoverable; the operator decides). Aborts on shutdown token per `r[store.gc.shutdown-abort]`.

r[store.admin.upstream-crud]
`StoreAdminService.{ListUpstreams, AddUpstream, RemoveUpstream}` manage `tenant_upstreams` rows over gRPC. `AddUpstream` validates `trusted_keys[]` entry format (`name:base64(32-byte-ed25519-pubkey)`) and `sig_mode ∈ {keep, add, replace}` before INSERT. `RemoveUpstream` deletes by `(tenant_id, base_url)`; `NOT_FOUND` if no row matched. `ListUpstreams` returns rows for one tenant ordered by `priority ASC`. These back `rio-cli upstream {add, rm, ls}`; `r[store.substitute.upstream]` consumes the resulting rows.

## PostgreSQL Schema

r[store.db.migrate-try-lock]
Startup migrations MUST serialize across replicas via a non-blocking `pg_try_advisory_lock` poll loop, with sqlx's built-in blocking `pg_advisory_lock` disabled (`Migrator::set_locking(false)`). Migrations 011 and 022 run `CREATE INDEX CONCURRENTLY`; CIC's final phase waits for every older virtualxid to release. Under sqlx's default lock, a second replica blocked in `SELECT pg_advisory_lock(...)` holds such a virtualxid for the duration — the leader's CIC waits on the follower's vxid, the follower waits on the leader's advisory lock, and PG's deadlock detector does not see across that pair. The try-lock poll holds no long-lived virtualxid (each probe is a sub-ms SELECT that completes immediately), so the leader's CIC proceeds.

r[store.db.pool-idle-timeout]
The PostgreSQL connection pool MUST set `idle_timeout` (60s) and `min_connections` (2). Aurora Serverless v2 scales `max_connections` with ACU — at `min_capacity=0.5` ACU only ~105 slots are usable, and idle connections count against that limit. The sqlx default 10-minute idle reap means a burst-grown pool holds its full `max_connections` long after the burst ends; with 2×store + 2×scheduler replicas the idle floor exceeds Aurora's min-ACU ceiling and ad-hoc psql gets `FATAL: remaining connection slots are reserved`. The same constraint applies to any service holding a PG pool against the shared database (scheduler).

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

> GC tables (`scheduler_live_pins`, `pending_s3_deletes`) are added by `migrations/006_gc_safety.sql`. See the `rio-store/src/gc/` module for the mark/sweep/drain implementation. The `gc_roots` explicit-pin table was created in 005 but never gained a production writer; dropped in 036.

```sql

-- Scheduler auto-pins input closures of dispatched derivations.
-- Unpinned on completion. NOT FK'd to narinfo (input paths may not
-- be in the local store yet — they'll arrive via executor upload).
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
- `rio-store/src/realisations.rs` --- CA (drv_hash, output_name) → output_path mapping
- `rio-store/src/signing.rs` --- ed25519 narinfo signing at PutPath time

## GC Files

- `rio-store/src/gc/mod.rs` --- GC lock constant (`GC_LOCK_ID`), `GcStats`, `run_gc` orchestration
- `rio-store/src/gc/mark.rs` --- `compute_unreachable` recursive CTE (seeds: scheduler_live_pins, uploading manifests, grace-period, extra_roots, tenant retention)
- `rio-store/src/gc/sweep.rs` --- Per-path batched delete with `FOR UPDATE OF m` + reference re-check
- `rio-store/src/gc/drain.rs` --- Background S3 delete drain (30s interval, blake3_hash re-check)
- `rio-store/src/gc/orphan.rs` --- Stale `'uploading'` manifest cleanup
- `rio-store/src/grpc/admin.rs` --- `StoreAdminService` (TriggerGC, PinPath, UnpinPath)
