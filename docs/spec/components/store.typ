#import "/lib/rio.typ": *
#show: rio.with(domains: ("store", "sec"))

The global artifact store. Two-layer design inspired by tvix's castore/store
split.

= Layer 1: Chunk Store (content-addressed blobs)

#r("store.cas.fastcdc")[
  - NAR archives are split into chunks using FastCDC (content-defined chunking)
  - Each chunk is stored by its BLAKE3 hash
  - Identical chunks across different store paths are stored once
    (deduplication)
  - Chunks stored in S3-compatible backend (production) or filesystem
    (dev/test)
  - Chunk size target: 64KB average, with min 16KB and max 256KB (compile-time
    constants in `chunker.rs`)
]

#r("store.cas.upload-bounded+2")[
  Chunk uploads within a single `put_chunked` call MUST be bounded to
  `RIO_CHUNK_UPLOAD_MAX_CONCURRENT` (default 64) concurrent S3 operations,
  AND the replica MUST gate all concurrent S3 chunk PutObject calls under a
  single `RIO_CHUNK_UPLOAD_GLOBAL_PERMITS` (default 256) semaphore. The
  aws-sdk's default hyper client has no in-flight connection cap (only an
  idle-pool cap), so the global semaphore is the de-facto fd/connection
  bound for the chunk-PUT plane; the per-ingest width gives fairness and
  bounds the per-ingest owned-`Bytes` overshoot.
]

#r("store.cas.s3-retry")[
  The S3 client MUST be configured with `RIO_S3_MAX_ATTEMPTS` (default 10)
  retry attempts and stalled-stream protection disabled. S3-compatible backends
  (rustfs, MinIO) recycle idle connections more aggressively than AWS S3; a
  pooled connection closed server-side surfaces as transient `DispatchFailure`.
  The aws-sdk default of 3 attempts exhausts under connection-churn bursts.
  Stalled-stream protection false-positives on small chunks against local
  backends where upload completes faster than the throughput monitor's baseline
  window.
]

= Inline Storage Fast-Path

#r("store.inline.threshold")[
  NARs below 256KB (`INLINE_THRESHOLD`, a compile-time constant) are stored
  directly in the `manifests.inline_blob` PostgreSQL `BYTEA` column, bypassing
  FastCDC chunking, S3, and manifest indirection entirely. This eliminates
  per-item overhead for the thousands of tiny `.drv` files found in nixpkgs
  closures.
]

@inline-storage #glspl("blob") never touch S3 --- they live entirely in PostgreSQL.
The inline/chunked decision is made at `PutPath` time based on @nar size; see the
"Inline vs. chunked invariant" in @store-schema.

= Layer 2: Nix Metadata Store

- Maps store paths to their chunk manifests (ordered list of chunk digests)
- Stores @narinfo metadata: deriver, NAR hash, NAR size, references, signatures,
  `tenant_id`
- @ca index: maps output content hash → @store-path (for CA early
  cutoff)
- Input-addressed index: maps derivation hash → output store paths (traditional
  lookups)
- Stored in PostgreSQL (shared with scheduler for query efficiency, separate
  schema)

= Hash Domain Separation

#r("store.hash.domain-sep")[
  Two hash algorithms are used, in strictly separated domains:

  #table(
    columns: 3,
    align: (left, left, left),
    table.header([Context], [Hash Algorithm], [Example]),
    [NAR hash in narinfo], [SHA-256], [`sha256:1b3a...`],
    [Store path computation], [SHA-256], [`/nix/store/{hash}-name`],
    [narinfo signature], [SHA-256], [signed over fingerprint],
    [CA output hash], [SHA-256], [`realisations.output_hash`],
    [Chunk storage key], [BLAKE3], [`chunks/a3/a3f7...`],
  )
]

These domains must never be confused. Inline blobs are not separately keyed ---
they are stored by `store_path_hash` in the `manifests` table alongside their
narinfo.

= Content Integrity Verification

#r("store.integrity.verify-on-put+3")[
  *On NAR-byte ingest (`PutPath`, `PutPathBatch`, the substituter):* the store
  independently computes SHA-256 over the uploaded NAR stream and rejects the
  upload on mismatch with the declared `NarHash`.
  *On `PutPathChunked`:* the store BLAKE3-verifies every chunk body received
  on the stream against its claimed digest, and length-checks it against the
  manifest, before the body is stored or referenced. Every per-file digest
  MUST be proven to match its file's chunk-run content before the commit
  binds it into the digest-keyed `file_blobs` namespace — by recomputing the
  whole-file BLAKE3 from the bodies received on this stream, by exact
  `(chunk digest, size)` window agreement with an already-committed binding
  of the same digest, or by re-fetching the run's chunks from the backend
  and recomputing; a mismatch rejects the upload. `nar_hash`, `nar_size`,
  and `references` are computed by the authenticated builder's fused walk
  over the same bytes it uploads (#rref("builder.upload.references-scanned")) and
  are committed as claimed; the store does not regenerate the NAR to
  recompute them.
]

The asymmetry is a trust-boundary line drawn by blast radius. The builder
runs adversary-supplied build instructions, so every claim in `Begin` is
attacker-controlled — but `nar_hash`, `nar_size`, and `references` are
recorded under the store *path* the assignment token authorized, so a lie
corrupts only outputs the builder was entitled to write (and NAR SHA-256 is
not composable from chunk digests: recomputing it on a fully-deduped upload
meant re-fetching every already-durable chunk — O(chunks) serial S3 round
trips on an upload that streams no bodies at all, long enough on large
outputs to exceed the client's stream timeout). Per-file digests are
different: they key the `file_blobs` dedup namespace that
`ReadBlob`/`StatBlob`/`HasBlobs` resolve by digest alone across every tenant
that can see any referrer, so a forged `digest → content` binding poisons
reads far beyond the forger's own paths — that one claim must be proven,
not trusted. The window-agreement form keeps the fully-deduped re-upload on
PostgreSQL metadata (zero chunk reads, preserving the stall fix above); the
refetch form is reserved for runs that mix deduped chunks into a digest the
store has never committed. Substituted content arrives from *outside* the
service boundary (an upstream binary cache), so the substituter keeps
independent NAR-hash verification.

#r("store.integrity.verify-on-get")[
  - *On chunk read (S3 or cache):* Every chunk fetched from S3 or the
    in-process LRU cache is BLAKE3-verified against its manifest-declared
    digest. Corrupt chunks are re-fetched (from S3 on cache corruption) or
    flagged as an error.
  - *On inline blob read:* The inline NAR is served directly from PostgreSQL.
    On `GetPath`, the client can verify the SHA-256 against `narinfo.nar_hash`
    (the store does not re-hash on every read; integrity is guaranteed by
    verify-on-put and PostgreSQL's own storage guarantees).
]

= Ingest Tree Bounds (castore validation boundary)

#r("store.ingest.tree-bounds+2")[
  Every NAR/castore ingest entry point --- `PutPath`, `PutPathBatch`, the
  substituter, and `PutPathChunked` --- MUST enforce one shared set of
  tree-shape bounds before any side effect: directory nesting depth
  (`MAX_NAR_DEPTH`), whole-archive entry count (`MAX_NAR_ENTRIES`),
  cumulative materialized index bytes --- joined entry paths plus symlink
  targets --- (`MAX_NAR_INDEX_BYTES`), entry-name and symlink-target
  lengths, and per-directory entry counts; buffered NAR input that the
  parser does not consume entirely MUST be rejected; and the serving-side
  caps named here --- `GetPath`'s regeneration-walk node cap and the
  chunk-count cap `MAX_CHUNKS` --- MUST be derived from --- and be at
  least as permissive as --- the same constants, so that "ingest accepts
  ⇒ the committed path can be regenerated through `GetPath` and
  re-ingested" holds structurally.
]

The constants live in `rio_nix::nar` (the NAR reader is the definitional
consumer: anything it would reject must never be committable) and are
enforced at the two chokepoints every ingest path goes through ---
`nar_ls` for NAR-byte ingest and the castore `TreeWalk` for Directory-DAG
ingest --- rather than per-RPC. Without the whole-archive bounds the
per-axis limits compose badly: a few-hundred-MB NAR can legally expand to
tens of GB of materialized index (\~350× amplification), a node count the
serving walk refuses commits paths that read as `DATA_LOSS` forever, and
a nesting depth the readers reject can never be substituted or imported
again. Trailing bytes after the root node are rejected for the same
reason: the claimed `nar_hash` covers them, the persisted castore content
does not, so the regenerated NAR could never hash back to its narinfo.

The recursive `GetDirectory` result cap is deliberately outside this
contract: that walk serves a builder's whole input closure in one call
(the FUSE mount-time prefetch), so its bound is per-walk rather than
per-path and no relation to the per-path ingest constants would make
"accepted ⇒ mountable" structural --- see the `TODO` at
`GET_DIRECTORY_MAX_RESULTS` in `rio-store/src/grpc/directory.rs` for what
closing that gap would take (paging the walk, not raising the cap).

= Chunk Manifest Format

#r("store.manifest.format")[
  Each manifest is stored as a single PostgreSQL row with a `bytea` column
  containing serialized `(BLAKE3_digest, chunk_size)` pairs --- not one row per
  chunk. This reduces the INSERT count per `PutPath` from N to 1. Manifests are
  keyed by store path hash. The serialization format includes a version byte
  prefix for future evolution.
]

= Chunk Storage

- *S3 key schema:* `chunks/{first-2-hex-chars}/{full-blake3-hex}`
  (prefix-partitioned to avoid S3 hotspots)
- Dedup during `PutPath` is decided inside the chunk-row upsert itself: the
  step-3 UPSERT returns `needs_upload = (uploaded_at IS NULL)` per row
  (#rref("store.cas.upsert-inserted")), so a chunk is skipped only when a
  prior upload was confirmed in the backend
  (#rref("store.chunk.liveness-not-presence"),
  #rref("store.cas.chunk-upload-committed")). Two concurrent uploaders of an
  unconfirmed chunk both upload --- S3 PutObject of identical bytes is
  idempotent, so the duplicate PUT is wasted bandwidth, never corruption ---
  and `GetPath` #gls("blake3")-verifies every chunk on read regardless. There
  is no separate dedup query and no batch "which chunks are missing" RPC
  (`grpc/chunk.rs` serves only the test-only `GetChunk`).
- *S3 backend requirements:* Strong read-after-write consistency is required.
  AWS S3 provides this natively. Non-AWS S3-compatible backends (MinIO, Ceph
  RADOS GW) must be validated for consistency.

#r("store.cas.zstd-at-rest")[
  Chunk objects MUST be zstd-compressed at rest on write; chunk digests MUST
  remain the BLAKE3 of the UNCOMPRESSED bytes (the digest space never
  re-keys); reads MUST decompress under a `CHUNK_MAX` output bound and verify
  BLAKE3 over the decompressed result; a stored object whose decode fails or
  whose decompressed bytes do not match the digest MUST surface as a
  corruption error naming the digest, never a decoder panic or unbounded
  allocation.
]

Dedup is unaffected: identical uncompressed content compresses to the
identical stored object under the same digest key.

#r("store.backend.filesystem")[
  For dev/single-node deployments, `FilesystemChunkBackend` stores chunks on
  local disk under `{base_dir}/chunks/{aa}/{full-blake3-hex}` --- the same
  two-hex-prefix fanout as the S3 key schema (so switching backends doesn't
  surprise operators, and per-directory file counts stay bounded). All 256
  `{aa}/` subdirs are pre-created at construction (\~256 mkdir calls, \~1ms) so
  `put()` never check-then-mkdirs on the hot path. Writes are crash-safe via
  temp + `fsync` + `rename` + parent-dir-`fsync`: a crash between `put()`
  returning and `complete_manifest()` committing must not leave the manifest
  claiming a chunk that's zero-length or absent. The temp file goes in the same
  `{aa}/` subdir (rename atomicity needs same filesystem) with a random suffix
  so concurrent puts of the same content-addressed chunk don't race on the same
  temp name.
]

= Chunk Lifecycle

The `chunks` table holds one row per content-addressed chunk; which manifests
reference a chunk is derived from `manifest_data.chunk_list` at collect time
(#rref("store.chunk.liveness-derived")), never maintained as a per-row
aggregate. The table schema:

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Column], [Type], [Description]),
  [`blake3_hash`], [`BYTEA PRIMARY KEY`], [BLAKE3 digest of chunk content],
  [`size`], [`BIGINT NOT NULL`], [Chunk size in bytes],
  [`created_at`], [`TIMESTAMPTZ`], [Insertion timestamp],
  [`uploaded_at`],
  [`TIMESTAMPTZ`],
  [Confirmed-backend-presence timestamp (NULL until a PUT for this hash has
    been observed to succeed --- #rref("store.cas.chunk-upload-committed"))],

  [`last_referenced_at`],
  [`TIMESTAMPTZ`],
  [Re-reference touch set by the upsert's conflict arm; with `created_at` it
    feeds the collect cycle's grace term (#rref("store.chunk.grace-ttl"))],

  [`deleted`],
  [`BOOLEAN NOT NULL DEFAULT FALSE`],
  [Soft-delete flag (set by the collect cycle, cleared by the resurrect arm)],
)

(The historical `chunks.refcount` column, its `CHECK`, and the `idx_chunks_gc`
partial index belong to the retired counter machinery: the CHECK and index are
dropped by migration 071 and the column itself by migration 072; none of them
exist in the live schema.)

#r("store.chunk.refcount-txn+2")[
  *Chunk-row write-ahead:* In the same PostgreSQL transaction that writes
  `manifest_data` (the write-ahead manifest step of PutPath), the chunk rows for every hash the
  manifest references MUST be upserted --- a single `INSERT ... ON CONFLICT
  (blake3_hash) DO UPDATE` over the full chunk list via `UNNEST`, whose
  conflict arm clears `deleted` (the resurrect) and sets
  `last_referenced_at = now()` (the re-reference touch the collect cycle's
  grace term reads). No per-chunk counter or other liveness aggregate is
  maintained. PostgreSQL's conflict resolution serializes INSERT vs UPDATE
  per-row, so concurrent `PutPath` calls with overlapping chunk lists are
  both recorded correctly without explicit locking.
]

The pairing matters in both directions: the durable manifest reference and
the chunk row it justifies commit together, so the collect cycle's mark fold
(which reads `manifest_data.chunk_list` of every existing manifest) and the
row state it collects against can never disagree about an in-flight upload.

#r("store.chunk.lock-order+2")[
  All batch row-locking statements keyed on `blake3_hash` (`UPDATE chunks ...
  WHERE blake3_hash = ANY($1)`, `INSERT ... ON CONFLICT` with UNNEST over hash
  arrays) MUST bind a sorted input array. After the counter writers'
  retirement the statements this binds are the PutPath chunk-row upsert,
  `mark_chunks_uploaded`, and the collect cycle's batch statements (candidate
  scan order feeding the sorted `= ANY` soft-delete and the outbox enqueue);
  the drain takes single-row locks and carries no array to sort. PostgreSQL
  acquires row locks in ANY()/UNNEST scan order; unsorted overlapping sets
  across concurrent transactions create circular lock-wait → SQLSTATE 40P01.
  Sorting makes lock-acquisition order deterministic across all writers.
  Note: a RETURNING set is NOT in input-array order --- re-sort before
  passing to downstream ANY() statements. A single defensive retry on 40P01
  is permitted (index-page splits can still deadlock under extreme
  contention); unbounded retry is NOT permitted (masks real lock-order bugs).
]

#r("store.chunk.grace-ttl+2")[
  A chunk MUST be GC-eligible only if it has zero manifest references at the
  collect cycle's mark snapshot (#rref("store.chunk.liveness-derived")) AND
  `GREATEST(created_at, last_referenced_at)` predates the cycle's snapshot
  cutoff by at least the grace window. The grace window MUST cover the three
  windows it exists for: the mark-snapshot window (a manifest whose upgrade
  transaction commits after the snapshot re-references an old chunk; only the
  upsert's `last_referenced_at` touch keeps that chunk inside the grace term),
  the interrupted-then-retried upload window (a crashed upload's chunk rows
  stay claimable by the retry before any reclamation fires --- the
  `uploaded_at`-skip optimization), and headroom over the writer-transaction
  soundness bound of #rref("store.gc.chunk-collect").
]

The eligibility predicate is evaluated against the cycle snapshot on the
database clock, never re-derived per batch; a `NULL last_referenced_at` is
equivalent to `created_at` (the column is touched only by the upsert's
`ON CONFLICT` arm). Before the cutover the same grace value also bounded the
orphan-chunk sweep's `refcount = 0` reaping --- that mechanism is retired with
the counter readers; the window itself (and its 300 s default) is unchanged.

#r("store.chunk.has-chunks-tenant")[
  `HasChunks` presence MUST be tenant-scoped: a bit may be set only if the
  calling tenant has a `chunk_tenants` row for that digest, in addition to the
  durable-presence predicate. The junction row is written in the same
  transaction that completes one of the tenant's manifests (atomic with the
  `durable` flip), so "manifest is complete" and "its chunks answer present to
  the uploader" cannot diverge. Chunk *storage* stays digest-keyed global ---
  dedup at rest is unaffected; tenant-scoped presence costs upload bandwidth
  only, because the duplicate upload of a tenant-invisible chunk is an
  idempotent overwrite of identical content.
]

ADR-024 P2 settled presence as tenant-scoped for ALL object kinds (chunks were
previously the documented cross-tenant exception). Migration rule: chunks
ingested before `chunk_tenants` existed (migration 116) --- and chunks of
paths a tenant adopted via an idempotent-skip or substitution --- have no
junction row for that tenant, so presence answers false and the tenant's next
upload re-sends them, binding visibility through the write-through put. No
backfill; the cost is bounded re-upload bandwidth, never correctness.

