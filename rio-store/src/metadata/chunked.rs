//! Chunked write-ahead: large NARs split by FastCDC, chunks in S3,
//! manifest_data holds the ordered (blake3, size) list.
//!
//! `upgrade_manifest_to_chunked` takes an EXISTING placeholder (from
//! `insert_manifest_uploading`) and adds manifest_data + increments chunk
// r[impl store.chunk.refcount-txn]
// r[impl store.put.wal-manifest]
//! refcounts — the placeholder is the idempotency lock, created BEFORE the
//! NAR stream is consumed, before we know the size. Refcounts go up here
//! (not at complete) so GC between upload and complete sees count > 0.

use super::*;
use sqlx::PgPool;
use std::collections::HashSet;
use tracing::{debug, instrument, warn};

/// Opaque generation token for an `'uploading'` placeholder, captured
/// at [`upgrade_manifest_to_chunked`] time and checked at
/// [`delete_manifest_chunked_uploading`] time. If the orphan reaper
/// reclaims the placeholder and a re-uploader inserts a fresh one, the
/// fresh row's token differs — the stale uploader's late rollback
/// bails instead of clobbering the re-uploader's state.
///
/// Encoded as `EXTRACT(EPOCH FROM updated_at)::float8` (PG float8 ↔
/// Rust f64 is an exact IEEE-754 byte transfer; the codebase has no
/// chrono/time dep). Compared by exact equality in PG. The
/// post-upgrade heartbeat advances `updated_at`, so a rollback after a
/// successful heartbeat sees a token mismatch on its OWN row — that's
/// a deliberate false-negative (rollback no-ops, orphan scanner cleans
/// up after `STALE_THRESHOLD`). Safe; the alternative (no token) lets
/// a stale uploader clobber a fresh one mid-upload.
pub(crate) type PlaceholderToken = f64;

// ---------------------------------------------------------------------------
// Chunked manifest ops
// ---------------------------------------------------------------------------

