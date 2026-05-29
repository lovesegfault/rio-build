//! Chunked write-ahead: large NARs split by FastCDC, chunks in S3,
//! manifest_data holds the ordered (blake3, size) list.
//!
//! `upgrade_manifest_to_chunked` takes an EXISTING placeholder (from
//! `insert_manifest_uploading`) and adds manifest_data + upserts the chunk
// r[impl store.chunk.refcount-txn+2]
// r[impl store.put.wal-manifest]
//! rows — the placeholder is the idempotency lock, created BEFORE the
//! NAR stream is consumed, before we know the size. The chunk rows are
//! established here (not at complete) so the durable manifest reference
//! and the per-chunk row commit together: the chunk collector's mark
//! fold sees the `'uploading'` manifest's references from the instant
//! this transaction commits, and the conflict-arm touch keeps an old,
//! re-referenced chunk inside the current collect cycle's grace term.

use super::*;
use sqlx::PgPool;
use std::collections::HashSet;
use tracing::{debug, instrument, warn};

// ---------------------------------------------------------------------------
// Chunked manifest ops
// ---------------------------------------------------------------------------

/// Upgrade an existing 'uploading' manifest to chunked: write manifest_data
/// + upsert the chunk rows in the same transaction.
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
/// # Why the chunk rows are written here (before upload), not at complete
///
/// The `manifest_data.chunk_list` written in this transaction is what
/// the chunk collector's mark fold derives liveness from
/// (`'uploading'` manifests count), so the chunks are protected from
/// collection the instant this commits. Writing the chunk rows in the
/// same transaction keeps the per-chunk bookkeeping (`size`, the
/// resurrect flag, the `last_referenced_at` touch, the `uploaded_at`
/// dedup verdict) atomic with the reference that justifies it. On
/// failure nothing is rolled forward by hand: the rollback deletes the
/// placeholder rows and the next collect cycle notices any chunks left
/// unreferenced.
///
/// # Chunk-row UPSERT
///
/// `INSERT ... ON CONFLICT DO UPDATE` is row-level atomic — no explicit
/// SELECT FOR UPDATE needed. Two concurrent PutPaths referencing the same
/// chunk both record their reference correctly (PG resolves the conflict,
/// second one sees the first's row and runs the UPDATE clause).
#[instrument(skip(pool, chunk_list, chunk_hashes, chunk_sizes), fields(store_path_hash = hex::encode(store_path_hash), chunks = chunk_hashes.len()))]
pub(crate) async fn upgrade_manifest_to_chunked(
    pool: &PgPool,
    store_path_hash: &[u8],
    chunk_list: &[u8],        // serialized Manifest
    chunk_hashes: &[Vec<u8>], // each is a 32-byte BLAKE3
    chunk_sizes: &[i64],      // parallel to chunk_hashes
) -> Result<HashSet<Vec<u8>>> {
    // Collect-soundness assumption (refcount-formal design §4.1): the
    // chunk collector's grace term protects a manifest that commits
    // after a mark snapshot only if this transaction — the single
    // chunk-referencing write transaction — is shorter than the grace
    // window. The duration is monitored (histogram + alert at grace/2),
    // not enforced; measured from before begin() so everything from
    // BEGIN to COMMIT is covered, recorded only on the success path
    // (an aborted upgrade commits no manifest, so its duration cannot
    // endanger collect soundness).
    let tx_started = std::time::Instant::now();
    let mut tx = pool.begin().await?;

    // Ownership lock: the manifests row MUST exist with status=
    // 'uploading' AND we must hold a FOR UPDATE lock on it for the
    // rest of this txn. A plain `SELECT EXISTS(...)` (no FOR UPDATE)
    // is wrong: under READ COMMITTED, the orphan reaper can delete +
    // commit between the EXISTS and the INSERT below, leaving an
    // orphaned `manifest_data` row with no `manifests` parent. FOR
    // UPDATE blocks `reap_one` (and `complete_manifest_chunked`) until
    // this tx commits, so the verdict holds for the whole tx. Same
    // pattern as `gc::orphan::reap_one`.
    let placeholder: Option<i32> = sqlx::query_scalar(
        r#"
        SELECT 1 FROM manifests
        WHERE store_path_hash = $1 AND status = 'uploading'
        FOR UPDATE
        "#,
    )
    .bind(store_path_hash)
    .fetch_optional(&mut *tx)
    .await?;
    if placeholder.is_none() {
        return Err(MetadataError::PlaceholderMissing {
            store_path: hex::encode(store_path_hash),
        });
    }

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

    // Chunk-row UPSERT. UNNEST over parallel arrays; lengths are asserted
    // equal above (the co-sort `zip` below would silently truncate to
    // the shorter input otherwise — there is NO PG-side length check).
    //
    // ON CONFLICT DO UPDATE is atomic per-row. PG's conflict resolution
    // serializes INSERT vs UPDATE — two concurrent PutPaths with
    // overlapping chunk lists both record their rows correctly.
    //
    // r[impl store.chunk.refcount-txn+2]
    // r[impl store.chunk.lock-order+2]
    // Co-sort (hash, size) pairs by hash before UNNEST: ON CONFLICT DO
    // UPDATE acquires row locks on the conflicted rows in UNNEST input
    // order; two concurrent upgrades with reversed-order overlapping
    // sets (or any other chunk writer walking its hash list in sorted
    // order) would otherwise deadlock. The co-sort keeps each hash
    // paired with its size.
    let mut pairs: Vec<(Vec<u8>, i64)> = chunk_hashes
        .iter()
        .cloned()
        .zip(chunk_sizes.iter().copied())
        .collect();
    pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let (chunk_hashes, chunk_sizes): (Vec<Vec<u8>>, Vec<i64>) = pairs.into_iter().unzip();
    //
    // `deleted = false`: resurrects a chunk that the collect cycle
    // soft-deleted between collect and drain. Without this, PutPath
    // would re-reference the chunk but leave `deleted=true` → the
    // drain would still consider it dead. The drain re-check
    // (drain.rs) is the PRIMARY guard; this is defense-in-depth so
    // the `chunks` row state is self-consistent (a referenced chunk
    // is not left flagged for deletion).
    //
    // r[impl store.cas.upsert-inserted+3]
    // RETURNING (uploaded_at IS NULL) AS needs_upload: a chunk needs
    // (re-)upload iff no prior PutPath has confirmed S3 presence via
    // `mark_chunks_uploaded`. Contrast with the original heuristic
    // (the row already existed ⇒ someone else already uploaded) —
    // false when that someone is mid-upload and gets SIGKILLed (helm
    // rolling update). M_033 has the full race.
    //
    // RETURNING sees the POST-update row state (SQL standard). So:
    //   fresh INSERT             → uploaded_at = NULL          → needs_upload = true
    //   CONFLICT, uploaded_at NULL (in-flight or interrupted)  → needs_upload = true
    //   CONFLICT, uploaded_at set (S3-confirmed)               → needs_upload = false
    //
    // Two concurrent PutPaths sharing a chunk now BOTH upload —
    // S3 PutObject is idempotent (same key, same bytes), so the
    // duplicate write is wasted bandwidth, not a correctness hazard.
    //
    // `last_referenced_at = now()` (the touch, migration 070): exists
    // for the lazy chunk collector's mark-snapshot race — a manifest
    // that commits after a collect cycle's mark snapshot but
    // re-references an old chunk is invisible to that cycle's mark;
    // the touch keeps the chunk inside the cycle's grace term. This
    // upsert is the column's ONLY writer (no cleanup path touches it),
    // and a fresh INSERT leaves it NULL (NULL ≡ created_at under the
    // collector's GREATEST predicate).
    let rows: Vec<(Vec<u8>, bool)> = sqlx::query_as(
        r#"
        INSERT INTO chunks (blake3_hash, size)
        SELECT * FROM UNNEST($1::bytea[], $2::bigint[])
               AS t(hash, size)
        ON CONFLICT (blake3_hash) DO UPDATE
            SET deleted = false,
                deleted_at = NULL,
                last_referenced_at = now()
        RETURNING blake3_hash, (uploaded_at IS NULL) AS needs_upload
        "#,
    )
    .bind(&chunk_hashes)
    .bind(&chunk_sizes)
    .fetch_all(&mut *tx)
    .await?;

    let needs_upload: HashSet<Vec<u8>> = rows
        .into_iter()
        .filter_map(|(h, need)| need.then_some(h))
        .collect();

    tx.commit().await?;
    metrics::histogram!("rio_store_chunk_upgrade_tx_seconds").record(tx_started.elapsed());
    Ok(needs_upload)
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
/// `AND deleted = FALSE` closes the late-mark window: an owner whose
/// row was stale-reclaimed (soft-deleted, `uploaded_at` cleared)
/// between its S3 PUTs and this call must not re-assert presence on a
/// row whose backend object the drain is about to delete — a deleted
/// row's presence is (re-)established only by the resurrecting upsert
/// plus a fresh PUT.
///
/// Hashes are sorted before binding — same lock-order discipline as
/// every other `chunks` writer (the chunk lock-order rule).
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
/// Does NOT touch the chunk rows (already upserted at uploading time).
///
/// `castore` is `None` only in metadata-layer tests that seed fake
/// chunk lists; such paths are not GetPath-servable. Production
/// (`cas::put_chunked`) always passes `Some`.
#[instrument(skip(pool, info, castore), fields(store_path = %info.store_path.as_str()))]
pub(crate) async fn complete_manifest_chunked(
    pool: &PgPool,
    info: &ValidatedPathInfo,
    claim: uuid::Uuid,
    castore: Option<&crate::cas::ParsedNar>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    super::complete_manifest_in_conn(&mut tx, info, claim, None, castore).await?;
    tx.commit().await?;
    debug!(store_path = %info.store_path.as_str(), "chunked upload completed");
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
/// narinfo, and castore index via [`super::complete_manifest_in_conn`],
/// and finally the `path_tenants` junction.
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
    // directory_paths/file_blobs. Reads the manifest_data row inserted
    // above.
    super::complete_manifest_in_conn(conn, info, claim, None, Some(parsed)).await?;

    // Tolerant variant: an assignment token can name a tenant deleted
    // while the build was in flight; that must skip the junction write,
    // not abort the commit transaction this output just completed in.
    insert_path_tenant_skipping_deleted_in_tx(conn, &info.store_path_hash, tenant_id).await
}

/// `path_tenants` junction insert (`r[store.castore.tenant-scope]`).
/// Idempotent; a `None` tenant (dev mode, service-token caller) writes
/// nothing. Runs for idempotent-skipped outputs too — the prior commit
/// may have been via legacy `PutPath`, which didn't write the row.
// r[impl store.castore.tenant-scope]
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

/// The PG-generated name of `path_tenants.tenant_id`'s foreign key to
/// `tenants` (012_path_tenants.sql). PG includes it verbatim in the
/// 23503 error message, which is how [`is_deleted_tenant_fk`]
/// recognizes the violation without matching any other conflict.
const PATH_TENANTS_TENANT_FK: &str = "path_tenants_tenant_id_fkey";

/// True iff `err` is the foreign-key violation raised by a
/// `path_tenants` junction insert whose tenant row no longer exists —
/// an assignment token minted for a tenant that was deleted while the
/// build was in flight. [`MetadataError::Conflict`] is only produced
/// for SQLSTATE 23503/23505 and carries PG's primary message, which
/// for an FK violation names the constraint; requiring the full
/// `violates foreign key constraint "<name>"` phrase means a unique
/// violation (23505) or an FK violation on any other constraint never
/// matches.
pub(crate) fn is_deleted_tenant_fk(err: &MetadataError) -> bool {
    matches!(
        err,
        MetadataError::Conflict(msg)
            if msg.contains(&format!(
                "violates foreign key constraint \"{PATH_TENANTS_TENANT_FK}\""
            ))
    )
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
/// MUST be inside a transaction (the §6.2 commit transaction is the
/// only one today).
// r[impl store.castore.tenant-scope]
pub(crate) async fn insert_path_tenant_skipping_deleted_in_tx(
    conn: &mut sqlx::PgConnection,
    store_path_hash: &[u8],
    tenant_id: Option<uuid::Uuid>,
) -> Result<()> {
    if tenant_id.is_none() {
        return Ok(());
    }
    sqlx::query("SAVEPOINT path_tenant_junction")
        .execute(&mut *conn)
        .await?;
    match insert_path_tenant_in_conn(conn, store_path_hash, tenant_id).await {
        Err(e) if is_deleted_tenant_fk(&e) => {
            warn!(
                store_path_hash = hex::encode(store_path_hash),
                tenant_id = %tenant_id.expect("checked non-None above"),
                "path_tenants junction skipped: tenant was deleted while the build was in flight"
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

        // Exactly one chunk row, with the bound size.
        let (n, size): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COALESCE(MAX(size), 0) FROM chunks")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(n, 1, "deduped insert → one chunk row");
        assert_eq!(size, 1024, "chunk row carries the bound size");
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
    /// set is driven by `uploaded_at`, never by row pre-existence.
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
        let need1 = upgrade_manifest_to_chunked(
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
        // A's row already exists with uploaded_at set → conflict arm
        // runs (touch), but uploaded_at IS NOT NULL → NOT in
        // needs_upload. C is fresh.
        let sph2 = vec![0x22u8; 32];
        seed_placeholder(&db.pool, &sph2).await;
        let need2 = upgrade_manifest_to_chunked(
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

        // Ground truth: the second upsert hit A's conflict arm (the
        // re-reference touch is set), and C's fresh insert left it NULL.
        let (touched_a, touched_c): (bool, bool) = sqlx::query_as(
            "SELECT \
               (SELECT last_referenced_at IS NOT NULL FROM chunks WHERE blake3_hash = $1), \
               (SELECT last_referenced_at IS NOT NULL FROM chunks WHERE blake3_hash = $2)",
        )
        .bind(&chunk_a)
        .bind(&chunk_c)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(touched_a, "A re-referenced by a second manifest → touched");
        assert!(!touched_c, "C fresh insert → last_referenced_at NULL");
    }

    /// The resurrection case (Audit B1 #8): chunk at deleted=true,
    /// uploaded_at NULL — the post-collect, pre-drain state (the
    /// collect cycle's soft-delete clears uploaded_at when it sets
    /// deleted). An upsert resurrects it — deleted flips false,
    /// uploaded_at stays NULL → MUST be in needs_upload. S3 may have
    /// already deleted the object between collect and now.
    #[tokio::test]
    async fn upsert_returning_resurrection_needs_upload() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk = vec![0xDEu8; 32];

        // Seed the chunk at deleted=true, uploaded_at NULL — the
        // post-collect, pre-drain state. Directly INSERT (bypassing
        // the upsert path) to set up the exact precondition.
        sqlx::query("INSERT INTO chunks (blake3_hash, size, deleted) VALUES ($1, 1024, true)")
            .bind(&chunk)
            .execute(&db.pool)
            .await
            .unwrap();

        // Precondition: confirm the seeded state.
        let (del0, up0): (bool, bool) = sqlx::query_as(
            "SELECT deleted, (uploaded_at IS NOT NULL) FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(del0, "precondition: deleted=true (awaiting drain)");
        assert!(!up0, "precondition: uploaded_at NULL");

        // Upsert resurrects: ON CONFLICT → deleted=false.
        let sph = vec![0xDDu8; 32];
        seed_placeholder(&db.pool, &sph).await;
        let need = upgrade_manifest_to_chunked(
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

        // Ground truth: deleted=false, and the conflict arm recorded
        // the re-reference (the touch is set).
        let (del, touched): (bool, bool) = sqlx::query_as(
            "SELECT deleted, (last_referenced_at IS NOT NULL) FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(!del, "resurrected: deleted flipped false");
        assert!(touched, "resurrected: conflict arm set the touch");
    }

    /// Migration 068 touch: a fresh INSERT leaves `last_referenced_at`
    /// NULL (NULL ≡ created_at under the collector's GREATEST
    /// predicate); a conflicting second upgrade referencing the same
    /// chunk sets it; a third advances it. The touch must not change
    /// the needs_upload verdict, which stays keyed on `uploaded_at`
    /// alone (the dedup verdict stays RETURNING-atomic and
    /// CR-4-shaped).
    // r[verify store.chunk.refcount-txn+2]
    #[tokio::test]
    async fn upsert_touch_advances_last_referenced_at() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk = vec![0x68u8; 32];

        // --- Fresh insert: last_referenced_at stays NULL ---
        let sph1 = vec![0x31u8; 32];
        seed_placeholder(&db.pool, &sph1).await;
        let need1 = upgrade_manifest_to_chunked(
            &db.pool,
            &sph1,
            b"manifest-touch-1",
            std::slice::from_ref(&chunk),
            &[1024],
        )
        .await
        .unwrap();
        assert!(need1.contains(&chunk), "fresh chunk needs upload");

        let touched1: Option<f64> = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM last_referenced_at)::float8 \
             FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(
            touched1.is_none(),
            "fresh INSERT leaves last_referenced_at NULL"
        );

        // Confirm S3 presence so the conflict path's needs_upload
        // verdict is observable (false) below.
        mark_chunks_uploaded(&db.pool, std::slice::from_ref(&chunk))
            .await
            .unwrap();

        // --- Second upgrade (conflict): touch is set ---
        let sph2 = vec![0x32u8; 32];
        seed_placeholder(&db.pool, &sph2).await;
        let need2 = upgrade_manifest_to_chunked(
            &db.pool,
            &sph2,
            b"manifest-touch-2",
            std::slice::from_ref(&chunk),
            &[1024],
        )
        .await
        .unwrap();
        assert!(
            !need2.contains(&chunk),
            "uploaded_at set → needs_upload unchanged by the touch"
        );

        let touched2: Option<f64> = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM last_referenced_at)::float8 \
             FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        let touched2 = touched2.expect("conflicting upgrade sets last_referenced_at");

        // --- Third upgrade: touch advances ---
        let sph3 = vec![0x33u8; 32];
        seed_placeholder(&db.pool, &sph3).await;
        upgrade_manifest_to_chunked(
            &db.pool,
            &sph3,
            b"manifest-touch-3",
            std::slice::from_ref(&chunk),
            &[1024],
        )
        .await
        .unwrap();

        let touched3: Option<f64> = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM last_referenced_at)::float8 \
             FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        let touched3 = touched3.expect("third upgrade keeps the touch set");
        assert!(
            touched3 >= touched2,
            "each conflicting upgrade advances (or repeats, within clock \
             resolution) the touch; it never regresses"
        );

        // Confirmed presence is unchanged by the touch.
        let up: bool =
            sqlx::query_scalar("SELECT uploaded_at IS NOT NULL FROM chunks WHERE blake3_hash = $1")
                .bind(&chunk)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(up, "the touch never clears uploaded_at");
    }

    /// The upgrade-transaction duration histogram — the runtime monitor
    /// of the chunk collector's collect-soundness assumption (no
    /// chunk-referencing write transaction outlives the grace window) —
    /// is recorded on every successful upgrade.
    #[tokio::test]
    async fn upgrade_tx_duration_histogram_recorded() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let sph = vec![0x71u8; 32];
        seed_placeholder(&db.pool, &sph).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        upgrade_manifest_to_chunked(
            &db.pool,
            &sph,
            b"manifest-tx-histogram",
            &[vec![0x72u8; 32]],
            &[1024],
        )
        .await
        .expect("upgrade succeeds");

        assert!(
            rec.histogram_touched("rio_store_chunk_upgrade_tx_seconds"),
            "successful upgrade must record the tx-duration histogram"
        );
    }

    // r[verify store.cas.chunk-upload-committed]
    /// The late-mark window: an owner whose chunk row was stale-reclaimed
    /// (soft-deleted, `uploaded_at` cleared) between its S3 PUTs and its
    /// mark must NOT re-assert `uploaded_at` on the soft-deleted row —
    /// the drain is about to delete the backend object, so a re-asserted
    /// presence would let the next writer of the same content skip its
    /// PUT against an object that no longer exists (the M_033 harm shape
    /// without consulting the counter). Presence on a soft-deleted row is
    /// (re-)established only by the resurrecting upsert + a fresh PUT.
    #[tokio::test]
    async fn mark_chunks_uploaded_skips_soft_deleted_rows() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Owner stages a chunked upload far enough to create the chunk
        // row (uploaded_at NULL — its S3 PUTs are in flight).
        let chunk = vec![0x7Eu8; 32];
        let sph = vec![0x7Au8; 32];
        seed_placeholder(&db.pool, &sph).await;
        let _ = upgrade_manifest_to_chunked(
            &db.pool,
            &sph,
            b"manifest-late-mark",
            std::slice::from_ref(&chunk),
            &[1024],
        )
        .await
        .unwrap();

        // A soft-deleted row with uploaded_at cleared is the state the
        // collect cycle leaves behind (and the legacy reclaim used to
        // leave). Simulate exactly that row state.
        sqlx::query("UPDATE chunks SET deleted = TRUE, uploaded_at = NULL WHERE blake3_hash = $1")
            .bind(&chunk)
            .execute(&db.pool)
            .await
            .unwrap();

        // The owner's late mark lands after the reclaim.
        mark_chunks_uploaded(&db.pool, std::slice::from_ref(&chunk))
            .await
            .unwrap();

        // The soft-deleted row must NOT have presence re-asserted.
        let uploaded: bool =
            sqlx::query_scalar("SELECT uploaded_at IS NOT NULL FROM chunks WHERE blake3_hash = $1")
                .bind(&chunk)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(
            !uploaded,
            "late mark must not re-assert uploaded_at on a soft-deleted chunk row"
        );
    }

    // r[verify store.cas.upsert-inserted+3]
    /// True-concurrent upserts via `tokio::join!`: two PutPaths share
    /// one chunk hash. Neither has called `mark_chunks_uploaded` yet,
    /// so BOTH see uploaded_at IS NULL → both get the shared chunk in
    /// their needs_upload set. S3 PutObject is idempotent — both
    /// uploading the same bytes to the same key is wasted bandwidth,
    /// not a correctness hazard.
    ///
    /// This is intentionally weaker than the original XOR property
    /// (exactly-one-uploader keyed on row pre-existence). The trade is
    /// one duplicate PUT under contention vs. surviving SIGKILL of the
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
        // the UPDATE clause (the touch). Both see uploaded_at NULL.
        let (need_a, need_b) = tokio::join!(
            upgrade_manifest_to_chunked(&db.pool, &sph_a, b"manifest-a", &hashes_a, &sizes_a),
            upgrade_manifest_to_chunked(&db.pool, &sph_b, b"manifest-b", &hashes_b, &sizes_b),
        );
        let need_a = need_a.unwrap();
        let need_b = need_b.unwrap();

        // Each side's unique chunk is always fresh.
        assert!(need_a.contains(&unique_a), "A's unique chunk needs upload");
        assert!(need_b.contains(&unique_b), "B's unique chunk needs upload");

        // THE KEY ASSERTION: both sides see the shared chunk as
        // needs_upload. Neither has called mark_chunks_uploaded yet,
        // so uploaded_at is NULL for both reads. Idempotent S3 PUT
        // makes the duplicate upload harmless; the alternative
        // (exactly-one keyed on row pre-existence) loses data when the
        // winner is SIGKILLed mid-upload.
        let a_has = need_a.contains(&shared);
        let b_has = need_b.contains(&shared);
        assert!(
            a_has && b_has,
            "both concurrent upserts see shared chunk as needs_upload \
             (got A={a_has}, B={b_has}; either-false = M033 regression)"
        );

        // Ground truth: the loser of the ON CONFLICT serialization ran
        // the conflict arm (the shared chunk's re-reference touch is
        // set); the unique chunks stayed fresh inserts (NULL).
        let (shared_touched, a_touched): (bool, bool) = sqlx::query_as(
            "SELECT \
               (SELECT last_referenced_at IS NOT NULL FROM chunks WHERE blake3_hash = $1), \
               (SELECT last_referenced_at IS NOT NULL FROM chunks WHERE blake3_hash = $2)",
        )
        .bind(&shared)
        .bind(&unique_a)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(shared_touched, "shared chunk hit the conflict arm");
        assert!(!a_touched, "unique chunk stayed a fresh insert");
    }

    // r[verify store.cas.chunk-upload-committed]
    /// The SIGKILL race that motivated `uploaded_at` (M_033):
    ///
    /// 1. PutPath A: upsert chunk X (uploaded_at NULL). Starts
    ///    upload. Process is SIGKILLed (helm rolling update) — no
    ///    rollback runs, no mark_chunks_uploaded runs. Manifest A
    ///    left at status='uploading'.
    /// 2. PutPath B (different path, same chunk X): upsert hits the
    ///    conflict arm. Under the original row-pre-existence
    ///    heuristic, B would skip upload here — permanent data loss
    ///    (X never reaches S3). Under `uploaded_at IS NULL`, B
    ///    uploads.
    /// 3. B's upload succeeds → mark_chunks_uploaded → uploaded_at
    ///    set. Manifest B completes.
    /// 4. PutPath C (third path, same chunk X): upsert (uploaded_at
    ///    set) → skips upload. Correct dedup.
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
        // Real serialized chunk_list (all three manifests share the
        // same single-chunk list).
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
        let need_a = upgrade_manifest_to_chunked(&db.pool, &sph_a, &chunk_list, one_chunk, &[1024])
            .await
            .unwrap();
        assert!(need_a.contains(&chunk_x), "A: fresh insert needs upload");
        // SIGKILL: drop here. PG state committed (uploaded_at NULL,
        // manifest A status='uploading'), S3 has nothing.

        // --- Step 2+3: PutPath B sees needs_upload, uploads, marks ---
        let sph_b = vec![0xBBu8; 32];
        seed_placeholder(&db.pool, &sph_b).await;
        let need_b = upgrade_manifest_to_chunked(&db.pool, &sph_b, &chunk_list, one_chunk, &[1024])
            .await
            .unwrap();
        assert!(
            need_b.contains(&chunk_x),
            "B: row pre-exists but uploaded_at NULL → needs upload \
             (a pre-existence heuristic would skip here → data loss)"
        );
        // B uploads to S3 (omitted — backend.put is idempotent), then:
        mark_chunks_uploaded(&db.pool, one_chunk).await.unwrap();

        let up: bool =
            sqlx::query_scalar("SELECT uploaded_at IS NOT NULL FROM chunks WHERE blake3_hash = $1")
                .bind(&chunk_x)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(up, "B's mark_chunks_uploaded set uploaded_at");

        // --- Step 4: PutPath C dedups against B's confirmed upload ---
        let sph_c = vec![0xCCu8; 32];
        seed_placeholder(&db.pool, &sph_c).await;
        let need_c = upgrade_manifest_to_chunked(&db.pool, &sph_c, &chunk_list, one_chunk, &[1024])
            .await
            .unwrap();
        assert!(
            !need_c.contains(&chunk_x),
            "C: uploaded_at set → skip upload (dedup works post-commit)"
        );

        // --- Epilogue: orphan reaper cleans manifest A ---
        // reap_one deletes A's abandoned placeholder rows and nothing
        // else. uploaded_at stays set (B's upload is real); the chunk
        // stays referenced by B+C's manifests, so the collect cycle
        // never touches it. A future PutPath still dedups correctly.
        let reaped = crate::gc::orphan::reap_one(
            &db.pool,
            &sph_a,
            crate::gc::orphan::ReapBy::Claim(claim_a),
            crate::test_helpers::gc_batch_authority(&db.pool).await,
        )
        .await
        .unwrap();
        assert!(reaped, "A's stale 'uploading' placeholder reaped");
        let (up, del): (bool, bool) = sqlx::query_as(
            "SELECT (uploaded_at IS NOT NULL), deleted FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk_x)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(
            up,
            "reaper does NOT clear uploaded_at — B+C still reference the chunk"
        );
        assert!(!del, "reaper never soft-deletes chunk rows");
    }

    /// Regression: a concurrent chunk-row upsert vs another chunk
    /// writer on overlapping hashes MUST NOT deadlock.
    ///
    /// `INSERT ... ON CONFLICT DO UPDATE` acquires its row locks in
    /// UNNEST input order, so the upsert's internal co-sort is what
    /// keeps its lock-acquisition order aligned with every other
    /// writer that walks its hash list in sorted order. The per-row
    /// contender below models such a writer (one UPDATE per hash,
    /// ascending, all in one tx). Feeding the upsert REVERSED input
    /// makes the co-sort load-bearing: without it the upsert locks
    /// descending against the contender's ascending walk → circular
    /// wait → 40P01. The 5s timeout backstops PG's deadlock detector
    /// (1s default).
    // r[verify store.chunk.lock-order+2]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn upsert_overlapping_no_deadlock() {
        use std::time::Duration;
        use tokio::time::timeout;

        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed 100 chunks via one manifest. 100 (vs 50) widens the
        // per-row contender's window — 100 sequential PG roundtrips
        // ~guarantees overlap with the conflicting upsert.
        let hashes: Vec<Vec<u8>> = (0u8..100).map(|i| vec![i; 32]).collect();
        let sizes: Vec<i64> = vec![1024; 100];
        let sph_a = vec![0xAAu8; 32];
        seed_placeholder(&db.pool, &sph_a).await;
        upgrade_manifest_to_chunked(&db.pool, &sph_a, b"ml-a", &hashes, &sizes)
            .await
            .unwrap();

        // Per-row contender: locks in ascending hash order (one UPDATE
        // per hash, all in one tx). No-op write — row-locks
        // unconditionally. Stands in for any chunk writer obeying the
        // lock-order rule that walks its hash list under one tx.
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

        // Side A: a second upgrade over the SAME 100 hashes, fed
        // REVERSED — pathological input the upsert's co-sort must
        // canonicalise. Side B: the ascending per-row contender.
        let mut hashes_rev = hashes.clone();
        hashes_rev.reverse();
        let mut sizes_rev = sizes.clone();
        sizes_rev.reverse();
        let hashes_asc = hashes.clone();

        let pool_a = db.pool.clone();
        let pool_b = db.pool.clone();
        let sph_b = vec![0xBBu8; 32];
        seed_placeholder(&db.pool, &sph_b).await;

        let task_a = tokio::spawn(async move {
            upgrade_manifest_to_chunked(&pool_a, &sph_b, b"ml-b", &hashes_rev, &sizes_rev).await
        });
        let task_b = tokio::spawn(async move { contend_per_row(&pool_b, &hashes_asc).await });

        let (ra, rb) = timeout(Duration::from_secs(5), async {
            tokio::try_join!(task_a, task_b).expect("tasks should not panic")
        })
        .await
        .expect("concurrent upsert+contender must complete within 5s — deadlock detected");

        ra.expect("the conflicting upsert should succeed (no 40P01)");
        rb.expect("the contender should succeed (no 40P01)");

        // Vacuity sentinel: the second upsert must have hit the
        // conflict arm on all 100 rows (every row's re-reference touch
        // is set). If a future seed regression makes the upsert match
        // zero rows, this fails loudly instead of going vacuous.
        let touched: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM chunks \
             WHERE blake3_hash = ANY($1) AND last_referenced_at IS NOT NULL",
        )
        .bind(&hashes)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(touched, 100, "the conflicting upsert touched all 100 rows");
    }

    /// I-040 chain, post-M033: the inline `delete_manifest_uploading`
    /// on a chunked placeholder leaves the chunk row behind (it deletes
    /// path rows only), but the abandoned row NO LONGER causes
    /// upload-skip on retry. The retry's upsert sees `uploaded_at IS
    /// NULL` and re-uploads regardless of the row's pre-existence.
    ///
    /// substitute.rs's call site uses `gc::orphan::reap_one` (the
    /// claim/stale-gated path-row janitor); this test asserts that
    /// even if a future caller bypasses it, the data-loss chain stays
    /// broken at the upsert level.
    #[tokio::test]
    async fn i040_inline_delete_stale_row_still_reuploads() {
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
        let ins1 = upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[100i64])
            .await
            .unwrap();
        assert!(ins1.contains(&chunk), "step 1: chunk fresh → would upload");
        // Crash here: chunk MAY or may not have made it to S3. PG state
        // is committed (chunk row exists, uploaded_at NULL).

        // --- Step 2: inline delete (the I-040 bug path) ---
        // This deletes manifests (CASCADE → manifest_data) and leaves
        // the chunk row exactly as it was (uploaded_at NULL).
        crate::metadata::delete_manifest_uploading(&db.pool, &sph)
            .await
            .unwrap();

        let (exists, up): (bool, bool) = sqlx::query_as(
            "SELECT TRUE, (uploaded_at IS NOT NULL) FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(exists, "step 2: chunk row left behind by the inline delete");
        assert!(!up, "step 2: uploaded_at still NULL (PUT never confirmed)");

        // --- Step 3: retry's upgrade_manifest_to_chunked ---
        crate::metadata::insert_manifest_uploading(&db.pool, &sph, &path, &[])
            .await
            .unwrap();
        let need2 = upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[100i64])
            .await
            .unwrap();

        // POST-M033: the chunk row pre-exists, but uploaded_at is
        // still NULL (step 1 never reached mark_chunks_uploaded) →
        // chunk IS in needs_upload → do_upload re-uploads. Data-loss
        // chain broken at the upsert.
        assert!(
            need2.contains(&chunk),
            "step 3: stale chunk row but uploaded_at NULL → re-upload \
             (data-loss chain broken regardless of call-site hygiene)"
        );
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
    /// re-uploaded + completed the same path MUST be a no-op. The
    /// rollback is the claim-gated row delete (`reap_one` with A's
    /// claim, the same call `cas::rollback` issues): A's stale claim
    /// never matches B's fresh placeholder or B's completed manifest.
    /// Previously the token-gated rollback's unguarded decrement +
    /// manifest_data DELETE could clobber B's committed state.
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

        // --- A: placeholder + upgrade. Then A's PUT hangs. ---
        let claim_a = crate::metadata::insert_manifest_uploading(&db.pool, &sph, &path, &[])
            .await
            .unwrap()
            .unwrap();
        let _ = upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[1024])
            .await
            .unwrap();

        // --- Reaper: reclaims A's stale placeholder. ---
        let reaped = crate::gc::orphan::reap_one(
            &db.pool,
            &sph,
            crate::gc::orphan::ReapBy::Stale { secs: 0 },
            crate::test_helpers::gc_batch_authority(&db.pool).await,
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
        complete_manifest_chunked(&db.pool, &info, claim_b, None)
            .await
            .unwrap();

        // --- A: hung PUT errors → late rollback fires (A's claim). ---
        let rolled_back = crate::gc::orphan::reap_one(
            &db.pool,
            &sph,
            crate::gc::orphan::ReapBy::Claim(claim_a),
            crate::test_helpers::gc_batch_authority(&db.pool).await,
        )
        .await
        .unwrap();
        assert!(!rolled_back, "A's stale claim matches nothing — no-op");

        // B's state MUST be intact: chunk row live, manifest_data
        // present, get_manifest returns Chunked.
        let (up, del): (bool, bool) = sqlx::query_as(
            "SELECT (uploaded_at IS NOT NULL), deleted FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(up, "B's confirmed upload untouched by A's late rollback");
        assert!(!del, "B's chunk row not soft-deleted by A's late rollback");

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
    /// Without the claim gate, A's `status='uploading'` match would
    /// hit B's fresh row and clobber it (manifest_data + placeholder
    /// deleted mid-upload). With the gate, A's stale claim ≠ B's fresh
    /// `claim_id` → rollback is a no-op.
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
        let claim_a = crate::metadata::insert_manifest_uploading(&db.pool, &sph, &path, &[])
            .await
            .unwrap()
            .unwrap();
        let _ = upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[1024])
            .await
            .unwrap();
        // Simulate staleness so the reaper's threshold matches A's row.
        sqlx::query(
            "UPDATE manifests SET updated_at = now() - interval '1 hour' \
             WHERE store_path_hash = $1",
        )
        .bind(&sph)
        .execute(&db.pool)
        .await
        .unwrap();

        // --- Reaper: reclaims A. ---
        assert!(
            crate::gc::orphan::reap_one(
                &db.pool,
                &sph,
                crate::gc::orphan::ReapBy::Stale { secs: 0 },
                crate::test_helpers::gc_batch_authority(&db.pool).await,
            )
            .await
            .unwrap()
        );

        // --- B: fresh placeholder + upgrade, SAME chunks. STILL
        //     'uploading' (mid-upload). ---
        let claim_b = crate::metadata::insert_manifest_uploading(&db.pool, &sph, &path, &[])
            .await
            .unwrap()
            .unwrap();
        let _ = upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[1024])
            .await
            .unwrap();
        assert_ne!(claim_a, claim_b, "B's fresh row has a distinct claim");

        // --- A: hung PUT errors → late rollback with A's STALE claim. ---
        let rolled_back = crate::gc::orphan::reap_one(
            &db.pool,
            &sph,
            crate::gc::orphan::ReapBy::Claim(claim_a),
            crate::test_helpers::gc_batch_authority(&db.pool).await,
        )
        .await
        .unwrap();
        assert!(!rolled_back, "A's stale claim ≠ B's fresh claim — no-op");

        // B's mid-upload state MUST be intact: chunk row live,
        // manifest_data present, manifests row still 'uploading'.
        let del: bool = sqlx::query_scalar("SELECT deleted FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert!(!del, "B's chunk row untouched by A's late rollback");

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

        // --- Sanity: B's OWN rollback (matching claim) DOES tear down. ---
        // This is the claim-roundtrip happy path: an uploader CAN
        // clean up its own placeholder, even after heartbeats.
        assert!(
            crate::gc::orphan::reap_one(
                &db.pool,
                &sph,
                crate::gc::orphan::ReapBy::Claim(claim_b),
                crate::test_helpers::gc_batch_authority(&db.pool).await,
            )
            .await
            .unwrap(),
            "B's own rollback (matching claim) tears down"
        );
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM manifests WHERE store_path_hash = $1")
                .bind(&sph)
                .fetch_optional(&db.pool)
                .await
                .unwrap();
        assert_eq!(status, None, "B's placeholder gone after its own rollback");
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
        complete_manifest_chunked(&db.pool, &info_a, claim_a, None)
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
        complete_manifest_chunked(&db.pool, &info_b, claim_b, None)
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