The two as-built counter rules that used to live here ---
`store.chunk.refcount-decrement` (every manifest-deleting transaction
decrements the counter from the deleted manifest's own `chunk_list`) and
`store.chunk.refcount-meaning` (the counter equals the manifest fold at every
quiescent point) --- were retired with the counter writers in Release B of
the refcount-formal campaign. Liveness is derived from the manifests at
collect time (#rref("store.chunk.liveness-derived"),
#rref("store.gc.chunk-collect")), which makes the old equality true by
construction with no maintained aggregate left to drift; the historical
`chunks.refcount` column is dropped by migration 072. The retirement record
(what each rule required, what replaced it, and the calibration evidence)
lives in `docs/spec/models/refcount-records.md`.

#r("store.chunk.no-live-collect")[
  No live chunk is ever collected: if any existing `'complete'` manifest's
  `chunk_list` references chunk `h`, the chunk-backend object for `h` MUST
  exist. A backend DeleteObject for `h` MUST be issued only from a state in
  which no existing manifest of any status references `h` at the instant of
  the delete. `'uploading'` manifests are excluded from the presence clause
  (their backend PUTs may still be in flight --- the write-ahead upsert
  deliberately precedes the upload); their chunks are protected by the same
  no-references condition on the delete action, the grace TTL
  (#rref("store.chunk.grace-ttl")), and the re-upload discipline of
  #rref("store.chunk.liveness-not-presence").
]

This is the chunk store's data-loss invariant, stated mechanism-neutrally:
today it is enforced by the write-ahead chunk-row upsert committing with the
manifest reference (#rref("store.chunk.refcount-txn")), the collect cycle's
fail-closed mark fold and grace term (#rref("store.gc.chunk-collect"),
#rref("store.chunk.grace-ttl")), the upsert's resurrect arm
(`deleted = false`) and `last_referenced_at` touch, the soft-delete clearing
`uploaded_at` (#rref("store.cas.chunk-upload-committed")), and the drain's
`FOR UPDATE` re-check immediately before the irreversible backend delete
(#rref("store.gc.pending-deletes")). It survived the counter's replacement
by manifest-derived collection unchanged.

#r("store.gc.bounded-garbage-retention+3")[
  No dead chunk is retained forever: an existing chunk row referenced by no
  existing manifest, and staying unreferenced, MUST eventually be
  soft-deleted and have its backend object deleted --- or have its
  `pending_s3_deletes` row parked at the attempts cap and surfaced by the
  stuck-deletes alerting (#rref("store.gc.pending-deletes")) --- within
  `ceil(eligible_backlog / COLLECT_CYCLE_VICTIM_CAP)` collect cycles
  (#rref("store.gc.chunk-collect"); one cycle in the steady state where the
  eligible backlog fits under the per-cycle cap), plus the grace window, plus
  drain lag, with the worst-case interval between cycles bounded by the daily
  collect backstop. The bound covers the ROW too: the chunk-row lifecycle is
  insert → live → soft-deleted (`deleted_at` stamped, migration 091) →
  drained (no `pending_s3_deletes` row) → reaped --- a soft-deleted, fully
  drained row whose `deleted_at` is at least the grace term old MUST be
  hard-DELETEd by the post-pass reap of a complete collect cycle (capped per
  cycle like the collect loop; stopping early only retains tombstone rows
  longer). The resurrect upsert clears `deleted_at` with `deleted`, so a
  re-referenced chunk is never reap-eligible. Carve-out: while any existing
  manifest's `chunk_list` fails validation, chunk collection is suspended
  fail-closed and every otherwise-eligible chunk MAY outlive this bound; the
  suspension MUST be alerted (the parse-failure abort), and the bound
  resumes within one cycle of the offending manifest being repaired,
  deleted, or quarantined.
]

The bound was previously one full pass of the applicable legacy reclamation
path (the path sweep's chunk block, the stale-placeholder reclaim, the hourly
orphan-chunk sweep) and was conditional on the hand-maintained refcount being
correct: a counter left above zero by a missed decrement (including the
corrupt-`chunk_list` decrement skip the as-built rules sanctioned) never
returned to zero and the chunk was retained indefinitely --- the carve-out
was a permanent, warning-level-only exemption. The collector replaces the
conditionality:
liveness is recomputed from the manifests each cycle, so a missed decrement
cannot occur, historical leak shapes become ordinary collect-cycle victims,
a backlog larger than the per-cycle cap drains across consecutive cycles
(visible via the backlog gauge and capped-cycles counter), and the carve-out
narrows to the fail-closed, alerted, remediation-bounded pause stated above.

#r("store.chunk.liveness-derived")[
  Chunk GC-eligibility MUST be derived from the durable manifests at collect
  time: a chunk is live iff at least one existing manifest row ---
  `'uploading'` and `'complete'` alike --- references its hash in
  `manifest_data.chunk_list`, and a chunk is eligible for collection only if
  it is absent from that fold as computed by the collect cycle's mark phase
  (#rref("store.gc.chunk-collect")) and outside the cycle's grace term. No
  maintained per-chunk counter or other incrementally-maintained liveness
  aggregate may be consulted for the eligibility decision, and the liveness
  fold MUST NOT be consulted for backend presence
  (#rref("store.chunk.liveness-not-presence")).
]

The replacement counterpart of the retired as-built counter-meaning rule: the
same fold, recomputed from `manifest_data.chunk_list` per cycle instead of
mirrored by hand-maintained arithmetic. It is what dissolves the leaked- and
under-counted-refcount bug classes --- there is no stored aggregate left to
drift --- while #rref("store.chunk.no-live-collect") (unchanged) remains the
data-loss obligation the recomputation must satisfy. The rule is
deliberately silent on the historical counter column itself (written by
pre-cutover pods for mixed-fleet safety until migration 072 dropped it); what
is forbidden is deciding eligibility from any such aggregate.

#r("store.gc.chunk-collect")[
  Chunk collection MUST run as a collect cycle of: (1) a snapshot of the
  cycle cutoff on the database clock; (2) a fail-closed mark over every
  existing manifest --- a server-side validation pass plus a set-based
  expansion of every `manifest_data.chunk_list` into the cycle's live set,
  in which corrupt input is distinguishable from an empty manifest and any
  validation failure aborts the cycle, with an alert, before any verdict or
  collection is produced; (3) collection of chunks absent from the live set
  whose `GREATEST(created_at, last_referenced_at)` predates the snapshot
  cutoff by at least the grace window, soft-deleting them and enqueueing
  them to `pending_s3_deletes` in per-batch transactions, collecting at most
  a fixed per-cycle victim cap with a keyset cursor carrying the remainder
  to subsequent cycles; and (4) the existing drain re-check before the
  irreversible backend delete (#rref("store.gc.pending-deletes")). The cycle
  MUST run as part of every GC run and from a periodic backstop, and the
  writer-transaction soundness condition --- no chunk-referencing write
  transaction outlives the grace window --- MUST be enforced by a
  transaction-duration bound or carried as a named, monitored assumption.
]

The cycle shape, the fail-closed polarity (a parser regression or data
damage suspends collection rather than collecting live data), the
`last_referenced_at` touch that closes the mark-snapshot race, the capped
cursor-resumable collect (a backlog drains across cycles instead of
stretching one cycle past its lock-held budget), and the soundness
assumption are the design's §4.1/§4.4 commitments; the collector
implementation in `rio-store/src/gc/collect.rs` ships the cycle in shadow
mode (mark + report) ahead of the cutover release that enables the
collecting arm. Like #rref("store.chunk.liveness-derived"), this rule never
treats the live set as a presence signal: presence remains keyed on
`uploaded_at` (#rref("store.chunk.liveness-not-presence")).

= Key Operations

#table(
  columns: 2,
  align: (left, left),
  table.header([Operation], [Description]),
  [`PutPath(narinfo, nar_stream)`],
  [Chunk the NAR, verify NAR hash, deduplicate chunks, store metadata],

  [`GetPath(store_path)`],
  [Return narinfo + reconstruct NAR from verified chunks],

  [`QueryPathInfo(store_path)`], [Return narinfo only],
  [`BatchQueryPathInfo(paths)`],
  [Batch narinfo lookup, one PG round-trip (#rref("store.api.batch-query"))],

  [`BatchGetManifest(paths)`],
  [Batch narinfo + manifest-availability lookup, 1 PG round-trip
    (#rref("store.api.batch-manifest"))],

  [`FindMissingPaths(paths)`],
  [Batch validity check (like REAPI's FindMissingBlobs)],

  [`QueryPathFromHashPart(hash_part)`],
  [Resolve full store path from 32-char @nixbase32 hash prefix
    (#rref("store.api.hash-part"))],

  [`AddSignatures(store_path, sigs)`],
  [Append ed25519 signatures to existing narinfo
    (#rref("store.api.add-signatures"))],

  [`RegisterRealisation` / `QueryRealisation`],
  [CA derivation output mapping (#rref("store.realisation.register"),
    #rref("store.realisation.query"))],
)

== Batch Query RPCs

#r("store.api.batch-query+2")[
  `BatchQueryPathInfo` returns `(store_path, Option<PathInfo>)` for many paths
  in ONE PostgreSQL round-trip. Local-only --- it does NOT trigger upstream
  substitution and does NOT apply the cross-tenant signature-visibility gate
  (both add per-path round-trips, defeating the batch); instead it rejects
  end-user tenant tokens with `PERMISSION_DENIED` (anonymous/service callers
  unfiltered per #rref("store.tenant.narinfo-filter")). The request is bounded
  by `max_batch_paths` (default `DEFAULT_MAX_BATCH_PATHS`, configurable via
  `RIO_MAX_BATCH_PATHS`); over-cap returns `INVALID_ARGUMENT` naming the env
  var. Every path is `validate_store_path`-checked before PG. I-110: builder
  closure-BFS (`compute_input_closure`) is the only current caller; the
  per-path → batch swap was the 130× scale unlock.
]

#r("store.api.batch-manifest+3")[
  `BatchGetManifest` returns `(store_path, Option<ManifestHint>)` for many
  paths in ONE PostgreSQL round-trip. A `ManifestHint` carries the path's
  `PathInfo`; it is present only for paths with a complete manifest and never
  carries manifest content (a per-file chunk list cannot reassemble a NAR
  without the Directory DAG --- clients that want the NAR call `GetPath`).
  Same local-only / DoS-bound / validation / end-user-tenant-rejection rules
  as #rref("store.api.batch-query").
]

#r("store.api.hash-part+2")[
  `QueryPathFromHashPart` resolves a full store path from its 32-char nixbase32
  hash prefix (the 20-byte `compressHash` output). The hash part MUST be
  exactly 32 chars and MUST decode as nixbase32 --- both checked BEFORE the PG
  query. The decoded bytes are discarded; the decode is purely a validator that
  blocks LIKE-injection (the lookup builds `'/nix/store/{hash}-%'`, and
  nixbase32's alphabet contains neither `%` nor `_`). Returns `NOT_FOUND` if no
  matching `'complete'` narinfo exists. Applies
  #rref("store.substitute.tenant-sig-visibility"). Backs the gateway's
  `wopQueryPathFromHashPart`.
]

#r("store.api.add-signatures+2")[
  `AddSignatures` appends ed25519 signature strings to an existing `'complete'`
  narinfo (`array_cat`, deduped). The signatures list is bounded by
  `MAX_SIGNATURES`. An empty list is a no-op (NOT an error --- `nix store sign`
  with no configured key legitimately produces it). Returns `NOT_FOUND` if the
  path has no `'complete'` narinfo. Applies
  #rref("store.substitute.tenant-sig-visibility"): an authenticated caller may
  only append signatures to a path it can see; gate-hidden paths return
  `NOT_FOUND` (the gate runs BEFORE the empty-list short-circuit so the RPC
  can't be used as a cross-tenant existence probe). The store does NOT verify
  the signatures --- that's the consumer's job at `narinfo` read time. Backs
  the gateway's `wopAddSignatures`.
]

== Write-Ahead Manifest Pattern (PutPath flow)

#r("store.put.wal-manifest")[
  *Authorization:* Executor `PutPath` calls include an HMAC-SHA256-signed
  assignment token in the `x-rio-assignment-token` gRPC metadata header. The
  store verifies the token signature, checks expiry, and rejects uploads whose
  `store_path` is not in `claims.expected_outputs`. The gateway (which handles
  `nix copy --to` and has no assignment) bypasses the assignment-token check
  via a service token --- see #rref("sec.authz.service-token").
]

#r("sec.authz.ca-path-derived+3")[
  For floating-CA derivations (`AssignmentClaims.is_ca = true`),
  `expected_outputs` is unknown at dispatch time. Instead of skipping
  authorization, the store derives the CA store path *server-side* via
  `StorePath::make_fixed_output(name, nar_hash, recursive=true, refs)` and
  rejects with `PERMISSION_DENIED` if it does not match the uploaded
  `store_path`. The `nar_hash` input is the SHA-256 the store computed over
  the buffered NAR for NAR-byte uploads, and the builder-claimed NAR hash for
  `PutPathChunked` (#rref("store.integrity.verify-on-put")) --- either way the
  uploaded path must be the fixed-output derivation of the hash and references
  the same upload asserts. The CA-path check MUST run BEFORE the `'uploading'`
  placeholder is claimed (#rref("store.put.wal-manifest") step 1), so a worker
  holding an `is_ca` token cannot squat placeholders for arbitrary paths (it
  would otherwise drip-feed chunks while heartbeating an arbitrary path's
  placeholder fresh, forcing legitimate uploaders into `Aborted`).
]

#r("sec.authz.service-token")[
  Trusted control-plane callers present an `x-rio-service-token` header: an
  HMAC-SHA256-signed `ServiceClaims { caller, expiry_unix }` keyed with
  `RIO_SERVICE_HMAC_KEY_PATH` (a separate secret from the assignment-token
  key). The store verifies signature and expiry, then checks `caller ∈
  service_bypass_callers` (default `["rio-gateway"]`). A valid service token
  bypasses the assignment-token check --- the gateway mints one per `PutPath`
  with a 60-second expiry. Transport-agnostic: works over plaintext-on-WireGuard
  with no TLS dependency.
]

+ *Idempotency check + `'uploading'` placeholder:* If a `'complete'` manifest
  already exists for this path, return success immediately (fast-path no-op).
  Otherwise, insert an `'uploading'` placeholder row in `manifests` as an
  idempotency lock --- this PG write happens *before* the NAR is buffered or
  verified.
+ *Buffer + verify:* Accumulate the streamed NAR chunks into a buffer, then
  compute SHA-256 over the buffered bytes and verify against the declared
  `NarHash`. On mismatch, delete the placeholder row and reject.
+ *Write-ahead manifest (PG):* Chunk the buffered NAR with @fastcdc, then in a
  single PostgreSQL transaction: write `manifest_data` (serialized chunk list)
  and upsert the chunk rows (#rref("store.chunk.refcount-txn")). The durable
  reference protects the chunks from collection immediately --- the collect
  cycle's mark fold counts `'uploading'` manifests --- and the upsert's
  RETURNING clause is the dedup verdict for the next step.
+ *Upload new chunks:* Parallel S3 PUTs (8-wide) for exactly the hashes the
  step-3 upsert returned as `needs_upload = (uploaded_at IS NULL)`
  (#rref("store.cas.upsert-inserted")) --- chunks with confirmed backend
  presence are skipped; everything else is (re-)uploaded. No post-upload
  HeadObject verification --- integrity is guaranteed by BLAKE3 verification
  on every read (see #rref("store.integrity.verify-on-get")). Successful PUTs
  are then recorded via `mark_chunks_uploaded`
  (#rref("store.cas.chunk-upload-committed")).
+ *Complete:* Flip manifest status to `'complete'` in a single PG transaction
  (also fills real narinfo fields, references, content index entries).

*On graceful error (steps 4--6 return `Err`):* `put_chunked` deletes its own
`'uploading'` placeholder rows (a claim-gated reap) before returning. Chunks
already uploaded to S3 are *not* deleted --- chunk reclamation is the collect
cycle + drain's responsibility, and deleting now would race with a concurrent
uploader that just re-referenced the same chunk.

*On crash (process dies between steps 3 and 6):* the orphan scanner reclaims
stale `'uploading'` records after a compile-time threshold (`STALE_THRESHOLD`,
15 minutes --- was 2 hours; tightened because substitution made stale
placeholders a hot-path blocker, see #rref("store.substitute.stale-reclaim")).
Reaping deletes the abandoned path rows only; chunks the dead manifest leaves
unreferenced age past grace and are collected by a later collect cycle
(#rref("store.gc.chunk-collect")). No full S3 enumeration needed.

#r("store.gc.orphan-heartbeat")[
  Uploaders MUST heartbeat `manifests.updated_at` during long-running chunk
  uploads (interval: ≤30s or ≤64 chunks, whichever first) so the orphan
  scanner's stale-threshold check distinguishes in-progress uploads from
  crashed ones. Without heartbeat, `updated_at` reflects insert time --- a
  16-minute upload over 50Mbps would be reaped at the 15-minute mark.
]

#r("store.put.idempotent")[
  *Idempotency:* If `PutPath` is called for a store path that already has a
  `'complete'` manifest, the call returns success immediately without
  re-uploading. This makes concurrent uploads of the same path safe.
]

#r("store.put.concurrent-wait")[
  When the write-ahead claim (#rref("store.put.placeholder-claim")) finds a
  live concurrent uploader for the same path, `PutPath` and `PutPathChunked`
  MUST wait --- bounded by a server-side budget (default 60 s, well under the
  client RPC deadline) --- for the in-flight upload to resolve, then take the
  idempotent-skip path (#rref("store.put.idempotent"), winner committed) or
  claim the freed placeholder (winner aborted). Only when the budget expires
  with the uploader still live does the call surface `ABORTED`.
]

Without the wait, the loser of a same-path race got `ABORTED "concurrent
PutPath in progress … retry"` immediately --- and for the gateway's
`wopAddMultipleToStore` leg the "retry" was a lie. The gateway's buffered
re-send retry has a \~6 s budget tuned for KB-sized `.drv` NARs and cannot
cover a winner streaming a chunked NAR for tens of seconds; the gateway's
streaming path (oversize entries) cannot retry at all because the NAR bytes
were already consumed off the wire. Either way the client's whole upload died
over a race whose outcome it didn't need to win: the loser's optimal result is
the idempotent skip, which requires no bytes. The store is the only layer that
can observe the winner's placeholder directly, so it waits there; client-side
retry budgets stay as the fallback for winners that outlive the server-side
budget.

#r("store.put.tenant-junction")[
  Every upload RPC (`PutPath`, `PutPathBatch`, `PutPathChunked`) MUST record
  the caller's resolved tenant (the same resolution the castore read side
  uses, #rref("store.castore.tenant-scope")) as a `path_tenants` row for
  every output it commits, and for every output it idempotent-skips as
  already complete. A caller with no resolvable tenant (dev mode,
  service-token caller without a JWT) writes no row. A junction insert that
  fails because the tenant was deleted while the upload was in flight MUST
  NOT fail the upload. Upstream substitution is NOT an upload RPC and MUST
  NOT write the junction (#rref("store.substitute.tenant-sig-visibility")).
]

Without the commit-time row, a path uploaded by a tenant is invisible to that
same tenant through the castore read surface
(#rref("store.castore.tenant-scope")): the gateway uploads every `.drv` via
`PutPath`, the builder then opens it through castore-FUSE `ReadBlob`, and the
inner join on `path_tenants` turns the missing row into `NotFound` → `EIO` →
the build dies as a spurious infrastructure failure. The idempotent-skip half
matters because content-addressed paths deduplicate across tenants: the prior
commit may belong to another tenant (or predate tenancy), and the skipping
caller still needs castore read access and a GC pin of its own.

The substitution exclusion is the other half of the same coin: a substituted
path's cross-tenant visibility is signature-gated
(#rref("store.substitute.tenant-sig-visibility")), so a junction row written
at substitution time launders the path into "built" --- hiding it from other
tenants who trust the same upstream and corrupting the scheduler's
cached-output check (#rref("store.substitute.find-missing-gated")). Substituted
outputs are pinned per-tenant by the scheduler's completion upsert instead
(#rref("sched.gc.path-tenants-upsert")).

#r("store.put.placeholder-refs")[
  The `'uploading'` placeholder narinfo MUST carry `references` from the
  instant it commits (same INSERT, same transaction as the `manifests` row).
  PutPath does NOT take any GC-related advisory lock. The placeholder's
  references are what protect its closure from GC: either mark's CTE seed (b)
  walks them (placeholder committed before mark's snapshot), or sweep's
  per-path re-check sees them (#rref("store.gc.sweep-recheck")). Rationale:
  I-192 --- the previous `GC_MARK_LOCK_ID` advisory lock was redundant with the
  re-check, and surfaced `Aborted` to `nix copy` under mark-CTE pressure
  (I-168).
]

#r("store.atomic.multi-output")[
  Multi-output derivation registration MUST be atomic at the DB level: all
  output rows commit in one transaction, or none do. Blob-store writes are NOT
  rolled back (orphaned blobs are unreferenced and eligible for the next
  collect cycle). The bound is ≤1 NAR-size per failure.
]

#r("store.put.nar-hold-envelope+2")[
  Every NAR-budget HOLD must be tiled by ONE typed transfer-deadline envelope
  from first permit grant to release: the envelope is derived from the hold's
  byte basis at the floor rate (`deadline = NAR_HOLD_GRACE_FACTOR ×
  stall_window + bytes_basis / NAR_HOLD_FLOOR_RATE`), armed exactly once at
  the FIRST grant (park time never consumes hold time), and every await
  reachable while holding derives its clock from that one envelope's
  remaining budget — per-span fresh clocks and unclocked inter-span awaits
  are both violations. Knowledge improvement (the buffered byte count
  becoming known at stream drain) may only TIGHTEN the deadline, never
  re-arm it. Every non-reservation budget acquire must shed typed
  (`ResourceExhausted`) within the wait grace (`BUDGET_WAIT_GRACE`);
  zero-holding parks are exempt backpressure.
]

The envelope's arming basis is the hold's own ceiling: the verified
narinfo's declared `NarSize` for a substitution hold (known up front), the
`MAX_NAR_SIZE` charged-permit cap for a PutPath/PutPathBatch ingest hold
(total size is trailer-only, so the handler's cumulative-charge ceiling is
the only sound basis at first grant), tightened to the actual buffered byte
count once the stream drains (`min` of deadlines — a stream that spent most
of its cap budget cannot buy a fresh tail allowance; that fresh-clock shape
is the bughunt-9 bug_114 defect: stage/commit clocks re-armed per span while
the inter-span claim/signer PG round-trips ran unclocked). The ingest plane
realizes the tiling structurally with a holder type (`NarIngestHold`): the
permits are private to it and its `bounded()` combinator is the only way to
await while holding — a bare await on a held frame does not typecheck. The
grace factor is pinned `≥ 2` so the per-read stall clock always fires first
on a genuinely wedged read; the wait grace is pinned strictly inside the
smallest hold grace so a waiting holder sheds its WAIT before its own HOLD
deadline can fire. Both knobs and the floor rate are violable builder
overrides (R17) with violating reds (`hold_envelope_floor_rate_binds`,
`wait_grace_binds`, `tenant_cap_binds`). The one deliberate non-clock: the
substitute leg's zero-holding budget park stays untimed (`BudgetParked`,
takeover-exempt) — parking is backpressure, never a strike, and the park's
boundedness is the #rref("store.put.nar-bytes-budget") theorem, not a clock.

#r("store.put.nar-bytes-budget+6")[
  A process-global `tokio::sync::Semaphore` (default `8 × MAX_NAR_SIZE` = 32
  GiB; configurable via `nar_buffer_budget_bytes`, floored by `validate()` at
  `MAX_NAR_SIZE` so every deployed budget admits at least one whole-NAR
  reservation) bounds in-flight NAR bytes across ALL concurrent `PutPath` AND
  upstream-substitution NAR ingests (one shared `Arc<Semaphore>`). Two charge
  regimes, one semaphore unit, ONE hold/wait discipline
  (#rref("store.put.nar-hold-envelope"): waiters park free, holders expire).
  *Trailer-mode PutPath/PutPathBatch (per-chunk):* each handler
  `acquire_many(nar_chunk_charge(chunk.len()))` BEFORE extending its
  `nar_data: Vec<u8>`, with the acquire wait-grace-bounded (grant or typed
  `ResourceExhausted` shed — uniform over all chunk acquires, first
  included); permits are held in a `Vec<SemaphorePermit>` and released on
  handler exit (any path), with holder residency bounded by the ingest hold
  envelope from the first granted permit and the persist spans enveloped over
  their buffered bytes. Empty `NarChunk` messages are rejected with
  `InvalidArgument`; tiny chunks are charged a floor of
  `MIN_NAR_CHUNK_CHARGE` (256) bytes so per-permit tracking overhead is
  itself bounded by the budget; the per-request `MAX_NAR_SIZE` raw-content
  check uses `>=` so a single chunk of exactly 2³² bytes is rejected before
  it reaches `acquire_many(0)`. Both RPCs track cumulative *charged permits*
  (each chunk charged `nar_chunk_charge(len) = max(len,
  MIN_NAR_CHUNK_CHARGE)`) and reject at `MAX_NAR_SIZE` BEFORE `acquire_many`
  --- `PutPathBatch` with `FailedPrecondition` (builder falls back to
  per-output `PutPath`), `PutPath` with `InvalidArgument`
  (too-many-tiny-chunks is a client bug). *Substitution (whole-NAR
  reservation):* the substitute leg MUST charge its entire declared size in
  ONE reservation (`declared.max(MIN_NAR_CHUNK_CHARGE)`), acquired after the
  placeholder claim arms and before the NAR GET, with `declared >=
  MAX_NAR_SIZE` rejected first (PutPath parity; also what makes the
  reservation `u32`-expressible); a budget waiter therefore holds ZERO
  permits, the read loop has no acquire site, the decompressed read cap is
  `declared + 1` (over-delivery fails as `SizeMismatch` during the read, so
  buffered bytes never exceed the reservation), and the reservation's
  lifetime MUST cover the bytes' residency --- it rides the buffer through
  the hash `spawn_blocking` detach and is credited back after `persist_nar`
  (or when the detached hash task's output drops, under cancellation) ---
  while the whole hold (read span AND the post-read hash→sigs→persist tail)
  is bounded by the reservation's hold envelope. *Cost axis (per-tenant
  cap):* substitution charge is declaration-priced (a hostile upstream's lies
  are free to make), so the reservation constructor additionally charges a
  per-tenant outstanding-aggregate ledger capped at `TENANT_RESERVATION_CAP =
  2 × MAX_NAR_SIZE` (¼ of the default pool: ≥ 4 tenants must collude to fill
  the pool by declaration; one tenant's parallel warm of two max-size
  closures is preserved) --- over-cap is a typed REFUSAL
  (`TenantBudgetExhausted`, before the park, never queued). The substitute
  leg's entire cumulative charged demand IS its single reservation `<
  MAX_NAR_SIZE`, so the charged-permit-unit law --- the unit MUST match the
  semaphore's so a single handler can never demand more than `MAX_NAR_SIZE`
  permits, and with `budget >= MAX_NAR_SIZE` (validate()-enforced; production
  8×) a single handler can never self-deadlock on permits it holds --- holds
  for EVERY handler in this rule's scope. *The no-deadlock theorem,
  UNCONDITIONAL over the machine acquire-site census (both regimes, no
  population proviso, no axis exemption):* every parked head is granted
  within the sum of the residual holder bounds. Premise-to-witness table ---
  (i) fair-FIFO grant order over `acquire_many`: W8-E
  `budget_grants_follow_arrival_order`; (ii) every hold expires (read spans,
  ingest residency, persist tails): W8-B
  `trickle_hold_aborts_at_the_transfer_deadline`, W8-B′
  `blackholed_persist_releases_by_the_hold_deadline` (the persist-span
  holder), W8-C `stopped_client_putpath_releases_by_the_ingest_deadline`,
  W8-F `stopped_client_batch_releases_by_the_ingest_deadline`; (iii) every
  holding wait sheds: W8-D `chunk_acquire_sheds_typed_after_wait_grace`;
  (iv) every single demand < the `validate()` floor ≤ budget: E-2(i) + the
  const-asserts. The hash compute bound (≈ 10 s / 4 GiB) is PRICED slack on
  a deadline-expired holder's release lag (the `spawn_blocking` digest is
  non-cancellable; its completion still drops the moved reservation) ---
  slack on top of the enforced deadline, never a premise. *Unification note
  (LANDED --- the recorded successor hypothesis discharged):* the wire now
  carries opt-in declared-size metadata, and a DECLARED `PutPath`
  (#rref("store.put.declared-reserve")) ingests in reservation mode --- the
  whole charge in ONE pre-stream acquisition, no per-chunk acquire site, the
  substitute leg's discipline applied to ingest. The per-chunk regime above
  remains the TRAILER-mode law (the builder's single-pass tee cannot declare
  --- the capability boundary, not a version bridge); the no-deadlock
  theorem gains no premise: the declared acquisition is a zero-holding park
  (the waiter holds no permits and no buffered bytes), the same class as the
  substitute reservation already in the census. *Priced
  adversarial-availability bound (NOT a
  safety proviso):* a hostile tenant pins at most `TENANT_RESERVATION_CAP`
  (¼ default pool) per tenant, each leg for at most one hold deadline
  (`5 × stall_window + declared / 256 KiB/s` ≈ 4.6 h worst case), and an
  extender must hold live TCP and deliver ≥ the floor rate --- all
  observable via the `budget_parked` claim-phase mirror. When the budget is
  exhausted, the `await` backpressures (gRPC flow control / `BudgetParked`
  substitution parking) within these bounds instead of OOMing the process.
  NOT shared with GetPath's chunk cache (moka-bounded separately).
]

#r("store.put.declared-reserve")[
  A `PutPath` whose metadata carries a nonzero `declared_nar_size` MUST be
  ingested in RESERVATION MODE: the handler charges
  `declared.max(MIN_NAR_CHUNK_CHARGE)` in ONE `acquire_many` BEFORE reading
  any chunk --- a zero-holding park (the waiter holds no permits and no
  buffered bytes; its placeholder claim carries no budget) --- arms the ONE
  hold envelope on the declared-byte basis at the grant, and never acquires
  again: the trailer regime's per-chunk acquire-while-holding is
  structurally unreachable on this path. `declared >= MAX_NAR_SIZE` refuses
  before the claim (which also makes the charge `u32`-expressible). The
  declaration is a BINDING BOUND: a chunk that would push the buffer past it
  refuses `InvalidArgument` AT the crossing chunk (buffered bytes never
  exceed the reservation); the still-MANDATORY trailer's `nar_size` MUST
  equal the declaration (refused at commit otherwise); a short stream dies
  on that equality or on `verify_nar`'s size check --- both delivery axes
  typed. `declared_nar_size = 0` IS trailer mode, byte-for-byte.
  `PutPathBatch` REJECTS a nonzero value fail-closed (its senders are
  trailer-capability). Opt-in is by sender CAPABILITY, never by version:
  buffered/size-known senders (the gateway copy and streaming paths)
  declare; the builder's single-pass tee cannot know the size up front and
  stays trailer-mode by design.
]
This is the root-cause kill of the ingest plane's hold-and-wait (N1): the
wave-8 envelope family bounded the hold's TIME; the declared mode removes
the SHAPE --- park free, then hold, exactly the substitute leg's
single-shot reservation discipline applied to upload. The capability
boundary is the round-8 refutation carried verbatim: a tee that streams
while hashing cannot declare, so the field is opt-in rather than a
release-skew bridge (the `--wipe` posture). The declared reservation's
COST is governed by #rref("store.budget.cost-axis") below.

#r("store.budget.cost-axis")[
  Every acquisition against the shared NAR-byte budget that is priced by
  DECLARATION (a wire-supplied size --- the substitute leg's upstream
  `NarSize` and PutPath's `declared_nar_size` --- rather than by delivered
  bytes) MUST carry the cost axis before any grant: the acquisition is
  constructible ONLY through the one `DeclaredCharge` constructor, whose
  signature requires the charging tenant and aggregate cap and whose ledger
  consult precedes the semaphore park (an over-cap tenant REFUSES typed and
  retryable, never queues). The raw semaphore MUST be module-private to the
  sealed budget home (`rio-store/src/budget.rs` --- `NarBudget`): the only
  debit paths are `DeclaredCharge::new` and the delivery-priced per-chunk
  face (`acquire_chunk`, wait-grace-bounded at its one chokepoint);
  `add_permits`/`forget`/bare acquires are unwritable outside the module,
  and reads (`available_permits`) are not debits. The declared envelope's
  axes, ALL bound: *time* --- the hold envelope armed at grant
  (#rref("store.put.nar-hold-envelope")); *size* --- per-charge
  `declared < MAX_NAR_SIZE`, refused up front; *cost* --- per-tenant
  aggregate outstanding charge `<= TENANT_RESERVATION_CAP` (2 ×
  `MAX_NAR_SIZE` = 8 GiB, ¼ of the default pool), keyed by the
  HMAC-claims-signed tenant with unattributed authorities sharing one
  capped nil bucket; *population* --- per-tenant concurrent CHARGE COUNT is
  documented-N/A: the byte aggregate is the pool's exhaustible resource and
  the cap bounds it directly, each charge rides a connection-bounded
  streaming RPC or an admission-gated substitute leg, and ledger memory is
  one map entry per actively-charged tenant.
]
The rule is merged_bug_005's close: wave-9's `reserve_declared` shipped as
the bare sibling of the substitute reserve --- whole wire-supplied charge,
no ledger --- so eight ~4 GiB declarations from one worker pinned the full
32 GiB pool at zero bandwidth, renewable for the whole hold envelope. The
constructor (not a callsite sweep) is the enforcement: the debit face is
compile-sealed by module privacy, and the crate-wide acquire-site census
(`nar_budget_acquire_site_census`, planted-red per R22′) is the belt under
it. Trailer mode stays delivery-priced by design --- an attacker there pays
real bandwidth, which IS the cost axis.

#r("store.budget.lane-fairness")[
  Granularity-mismatched waiters on one budget carry a priced ORDERING
  axis: when the pool exceeds one whole-NAR reservation, the budget MUST
  reserve a chunk lane --- `min(pool/8, pool - MAX_NAR_SIZE)` byte-permits
  acquirable only through the per-chunk debit face --- so an in-flight
  trailer chunk is never starved to a shed by a whole-NAR declared
  reservation parked at the declared face's fair-FIFO head, while the
  parked declaration retains its FIFO liveness over a declared face that
  never shrinks below `MAX_NAR_SIZE` (every admissible declaration stays
  grantable). The two faces MUST sum to the constructed pool (the
  in-flight NAR-bytes bound is unchanged), both faces MUST live inside the
  sealed budget home as one type, and the chunk-shed disclosure MUST state
  the measured cause (free bytes at shed versus the charge), never an
  unconditional at-bound claim.
]
The parked-head freeze itself is designed and load-bearing (the
merged_bug_101 triage correction, binding): holder sheds release the head,
and FIFO-head admissibility is what keeps near-MAX declarations live ---
re-queueing the head (the rejected tail-requeue fix) would trade the chunk
starvation for indefinite declaration starvation. The lane buys both
halves: chunks drain at bounded latency through their own face; the head
keeps its position. Pools at or under one reservation (tests, dev
profiles) carry no lane and keep the single-face semantics.

#r("store.budget.lane-floor")[
  A carved sub-pool consumed via `acquire_many` MUST carry a structural
  relation to the largest unit request it can lawfully receive: the chunk
  lane's derived capacity is FLOORED at the largest lawful chunk charge
  (the gRPC message cap; charge is identity above the 256-byte minimum)
  --- `lane = 0` below one lawful charge (the pool keeps the legacy
  single-face semantics), else the derived size. The value axis is total
  over its three bands {zero, band, in-band}; no admitted configuration
  may mint a lane smaller than a charge the wire admits.
]
A lane in `(0, max-charge)` is an anti-progress device, strictly worse
than no lane: a lawful max-size chunk's lane arm parks UNSATISFIABLE at
the fair-FIFO head (charge exceeds capacity even at full idle), hoarding
released permits while every smaller acquire behind it sheds at grace and
re-wedges on retry --- client-controllable chunk sizes made the band an
ingest-plane DoS in a validation-admitted configuration (merged_bug_133:
the wave-11 seal enumerated the capacity axis as {zero, positive} and its
witness measured only the in-band cell). `validate()` names the band at
its admitting floor; the derivation owns the repair --- no new rejection.

#r("store.put.placeholder-claim+2")[
  `insert_manifest_uploading` generates a fresh `claim_id UUID` per placeholder
  and returns it to the caller. Every owner-side mutation ---
  `heartbeat_uploading`, `complete_manifest_in_conn`, `abort_placeholder`, the
  drop-guard, `put_chunked`'s complete-failure rollback --- filters on
  `claim_id = $id` (cleanup paths via `reap_one(ReapBy::Claim(id))`, which
  additionally filters `status='uploading'`). This makes a late-firing
  operation a no-op when the owner's row was already deleted (orphan scanner /
  `stage_chunked` rollback) and another uploader has since inserted a fresh row
  at the same `store_path_hash` --- status alone is not ownership. Without the
  heartbeat filter, a stale uploader's still-running heartbeat keeps the
  foreign placeholder artificially fresh; without the completion filter, a
  stale uploader overwrites the re-uploader's
  `signatures`/`deriver`/`registration_time`. The orphan scanner and hot-path
  stale-reclaim use `ReapBy::Stale{secs}` (the `updated_at` heartbeat protects
  live uploaders there). The `Option<i64>`-threshold form is removed: every
  caller supplies a staleness gate or its claim token.
]

#r("store.put.drop-cleanup+3")[
  The `PlaceholderGuard` is armed INSIDE `claim_placeholder`, BEFORE the
  placeholder INSERT, with the client-generated `claim_id` it will carry;
  `PlaceholderClaim::Owned` carries the guard by value, so a handler future
  cannot observe ownership without the drop-reap already armed. The guard
  (a) heartbeats `manifests.updated_at` every 30s while held, so
  #rref("store.put.stale-reclaim")'s `reap_one(SUBSTITUTE_STALE_THRESHOLD)`
  never reaps a live owner during a long ingest/stage (6 GB at 50 Mbps ≈
  16 min); and (b) on Drop, spawns
  `gc::orphan::reap_one(store_path_hash, ReapBy::Claim(claim_id))`. The guard's
  heartbeat (`WHERE claim_id=$id`) and drop-reap (`ReapBy::Claim(id)`, which
  filters `status='uploading' AND claim_id=$claim`) are both no-ops against a
  row that was never inserted, so pre-arming is free; the guard is defused on
  `AlreadyComplete`/`Concurrent` and only on success otherwise. This covers the
  handler future being DROPPED --- tonic aborts the task when the client
  `RST_STREAM`s (builder killed mid-upload) --- which the explicit
  `abort_upload` calls on `return Err` paths do NOT cover, and (sh-023) closes
  the residual where the future is dropped at the INSERT's own
  `tx.commit().await` cancellation point: PG committed the row but the sqlx
  future was dropped before reading the result, so the +2-era caller-side
  `spawn_placeholder_guard` never ran and every retry inside 5 min hit
  `Concurrent`. Firing after an explicit `abort_upload`, after the orphan
  scanner reaped our row, or after a fresh re-upload took the slot is a
  harmless no-op (#rref("store.put.placeholder-claim")); firing after
  `upgrade_manifest_to_chunked` deletes the staged placeholder rows, leaving
  the staged chunks unreferenced for the collect cycle. I-125a: a
  phantom-drained builder used to leak the placeholder until the orphan scanner
  reaped it (15 min); the builder treats that `Aborted` as a transient error
  inside its normal upload retry budget --- by the next attempt this drop-path
  cleanup has released the placeholder, or the concurrent upload finished and
  the retry lands as an idempotent skip
  (#rref("builder.upload.idempotent-precheck")).
]

= NAR Reassembly

#r("store.nar.reassembly")[
  - Load the full manifest into memory (list of chunk digests --- even a 10GB
    NAR is only \~5MB of manifest)
  - Parallel chunk prefetch with a sliding window (K=8 concurrent fetches via
    `futures::stream::buffered()`, which preserves chunk ordering)
  - In-process LRU chunk cache (configurable, default 2GB) to avoid repeated S3
    round-trips for hot chunks
  - BLAKE3-verify every chunk on read (see Content Integrity Verification
    above)
  - Stream the reassembled NAR to the client without materializing the full NAR
    in memory
]

#r("store.get.size-sanity-check")[
  Before streaming, `GetPath` MUST verify the manifest's summed size (inline
  blob length, or sum of chunk sizes) equals `narinfo.nar_size`. A mismatch
  indicates manifest/narinfo drift --- PutPath wrote inconsistent state, or the
  DB was manually modified. The store MUST return `DATA_LOSS` without streaming
  any NAR bytes. This is a fail-fast over the post-stream integrity check,
  which would only catch the drift after the client received (and wasted
  bandwidth on) a corrupt NAR.
]

#r("store.get.chunk-prefetch")[
  The chunked-manifest stream MUST drive `chunk_prefetch_k` (default 64,
  configurable via `RIO_CHUNK_PREFETCH_K`) `get_verified()` futures in flight
  via order-preserving `.buffered()`. Cold-cache throughput is latency-bound at
  `K × CHUNK_AVG / s3_ttfb`, so K is the primary throughput knob; per-stream
  memory cost is bounded by `K × CHUNK_MAX`. `buffer_unordered` MUST NOT be
  used --- chunk order is the NAR byte order.
]

#r("store.shutdown.drain-getpath")[
  On SIGTERM, after flipping health to `NOT_SERVING` and sleeping `drain_grace`
  for endpoint propagation, rio-store MUST wait for the active-`GetPath`-stream
  count to reach zero (or `stream_drain_secs` to elapse, default 90) BEFORE
  cancelling the tonic listener. The pod's `terminationGracePeriodSeconds` MUST
  cover `drain_grace + stream_drain_secs` plus slack so kubelet's SIGKILL is a
  backstop, not the normal exit path. ComponentScaler's `MAX_SCALE_DOWN_STEP`
  is sized assuming SIGTERM drains in-flight work; without this wait, a
  scale-down resets the h2 connection mid-stream and the client retries the
  whole NAR from byte zero.
]

= Request Coalescing (Singleflight)

#r("store.singleflight")[
  When multiple concurrent requests need the same chunk from S3 (common during
  cold starts or thundering herd scenarios), rio-store coalesces them into a
  single in-flight fetch using a singleflight pattern:

  - A `DashMap<[u8; 32], Shared<BoxFuture<'static, Option<Bytes>>>>` tracks
    in-flight S3 GETs (the fetch is spawned as a tokio task and its
    `JoinHandle` is mapped to `Option<Bytes>` before `.shared()`, since
    `JoinError` is not `Clone`)
  - First request for chunk X spawns the fetch task and inserts the shared
    future
  - Subsequent requests for chunk X `.await` the existing shared future instead
    of issuing duplicate S3 GETs
  - On completion (success or failure), the entry is removed from the map
  - Failed fetches (including task panics) resolve to `None`, and removal from
    the map means the next request retries cleanly
]

This is critical for cold start thundering herd: when many builds start
simultaneously and request overlapping closures, without coalescing S3 would
see O(N·M) GET requests instead of O(M) where N is concurrent builds and M is
unique chunks.

= Signing Key Management

#r("store.signing.fingerprint")[
  - Per-instance signing key stored in a Kubernetes Secret (recommend KMS/Vault
    for production)
  - Signatures are computed at `PutPath` time --- read-path consumers (gRPC
    `QueryPathInfo`, gateway narinfo responses) do not need private key access
    at serve time
  - Narinfo `Sig:` field format: `<key-name>:<base64-ed25519-signature>`
    (compatible with `nix.settings.trusted-public-keys`)
  - Signed message: canonical fingerprint
    `1;<store-path>;sha256:<nar-hash-nixbase32>;<nar-size>;<sorted-refs-comma-sep>`
    --- semicolon separator, `1;` version prefix, `sha256:` algorithm tag,
    references are full paths (not basenames) joined by comma. Matches Nix's
    `ValidPathInfo::fingerprint()` in `path-info.cc`. See `fingerprint()` in
    `rio-nix/src/narinfo.rs`.
  - Multi-tenant: each tenant can have their own signing key for their paths
]

#r("store.signing.empty-refs-warn")[
  When signing a non-CA path with zero references, the store MUST emit a
  warning. Non-leaf derivations with empty references indicate the executor's
  reference scanner missed deps.
]

#r("store.tenant.sign-key")[
  narinfo signing MUST use the tenant's active signing key from `tenant_keys`
  when present, falling back to the cluster key otherwise. A tenant with its
  own key produces narinfo that `nix store verify --trusted-public-keys
  tenant:<pk>` accepts for that tenant's paths only.
]

#r("store.tenant.narinfo-filter")[
  Authenticated narinfo requests MUST filter results by
  `path_tenants.tenant_id = auth.tenant_id`. Anonymous (unauthenticated)
  requests return unfiltered
  results for backward compatibility.
]

#r("store.tenant.valid-paths-filter")[
  Validity and missing-path checks (`QueryPathInfo` presence,
  `FindMissingPaths` --- what `wopIsValidPath` and `wopQueryValidPaths`
  consume) MUST apply the same tenant visibility as the castore read surface,
  with no `.drv` exemption: an authenticated caller is told a path is valid
  only if a `path_tenants` row grants its tenant read access, or the path is
  substitution-only and signature-visible
  (#rref("store.substitute.tenant-sig-visibility")). A path MUST NOT be
  reported valid to a caller whose castore reads of it would fail.
]

Valid-but-unreadable is the failure mode this rule forbids. A `.drv` exemption
("build inputs, not tenant-owned outputs") once made a `.drv` uploaded by
tenant A count as valid for tenant B: B's nix client skipped the upload, B's
builder then opened the `.drv` through castore-FUSE, and the tenant-scoped
read (#rref("store.castore.tenant-scope")) returned `NotFound` → `EIO` → the
build died after exhausting its infrastructure retries --- reproduced live with
two tenants sharing one busybox `.drv`. Reporting the path missing instead is
self-healing: the client re-uploads, the idempotent-skip arm of
#rref("store.put.tenant-junction") writes the caller's junction row, and the
path becomes both valid and readable for that tenant. The same re-upload flow
covers `.drv`s uploaded under one identity and queried under another (the case
that motivated the exemption): the second identity re-uploads instead of
receiving a stale "valid" answer it cannot use.

== Key Rotation

+ Generate a new ed25519 signing key with a NEW key name (e.g., `rio-prod-2` if
  the prior was `rio-prod-1`)
+ Add the new public key to all clients' `trusted-public-keys` configuration
+ New paths are signed with the new key immediately
+ Prior cluster public keys stay in the trusted set via `cluster_key_history`;
  no re-sign needed while the history row exists (see
  #rref("store.key.rotation-cluster-history"))
+ After a grace period (default: 30 days), remove the old key from
  `trusted-public-keys` and delete its `cluster_key_history` row

#r("store.key.rotation-cluster-history")[
  The cluster signing key MAY be rotated. Prior cluster public keys MUST remain
  in the trusted set for `sig_visibility_gate` verification until the grace
  period expires --- otherwise paths signed under the old key become invisible
  to cross-tenant reads when `path_tenants` row count hits zero (CASCADE on
  tenant deletion). Prior keys are loaded from `cluster_key_history` alongside
  the active `Signer`. Load-time validation also warns if a prior key's name
  collides with the current cluster key name; verification tries all
  matching-name keys regardless, so a name collision does not break visibility,
  but the warning surfaces a runbook violation.
]

#memo(title: [Future work])[
  Cluster key rotation history and per-tenant signing keys are intended to be
  manageable via a future `keys` subcommand (validating
  `name:base64(32-byte-ed25519-pubkey)` format before INSERT; retirement sets
  `retired_at` to preserve the audit trail). Until that lands, manual `psql` is
  the workflow --- load-time checks (#rref("store.key.rotation-cluster-history"))
  catch malformed rows regardless.
]

== Realisations

#r("store.realisation.register+2")[
  `RegisterRealisation` inserts a CA derivation realisation row `(drv_hash,
  output_name) → (output_path, output_hash, signatures)` into the
  `realisations` table. `drv_hash` is the @modular-hash
  (`hashDerivationModulo`) --- it depends only on the derivation's fixed
  attributes, NOT on output paths, so two CA derivations with identical inputs
  hash the same. Service-caller-only (`PERMISSION_DENIED` otherwise); the
  gateway rejects `wopRegisterDrvOutput` (rio has no trusted-user concept ---
  realisations are scheduler-written at build completion via direct-PG
  `insert_realisation_batch`). The insert is `ON CONFLICT (drv_hash,
  output_name) DO NOTHING` for identical re-inserts; a conflicting
  `output_path` for an existing key returns `ALREADY_EXISTS` and WARN-logs both
  paths. `drv_hash`/`output_hash` MUST be 32 bytes; the gRPC layer validates
  and converts to `[u8; 32]` at the trust boundary.
]

#r("store.realisation.query")[
  `QueryRealisation` returns the realisation row for `(drv_hash, output_name)`,
  or `NOT_FOUND` if no row exists. `NOT_FOUND` means cache miss, not error ---
  the gateway maps it to an empty-set wire response for `wopQueryRealisation`.
  DB-egress validation re-checks hash lengths so a row written directly via
  psql can't poison the response.
]

#r("store.realisation.gc-sweep")[
  GC sweep MUST `DELETE FROM realisations WHERE output_path = $swept_path` in
  the same transaction as the `narinfo` DELETE. The `realisations` table has NO
  foreign key to `narinfo` (migration 002) so CASCADE does not cover it;
  without explicit cleanup, stale rows would point to swept paths and
  `QueryRealisation` would claim a CA cache hit for an output no longer in the
  store. The `realisations_output_idx` index makes the per-path DELETE fast.
]

CA `Realisation` objects carry their own ed25519 signatures over the tuple
`(drv_hash, output_name, output_path, nar_hash)`. This provides integrity for
content-addressed output mappings independently of narinfo signatures.

= Castore RPC Surface (ADR-022)

snix-compatible Directory/Blob surface backed by `directories`/`file_blobs`
(populated by `metadata::set_nar_index_in_conn` inside the manifest-complete
transaction); serves the castore-FUSE builder.

#r("store.castore.blob-stat")[
  `StatBlob(file_digest, send_chunks=true)` returns the `ChunkMeta[]` (digest,
  size) list spanning that file's bytes, resolved server-side via `file_blobs`
  → manifest chunk-cumsum. snix `BlobService.Stat` wire-compatible. The
  builder's castore-FUSE `open()` calls this for files above the streaming
  threshold.
]

#r("store.castore.tenant-scope+3")[
  `GetDirectory`/`HasDirectories`/`HasBlobs`/`ReadBlob`/`StatBlob` MUST be
  tenant-scoped: queries resolve a digest to its containing store path(s)
  (`directory_paths` / `file_blobs.store_path_hash`) and join `path_tenants`
  on the caller's `tenant_id` (from JWT `Claims.sub` or HMAC
  `AssignmentClaims.tenant`, #rref("common.hmac.claims")). When no junction
  row grants the caller access, a digest MAY still resolve through a
  substitution-only containing path (zero `path_tenants` rows) whose narinfo
  signature is visible to the caller
  (#rref("store.substitute.tenant-sig-visibility")) --- the SAME per-caller
  predicate the validity surface applies
  (#rref("store.tenant.valid-paths-filter")), so a path reported valid is
  always castore-readable. The fallback MUST NOT apply to paths any tenant
  has built (a `path_tenants` row for another tenant keeps the path hidden
  regardless of signatures). Return NotFound for digests the caller's tenant
  cannot reach via any owned path or sig-visible substitution-only path.
  Directory bodies
  leak child names/digests --- cross-tenant exposure here is a confidentiality
  issue. Chunk *retrieval* (`GetChunk`/`GetChunks`) is identity-gated but
  *not* tenant-scoped: a 32-byte BLAKE3 chunk digest is unguessable and is
  only disclosed via tenant-scoped `StatBlob`/`ReadBlob` or by self-computing
  it from bytes the caller already holds --- knowing the digest is the read
  capability. Chunk *presence* (`HasChunks`) IS tenant-scoped
  (#rref("store.chunk.has-chunks-tenant")), as are drv-blob presence and
  reads (#rref("store.drv.blob-kind")) --- ADR-024 P2 reconciled presence as
  tenant-scoped across all object kinds.
]

Without the substitution-only fallback the two surfaces disagreed:
substituted paths deliberately carry zero `path_tenants` rows, the
sig-visibility gate reported them VALID to tenants whose trusted keys cover
them, and the strict junction join then failed every castore read of the same
path --- valid-but-unreadable, so schedulers never re-registered the path and
castore mounts of substituted inputs failed forever.

#r("store.castore.gc")[
  `directories` rows are refcounted (one increment per referencing manifest).
  `file_blobs` and `directory_paths` are `(digest, store_path_hash)` junctions
  with `ON DELETE CASCADE` from `manifests` --- GC of one referrer
  cascade-deletes its rows, surviving referrers' rows remain, so
  `ReadBlob`/`StatBlob` never resolve to a dead manifest.
]

= Drv Blob Storage (ADR-024)

Derivation content is a first-class castore blob kind: the canonical
`rio.drv.v1.Derivation` proto bytes, keyed by `blake3(canonical bytes)` ---
the negotiation digest of the ADR-024 build plan. Bodies live in PG
(`drv_blobs.body`) like `directories.body`: drv blobs are a few KB, the same
size class as Directory bodies, far below the chunked-CAS minimum.

#r("store.drv.blob-kind")[
  `DrvBlobService` stores canonical drv bytes keyed by `blake3(received
  bytes)` and serves them back byte-identically (`GetDrvBlob`). Presence
  (`HasDrvs`, bitmap semantics identical to `HasBlobs`) and reads are
  tenant-scoped via the `drv_blob_tenants` junction, resolved through the
  same JWT/HMAC tenant ladder as the rest of the castore surface; storage is
  digest-keyed global. Puts are write-through idempotent: an existing digest
  is an idempotent overwrite-equivalent (and refreshes the GC grace clock),
  never a present/absent timing oracle, and `created[i]` reports only the
  calling tenant's visibility binding.
]

#r("store.drv.verify-on-put")[
  Every `PutDrvBlobs` blob MUST pass the full server-side cross-check
  (`verify_drv_blob`) before anything is written: blake3 over received bytes
  equals the claimed digest; proto decode; structural validation (sorted,
  unique repeated fields --- hostile input is rejected, never re-sorted);
  canonical re-encode byte-compares equal to the received bytes; ATerm
  reconstruction; drv_path recompute equals the claimed path; fixed-output
  output-path recompute. Any failure rejects the whole batch with
  `INVALID_ARGUMENT` naming the failing blob --- non-canonical bytes are
  NEVER stored, which is what makes served bytes equal received bytes equal
  canonical bytes by construction.
]

#r("store.drv.gc-build-pinned")[
  Drv blobs referenced by live builds MUST survive GC. The drv sweep deletes
  a blob only when it is past the GC grace window AND its
  `drv_path_hash = sha256(drv_path)` matches no `scheduler_live_pins` row AND
  its `drv_path` is not in the run's `extra_roots` --- the same pin mechanism
  and keying the path mark phase seeds from, not a parallel one. The
  scheduler pins the `.drv` paths of a submission's live closure through the
  existing `pin_live_inputs` call and unpins on terminal status.
]

The drv sweep runs inside `TriggerGC` after the path sweep, under the same
advisory lock, honoring `dry_run`. Bodies are PG-only, so the DELETE is the
whole sweep --- no `pending_s3_deletes` leg. The client-side ack-record TTL
from ADR-024 ("TTL ≤ the cluster's minimum unpinned-blob lifetime") binds to
this grace window; a re-put refreshes `created_at`, restarting the clock.

= Upstream Cache Substitution

#r("store.substitute.upstream")[
  rio-store MAY be configured with per-tenant upstream binary caches
  (`tenant_upstreams` table). On `QueryPathInfo`/`GetPath` miss, the store
  queries each upstream in priority order (`ORDER BY priority ASC`), fetches
  the narinfo, verifies at least one `Sig:` line against the tenant's
  `trusted_keys`, and if valid ingests the NAR via the same chunked-CAS path as
  `PutPath`. Substitution is synchronous (block-and-fetch): the originating RPC
  waits for ingest to complete.
]

#r("store.substitute.progress-stream")[
  `SubstitutePath` is the server-streaming variant of `QueryPathInfo`'s on-miss
  substitute fallback: same semantics on success/miss/error, but emits
  `SubstitutePathProgress{bytes_done, bytes_expected, upstream_uri}` per \~1
  MiB of decompressed NAR during the download (plus a final tick at completion)
  before the terminal `PathInfo`. The scheduler's `walk_substitute_closure`
  calls this instead of unary `QueryPathInfo` and aggregates per-path progress
  into per-derivation `Event::SubstituteProgress` (display-only, routed via the
  log broadcast ring, not persisted) → gateway `actCopyPath` + `resProgress`
  (#rref("gw.activity.subst-progress")). Unlike `QueryPathInfo`,
  `SubstitutePath` skips the local-PG check (a local hit means the closure
  walk's batch fast-path already returned it) and bypasses moka singleflight on
  miss (`claim_placeholder` still serializes the actual write; the loser sees
  `Concurrent` before downloading).
]

#r("store.substitute.sig-mode")[
  Per-upstream `sig_mode` controls post-substitution signature storage: `keep`
  stores the upstream's `Sig:` lines unchanged; `add` stores upstream sigs plus
  a fresh signature from the tenant's active key (or cluster key); `replace`
  discards upstream sigs and stores only the rio-generated signature.
]

#r("store.substitute.tenant-sig-visibility+2")[
  A substituted path is cross-tenant visible only by signature: tenant B's read
  RPCs (`QueryPathInfo`, `GetPath`, `QueryPathFromHashPart`,
  `FindMissingPaths`) for a path substituted by tenant A return the path IFF at
  least one of the stored `narinfo.signatures` verifies against tenant B's
  trusted set: B's upstream `trusted_keys` arrays ∪ the cluster key (current +
  #rref("store.key.rotation-cluster-history")) ∪ B's own `tenant_keys`
  pubkeys. This prevents tenant A from poisoning tenant B's store by
  substituting from a cache B doesn't trust. The builder-internal batch RPCs
  reject end-user tenant tokens instead (see #rref("store.api.batch-query")).
]

#r("store.substitute.find-missing-gated")[
  `FindMissingPaths` MUST apply #rref("store.substitute.tenant-sig-visibility")
  to the locally-present subset and report gate-failed paths as missing.
  Without this, the scheduler's `check_cached_outputs` →
  `upsert_path_tenants_for_batch` would launder a substitution-only path into
  "built" (≥1 `path_tenants` row), permanently defeating the gate for every
  tenant. The batch gate uses ≤3 PostgreSQL round-trips regardless of batch
  size (one `path_tenants` GROUP BY, one `narinfo` ANY-fetch, trusted-set
  construction).
]

#r("store.visibility.one-body")[
  Every tenant-visibility decision — the single-path read gates and the
  `FindMissingPaths` batch gate — MUST evaluate through the one
  `crate::visibility` body: one `(owned, any_built)` projection, one `.drv`
  exemption test, one signature cell, and ONE malformed-row disposition (a
  narinfo row that fails DB-egress validation surfaces as an error on every
  entrypoint, never as "hidden"). A second open-coded evaluation of the
  visibility policy outside that body is a defect even when its verdicts
  currently agree.
]

The disposition clause is the load-bearing half: a corrupt row that is
silently hidden answers "missing" on the batch RPC while the single-path RPC
answers Internal for the same row — corruption laundered into
re-substitution churn instead of an operator signal.

#r("store.substitute.unverifiable-token-rejects")[
  A request that carries `x-rio-probe-tenant-id` while its
  `x-rio-service-token` is absent, unverifiable (bad HMAC, expired, verifier
  unset), or from a non-allowlisted caller MUST be rejected
  `UNAUTHENTICATED` — never silently downgraded to an anonymous probe. The
  store MUST echo `FindMissingPathsResponse.probe_ran_tenant_scoped` (true
  iff a verified tenant scope was resolved AND a substituter is configured),
  and the scheduler MUST derive its confirmed-missing authority from that
  echo, never from having attached a probe header.
]

The silent downgrade was the wire-level capability-fault laundering
(bughunt-3 merged_bug_003, owner-signed Q3): an anonymous probe answers
missing-with-empty-substitutable, indistinguishable from "probed every
upstream, all 404'd" — routine service-HMAC rotation skew folded to
`ConfirmedMissing`, fail-fasting builds whose outputs sat in the upstream
cache, and the anonymous pass-through re-opened the cross-tenant
sig-visibility laundering the per-tenant probe partition exists to prevent.
Identity honored, never identity attached.

#r("store.substitute.probe-bounded+4")[
  `check_available` (the HEAD-only probe feeding
  `FindMissingPathsResponse.substitutable_paths`) MUST bound its upstream load.
  Per-path probe results (positive and negative --- HTTP 404, 403, or 410; S3
  returns 403 for missing keys when `s3:ListBucket` is not granted) are cached
  for 1h (cap 100k entries) so overlapping `FindMissingPaths` for the same
  closure don't re-probe; probe results for paths where no upstream returned
  2xx and at least one returned a non-404/403/410 error are NOT cached (a
  transient 503 must not pin the path to "miss" for 1h). For uncached paths,
  concurrency is gated on each upstream's `/nix-cache-info` (also cached, 1h
  TTL): 128 concurrent HEADs if every upstream advertises `WantMassQuery: 1`,
  else 8. There is NO batch-size truncation: every uncached path is probed in
  one call (the originating RPC's wall-clock is `⌈N_uncached/128⌉ × RTT`; the
  scheduler's merge-time caller carries `MERGE_FMP_TIMEOUT` per
  #rref("sched.substitute.eager-probe")). Upstream rate-limiting is the actual
  feedback signal --- see #rref("store.substitute.probe-429-retry"). The
  gRPC-level `DEFAULT_MAX_BATCH_PATHS` (1 048 576) remains the DoS guard.
]

#r("store.substitute.probe-429-retry+3")[
  The substituter MUST honor HTTP 429 + `Retry-After` (delta-seconds OR RFC
  9110 HTTP-date) on both the narinfo HEAD and GET. A 429 is NOT cached. *HEAD
  path* (`check_available`): after each pass over the uncached set, the
  rate-limited subset is re-queued for a retry pass after sleeping
  `max(Retry-After).unwrap_or(1s)` (one sleep per pass --- Fastly's 429 is
  edge-wide, not per-object). The retry pass is taken ONLY if the wait fits the
  caller's remaining `deadline` budget (with 2s headroom for the HEADs);
  otherwise the rate-limited paths are returned as not-substitutable for this
  call (uncached) and re-probed at dispatch time. At most
  `SUBSTITUTE_PROBE_429_MAX_PASSES` (3) retry passes. When more than
  `SUBSTITUTE_PROBE_429_ADAPT_THRESHOLD` (10%) of a pass's batch comes back
  rate-limited, concurrency for the next pass is halved (floor
  `SUBSTITUTE_PROBE_CONCURRENCY_CONSERVATIVE` = 8). *GET path* (`try_upstream`
  / `fetch_nar`): a 429 on either the narinfo or NAR-body GET returns
  `SubstituteError::RateLimited{retry_after}` (`retry_after = None` when the
  header is absent --- still a rate-limit, NOT a miss); `do_substitute` records
  it and CONTINUES to the next upstream (matching the HEAD-path semantics --- a
  tenant with `[rate-limited-A, healthy-B]` should hit B). Only if no upstream
  had the path does `RateLimited` propagate (no inline sleep) so the
  #rref("store.substitute.admission") permit is released before any wait; the
  gRPC caller (gateway #rref("gw.store.transient-retry")) owns the retry, and
  the in-process materialization executor re-arms through its job budget
  (#rref("store.materialize.executor")).
]

#r("store.substitute.admission+2")[
  Each rio-store replica MUST gate `try_substitute_on_miss` behind a
  per-replica admission semaphore (`substitute_admission_permits`; default
  `(pg_max_connections × 3).clamp(64, 128)`). Requests beyond cap queue
  server-side up to `SUBSTITUTE_ADMISSION_WAIT` (25 s --- kept below the 30 s
  `DEFAULT_GRPC_TIMEOUT` so callers observe `RESOURCE_EXHAUSTED`, not a
  client-side `DeadlineExceeded`); on timeout the store returns
  `RESOURCE_EXHAUSTED` (transient --- the gateway caller retries per
  #rref("gw.store.transient-retry"); the in-process materialization
  executor's fetch re-arms through its job budget,
  #rref("store.materialize.executor")).
  Additive to #rref("store.put.nar-bytes-budget"): admission bounds request
  COUNT, byte-budget bounds buffered BYTES. The permit is acquired by the
  singleflight leader only, inside the moka init future (per
  #rref("store.substitute.singleflight")); coalesced same-path waiters do NOT
  consume permits, so a wide fan-out on one cold path cannot pin the cap.
]

#r("store.substitute.singleflight+4")[
  `try_substitute` is wrapped in a moka `Cache<(tenant_id, store_path),
  Option<Arc<ValidatedPathInfo>>>` with 30s TTL and 10 000-entry cap. moka's
  `try_get_with` coalesces N concurrent callers for the same key into one
  `do_substitute` call. This is a singleflight coalescer, not a PathInfo cache
  (the narinfo table IS the cache) --- 30s is long enough to coalesce a burst
  of `GetPath`s for the same path from N workers, short enough that a
  substitution-miss doesn't stay stale. `try_get_with` so transient errors
  propagate to every coalesced waiter without being cached; `Ok(None)` (every
  upstream returned 404/403/410) IS cached. A `Concurrent` placeholder claim
  returns `Err(Raced)` so it is NOT cached and the caller's retry reaches
  `AlreadyComplete`. The narinfo/`nix-cache-info`/HEAD requests have a 30s
  per-request timeout so a hung upstream can't wedge the singleflight slot
  forever; the NAR GET is bounded only by the `MAX_NAR_SIZE` decompressed cap
  and the stale-reclaim threshold (#rref("store.substitute.stale-reclaim")).
]

#r("store.substitute.raced-subscribe")[
  On the materialization-executor plane, a `Raced` substitution answer MUST
  park on a placeholder-event subscription instead of poll-racing the claim:
  every transition that frees an `'uploading'` slot (release-in-place, the
  reap/abort delete chokepoint, the `'complete'` flip) announces the
  `store_path_hash` on one PG NOTIFY channel, transactional senders notifying
  inside their transaction so a woken waiter's re-check observes the new row
  state. The park MUST hold nothing --- no admission permit, no NAR-budget
  reservation, no claim, and no singleflight slot a coalesced caller waits on
  (each re-attempt runs the ordinary singleflight; the unary
  `QueryPathInfo`/`GetPath` plane keeps its immediate-`Raced` mapping). The
  waiter MUST register its subscription BEFORE re-checking the row state (the
  lost-wakeup kill), MUST re-poll on a bounded fallback interval (NOTIFY is
  best-effort; every fallback wake re-runs the full claim, whose takeover arms
  reclaim a silently wedged holder), and MUST return `Raced` to the caller's
  retry plane once a typed park budget --- derived from the stall window plus
  fallback slack --- expires.
]
Rationale: the live_055 capture measured 633 re-claim round-trips at \~2/s
against one blocked placeholder (94% of raced losses concentrated on two
chain-head paths), with post-reclaim completion under 200ms --- the polling
plane was pure waste between two wakeups' worth of information. The
subscription collapses it to \~2 round-trips (the raced attempt and the
post-release re-attempt) while the fallback poll preserves the reclaim
plane's takeover latency to within one interval.

#r("store.substitute.untrusted-upstream+3")[
  `tenant_upstreams` rows (URL and `trusted_keys`) are tenant-supplied via
  `AddUpstream`, and the substituter is process-global, so one tenant's hostile
  upstream MUST NOT be able to OOM or stall rio-store for all tenants. Every
  upstream-supplied body is size-capped: narinfo bodies at `MAX_NARINFO_BYTES`
  (1 MiB --- sized for `MAX_REFERENCES` basenames), `/nix-cache-info` bodies at
  `MAX_CACHE_INFO_BYTES` (4 KiB), and decompressed NARs at `MAX_NAR_SIZE` (4
  GiB) --- the decompressed cap is applied AFTER the decoder so a zstd bomb is
  bounded regardless of what `NarSize` claimed. The actual decompressed length
  MUST equal the narinfo's declared `NarSize` (rejected as integrity failure on
  mismatch); signatures are computed only after this check so stored
  `(nar_size, signatures)` are always mutually consistent. narinfo
  `References:` count MUST NOT exceed `MAX_REFERENCES`. The
  narinfo/`nix-cache-info`/HEAD requests carry a 30s per-request timeout. The
  NAR SHA-256 digest runs on `spawn_blocking` so a multi-GB hash does not stall
  a tokio worker.
]

#r("store.substitute.content-binding")[
  A substitution Hit over an ALREADY-STORED row MUST content-bind the
  upstream's narinfo claim to the stored row before the row is returned as
  that upstream's Hit or any of the upstream's signatures are appended: the
  claimed `NarHash`, `NarSize`, and reference set MUST equal the stored row's.
  On disagreement the upstream answers Miss for this path — the narinfo
  verified only against tenant-supplied `trusted_keys`, which is not a trust
  boundary, and the `AlreadyComplete` arm runs before any body fetch, so a
  path-name-only claim must never yield stored bytes.
]

Without the binding, a tenant whose upstream self-signs a fabricated narinfo
naming a victim path gets cross-tenant content disclosure (the stored row
returns as a Hit and flows into `substituted_for_tenant` serving and the
walk's pin/stamp lane) plus persisted upstream signatures whose fingerprint
cannot match the stored row — violating the
#rref("store.substitute.untrusted-upstream") `(nar_size, signatures)`
consistency invariant.

#r("store.substitute.compression")[
  `fetch_nar` MUST decode every `Compression:` value reference Nix's
  `libutil/compression.cc` accepts: `none`/empty, `xz`, `zstd`, `bzip2`, `br`,
  `gzip`. cache.nixos.org never recompresses; pre-2016 paths still serve
  `bzip2`. An unrecognised value is treated as a per-upstream fetch failure
  (`Ok(None)` after exhausting upstreams), not a hard error, so a single
  oddly-configured tenant upstream cannot fail the originating RPC. The
  `MAX_NAR_SIZE` decompressed-side cap from
  #rref("store.substitute.untrusted-upstream") applies uniformly across all
  decoders.
]

#r("store.substitute.identity-check")[
  The parsed narinfo's `StorePath:` MUST equal the requested store path;
  mismatch is rejected before signature verification. Signature verification
  proves the upstream signed _that_ narinfo, not that it answers
  `{hash_part}.narinfo` --- a valid-signed narinfo for path A served at
  `B.narinfo` would otherwise ingest A and return it from `QueryPathInfo(B)`.
]

#r("store.substitute.progress-heartbeat")[
  Substitution-claimed `'uploading'` placeholders MUST carry download-progress
  evidence: the placeholder guard's 30 s heartbeat
  (`ingest::spawn_placeholder_guard` → `cas::heartbeat_uploading_with_progress`)
  writes the owner's decompressed-byte count to `manifests.fetched_bytes` and
  advances `manifests.last_progress_at` only when the count changed since the
  previous heartbeat --- one claim-guarded UPDATE per tick. The byte count is
  advanced by `fetch_nar`'s read loop, so the evidence is download-phase only:
  it stops at `nar_size` when the download completes and the persist phase
  rides the same heartbeat as plain liveness. `PutPath`/`PutPathBatch` claims
  pass no progress handle --- their `fetched_bytes` stays NULL, the structural
  exemption from every stall rule keyed on progress evidence. The
  #(refs.metric)("rio_store_placeholders_uploading") gauge tracks live owned
  placeholders per replica (+1 at guard spawn, −1 at guard drop).
]
The progress columns make *stuck ≠ slow* decidable by competing claimants and
by the owner itself: a slow owner advances `last_progress_at` every heartbeat;
a wedged one keeps `updated_at` (liveness) fresh while the progress clock
freezes (#rref("store.substitute.stale-reclaim")).

#r("store.substitute.stall-abort+2")[
  A substitution download with no NAR body bytes for `RIO_SUBSTITUTE_STALL_SECS`
  (config `substitute_stall_secs`, default 180 s) MUST be aborted by its own
  owner (`fetch_nar`'s per-read watchdog; the NAR GET carries no request-level
  timeout, so this is the only clock on the body). The abort releases the claim
  **in place** --- `claim_id`/`claimed_by` cleared, progress evidence NULLed,
  durable `stall_count` incremented --- so the row survives with its stall
  evidence and the next attempt re-claims it immediately
  (#rref("store.substitute.stale-reclaim")). The release is claim-guarded on
  the aborting owner's `claim_id`: racing a competing stall-reclaim of the same
  stall event, whichever lands first wins and `stall_count` increments exactly
  once. The abort ends only THAT upstream's fetch: the upstream loop MUST
  fail over to the remaining upstreams (mirroring the 429 failover --- the
  strike is already durably recorded, so trying the next upstream loses no
  evidence), and a later upstream serving the path turns the attempt into a
  hit (the released row is immediately re-claimable in the same iteration).
  Only when NO upstream serves does the recorded stall surface to every
  coalesced singleflight waiter as `Stalled` (never cached, never folded into
  a miss, and dominating a concurrently observed 429: charging evidence
  outranks back-off advice); the in-process materialization executor
  classifies it as retryable infrastructure trouble. Counted as
  #(refs.metric)("rio_store_substitute_stale_reclaimed_total")`{reason="stall_abort"}`.
]
The owner-side abort is what makes stall recovery reach the singleflight
leader: every same-replica caller coalesces behind the owner, so no competing
`claim_placeholder` would ever observe the stall from outside. Waiting on the
local NAR-bytes budget is exempt as durable data --- the owner stamps
`claim_phase = 'budget_parked'` before blocking on the budget (and
`'persisting'` before the persist), and the takeover predicate strikes only
`'downloading'` claims: backpressure is not an upstream stall.

#r("store.substitute.loop-evidence-total")[
  Every non-hit arm of the per-upstream substitution loop MUST record its
  failure as fold evidence keyed on the total failure-class alphabet --- no
  loop arm may continue past an upstream failure without recording it --- and
  the post-loop verdict MUST be a pure fold over the recorded cells with
  precedence stall > rate-limit > errored > clean-miss. The cacheable
  definitive-miss verdict (`Ok(None)`, which the in-process cache stores for
  its TTL) MUST be reachable only when EVERY consulted upstream answered
  hit-or-404; an iteration in which any upstream errored (connect/TLS/5xx,
  served garbage, integrity or ingest failure) MUST surface an uncached
  retryable error instead.
]
The pre-fix loop's catch-all error arm recorded nothing, so an all-errored
iteration folded to the same cacheable clean miss as an all-404 one: a 30 s
upstream outage poisoned every (tenant, path) probed during it for the full
cache TTL, silently degrading those paths to build-from-source (the
2026-05-23 incident class). Recording the error axis makes the clean-miss
cache contract real for the first time.

#r("store.substitute.ca-self-auth")[
  An upstream narinfo's `CA:` claim MUST be persisted only if the store path
  recomputed from it (Nix `makeFixedOutputPath` / `makeTextPath` over the
  narinfo's references) equals the path being substituted; otherwise the
  claim is dropped (persisted as absent) and logged at `warn`.
]
The narinfo signature fingerprint covers only `(store_path, nar_hash,
nar_size, references)` --- `Deriver:` and `CA:` are NOT signature-covered, so
a compromised upstream can attach arbitrary claims to correctly-signed
content (the rpm CVE-2021-20271 signature-scope class). A self-consistent
`CA:` is exactly as trustworthy as the (signed) store path, because the path
name commits to the content address by construction --- this is the same
self-authentication check reference Nix performs in
`ValidPathInfo::isContentAddressed`. `Deriver:` is persisted as-is: it is
informational in the Nix trust model (`nix-store -q --deriver`) and nothing
security-relevant consumes it (the PutPathChunked deriver/token binding
applies to builder uploads, not substitution).

#r("store.substitute.stale-reclaim+4")[
  When a claim attempt finds an existing `'uploading'` placeholder for the
  requested path, `claim_placeholder` MUST apply three takeover arms in
  precedence order. (1) A **released-in-place** row (`claim_id` IS NULL ---
  what #rref("store.substitute.stall-abort") leaves behind) is claimable
  immediately by any caller, with no staleness threshold and `stall_count`
  preserved. (2) **Heartbeat death**: a placeholder older than
  `SUBSTITUTE_STALE_THRESHOLD` (90 seconds --- 3× the 30 s placeholder
  heartbeat, so a live owner is never collected; `reap_one` re-checks the
  threshold inside its transaction as the race guard) is reclaimed by
  DELETE + re-INSERT
  --- benign churn (deploys, scale-in, crashes) resets stall evidence and
  never accrues strikes; this arm precedes the stall arm so a dead owner is
  reaped, not striked, when both predicates hold. (3) **Download-stalled**
  (substitution claimants only, which carry the verified narinfo's `NarSize`):
  the takeover predicate is two-clock and PHASE-KEYED over durable data
  (migration 092) --- `claim_phase = 'downloading'` ∧
  `fetched_bytes IS NOT NULL` ∧ `fetched_bytes < nar_size` ∧
  `last_progress_at` older than the stall window ∧ `updated_at` WITHIN the
  stall window (the owner is alive; a dead owner falls to arm 2 and is
  reaped, not striked). A matching claim is taken over **in place** (new
  `claim_id`/`claimed_by`, `claim_phase = 'downloading'`, progress reset,
  `stall_count += 1`) so stall evidence survives ownership changes. Owners
  parked on the local NAR-byte budget (`claim_phase = 'budget_parked'`) and
  persisting owners (`claim_phase = 'persisting'`) are exempt AS DATA ---
  never by a size-equality inference, which held only when the competitor's
  expected `NarSize` equalled the owner's. PutPath claims (`fetched_bytes`
  and `claim_phase` NULL, #rref("store.substitute.progress-heartbeat")) are
  NEVER stall-reclaimable --- they stay under the heartbeat-death rule
  alone. The owner's heartbeat carries the phase in the same claim-guarded
  UPDATE as the progress counter, so phase durability lags at most one
  heartbeat (30 s, with the validated stall floor at ≥ 2× that). A
  placeholder matching no arm indicates a live, advancing (or parked, or
  persisting) uploader and returns a miss. The
  #(refs.metric)("rio_store_substitute_stale_reclaimed_total") counter tracks
  reclaim events, labeled by reason
  (`heartbeat` | `stall_abort` | `stall_reclaim`).
]

#r("store.put.stale-reclaim")[
  `PutPath` and `PutPathBatch` MUST apply the same stale-reclaim as
  #rref("store.substitute.stale-reclaim") when `insert_manifest_uploading`
  reports a pre-existing placeholder: reap it via `gc::orphan::reap_one` with
  the same `SUBSTITUTE_STALE_THRESHOLD`, then retry the insert once. A fetcher
  that died mid-upload (storage eviction, OOM) otherwise blocks the next
  attempt with `Aborted("concurrent PutPath in progress")` until the orphan
  scanner's 15-minute sweep. The
  #(refs.metric)("rio_store_putpath_stale_reclaimed_total") counter tracks
  reclaim events; sustained high alongside
  #(refs.metric)("rio_scheduler_resource_floor_bumps_total")`{reason=cgroup_oom}`
  indicates under-sized fetcher pods (I-207/I-208).
]

#r("store.materialize.live-wanted")[
  The executor's reported coverage --- hits AND misses --- quantifies over
  EXECUTION-END LIVE WANTED, in both directions of the live-interest
  relation: wanted-set growth re-enters the walk (re-seeds), and
  wanted-set SHRINKAGE drops moot misses at the store-side rootage fold
  (the last point with rootage; the wire carries none --- `refs_missing`
  is bare non-emptiness by design). A miss whose every root departed the
  wanted set MUST NOT be wired: `missing_paths` members are in the final
  wanted set; `missing_reference_paths` members are reachable from a
  final want over the walked reference edges (recorded at every
  encounter, so closure diamonds keep all their rootage); the
  trust/content refusal echoes quantify over live misses only.
]
The bug_266 seal covered one axis (tenant) in one direction (growth) ---
the exact R28 multi-axis shape, pre-campaign: a 404'd reference rooted
solely in a departed want compiled into `Unobtainable` and routed a
fully-covered surviving build to ResolveFromSource (or pruned-origin
FailFast) on routine mid-walk interest churn (bug_140). The wire-rootage
alternative is RECORDED REJECTED (path-to-root provenance on the wire is
a protocol expansion no consumer needs once the fold is sound --- the
OQ-9 ruling).

#r("store.materialize.executor+5")[
  Whenever a scheduler address is configured, each store replica
  MUST execute materialization jobs as a pull-protocol client: discover jobs by
  polling the scheduler, claim exactly one open attempt per job through
  PullAssignment carrying the materialization kind and a per-replica executor
  instance identity, perform the reference-closure walk in-process against its
  own substitution machinery (never via per-path RPC to another component),
  pin every ingested or verified path at ingest, and report the outcome through
  ReportOutcome retried until acknowledged. The executor MUST re-resolve the
  job's tenant context against live interest at execution start as the FULL
  set of live interested tenants (the recorded creating-build tenant is
  honored first only while a live interested build carries it; the remaining
  live tenants follow in stable order), and the walk MUST consult every
  resolved tenant's upstream view per path: any tenant's upstream serving a
  path satisfies it (the fetch ingests under the serving tenant; per-tenant
  read visibility stays gated elsewhere), a missing-path verdict requires a
  clean confirmed miss under EVERY resolved tenant, and an indeterminate view
  under any tenant degrades the verdict to InfraFailure --- a job fails only
  when NO interested tenant can obtain. A job whose tenant set resolves empty
  or has no configured upstreams MUST be reported as InfraFailure, never as
  Unobtainable and never silently completed. The walk MUST classify its verdicts through
  total, witness-carrying cells: a missing-path verdict requires a local-
  presence miss (a path with a complete local manifest verifies, is pinned,
  and extends the walk from the LOCAL row's references — upstream absence
  alone is never a miss; a failing local probe is InfraFailure); confirmed
  misses are partitioned by cell — live-wanted seeds into the wanted-missing
  set, narinfo reference extensions into the reference-missing set — and a
  first walk iteration that resolves no verifiable wanted path is
  InfraFailure, never an empty Success; substitute failures MUST be classified
  through a total table with no catch-all arm: a raced placeholder slot and an
  upstream rate-limit are reported RetryLater (transient, to be closed
  uncharged) while download stalls and admission saturation deliberately
  remain InfraFailure (capacity and stall evidence must reach the charge
  ladder). Per-path fetch progress MUST be relayed with the declared NAR size
  leading the streamed bytes and the serving upstream named. Job discovery
  and outcome reporting MUST
  survive scheduler replica replacement: a transport connection whose peer
  answers the not-leader rejection MUST be abandoned and re-dialed rather than
  retried indefinitely, so that polls and reports converge on the serving
  leader after a scheduler Deployment rollout or failover.
]
The store-as-pull-client decision is design §2.2 (adjudication OQ3): the work of
materialization is upstream-fetch-and-ingest, which is already store-internal;
only the control loop moves. The per-replica identity (`executor_instance`) is
the recorded extension to the frozen pull contract (review finding BC-1): unlike
builders --- where a replacement pod of the same intent converges on the same open
attempt --- two store replicas must NOT both execute, so materialization attempts
are keyed per replica and the scheduler's open-attempt arm is the one-winner
arbiter. The tenant rule is review finding AS-4 (the 2026-05-23 incident class:
tenant-less probes degrading to definitive miss). The rollout-survivability
clause is Phase B finding 18 (the flag-transition claim stall): the executor
dials the scheduler ClusterIP Service, kube-proxy pins each TCP connection to
one backend pod, gRPC multiplexes every RPC onto that connection, and only the
leader serves --- so a connection that lands on the standby answers UNAVAILABLE
forever over a healthy h2 session, and without the abandon-and-redial rule the
executor strands every pending job after a scheduler rollout. The gateway and
builders avoid the same hazard with the health-probing balanced channel over
the headless Service; the executor's poll loop is off the hot path, so the
documented ClusterIP-plus-retry posture (templates/scheduler.yaml's Service
comment) is the chosen mechanism here.

#r("store.materialize.probe-polarity")[
  The miss-confirmation HEAD-probe leg MUST classify its per-tenant
  failures through the same substitute-failure truth table as the
  GET/attempt leg --- congruence per CLASS, never per leg. A terminal
  probe rate-limit (429 on every pass, or `Retry-After` past the call
  budget) MUST close the job UNCHARGED with the upstream's advice riding
  the deferral; probe 5xx/timeout/transport failures and the per-call
  deadline cut MUST stay CHARGED (a GET 5xx charges, so a HEAD 5xx must
  too). The availability result MUST carry the rate-limited paths as a
  distinct lane from indeterminate so in-process callers can route the
  class; the FindMissingPaths wire surface merges the lane back into
  `indeterminate_paths` unchanged.
]
The pre-fix probe laundered terminal 429s into `indeterminate`, which the
executor charged as infrastructure: a rate-limit wave on the PROBE leg
burned the park budget that the attempt leg's transient lane was already
shielding (the park-burning harm case, owner decision §5-Q23). The
deadline-cut charge is deliberate: the cut is our own probe budget failing
to classify, not upstream politeness --- treating it as transient would let
a chronically slow upstream defer jobs forever without ever surfacing as
infrastructure evidence.

#r("store.materialize.tenant-fold+2")[
  The executor's per-tenant iteration over a path (both the substitution
  attempt loop and the miss-confirmation probe loop) MUST record evidence
  only through the kernel's tenant-cell chokepoint and MUST obey its loop
  control: a serving hit may break the iteration, a `Raced` verdict aborts
  it (the placeholder slot is path-keyed --- the raced cell is recorded
  first, so the uncharged deferral survives), and every other failure
  disposition MUST leave the iteration running until EVERY resolved tenant
  has been consulted. All failure dispositions MUST exit through the pure
  post-loop fold, with precedence charge > transient > all-clean-miss. No
  tenant-axis loop may return a job-level outcome from inside its body.
]
This is the structural form of the owner-Q2 contract ("a job fails only when
NO interested tenant can obtain"): the tenant resolve order is deterministic
(creating-build hint first), so a pre-fold in-loop return on a charging
failure starved every later tenant of its chance to serve --- a dead first
tenant turned an obtainable path into InfraFailure. Charging evidence still
outranks back-off advice at the fold (matching the per-upstream loop's
ordering one level down), and the transient lane still closes uncharged.

#r("store.materialize.path-fold+1")[
  The walk MUST resolve paths through a single-driver evidence model: one
  driver task owns ALL job state (frontier, visited set, verdict-cell
  registries, the committed progress floor, generation machinery, the abort
  latch), and per-path resolution runs as an evidence-returning future that
  never mutates job state and never returns a job-level outcome (the
  per-tenant axis inside a path future keeps
  #rref("store.materialize.tenant-fold") verbatim). The driver MUST spawn
  path futures in frontier order into a window of at most `path_fanout` in
  flight (visited marked at ENQUEUE, so frontier membership witnesses
  spawnable work — a nonempty frontier MUST imply the next pop spawns, and
  the closure-walk cap is checked at the enqueue sites)
  and apply completions in COMPLETION order: a served path commits the
  floor, records its verified tenants, and extends the frontier; a settled
  path records generation-stamped cells at the driver's current generation;
  the first abort-grade completion latches the walk --- no further spawns,
  the already-completed backlog is folded by the kernel disposition tier
  (charge dominates transient; the transient tier carries the MAX
  `retry_after` across completed transient abort-grades; each tier names
  the lexicographically-first path as its wire representative, never
  first-dequeued), non-abort backlog members are applied normally, and
  in-flight siblings are cancelled by drop. The job outcome MUST be a
  function of the completed-evidence multiset, never of arrival order
  within it. The window MUST drain to empty before any tenant re-resolve,
  so every cell recorded in one iteration carries that iteration's
  generation. Path futures MUST emit progress only as droppable events on
  the driver's completion stream --- the driver is the SOLE caller of the
  progress adapter, so commits and provisional emissions form one total
  order and emission forwarding stops at the job outcome.
]
The serial walk's "determinism" was a BFS-prefix artifact, not a semantic
commitment --- but charge-vs-defer is park-budget-bearing, which is why the
backlog fold (charge dominates within everything actually completed) is
mandatory and first-dequeued-wins is rejected. Window membership at the
latch is schedule-dependent within ≤ `path_fanout − 1` paths: a charge
surfaces at the first attempt in which the charging path completes before
any abort-grade sibling --- under a co-persistent FASTER transient the job
defers uncharged for unboundedly many paced attempts (5--300 s per re-arm,
never fed to the park budget), the same defended direction as the
tenant-fold law one axis down ("a 429 wave must never park a healthy job");
once the transient clears, the charge lands through the ladder. Cancelled
siblings contribute zero cells and zero floor trace; their placeholder
claims are reaped by the drop-guard, their budget reservations ride the
hash bytes (#rref("store.put.nar-bytes-budget")), and coalesced
singleflight waiters of a cancelled leader recover by re-running their own
init futures (moka retry semantics --- a transient raced window during the
guard reap is lawful).

#r("store.materialize.gate-share+1")[
  The executor alone MUST NOT be able to saturate the substitute admission
  gate: at least cap/2 admission permits MUST remain available to RPC miss
  traffic at all times, where cap is the EFFECTIVE admission capacity (the
  `substitute_admission_permits` override included). The bound MUST be
  structural, not arithmetic: every executor path future holds a slot from
  ONE pod-wide fair-FIFO pool of `P = cap/2` permits for its whole
  lifetime, so executor-held admission permits ≤ in-flight path futures ≤
  held slots ≤ P, independent of `path_fanout` and `executor_concurrency`.
  The process gate order is `slot ≺ claim ≺ admission ≺ budget` --- a
  worker MUST hold its first path slot BEFORE the claiming pull (claim
  admission is a non-blocking, leftover-only acquire: a slotless worker
  mints no claim and the job stays scheduler-listed), so slot waiters hold
  nothing, claim holders hold exactly one slot and wait only on bounded
  RPC, admission waiters may hold only slots, budget waiters hold both but
  never wait on either --- keeping the wait graph acyclic and the attempt
  window open only on admitted work. Width-1 baseline invariant: the claim
  carries its first slot into the walk (iteration 1's width-0 spawn
  consumes it without entering the FIFO); thereafter, whenever a walk
  holds ZERO slots with a nonempty frontier, its next slot acquire MUST be
  blocking-FIFO; only spawns that would take the walk to width ≥ 2 may use
  a non-blocking try-acquire. Yield law: the pool MUST be one fair FIFO
  semaphore whose freed slots are assigned to queued baseline waiters
  BEFORE any try-acquire can observe them (widening and claim admission
  are non-preferential) --- a split counter, a second extras-semaphore, or
  any non-queue-respecting fast path violates this rule even while the
  permit ceiling holds. An admission capacity below 2 MUST be rejected at
  config validation (P = 0 wedges every worker at claim admission;
  flooring P at 1 = cap would hand the executor 100% of a cap-1 gate).
]
This is the d1f18610d n=32 envelope ("32 caps the executor at half the
gate") converted from sizing arithmetic in a values comment --- which any
F > 1 at n = 32 breached (n×F = 128 vs cap 64) --- into a structural bound
the config surface cannot invert: the pool absorbs F, n, AND the override,
so the cap formula does not scale with fan-out and the S3 single-prefix
ceiling derivation behind the 128 clamp is untouched. Total executor
concurrency is constant at P; F vs n is adaptive --- at full worker
occupancy each walk holds ~1 slot (job-grain, the backlog burst), at low
occupancy walks widen to F (path-grain, the drain tail) --- GIVEN the yield
law (without it, widened walks re-capture freed slots and the equilibrium
is false). Baseline-slot queuing is MID-WALK-ONLY by construction: the
slot ≺ claim gate means the attempt window opens only on admitted work, so
the interval between claim and first spawn contains zero slot waits and
the establishment sweep's pricing premise --- window time is slot-held
work or crash --- holds again. n×F > P (helm: 128 > 32) shifts queuing to
mid-walk re-acquires, where the walk IS the attempt and the FIFO gives it
the per-path fair share priced by the queued-baseline-waiters/wait-age
facet; a mid-walk wait that pushes a REAL attempt --- one that held a slot
from claim onward --- past the dispatched deadline lands in the
PRE-EXISTING dispatch-deadline-vs-slow-work contract (scheduler machinery
predating fan-out), with monotone cross-attempt progress because committed
paths are pinned and re-serve through the local probe on re-claim ---
nothing width-authored remains on this property. n > P means excess
workers idle at claim admission (boot warn; slotless passes mint no claims
and jobs stay listed for pods with headroom --- strictly better placement
than queueing inside an open window). Executor admission timeouts when RPC
traffic holds the whole gate remain the pre-existing charged posture ---
the pool prevents only the executor-as-cause direction.

#r("store.materialize.progress-monotone+1")[
  Every materialization progress emission MUST route through a
  job-level adapter that owns the COMMITTED progress floor and is the
  ONLY constructor of per-path progress callbacks. The floor is
  mutated ONLY by the success-witness commit of a fully-processed
  path; per-path streaming emissions are provisional --- clamped to
  the committed floor (never below completed work) but structurally
  unable to raise it, so bytes streamed by an attempt that later
  fails leave no trace on job-level state and the final report equals
  the closure's true byte total. `bytes_done <= bytes_expected` MUST
  hold at every call; emitted `bytes_done` MUST never drop below the
  committed floor (display MAY step back from a failed attempt's
  provisional peak --- the truthful reading).
]
The per-fetch byte counter is local to each download attempt, so a stall
failover to the next upstream (or a per-tenant retry) restarts it at
zero; the pre-fix per-path adapter forwarded `base + done` raw and the
documented monotone contract was violated on exactly the traces the
stall-failover machinery exists to produce. The clamp law is a pure
function (`done' = max(high_water, done)`, `expected' = max(expected,
done')`), proptest-swept over arbitrary candidate traces; the adapter
moves the raw callback in, so an unclamped emission site is unwritable.

#r("store.materialize.worker-identity")[
  The executor's per-worker wire identity MUST be carried by a
  validated DNS-1123 label type whose sanitizer budgets the worker
  suffix INSIDE the 63-character bound and whose `with_worker`
  composition is the ONLY way to attach a worker index; the
  scheduler-side validator MUST read the same single-sourced alphabet.
  The composed `{instance}-w{n}` value --- the value the scheduler
  actually validates --- MUST be a DNS-1123 label for every raw
  hostname and worker index.
]
The pre-fix shape validated the BASE identity and then composed past the
bound: any 61--63-char pod name (long Helm release names; the salted
sanitizer arm lands at exactly 63) composed a 64--66-char wire identity,
every claim was rejected `InvalidArgument`, and the poll loop
warn-and-skipped --- a silent, deterministic, fleet-wide materialization
outage keyed on release-name length. Recorded trade: raws of 59--63 valid
chars used to pass through unchanged and now truncate+salt to fit the
budget, so their identity changes once at the deploy boundary; the
scheduler's establishment sweep absorbs the orphaned claims.

#r("store.materialize.local-visibility")[
  The walk's local-presence probe MUST apply the tenant sig-visibility
  verdict --- the SAME decision body as the tenant-facing read gates,
  over the I-217 table `(owned, any_built, sig_trusted)` --- before a
  locally-present row is pinned, counted verified, or used to extend
  the frontier. A row hidden from EVERY interested tenant MUST degrade
  to the per-tenant substitute lane (and, failing that, fold into the
  per-tenant absence verdict); raw physical presence MUST NOT be
  sufficient to serve.
]
Presence is a per-tenant fact (owner Q2). The pre-fix walk treated any
complete local manifest as servable: a substitution-only row signed by keys
none of the interested tenants trust, or another tenant's built output
(I-217 isolation), was counted verified and --- via the consumption path's
`upsert_path_tenants_for_batch` --- laundered into every interested build's
durable per-tenant ownership, after which the read gates served it through
the owned fast-path. The structural form: the kernel's `visibility_verdict`
is the one table, `rio-store/src/visibility.rs` is the one projection body
shared by the gRPC gates and the walk, and the walk's `Present` arm
requires the `TenantVisible` witness that only that body mints --- a
tenant-blind Present does not compile.

#r("store.materialize.honest-beat")[
  A materialization worker MUST NOT issue a listing call on a poll
  pass that cannot convert a served job into a claim: mint-headroom
  exhaustion (the claim budget pinned by unanswered mints, or the
  resume ledger at capacity) MUST withhold the beat, a
  conversion-futility streak (every fresh mint of the pass answered
  with a conversion-disproving rejection, at three consecutive
  passes) MUST withhold the beat for an interval that exceeds the
  scheduler's listing-membership TTL before one probe pass re-lists,
  and the resume presentation lane MUST never be withheld.
]
The scheduler's steal horizon re-homes CLAIM capability but keys on
listing recency --- the only freshness signal it has (RULED CF-3: no
wire change, no lane flag). The beat is therefore the capability
channel: pre-fix, a worker whose budget was pinned by a Charged
orphan, whose ledger sat at cap, or whose every mint was refused with
a conversion-disproving rejection KEPT LISTING --- staying eternally
fresh under the horizon and pinning its rendezvous slice (~1/N of the
claimable head) fleet-wide against the module's own
served-more-broadly degradation law. Withholding makes the wedged
worker exactly the stale owner the horizon already handles: its slice
serves broadly within 5 s and re-homes permanently once the 60 s
membership TTL drops it; the futility re-probe interval (64 passes at
the 1 s beat floor) is pinned `>=` the TTL by a const-relation test,
and the mirrored TTL/horizon constants are parity-pinned against the
scheduler's through the exported store symbols (the dependency runs
scheduler->store, so the store cannot import them). NotYetReady
contest losses are NEVER futility evidence --- they are the healthy
fleet>work steady state the FS-4 speculation bound already prices.
Machine witness: `docs/spec/models/materializationDistribution.qnt`
(the capability axis: `wedgedOwnerSliceRecovered`; the
wedged-keeps-beating falsify twin). Premise reachability in the live
withheld-beat regime is itself machine-checked: the wired
expect-violation witness `canReachWedgedPastHorizonOwner`
(`quint-matdist-wedge-premise-reachable`) goes red the moment the
wedge corridor closes, retiring the model-header
verified-at-a-commit note.