/// Upgrade an existing 'uploading' manifest to chunked: write manifest_data
/// + increment chunk refcounts.
///
/// # Why this takes an EXISTING placeholder
///
/// grpc.rs PutPath runs `insert_manifest_uploading()` at step 3, BEFORE
/// the NAR stream is consumed (it's the idempotency lock — prevents
/// concurrent uploaders). Only at step 6, after buffering + validating,
/// do we know the size. At that point we already OWN the placeholder;
/// this function adds the chunked metadata to it.
///
/// A standalone `insert_manifest_chunked_uploading` that creates its own
/// placeholder would either (a) need to know the size upfront (can't —
/// stream isn't consumed yet), or (b) delete+recreate the placeholder
/// (window for another uploader to slip in). Upgrade-in-place avoids both.
///
/// # Why refcounts are incremented here (before upload), not at complete
///
/// Per `store.typ`: incrementing before upload protects chunks from GC
/// sweep immediately. If a GC pass runs between upload and complete, it
/// sees refcount > 0 and skips. If we waited until complete, a GC between
/// "chunks uploaded to S3" and "status flipped" would sweep → orphaned.
///
/// The tradeoff: if the upload fails and we forget to decrement, refcounts
/// are leaked. The orphan scanner (future phase) catches this via stale
/// 'uploading' manifests.
#[instrument(skip(pool, chunk_list, chunk_hashes, chunk_sizes), fields(store_path_hash = hex::encode(store_path_hash), chunks = chunk_hashes.len()))]
pub(crate) async fn upgrade_manifest_to_chunked(
    pool: &PgPool,
    store_path_hash: &[u8],
    chunk_list: &[u8],        // serialized Manifest
    chunk_hashes: &[Vec<u8>], // each is a 32-byte BLAKE3
    chunk_sizes: &[i64],      // parallel to chunk_hashes
) -> Result<(HashSet<Vec<u8>>, PlaceholderToken)> {
    let mut tx = pool.begin().await?;

    // Ownership lock: the manifests row MUST exist with status=
    // 'uploading' AND we must hold a FOR UPDATE lock on it for the
    // rest of this txn. A plain `SELECT EXISTS(...)` (no FOR UPDATE)
    // is wrong: under READ COMMITTED, the orphan reaper can delete +
    // commit between the EXISTS and the INSERT below, leaving an
    // orphaned `manifest_data` row + leaked refcounts with no
    // `manifests` parent. FOR UPDATE blocks `reap_one` (and
    // `complete_manifest_chunked`) until this tx commits, so the
    // verdict holds for the whole tx. Same pattern as
    // `gc::orphan::reap_one`.
    //
    // The returned epoch is the [`PlaceholderToken`] — passed through
    // to `delete_manifest_chunked_uploading` so a late rollback can
    // distinguish "still my placeholder" from "reaper reclaimed mine
    // and a re-uploader inserted a fresh one".
    let token: Option<PlaceholderToken> = sqlx::query_scalar(
        r#"
        SELECT EXTRACT(EPOCH FROM updated_at)::float8 FROM manifests
        WHERE store_path_hash = $1 AND status = 'uploading'
        FOR UPDATE
        "#,
    )
    .bind(store_path_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(token) = token else {
        return Err(MetadataError::PlaceholderMissing {
            store_path: hex::encode(store_path_hash),
        });
    };

    // manifest_data: the chunk list. No ON CONFLICT — the placeholder
    // from step 3 didn't write manifest_data, so this row shouldn't
    // exist. If it does (caller called us twice?), PG errors on PK
    // conflict — that's a bug, let it fail.
    sqlx::query(
        r#"
        INSERT INTO manifest_data (store_path_hash, chunk_list)
        VALUES ($1, $2)
        "#,
    )
    .bind(store_path_hash)
    .bind(chunk_list)
    .execute(&mut *tx)
    .await?;

    // Hard length check: the co-sort `zip` below truncates to
    // `min(len)`, so a mismatch would silently drop trailing chunks
    // (manifest_data references hashes with no `chunks` row → GC-
    // eligible). The sole prod caller (cas.rs) builds both via
    // `.unzip()` so this can't fire today; the check makes the
    // contract executable instead of a comment promise.
    if chunk_hashes.len() != chunk_sizes.len() {
        return Err(MetadataError::InvariantViolation(format!(
            "chunk_hashes/chunk_sizes length mismatch: {} vs {}",
            chunk_hashes.len(),
            chunk_sizes.len()
        )));
    }

    // Refcount UPSERT. UNNEST over parallel arrays (lengths asserted
    // equal above — there is NO PG-side length check).
    //
    // The array-of-1s for initial refcount: can't use a literal `1` in
    // the UNNEST position (not an array). Materializing N×1 is mildly
    // silly but cleaner than CROSS JOIN with a single-row constant.
    //
    // ON CONFLICT DO UPDATE is atomic per-row. PG's conflict resolution
    // serializes INSERT vs UPDATE — two concurrent PutPaths with
    // overlapping chunk lists both increment correctly.
    //
    // r[impl store.chunk.refcount-txn]
    // Co-sort (hash, size) pairs by hash before UNNEST: same deadlock
    // prevention as the rollback path (see delete_manifest_chunked_
    // uploading). ON CONFLICT DO UPDATE acquires row locks on the
    // conflicted rows in UNNEST input order; two concurrent upgrades
    // with reversed-order overlapping sets would otherwise deadlock.
    // The co-sort keeps each hash paired with its size.
    let mut pairs: Vec<(Vec<u8>, i64)> = chunk_hashes
        .iter()
        .cloned()
        .zip(chunk_sizes.iter().copied())
        .collect();
    pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let (chunk_hashes, chunk_sizes): (Vec<Vec<u8>>, Vec<i64>) = pairs.into_iter().unzip();
    //
    // `deleted = false`: resurrects a chunk that GC sweep marked for
    // deletion (refcount hit 0) between sweep and drain. Without
    // this, PutPath would bump refcount but leave `deleted=true` →
    // chunk still looks dead. The drain re-check (drain.rs) is the
    // PRIMARY guard; this is defense-in-depth so `chunks` row state
    // is self-consistent (refcount>0 implies deleted=false).
    //
    // r[impl store.cas.upsert-inserted+2]
    // RETURNING (uploaded_at IS NULL) AS needs_upload: a chunk needs
    // (re-)upload iff no prior PutPath has confirmed S3 presence via
    // `mark_chunks_uploaded`. Contrast with the previous heuristic
    // `(refcount = 1)`, which assumed "rc≥1 before me ⇒ someone else
    // already uploaded" — false when that someone is mid-upload and
    // gets SIGKILLed (helm rolling update). M_033 has the full race.
    //
    // RETURNING sees the POST-update row state (SQL standard). So:
    //   fresh INSERT             → uploaded_at = NULL          → needs_upload = true
    //   CONFLICT, uploaded_at NULL (in-flight or interrupted)  → needs_upload = true
    //   CONFLICT, uploaded_at set (S3-confirmed)               → needs_upload = false
    //
    // Two concurrent PutPaths sharing a chunk now BOTH upload —
    // S3 PutObject is idempotent (same key, same bytes), so the
    // duplicate write is wasted bandwidth, not a correctness hazard.
    let rows: Vec<(Vec<u8>, bool)> = sqlx::query_as(
        r#"
        INSERT INTO chunks (blake3_hash, refcount, size)
        SELECT * FROM UNNEST($1::bytea[], $2::bigint[], $3::bigint[])
               AS t(hash, one, size)
        ON CONFLICT (blake3_hash) DO UPDATE
            SET refcount = chunks.refcount + 1, deleted = false
        RETURNING blake3_hash, (uploaded_at IS NULL) AS needs_upload
        "#,
    )
    .bind(&chunk_hashes)
    .bind(vec![1i64; chunk_hashes.len()])
    .bind(&chunk_sizes)
    .fetch_all(&mut *tx)
    .await?;

    let needs_upload: HashSet<Vec<u8>> = rows
        .into_iter()
        .filter_map(|(h, need)| need.then_some(h))
        .collect();

    tx.commit().await?;
    Ok((needs_upload, token))
}

/// Record that the given chunk hashes are now durably present in the
/// backend. Called by `cas::put_chunked` AFTER `do_upload` succeeds,
/// BEFORE the manifest is flipped to `complete`.
///
/// `WHERE uploaded_at IS NULL` makes this idempotent: two concurrent
/// PutPaths that both uploaded the same chunk both call here; the
/// second one is a no-op (0 rows updated). The timestamp is the FIRST
/// confirmed upload, not the last.
///
/// Hashes are sorted before binding — same lock-order discipline as
/// every other `chunks` writer (`r[store.chunk.lock-order]`).
// r[impl store.cas.chunk-upload-committed]
#[instrument(skip(pool, hashes), fields(count = hashes.len()))]
pub(crate) async fn mark_chunks_uploaded(pool: &PgPool, hashes: &[Vec<u8>]) -> Result<()> {
    let mut conn = pool.acquire().await?;
    mark_chunks_uploaded_in_conn(&mut conn, hashes).await
}

/// In-transaction variant of [`mark_chunks_uploaded`] for callers that
/// must record S3 presence atomically with the rest of their commit
/// (`PutPathChunked`'s single commit transaction).
///
/// `AND deleted = FALSE`: a row the orphan sweep tombstoned between
/// the caller's S3 PUT and this stamp (possible for the pool-level
/// [`mark_chunks_uploaded`] call from `cas::stage_chunked`, which runs
/// outside any chunk row lock) must NOT get `uploaded_at` — the drain
/// may already be deleting the object, and a stamped-then-resurrected
/// row would skip the re-PUT and let a manifest point at a drained
/// object.
// r[impl store.cas.chunk-upload-committed]
pub(crate) async fn mark_chunks_uploaded_in_conn(
    conn: &mut sqlx::PgConnection,
    hashes: &[Vec<u8>],
) -> Result<()> {
    if hashes.is_empty() {
        return Ok(());
    }
    let mut sorted = hashes.to_vec();
    sorted.sort_unstable();
    sqlx::query(
        "UPDATE chunks SET uploaded_at = now() \
         WHERE blake3_hash = ANY($1) AND uploaded_at IS NULL AND deleted = FALSE",
    )
    .bind(&sorted)
    .execute(conn)
    .await?;
    Ok(())
}

/// Finalize a chunked upload: fill real narinfo + flip status to
/// 'complete' + write the castore index.
///
/// Does NOT write inline_blob (stays NULL — that's the chunked marker).
/// Does NOT touch manifest_data (already written at uploading time).
/// Does NOT touch refcounts (already incremented at uploading time).
///
/// `castore` is `None` only in metadata-layer tests that seed fake
/// chunk lists; such paths are not GetPath-servable. Production
/// (`cas::put_chunked`) always passes `Some`.
///
/// `tenant` is the uploader's resolved tenant for the `path_tenants`
/// junction (`r[store.put.tenant-junction]`); `None` writes no row.
#[instrument(skip(pool, info, castore), fields(store_path = %info.store_path.as_str()))]
pub(crate) async fn complete_manifest_chunked(
    pool: &PgPool,
    info: &ValidatedPathInfo,
    claim: uuid::Uuid,
    castore: Option<&crate::cas::ParsedNar>,
    tenant: Option<uuid::Uuid>,
) -> Result<()> {
    // Single bounded retry on 40P01 (same policy as
    // `with_sorted_retry` and the sweep batch loop): the sorted
    // lock-then-flip in `mark_manifest_chunks_durable` SHOULD prevent
    // deadlock, but PG can still 40P01 under index-page-split
    // contention — and unlike PutPathBatch (whose caller pre-locks
    // via `lock_staged_chunks_for_commit` and surfaces retryable
    // statuses), this single-output commit was the client-visible
    // ~1%-of-PutPaths failure with no server-side retry. The first
    // attempt's tx rolled back on deadlock, leaving the placeholder
    // claim intact for the re-run.
    crate::metadata::retry_once_on_deadlock(|| {
        complete_manifest_chunked_attempt(pool, info, claim, castore, tenant)
    })
    .await?;
    debug!(store_path = %info.store_path.as_str(), "chunked upload completed");
    Ok(())
}

/// One completion transaction attempt — split out so
/// [`complete_manifest_chunked`] can retry the whole txn on 40P01.
async fn complete_manifest_chunked_attempt(
    pool: &PgPool,
    info: &ValidatedPathInfo,
    claim: uuid::Uuid,
    castore: Option<&crate::cas::ParsedNar>,
    tenant: Option<uuid::Uuid>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    super::complete_manifest_in_conn(&mut tx, info, claim, None, castore, tenant).await?;
    tx.commit().await?;
    Ok(())
}

/// Reclaim a failed chunked upload: decrement refcounts + delete rows.
///
/// **Must be called with the SAME chunk_hashes that were passed to
/// insert_manifest_chunked_uploading.** Decrementing a different set
/// would corrupt refcounts. The caller (cas.rs) holds the Manifest
/// across the upload, so this invariant is easy to maintain.
///
/// `token` gates the whole rollback on still owning the placeholder —
/// see the ownership-lock comment in
/// `delete_manifest_chunked_uploading_inner` for the race it closes.
#[instrument(skip(pool, chunk_hashes), fields(store_path_hash = hex::encode(store_path_hash), chunks = chunk_hashes.len()))]
pub(crate) async fn delete_manifest_chunked_uploading(
    pool: &PgPool,
    store_path_hash: &[u8],
    token: PlaceholderToken,
    chunk_hashes: &[Vec<u8>],
) -> Result<()> {
    // r[impl store.chunk.refcount-txn]
    // r[impl store.put.wal-manifest]
    // Sort-before-ANY() + single-retry-on-40P01 via the shared helper.
    // See with_sorted_retry doc for the deadlock-prevention rationale.
    with_sorted_retry(chunk_hashes.to_vec(), |hashes| async move {
        delete_manifest_chunked_uploading_inner(pool, store_path_hash, token, &hashes).await
    })
    .await
}

/// Transaction body for `delete_manifest_chunked_uploading`. Split out
/// so the outer function can retry the whole txn on 40P01.
async fn delete_manifest_chunked_uploading_inner(
    pool: &PgPool,
    store_path_hash: &[u8],
    token: PlaceholderToken,
    hashes: &[Vec<u8>],
) -> Result<()> {
    let mut tx = pool.begin().await?;

    // Ownership lock: verify the `'uploading'` placeholder is still
    // ours and row-lock it for the rest of this txn. If A's PUT hangs
    // >15min → orphan reaper reclaims A's placeholder → B re-uploads
    // the same path → A's hung PUT errors → without this check, the
    // unguarded refcount decrement + manifest_data DELETE below would
    // clobber B's state (mid-upload OR complete). Mirrors
    // `gc::orphan::reap_one`'s freshness re-check.
    //
    // `status='uploading'` alone is NOT sufficient: B's fresh row is
    // ALSO `'uploading'` until B completes. The `updated_at` token
    // (captured at A's `upgrade_manifest_to_chunked`) distinguishes
    // A's generation from B's. A token mismatch means either (a) the
    // reaper reclaimed + B inserted a fresh row, or (b) A's own
    // heartbeat advanced `updated_at` — case (b) is a deliberate
    // false-negative (orphan scanner cleans up A's leaked refcounts;
    // see [`PlaceholderToken`] doc). The FOR UPDATE serializes against
    // `reap_one` and `complete_manifest_chunked` for the rest of the tx.
    let still_ours: Option<(i32,)> = sqlx::query_as(
        r#"
        SELECT 1 FROM manifests
        WHERE store_path_hash = $1
          AND status = 'uploading'
          AND EXTRACT(EPOCH FROM updated_at)::float8 = $2
        FOR UPDATE
        "#,
    )
    .bind(store_path_hash)
    .bind(token)
    .fetch_optional(&mut *tx)
    .await?;
    if still_ours.is_none() {
        // Reaper already cleaned up, or a re-uploader holds a fresh
        // placeholder, or our own heartbeat advanced updated_at.
        // Nothing to do — the dependent rows are not ours to touch.
        debug!("rollback: placeholder token mismatch — leaving for orphan scanner");
        return Ok(());
    }

    // Decrement refcounts FIRST. If we deleted manifest_data first and
    // then crashed before decrementing, the refcounts would be leaked
    // forever (no manifest references them, but count > 0 so GC skips).
    // Decrementing first means a crash here leaves manifest_data
    // pointing at chunks with count=0 — the orphan scanner (later phase)
    // catches that by finding stale 'uploading' manifests.
    //
    // M023 `CHECK (refcount >= 0)` makes a would-be-negative refcount a
    // constraint violation → transaction rolls back → surfaces as
    // `MetadataError::Other` → gRPC INTERNAL. A negative here means the
    // caller passed wrong hashes (or double-decremented) — fail loud at
    // the source, don't silently leak the chunk. See migrations.rs M_023.
    sqlx::query(
        r#"
        UPDATE chunks SET refcount = refcount - 1
        WHERE blake3_hash = ANY($1)
        "#,
    )
    .bind(hashes)
    .execute(&mut *tx)
    .await?;

    // Delete manifest_data (via CASCADE from manifests, but explicit for
    // clarity and to not depend on schema details).
    sqlx::query(
        r#"
        DELETE FROM manifest_data
        WHERE store_path_hash = $1
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;

    // manifests + narinfo placeholders (same guards as inline variant).
    sqlx::query(
        r#"
        DELETE FROM manifests
        WHERE store_path_hash = $1 AND status = 'uploading'
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        DELETE FROM narinfo
        WHERE store_path_hash = $1 AND nar_size = 0
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Write-ahead `chunks` rows for `PutPathChunked` (ADR-022 §6.2):
/// every digest the builder is about to stream gets a
/// `refcount = 0, uploaded_at = NULL, durable = FALSE` row **before**
/// the first S3 `PutObject`. This row is what makes a crash-orphaned
/// S3 object findable by the `r[store.chunk.grace-ttl]` sweep; without
/// it, a verify task that dies between `backend.put` and the commit
/// transaction leaves S3 objects no GC pass can enumerate.
///
/// Refcounts are NOT bumped here — that happens in the commit
/// transaction, per referencing output. A pre-existing **refcount-0**
/// row (left by a prior failed attempt, or claimed by the GC sweep
/// between the builder's `HasChunks` probe and now) gets its grace
/// clock restarted and its `deleted` mark cleared: the upload is about
/// to re-PUT the object, and clearing `deleted` makes the drain's
/// re-check skip the pending S3 delete instead of racing the new
/// `PutObject`. Rows with `refcount > 0` are live under another
/// manifest and are left alone. Hashes are sorted ascending before
/// binding (`r[store.chunk.lock-order]`).
///
/// The grace clock is NOT refreshed again during the verify walk, so
/// an upload that takes longer than `CHUNK_GRACE_SECS` and straddles a
/// sweep run finds its rows re-claimed at commit time and aborts
/// `UNAVAILABLE` (see [`lock_chunks_for_commit`]) — a retryable error,
/// not corruption.
// TODO: fold a `chunks.created_at = now()` refresh for the pending set
// into the placeholder guard's heartbeat so a >CHUNK_GRACE_SECS upload
// cannot collide with the orphan sweep at all.
// r[impl store.put.wal-manifest]
#[instrument(skip(pool, chunks), fields(count = chunks.len()))]
pub(crate) async fn insert_pending_chunks(pool: &PgPool, chunks: &[([u8; 32], u32)]) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let mut sorted: Vec<([u8; 32], u32)> = chunks.to_vec();
    sorted.sort_unstable_by_key(|(d, _)| *d);
    let (hashes, sizes): (Vec<Vec<u8>>, Vec<i64>) = sorted
        .into_iter()
        .map(|(h, s)| (h.to_vec(), i64::from(s)))
        .unzip();
    sqlx::query(
        r#"
        INSERT INTO chunks (blake3_hash, refcount, size)
        SELECT u.hash, 0, u.size FROM UNNEST($1::bytea[], $2::bigint[]) AS u(hash, size)
        ON CONFLICT (blake3_hash) DO UPDATE
            SET deleted = FALSE, created_at = now()
            WHERE chunks.refcount = 0
        "#,
    )
    .bind(&hashes)
    .bind(&sizes)
    .execute(pool)
    .await?;
    Ok(())
}

/// Stop claiming presence for a chunk whose backing object is provably
/// gone: clear `durable` and `uploaded_at` (both assert "the S3 object
/// exists") so `HasChunks` answers absent and the next uploader streams
/// the chunk again instead of skipping it. The row, its refcount, and
/// any referencing manifests are left alone — the re-upload is an
/// idempotent overwrite that makes them readable again, whereas a
/// row delete would orphan their refcount accounting.
///
/// Used by the `PutPathChunked` deferred file-digest verification when
/// a backend GET for a supposedly-durable chunk returns "no object"
/// (the GC-grace-vs-ack-TTL hole observed in production: a presence row
/// that outlived its S3 object made every client skip the upload and
/// fail forever).
pub(crate) async fn clear_chunk_presence(pool: &PgPool, digest: &[u8; 32]) -> Result<()> {
    sqlx::query("UPDATE chunks SET durable = FALSE, uploaded_at = NULL WHERE blake3_hash = $1")
        .bind(digest.as_slice())
        .execute(pool)
        .await?;
    Ok(())
}

/// One committed `file_blobs` binding per requested digest:
/// `(digest, store_path_hash, nar_offset, size)` for the
/// lowest-`(store_path_hash, nar_offset)` row whose manifest is
/// `'complete'` and chunked (`manifest_data` row exists — inline
/// referrers have no chunk window to compare against).
///
/// Used by the `PutPathChunked` deferred file-digest verification: a
/// deferred run whose claimed digest already has a committed binding
/// with the *identical* chunk window is proven without re-fetching any
/// bytes (chunk digests determine content). The `(store_path_hash,
/// nar_offset)` ordering matches the `ReadBlob`/`StatBlob` `ORDER BY`,
/// so this picks the same canonical row read-side resolution serves —
/// except that reads are additionally tenant-filtered, so a tenant
/// that cannot see the global winner resolves a different (still
/// proven) binding.
pub(crate) async fn trusted_file_windows(
    pool: &PgPool,
    digests: &[Vec<u8>],
) -> Result<Vec<(Vec<u8>, Vec<u8>, i64, i64)>> {
    if digests.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(
        r#"
        SELECT DISTINCT ON (f.digest) f.digest, f.store_path_hash, f.nar_offset, f.size
          FROM file_blobs f
          JOIN manifests m ON m.store_path_hash = f.store_path_hash
               AND m.status = 'complete'
          JOIN manifest_data md ON md.store_path_hash = f.store_path_hash
         WHERE f.digest = ANY($1)
         ORDER BY f.digest, f.store_path_hash, f.nar_offset
        "#,
    )
    .bind(digests)
    .fetch_all(pool)
    .await?)
}

/// `manifest_data.chunk_list` for a set of store paths — the second
/// half of [`trusted_file_windows`]: chunk lists are TOASTed (MBs for
/// a large NAR), so they are fetched once per distinct referrer, not
/// once per digest.
pub(crate) async fn chunk_lists_for_paths(
    pool: &PgPool,
    store_path_hashes: &[Vec<u8>],
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    if store_path_hashes.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(
        "SELECT store_path_hash, chunk_list FROM manifest_data WHERE store_path_hash = ANY($1)",
    )
    .bind(store_path_hashes)
    .fetch_all(pool)
    .await?)
}

/// Lock every chunk a multi-statement commit transaction will touch
/// and return the digests whose S3 object cannot be proven to exist.
///
/// Run FIRST in the commit transaction. This is THE lock-acquisition
/// helper for `chunks` rows in commit transactions — every later
/// statement in the same transaction (refcount UPSERTs, the `durable`
/// flip in `mark_manifest_chunks_durable`,
/// `mark_chunks_uploaded_in_conn`) must only touch rows this call
/// already locked, which is why it takes the COMPLETE set: `digests`
/// (the union of every non-skipped output's manifest digests) **plus**
/// every digest in `uploaded_by_this_stream` (novel chunks this stream
/// PUT, including ones whose only manifest occurrence is in an
/// idempotent-skipped output — `mark_chunks_uploaded_in_conn` still
/// stamps those). The union is sorted internally, so callers cannot
/// get the order or the set wrong. Two jobs:
///
/// 1. **Lock order** (`r[store.chunk.lock-order]`): one sorted
///    `FOR UPDATE` acquires every `chunks` row lock the transaction
///    will need, so the statements that follow only touch already-held
///    rows regardless of their relative order — locking a row outside
///    this set after the sorted batch would re-open the ABBA window
///    with a concurrent sorted writer. This also serializes against
///    the GC drain's `FOR UPDATE` re-check: whichever side locks
///    first, the other observes its committed outcome.
/// 2. **Presence proof** (`r[store.chunk.durable-flag]`): the commit
///    is about to flip `durable = TRUE` and/or stamp `uploaded_at` for
///    these digests, which asserts "the S3 object exists". A chunk is
///    provably present iff its row exists, is not `deleted` (the sweep
///    has not claimed it for the drain since the verify walk obtained
///    its bytes), and either has `uploaded_at` set (a prior commit
///    confirmed the object) or was `PutObject`'d by **this** stream
///    (`uploaded_by_this_stream`). Anything else — a missing row, a
///    sweep-claimed row, a never-confirmed row served from a cache —
///    is returned so the caller can abort with `UNAVAILABLE` instead
///    of committing a manifest that points at an object the drain may
///    already have deleted (the I-201 lie, through the GC round-trip).
// TODO: r[store.chunk.lock-order]'s spec text only asks for sorted
// per-statement lock acquisition; the discipline this helper (and
// lock_staged_chunks_for_commit) now enforces is stronger — the FULL
// union of every chunk row a commit transaction will touch is locked
// up front in one sorted FOR UPDATE. Tighten the rule text to match in
// a follow-up spec pass (with `tracey bump`) so the spec states the
// guarantee the code actually relies on.
// r[impl store.chunk.lock-order]
pub(crate) async fn lock_chunks_for_commit(
    conn: &mut sqlx::PgConnection,
    digests: &[Vec<u8>],
    uploaded_by_this_stream: &std::collections::HashSet<[u8; 32]>,
) -> Result<Vec<Vec<u8>>> {
    // Union + dedup + sort in one pass: BTreeSet iteration is
    // ascending, which is exactly the lock order the rule requires.
    let lock_set: Vec<Vec<u8>> = digests
        .iter()
        .cloned()
        .chain(uploaded_by_this_stream.iter().map(|d| d.to_vec()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if lock_set.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(Vec<u8>, bool, bool)> = sqlx::query_as(
        r#"
        SELECT blake3_hash, deleted, uploaded_at IS NOT NULL
          FROM chunks
         WHERE blake3_hash = ANY($1)
         ORDER BY blake3_hash
           FOR UPDATE
        "#,
    )
    .bind(&lock_set)
    .fetch_all(&mut *conn)
    .await?;
    let by_hash: std::collections::HashMap<&[u8], (bool, bool)> = rows
        .iter()
        .map(|(h, deleted, uploaded)| (h.as_slice(), (*deleted, *uploaded)))
        .collect();
    let unproven = lock_set
        .iter()
        .filter(|d| {
            let ours = <&[u8; 32]>::try_from(d.as_slice())
                .is_ok_and(|a| uploaded_by_this_stream.contains(a));
            match by_hash.get(d.as_slice()) {
                Some((deleted, uploaded)) => *deleted || !(*uploaded || ours),
                None => true,
            }
        })
        .cloned()
        .collect();
    Ok(unproven)
}

/// `PutPathBatch` pre-lock: the batch's phase-3 transaction completes
/// N staged outputs in output-index order, and each completion's
/// `mark_manifest_chunks_durable` row-locks that output's chunks — so
/// without a batch-wide pre-lock the cross-output acquisition order
/// follows output index, not digest order, and two concurrent batches
/// sharing freshly-staged chunks can ABBA-deadlock (40P01) after their
/// NAR verification already succeeded. This reads the staged
/// `manifest_data.chunk_list` for every output in the batch (written
/// by `cas::stage_chunked` in phase 2) and feeds the digest union
/// through [`lock_chunks_for_commit`] — the same single lock helper
/// the chunked-upload commit uses — so the per-output `durable` flips
/// only touch already-held rows.
///
/// Returns the unproven set (see [`lock_chunks_for_commit`]); a
/// non-empty result means a staged chunk was reclaimed by GC between
/// staging and commit and the batch must abort retryably instead of
/// flipping `durable` for an object the drain may already have
/// deleted. Inline outputs have no `manifest_data` row and contribute
/// nothing.
// r[impl store.chunk.lock-order]
pub(crate) async fn lock_staged_chunks_for_commit(
    conn: &mut sqlx::PgConnection,
    store_path_hashes: &[Vec<u8>],
) -> Result<Vec<Vec<u8>>> {
    if store_path_hashes.is_empty() {
        return Ok(Vec::new());
    }
    let chunk_lists: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT chunk_list FROM manifest_data WHERE store_path_hash = ANY($1)")
            .bind(store_path_hashes)
            .fetch_all(&mut *conn)
            .await?;
    let mut digests: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    for chunk_list in &chunk_lists {
        // The staged chunk_list was serialized by `stage_chunked` from
        // a `Manifest` we produced; a parse failure here is the same
        // corruption `mark_manifest_chunks_durable` would hit one
        // statement later — fail the commit rather than skip the lock.
        let manifest = crate::manifest::Manifest::deserialize(chunk_list).map_err(|e| {
            MetadataError::InvariantViolation(format!(
                "staged manifest_data.chunk_list is corrupt at batch commit time: {e}"
            ))
        })?;
        digests.extend(manifest.entries.iter().map(|e| e.hash.to_vec()));
    }
    let digests: Vec<Vec<u8>> = digests.into_iter().collect();
    lock_chunks_for_commit(conn, &digests, &std::collections::HashSet::new()).await
}

/// Commit one `PutPathChunked` output inside the caller's transaction:
/// `manifest_data` insert, chunk refcount bump, then the status flip,
/// narinfo, castore index, and `path_tenants` junction via
/// [`super::complete_manifest_in_conn`].
///
/// Ordering matters: `complete_manifest_in_conn`'s
/// `mark_manifest_chunks_durable` reads `manifest_data.chunk_list`
/// for this path, so the `manifest_data` insert MUST precede it in the
/// same transaction. The refcount bump precedes the durable flip for
/// the same reason every other writer's does: `durable = TRUE` asserts
/// "some complete manifest references this chunk", which is only true
/// once the refcount reflects that reference.
///
/// `chunk_hashes`/`chunk_sizes` are the output's **distinct** digests
/// (one refcount per unique chunk per manifest, matching the GC
/// decrement) and MUST be pre-sorted ascending
/// (`r[store.chunk.lock-order]`).
// r[impl store.chunk.refcount-txn]
// r[impl store.put.tenant-junction]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn commit_chunked_output_in_conn(
    conn: &mut sqlx::PgConnection,
    info: &ValidatedPathInfo,
    claim: uuid::Uuid,
    manifest_bytes: &[u8],
    chunk_hashes: &[Vec<u8>],
    chunk_sizes: &[i64],
    parsed: &crate::cas::ParsedNar,
    tenant_id: Option<uuid::Uuid>,
) -> Result<()> {
    if chunk_hashes.len() != chunk_sizes.len() {
        return Err(MetadataError::InvariantViolation(format!(
            "chunk_hashes/chunk_sizes length mismatch: {} vs {}",
            chunk_hashes.len(),
            chunk_sizes.len()
        )));
    }
    // The placeholder claimed in phase A wrote no manifest_data row,
    // and a reaped placeholder CASCADEs its manifest_data away — a
    // surviving row here means a double-commit, which PG surfaces as a
    // PK conflict (same contract as `upgrade_manifest_to_chunked`).
    sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
        .bind(&info.store_path_hash)
        .bind(manifest_bytes)
        .execute(&mut *conn)
        .await?;

    // Same UNNEST upsert shape as `upgrade_manifest_to_chunked`, minus
    // the `RETURNING needs_upload` (the §6.3 verify walk already
    // decided what to upload from `Begin.novel`). `deleted = false`
    // resurrects a chunk the GC sweep marked between the builder's
    // HasChunks probe and now.
    if !chunk_hashes.is_empty() {
        sqlx::query(
            r#"
            INSERT INTO chunks (blake3_hash, refcount, size)
            SELECT * FROM UNNEST($1::bytea[], $2::bigint[], $3::bigint[]) AS t(hash, one, size)
            ON CONFLICT (blake3_hash) DO UPDATE
                SET refcount = chunks.refcount + 1, deleted = false
            "#,
        )
        .bind(chunk_hashes)
        .bind(vec![1i64; chunk_hashes.len()])
        .bind(chunk_sizes)
        .execute(&mut *conn)
        .await?;
    }

    // Status flip + narinfo + chunks.durable + nar_index/directories/
    // directory_paths/file_blobs + path_tenants junction. Reads the
    // manifest_data row inserted above. The junction insert is the
    // tolerant variant: an assignment token can name a tenant deleted
    // while the build was in flight; that skips the junction write
    // instead of aborting the commit transaction this output just
    // completed in.
    super::complete_manifest_in_conn(conn, info, claim, None, Some(parsed), tenant_id).await
}

/// `path_tenants` junction insert (`r[store.castore.tenant-scope+3]`).
/// Idempotent; a `None` tenant (dev mode, service-token caller) writes
/// nothing. Runs for idempotent-skipped outputs too — the prior commit
/// may belong to another tenant or predate tenancy.
// r[impl store.castore.tenant-scope+3]
// r[impl store.put.tenant-junction]
pub(crate) async fn insert_path_tenant_in_conn(
    conn: &mut sqlx::PgConnection,
    store_path_hash: &[u8],
    tenant_id: Option<uuid::Uuid>,
) -> Result<()> {
    let Some(tenant_id) = tenant_id else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(store_path_hash)
    .bind(tenant_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The PG-generated names of the tenant-junction foreign keys to
/// `tenants` (`path_tenants` from 012, `chunk_tenants` from 072). PG
/// includes the constraint name verbatim in the 23503 error message,
/// which is how [`is_deleted_tenant_fk`] recognizes the violation
/// without matching any other conflict.
const TENANT_JUNCTION_FKS: [&str; 2] = [
    "path_tenants_tenant_id_fkey",
    "chunk_tenants_tenant_id_fkey",
];

/// True iff `err` is the foreign-key violation raised by a tenant
/// junction insert (`path_tenants` or `chunk_tenants`) whose tenant
/// row no longer exists — an assignment token minted for a tenant that
/// was deleted while the build was in flight. [`MetadataError::
/// Conflict`] is only produced for SQLSTATE 23503/23505 and carries
/// PG's primary message, which for an FK violation names the
/// constraint; requiring the full `violates foreign key constraint
/// "<name>"` phrase means a unique violation (23505) or an FK
/// violation on any other constraint never matches.
pub(crate) fn is_deleted_tenant_fk(err: &MetadataError) -> bool {
    matches!(
        err,
        MetadataError::Conflict(msg)
            if TENANT_JUNCTION_FKS.iter().any(|fk| msg.contains(
                &format!("violates foreign key constraint \"{fk}\"")
            ))
    )
}

/// `chunk_tenants` junction insert (`r[store.chunk.has-chunks-tenant]`):
/// record that `tenant_id` has seen every chunk in `chunk_hashes`.
/// Idempotent (`ON CONFLICT DO NOTHING`); a `None` tenant (dev mode,
/// service-token caller) writes nothing. `chunk_hashes` MUST be
/// pre-sorted ascending (`r[store.chunk.lock-order]` — the one caller
/// feeds `mark_manifest_chunks_durable`'s sorted output).
pub(crate) async fn insert_chunk_tenants_in_conn(
    conn: &mut sqlx::PgConnection,
    chunk_hashes: &[Vec<u8>],
    tenant_id: Option<uuid::Uuid>,
) -> Result<()> {
    let Some(tenant_id) = tenant_id else {
        return Ok(());
    };
    if chunk_hashes.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO chunk_tenants (blake3_hash, tenant_id) \
         SELECT h, $2 FROM UNNEST($1::bytea[]) AS u(h) \
         ON CONFLICT DO NOTHING",
    )
    .bind(chunk_hashes)
    .bind(tenant_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// [`insert_path_tenant_in_conn`] for callers inside a multi-statement
/// commit transaction, tolerating a tenant deleted while the build was
/// in flight: the insert runs under a savepoint, and the FK violation
/// on `path_tenants_tenant_id_fkey` (and ONLY that error — see
/// [`is_deleted_tenant_fk`]) rolls back to the savepoint and skips the
/// junction write instead of failing the upload that already verified.
/// A junction row for a deleted tenant is meaningless — the content
/// commit stays valid and the un-pinned path simply ages out via
/// normal GC retention. The savepoint is what keeps the surrounding
/// transaction usable after PG aborts the failed statement, so callers
/// MUST be inside a transaction (every caller today is a
/// manifest-completion transaction).
// r[impl store.castore.tenant-scope+3]
// r[impl store.put.tenant-junction]
pub(crate) async fn insert_path_tenant_skipping_deleted_in_tx(
    conn: &mut sqlx::PgConnection,
    store_path_hash: &[u8],
    tenant_id: Option<uuid::Uuid>,
) -> Result<()> {
    insert_tenant_junctions_skipping_deleted_in_tx(conn, store_path_hash, &[], tenant_id).await
}

/// [`insert_path_tenant_skipping_deleted_in_tx`] plus the chunk-level
/// visibility rows (`r[store.chunk.has-chunks-tenant]`): one savepoint
/// covers BOTH junction inserts so a tenant deleted mid-build skips
/// path and chunk visibility together — never a half-bound tenant.
/// `chunk_hashes` is the manifest's deduped sorted chunk set (from
/// `mark_manifest_chunks_durable`); empty for inline manifests and for
/// the legacy callers that only bind the path junction.
// r[impl store.castore.tenant-scope+3]
// r[impl store.put.tenant-junction]
// r[impl store.chunk.has-chunks-tenant]
pub(crate) async fn insert_tenant_junctions_skipping_deleted_in_tx(
    conn: &mut sqlx::PgConnection,
    store_path_hash: &[u8],
    chunk_hashes: &[Vec<u8>],
    tenant_id: Option<uuid::Uuid>,
) -> Result<()> {
    if tenant_id.is_none() {
        return Ok(());
    }
    sqlx::query("SAVEPOINT path_tenant_junction")
        .execute(&mut *conn)
        .await?;
    let inserts = async {
        insert_path_tenant_in_conn(conn, store_path_hash, tenant_id).await?;
        insert_chunk_tenants_in_conn(conn, chunk_hashes, tenant_id).await
    };
    match inserts.await {
        Err(e) if is_deleted_tenant_fk(&e) => {
            warn!(
                store_path_hash = hex::encode(store_path_hash),
                tenant_id = %tenant_id.expect("checked non-None above"),
                "tenant junctions skipped: tenant was deleted while the build was in flight"
            );
            sqlx::query("ROLLBACK TO SAVEPOINT path_tenant_junction")
                .execute(&mut *conn)
                .await?;
            Ok(())
        }
        Ok(()) => {
            sqlx::query("RELEASE SAVEPOINT path_tenant_junction")
                .execute(&mut *conn)
                .await?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    /// The commit-time presence proof: a chunk is committable iff its
    /// row exists, is not GC-claimed (`deleted`), and either a prior
    /// commit confirmed its S3 object (`uploaded_at`) or this stream
    /// just PUT it. Everything else must surface as unproven so the
    /// caller aborts instead of flipping `durable = TRUE` for an
    /// object the drain may already have deleted.
    // r[verify store.chunk.durable-flag]
    #[tokio::test]
    async fn lock_chunks_for_commit_rejects_unprovable_objects() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Five digests, sorted by construction (0x01 < … < 0x05).
        let healthy = vec![0x01u8; 32]; // uploaded_at set, not deleted
        let swept = vec![0x02u8; 32]; // deleted = TRUE (GC claimed it)
        let unconfirmed = vec![0x03u8; 32]; // uploaded_at NULL, not ours
        let ours = vec![0x04u8; 32]; // uploaded_at NULL, PUT by this stream
        let missing = vec![0x05u8; 32]; // no row at all
        for (hash, deleted, uploaded) in [
            (&healthy, false, true),
            (&swept, true, false),
            (&unconfirmed, false, false),
            (&ours, false, false),
        ] {
            sqlx::query(
                "INSERT INTO chunks (blake3_hash, refcount, size, deleted, uploaded_at) \
                 VALUES ($1, 0, 64, $2, CASE WHEN $3 THEN now() END)",
            )
            .bind(hash)
            .bind(deleted)
            .bind(uploaded)
            .execute(&db.pool)
            .await
            .unwrap();
        }

        let all = vec![
            healthy.clone(),
            swept.clone(),
            unconfirmed.clone(),
            ours.clone(),
            missing.clone(),
        ];
        let uploaded_by_stream = std::collections::HashSet::from([[0x04u8; 32]]);

        let mut conn = db.pool.acquire().await.unwrap();
        let unproven = lock_chunks_for_commit(&mut conn, &all, &uploaded_by_stream)
            .await
            .unwrap();
        assert_eq!(
            unproven,
            vec![swept, unconfirmed, missing],
            "healthy and just-uploaded chunks are provable; GC-claimed, \
             never-confirmed, and missing rows are not"
        );
    }

    /// Assert the given chunk row is currently row-locked by another
    /// transaction: a fresh connection's `FOR UPDATE NOWAIT` must fail
    /// with SQLSTATE 55P03 (lock_not_available).
    async fn assert_chunk_row_locked(pool: &PgPool, hash: &[u8]) {
        let mut probe = pool.acquire().await.unwrap();
        let err = sqlx::query("SELECT 1 FROM chunks WHERE blake3_hash = $1 FOR UPDATE NOWAIT")
            .bind(hash)
            .fetch_optional(&mut *probe)
            .await
            .expect_err("chunk row should be locked by the in-flight commit transaction");
        let code = match &err {
            sqlx::Error::Database(e) => e.code().map(|c| c.to_string()),
            _ => None,
        };
        assert_eq!(
            code.as_deref(),
            Some("55P03"),
            "expected lock_not_available probing {}, got {err:?}",
            hex::encode(hash)
        );
    }

    // r[verify store.chunk.lock-order]
    /// The commit transaction's final statement stamps `uploaded_at`
    /// for every chunk this stream PUT — including digests whose only
    /// manifest occurrence is in an idempotent-skipped output and which
    /// therefore appear in `uploaded_by_this_stream` but NOT in the
    /// manifest-digest union. Those rows MUST be part of the up-front
    /// sorted FOR UPDATE: locking them for the first time at the end of
    /// the transaction breaks the sorted acquisition order and re-opens
    /// the ABBA-deadlock window against concurrent sorted writers.
    #[tokio::test]
    async fn lock_chunks_for_commit_locks_uploaded_only_digests() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let manifest_digest = [0x0Du8; 32]; // referenced by a non-skipped output
        let uploaded_only = [0x0Cu8; 32]; // PUT by this stream; skipped-output only
        for hash in [&manifest_digest, &uploaded_only] {
            sqlx::query("INSERT INTO chunks (blake3_hash, refcount, size) VALUES ($1, 0, 64)")
                .bind(hash.as_slice())
                .execute(&db.pool)
                .await
                .unwrap();
        }

        let mut tx = db.pool.begin().await.unwrap();
        let unproven = lock_chunks_for_commit(
            &mut tx,
            &[manifest_digest.to_vec()],
            &std::collections::HashSet::from([uploaded_only, manifest_digest]),
        )
        .await
        .unwrap();
        assert!(
            unproven.is_empty(),
            "both digests were PUT by this stream, got {unproven:?}"
        );

        // From a second connection, BOTH rows must already be locked —
        // the manifest digest and the uploaded-only one.
        assert_chunk_row_locked(&db.pool, manifest_digest.as_slice()).await;
        assert_chunk_row_locked(&db.pool, uploaded_only.as_slice()).await;
        tx.rollback().await.unwrap();
    }

    // r[verify store.chunk.lock-order]
    /// `PutPathBatch`'s phase-3 pre-lock: the union of every staged
    /// output's `manifest_data` chunk digests is locked up front in one
    /// sorted FOR UPDATE, so the per-output `durable` flips that follow
    /// only touch already-held rows (cross-output completion order is
    /// output-index order, not digest order — without the pre-lock two
    /// concurrent batches sharing freshly-staged chunks can
    /// ABBA-deadlock after verification already succeeded). Healthy
    /// staged chunks (uploaded_at set, not deleted) are proven; a
    /// GC-claimed one is reported unproven so the batch aborts
    /// retryably instead of committing a manifest that lies about S3
    /// presence.
    #[tokio::test]
    async fn lock_staged_chunks_for_commit_locks_batch_union() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Two staged outputs with overlapping chunk lists, in the state
        // phase-2 staging leaves them: uploaded (uploaded_at set), not
        // yet durable.
        let chunk_w = [0x21u8; 32];
        let chunk_x = [0x22u8; 32];
        let chunk_y = [0x23u8; 32];
        for hash in [&chunk_w, &chunk_x, &chunk_y] {
            sqlx::query(
                "INSERT INTO chunks (blake3_hash, refcount, size, uploaded_at) \
                 VALUES ($1, 1, 64, now())",
            )
            .bind(hash.as_slice())
            .execute(&db.pool)
            .await
            .unwrap();
        }
        let sph_a = vec![0x31u8; 32];
        let sph_b = vec![0x32u8; 32];
        for (sph, chunks) in [(&sph_a, [chunk_w, chunk_x]), (&sph_b, [chunk_x, chunk_y])] {
            seed_placeholder(&db.pool, sph).await;
            let list = crate::manifest::Manifest {
                entries: chunks
                    .iter()
                    .map(|h| crate::manifest::ManifestEntry { hash: *h, size: 64 })
                    .collect(),
            }
            .serialize();
            sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
                .bind(sph)
                .bind(&list)
                .execute(&db.pool)
                .await
                .unwrap();
        }

        let mut tx = db.pool.begin().await.unwrap();
        let unproven = lock_staged_chunks_for_commit(&mut tx, &[sph_a.clone(), sph_b.clone()])
            .await
            .unwrap();
        assert!(
            unproven.is_empty(),
            "healthy staged chunks are provable, got {unproven:?}"
        );
        // Every chunk referenced by EITHER output is row-locked before
        // any per-output completion runs.
        assert_chunk_row_locked(&db.pool, chunk_w.as_slice()).await;
        assert_chunk_row_locked(&db.pool, chunk_x.as_slice()).await;
        assert_chunk_row_locked(&db.pool, chunk_y.as_slice()).await;
        tx.rollback().await.unwrap();

        // A staged chunk the GC claimed between staging and commit is
        // reported unproven.
        sqlx::query("UPDATE chunks SET deleted = TRUE, uploaded_at = NULL WHERE blake3_hash = $1")
            .bind(chunk_y.as_slice())
            .execute(&db.pool)
            .await
            .unwrap();
        let mut tx2 = db.pool.begin().await.unwrap();
        let unproven = lock_staged_chunks_for_commit(&mut tx2, &[sph_a, sph_b])
            .await
            .unwrap();
        assert_eq!(
            unproven,
            vec![chunk_y.to_vec()],
            "a GC-claimed staged chunk must abort the batch commit"
        );
    }

    /// `insert_pending_chunks` must restart the grace clock and clear a
    /// GC claim on a refcount-0 row it is about to re-upload — without
    /// that, a retry of a swept upload re-PUTs the S3 object but leaves
    /// the row `deleted = TRUE`, so the commit-time presence check can
    /// never pass and the upload livelocks. A live (refcount > 0) row
    /// is left alone.
    #[tokio::test]
    async fn insert_pending_chunks_resurrects_swept_rows() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let swept = [0x0Au8; 32];
        let live = [0x0Bu8; 32];
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, deleted, created_at) \
             VALUES ($1, 0, 64, TRUE, now() - interval '1 hour'), \
                    ($2, 3, 64, FALSE, now() - interval '1 hour')",
        )
        .bind(swept.as_slice())
        .bind(live.as_slice())
        .execute(&db.pool)
        .await
        .unwrap();

        insert_pending_chunks(&db.pool, &[(swept, 64), (live, 64)])
            .await
            .unwrap();

        let (deleted, fresh): (bool, bool) = sqlx::query_as(
            "SELECT deleted, created_at > now() - interval '1 minute' \
             FROM chunks WHERE blake3_hash = $1",
        )
        .bind(swept.as_slice())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(!deleted, "the GC claim must be cleared for a re-upload");
        assert!(fresh, "the grace clock must restart for a re-upload");

        let (live_deleted, live_fresh): (bool, bool) = sqlx::query_as(
            "SELECT deleted, created_at > now() - interval '1 minute' \
             FROM chunks WHERE blake3_hash = $1",
        )
        .bind(live.as_slice())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(!live_deleted);
        assert!(
            !live_fresh,
            "a refcount > 0 row is owned by other manifests; leave it alone"
        );
    }

    /// `mark_chunks_uploaded` must not stamp `uploaded_at` on a row the
    /// orphan sweep tombstoned mid-staging: the drain may already be
    /// deleting the S3 object, and a stamped-then-resurrected row would
    /// skip the re-PUT and let a manifest point at a drained object. A
    /// live (not-deleted) row in the same batch is still stamped.
    // r[verify store.cas.chunk-upload-committed]
    #[tokio::test]
    async fn mark_chunks_uploaded_skips_tombstoned_rows() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let tombstoned = vec![0xE1u8; 32];
        let live = vec![0xE2u8; 32];
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, deleted) \
             VALUES ($1, 0, 64, TRUE), ($2, 0, 64, FALSE)",
        )
        .bind(&tombstoned)
        .bind(&live)
        .execute(&db.pool)
        .await
        .unwrap();

        mark_chunks_uploaded(&db.pool, &[tombstoned.clone(), live.clone()])
            .await
            .unwrap();

        let stamped: Vec<(Vec<u8>, bool)> = sqlx::query_as(
            "SELECT blake3_hash, uploaded_at IS NOT NULL FROM chunks \
             WHERE blake3_hash = ANY($1) ORDER BY blake3_hash",
        )
        .bind(vec![tombstoned.clone(), live.clone()])
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            stamped,
            vec![(tombstoned, false), (live, true)],
            "a sweep-tombstoned row keeps uploaded_at NULL; the live row is stamped"
        );
    }

    /// PG constraint: duplicate hashes in the UNNEST batch → PG error
    /// "ON CONFLICT DO UPDATE command cannot affect row a second time".
    /// cas.rs put_chunked dedups before calling this. This test documents
    /// the PG behavior that motivates the dedup.
    #[tokio::test]
    async fn upgrade_duplicate_hashes_pg_rejects() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed placeholder (upgrade requires an existing 'uploading'
        // manifest row).
        let store_path_hash = vec![0xAAu8; 32];
        let path = rio_test_support::fixtures::test_store_path("dup-chunks");
        crate::metadata::insert_manifest_uploading(&db.pool, &store_path_hash, &path, &[])
            .await
            .unwrap();

        // Duplicate hashes: same BLAKE3 twice. FastCDC can produce
        // this for identical content blocks (e.g., zero-filled pages).
        let dup_hash = vec![0xBBu8; 32];
        let chunk_hashes = vec![dup_hash.clone(), dup_hash.clone()];
        let chunk_sizes = vec![1024i64, 1024i64];

        let result = upgrade_manifest_to_chunked(
            &db.pool,
            &store_path_hash,
            b"dummy-chunk-list",
            &chunk_hashes,
            &chunk_sizes,
        )
        .await;

        // PG rejects with SQLSTATE 21000 (cardinality violation).
        // This documents WHY cas.rs must dedup before calling us.
        assert!(
            result.is_err(),
            "duplicate hashes in UNNEST batch MUST be rejected by PG"
        );
        let err = result.unwrap_err();
        let err_str = format!("{err}");
        assert!(
            err_str.contains("affect row a second time") || err_str.contains("21000"),
            "expected PG cardinality violation, got: {err_str}"
        );
    }

    /// Deduped hashes → upgrade succeeds. Proves the cas.rs dedup is correct.
    #[tokio::test]
    async fn upgrade_deduped_hashes_ok() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let store_path_hash = vec![0xCCu8; 32];
        let path = rio_test_support::fixtures::test_store_path("deduped");
        crate::metadata::insert_manifest_uploading(&db.pool, &store_path_hash, &path, &[])
            .await
            .unwrap();

        // One unique hash (cas.rs's dedup would produce this from
        // the duplicate input above).
        let chunk_hashes = vec![vec![0xDDu8; 32]];
        let chunk_sizes = vec![1024i64];

        let _ = upgrade_manifest_to_chunked(
            &db.pool,
            &store_path_hash,
            b"dummy-chunk-list",
            &chunk_hashes,
            &chunk_sizes,
        )
        .await
        .expect("deduped hashes should succeed");

        // refcount = 1 (one unique hash).
        let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk_hashes[0])
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc, 1, "deduped insert → refcount = 1");
    }

    /// Seed an 'uploading' placeholder for the given store-path hash.
    /// Path string is synthesized from the first hash byte (distinct
    /// enough for unit tests; the narinfo placeholder just needs a
    /// valid-shaped path). Returns the M_052 claim_id.
    async fn seed_placeholder(pool: &PgPool, store_path_hash: &[u8]) -> uuid::Uuid {
        let b = store_path_hash[0];
        let path = format!("/nix/store/{}-p208-test", format!("{b:02x}").repeat(16));
        crate::metadata::insert_manifest_uploading(pool, store_path_hash, &path, &[])
            .await
            .unwrap()
            .expect("fresh path → placeholder inserted")
    }

    /// Sequential upserts simulate two PutPaths: first inserts {A,B},
    /// uploads, marks-uploaded; second inserts {A,C}. The needs_upload
    /// set is driven by `uploaded_at`, not refcount.
    ///
    /// First call: A,B both new (uploaded_at NULL) → both need upload.
    /// After mark_chunks_uploaded({A,B}): A,B uploaded_at set.
    /// Second call: A uploaded_at set → NOT in set. C new → in set.
    #[tokio::test]
    async fn upsert_returning_sequential_needs_upload_set() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk_a = vec![0xA1u8; 32];
        let chunk_b = vec![0xB2u8; 32];
        let chunk_c = vec![0xC3u8; 32];

        // --- First upsert: {A, B} ---
        let sph1 = vec![0x11u8; 32];
        seed_placeholder(&db.pool, &sph1).await;
        let (need1, _) = upgrade_manifest_to_chunked(
            &db.pool,
            &sph1,
            b"manifest-1",
            &[chunk_a.clone(), chunk_b.clone()],
            &[1024, 2048],
        )
        .await
        .unwrap();

        assert_eq!(need1.len(), 2, "first upsert: both A and B need upload");
        assert!(need1.contains(&chunk_a), "A: uploaded_at NULL");
        assert!(need1.contains(&chunk_b), "B: uploaded_at NULL");

        // Simulate the S3 upload + commit point.
        mark_chunks_uploaded(&db.pool, &[chunk_a.clone(), chunk_b.clone()])
            .await
            .unwrap();

        // --- Second upsert: {A, C} ---
        // A is at refcount=1, uploaded_at set → bumps to 2, but
        // uploaded_at IS NOT NULL → NOT in needs_upload. C is fresh.
        let sph2 = vec![0x22u8; 32];
        seed_placeholder(&db.pool, &sph2).await;
        let (need2, _) = upgrade_manifest_to_chunked(
            &db.pool,
            &sph2,
            b"manifest-2",
            &[chunk_a.clone(), chunk_c.clone()],
            &[1024, 4096],
        )
        .await
        .unwrap();

        assert_eq!(need2.len(), 1, "second upsert: only C needs upload");
        assert!(
            !need2.contains(&chunk_a),
            "A uploaded_at set — NOT in needs_upload set"
        );
        assert!(need2.contains(&chunk_c), "C: uploaded_at NULL");

        // Ground truth: refcounts as expected.
        let rc_a: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk_a)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc_a, 2, "A referenced by two manifests");
    }

    /// The resurrection case (Audit B1 #8): chunk at refcount=0,
    /// deleted=true, uploaded_at NULL — the post-sweep, pre-drain
    /// state (`decrement_and_enqueue` clears uploaded_at when it sets
    /// deleted). An upsert resurrects it — refcount 0→1, deleted
    /// flips false, uploaded_at stays NULL → MUST be in needs_upload.
    /// S3 may have already deleted the object between sweep and now.
    #[tokio::test]
    async fn upsert_returning_resurrection_needs_upload() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk = vec![0xDEu8; 32];

        // Seed the chunk at refcount=0, deleted=true, uploaded_at NULL
        // — the post-sweep, pre-drain state. Directly INSERT (bypassing
        // the upsert path) to set up the exact precondition.
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, deleted) \
             VALUES ($1, 0, 1024, true)",
        )
        .bind(&chunk)
        .execute(&db.pool)
        .await
        .unwrap();

        // Precondition: confirm the seeded state.
        let (rc0, del0, up0): (i32, bool, bool) = sqlx::query_as(
            "SELECT refcount, deleted, (uploaded_at IS NOT NULL) FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(rc0, 0, "precondition: refcount=0 (soft-deleted)");
        assert!(del0, "precondition: deleted=true (awaiting drain)");
        assert!(!up0, "precondition: uploaded_at NULL");

        // Upsert resurrects: ON CONFLICT → refcount 0+1=1, deleted=false.
        let sph = vec![0xDDu8; 32];
        seed_placeholder(&db.pool, &sph).await;
        let (need, _) = upgrade_manifest_to_chunked(
            &db.pool,
            &sph,
            b"manifest-resurrect",
            std::slice::from_ref(&chunk),
            &[1024],
        )
        .await
        .unwrap();

        // THE KEY ASSERTION: resurrected chunk IS in needs_upload.
        assert!(
            need.contains(&chunk),
            "resurrected chunk (uploaded_at NULL) MUST be in needs_upload \
             — S3 may have already deleted it"
        );

        // Ground truth: refcount=1, deleted=false.
        let (rc, del): (i32, bool) =
            sqlx::query_as("SELECT refcount, deleted FROM chunks WHERE blake3_hash = $1")
                .bind(&chunk)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(rc, 1, "resurrected: 0→1");
        assert!(!del, "resurrected: deleted flipped false");
    }

    // r[verify store.cas.upsert-inserted+2]
    /// True-concurrent upserts via `tokio::join!`: two PutPaths share
    /// one chunk hash. Neither has called `mark_chunks_uploaded` yet,
    /// so BOTH see uploaded_at IS NULL → both get the shared chunk in
    /// their needs_upload set. S3 PutObject is idempotent — both
    /// uploading the same bytes to the same key is wasted bandwidth,
    /// not a correctness hazard.
    ///
    /// This is intentionally weaker than the old XOR property
    /// (`refcount = 1` gave exactly-one-uploader). The trade is one
    /// duplicate PUT under contention vs. surviving SIGKILL of the
    /// first uploader — see `sigkill_race_second_uploader_covers`.
    ///
    /// The assertion is `a_has && b_has` — both sides upload regardless
    /// of which won the ON CONFLICT serialization.
    #[tokio::test]
    async fn upsert_returning_concurrent_both_need_upload() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let shared = vec![0x5Au8; 32]; // the contested chunk
        let unique_a = vec![0xA0u8; 32];
        let unique_b = vec![0xB0u8; 32];

        // Two store paths (separate placeholders — no contention there).
        let sph_a = vec![0xAAu8; 32];
        let sph_b = vec![0xBBu8; 32];
        seed_placeholder(&db.pool, &sph_a).await;
        seed_placeholder(&db.pool, &sph_b).await;

        // Chunk-array bindings must outlive the join! — inline
        // `&[...]` temporaries drop at end-of-statement, but the
        // futures borrow them across await points.
        let hashes_a = [shared.clone(), unique_a.clone()];
        let hashes_b = [shared.clone(), unique_b.clone()];
        let sizes_a = [1024i64, 2048];
        let sizes_b = [1024i64, 4096];

        // PgPool hands out distinct connections for concurrent calls;
        // each upgrade_manifest_to_chunked runs in its own tx.
        //
        // Under READ COMMITTED (PG default), ON CONFLICT is special-
        // cased: if tx A inserts the shared row and tx B tries to
        // insert the same PK before A commits, B BLOCKS on A's row
        // lock. Once A commits, B re-reads the committed row and runs
        // the UPDATE clause (refcount 1→2). Both see uploaded_at NULL.
        let (need_a, need_b) = tokio::join!(
            upgrade_manifest_to_chunked(&db.pool, &sph_a, b"manifest-a", &hashes_a, &sizes_a),
            upgrade_manifest_to_chunked(&db.pool, &sph_b, b"manifest-b", &hashes_b, &sizes_b),
        );
        let (need_a, _) = need_a.unwrap();
        let (need_b, _) = need_b.unwrap();

        // Each side's unique chunk is always fresh.
        assert!(need_a.contains(&unique_a), "A's unique chunk needs upload");
        assert!(need_b.contains(&unique_b), "B's unique chunk needs upload");

        // THE KEY ASSERTION: both sides see the shared chunk as
        // needs_upload. Neither has called mark_chunks_uploaded yet,
        // so uploaded_at is NULL for both reads. Idempotent S3 PUT
        // makes the duplicate upload harmless; the alternative
        // (exactly-one via refcount=1) loses data when the winner is
        // SIGKILLed mid-upload.
        let a_has = need_a.contains(&shared);
        let b_has = need_b.contains(&shared);
        assert!(
            a_has && b_has,
            "both concurrent upserts see shared chunk as needs_upload \
             (got A={a_has}, B={b_has}; either-false = M033 regression)"
        );

        // Ground truth: final refcount = 2.
        let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&shared)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc, 2, "shared chunk referenced by both manifests");
    }

    // r[verify store.cas.chunk-upload-committed]
    /// The SIGKILL race that motivated `uploaded_at` (M_033):
    ///
    /// 1. PutPath A: upsert chunk X (rc=1, uploaded_at NULL). Starts
    ///    upload. Process is SIGKILLed (helm rolling update) — no
    ///    rollback runs, no mark_chunks_uploaded runs. Manifest A
    ///    left at status='uploading'.
    /// 2. PutPath B (different path, same chunk X): upsert (rc=2).
    ///    Under the OLD `(refcount=1)` heuristic, B would skip upload
    ///    here — permanent data loss (X never reaches S3, rc never
    ///    drops to 0 once B completes). Under `uploaded_at IS NULL`,
    ///    B uploads.
    /// 3. B's upload succeeds → mark_chunks_uploaded → uploaded_at
    ///    set. Manifest B completes.
    /// 4. PutPath C (third path, same chunk X): upsert (rc=3,
    ///    uploaded_at set) → skips upload. Correct dedup.
    ///
    /// We don't run a real `cas::put_chunked` for A — just the upsert
    /// step, then nothing (the SIGKILL happens before any further PG
    /// or S3 write, so dropping the future after the upsert tx commits
    /// is the exact post-kill state).
    #[tokio::test]
    async fn sigkill_race_second_uploader_covers() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk_x = vec![0x51u8; 32];
        let one_chunk = std::slice::from_ref(&chunk_x);
        // Real serialized chunk_list — `reap_one` deserializes it to
        // know which chunks to decrement. All three manifests share
        // the same single-chunk list.
        let chunk_list = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: chunk_x.as_slice().try_into().unwrap(),
                size: 1024,
            }],
        }
        .serialize();

        // --- Step 1: PutPath A's upsert, then SIGKILL ---
        let sph_a = vec![0xAAu8; 32];
        let claim_a = seed_placeholder(&db.pool, &sph_a).await;
        let (need_a, _) =
            upgrade_manifest_to_chunked(&db.pool, &sph_a, &chunk_list, one_chunk, &[1024])
                .await
                .unwrap();
        assert!(need_a.contains(&chunk_x), "A: fresh insert needs upload");
        // SIGKILL: drop here. PG state committed (rc=1, uploaded_at
        // NULL, manifest A status='uploading'), S3 has nothing.

        // --- Step 2+3: PutPath B sees needs_upload, uploads, marks ---
        let sph_b = vec![0xBBu8; 32];
        seed_placeholder(&db.pool, &sph_b).await;
        let (need_b, _) =
            upgrade_manifest_to_chunked(&db.pool, &sph_b, &chunk_list, one_chunk, &[1024])
                .await
                .unwrap();
        assert!(
            need_b.contains(&chunk_x),
            "B: rc=2 but uploaded_at NULL → needs upload \
             (refcount-based heuristic would skip here → data loss)"
        );
        // B uploads to S3 (omitted — backend.put is idempotent), then:
        mark_chunks_uploaded(&db.pool, one_chunk).await.unwrap();

        let (rc, up): (i32, bool) = sqlx::query_as(
            "SELECT refcount, (uploaded_at IS NOT NULL) FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk_x)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(rc, 2, "A leaked + B = 2");
        assert!(up, "B's mark_chunks_uploaded set uploaded_at");

        // --- Step 4: PutPath C dedups against B's confirmed upload ---
        let sph_c = vec![0xCCu8; 32];
        seed_placeholder(&db.pool, &sph_c).await;
        let (need_c, _) =
            upgrade_manifest_to_chunked(&db.pool, &sph_c, &chunk_list, one_chunk, &[1024])
                .await
                .unwrap();
        assert!(
            !need_c.contains(&chunk_x),
            "C: uploaded_at set → skip upload (dedup works post-commit)"
        );

        // --- Epilogue: orphan reaper cleans manifest A ---
        // reap_one decrements rc 3→2. uploaded_at stays set (B's
        // upload is real). A future PutPath still dedups correctly.
        let no_backend: Option<&std::sync::Arc<dyn crate::backend::ChunkBackend>> = None;
        let reaped = crate::gc::orphan::reap_one(
            &db.pool,
            &sph_a,
            crate::gc::orphan::ReapBy::Claim(claim_a),
            no_backend,
        )
        .await
        .unwrap();
        assert!(reaped, "A's stale 'uploading' placeholder reaped");
        let (rc, up): (i32, bool) = sqlx::query_as(
            "SELECT refcount, (uploaded_at IS NOT NULL) FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk_x)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(rc, 2, "reaper decremented A's leaked ref");
        assert!(
            up,
            "reaper does NOT clear uploaded_at when rc>0 — B+C still reference it"
        );
    }

    /// Regression: concurrent rollback (production path) vs another
    /// chunk writer on overlapping hashes MUST NOT deadlock.
    ///
    /// PG semantics: a single-statement `UPDATE ... WHERE blake3_hash
    /// = ANY($1)` evaluates the qual at the scan node and row-locks in
    /// SCAN order (btree PK ascending), NOT in `$1` array order. So
    /// the previous fwd/rev two-rollback shape was theatre — both
    /// sides locked ascending regardless of `with_sorted_retry`'s
    /// sort. The sort is observable only when at least one writer
    /// row-locks in ITERATION order, i.e. issues one statement per
    /// key inside one txn. The per-row contender below models that.
    ///
    /// Both sides go through `with_sorted_retry`. With its sort in
    /// place, both bodies see ascending input → no circular wait → 1
    /// attempt each. Mutation-tested: removing the sort in
    /// `with_sorted_retry` makes the contender (fed reversed input)
    /// lock descending while rollback's batch UPDATE locks ascending
    /// → 40P01 → one side retries → attempts==3 → fails here. The 5s
    /// timeout backstops PG's deadlock detector (1s default).
    // r[verify store.chunk.refcount-txn]
    // r[verify store.put.wal-manifest]
    // r[verify store.chunk.lock-order]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rollback_overlapping_no_deadlock() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use tokio::time::timeout;

        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed 100 chunks at refcount=1 via one manifest. 100 (vs 50)
        // widens the per-row contender's window — 100 sequential PG
        // roundtrips ~guarantees overlap with the batch UPDATE.
        let hashes: Vec<Vec<u8>> = (0u8..100).map(|i| vec![i; 32]).collect();
        let sizes: Vec<i64> = vec![1024; 100];
        let sph_a = vec![0xAAu8; 32];
        seed_placeholder(&db.pool, &sph_a).await;
        let (_, token_a) = upgrade_manifest_to_chunked(&db.pool, &sph_a, b"ml-a", &hashes, &sizes)
            .await
            .unwrap();

        // Per-row contender: locks in `sorted` ARRAY order (one UPDATE
        // per hash, all in one tx). No-op write — row-locks
        // unconditionally. Stands in for "any chunk writer obeying
        // r[store.chunk.lock-order] that walks its hash list under
        // one tx".
        async fn contend_per_row(pool: &PgPool, sorted: &[Vec<u8>]) -> crate::metadata::Result<()> {
            let mut tx = pool.begin().await?;
            for h in sorted {
                sqlx::query("UPDATE chunks SET size = size WHERE blake3_hash = $1")
                    .bind(h)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            Ok(())
        }

        // Side A: production rollback body (`_inner` directly so the
        // closure owns the attempt counter). Fed forward order.
        // Side B: per-row contender. Fed REVERSED — pathological
        // input that the helper's sort must canonicalise.
        let hashes_fwd = hashes.clone();
        let mut hashes_rev = hashes.clone();
        hashes_rev.reverse();

        let pool_a = db.pool.clone();
        let pool_b = db.pool.clone();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_a = Arc::clone(&attempts);
        let attempts_b = Arc::clone(&attempts);

        let task_a = tokio::spawn(async move {
            crate::metadata::with_sorted_retry(hashes_fwd, move |sorted| {
                attempts_a.fetch_add(1, Ordering::Relaxed);
                let pool_a = pool_a.clone();
                let sph_a = sph_a.clone();
                async move {
                    delete_manifest_chunked_uploading_inner(&pool_a, &sph_a, token_a, &sorted).await
                }
            })
            .await
        });
        let task_b = tokio::spawn(async move {
            crate::metadata::with_sorted_retry(hashes_rev, move |sorted| {
                attempts_b.fetch_add(1, Ordering::Relaxed);
                let pool_b = pool_b.clone();
                async move { contend_per_row(&pool_b, &sorted).await }
            })
            .await
        });

        let (ra, rb) = timeout(Duration::from_secs(5), async {
            tokio::try_join!(task_a, task_b).expect("tasks should not panic")
        })
        .await
        .expect("concurrent rollback+contender must complete within 5s — deadlock detected");

        ra.expect("rollback should succeed");
        rb.expect("contender should succeed");

        // Mutation sentinel: with_sorted_retry's sort means both sides
        // lock ascending → no 40P01 → no retry → exactly 2 body
        // invocations total. Removing the sort makes this 3 (one side
        // 40P01s, helper retries). Mutation-tested locally: sort
        // removed → attempts==3 → fails here.
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            2,
            "with_sorted_retry sort should prevent 40P01 (no retry needed)"
        );

        // Vacuity sentinel: rollback's UPDATE must have matched and
        // decremented all 100. If a future seed regression makes the
        // UPDATE match zero rows, this fails loudly instead of going
        // vacuous again.
        let sum: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(refcount),0) FROM chunks WHERE blake3_hash = ANY($1)",
        )
        .bind(&hashes)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(sum, 0, "rollback decremented all 100 chunks to zero");
    }

    /// I-040 chain, post-M033: the inline `delete_manifest_uploading`
    /// on a chunked placeholder still leaks refcounts (it doesn't
    /// decrement — correct for inline placeholders, wrong for chunked),
    /// but the leak NO LONGER causes upload-skip on retry. The retry's
    /// upsert sees `uploaded_at IS NULL` and re-uploads regardless of
    /// the leaked refcount.
    ///
    /// substitute.rs's call site still uses `gc::orphan::reap_one`
    /// (which DOES decrement) for refcount hygiene; this test asserts
    /// that even if a future caller gets that wrong, the data-loss
    /// chain stays broken at the upsert level.
    #[tokio::test]
    async fn i040_inline_delete_leaked_refcount_still_reuploads() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk = vec![0x40u8; 32];
        let path = rio_test_support::fixtures::test_store_path("i040-chain");
        // path-derived hash (matches what real callers compute) so the
        // narinfo placeholder's store_path column round-trips.
        let sph = rio_nix::store_path::StorePath::parse(&path)
            .unwrap()
            .sha256_digest()
            .to_vec();

        // --- Step 1: prior upload's upgrade_manifest_to_chunked ---
        // Simulates: cas::put_chunked got past upgrade, then crashed.
        crate::metadata::insert_manifest_uploading(&db.pool, &sph, &path, &[])
            .await
            .unwrap();
        let chunk_list = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: chunk.as_slice().try_into().unwrap(),
                size: 100,
            }],
        }
        .serialize();
        let one_chunk = std::slice::from_ref(&chunk);
        let (ins1, _) =
            upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[100i64])
                .await
                .unwrap();
        assert!(ins1.contains(&chunk), "step 1: chunk fresh → would upload");
        // Crash here: chunk MAY or may not have made it to S3. PG state
        // is committed (refcount=1).

        // --- Step 2: inline delete (the I-040 bug path) ---
        // This deletes manifests (CASCADE → manifest_data) but does NOT
        // touch chunk refcounts. Correct for inline placeholders, WRONG
        // for chunked — substitute.rs called this unconditionally.
        crate::metadata::delete_manifest_uploading(&db.pool, &sph)
            .await
            .unwrap();

        let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            rc, 1,
            "step 2: refcount LEAKED at 1 (inline delete ≠ decrement)"
        );

        // --- Step 3: retry's upgrade_manifest_to_chunked ---
        crate::metadata::insert_manifest_uploading(&db.pool, &sph, &path, &[])
            .await
            .unwrap();
        let (need2, _) =
            upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[100i64])
                .await
                .unwrap();

        // POST-M033: refcount went 1→2 (leak), but uploaded_at is
        // still NULL (step 1 never reached mark_chunks_uploaded) →
        // chunk IS in needs_upload → do_upload re-uploads. Data-loss
        // chain broken at the upsert.
        assert!(
            need2.contains(&chunk),
            "step 3: leaked refcount but uploaded_at NULL → re-upload \
             (data-loss chain broken regardless of call-site hygiene)"
        );

        let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc, 2, "1 leaked + 1 real = 2");
    }

    /// Mismatched parallel-array lengths → InvariantViolation BEFORE
    /// the zip truncates. Previously the zip silently dropped trailing
    /// chunks and PG never saw a mismatch (the "PG errors if lengths
    /// differ" comment was false).
    #[tokio::test]
    async fn upgrade_mismatched_lengths_rejected() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let sph = vec![0x77u8; 32];
        seed_placeholder(&db.pool, &sph).await;

        let hashes = vec![vec![0xA0; 32], vec![0xA1; 32], vec![0xA2; 32]];
        let sizes = vec![1024i64, 2048]; // len 2 ≠ 3

        let err = upgrade_manifest_to_chunked(&db.pool, &sph, b"ml", &hashes, &sizes)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, MetadataError::InvariantViolation(s) if s.contains("length mismatch")),
            "expected InvariantViolation(length mismatch), got {err:?}"
        );

        // Tx rolled back: no manifest_data row written.
        let md: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT chunk_list FROM manifest_data WHERE store_path_hash = $1")
                .bind(&sph)
                .fetch_optional(&db.pool)
                .await
                .unwrap();
        assert!(md.is_none(), "manifest_data must NOT be inserted on reject");
    }

    // r[verify store.put.wal-manifest]
    /// A's late rollback after reaper reclaimed A's placeholder AND B
    /// re-uploaded + completed the same path MUST be a no-op.
    /// Previously: the unguarded refcount decrement + manifest_data
    /// DELETE clobbered B's committed state (rc 1→0, chunk_list gone,
    /// `get_manifest` → InvariantViolation).
    #[tokio::test]
    async fn rollback_after_reap_and_reupload_is_noop() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let path = rio_test_support::fixtures::test_store_path("reap-reupload");
        let sph = rio_nix::store_path::StorePath::parse(&path)
            .unwrap()
            .sha256_digest()
            .to_vec();
        let chunk = vec![0x9Au8; 32];
        let one_chunk = std::slice::from_ref(&chunk);
        let chunk_list = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: chunk.as_slice().try_into().unwrap(),
                size: 1024,
            }],
        }
        .serialize();

        // --- A: placeholder + upgrade (rc=1). Then A's PUT hangs. ---
        crate::metadata::insert_manifest_uploading(&db.pool, &sph, &path, &[])
            .await
            .unwrap();
        let (_, token_a) =
            upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[1024])
                .await
                .unwrap();

        // --- Reaper: reclaims A's stale placeholder (rc 1→0). ---
        let no_backend: Option<&std::sync::Arc<dyn crate::backend::ChunkBackend>> = None;
        let reaped = crate::gc::orphan::reap_one(
            &db.pool,
            &sph,
            crate::gc::orphan::ReapBy::Stale { secs: 0 },
            no_backend,
        )
        .await
        .unwrap();
        assert!(reaped, "reaper reclaims A's placeholder");

        // --- B: re-uploads same path, same chunk hash, completes. ---
        let claim_b = crate::metadata::insert_manifest_uploading(&db.pool, &sph, &path, &[])
            .await
            .unwrap()
            .unwrap();
        let _ = upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[1024])
            .await
            .unwrap();
        mark_chunks_uploaded(&db.pool, one_chunk).await.unwrap();
        let mut info = rio_test_support::fixtures::make_path_info(&path, &[0u8; 1024], [0x55; 32]);
        info.store_path_hash = sph.clone();
        complete_manifest_chunked(&db.pool, &info, claim_b, None, None)
            .await
            .unwrap();

        // --- A: hung PUT errors → late rollback fires. ---
        delete_manifest_chunked_uploading(&db.pool, &sph, token_a, one_chunk)
            .await
            .unwrap();

        // B's state MUST be intact: rc==1, manifest_data present,
        // get_manifest returns Chunked.
        let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc, 1, "B's refcount untouched by A's late rollback");

        let md: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT chunk_list FROM manifest_data WHERE store_path_hash = $1")
                .bind(&sph)
                .fetch_optional(&db.pool)
                .await
                .unwrap();
        assert!(md.is_some(), "B's manifest_data row survives");

        let kind = crate::metadata::queries::get_manifest_with_dag(&db.pool, &path)
            .await
            .unwrap()
            .map(|(kind, _dag)| kind);
        assert!(
            matches!(kind, Some(crate::metadata::ManifestKind::Chunked(_))),
            "get_manifest_with_dag still resolves B's chunked manifest, got {kind:?}"
        );
    }

    // r[verify store.put.wal-manifest]
    /// Residual-bypass coverage: reaper reclaims A → B inserts a FRESH
    /// `'uploading'` placeholder with IDENTICAL chunks (deterministic
    /// build) but does NOT complete yet → A's late rollback fires.
    /// Without the [`PlaceholderToken`] freshness guard, A's
    /// `status='uploading'` FOR UPDATE matches B's fresh row and
    /// clobbers it (rc 1→0, manifest_data + placeholder deleted).
    /// With the guard, A's stale token ≠ B's fresh `updated_at` →
    /// rollback is a no-op.
    #[tokio::test]
    async fn rollback_after_reap_and_fresh_reupload_mid_upload_is_noop() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let path = rio_test_support::fixtures::test_store_path("reap-reupload-mid");
        let sph = rio_nix::store_path::StorePath::parse(&path)
            .unwrap()
            .sha256_digest()
            .to_vec();
        let chunk = vec![0x9Bu8; 32];
        let one_chunk = std::slice::from_ref(&chunk);
        let chunk_list = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: chunk.as_slice().try_into().unwrap(),
                size: 1024,
            }],
        }
        .serialize();

        // --- A: placeholder + upgrade. PUT hangs >15min. ---
        crate::metadata::insert_manifest_uploading(&db.pool, &sph, &path, &[])
            .await
            .unwrap();
        let (_, token_a) =
            upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[1024])
                .await
                .unwrap();
        // Simulate staleness: backdate updated_at past STALE_THRESHOLD
        // so token_a ≠ B's fresh updated_at deterministically
        // (otherwise PG's timestamp granularity might collide).
        sqlx::query(
            "UPDATE manifests SET updated_at = now() - interval '1 hour' \
             WHERE store_path_hash = $1",
        )
        .bind(&sph)
        .execute(&db.pool)
        .await
        .unwrap();

        // --- Reaper: reclaims A. ---
        let no_backend: Option<&std::sync::Arc<dyn crate::backend::ChunkBackend>> = None;
        assert!(
            crate::gc::orphan::reap_one(
                &db.pool,
                &sph,
                crate::gc::orphan::ReapBy::Stale { secs: 0 },
                no_backend
            )
            .await
            .unwrap()
        );

        // --- B: fresh placeholder + upgrade, SAME chunks. STILL
        //     'uploading' (mid-upload). ---
        crate::metadata::insert_manifest_uploading(&db.pool, &sph, &path, &[])
            .await
            .unwrap();
        let (_, token_b) =
            upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[1024])
                .await
                .unwrap();
        assert_ne!(token_a, token_b, "B's fresh row has a distinct token");

        // --- A: hung PUT errors → late rollback with A's STALE token. ---
        delete_manifest_chunked_uploading(&db.pool, &sph, token_a, one_chunk)
            .await
            .unwrap();

        // B's mid-upload state MUST be intact: rc==1, manifest_data
        // present, manifests row still 'uploading'.
        let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc, 1, "B's refcount untouched by A's late rollback");

        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM manifests WHERE store_path_hash = $1")
                .bind(&sph)
                .fetch_optional(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            status.as_deref(),
            Some("uploading"),
            "B's 'uploading' placeholder survives A's late rollback"
        );

        let md: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT chunk_list FROM manifest_data WHERE store_path_hash = $1")
                .bind(&sph)
                .fetch_optional(&db.pool)
                .await
                .unwrap();
        assert!(md.is_some(), "B's manifest_data row survives");

        // --- Sanity: B's OWN rollback (correct token) DOES tear down. ---
        // This is the token-roundtrip happy path: float8 epoch
        // round-trips PG↔Rust exactly so an uploader CAN clean up its
        // own placeholder.
        delete_manifest_chunked_uploading(&db.pool, &sph, token_b, one_chunk)
            .await
            .unwrap();
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM manifests WHERE store_path_hash = $1")
                .bind(&sph)
                .fetch_optional(&db.pool)
                .await
                .unwrap();
        assert_eq!(status, None, "B's own rollback (matching token) tears down");
    }

    /// `upgrade_manifest_to_chunked` holds a FOR UPDATE lock on the
    /// placeholder, blocking until a competing FOR UPDATE tx commits.
    /// Previously: `SELECT EXISTS(...)` (no FOR UPDATE) returned
    /// immediately, so the reaper could delete + commit between the
    /// EXISTS and the manifest_data INSERT.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upgrade_holds_for_update_against_reaper() {
        use std::time::Duration;
        use tokio::sync::oneshot;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let sph = vec![0x88u8; 32];
        seed_placeholder(&db.pool, &sph).await;

        // Competing tx: lock the placeholder FOR UPDATE, hold ~100ms,
        // then commit. Stands in for `reap_one`'s FOR UPDATE.
        let pool_c = db.pool.clone();
        let sph_c = sph.clone();
        let (locked_tx, locked_rx) = oneshot::channel();
        let competitor = tokio::spawn(async move {
            let mut tx = pool_c.begin().await.unwrap();
            sqlx::query(
                "SELECT 1 FROM manifests \
                 WHERE store_path_hash = $1 AND status = 'uploading' FOR UPDATE",
            )
            .bind(&sph_c)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
            // Signal: lock held. Upgrade may now start (and must block).
            let _ = locked_tx.send(());
            tokio::time::sleep(Duration::from_millis(100)).await;
            tx.commit().await.unwrap();
        });

        // Wait until the competitor holds the lock, THEN start upgrade.
        locked_rx.await.unwrap();
        let started = std::time::Instant::now();
        let _ = upgrade_manifest_to_chunked(&db.pool, &sph, b"ml", &[vec![0x99; 32]], &[1024])
            .await
            .unwrap();
        let elapsed = started.elapsed();

        competitor.await.unwrap();

        // Ordering witness: upgrade must have BLOCKED on the
        // competitor's FOR UPDATE for ≥ the competitor's hold time.
        // 50ms slack budget for scheduler jitter (structural assertion
        // — `SELECT EXISTS` without FOR UPDATE returns in <5ms).
        assert!(
            elapsed >= Duration::from_millis(50),
            "upgrade should block on competing FOR UPDATE; elapsed={elapsed:?}"
        );
    }

    // r[verify store.chunk.durable-flag]
    /// `durable` flips exactly when the manifest completes — never at
    /// staging time (the WAL window is the I-201 hazard HasChunks
    /// guards against), and a second manifest sharing the chunk leaves
    /// the flag set (no un-flip, no error from the `AND NOT durable`
    /// no-op).
    #[tokio::test]
    async fn durable_flips_on_complete_not_on_stage() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk = vec![0x6Du8; 32];
        let one_chunk = std::slice::from_ref(&chunk);
        let chunk_list = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: chunk.as_slice().try_into().unwrap(),
                size: 1024,
            }],
        }
        .serialize();
        let durable = |pool: &PgPool, h: &[u8]| {
            let pool = pool.clone();
            let h = h.to_vec();
            async move {
                sqlx::query_scalar::<_, bool>("SELECT durable FROM chunks WHERE blake3_hash = $1")
                    .bind(&h)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };

        // --- Manifest A: stage → not durable; complete → durable. ---
        let path_a = rio_test_support::fixtures::test_store_path("durable-a");
        let sph_a = rio_nix::store_path::StorePath::parse(&path_a)
            .unwrap()
            .sha256_digest()
            .to_vec();
        let claim_a = crate::metadata::insert_manifest_uploading(&db.pool, &sph_a, &path_a, &[])
            .await
            .unwrap()
            .unwrap();
        upgrade_manifest_to_chunked(&db.pool, &sph_a, &chunk_list, one_chunk, &[1024])
            .await
            .unwrap();
        assert!(
            !durable(&db.pool, &chunk).await,
            "staged-but-not-complete chunk must NOT be durable (I-201 WAL window)"
        );

        let mut info_a =
            rio_test_support::fixtures::make_path_info(&path_a, &[0u8; 1024], [0x55; 32]);
        info_a.store_path_hash = sph_a.clone();
        complete_manifest_chunked(&db.pool, &info_a, claim_a, None, None)
            .await
            .unwrap();
        assert!(
            durable(&db.pool, &chunk).await,
            "completing the manifest must flip its chunks durable"
        );

        // --- Manifest B shares the chunk: completing it is a no-op
        //     on the already-durable row, not an error. ---
        let path_b = rio_test_support::fixtures::test_store_path("durable-b");
        let sph_b = rio_nix::store_path::StorePath::parse(&path_b)
            .unwrap()
            .sha256_digest()
            .to_vec();
        let claim_b = crate::metadata::insert_manifest_uploading(&db.pool, &sph_b, &path_b, &[])
            .await
            .unwrap()
            .unwrap();
        upgrade_manifest_to_chunked(&db.pool, &sph_b, &chunk_list, one_chunk, &[1024])
            .await
            .unwrap();
        let mut info_b =
            rio_test_support::fixtures::make_path_info(&path_b, &[0u8; 1024], [0x66; 32]);
        info_b.store_path_hash = sph_b.clone();
        complete_manifest_chunked(&db.pool, &info_b, claim_b, None, None)
            .await
            .unwrap();
        assert!(durable(&db.pool, &chunk).await, "still durable after B");
        let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc, 2, "refcount unaffected by the durable flip");
    }
}