#r("store.materialize.pass-outcome")[
  Every pass-scoped observer MUST consume the single sealed pass
  outcome; pacing MUST be a total function over the outcome alphabet;
  a pass that neither delivered a claim nor removed ledger entries
  MUST NOT re-poll unpaced, and a contested pass MUST honor the
  server's answered retry floor.
]
One poll pass has several pass-scoped observers --- pacing, the
conversion-futility latch, the claim-wedge latch --- and round-8 found
each consuming a different hand-picked PARTIAL projection of the pass,
each with a load-bearing unobserved edge (a resume delivery at
production slots=1 classified EMPTY by the withheld short-circuit; a
zero-action listing classified productive by raw listing
non-emptiness; the server's `retry_after_seconds` discarded at the
wire mapping). The structural form: `poll_and_claim` seals exactly ONE
`PassOutcome` at its single exit chokepoint (`finish!` --- every exit
names a typed `PassExit`; the seal is the sole constructor), and every
observer total-folds the sealed value, so an unobserved transition
cannot compile. The pacing law is the no-spin invariant stated over
the alphabet: immediate re-poll is licensed only by variants that
consumed finite supply (`Delivered` executed a claim; `Settled`
strictly shrank the ledger --- the backlog is finite, so termination
is structural), contested passes pace at the server's answered floor
(cap-fill at `(cap/allowance - 1) x floor`, the FS-4 envelope), and
every other shape (zero-action listing, empty, gated-wedged,
list-failed) paces at the beat. Withhold reasons are typed
(`WedgeKind`), never a boolean that shadows delivery.

#r("store.materialize.remint-cooldown")[
  Every answered claim outcome MUST pace the job's next fresh
  presentation: an answer that leaves a surviving credential
  (NotYetReady) paces through the one-nonce-per-job-lifecycle mint
  gate, and an answer that RESOLVES the ledger entry without a
  delivery (Gone, a mint-disproving rejection, a fresh-lane auth
  rejection) MUST bar fresh re-mints of the same job for a typed
  cooldown window --- so a bounded stuck set of K listed rows absorbs
  at most ceil(K / mint allowance) passes' worth of fresh mints once
  per cooldown window, never the whole pass budget.
]
live_061's starvation mechanics (2026-06-12): the pacing law above
was HALF-built --- NotYetReady's surviving credential made contested
rows self-pacing (the next pass skips them at the mint gate and the
resume lane carries them through the structural queue), but Gone
resolved the entry and left NOTHING behind, so a still-listed
Gone-answering row (a zombie the scheduler kept advertising) was
re-minted every pass. At production slots=1 the per-pass allowance is
2: a 2-row stuck prefix absorbed 100% of every pass's mints, and the
fleet converted ~0.5% of claim attempts for hours. The asymmetry ---
not the refusal volume --- was the defect; the cooldown is its
symmetric completion. Delivered claims never enter the cooldown (a
delivery removes the job from the listing server-side), and a job
re-listed past the cooldown re-mints on the first pass after expiry
(a NEW job row under the same derivation carries a new job_id and is
never barred --- the cooldown keys on job_id).

= Two-Phase Garbage Collection

#r("store.gc.two-phase+2")[
  - *Phase 1 (Mark):* Identify paths unreachable from GC roots via a recursive
    CTE over `narinfo."references"`. GC root seeds: auto-pinned live-build
    inputs in the `scheduler_live_pins` table, manifests with
    `status='uploading'` (in-flight PutPath), paths with `created_at > now() -
    grace_hours` (recent uploads), per-tenant retention windows, and
    `extra_roots` passed from the scheduler's live-build output paths
    (`ActorCommand::GcRoots`). Mark takes NO lock against PutPath (I-192);
    PutPath runs freely throughout. The CTE's MVCC snapshot is a point-in-time
    view --- placeholders that commit after it are caught by sweep's re-check.
  - *Grace period:* Configurable per-invocation via `GcRequest.grace_period_hours`
    (default *#(refs.const)("DEFAULT_GC_GRACE_HOURS")h*). Protects paths uploaded shortly before GC that builds
    haven't referenced yet.
  - *Phase 2 (Sweep):* Per unreachable path, in batched transactions: lock the
    path's manifest row (`FOR UPDATE` --- a concurrent PutPath for the same
    path blocks until the batch commits), re-check references
    (#rref("store.gc.sweep-recheck")), then DELETE narinfo (CASCADE to
    manifests/manifest_data) plus the path's realisations and `path_tenants`
    rows --- all in the same PG transaction. The sweep MUST NOT decrement,
    soft-delete, or enqueue chunks: chunk collection is decoupled from path GC
    and owned by the collect cycle (#rref("store.gc.chunk-collect")), which
    picks up a swept path's now-unreferenced chunks once they age past the
    grace window.
]

#r("store.gc.sweep-referrer-order")[
  The delete loop MUST process paths in referrer-first order within
  `sweep_unreachable`: depth 0 = paths with no in-set referrer, depth N = paths
  whose only in-set referrers are at depth \<N. Any Y is then re-checked at
  index ≤ its dep Z, so a `PutPath(P, refs=[Y])` landing mid-loop
  closure-resurrects Y (and removes Z from the candidate set) BEFORE Z's batch.
  Without this ordering, Z-in-batch-K can commit deleted before Y-in-batch-K+M
  is re-checked, leaving live `Y→deleted Z`. Cycles share a depth and are
  handled by the within-batch `still_unreachable` probe.
]

#r("store.gc.sweep-recheck+2")[
  Sweep MUST, before deleting each candidate path Q, re-check ALL
  concurrent-writable mark seeds against a fresh READ-COMMITTED snapshot: (i)
  `narinfo.references` referrers `∉ sweep_unreachable` (covers PutPath
  placeholders, which carry references from insert per
  #rref("store.put.placeholder-refs")); (ii) `scheduler_live_pins` rows for Q
  (dispatch-time auto-pin); (iii) `path_tenants` rows for Q inside any tenant's
  `gc_retention_hours` window (merge-time tenant attribution; an all-cache-hit
  merge writes ONLY this table). On match, sweep MUST skip Q and increment
  #(refs.metric)("rio_store_gc_path_resurrected_total"). This re-check is the
  sole load-bearing mark-vs-concurrent-writer guard: a writer that commits any
  of (i)/(ii)/(iii) for Q at any point before the re-check resurrects Q
  regardless of mark's snapshot timing. A μs-scale TOCTOU between re-check and
  DELETE remains; the grace period makes it operationally negligible (Q being
  swept means it's >grace old with zero referrers).
]

*Orphan cleanup:* Stale `'uploading'` manifests are reclaimed after a
compile-time threshold (`STALE_THRESHOLD`, 15 minutes); reaping deletes the
abandoned path rows only. Chunks left unreferenced by any cleanup --- a reaped
upload, a swept path, or a historical leak --- are picked up by the collect
cycle (every `run_gc` phase 3 plus the daily backstop,
#rref("store.gc.chunk-collect")), which recomputes liveness from the manifests
each cycle and therefore needs no per-row repair path and no separate
safety-net scan. No full S3 enumeration needed.

#r("store.gc.sweep-path-tenants+1")[
  Sweep MUST delete `path_tenants` rows for each swept `store_path_hash` in the
  same transaction as the `narinfo` DELETE, after copying them into the
  registration tombstones per #rref("store.gc.evidence-outlives-bytes").
  `path_tenants` has no FK CASCADE to `narinfo` (migration 012); without
  explicit cleanup, orphaned rows survive the sweep and grant wrong-tenant
  visibility when a different tenant later re-uploads the same store path (the
  stale row still JOINs in the #rref("store.gc.tenant-retention") CTE arm).
]

#r("store.gc.evidence-outlives-bytes")[
  Sweep MUST copy each swept path's registration records (its `path_tenants`
  rows, with `store_path` and `deriver`) and identity records (its
  `realisations` rows) into the append-only tombstone tables
  (`path_tenant_tombstones`, `realisation_tombstones`, migration 103) inside
  the same transaction that deletes them --- registration and audit evidence
  outlives the bytes; a swept path's records are atomically either live or
  tombstoned, never lost.
]
The live rows still die with the path: their deletion is what defends the
wrong-tenant-revival leak (#rref("store.gc.sweep-path-tenants+1")) and keeps
every live reader's semantics untouched. The tombstones are history, consulted
by operators and backfills, never by visibility or retention.

#r("store.gc.hold+2")[
  The store MUST honor a typed, persisted GC hold (the `gc_holds` table,
  migration 103): an active global-scope hold suspends EVERY destructive
  lane (#rref("store.gc.hold-lanes") --- the census-derived lane set, not
  just the collection pass): `run_gc` is a no-op before mark whose HELD
  tick still stamps `last_live_cycle_at` (a held cycle is a live cycle for
  staleness purposes, so the hold itself can never make the backstop come
  due or trip the stalled alert), and the periodic delete lanes skip their
  ticks. An active tenant-scope hold makes the held tenant's registered
  paths reachable in the mark seed and the sweep re-check regardless of
  their retention window. A hold is active while it is unreleased and
  unexpired; `expires_at` NULL is an unbounded hold --- an explicit
  operator decision recorded in the row. An unreadable hold table MUST
  fail every consult closed.
]
The hold replaces the freeze-by-scale-to-0 workaround (the signed Q3 record).
Interim operator surface: the typed in-crate API (`gc::hold`) plus any PG
session (`INSERT INTO gc_holds (scope, reason, created_by) VALUES ('global',
..., ...)`; release via `UPDATE gc_holds SET released_at = now() WHERE hold_id
= ...`); a wire admin verb rides the next proto-granted slot --- this wave's
proto partition grants the store none (recorded divergence, never silent).

#r("store.gc.hold-lanes+2")[
  During an active global hold, NO lane with delete authority may execute a
  destructive act: every destructive lane MUST consult the active-hold
  predicate FAIL-CLOSED at each tick before any destructive work (a consult
  error is a skip, never a bypass), and BOTH population layers MUST be
  machine-derived, never author-enumerated --- the LANE census over the
  spawn-periodic family (`spawn_periodic` + `spawn_periodic_with` call
  sites) intersected with the reaches-delete-sink predicate, union `run_gc`
  pinned; and the BODY census over destructive-loop idioms (every
  production loop transitively reaching a durable gc-victim sink, with its
  per-batch demand form derived --- `gensets/destructive-body-census.txt`).
  Periodic registration is the `DestructiveLane` wrapper (the only way to
  register a deleting periodic lane); the per-tick `HoldClearance`
  capability is demanded by the named delete sinks, and demand-driven
  delete callers consult at call time (`reap_one_consulted`). Every
  multi-batch destructive body MUST re-authorize at each
  committed-transaction batch boundary through the per-batch token
  (#rref("store.gc.batch-authority")), so destructive work after
  hold-activation is bounded by the one batch already in flight --- that
  batch completes-or-aborts within `DESTRUCTIVE_BATCH_DRAIN_BOUND` (one
  drain cadence) and the next batch never starts at any boundary, tick or
  intra-tick; the drain lane HOLDS its queue (`pending_s3_deletes` rows
  age, never execute).
]
Wave-9 shipped the hold consulted at exactly one entry (`run_gc`) while four
sibling lanes kept deleting during the freeze --- and the held `run_gc`
starving `last_live_cycle_at` GUARANTEED the backstop fired (merged_bug_050,
HIGH). The lane census (`gc/lane.rs`, committed at
`rio-store/tests/gensets/destructive-lane-census.txt`) is the load-bearing
spawn-layer totality // quantifier: census(destructive_lane_census)
--- the author-enumerated four-lane list this close first
carried was the round-6 closure-set defect recurring inside a high close ---
the gc-orphan-scanner (the fifth lane) hid from it; a sixth cannot hide from
the family scan. The same defect then recurred one layer down (bug_084,
merged_bug_006): the wave-11 batch-boundary law quantified over "multi-batch
tick bodies" but its enforcement was a hand list that wired four of six
bodies --- run_gc's phase-2 path sweep and the log TTL sweep shipped
unwired, so a global hold could not stop either mid-pass. The BODY census
(`gc/lane.rs`, committed at
`rio-store/tests/gensets/destructive-body-census.txt`) is the body-layer
totality // quantifier: census(destructive_body_census)
--- it derives every destructive loop from the idiom and refuses any
member without the per-batch demand. Enforcement tiers, stated honestly
(R24/R28-as-amended-by-R31): the clearance type compile-seals the named
sinks and the per-batch token compile-seals the batches (reachability); the
expiry + batch re-authorization seal the time axis at the type's own seam;
the two censuses DERIVE the population. The wave-10 form of this rule
promised the drain bound while consulting once per tick --- false for the
backstop's full collect cycle (a five-minute lock-held budget over fifty
committed batches, merged_bug_067); the per-batch re-authorization through
the token is what makes the bound true.

#r("store.gc.clearance-expiry+2")[
  A `HoldClearance` MUST expire `DESTRUCTIVE_BATCH_DRAIN_BOUND` after its
  last successful consult (mint, batch re-authorization, or a declared
  phase-seam consult): an expired clearance MUST refuse batch
  authorization unconditionally --- with no active hold in `gc_holds`,
  where a bare re-consult would authorize --- and within a destructive
  cadence no re-consult resurrects it (the owning tick ends; the next tick
  re-gates at the lane wrapper). The ONLY lawful window restart is the // quantifier: census(aged_clearance_refuses_with_no_hold_present)
  declared phase-seam consult (#rref("store.gc.consult-aged-clearance")):
  a consult opportunity between non-destructive phases, which refuses
  under a hold and yields no batch authority.
]
The expiry is the time axis's own law (R28): re-consult alone leaves a
stalled body riding a tick-start consult for unbounded wall time between
boundaries it happens not to reach; expiry caps the authority itself, so
the freeze guarantee degrades only to the one in-flight batch even under
pathological stalls. Refusal cause is typed end-to-end
(`BatchAuthorize::{Held, Expired}` at the seam, `ClearanceStop` in the
collect report) --- never inferred from logs.

#r("store.gc.batch-authority")[
  Every multi-batch destructive body MUST re-authorize per batch THROUGH
  THE TOKEN: the batch-boundary consult (`HoldClearance::authorize_batch`)
  is the sole mint of a per-batch linear `BatchAuthority` --- non-clonable,
  consumed by value, one batch per token --- and every destructive sink
  (the path-sweep batch, the log-sweep batch, the outbox enqueue, the
  per-row drain delete, the placeholder reap) MUST demand it, so no
  destructive batch is reachable without a boundary authorization. A
  refused boundary (`Held`/`Expired`) structurally yields no token: the
  body stops, committed batches stand, and a global hold landing mid-pass
  binds at the NEXT batch boundary of EVERY body --- never "the next run". // quantifier: census(destructive_body_census)
]
The wave-11 form of this law quantified over "multi-batch tick bodies" but
enforced by hand enumeration: the verdict's `Authorized` arm was a unit
variant --- advisory data a body could match and ignore --- and two of six
bodies shipped unwired (run_gc's phase-2 path sweep took no clearance; the
log TTL sweep discarded its lane clearance as `move |_clearance|`), so a
global hold could not stop thousands of in-flight path-delete batches in
exactly the suspected-bad-mark scenario the hold exists for, where
continued deletes are unrecoverable without re-substitution (bug_084,
merged_bug_006). The token is the R32 repair --- obligations are linear
resources, not advisory data --- and the body population is DERIVED by the
destructive-body census (R31), never author-listed.

#r("store.gc.consult-aged-clearance")[
  Authority windows age from the CONSUMER'S LAST CONSULT OPPORTUNITY, not
  the mint: a destructive consumer whose pre-batch phase exceeds the
  drain-cadence bound MUST re-consult at its declared phase seams (the
  post-mark seam in `run_gc`; the pre-drain seam in `collect_cycle` after
  the read phase) so the staleness clock starts where consumption starts.
  The seam consult (`HoldClearance::regate`) restarts the window on a
  clear consult, MUST refuse under an active global hold, MUST fail
  closed on a consult error, and MUST NOT yield batch authority --- the
  destructive cadence below it still re-authorizes per batch through the
  token, where an aged clearance refuses unconditionally.
]
The drain-cadence bound was frozen from the S3 drain lane's mint-adjacent
tick (merged_bug_067) and never entailed the per-batch law at the
collect/run_gc consumers, whose mint-to-first-use distance is a full
mark+sweep or a multi-minute validation/mark phase at the documented
1.5M-path design point (merged_bug_081): past 30s of pre-batch work every
cycle Expired at batch 1 with zero batches --- permanent zero
chunk-collect progress exactly at scale, with "next tick re-gates"
re-minting into the same structure. The seam is the consult-clock proper
(R29': consult, not mint) --- one bound, correct denominator; a per-lane
bound parameter was considered and REJECTED for this close (no consumer
lacks a seam; widening the global bound would weaken the freeze guarantee
for every lane).

#r("store.gc.sweep-cycle-reclaim")[
  The sweep-phase reference re-check MUST exclude referrers that are themselves
  in the current unreachable batch. Without this exclusion, mutual-reference
  cycles (A→B, B→A) and self-references (A→A) are never swept: the re-check
  sees an intra-batch referrer and skips both paths forever. The exclusion is a
  `NOT EXISTS` anti-join against a temp table populated once at sweep start
  with the full unreachable set --- O(N) wire bytes and an index probe per row,
  vs. O(N²) for a per-path array bind.
]

#r("store.gc.tenant-retention")[
  A store path survives GC if _any_ tenant that has referenced it still has the
  path inside its retention window. This is the 6th UNION arm in the mark phase
  CTE (after roots, uploading-manifests, global grace, extra-roots, and
  scheduler-live-pins): it joins `path_tenants` against
  `tenants.gc_retention_hours` --- `WHERE pt.first_referenced_at > now() -
  make_interval(hours => t.gc_retention_hours)`. Union-of-retention semantics:
  the most generous tenant wins. The global grace period (`narinfo.created_at`
  window) is a floor; tenant retention extends it but never shortens it. An
  empty `path_tenants` table makes this arm a no-op (0 rows contributed).
]

An active tenant-scope GC hold (#rref("store.gc.hold")) widens the same arm:
a held tenant's referenced paths stay reachable past the window until the hold
is released or expires. Reachability only ever widens; the window law above is
unchanged.

#r("store.registration.ingest-stamps")[
  The PutPath ingest lanes MUST stamp the authenticated uploading tenant's
  `path_tenants` registration row at the ingest commit point: PutPathBatch
  inside the same transaction that completes the manifests; single-path
  PutPath immediately after the persist commit, before the response.
  Anonymous uploads (no tenant claims) register no per-tenant ownership. The
  stamp is the registration record the visibility projection and the
  tenant-retention mark arm consume --- the signature fallback is
  defense-in-depth for it, never the only line.
]
Every byte-complete upload the store accepted is registered evidence (the
signed round-9 Q1 invariant). Before this rule, 93.4% of the store sat
unstamped: visibility for fresh uploads rode entirely on the
#(refs.const)("DEFAULT_GC_GRACE_HOURS")h grace plus
signatures, and an uploaded-then-never-built-against path had no registration
record at all.

#r("store.registration.cancel-survives")[
  Cancellation MAY stop future work; it MUST NOT discard registered or
  registrable completed work. A late success-class completion report
  (Built/Substituted/AlreadyValid) for a cancelled or evicted derivation MUST
  classify to a registration effect at the scheduler's late-report chokepoint
  --- stamping `path_tenants` for the historically-interested tenants
  (cold-resolved from the durable build-interest rows), filling the
  (path-to-deriver) identity linkage, and inserting CA realisations where the
  modular hash is resolvable --- never to an acknowledged drop.
]
The measured pre-rule loss: 1,735/4,529 (38.3%) of one run's completed uploads
lost their registration to the cancel-intake no-op arm while their bytes
stayed durable --- tenant-invisible and GC-exposed. The boundary, signed: a
SIGTERM mid-build before upload is genuinely lost work, a different class out
of this rule's scope.

#r("store.gc.tenant-quota")[
  Per-tenant store accounting sums `narinfo.nar_size` over all paths the tenant
  has referenced (`JOIN path_tenants USING (store_path_hash) WHERE tenant_id =
  $1`). This is the accounting query; enforcement is the sibling
  #rref("store.gc.tenant-quota-enforce") below (gateway rejects SubmitBuild
  over quota).
]

#r("store.gc.tenant-quota-enforce")[
  The gateway MUST reject `SubmitBuild` with `STDERR_ERROR` when
  `tenant_store_bytes(tenant_id)` exceeds `tenants.gc_max_store_bytes`.
  Enforcement is eventually-consistent --- `tenant_store_bytes` may be cached
  with ≤30s TTL. The connection stays open; the user can retry after GC.
  (Sibling to #rref("store.gc.tenant-quota") --- distinguishes enforcement
  from accounting.)
]

#r("store.gc.acquire-census-derived")[
  The gc pooled-acquire census's UNIVERSE derives from the module tree's
  one authoritative declaration list (`include_str!("mod.rs")` parsed for
  non-`cfg(test)` FILE-module declarations): the `include_str!` sibling
  array MUST cover every derived member, with exceptions TYPED and
  asserted rather than skipped --- the census home itself discharges via
  its in-file exactly-once rule, `cfg(test)`-gated declarations are
  lawfully outside the production universe, and inline modules discharge
  via their host file's row. A sibling landing without enrollment reds
  the census; a hand-maintained array is a second copy of the module
  tree.
]
Wave-10 added `lane.rs` to the gc tree without extending the
pre-campaign census (bug_002): a future bare `pool.acquire(` there would
have silently bypassed the SessionConn law the test exists to enforce.
No live violation existed --- the gap was census-universe staleness, the
R31 in-crate tier's founding instance; the same `include_str!` is
same-crate, so the cross-crate embed ban does not bite (checked
explicitly).

#r("store.gc.serialize-lock")[
  `run_gc` serializes against itself via `pg_try_advisory_lock(GC_LOCK_ID)` on
  a dedicated session-scoped pool connection. If the lock is held, `run_gc`
  returns `Ok(None)` immediately ("already running") --- two concurrent sweeps
  would not corrupt anything but waste work and produce misleading stats. The
  lock is explicitly released via `pg_advisory_unlock` on every exit path (a
  scopeguard backs the explicit calls). The constant `GC_LOCK_ID =
  0x724F47430001` is arbitrary; it just must not collide with other advisory
  locks in the schema (#rref("store.db.migrate-try-lock") uses a different
  ID).
]

#r("store.gc.dry-run+3")[
  `GcRequest.dry_run=true` runs mark + sweep with full stats computation but
  the per-batch transaction's narinfo DELETEs are
  `ROLLBACK TO SAVEPOINT`ed (and the chunk-collect phase stays in its
  report-only shadow arm); the temp-table
  `closure_remove_from_unreachable` mutations COMMIT (so batch N+1 sees Y's
  resurrection of Z and dry-run stats match what a real run would do). The
  shadow chunk estimate is computed against SIMULATED post-sweep state: the
  sweep's settled swept set is excluded from the shadow mark expansion AND
  its fail-closed validation (`CollectMode::Shadow{simulated_swept}`), so
  the would-be-swept manifests' now-unreferenced chunks count as collectible
  exactly as a real run would leave them, and a corrupt manifest the sweep
  removes cannot abort a dry run it would not abort live. The operator sees
  "would delete N paths, free M bytes" without touching narinfo, chunk rows,
  or `pending_s3_deletes`. The final progress message's `current_path` reads
  `"dry-run: no paths actually deleted"`.
  #(refs.metric)("rio_store_gc_path_swept_total") is NOT incremented on
  dry-run.
]

#r("store.gc.observation-basis+2")[
  The collect cycle's committed observation (the `gc_collect_state`
  singleton's live-count and backlog estimate) MUST anchor the REAL basis:
  what is live and what is eligible under NO exclusions, on the cycle's own
  REPEATABLE READ snapshot. The simulated products of a shadow cycle with a
  non-empty `simulated_swept` exclusion (the dry-run preview lane) are
  reporting-only --- `GcStats` carries them, the durable row never does. In
  code the law is a type: `DurableObservation`'s constructor is private to
  the real-basis computation (`from_real_basis`), and
  `CycleCommit::{Shadow, Live}` accept nothing else --- committing a
  counterfactual mark-set size is unwritable. A shadow cycle with
  exclusions therefore materializes a second, exclusion-FREE mark product
  on the same snapshot (2x cost, operator dry runs only) --- and that
  product gets its OWN fail-closed validation over the exclusion-free
  population first: validation and expansion are paired in one builder
  parameterized identically, so an expansion over an unvalidated
  population does not typecheck, and corruption inside the
  simulated-swept set WITHHOLDS the durable observation (the dry run
  stays preview-only; merged_bug_147). An exclusion-free shadow reuses
  its preview as the real basis.
]

Pre-fix, the shadow commit wrote the preview's counts: every pacing and
alerting consumer of `gc_collect_state` acted on a world where the excluded
upstreams' manifests were already gone --- live-count too small, backlog
too large, and the backstop cadence paced against the counterfactual
(bug_226). The quint model (`gcCollectState.qnt`) carries the pair of basis
invariants; its calibration twin re-wires the commit to the simulated
products and both falsify.

#r("store.gc.collect-cadence+5")[
  The collect cycle's cadence, resume cursor, and gauge sources are CLUSTER
  state, durable in the `gc_collect_state` singleton row (migrations 090,
  100), never process state. The backstop MUST run a live cycle only when
  `now() - last_live_cycle_at` is at least the backstop interval AND
  `now() - last_attempt_at` is at least the same interval, evaluated on the
  database clock against the durable stamps (double-checked under the GC
  advisory lock); the ATTEMPT stamp is written through the lock session
  BEFORE the cycle runs --- backstop and live `run_gc` phase 3 alike, never
  by a dry run --- so no outcome arm (success, fail-closed abort, database
  error) can yield a faster-than-interval retry of the heavy cycle
  (bug_284). `last_live_cycle_at` is written by exactly the census-derived
  writer set (`live-cycle-anchor-writers.txt`): the committed live cycle's
  same-statement stamp and the held `run_gc` tick's stamp (a held cycle is
  a live cycle for staleness purposes; merged_bug_050) --- and commit
  recognition MUST treat the anchor as inadmissible whenever a global hold
  overlapped the attempt-to-probe span, since the held-tick writer can
  forge it (merged_bug_073). The stalled alert keys on the column.
  Per-replica timers are cheap CHECK
  ticks --- N replicas MUST NOT yield more than one live backstop cycle per
  interval. A live cycle commits its stamp, stop cursor, and decremented
  --- or, when no anchor exists, freshly seeded from the cycle's
  unmarked-rows count (bug_306) --- backlog estimate in one update through
  the lock's session; if that session died while idle through the cycle,
  the commit is retried ONCE on a fresh connection guarded by the epoch the
  lease read at acquire, so a stale late commit no-ops instead of
  clobbering a successor's state (merged_bug_218). The commit outcome is
  THREE-VALUED (merged_bug_022): the `CycleCommitted` witness MUST be
  minted at the durability point --- in the expression observing the
  commit statement's success, before any post-commit cleanup, so a failed
  lock release cannot alter attribution --- and a zero-row guarded retry
  MUST be classified on row evidence read on the same fresh connection.
  The own-recognition echo is pure payload PLUS the own-held-attempt
  anchor (merged_bug_021): the attempt stamp statement returns the value
  it wrote and the holder keeps it as an opaque token, the probe compares
  the live stamp against THAT held value DB-side --- never against the
  shared `last_attempt_at` column, which any holder lawfully overwrites
  with no dueness gate. An epoch at expected+1 whose payload echoes the
  intended write with the live stamp at-or-after the held anchor is the
  cycle's OWN landed commit (applied-but-response-lost; `outcome="ok"`);
  PROVEN loss (`outcome="commit_failed"`) requires a POSITIVE pure-payload
  contradiction of the intended write; a temporal-anchor failure with a
  matching payload --- a stale or absent anchor, including a holder whose
  attempt stamp failed and holds none --- is
  `outcome="commit_indeterminate"`, never `commit_failed`, as is anything
  else unprovable (retry or diagnostic error, epoch past +1) --- the
  `outcome="ok"` cycle metric ticks only on a landed commit (the
  witness), and `commit_failed` only on proof of loss. A dry-run (shadow) cycle commits its observation WITHOUT the
  live or attempt stamps. Every replica publishes
  #(refs.metric)("rio_store_gc_collect_backlog_chunks"),
  #(refs.metric)("rio_store_gc_chunks_live"), and
  #(refs.metric)("rio_store_gc_chunks_would_collect") from a periodic read
  of the row (NULL fields leave the pre-registered zero standing): the
  gauges are a replicated cluster fact --- aggregate with `max()`, never
  `sum()`.
]

#r("store.gc.completion-witness+2")[
  Completion of a live collect pass is a typed property of having scanned
  the FULL keyspace under this cycle's mark: only an unresumed pass that
  exhausts the candidate scan may anchor the durable backlog estimate at
  zero. A cursor-resumed pass that exhausts the remainder resets the
  cursor but MUST keep the decremented backlog estimate. The post-drain
  tombstone reap is NOT disposition-gated: its qual is entirely row-local
  (soft-deleted at least the grace term ago, fully drained), so it MUST
  run on every live cycle, bounded by a per-cycle reap cap --- coupling it
  to the full-scan proof starves reaping permanently when daily
  eligible-garbage production exceeds the victim cap (bug_193). The
  post-drain tail work (tombstone reap, mark cleanup) MUST NOT be able to
  fail the already-drained cycle's commit --- tail failures are contained
  (warn + counter) and retried on the next live cycle.
]

The disposition is one enum (`PassDisposition`) constructed at a single
site from the resume state and the scan exit; the durable commit consumes
it rather than re-deriving booleans --- a resumed partial scan asserting
completion is unrepresentable (bug_174, bug_137); the reap consults no
disposition at all (bug_193).

#r("store.gc.shutdown-abort")[
  `sweep` checks the shutdown token between batches (NOT mid-transaction --- a
  partial batch ROLLBACKs cleanly via tx drop). On cancellation it returns
  `SweepAbort::Shutdown`; `run_gc` releases `GC_LOCK_ID` and returns
  `Status::aborted("GC aborted: process shutting down")`. `VerifyChunks`
  likewise checks the token between PG batches and sends `Aborted` on the
  progress stream.
]

#r("store.cas.upsert-inserted+3")[
  The chunk-upsert batch INSERT returns per-row `(uploaded_at IS NULL) AS
  needs_upload` so the caller knows which blake3 hashes need upload to backend.
  The predicate is atomic with the upsert (no re-query window) and is keyed on
  confirmed backend presence rather than any liveness signal or row
  pre-existence: a chunk whose first uploader was killed mid-PUT has its row
  in place but `uploaded_at IS NULL`, so the next PutPath re-uploads instead
  of skipping into permanent data loss.
]

#r("store.cas.chunk-upload-committed")[
  `chunks.uploaded_at` is non-NULL iff a `ChunkBackend::put` for that hash has
  been observed to succeed. `cas::put_chunked` MUST set it (via
  `mark_chunks_uploaded`) only after `do_upload` returns Ok, and GC paths MUST
  clear it back to NULL when they mark the chunk `deleted=true`. Concurrent
  uploaders racing on a NULL `uploaded_at` all upload (S3 PutObject is
  idempotent for identical content); the first to reach `mark_chunks_uploaded`
  wins the timestamp.
]

#r("store.chunk.liveness-not-presence")[
  The chunk-liveness signal (the collect cycle's mark set; historically the
  `chunks.refcount` counter) MUST NOT be used to decide
  whether a chunk's bytes are present in the chunk backend. A writer MAY skip
  the backend PUT for a chunk only when `chunks.uploaded_at` is non-NULL
  (#rref("store.cas.upsert-inserted")), and `uploaded_at` is non-NULL only if
  a backend put for that hash has succeeded since the chunk was last
  soft-deleted (#rref("store.cas.chunk-upload-committed")). Probes and
  operator tooling that ask "should this chunk's object exist?" MUST key on
  `uploaded_at` (as #rref("store.admin.verify-chunks") does), never on the
  liveness signal.
]

Lesson of the M_033 production data loss: using `refcount` as a "someone
already uploaded this" signal turned a SIGKILLed uploader into a permanently
missing chunk --- the loser of the dedup race skipped its PUT against an
object that never arrived. The rule is mechanism-neutral and survives the
counter's replacement (a manifest-derived mark set is no more a presence
signal than the counter was); together with
#rref("store.chunk.no-live-collect") it is the pair of obligations any chunk
GC redesign must preserve.

= Crash-Safe S3 Deletion (`pending_s3_deletes`)

#r("store.gc.pending-deletes+2")[
  S3 deletes are not transactional with PostgreSQL. To prevent data leaks
  (chunks removed from PG but never deleted from S3) or premature deletes on
  crash, rio-store MUST use a transactional outbox: the collect cycle
  (#rref("store.gc.chunk-collect")) is the producer --- in the same
  per-batch transaction that soft-deletes chunks it writes the corresponding
  S3 keys and `blake3_hash` to `pending_s3_deletes` --- and the background
  drain MUST re-check `chunks.deleted` by `blake3_hash` under a row lock
  immediately before each irreversible backend delete, skipping and dropping
  the row when the chunk has been resurrected (`deleted = false`).
]

+ A background drain task polls `pending_s3_deletes` on a *fixed 30s interval*
  (`DRAIN_INTERVAL`, not exponential --- S3 DELETE failures are rare and
  transient; queueing absorbs bursts). The resurrection skip increments
  #(refs.metric)("rio_store_gc_chunk_resurrected_total"). On S3
  success, the row is removed; on failure, `attempts` is incremented.
+ The drain re-check consults `deleted` only --- never the legacy refcount
  counter. The post-snapshot re-reference window that the counter conjunct
  used to defend is closed by the upsert's `last_referenced_at` touch plus
  the collect grace term (#rref("store.chunk.grace-ttl")).
+ On crash/restart, unprocessed rows are retried automatically --- S3 DELETE is
  idempotent.
+ Rows exceeding max retry count (default: 10) remain in the table for alerting
  (#(refs.metric)("rio_store_s3_deletes_stuck") gauge).

#r("store.gc.outbox-reset+2")[
  Outbox exhaustion MUST NOT be absorbing, and the reset edge MUST carry
  the fresh decision WHOLE: a fresh collect decision for an object whose
  `pending_s3_deletes` row has exhausted its retry budget
  (`attempts >= MAX_ATTEMPTS`) MUST reset EVERY decision-derived column // quantifier: census(outbox_reset_carries_the_recomputed_key)
  (`attempts = 0`, `enqueued_at = now()`, and `s3_key` carrying the
  recomputed `EXCLUDED` value --- the fresh backend key; an edge that
  re-activates the row while
  keeping a stale key converts the parked-but-visible posture into a
  silent permanent object leak after a key-layout migration), while a
  duplicate decision against a row whose budget remains MUST stay swallowed
  (the dedup the partial unique index exists for). Enqueue accounting MUST
  be `rows_affected()`-based --- inserted or reset rows only, never
  keys-attempted --- so the enqueued-total counter measures what its HELP
  claims.
]
The exit edge rides the enqueue's conflict arm (the guarded
`DO UPDATE ... WHERE attempts >= MAX`): the one event that logically
restarts the budget --- a fresh decision to delete the same object --- is
exactly the event the pre-fix `ON CONFLICT DO NOTHING` swallowed, which
promoted a transient S3 outage into an unreaped tombstone and a leaked
object with only the `_stuck` gauge as evidence (bug_111; the R30
liveness-dual discipline --- the latch and its exit edge ship together).

#r("store.gc.outbox-veto-letter")[
  The outbox veto's liveness is TYPED, never narrated in prose: every
  narration over the `pending_s3_deletes` population MUST consume the
  two-variant letter `OutboxVetoLiveness {finite-drain,
  parked-operator}` --- `finite-drain` for in-budget rows (drain on
  cadence) and exhausted rows over LIVE chunks (the `deleted = FALSE`
  collect feeder re-decides when the chunk next ages out);
  `parked-operator` for exhausted rows over TOMBSTONED chunks, which NO
  production event resets (the reset feeder is gated `deleted = FALSE`)
  --- and the finite-drain claim MUST be witnessed FROM the production
  feeder end-to-end (candidate scan, soft-delete, reset arm, drain),
  never by driving the producer statement with hand-built rows.
]
The wave-11 close justified the reap's NOT-EXISTS conjunct as "a FINITE
wait, never a permanent veto" via an exit-edge witness that called the
producer directly with hand-built rows --- the edge's sole production
feeder is `deleted = FALSE`-gated and structurally unreachable for the
ordinary stuck population (S3 permissions, key-format mismatch, Glacier),
so collect.rs and drain.rs narrated one population with opposite liveness
(bug_116). The retention posture is defensible --- the CLAIM was the
defect; the letter makes the parked truth the only writable narration for
that population. A self-healing long-backoff attempts reset was
considered and REJECTED this round (a behavior change to a working
retention posture, unpriced); it is the commissioned candidate if
operator-parked rows recur in soak readbacks.

*GC-vs-GC serialization:* see #rref("store.gc.serialize-lock").

= Admin RPCs

#r("store.admin.service-gate")[
  Every `StoreAdminService` RPC MUST verify `x-rio-service-token` against a
  per-RPC caller allowlist (via `rio_auth::hmac::ensure_service_caller`) before
  reading the request body. `StoreAdminService` shares port 9002 with
  `StoreService` behind only the permissive-on-absent JWT interceptor, and
  builder-egress CCNP allows builders → 9002 at L4; builders are untrusted
  (#rref("sec.authz.service-token")). Without this gate a compromised builder
  could call `AddUpstream{tenant_id: <victim>, trusted_keys: [attacker_key]}`
  and poison every other tenant's substitution path. Per-RPC allowlists:
  `TriggerGC` ← `["rio-scheduler", "rio-controller", "rio-cli"]`;
  `VerifyChunks`/`ListUpstreams`/`AddUpstream`/`RemoveUpstream` ←
  `["rio-cli"]`; `GetLoad` ← `["rio-controller"]`. `service_verifier == None` →
  dev-mode pass-through.
]

#r("store.admin.verify-chunks")[
  `StoreAdminService.VerifyChunks` server-streams
  `VerifyChunksProgress{scanned, missing, missing_hashes, is_complete}` while
  keyset-paginating `chunks WHERE deleted=FALSE AND blake3_hash > $cursor ORDER
  BY blake3_hash LIMIT batch_size` and calling `ChunkBackend.exists_batch` per
  page. Keyset (NOT OFFSET) so a 100k-chunk store is O(N) overall.
  `batch_size=0` → default; clamped at `VERIFY_BATCH_MAX`. `deleted=TRUE` rows
  are skipped (awaiting S3-delete drain --- presence is undefined); rows no
  manifest currently references ARE verified (e.g. a mid-upload row inside the
  grace window --- the object SHOULD exist once `uploaded_at` is set). Returns
  `FAILED_PRECONDITION`
  for inline-only stores (no chunk backend). Read-only --- no `--repair`
  (deleting the PG row would be wrong if the object is recoverable; the
  operator decides). Aborts on shutdown token per
  #rref("store.gc.shutdown-abort").
]

#r("store.admin.verify-emission-cadence")[
  `VerifyChunks` MUST emit a progress frame at least every
  `ADMIN_VERIFY_EMIT_EVERY` chunk probes (`rio_common::liveness`): each PG
  batch is probed in emission sub-batches of that size with a frame after
  each, so the worst-case inter-frame gap is one sub-batch's `HeadObject`
  waves and a client inactivity bound of
  `ADMIN_STREAM_INACTIVITY_TIMEOUT` cannot fire on a healthy verify.
]
The producer-side cadence and the client-side bound are one bilateral
contract: the conformance test in `rio_common::liveness` binds the derived
worst-case emission gap strictly inside the client bound, so neither crate
can drift the contract alone. Pre-cadence, one frame per PG batch meant a
max batch (5000 chunks) legitimately outran the client's 120 s bound
against a degraded S3 and the CLI killed healthy verifies as half-open.

#r("store.admin.upstream-crud")[
  `StoreAdminService.{ListUpstreams, AddUpstream, RemoveUpstream}` manage
  `tenant_upstreams` rows over gRPC. `AddUpstream` validates `trusted_keys[]`
  entry format (`name:base64(32-byte-ed25519-pubkey)`) and `sig_mode ∈ {keep,
  add, replace}` before INSERT. `RemoveUpstream` deletes by `(tenant_id,
  base_url)`; `NOT_FOUND` if no row matched. `ListUpstreams` returns rows for
  one tenant ordered by `priority ASC`. These back `rio-cli upstream {add, rm,
  ls}`; #rref("store.substitute.upstream") consumes the resulting rows.
]

= Build Log Service
<store-log-service>

rio-store owns the build-log data plane. `LogService.AppendLog` is the
builder's authenticated bidirectional ingest stream; `LogService.TailLog` is
the unauthenticated (route-gated) read/follow stream used by the gateway's
live tail, the dashboard, and the CLI. Log chunks share the chunks bucket
under the `logs/` prefix but are position-addressed, not content-addressed
--- they do not participate in the CAS, chunk collection, or reachability GC, and
are retained on a wall-clock TTL enforced by an hourly sweep plus an S3
lifecycle rule that collects orphans (objects whose manifest row was never
written because the replica crashed between the PUT and the INSERT).

#r("store.log.append-auth+2")[
  An `AppendLog` stream MUST be authorized by the caller's HMAC assignment
  token before any batch is accepted: the header's derivation MUST normalize
  to the token's `drv_hash`, and the claimed `exec_id` MUST name a recorded
  assignment attempt of that derivation that was assigned to the token's
  `executor_id` and whose execution kind is `build`. A claimed execution
  with no matching assignment row, a different executor, or a non-`build`
  kind MUST be rejected with the permanent `superseded` class.
]

The token is a bearer credential --- the comparison is a token-currency check
over the CLAIMED execution's own row, not a presenter-identity check and not
a "latest assignment" race. An executor's own superseded attempt stays
writable (the post-completion late replay --- a builder draining its
retransmit buffer after the build finished, even after the derivation was
re-probed by a materialization mint or re-dispatched): containment is
exec-keyed chunks plus the durable caps plus the `final_line_count` ceiling,
so a stale writer can fill its own execution's gaps but never grow another's
log or its own past the recorded end. The one revocation path is the
scheduler's in-place assignment rewrite (the claimed row no longer exists);
the dropped tail in that case is disclosed by the builder's loss-disclosure
counter rather than silently retried forever. This is the relocated
scheduler-side log-batch binding gate: a compromised builder spamming a
fabricated `derivation_path` cannot pollute another execution's log, and a
late batch from a timed-out executor cannot be attributed to the next
execution.

#r("store.log.caps-durable+2")[
  The per-execution log byte cap and chunk cap MUST be enforced against
  durable per-execution aggregates: every `AppendLog` open MUST seed the
  session's accepted-byte and chunk-attempt counters from the execution's
  committed chunk manifest's MERGED coverage account (the idempotent
  union --- an honest retry's re-send of committed content is never
  double-charged, and a batch fully inside durable coverage MUST be
  dropped uncharged and un-written, acknowledged from the manifest), an
  open for an execution at or over either cap MUST be rejected with
  `FAILED_PRECONDITION` and the `x-rio-log-reject` metadata naming the
  `cap` class, and a mid-stream cap trip MUST use the same code and
  metadata. `RESOURCE_EXHAUSTED` is reserved for per-replica capacity
  conditions that a retry on another replica can satisfy.
]

A reconnect used to zero both caps (they compared session-local counters),
so the documented per-execution bounds were really per-session and a
reconnecting builder could store unbounded bytes. The accounted size
(content plus the per-line overhead, the same formula every in-memory bound
charges) is persisted per chunk (`drv_log_chunks.accounted_bytes`,
migration 089) and summed at open. The status-code split is what lets the
builder classify correctly: cap exhaustion travels with the execution
(permanent --- give up and disclose), replica capacity does not (retry
elsewhere). The merged account is one axis of a dual-axis law:
forgiveness. Its algebra is idempotent --- which is exactly why it cannot
be the only measure (the rule below).

#r("store.log.frontier-denominated")[
  Every durable-progress acknowledgement MUST name a contiguous durable
  frontier: the ack value `v` asserts that every line at or below `v` is
  durably stored, every producer MUST derive `v` through the one producing
  formula (the coverage map's contiguous prefix end), and no producer may
  emit a value above that frontier --- when nothing is contiguously durable
  from line zero, no acknowledgement is sent at all.
]

The denomination is the CONSUMER'S ordering domain, not the producer's
coverage measure: the builder's retransmit `trim()` prefix-pops every frame
at or below the ack, so the wire value is a contiguous-prefix claim, and
per-span set containment does not entail it. The covered-replay consult is
reachable only in holey-coverage states (post-floor containment is
impossible under a single contiguous prefix), so before this rule 100% of
its live inputs were unsound: a hole-spanning ack destroyed the builder's
only retransmit copy of the hole-filling lines, and a replica crash
converted the transient hole to permanent undisclosed loss. The cut leg
had the dual fault (a run committed past a silently-rejected gap acked its
own end, laundering never-accepted lines). The caps-durable account
measures above keep their frozen merged/raw algebras unchanged --- this
rule EXTENDS the seal with the ack field's own measure rather than editing
those; the machine census of every in-crate carrier site (producer, forward,
bind, and declaration classes, jurisdiction derived from the module
declarations) is `rio-store/src/logs/ack_census.rs`, and the cross-crate
union face lands with the round's census registry.

#r("store.log.raw-ceiling")[
  Every durable log write MUST be charged against a monotone
  per-execution ceiling that no reconnect resets: the `AppendLog` open
  gate MUST refuse an execution whose raw durable totals --- accounted
  bytes, and manifest rows, each summed over all committed chunk rows
  --- have reached the documented replay allowance times the
  corresponding per-execution cap, with the same permanent rejection
  class as the cap trips. Both quantities MUST carry the ceiling: bytes
  and rows.
]

The measure-swap lesson (merged_bug_002): coverage and consumption have
opposite algebras. The merged account is an idempotent union ---
identical-interval replay rows witness zero there --- so seeding a
containment budget for an untrusted at-least-once writer from it alone
hands the budget delta to whoever controls duplication: an attacker loops
open, replay, cut, reconnect, durably minting objects and manifest rows
every cycle while the seed never grows. The raw totals are monotone sums:
every committed row counts, forever, so the ceiling is cycle-cumulative by
algebra. The quantity-neutral form is load-bearing --- a byte-only ceiling
leaves the small-chunk orbit open (cover-then-prune row blocks mint
unbounded manifest rows below every byte quantity). The four measures
(merged and raw, bytes and rows) and their algebras are frozen with the
seal: the kernel's `log_account` defines them, kani pins the algebra laws,
and the gate's seed SQL is differentially pinned against the kernel fold
--- swapping any witnessed quantity re-derives the seal, never edits
inside it.

#r("store.log.read-authority")[
  Resolving a derivation to an execution for log access MUST consider only
  `build`-kind executions: the unpinned `TailLog` resolution MUST read the
  kind-filtered `latest_build_exec` view, and `AppendLog` write authority
  MUST verify the claimed execution's kind is `build`, so a materialization
  execution can neither receive nor shadow build log content.
]

A materialization attempt (the store-side substitution executor) shares the
`drv_executions` table and mints newer `exec_id`s than the build it
re-probes; kind-blind "latest execution" reads resolved to it and served an
empty log for a derivation whose build log exists. The view carries the
filter so every consumer inherits it; the `log-no-raw-latest-exec` policy
check bans new raw `ORDER BY exec_id DESC` reads of `drv_executions` outside
migrations.

#r("store.log.method-credential+2")[
  Every gRPC method bound on the store's service port MUST carry an explicit
  credential class — keyed (assignment-token, tenant-JWT, or service),
  public with a recorded rationale, or handler-enforced naming the handler
  check whose typed witness the data path requires — in one reviewable
  table, enforced by a transport-layer gate that fails closed on undeclared
  methods. There is no catch-all open class. `TailLog` and `TenantQuota`
  MUST require a verified tenant token whenever a JWT pubkey is configured,
  with no service-token bypass; `TailLog` ownership is checked in the
  handler against the verified claims.
]

Before the table existed, a method's credential demand was implicit in its
handler — `TailLog` required nothing and was indistinguishable from a
missing check, leaving build-log content readable by any pod that could
reach the port (untrusted builders included). The class table makes the
demand explicit per method, the layer rejects any method added without a
declared class, and ownership is checked in the handler per the tail
ownership rule below. Enforcement is enforce-when-configured so
keyless dev/VM deployments keep working. Builder/fetcher network policy
additionally pins an L7 allow-list that omits `TailLog` — an untrusted
build cannot reach the method even with a stolen token.

#r("store.log.tail-ownership")[
  `TailLog` ownership MUST be build-membership over production-written
  rows: a verified tenant may read an execution's log iff one of its
  builds contains the execution's derivation
  (`assignments`→`build_derivations`→`builds.tenant_id`, with a
  swept-assignment arm keyed on the execution's own recorded
  `drv_executions.drv_hash` — never the request string, which MUST
  appear in no ownership predicate). Resolution and ownership MUST fold
  into a single authorization gate whose typed witness the serve layer
  requires, and deny-with-claims MUST be absence-shaped: byte-identical
  to the error for an execution that does not exist.
]

Ownership previously keyed on `derivations.tenant_id` — a column no
production write path ever populated (dropped by migration 095), so the
gate was constant-false and the test fixtures that stamped the column
proved a vacuous truth; the swept-assignment fallback additionally matched
the *caller's request string* against derivations the caller already
owned, authorizing a pinned foreign execution (own-drv + foreign-pin).
Build-membership over `builds.tenant_id` is the production-populated
chain (owner decision 2026-06-04: any tenant whose build contains the
content-addressed derivation may read its execution logs — amends the
earlier "derivation ownership" wording; the no-service-bypass posture is
unchanged). Absence-shaped denial kills the cross-tenant existence
oracle of a distinguishable permission error; a retention-swept
execution therefore denies even an authenticated pin (accepted
narrowing). Keyless deployments (`tenant = None`) keep the
distinguishable resolution errors — there is no claims-bearing caller to
oracle.

#r("store.log.consumer-registry")[
  Every production consumer surface of the store's tenant-authenticated
  log/quota methods MUST be declared in the consumer registry
  (`METHOD_CONSUMERS`) with its credential source and a greppable code
  anchor. A keyless surface MUST carry an explicit dated owner-decision
  rationale, and MUST surface a terminal auth-required state instead of
  retrying when the store demands credentials. Browser-sent credential
  headers MUST be derived into the CORS `allow_headers` list from the
  single shared `BROWSER_CREDENTIAL_HEADERS` set.
]

Strict `TailLog` made three consumer surfaces' credential posture
load-bearing, but nothing declared what each surface sends: the
dashboard silently broke in jwt-enabled deployments (and retried the
store forever on every deny), and the CORS layer did not even allow the
tenant-token header a credentialed browser caller would need — two
halves of the same undeclared-consumer failure. The registry makes the
posture reviewable (the dashboard's KeylessOnly row cites owner decision
Q1, 2026-06-04 — the Logs tab stays keyless until a dashboard credential
is funded under its own decision; the `authRequired` terminal stream
state is the declared surface of that break), the anchors make registry
rot a CI failure, and the header derivation makes "SPA holds a token the
preflight refuses" unwritable.

#r("store.authz.declared-verifier")[
  A credential class's transport verdict MUST be derived only from the
  verifier family the class declares: the verdict arms receive single-knob
  projections of the verifier configuration, and two configurations that
  agree on the declared family MUST produce identical verdicts for that
  class. Tenant claims MUST NOT admit a service-class method.
]

The round-2 audit found the admin class keyed on the JWT knob it never
declared: the half-configured state (JWT on, service key off) admitted any
tenant's claims to every admin method, whose handlers then passed the
caller through as a service caller — any tenant was a cluster admin, and
the class's own documentation certified the state as safe. The kernel
(`rio-authz-kernel`) makes the failure unwritable rather than merely
fixed: arms cannot name a foreign knob (the projection types carry exactly
one), and the projection-constructing dispatch is pinned by a CBMC proof
of foreign-knob independence.

#r("store.authz.key-coherence")[
  The store MUST refuse to serve when the JWT pubkey is configured but the
  service HMAC key or the assignment HMAC key is not
  (`jwt ⇒ (service ∧ hmac)`), naming the missing knob in the refusal. All
  other verifier configurations MUST boot.
]

The refused states are exactly the exploitable half-configurations: with
JWT on and a key missing, some keyed class is silently unenforced while
the deployment believes itself authenticated. Dev mode (no keys), the
helm default (both HMAC keys, no JWT), and the fully-keyed production
posture all keep booting — dual-mode is permanent doctrine; half-keyed
authentication is not a mode.

#r("store.log.gap-provenance")[
  A live-tail subscriber observing a forward jump in its fan-out
  stream MUST classify the missing span against the batch's coverage
  floor (the ingest session's accepted high-water mark at fan-out
  time): a span below the floor was accepted and MUST be recovered by
  one counted backfill from manifest and live buffer; a span at or
  above the floor was never accepted and MUST be served across with no
  recovery work. A recovered backfill that still leaves an accepted
  span missing MUST finalize the stream at the unadvanced cursor
  rather than advance past unserved lines.
]
The old code treated every jump as recoverable — a worker emitting
forward jumps drove a buffer clone, a manifest read, and a recovery
count per jump (merged_bug_187's amplification), and the recovery
fold's `filter`+`advance` silently absorbed any residual gap
(merged_bug_205). The typed `GapProvenance` and the sealed
`CursorAdvance` make both shapes unrepresentable; builder suppression
is number-free by property test, so "(suppressed lines)" can no longer
be misattributed (merged_bug_275).

#r("store.log.served-claim+2")[
  A `TailLog` final message's `is_complete` MUST be minted as a
  served-stream claim correlated with the reader's served cursor:
  `complete` if and only if the execution's sealed `final_line_count`
  exists, the manifest covers it contiguously, the served watermark
  has reached it, and no advance of that watermark ever crossed a gap.
  A completeness predicate computed from durable state alone MUST NOT
  stamp a final message.
]
A seal and its covering cut can commit mid-serve; the uncorrelated
predicate then advertises a complete stream to a reader that was served
half of it, and the reconnect heal never fires (merged_bug_063). The
kernel's `final_claim` is the only constructor of the claim.

#r("store.log.final-served")[
  A final claim MUST assert completeness only over the
  served-contiguous prefix: the served-contiguity fact travels INSIDE
  the claim — every cursor movement declares whether it crossed a gap
  (the sealed advance is the only way to move a cursor), the reader
  latches the first crossing, and a latched crossing poisons
  `is_complete` regardless of what the manifest covers at claim time.
  No serve seam may mint completeness without declaring whether it
  gap-crossed.
]

Covers-now plus cursor-reached is not delivery evidence (bug_048, the
R26 lens on a pre-campaign weak witness): a gap-crossing serve advances
the watermark past an unserved span, the late-replay gate legitimately
backfills the hole afterwards, and the claim-time fold then covers
contiguously --- stamping `is_complete = true` over lines this reader
never received. The three serve seams (the live fan-out, the manifest
catch-up, the gateway relay) either advance through the latching
cursor or consume the wire claim verbatim; none re-derives
completeness from durable state.

#r("store.log.completeness-gate")[
  An execution's log is complete when its lifecycle row is terminal, its
  builder-reported `final_line_count` is known, and its chunk manifest
  contiguously covers `[0, final_line_count)`. Completeness MUST be computed
  from the manifest at read time, never latched. A complete log MUST reject
  further `AppendLog` opens, and accepted lines numbered at or past
  `final_line_count` MUST be dropped.
]

One predicate serves both "is this log done" (the `is_complete` flag readers
surface) and "may this log still be appended to" (the seal). It is monotone
and self-healing: a delayed replay that fills the last gap flips the log to
complete on the next read with no coordination, and the ingest-side rejection
closes the post-terminal injection hole without breaking the legitimate
late-replay path (an *incomplete* terminal log keeps accepting the replay
that completes it).

#r("store.log.chunk-dirent-durable+2")[
  A filesystem-backed log-chunk `put` MUST be per-put self-sufficient
  about dirent durability: after syncing the file it MUST fsync the
  chunk's FULL ancestor directory chain, child-to-root through the
  store root, unconditionally on every put --- including puts that
  find the object already present --- never classifying ancestors by
  observation; the store root is additionally fsynced at construction.
]

`sync_all` on the file makes its content durable but the dirent naming it
lives in the parent directory's data block — a crash between file-sync and
dirent-sync loses a chunk whose manifest row commits afterwards, which
reads back as a permanent `NotFound` for a line range the manifest says
exists. The S3 backend gets the equivalent ordering for free from
`PutObject` response semantics; the filesystem backend (standalone VM,
single-node dev) must do it by hand. Durability recipes that derive fsync
obligations from point-in-time observation create an implicit cross-task
obligation transfer with no handoff protocol (bug_120): a sibling put's
`create_dir_all` makes a directory observable arbitrarily long before the
sibling's fsync loop runs --- and its error paths never run it --- so the
observer's `Created` rested on a put under no obligation to complete.
Unconditional chain fsync (3-4 directories, idempotent, cheap) deletes
the probe and the shared-fate coupling entirely.

#r("store.log.chunk-immutable")[
  A committed log chunk MUST never be overwritten: chunk keys are unique per
  `(exec_id, session_id, chunk_seq)`, a chunk sequence number is consumed per
  cut attempt (not per success), and the chunk object MUST be durably written
  before the manifest row that makes it reachable.
]

Burning the sequence number on a failed cut means a retried cut --- whose
buffer may have grown since the failed attempt --- can never re-PUT a key
that a lost-response predecessor may have already committed with different
content. Object-before-manifest ordering means a crash between the two leaves
an unreachable orphan (collected by the lifecycle rule), never a manifest row
pointing at a missing object (which would read as data loss).

#r("store.log.session-margin+2")[
  The ingest-lease staleness bound MUST be derived at the consumer's
  clock from the executable schedule's own constants:
  `SESSION_STALE_AFTER` MUST exceed the schedule-derived worst
  committed-stamp age of a healthy one-miss session by a strictly
  positive, named slack term, certified by a compile-time assertion
  whose inputs are the heartbeat loop's constants; a tick's body MUST
  never displace its successor tick; a timed-out attempt is terminal
  for its tick; and a known-failed attempt MUST be retried within the
  tick-body envelope exactly when its failure returned inside the
  fast-retry budget.
]

The wave-11 retry is the cautionary instance: the same commit that
added a second bounded attempt left the 2I+R certificate, its 40 s
fixture, and the "longest possible in-flight await" prose pricing the
one-attempt schedule it replaced --- the worst committed age reached
the staleness bound exactly (zero margin) while every frozen witness
passed, and the gap shipped as a self-reported comment residual. A
compile-certified timing law derives from the loop it certifies: the
formula's inputs are the constants the loop executes, so the next
schedule change reddens the seal, not the fleet.

Every consumer of the bound (the steal arm, `lookup_live`, the
scheduler's gc conjunct) evaluates the age of the last COMMITTED
`heartbeat_at` stamp — not the producer's tick cadence. A margin
derived from the cadence (the predecessor's
`interval × 2 == stale` pair) read a healthy one-miss session dead for
up to the RPC bound: the steal deposed healthy owners mid-stream
(spurious abort, full replay), `lookup_live` reported `Stale`, and the
gc conjunct misread — a compile-certified false law (merged_bug_014,
the prescription-adopted shape: the inherited math was certified, not
re-derived). The slack is a certified TERM of the inequality, not
prose: a bare `≥` admits the exact zero-margin boundary
(`stale == 2×interval + rpc`), and constant drift to either collapse
shape is a compile red.

#r("store.log.session-keyed")[
  Concurrent or successive ingest sessions for one execution MUST NOT collide:
  each `AppendLog` stream mints a fresh `session_id` that namespaces its chunk
  keys, at most one session per execution holds the ingest lease at a time,
  and the read path MUST deduplicate overlapping session line ranges by line
  number, deterministically.
]

The per-execution session lease is an admission and routing mechanism, not a
mutual-exclusion guarantee: in the window between a lease steal and the
deposed owner observing it, two sessions can ingest and cut chunks for the
same execution concurrently. The system is correct under that overlap because
the chunk keys cannot collide and the manifest's line ranges union --- the
read path visits chunks in `(first_line, session_id)` order and keeps the
first copy of each line number, so which copy wins is a function of stable
column values, identical across reads, restarts, and replicas.

#r("store.log.ingest-bounds")[
  The `AppendLog` ingest path MUST truncate individual lines to a fixed
  maximum length, reject batches whose line numbers are not monotonically
  increasing within the stream or would overflow the manifest's integer
  range, and abort the stream once a per-execution accepted-byte cap is
  exceeded. Per-replica ingest is bounded by a byte budget and a
  concurrent-stream cap.
]

The relocated scheduler-side ring-buffer bounds, enforced at the new trust
boundary. Builders are untrusted; without the per-line and per-execution caps
a compromised builder holding a valid token could exhaust the replica's
memory, and without the monotone gate it could corrupt the manifest's line
arithmetic for its own execution.

#r("store.log.sweep-ownership+2")[
  The store's log TTL sweep owns log artifacts only, and its victims
  carry their exclusions structurally: it deletes `drv_log_chunks`
  rows, their backing objects, and dead `log_ingest_sessions` rows for
  executions past `log_retention_days` that are TERMINAL and have no
  ingest-session row inside the reap grace --- never by age alone ---
  selecting candidates with `FOR UPDATE SKIP LOCKED` so concurrent
  replicas sweep disjoint batches; config validation MUST refuse
  retention values that do not exceed the scheduler's build deadline
  cap; and the sweep MUST NOT delete `drv_executions` rows. The execution lifecycle row
  is collected by the scheduler's execution-row GC, and only when the
  row is terminal, has no active assignment, is referenced by no
  `drv_attempts` row, has no surviving `drv_log_chunks` row and no
  `log_ingest_sessions` row (artifact-before-row: the lifecycle row
  MUST outlive every log artifact keyed on it, under ANY retention
  configuration --- the deletion order is data-structural, never
  schedule- or config-dependent), and is older than
  `exec_retention_days` --- the pure conjunction
  `exec_row_sweep_eligible`, every conjunct a distinct safety guard.
]
The store-side sweep selected victims by age alone; `drv_executions` is
scheduler-owned cross-service state (terminality, report idempotency,
attempt-kind resolution), and deleting it by age destroyed the kind context
of still-referenced ledger rows behind the scheduler's back. Age alone is
also not a liveness proof (merged_bug_071): "retention is days, a session
is minutes" was an unvalidated cross-crate inequality between independently
tunable constants with zero margin at the floor --- a near-deadline-cap
build at minimum retention was still streaming when the hourly sweep fired,
permanent interior log loss recurring hourly. The exclusion is the sibling
stale-session reap's own grace discipline, the validation floor refuses the
boundary collapse (R29), and the exactness of the swept counter derives
from the locking primitive, never from sequential-pass reasoning
(bug_104).

The session-row conjunct is denominated in heartbeat STALENESS, so "the
drain holds the row" is true exactly as far as a beat covers the drain
(F10): the dedicated heartbeat task spans the disconnect drain and is
RESPAWNED for it when the original died (the panic face --- the only
no-latch death; PG errors retry by design), with the cadence-vs-staleness
coupling compile-certified by the sessions margin certificate. The model's
prior doc credited the in-memory deregistration scopeguard --- which
governs the TailLog-routing registry entry and runs BEFORE the drain ---
with this DB row's lifetime; that wrong-mechanism claim is retired (the
audit's rot exhibit), and the beat-death drain window is witnessed
RED/GREEN at the conjunct's own predicate text.

#r("store.log.write-read-bound+2")[
  The chunk payload ceiling is ONE shared constant consumed by both halves
  of the codec, and it is denominated in CHARGED bytes --- content plus the
  kernel's per-line overhead plus the frame prefix, the same formula every
  other byte-denominated bound in the pipeline charges: the cutter MUST NOT
  drain a contiguous run whose charged payload exceeds the ceiling (an
  over-bound run drains as multiple chunks), the compressor MUST refuse to
  frame a payload past it, the reader MUST refuse to decompress past it,
  the reader MUST refuse a chunk whose manifest row claims more lines than
  any decodable payload could frame (the absolute line bound,
  ceiling/prefix-width) BEFORE fetching the object, ingest MUST reject a
  single batch holding more lines than one chunk's charged capacity at
  admission (per-batch; the stream stays open), and the log plane's wire
  decode cap MUST equal the chunk ceiling. A committed chunk is therefore
  decodable by construction AND its line count is bounded by the same
  constant that bounds its bytes; a single maximally-truncated line plus
  its frame prefix always fits (compile-time asserted).
]
The write path used to bound a chunk only by line contiguity while the read
path enforced a 16 MiB decompression ceiling --- a multi-MiB contiguous run
committed a chunk the read path then refused, making the tail of that log
unreadable while the manifest claimed coverage. The first repair shared the
byte ceiling but charged bare framing, leaving the LINE-COUNT axis open
(bug_298): sixteen MiB of near-empty lines was 4.19M frames the write side
would commit and the read side then materialized as 4.19M allocations ---
a ~33x resident amplification the byte bound never saw, reachable through
a wire admission path whose decode cap (256 MiB) dwarfed the chunk it fed.

#r("store.log.loss-event-identity+1")[
  Every read-path anomaly counter is incremented in exactly one module,
  once per anomaly identity (family, execution, object key) per process
  --- never per read visit. For the unrecoverable-loss family: a manifest
  row that no longer stands when its object is found missing (the sweep
  race) MUST be a clean skip, and a divergence every claimed line still
  serves across (overlong object, covered short or missing object) MUST
  feed the warn-severity divergence family instead. The divergence family
  counts once per divergent-object identity with the kind as a label ---
  its trend alert reads the counter as incidence, so a persistent
  divergent object re-visited by every traversal MUST NOT re-tick it. The
  family is part of the ledger key: a hole and a divergence on the same
  object are distinct anomalies. The loss alert pages on any increment;
  the increment therefore carries the page's meaning.
]
The pre-fix read path incremented the page-on-any-increment loss counter
once per VISIT (N readers of one missing object = N pages), and routed
served-anyway overlong divergence into the same counter --- operators
learned to triage "data loss" pages as maybe-nothing, which is how real
holes go unnoticed.

#r("store.log.cap-reject-class")[
  Every permanent per-execution AppendLog rejection (byte cap, chunk cap
  --- mid-stream and during the final drain --- completeness seal,
  supersession) MUST be constructed by the single gate constructor that
  stamps FAILED_PRECONDITION plus the x-rio-log-reject class metadata,
  and RESOURCE_EXHAUSTED MUST appear in the log plane only via the
  replica-capacity constructor (admission gates: stream cap, byte
  budget). The two vocabularies are disjoint by construction: per-replica
  means retry elsewhere, per-execution means stop everywhere.
]
bug_068: the mid-stream chunk cap was hand-rolled as bare
RESOURCE_EXHAUSTED --- per-replica vocabulary for a per-execution fact ---
so the builder re-dialed at 1 Hz forever, and the final drain's cap arm
told the builder to "reconnect and replay" with the same effect. The
classifier on the builder side was correct all along; the server simply
never spoke the class it listened for.

#r("store.log.progress-clock")[
  Abort backstops in the log-ingest plane MUST measure durable
  progress, never occupancy: the stale-buffer staleness arm's clock
  MUST refresh on every successful chunk cut (time since the durable
  frontier last advanced while lines were pending), so that a
  continuously-busy stream that keeps committing is never stale
  regardless of how old its oldest pending line is.
]

An occupancy clock (time since the pending set was last empty) pins at
stream start under steady load --- the exact regime a busy production
stream lives in --- so any predicate gated on it silently degenerates:
the 3-strike consecutive-failure budget collapsed to 1-strike for every
busy stream older than two cut intervals, and one transient S3/PG blip
mass-aborted every such stream on the replica with a full unacked
replay (merged_bug_007). The R29 form: the envelope is denominated in
the consumer's execution domain --- the abort predicate consumes
"progress stalled", so the clock must measure progress.

#r("store.log.release-totality")[
  A held ingest-session lease row MUST be released on every path that
  does not hand it to a live owner: the release obligation is minted
  at the acquire observation as a linear value whose only discharge is
  the handoff to the teardown-owning driver, so every fallible or
  cancellable step between acquire and handoff --- present and future
  --- releases by construction rather than by arm enumeration.
]

The wave-11 acquire-race close got every DRIVER exit right and itself
minted a pre-driver fallible step (the ownership witness) outside the
enforced alphabet: one PG blip on the witness SELECT stranded the
just-acquired row for the full staleness window --- cross-pod
reconnects got Busy, `lookup_live` reported a phantom Live. The law's
quantifier is "every path after a successful acquire"; the enforced
population was "every driver exit". `LeaseReleaseGuard`
(release-on-drop, session-id-predicated, disarmed only at the driver
handoff) makes the two equal by type --- cancellation included, which
no per-arm compensation can cover --- and the linear-obligation form is
the `sys.obligation.linear-discharge` doctrine's lease instance. The
restore path gains the dual face: a failed open never resurrects a
DEAD predecessor entry (driver-done is marked by the teardown
scopeguard cancelling the entry's token), so the spent-scopeguard hang
is unrepresentable.

#r("store.log.arrival-clock+2")[
  Liveness gates on peer conduct MUST read arrival evidence for the
  abort decision --- frames the peer has already sent, drained and
  stamped at the gate's own consult, never the enforcer's read
  progress standing in for it. An explicit self-activity conjunct that
  only DELAYS a trip (masking the enforcer's own stall so it cannot
  masquerade as peer silence) is PERMITTED and MUST be named in the
  gate's documentation together with the resulting delay budget; its
  stamp MUST carry occupancy evidence --- an outcome that performed
  work --- never the refreshing timer's own schedule: a stamp
  writable by a no-op tick is FORBIDDEN (a refresh period at or under
  the bound otherwise kills the gate except at exact phase
  coincidence), and every cross-constant coupling in the disclosed
  budget MUST hold by compile-time assert with its phase margin
  written down.
]

The inbound-idle gate (bug_020) is the founding instance: `last_inbound`
stamped only at application reads while each in-arm cut legally occupies
up to one watchdog bound plus a bounded ack send, so a slow-but-
successful cut starved reads past the bound with a conformant builder's
keepalives sitting queued unread --- the documented "a CONFORMANT peer
cannot trip this arm" was falsified by the enforcer's own occupancy. The
post-fix clock is honestly two-axis, `max(last-drained-arrival,
last-self-activity)`: the housekeeping arm drains ready inbound before
the predicate (arrival truth made visible) and only cut outcomes that
DID WORK stamp self-activity --- a no-op periodic tick writes nothing
(bug_018: with the cut period at the idle bound, an Empty-tick stamp
made the conjunct satisfiable only at exact phase coincidence; the
enforcement half of the bilateral law was structurally dead, and a
wedged-but-connected builder renewed its ingest lease forever). The
disclosed delay budget, in its honest expanded form: past LAST ARRIVAL
the trip is delayed by at most the idle bound plus three
cut-interval-denominated terms --- the stamp lag (a sub-threshold final
batch buffers until the next periodic tick), the occupied cut's
watchdog bound, and its bounded ack send --- plus one housekeeping
consult quantum; 255 s at the shipped constants, compile-asserted
against the disclosed 300 s eviction ceiling with a 15 s phase margin
(`idle_trip_worst_case`, the one producer of the arithmetic), and the
operator `log_cut_interval` validates against the same ceiling from
ABOVE (70 s maximum --- a lower bound would be the wrong direction).
Never earlier than the bound; a genuinely silent peer with an idle
enforcer trips at exactly the bound, because no occupancy stamp exists
to delay it. A wave-10 fix had diagnosed this
same cut-latency starvation and relocated only the lease heartbeat ---
the cured-one-consumer shape this rule's census face exists to refuse;
the wave-12 close then implemented the self-activity axis as the cut
timer's own schedule --- the refresh-on-no-op shape this rule now
forbids outright.

#r("store.log.ingest-idle-abort+2")[
  The log-ingest liveness law is bilateral. An AppendLog client whose
  session is open with an empty buffer MUST emit an empty keepalive
  batch at least every `UPLOADER_KEEPALIVE_PERIOD`; an ingest driver
  holding NO accepted-but-not-yet-durable lines (the buffer and any
  cut-staged in-flight run both empty — one derived emptiness over
  every place an accepted line can wait) whose inbound stream has been
  silent for `INBOUND_IDLE_ABORT` (four heartbeat intervals) MUST
  abort the stream (counted, `reason="inbound_idle"`) rather than
  continue renewing the ingest lease; while any accepted line is
  not yet durable the abort MUST defer (the pending work is retried by
  the cut path, whose own failure laws bound the stream instead). The
  two bounds live as one shared const pair whose conformance relation
  (period times margin strictly less than abort) is enforced by test.
]

Lease renewal is thereby structurally coupled to observed stream
liveness: a builder that vanished without a FIN cannot hold its
execution's ingest lease indefinitely through a driver that renews
forever --- and neither can a wedged-but-connected one (bug_018: the
self-activity conjunct is occupancy-denominated under the
arrival-clock rule above, so an idle enforcer's no-op cut ticks never
hold this gate open; the enforcement half is witnessed de-phased,
end to end, past four idle bounds). The nothing-pending gate makes the abort loss-free by
construction — the predecessor form gated on buffer bytes alone, which
excluded the in-flight run a watchdog-abandoned cut leaves staged, so
the abort destroyed committable lines the next cut's restore would
have retried (merged_bug_144); pending lines' liveness is owned by the
cut path's bounded ack send and its failure counters, not by this arm.

SIGNED 2026-06-08 (owner, bughunt-4 fix-wave #5-S Q1): the idle-abort
law is bilateral. The builder uploader carries the producer side (an
empty-batch keepalive every `UPLOADER_KEEPALIVE_PERIOD` while a session
is open with an empty buffer); this rule's abort is the enforcement
side; the shared const pair lives in `rio-common`'s liveness module
with a conformance test proving period times margin strictly under the
abort bound. The round-3 dashboard test writer's keepalive is RATIFIED
as that client's own conformance duty under this law --- never removed.
Clients that bypass the builder uploader are their own producers.

#r("store.log.driver-bounded")[
  Every await the AppendLog driver performs on behalf of the builder
  (ack delivery, chunk cuts — in-loop, drain, and cleanup) MUST be
  bounded: in-loop acks wait at most one cut interval before the stream
  ends as a client disconnect, cleanup and drain acks never wait, and
  every chunk cut runs under the one-cut-interval watchdog.
]

The driver is the server's representative of one builder; nothing the
builder does or fails to do (stop reading acks, vanish mid-cut behind a
wedged backend) may park the driver — its select loop owns the abort
check and the inbound read, while the session-lease heartbeat runs on
its own dedicated task whose single RPC await is compile-asserted
inside the staleness margin, so no loop await can starve the lease
cadence and a parked driver is a leaked slot at worst, never an
immortal lease-renewer or a silently-stale healthy session. The two
named ack forms and the single named cut form are the only callable
shapes; an in-file self-scan pins the census, and the drive-loop
liveness census pins the heartbeat's off-loop home.

#r("store.log.proxy-disabled-not-failure")[
  A replica whose cross-replica tail proxy is disabled (no peer URL
  template) MUST treat the disabled state as configuration, not
  failure: it performs no live-owner lookup, dials nothing, increments
  no proxy-failure counter, and logs nothing per read — one boot-time
  statement of the disabled posture is the only signal.
]

Disabledness is decided once, at construction (`Option<PeerResolver>`;
an empty template constructs `None`), so the proxy arm — lookup,
dial, failure counter, warn — is structurally unreachable for a
disabled deployment rather than reached-and-failing on every read of
a non-owned execution.

#r("store.log.tail-reconnect")[
  A `follow`-mode `TailLog` stream ends when the ingest session it is
  attached to closes, which does not imply the execution is finished. A
  reader whose follow stream ends while the execution is not yet terminal
  MUST re-subscribe with `since_line` set to one past the last line it
  relayed; the server MAY resend lines below the requested cursor (chunk
  granularity) and the client MUST skip them.
]

#r("store.log.attach-hello")[
  A `follow`-mode `TailLog` attach MUST emit one zero-line, non-final,
  exec-stamped chunk between the replayed history and the live
  subscription, so the attaching client observes the serving
  execution's identity immediately even when nothing at or past its
  cursor exists yet.
]

The hello is what makes a follow-the-retry reconnect detect an execution
switch on a quiet stream: a reader carrying the previous execution's longer
cursor attaches above the new execution's watermark, history replays
nothing, and without the hello the first signal would be the builder's next
batch --- a dead-quiet fresh execution indistinguishable from a live stream
on the old one. Zero-line chunks are already protocol-tolerated ("clients
must tolerate and skip them"), so the hello is invisible to consumers that
do not key on it.

Store deploys, replica crashes, and ingest-lease handoffs all close follow
streams that a new ingest session immediately replaces; the server does not
chase the new session across replicas, so the client owns re-subscription.
The dedup floor advancing only on lines actually relayed is what makes the
contract exactly-once on the client's wire: the store's at-least-once,
chunk-granular delivery becomes a gapless, duplicate-free line stream.

The live tail is served history-then-live from the ingesting replica: a
subscriber registers and snapshots the in-memory buffer atomically with
respect to chunk cuts, then reads the manifest, then drains the
subscription --- a line is therefore observed at least once (possibly twice,
deduplicated by the cursor) and never zero times across the
stored-to-live seam. A `TailLog` reaching a replica that does not hold the
execution's ingest session is proxied one hop to the owner; on proxy failure
it degrades to the manifest-only view rather than erroring.

#r("store.log.tail-grace-drain+2")[
  A live-tail relay MUST NOT stop re-subscribing while it still has grace
  budget and the served log is not complete. The exit decision is one total
  function over (stop cause, terminal, grace expired, served complete):
  exit exactly when the post-terminal grace has expired, or when the relay
  is orphaned (its consumer-side lifecycle channel is gone --- no reader
  remains and no signal can ever arrive), or when the failure is
  typed-permanent (the store stamped the status unservable-forever ---
  every future open refuses identically, so re-dialing is a wedge, not a
  recovery), or when the stream ended naturally with the execution
  terminal and the store's final message claiming the served log complete.
  Transport errors and open failures after terminal re-open within the
  remaining grace; the post-terminal grace deadline is armed exactly once
  per subscription. A forward jump in the served stream is re-opened at
  the gap exactly once before being accepted and disclosed inline, the
  lines that arrived past the jump are WITHHELD with the pending gap (not
  dropped), every exit path flushes a still-pending gap --- marker plus
  withheld lines --- through the single accept-and-disclose path, and a
  first sighting whose remaining grace cannot fund the re-open chance is
  accepted immediately. An orphaned relay MUST NOT open another stream ---
  its exit is unconditional --- and relay ownership MUST be drop-safe:
  dropping the owning subscription set aborts every relay, so no drop path
  can leave one running unowned.
]

The conflations this rule forbids each lost final lines in production
shapes: a transport error after terminal exited with zero re-opens (the
replica serving the stream was restarting --- precisely when the final
lines are still in flight); an open failure at terminal gave up with zero
attempts; a terminal signal landing during a backoff exited the
subscription outright; and a natural end was treated as drained without
consulting the store's own completeness claim, which the relay discarded
unread. The served-complete bit is load-bearing: it is the only signal
that distinguishes "the session closed because everything durable was
served" from "the session closed mid-upload". The orphan clause closes
the inverse failure (merged_bug_130): a relay whose owning set was
dropped without an abort observed its lifecycle channel's death as a
wake-up, skipped every backoff, and hot-looped stream opens at full
speed --- for a consumer that no longer existed.

Exit-at-expiry binds EVERY await in the loop, the open included // quantifier: census(hung_open_abandons_at_drain_deadline)
(bug_038): the re-open's per-attempt bound is DEADLINE-TYPED --- the
fixed bound clamped to whatever remains of the armed grace envelope,
consulted before the open arms --- so a hung re-open against a
half-open replica is cut at the remaining grace instead of running its
full fixed bound (shipped: a 10 s open bound against a 2 s grace ---
the unclamped await stretched exit-at-expiry 5-6x and delayed the
truncation disclosure by the difference). The backoff sleep consumes
the same clamp producer, and grace-conformance fixtures preserve the
production parameter ordering (open bound LARGER than grace) --- the
inverted-ratio fixture could not represent the breach it existed to
refuse.

#r("store.log.tail-fanout-recovery")[
  The live-tail fan-out MAY drop batches (a slow reader must never
  backpressure ingest), but a follow serve loop MUST NOT advance its
  cursor past a dropped span: on observing a forward jump it MUST
  back-fill from the chunk manifest and the live ingest buffer --- which
  together hold every accepted line, because the drop and the buffering
  happen in one critical section --- before serving the triggering batch,
  and MUST run the same catch-up before its final message so the
  advertised resume cursor covers only served lines.
]

The recovery is in-stream because the alternative --- the reader noticing
the hole on reconnect --- never happens for the common consumer: the
gateway relay holds one continuous follow stream for a build's whole
lifetime, so a dropped batch used to become a permanent hole in the nix
client's output while the final message advertised a clean cursor past it.
A drop now costs one recovery pass (a manifest read and possibly chunk
GETs) instead of lines.

#r("store.log.read-divergence+2")[
  A chunk's served range and post-visit watermark MUST be bounded by the
  smaller of the manifest row's claim and the object's actual line count,
  and any disagreement between the two MUST be classified and disclosed: a
  long object serves the claimed range only, the unclaimed excess
  discarded and counted. A short object (object holds fewer lines than
  the row claims) and a missing object whose manifest row still stands
  MUST be decided by COVERAGE, not arm order: the missing span is first
  clamped by the reader's served-prefix cursor (lines already yielded by
  earlier overlapping-session chunks are proof of service), and when the
  cursor plus the remaining manifest rows cover the span, the servable
  lines are served and the covering rows supply the rest; when no
  coverage exists, the read fails through the single permanent-refusal
  constructor as a TYPED-permanent error (the unservable-hole gRPC
  metadata key) naming the chunk key, with the hole ledgered --- never a
  silently shorter stream, and never an untyped error a reader re-dials
  forever. Every identically-forever refusal arm (oversized row, missing
  object, short object, undecodable chunk) MUST route through that
  constructor. The manifest claim is the single authoritative bound at
  every decision point of the read path.
]

The two divergence directions have different blast radii: an over-length
object used to serve its excess under the NEXT chunk's line numbers ---
garbage output that also suppressed the genuine successor lines via the
advanced watermark; an under-length object was a silent hole presented as
complete. Both are corruption-grade (the row and object are written from
the same slice in the same call). The short direction splits by coverage
(bug_233): the covered topology (a second session's overlapping chunk) is
fully servable and was previously wedged behind an untyped internal error
at the relay's 1 Hz re-dial — it is disclosed as divergence, not counted
as loss (#(refs.metric)("rio_store_log_read_data_loss_total") alerts on
any increment and is reserved for unrecoverable holes); the uncovered
topology is genuine unrecoverable loss, counted and typed so readers
stop. The
seal/read contradiction itself is disclosed, not repaired --- there is no
in-band repair for a row and object written from one slice that
disagree.

= PostgreSQL Schema
<store-schema>

#r("store.db.migrate-try-lock+2")[
  Migration runs MUST serialize across concurrent runners via a non-blocking
  `pg_try_advisory_lock` poll loop, with sqlx's built-in blocking
  `pg_advisory_lock` disabled (`Migrator::set_locking(false)`). Migrations 011
  and 022 run `CREATE INDEX CONCURRENTLY`; CIC's final phase waits for every
  older virtualxid to release. Under sqlx's default lock, a second runner
  blocked in `SELECT pg_advisory_lock(...)` holds such a virtualxid for the
  duration --- the leader's CIC waits on the follower's vxid, the follower
  waits on the leader's advisory lock, and PG's deadlock detector does not see
  across that pair. The try-lock poll holds no long-lived virtualxid (each
  probe is a sub-ms SELECT that completes immediately), so the leader's CIC
  proceeds.
]

Migrations normally execute in exactly one place — the `rio-migrate` Job
(`rio-store migrate`, see
#cross-link("/spec/system/deployment.typ")[Deployment]) — but the lock stays:
it is what makes concurrent runner invocations safe (an old-named Job still
running across an upgrade, a legacy in-pod migrator during mid-upgrade skew,
a manually re-run Job).

#r("store.db.schema-current+2")[
  Service startup MUST NOT run migrations. It MUST verify that every
  embedded migration is applied (`rio_migrations::migrate::assert_current`)
  and fail with an error naming the migration runner (`rio-store migrate` /
  the `rio-migrate` Job) when the schema is missing or stale.
  Applied-but-not-embedded versions are accepted: during a rolling upgrade
  the migrate Job lands the newer schema while old-binary replicas may
  still restart against it (migrations are forward-compatible by policy).
]

Running migrations out-of-band, always as the database master, decouples
schema DDL from app-pod credentials: `postgres.authMode=iam` pods never
need DDL-capable database privileges.

#r("store.db.ensure-roles")[
  Every migrate run MUST re-assert the `rio_app` role, its `rds_iam`
  membership (where that role exists), and its full table/sequence/default
  privileges (`rio_migrations::ensure_roles`), under the same advisory-lock
  hold as the migrations themselves; role and grant management MUST NOT
  ship as checksum-frozen migrations. Where the connected user lacks the
  required privileges (k3s migrates as the bitnami app user, which has no
  CREATEROLE), the pass MUST degrade to a warning, not a failure.
]

Roles and grants are desired state — cluster-wide, environment-dependent
(`rds_iam` exists only on RDS), and subject to drift from manual incident
recovery — not schema history. Two live incidents motivated the move out
of frozen SQL: a role migration's `GRANT rio_app TO <master>` made the
master inherit `rds_iam` and RDS PAM rejected its password (locking out
the migration runner itself), and the frozen follow-up's
`REASSIGN OWNED BY rio_app` rewrote owner-ACL entries and stripped all of
rio_app's privileges while the migrate Job reported success. Re-asserting
grants on every run makes both classes self-healing; the advisory lock
serializes the cluster-wide role DDL across concurrent runner invocations.

#r("store.db.pool-idle-timeout+2")[
  The PostgreSQL connection pool MUST set `idle_timeout` (60s) and
  `min_connections` (2). Aurora Serverless v2's `max_connections` is
  RUNTIME-CONSTANT, derived from the configured MAXIMUM capacity (the AWS
  PostgreSQL table in `infra/eks/rds.tf`) and capped at 2,000 when
  `min_capacity` ≤ 0.5 ACU --- it does not scale with the live ACU. Idle
  connections count against that fixed budget: the sqlx default 10-minute
  idle reap means a burst-grown pool holds its full `max_connections` long
  after the burst ends, so N store + scheduler replicas at their pool maxima
  can exhaust the budget and ad-hoc psql gets `FATAL: remaining connection
  slots are reserved`. The same constraint applies to any service holding a
  PG pool against the shared database (scheduler).
]

Pseudo-DDL for all store tables. `narinfo` and `manifests` are split to avoid
TOAST write amplification when updating manifest status.

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

#info[
  The `manifests` + `manifest_data` + `chunks` tables are the active schema as
  of Phase 2c (migration `002_store.sql` dropped the Phase 2a `nar_blobs`
  table). Small NARs (\< 256 KiB) store inline in `manifests.inline_blob`;
  larger NARs are FastCDC-chunked with BLAKE3 dedup. ChunkBackend is
  constructed from config (`ChunkBackendKind` enum: `Inline` / `Filesystem` /
  `S3`, default `Inline` for back-compat).
]

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

*Inline vs. chunked invariant:* If `manifests.inline_blob IS NOT NULL`, no
corresponding `manifest_data` row exists --- the NAR content is stored entirely
in the `inline_blob` field. Conversely, if `inline_blob IS NULL`, a
`manifest_data` row MUST exist with a valid `chunk_list`. Code must check
`inline_blob` first; only if it is NULL should `manifest_data` be queried. The
`manifest_data` foreign key does not require a row to exist (no `ON DELETE`
action forces creation).

```sql
CREATE TABLE chunks (
    blake3_hash        BYTEA PRIMARY KEY,
    size               BIGINT NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    uploaded_at        TIMESTAMPTZ,            -- confirmed backend presence (M_033)
    last_referenced_at TIMESTAMPTZ,            -- upsert conflict-arm touch (070_chunks_last_referenced_at)
    deleted            BOOLEAN NOT NULL DEFAULT FALSE
);
-- (the historical refcount column, its CHECK, and idx_chunks_gc are
-- dropped by migrations 069/070 with the retired counter machinery)

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

#info[
  GC tables (`scheduler_live_pins`, `pending_s3_deletes`) are added by
  `migrations/006_gc_safety.sql`. See the `rio-store/src/gc/` module for the
  mark/sweep/drain implementation. The `gc_roots` explicit-pin table was
  created in 005 but never gained a production writer; dropped in 036.
]

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

= Design References (no code dependencies)

- tvix `castore` protobuf definitions (MIT-licensed): inform our @cas gRPC API
  design
- tvix `store` protobuf definitions (MIT-licensed): inform our PathInfo API
  design
- Attic's FastCDC-based chunking approach: reference for chunk size tuning and
  dedup strategy
- NAR and narinfo formats: implemented from scratch in `rio-nix`

= Key Files

- `rio-store/src/grpc/` --- StoreService gRPC implementation
  - `mod.rs` --- service struct + shared state
  - `put_path.rs` --- PutPath handler (buffer, verify, branch inline/chunked)
  - `get_path.rs` --- GetPath handler (manifest load, parallel reassembly
    stream)
  - `chunk.rs` --- test-only `GetChunk` retrieval surface (the
    FindMissingChunks batch RPC was removed)
- `rio-store/src/metadata/` --- narinfo + manifest persistence (PostgreSQL)
  - `mod.rs` --- re-exports + shared types
  - `inline.rs` --- inline-blob fast path (write `manifests.inline_blob` BYTEA)
  - `chunked.rs` --- chunked-path manifest + chunk-row UPSERT
  - `queries.rs` --- narinfo SELECT/UPDATE, QueryPathInfo, FindMissingPaths
- `rio-store/src/validate.rs` --- NAR hash verification (HashingReader,
  NarDigest, `validate_nar_digest`)
- `rio-store/src/backend/` --- ChunkBackend trait + S3/filesystem/memory impls
- `rio-store/src/cas.rs` --- `put_chunked` orchestration, ChunkCache (moka LRU
  + @singleflight + BLAKE3 verify)
- `rio-store/src/chunker.rs` --- FastCDC wrapper (16K/64K/256K min/avg/max)
- `rio-store/src/manifest.rs` --- Chunk manifest (de)serialization, versioned
  binary format
- `rio-store/src/realisations.rs` --- CA `(drv_hash, output_name) →
  output_path` mapping
- `rio-store/src/signing.rs` --- ed25519 narinfo signing at PutPath time

= GC Files

- `rio-store/src/gc/mod.rs` --- GC lock constant (`GC_LOCK_ID`), `GcStats`,
  `run_gc` orchestration
- `rio-store/src/gc/mark.rs` --- `compute_unreachable` recursive CTE (seeds:
  `scheduler_live_pins`, uploading manifests, grace-period, `extra_roots`,
  tenant retention)
- `rio-store/src/gc/sweep.rs` --- Per-path batched delete with `FOR UPDATE OF
  m` + reference re-check
- `rio-store/src/gc/drain.rs` --- Background S3 delete drain (30s interval,
  `blake3_hash` re-check)
- `rio-store/src/gc/orphan.rs` --- Stale `'uploading'` manifest cleanup
- `rio-store/src/grpc/admin.rs` --- `StoreAdminService` (TriggerGC, PinPath,
  UnpinPath)

= Rationale

== Custom chunked CAS // supersedes ADR-006

NAR archives for different store paths often share significant content (common
libraries, similar build outputs). Efficient storage and transfer require
sub-NAR deduplication. NAR archives are chunked using content-defined chunking
(FastCDC); identical chunks across store paths are stored once. Two tiers: an
*inline fast-path* where NARs below 256KB are stored as a single PostgreSQL
`BYTEA` blob with no chunking overhead and no S3 round-trip (most small
derivations fall here), and a *chunked path* where larger NARs are split by
FastCDC, a chunk manifest in PostgreSQL maps each NAR to its ordered list of
chunk references, and the chunk bodies live in S3.

Hash domains are strictly separated: SHA-256 for all Nix-facing hashes (store
path hashes, NAR hashes, output hashes) and BLAKE3 for internal chunk
addressing only --- BLAKE3 is \~3× faster for chunking workloads and is never
exposed to Nix, so it's a free performance win. Whole-NAR storage was rejected
(no cross-path dedup; for large closures with shared libraries, storage and
transfer costs are significantly higher). The inline fast-path avoids chunking
overhead for small paths (the majority by count), and chunk-manifest lookup
latency is mitigated by caching.

*The hard part: store path transfer efficiency.* Moving closures between
rio-store and executors is the main bottleneck. NAR streaming avoids
materializing full NARs in memory; chunk-level deduplication enables
incremental transfers; the scheduler sends #glspl("prefetch-hint") to the executor's
@fuse cache before assigning work; and the per-executor FUSE store with local
SSD cache provides local-disk performance for hot paths without shared
infrastructure.

*The hard part: CAS durability under partial failure.* The content-addressable
store has two write-time failure modes: _orphaned chunks_ (chunk upload
succeeds but metadata write fails, leaving unreferenced chunks in blob storage)
and _broken manifests_ (metadata write succeeds but some chunks are missing,
producing an unreadable manifest). The @write-ahead-manifest pattern resolves
both: write chunk references to a pending manifest before uploading chunks,
then promote to committed after all chunks are verified ---
#rref("store.put.wal-manifest").

== CA-ready data model // supersedes ADR-004

Content-addressed (CA) derivations enable early cutoff: if a derivation's
output is identical to a previous build, downstream rebuilds are skipped. Most
of nixpkgs is input-addressed today, but CA adoption is expected to grow. The
data model is CA-ready from Phase 2c so CA support activates without a later
migration: PostgreSQL tables include content-indexed lookups and a
`realisations` table mapping `(drv_hash, output_name)` to `(output_path,
output_hash)`; gateway stubs for `wopRegisterDrvOutput` and
`wopQueryRealisation` write and read this metadata; and input-addressed
derivations remain the primary execution path. Retrofitting CA later would have
required coordinated schema, store, scheduler, and protocol changes; doing it
CA-first would have required derivation resolution and output rewriting before
the basic build pipeline worked.

*The hard part: CA early cutoff correctness.* When a CA derivation's output
matches cached content, the cutoff must propagate correctly through the @dag ---
a cutoff at node N means all transitive dependents of N can potentially skip
rebuilding if their other inputs are also unchanged. This requires careful
state management in the scheduler.

== CA resolution and `dependentRealisations` // supersedes ADR-018

The `realisation_deps(realisation_id, dep_realisation_id)` junction table
stands, but the population source is the *scheduler* during its own
`tryResolve`-equivalent pass --- NOT the gateway from wire payloads.
Source-level analysis of the locked Nix input showed that *Nix removed
`dependentRealisations` from the data model upstream*: the struct has no such
field, the serializer writes a `{}` stub for back-compat, and the deserializer
ignores it. On the wire, `dependentRealisations` is always `{}` for any current
Nix client.

Nix's build-trace model split realisations into a *base build trace*
(resolved-derivation-hash → output-path; coherent by construction; the unit of
exchange) and a *derived build trace* (unresolved-derivation-hash →
output-path; a local memoization cache). `dependentRealisations` had bundled
derived-trace provenance into the wire record, conflating the two.

When #rref("sched.ca.resolve") rewrites a CA derivation's `inputDrvs` by
querying `realisations` for each input's `(drv_hash, output_name)`, each
successful lookup is a dependency edge inserted into `realisation_deps` at
resolution time. This is rio's derived build trace --- a local cache rio
computes from its own base entries; it never crosses the wire. Gateway handlers
stay as-is: `handle_register_drv_output` continues ignoring the field, and
`handle_query_realisation` continues emitting `{}`.

CA-on-CA frequency is bimodal: in default nixpkgs, no individual package opts
into CA piecemeal (all gates are behind `config.contentAddressedByDefault`,
default `false`), so resolution never fires; under a full-CA config, resolution
fires on every dispatch. The resolve step is either dead code or critical-path;
there is no "occasional" case.

= Failure modes

#figure(
  table(
    columns: (auto, 1fr, 1fr, 1fr),
    align: (left, left, left, left),
    table.header(
      [Component down], [Immediate effect], [Cascading effect], [Recovery]
    ),
    [*Store (all replicas)*],
    [`QueryPathInfo`, `PutPath`, `GetPath` fail],
    [Executors can't fetch inputs (FUSE cache misses fail) or upload outputs;
      builds stall on I/O. Gateway can't answer `wopQueryPathInfo`.],
    [Retry on store recovery. Builds that timed out waiting for store are
      retried. Executor overlay outputs are lost if all upload retries fail.],

    [*PostgreSQL*],
    [Scheduler can't persist state; store can't query metadata],
    [Full system halt --- no scheduling, no metadata lookups, no new builds],
    [Restore PG from backup. All components reconnect via connection retry.
      Scheduler rebuilds DAG via `recover_from_pg` on next LeaderAcquired. If
      PG is restored but DAG state is lost (full data loss), clients must
      resubmit.],

    [*S3 (object storage)*],
    [Chunk reads/writes fail],
    [Store returns errors to executors and gateways; executor uploads fail.
      Builds whose inputs are fully SSD-cached may continue.],
    [Retry with backoff (S3 DELETE is idempotent). Executor overlay outputs are
      lost if all upload retries fail.],
  ),
)

If rio-store is degraded (slow but not down), all executors' FUSE cache misses
queue up: FUSE read operations block, build sandboxes stall, and the
scheduler's @backpressure mechanism (actor queue depth > 80%) rejects new builds
with `RESOURCE_EXHAUSTED`. After 5 consecutive `ensure_cached` failures, the
FUSE circuit breaker opens and `check()` returns `EIO` immediately (fail-fast)
--- see #rref("builder.fuse.circuit-breaker").
