//! Sweep phase: delete unreachable paths (narinfo/manifests and their
//! path-level bookkeeping). Chunk GC is decoupled: the collect cycle
//! (`super::collect`) is the only producer of chunk soft-deletes and
//! `pending_s3_deletes` rows.
// r[impl store.gc.two-phase+2]

use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use sqlx::{Connection, PgPool, Postgres, Transaction};
use tracing::{info, warn};

use crate::backend::ChunkBackend;

use super::GcStats;

/// Terminal state of [`sweep`] other than success.
#[derive(Debug)]
pub enum SweepAbort {
    /// Process shutdown token fired between batches. Partial
    /// progress committed; caller should release advisory lock
    /// and return Aborted to the client.
    Shutdown,
    /// Database error. Transaction rolled back (sqlx drops the
    /// uncommitted tx on error-return).
    Db(sqlx::Error),
}

impl From<sqlx::Error> for SweepAbort {
    fn from(e: sqlx::Error) -> Self {
        SweepAbort::Db(e)
    }
}

/// Batch size for sweep transactions. Each batch is a single tx of
/// N narinfo DELETEs (CASCADE to manifests/manifest_data). Small
/// enough that a batch-rollback on conflict doesn't waste much; large
/// enough to amortize tx overhead.
#[cfg(not(test))]
const SWEEP_BATCH_SIZE: usize = 100;
#[cfg(test)]
const SWEEP_BATCH_SIZE: usize = 2;

/// Grace period before an unreferenced chunk becomes GC-eligible
/// (the collect cycle's grace term).
///
/// # Why a grace window exists
///
/// `cas::put_chunked` upserts the `chunks` row BEFORE uploading to
/// S3 (write-ahead). If the upload crashes, the row sits unreferenced
/// once its placeholder is reaped, with `uploaded_at IS NULL`. A retry
/// of the same PutPath re-references it (and clears `deleted` if a
/// collect cycle got there first); the grace window gives that retry
/// time to land before the collect cycle fires, and absorbs the
/// mark-snapshot race together with the upsert's
/// `last_referenced_at` touch.
///
/// # Why 300s
///
/// Long enough to cover a stalled-then-retried PutPath; short
/// enough that a genuinely abandoned chunk leaks storage for only
/// minutes beyond the next collect cycle. Compare
/// `orphan::STALE_THRESHOLD` (15min) — that's for stale `uploading`
/// manifests, whose false-positive reaping is costlier (a whole
/// NAR re-upload).
///
/// `i64` to match the bind pattern in `orphan.rs` (`make_interval(
/// secs => $1)` accepts a bigint bind — PG casts it to double
/// internally for the `secs` named argument).
pub const CHUNK_GRACE_SECS: i64 = 300;

/// Populate the session-scoped `sweep_unreachable` temp table used by
/// the reference re-check anti-join.
///
/// Before P0449 the re-check bound the WHOLE `unreachable` `Vec<bytea>`
/// as $2 inside the per-path loop: N paths × N-element bytea[] = O(N²)
/// wire bytes. A 10k-path sweep sent ~3GB of $2 traffic, and
/// `<> ALL(array_param)` is an unindexed linear scan PG-side.
/// Populating once here is O(N) wire; NOT EXISTS against the PRIMARY
/// KEY is an index probe per-row.
///
/// `DROP IF EXISTS` first: defends against a prior sweep that crashed
/// mid-run on the same pooled connection (temp tables are
/// session-scoped, not transaction-scoped).
async fn setup_sweep_unreachable(
    conn: &mut sqlx::PgConnection,
    unreachable: &[Vec<u8>],
) -> Result<(), sqlx::Error> {
    sqlx::query("DROP TABLE IF EXISTS sweep_unreachable")
        .execute(&mut *conn)
        .await?;
    sqlx::query("CREATE TEMP TABLE sweep_unreachable (path_hash bytea PRIMARY KEY)")
        .execute(&mut *conn)
        .await?;
    sqlx::query("INSERT INTO sweep_unreachable (path_hash) SELECT unnest($1::bytea[])")
        .bind(unreachable)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Referrer-first iteration order over `sweep_unreachable`: layer 0 =
/// paths with NO referrer inside the set; layer N = paths whose only
/// in-set referrers are at layer <N. ORDER BY layer ASC (within a
/// layer: by hash for determinism).
///
/// Ensures any Y appears at index ≤ its dep Z, so the delete loop
/// re-checks (and possibly resurrects + closure-removes Z from) Y
/// BEFORE Z's batch. Without this, a `PutPath(P, refs=[Y])` landing
/// between batch K's commit (Z) and batch K+M's re-check (Y) leaves
/// live Y with `references=[deleted Z]`.
///
/// # Algorithm
///
/// Rust-side Kahn's topological layering over the in-set referrer
/// graph (referrer→reference edges, restricted to `sweep_unreachable`
/// members). `layer[X] = 1 + max(layer[referrer of X])` is the
/// LONGEST path from any 0-indeg seed — exactly the spec definition.
/// O(|nodes| + |edges|). Cycle members never reach indeg 0, get no
/// `layer` entry, and sort LAST (the within-batch `still_unreachable`
/// probe handles them).
///
/// The previous SQL recursive CTE with per-row `visited[]` enumerated
/// one row per *distinct walk* through the DAG (exponential on
/// diamond-shaped reference graphs — Nix closures are diamond-dense)
/// and aggregated with `MIN(d)` (shortest-path; a diamond's bottom
/// node tied with its referrer and could sort before it). Kahn makes
/// both unrepresentable.
// r[impl store.gc.sweep-referrer-order]
async fn select_sweep_order(conn: &mut sqlx::PgConnection) -> Result<Vec<Vec<u8>>, sqlx::Error> {
    // One query: (hash, store_path, references[]) for every
    // sweep_unreachable row. The set is already in memory at the
    // caller (`unreachable: Vec<Vec<u8>>`), so materializing here
    // adds only the references arrays.
    let rows: Vec<(Vec<u8>, String, Vec<String>)> = sqlx::query_as(
        r#"
        SELECT su.path_hash, n.store_path, n."references"
          FROM sweep_unreachable su
          JOIN narinfo n ON n.store_path_hash = su.path_hash
        "#,
    )
    .fetch_all(&mut *conn)
    .await?;

    // store_path → hash for in-set membership; adjacency referrer→deps.
    let by_path: HashMap<&str, &[u8]> = rows
        .iter()
        .map(|(h, p, _)| (p.as_str(), h.as_slice()))
        .collect();
    let mut indeg: HashMap<&[u8], usize> = rows.iter().map(|(h, _, _)| (h.as_slice(), 0)).collect();
    let mut deps: HashMap<&[u8], Vec<&[u8]>> = HashMap::new();
    for (h, _, refs) in &rows {
        for r in refs {
            if let Some(&dep_h) = by_path.get(r.as_str()) {
                // Skip self-references (would be a 1-cycle).
                if dep_h != h.as_slice() {
                    deps.entry(h.as_slice()).or_default().push(dep_h);
                    *indeg.get_mut(dep_h).unwrap() += 1;
                }
            }
        }
    }

    // Kahn: layer[X] = 1 + max(layer[referrer]) (longest path from a
    // 0-indeg seed).
    let mut layer: HashMap<&[u8], u32> = HashMap::new();
    let mut q: VecDeque<&[u8]> = indeg
        .iter()
        .filter(|&(_, &d)| d == 0)
        .map(|(&h, _)| {
            layer.insert(h, 0);
            h
        })
        .collect();
    while let Some(h) = q.pop_front() {
        let lh = layer[h];
        for &d in deps.get(h).into_iter().flatten() {
            let e = layer.entry(d).or_insert(0);
            *e = (*e).max(lh + 1);
            let id = indeg.get_mut(d).unwrap();
            *id -= 1;
            if *id == 0 {
                q.push_back(d);
            }
        }
    }

    // Cycle members: indeg never hit 0 → no `layer` entry → sort last.
    let mut out: Vec<Vec<u8>> = rows.iter().map(|(h, _, _)| h.clone()).collect();
    out.sort_by(
        |a, b| match (layer.get(a.as_slice()), layer.get(b.as_slice())) {
            (Some(x), Some(y)) => x.cmp(y).then_with(|| a.cmp(b)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.cmp(b),
        },
    );
    Ok(out)
}

/// Delete one swept path's metadata: realisations + path_tenants +
/// narinfo (CASCADE → manifests/manifest_data). Runs inside the
/// caller's batch transaction.
///
/// Returns `false` if narinfo was already gone (defensive; shouldn't
/// happen under FOR UPDATE). Chunk state is not touched at all (chunk
/// GC is the collect cycle's job) — this only touches the path-keyed
/// tables.
// r[impl store.realisation.gc-sweep]
// r[impl store.gc.sweep-path-tenants+1]
// r[impl store.gc.evidence-outlives-bytes]
async fn delete_swept_path(
    tx: &mut Transaction<'_, Postgres>,
    store_path_hash: &[u8],
) -> Result<bool, sqlx::Error> {
    // Round-9 WO-S1-4 (evidence-outlives-bytes, signed Q3): copy the
    // dying registration/identity records into the append-only
    // tombstone tables INSIDE this batch tx, BEFORE the deletes — a
    // swept path's records are atomically either live or tombstoned,
    // never lost. The LIVE rows still die below (their deletion is
    // what defends the wrong-tenant-revival leak and keeps every live
    // reader's semantics intact); what survives is the AUDIT record:
    // who registered what, when, derived from which drv.
    sqlx::query(
        r#"
        INSERT INTO path_tenant_tombstones
            (store_path_hash, store_path, tenant_id, first_referenced_at, deriver)
        SELECT pt.store_path_hash, n.store_path, pt.tenant_id,
               pt.first_referenced_at, n.deriver
          FROM path_tenants pt
          JOIN narinfo n USING (store_path_hash)
         WHERE pt.store_path_hash = $1
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO realisation_tombstones
            (drv_hash, output_name, output_path, output_hash)
        SELECT r.drv_hash, r.output_name, r.output_path, r.output_hash
          FROM realisations r
         WHERE r.output_path = (
           SELECT store_path FROM narinfo WHERE store_path_hash = $1
         )
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut **tx)
    .await?;

    // Step 2a: DELETE realisations. NOT via CASCADE — realisations has
    // NO FK to narinfo (002_store.sql:134). Without this, dangling
    // realisations rows point to swept paths → wopQueryRealisation
    // returns a path that 404s on fetch. realisations_output_idx makes
    // the subselect fast.
    sqlx::query(
        r#"
        DELETE FROM realisations
         WHERE output_path = (
           SELECT store_path FROM narinfo WHERE store_path_hash = $1
         )
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut **tx)
    .await?;

    // Step 2a': DELETE path_tenants. NOT via CASCADE — path_tenants has
    // NO FK to narinfo (012_path_tenants.sql). Without this, orphaned
    // rows survive the sweep and grant wrong-tenant visibility when a
    // different tenant later re-uploads the same store path (the stale
    // row still JOINs in the r[store.gc.tenant-retention] CTE arm).
    sqlx::query("DELETE FROM path_tenants WHERE store_path_hash = $1")
        .bind(store_path_hash)
        .execute(&mut **tx)
        .await?;

    // Step 2b: DELETE narinfo. CASCADE takes manifests, manifest_data.
    let deleted = sqlx::query("DELETE FROM narinfo WHERE store_path_hash = $1")
        .bind(store_path_hash)
        .execute(&mut **tx)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Re-check whether `store_path_hash` has any concurrent-writable mark
/// seed: (i) a `narinfo.references` referrer outside the
/// `sweep_unreachable` temp table, (ii) a direct `scheduler_live_pins`
/// entry, or (iii) a `path_tenants` row inside any tenant's retention
/// window OR under an active tenant-scoped GC hold (the round-9 hold
/// conjunct — a hold set between mark and this path's batch still
/// protects it). See the call-site comment in [`sweep`] for the
/// GIN/anti-join rationale.
// r[impl store.gc.hold+2]
async fn recheck_has_live_referrer(
    tx: &mut Transaction<'_, Postgres>,
    store_path_hash: &[u8],
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(concat!(
        r#"
        SELECT
          EXISTS (
            SELECT 1 FROM narinfo n
             WHERE n."references" @> ARRAY[
                     (SELECT store_path FROM narinfo WHERE store_path_hash = $1)
                   ]
               AND NOT EXISTS (
                 SELECT 1 FROM sweep_unreachable su
                  WHERE su.path_hash = n.store_path_hash
               )
             LIMIT 1
          )
          OR EXISTS (SELECT 1 FROM scheduler_live_pins WHERE store_path_hash = $1)
          OR EXISTS (
            SELECT 1 FROM path_tenants pt
              JOIN tenants t USING (tenant_id)
             WHERE pt.store_path_hash = $1
               AND (pt.first_referenced_at > now() - make_interval(hours => t.gc_retention_hours)
                    OR EXISTS (
                      SELECT 1 FROM gc_holds h
                       WHERE h.scope = 'tenant' AND h.tenant_id = pt.tenant_id
                         AND h."#,
        super::hold::active_hold_predicate!(),
        r#"
                    ))
          )
        "#
    ))
    .bind(store_path_hash)
    .fetch_one(&mut **tx)
    .await
}

/// Walk `store_path_hash`'s reference closure within
/// `sweep_unreachable` and DELETE all closure members from the temp
/// table. Bounded to nodes already in the table (the JOIN), so
/// terminates and stays ≤ |unreachable|.
async fn closure_remove_from_unreachable(
    tx: &mut Transaction<'_, Postgres>,
    store_path_hash: &[u8],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        WITH RECURSIVE closure(path_hash) AS (
            SELECT $1::bytea
          UNION
            SELECT dep.store_path_hash
              FROM closure c
              JOIN narinfo n ON n.store_path_hash = c.path_hash
              JOIN narinfo dep ON dep.store_path = ANY(n."references")
              JOIN sweep_unreachable su ON su.path_hash = dep.store_path_hash
        )
        DELETE FROM sweep_unreachable
         WHERE path_hash IN (SELECT path_hash FROM closure)
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Sweep unreachable paths. For each:
/// 1. `SELECT 1 FROM manifests ... FOR UPDATE` (a concurrent PutPath
///    for the SAME path blocks until this batch commits — prevents a
///    re-upload racing the delete)
/// 2. `DELETE realisations` for this path (NO FK to narinfo —
///    explicit delete prevents dangling wopQueryRealisation rows)
/// 3. `DELETE narinfo` (CASCADE → manifests/manifest_data) plus the
///    path_tenants cleanup
///
/// The sweep does NOT touch `chunks` or `pending_s3_deletes`: a swept
/// path's now-unreferenced chunks are collected by the collect cycle
/// (run_gc phase 3 / the daily backstop) once they fall outside the
/// grace window — chunk GC is fully decoupled from path GC.
///
/// Batched: the steps run in ONE transaction for SWEEP_BATCH_SIZE
/// paths at a time. If `dry_run`: do the work, compute stats, then
/// `ROLLBACK` instead of `COMMIT` — operators can see what WOULD
/// be deleted without committing.
///
/// The chunk-backend parameter is no longer consulted by the path
/// sweep (it used to drive the chunk enqueue's `key_for`); it stays in
/// the signature, underscore-bound, until the legacy chunk machinery
/// it belonged to is deleted wholesale in the writer-removal release,
/// so this retirement stays a pure reader removal.
///
/// `shutdown` is checked at each batch boundary (BEFORE `pool.begin`).
/// If fired, returns [`SweepAbort::Shutdown`] — the in-progress batch
/// already committed (previous iteration), the next batch never
/// starts. Safe point: no transaction open, no locks held other than
/// the caller's advisory GC lock (which the caller releases).
/// What one sweep settled: the stats plus the SET of paths it swept
/// (deleted live, or WOULD have deleted under dry_run — the per-batch
/// computation is identical, taken before the savepoint rollback).
/// The swept set feeds the dry-run collect cycle's simulated-sweep
/// exclusion (bug_199): the chunk estimate must be computed against
/// post-sweep state, and the type makes every shadow caller state its
/// sweep composition.
pub struct SweepOutcome {
    pub stats: GcStats,
    pub swept_paths: Vec<Vec<u8>>,
}

pub async fn sweep(
    pool: &PgPool,
    _chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    unreachable: Vec<Vec<u8>>,
    dry_run: bool,
    shutdown: &rio_common::signal::Token,
) -> Result<SweepOutcome, SweepAbort> {
    let mut stats = GcStats::default();
    let mut swept_paths: Vec<Vec<u8>> = Vec::new();

    if unreachable.is_empty() {
        // Skip connection acquire + temp-table setup for no-op sweeps.
        return Ok(SweepOutcome { stats, swept_paths });
    }

    // Reset on ANY exit (Ok, SweepAbort, panic). The gauge contract is
    // 0 between sweeps; without this drop guard, an abort left it at
    // the last per-batch set() and read as "sweep stalled".
    struct GaugeReset;
    impl Drop for GaugeReset {
        fn drop(&mut self) {
            metrics::gauge!("rio_store_gc_sweep_paths_remaining").set(0.0);
        }
    }
    let _gauge_reset = GaugeReset;

    // Dedicated connection: the sweep_unreachable temp table below is
    // session-scoped and must survive across the batch-transaction
    // boundaries in the loop. pool.begin() would acquire a FRESH
    // connection each time — temp table invisible. Acquiring once
    // and begin()-ing on this conn keeps the session (and temp table)
    // alive for the whole sweep. Temp table drops automatically when
    // conn returns to the pool and PG eventually recycles it; the
    // defensive DROP IF EXISTS handles the case where a prior sweep
    // crashed mid-run and this call happens to reacquire that same
    // pooled connection with stale state.
    // SessionConn from the FIRST await (merged_bug_223): the sweep's
    // session temp table can no longer ride a pooled connection back
    // to a sibling on ANY exit — clean completion included, the
    // session detaches and the temp table dies with it (the defensive
    // DROP IF EXISTS in setup becomes belt-and-braces).
    let mut session = super::lock::SessionConn::acquire(pool).await?;

    setup_sweep_unreachable(session.conn(), &unreachable).await?;

    // Referrer-first iteration order: Y before its dep Z so a mid-loop
    // resurrection of Y closure-removes Z before Z's batch. Computed
    // BEFORE pass-1 (from the full set) and used for BOTH pass-1 and
    // the delete loop — pass-1's closure_remove drains the temp table,
    // so an order computed AFTER it would skip drained paths and lose
    // their `paths_resurrected` accounting.
    // r[impl store.gc.sweep-referrer-order]
    let ordered = select_sweep_order(session.conn()).await?;
    let total = ordered.len();

    // Pass 1 (whole-sweep): drain resurrections from sweep_unreachable.
    // For each candidate, re-check for a live referrer; if found,
    // closure-delete the candidate and its reference tree from the temp
    // table. After this pass, sweep_unreachable is settled w.r.t.
    // uploads that landed before pass-1 started — so the delete loop
    // below cannot commit Z-in-batch-N before observing that
    // Y-in-batch-N+1 (Y→Z) was resurrected. The delete loop re-runs
    // the same re-check under FOR UPDATE; that remains the
    // LOAD-BEARING guard for uploads landing DURING the sweep.
    for batch in ordered.chunks(SWEEP_BATCH_SIZE) {
        if shutdown.is_cancelled() {
            return Err(SweepAbort::Shutdown);
        }
        let mut tx = sqlx::Connection::begin(&mut **session.conn()).await?;
        for store_path_hash in batch {
            // Cheap PK probe: skip items an earlier closure-delete
            // already removed (avoids the heavier referrer query).
            let still_in: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM sweep_unreachable WHERE path_hash = $1)",
            )
            .bind(store_path_hash)
            .fetch_one(&mut *tx)
            .await?;
            if still_in && recheck_has_live_referrer(&mut tx, store_path_hash).await? {
                closure_remove_from_unreachable(&mut tx, store_path_hash).await?;
            }
        }
        // Only the temp table changed. Always commit (even dry-run)
        // so the delete loop sees the settled state.
        tx.commit().await?;
    }

    for (i, batch) in ordered.chunks(SWEEP_BATCH_SIZE).enumerate() {
        // Progress gauge: paths NOT yet processed (including this batch).
        // Emitted at batch boundary so an operator watching a long sweep
        // sees `remaining` ticking down per SWEEP_BATCH_SIZE commit. Set
        // to 0 after the loop. dry_run included — the gauge measures
        // sweep-loop progress, not committed deletes (that's
        // `gc_path_swept_total`).
        metrics::gauge!("rio_store_gc_sweep_paths_remaining")
            .set((total - i * SWEEP_BATCH_SIZE) as f64);
        // Shutdown check at batch boundary — safe point (no tx
        // open). A large sweep (thousands of batches × ~100ms each)
        // would otherwise survive SIGTERM grace → pod SIGKILLed
        // mid-transaction → next GC run starts from scratch anyway
        // (advisory lock released by connection close). Bailing here
        // is strictly better: committed batches stay committed,
        // caller sees a clean Aborted status.
        if shutdown.is_cancelled() {
            info!(
                swept = stats.paths_deleted,
                remaining = total as u64 - stats.paths_deleted - stats.paths_resurrected,
                "sweep: shutdown signal received, aborting at batch boundary"
            );
            return Err(SweepAbort::Shutdown);
        }
        // Retry-once-on-40P01 (defense-in-depth: the batch takes only
        // manifest-row and narinfo locks now, but PG can still 40P01
        // under index-page-split contention). The `?` propagates
        // SweepAbort::Db on the second failure.
        let (delta, batch_swept) = match sweep_one_batch(session.conn(), batch, dry_run).await {
            Err(e) if is_deadlock(&e) => {
                warn!(error = %e, "sweep: 40P01 on batch tx; retrying once");
                tokio::time::sleep(crate::metadata::jitter()).await;
                sweep_one_batch(session.conn(), batch, dry_run).await?
            }
            r => r?,
        };
        swept_paths.extend(batch_swept);
        stats.paths_deleted += delta.paths_deleted;
        stats.paths_resurrected += delta.paths_resurrected;

        if !dry_run {
            // Per-batch, post-commit: every increment ↔ exactly one
            // committed tx. Survives SweepAbort (prior batches already
            // emitted); never fires under dry_run (rolled back —
            // a counter is a promise of monotonic fact, not a what-if).
            // The S3-key-enqueued counter is no longer emitted here:
            // the sweep enqueues nothing — the collect cycle owns the
            // outbox and increments that counter per collect batch.
            metrics::counter!("rio_store_gc_path_swept_total").increment(delta.paths_deleted);
            metrics::counter!("rio_store_gc_path_resurrected_total")
                .increment(delta.paths_resurrected);
        }
    }

    info!(
        paths_deleted = stats.paths_deleted,
        paths_resurrected = stats.paths_resurrected,
        dry_run,
        "GC sweep complete (path-level only; chunk collection is the collect cycle's job)"
    );

    Ok(SweepOutcome { stats, swept_paths })
}

/// SQLSTATE 40P01 (deadlock_detected). Same check as
/// `MetadataError::from(sqlx::Error)` but on the bare `sqlx::Error`
/// (sweep's batch tx body returns that, not `MetadataError`).
fn is_deadlock(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "40P01")
}

/// One sweep-batch transaction body. Extracted so [`sweep`] can
/// retry-once on 40P01 (PG aborts the whole txn on deadlock).
/// Returns per-batch deltas; caller accumulates.
async fn sweep_one_batch(
    conn: &mut sqlx::PgConnection,
    batch: &[Vec<u8>],
    dry_run: bool,
) -> Result<(GcStats, Vec<Vec<u8>>), sqlx::Error> {
    let mut delta = GcStats::default();
    let mut swept: Vec<Vec<u8>> = Vec::new();
    let mut tx = conn.begin().await?;

    // Within-batch two-pass: lock + re-check every batch item before
    // any narinfo DELETE. The whole-sweep resurrection drain above
    // settled sweep_unreachable for uploads that landed BEFORE the
    // sweep; this remaining split + the still_unreachable filter
    // below catch a PutPath landing DURING this delete loop where
    // the resurrecting path is later in the same batch.
    let mut to_delete: Vec<&Vec<u8>> = Vec::with_capacity(batch.len());

    for store_path_hash in batch {
        // Step 1: lock the path's manifest row. The FOR UPDATE means a
        // concurrent PutPath for the SAME path blocks until we COMMIT
        // (prevents re-upload mid-sweep). The sweep no longer reads
        // chunk_list and never touches `chunks`: a swept path's
        // now-unreferenced chunks are picked up by the collect cycle
        // once they age past grace (chunk GC decoupled from path GC).
        let _locked: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM manifests WHERE store_path_hash = $1 FOR UPDATE")
                .bind(store_path_hash)
                .fetch_optional(&mut *tx)
                .await?;

        // Step 1b: reference re-check. Mark's CTE took a
        // point-in-time MVCC snapshot; a PutPath that committed
        // AFTER that snapshot (during mark, or between mark and
        // now) may have written references=[this_path] —
        // including `'uploading'` placeholders, which carry
        // references from insert. Re-check via GIN index before
        // deleting. If found: skip, increment resurrected metric.
        // I-192: this is the LOAD-BEARING mark-vs-PutPath guard
        // (there is no advisory lock).
        // r[impl store.gc.sweep-recheck+2]
        //
        // The subquery resolves hash→path because narinfo."references"
        // is TEXT[] (store_path strings, not hashes). The GIN index
        // (migration 008) makes `"references" @> ARRAY[$path]` an
        // index scan. I-145: the previous `$path = ANY("references")`
        // form is semantically equivalent but does NOT use GIN — PG's
        // array-GIN opclass supports `@>`/`<@`/`&&`/`=` only, and the
        // planner does not rewrite `scalar = ANY(arrcol)` into `@>`.
        // At 100k+ narinfo rows that was a ~1.3s seqscan per swept
        // path. EXPLAIN-verified: `@>` → Bitmap Index Scan on
        // idx_narinfo_references_gin even with the InitPlan subquery.
        //
        // The NOT EXISTS anti-join against sweep_unreachable
        // excludes referrers that are themselves in the unreachable
        // set. Without this, mutual-reference cycles (A→B, B→A) and
        // self-references (A→A) are never swept: the re-check sees
        // an intra-set referrer and skips both paths forever. The
        // temp table holds the WHOLE `unreachable` set (not just
        // `batch`) — a cycle may span SWEEP_BATCH_SIZE boundaries.
        // r[impl store.gc.sweep-cycle-reclaim]
        //
        // Also re-check `scheduler_live_pins`: a scheduler dispatch
        // that landed between mark and now is a direct root on THIS
        // path that mark's snapshot missed. The table keys on
        // store_path_hash (first index column) so the EXISTS is a
        // point probe.
        //
        // Also re-check `path_tenants` (mark seed f): a scheduler
        // merge-time tenant attribution (`upsert_path_tenants_raw`
        // from `apply_cached_hits`/`reconcile_preexisting`) that
        // landed between mark and now writes ONLY to path_tenants —
        // an all-cache-hit merge never dispatches, so no
        // scheduler_live_pins row, no narinfo write. PK on
        // (store_path_hash, tenant_id) → point probe.
        if recheck_has_live_referrer(&mut tx, store_path_hash).await? {
            tracing::debug!(
                store_path_hash = %hex::encode(store_path_hash),
                "GC sweep: path resurrected (new referrer after mark), skipping"
            );
            delta.paths_resurrected += 1;
            // Transitive resurrection: this path is now live, so
            // its own references (and theirs, recursively) must
            // not be excluded by the anti-join above when later
            // batch entries are checked.
            closure_remove_from_unreachable(&mut tx, store_path_hash).await?;
            continue;
        }
        to_delete.push(store_path_hash);
    }

    // A closure-delete from a LATER item in the lock loop above may
    // have removed an EARLIER candidate from sweep_unreachable.
    // One batch probe.
    let still_unreachable: std::collections::HashSet<Vec<u8>> = if to_delete.is_empty() {
        std::collections::HashSet::new()
    } else {
        let candidate_hashes: Vec<Vec<u8>> = to_delete.iter().map(|h| (*h).clone()).collect();
        sqlx::query_scalar(
            "SELECT path_hash FROM sweep_unreachable WHERE path_hash = ANY($1::bytea[])",
        )
        .bind(&candidate_hashes)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect()
    };

    // SAVEPOINT: under dry_run we ROLLBACK TO this point, undoing the
    // narinfo/realisation/path_tenants DELETEs but KEEPING the
    // closure_remove temp-table mutations above (so batch N+1's
    // still_unreachable probe sees Y's resurrection of Z). Without
    // this, dry-run rolled back the closure_remove and re-counted Z in
    // batch N+1 → over-reported paths_deleted.
    // r[impl store.gc.dry-run+3]
    sqlx::query("SAVEPOINT sweep_deletes")
        .execute(&mut *tx)
        .await?;

    for store_path_hash in to_delete {
        if !still_unreachable.contains(store_path_hash) {
            delta.paths_resurrected += 1;
            continue;
        }

        if !delete_swept_path(&mut tx, store_path_hash).await? {
            // narinfo already gone (concurrent sweep? shouldn't
            // happen under FOR UPDATE).
            continue;
        }
        delta.paths_deleted += 1;
        // The settled swept set — computed BEFORE the dry-run
        // rollback below, so it is identical under both arms (the
        // shadow collect's simulated-sweep input, bug_199).
        swept.push((*store_path_hash).clone());
    }

    if dry_run {
        // Rollback DELETES only; closure_remove temp-table writes
        // (above the savepoint) survive the outer commit.
        sqlx::query("ROLLBACK TO SAVEPOINT sweep_deletes")
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok((delta, swept))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{ChunkSeed, StoreSeed, TenantSeed, mem_backend, path_hash};
    use rio_test_support::TestDb;
    use rio_test_support::fixtures::test_store_path;
    use rstest::rstest;

    /// Never-cancelled token for sweep tests that don't exercise
    /// the shutdown path.
    fn no_shutdown() -> rio_common::signal::Token {
        rio_common::signal::Token::new()
    }

    /// Sweep must DELETE realisations rows pointing to swept paths.
    /// realisations has NO FK to narinfo (002_store.sql:134); without
    /// the explicit DELETE, dangling rows → wopQueryRealisation returns
    /// a path that 404s on fetch.
    // r[verify store.realisation.gc-sweep]
    #[tokio::test]
    async fn sweep_deletes_realisations() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed a complete path + a realisation pointing to it.
        let path = test_store_path("sweep-target");
        let hash = StoreSeed::raw_path(&path).seed(&db.pool).await;
        sqlx::query(
            "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash) \
             VALUES ($1, 'out', $2, $3)",
        )
        .bind(vec![0x11u8; 32])
        .bind(path)
        .bind(vec![0x22u8; 32])
        .execute(&db.pool)
        .await
        .unwrap();

        // Sweep the path.
        let stats = sweep(&db.pool, None, vec![hash.clone()], false, &no_shutdown())
            .await
            .unwrap()
            .stats;
        assert_eq!(stats.paths_deleted, 1);

        // narinfo gone (CASCADE took manifests too).
        let narinfo_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM narinfo")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(narinfo_count, 0);

        // Realisation ALSO gone (explicit DELETE, not CASCADE).
        let realisations_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM realisations")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            realisations_count, 0,
            "sweep should delete realisations pointing to swept path (no FK CASCADE)"
        );
    }

    /// merged_bug_019: dry-run must NOT increment any of the three
    /// sweep counters (swept/enqueued/resurrected). Before the fix,
    /// `_resurrected_total` was emitted inline pre-rollback, so
    /// repeated `--dry-run` invocations inflated it.
    ///
    /// The mid-sweep-abort case (batch 1 commits, batch 2 aborts,
    /// counter==batch1) is correct-by-construction with per-batch
    /// post-commit emission and shares the same root cause this test
    /// proves moved; not deterministically unit-testable without
    /// instrumenting the loop.
    #[tokio::test]
    async fn sweep_dry_run_emits_no_counters() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;

        // P: in unreachable list. Q: references P → P resurrects.
        let p = test_store_path("dryrun-resurrected");
        let p_hash = StoreSeed::raw_path(&p).seed(&db.pool).await;
        StoreSeed::path("dryrun-referrer")
            .with_refs(&[&p])
            .seed(&db.pool)
            .await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        let stats = sweep(&db.pool, None, vec![p_hash], true, &no_shutdown())
            .await
            .unwrap()
            .stats;
        assert_eq!(stats.paths_resurrected, 1, "P resurrected (stats)");
        assert_eq!(stats.paths_deleted, 0);

        assert_eq!(
            rec.get("rio_store_gc_path_resurrected_total{}"),
            0,
            "dry-run rolled back → resurrected counter must NOT fire; saw {:?}",
            rec.all_keys()
        );
        assert_eq!(rec.get("rio_store_gc_path_swept_total{}"), 0);
        assert_eq!(rec.get("rio_store_gc_s3_key_enqueued_total{}"), 0);
    }

    /// merged_bug_019: real sweep (dry_run=false) — all three counters
    /// must equal the corresponding `stats` field. Locks the contract
    /// that per-batch deltas sum to the final stats.
    #[tokio::test]
    async fn sweep_counters_match_stats() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let h1 = StoreSeed::path("ctr-a").seed(&db.pool).await;
        let h2 = StoreSeed::path("ctr-b").seed(&db.pool).await;
        let h3 = StoreSeed::path("ctr-c").seed(&db.pool).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        let stats = sweep(&db.pool, None, vec![h1, h2, h3], false, &no_shutdown())
            .await
            .unwrap()
            .stats;
        assert_eq!(stats.paths_deleted, 3);
        assert_eq!(stats.paths_resurrected, 0);

        assert_eq!(
            rec.get("rio_store_gc_path_swept_total{}"),
            stats.paths_deleted
        );
        assert_eq!(
            rec.get("rio_store_gc_path_resurrected_total{}"),
            stats.paths_resurrected
        );
        assert_eq!(
            rec.get("rio_store_gc_s3_key_enqueued_total{}"),
            stats.s3_keys_enqueued
        );
    }

    /// Dry-run: compute stats but ROLLBACK. Nothing actually deleted.
    #[tokio::test]
    async fn sweep_dry_run_rolls_back() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = StoreSeed::path("dryrun").seed(&db.pool).await;

        let stats = sweep(&db.pool, None, vec![hash.clone()], true, &no_shutdown())
            .await
            .unwrap()
            .stats;
        // Stats SHOW the path would be deleted.
        assert_eq!(stats.paths_deleted, 1);

        // But narinfo still there (rolled back).
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM narinfo")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "dry-run should roll back");
    }

    /// Sweep's per-path reference re-check catches paths that gained
    /// a new referrer AFTER mark. This simulates the race: mark
    /// declared P unreachable, a PutPath for Q completes with
    /// references=[P], sweep runs and must skip P.
    #[tokio::test]
    async fn sweep_resurrected_path_skipped() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // P: marked unreachable (would be swept).
        let p = test_store_path("resurrected");
        let p_hash = StoreSeed::raw_path(&p).seed(&db.pool).await;

        // Q: references P. Seeded AFTER mark (simulating the race:
        // mark returned [p_hash], THEN PutPath for Q completed).
        StoreSeed::path("referrer")
            .with_refs(&[&p])
            .seed(&db.pool)
            .await;

        // Sweep with P in the unreachable list. The reference re-check should
        // find Q.references=[P] → skip P → paths_resurrected=1.
        let stats = sweep(&db.pool, None, vec![p_hash.clone()], false, &no_shutdown())
            .await
            .unwrap()
            .stats;
        assert_eq!(
            stats.paths_deleted, 0,
            "P should NOT be deleted — Q references it"
        );
        assert_eq!(
            stats.paths_resurrected, 1,
            "P should be counted as resurrected (reference re-check)"
        );

        // P still exists in narinfo.
        let p_exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM narinfo WHERE store_path_hash = $1)")
                .bind(&p_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(p_exists, "P should still exist (resurrected, not swept)");
    }

    /// I-192: same race as `sweep_resurrected_path_skipped`, but the
    /// new referrer is an `'uploading'` PLACEHOLDER (not a complete
    /// path). This is the precise mark-vs-PutPath case the now-removed
    /// `GC_MARK_LOCK_ID` advisory lock guarded against: mark snapshot
    /// at T0 → `insert_manifest_uploading(P, refs=[Q])` commits at T1
    /// → sweep at T2. The re-check scans ALL narinfo (no
    /// `status='complete'` filter), so the placeholder's `references`
    /// column is enough to resurrect Q. The grace window protects P
    /// itself; this proves Q (P's reference, past grace) is also
    /// protected — without an advisory lock.
    // r[verify store.gc.sweep-recheck+2]
    #[tokio::test]
    async fn sweep_recheck_sees_uploading_placeholder() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Q: old, no roots — mark would (and did) declare unreachable.
        let q = test_store_path("placeholder-ref-target");
        let q_hash = StoreSeed::raw_path(&q)
            .created_hours_ago(48)
            .seed(&db.pool)
            .await;

        // Simulate "mark already returned [Q]" by passing it directly
        // to sweep. Now P's placeholder commits — exactly the race
        // window the lock used to (redundantly) close.
        let p = test_store_path("placeholder-referrer");
        let p_hash = path_hash(&p);
        let inserted = crate::metadata::insert_manifest_uploading(
            &db.pool,
            &p_hash,
            &p,
            std::slice::from_ref(&q),
        )
        .await
        .unwrap();
        assert!(inserted.is_some());

        // Sanity: P is status='uploading', nar_size=0 — a real
        // placeholder, not a complete path.
        let (status, nar_size): (String, i64) = sqlx::query_as(
            "SELECT m.status::text, n.nar_size FROM manifests m \
             JOIN narinfo n USING (store_path_hash) WHERE m.store_path_hash = $1",
        )
        .bind(&p_hash)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(status, "uploading");
        assert_eq!(nar_size, 0);

        // Sweep with Q in unreachable. Re-check must see P's
        // placeholder narinfo.references @> [Q] → resurrect.
        let stats = sweep(&db.pool, None, vec![q_hash.clone()], false, &no_shutdown())
            .await
            .unwrap()
            .stats;
        assert_eq!(
            stats.paths_deleted, 0,
            "Q must NOT be deleted — uploading placeholder P references it"
        );
        assert_eq!(stats.paths_resurrected, 1);

        let q_exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM narinfo WHERE store_path_hash = $1)")
                .bind(&q_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(q_exists, "Q's narinfo must survive sweep");
    }

    /// Transitive resurrection: when Y is resurrected because live P
    /// references it, Y's own dependency Z (also in the unreachable
    /// set) must NOT be swept — Y is now a live referrer of Z.
    /// Without the closure-delete from sweep_unreachable, the
    /// anti-join keeps excluding Y as a referrer of Z and Z is
    /// deleted, leaving live P→Y→Z dangling.
    ///
    /// `forward` [Y, Z]: Y's pass-1 re-check resurrects + closure-
    /// deletes Z; Z's pass-1 re-check then sees Y as live.
    ///
    /// `reverse` [Z, Y]: Z's pass-1 re-check finds no live referrer
    /// (Y is in sweep_unreachable, anti-join excludes it) → Z is a
    /// delete candidate. Y's pass-1 re-check then resurrects and
    /// closure-deletes Z. Pass-2's filter sees Z gone and skips it.
    /// Single-pass would have already deleted Z.
    ///
    /// `cross_batch` [Z, filler, Y] with cfg(test) SWEEP_BATCH_SIZE=2:
    /// Z lands in batch 1, Y in batch 2. Without the whole-sweep
    /// resurrection drain, batch 1's tx commits Z deleted before
    /// batch 2 ever observes P→Y→Z. Filler is genuinely unreachable
    /// → `paths_deleted == 1`.
    #[rstest]
    #[case::forward(false, false, 0)]
    #[case::reverse(true, false, 0)]
    #[case::cross_batch(true, true, 1)]
    #[tokio::test]
    async fn sweep_resurrection_is_transitive(
        #[case] z_first: bool,
        #[case] with_filler: bool,
        #[case] expected_deleted: u64,
    ) {
        const _: () = assert!(
            SWEEP_BATCH_SIZE == 2,
            "cross_batch case assumes batch size 2"
        );
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Z: leaf, marked unreachable.
        let z = test_store_path("transitive-z");
        let z_hash = StoreSeed::raw_path(&z).seed(&db.pool).await;
        // Y: references Z, marked unreachable.
        let y = test_store_path("transitive-y");
        let y_hash = StoreSeed::raw_path(&y)
            .with_refs(&[&z])
            .seed(&db.pool)
            .await;
        // P: references Y. NOT in unreachable (the post-mark live
        // referrer that triggers Y's resurrection).
        StoreSeed::path("transitive-p")
            .with_refs(&[&y])
            .seed(&db.pool)
            .await;

        let unreachable = match (z_first, with_filler) {
            (false, _) => vec![y_hash.clone(), z_hash.clone()],
            (true, false) => vec![z_hash.clone(), y_hash.clone()],
            (true, true) => {
                let filler = StoreSeed::path("transitive-filler").seed(&db.pool).await;
                vec![z_hash.clone(), filler, y_hash.clone()]
            }
        };

        let stats = sweep(&db.pool, None, unreachable, false, &no_shutdown())
            .await
            .unwrap()
            .stats;
        assert_eq!(stats.paths_deleted, expected_deleted);
        assert_eq!(stats.paths_resurrected, 2, "Y resurrected by P; Z by Y");

        for (h, name) in [(&y_hash, "Y"), (&z_hash, "Z")] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM narinfo WHERE store_path_hash = $1)",
            )
            .bind(h)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert!(exists, "{name} must survive (transitive resurrection)");
        }
    }

    /// Re-check must consult `scheduler_live_pins`: a scheduler dispatch
    /// between mark and sweep is a direct root mark's snapshot missed.
    #[tokio::test]
    async fn sweep_recheck_sees_late_pins() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Q: pinned via scheduler_live_pins after mark. `query_as!`
        // into the shared `LivePin` struct = compile-time anchor for
        // the column shape `recheck_has_live_referrer` reads (its
        // own SQL can't be macro-checked — it joins a session temp
        // table sqlx-prepare can't see).
        let q_hash = StoreSeed::path("late-live-pin").seed(&db.pool).await;
        let _pin = sqlx::query_as!(
            rio_migrations::schema::LivePin,
            "INSERT INTO scheduler_live_pins (store_path_hash, drv_hash) \
             VALUES ($1, 'drv') RETURNING store_path_hash, drv_hash",
            &q_hash,
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();

        // R: NOT pinned — control. Sweep should delete it.
        let r_hash = StoreSeed::path("unpinned").seed(&db.pool).await;

        let stats = sweep(
            &db.pool,
            None,
            vec![q_hash.clone(), r_hash.clone()],
            false,
            &no_shutdown(),
        )
        .await
        .unwrap()
        .stats;
        assert_eq!(stats.paths_deleted, 1);
        assert_eq!(stats.paths_resurrected, 1);

        let survivor: Vec<u8> = sqlx::query_scalar("SELECT store_path_hash FROM narinfo")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            survivor, q_hash,
            "live-pinned path survives; unpinned swept"
        );
    }

    /// Sweep must DELETE path_tenants rows for swept paths.
    /// path_tenants has NO FK to narinfo (012_path_tenants.sql);
    /// without the explicit DELETE, orphaned rows survive and grant
    /// wrong-tenant visibility when a different tenant later
    /// re-uploads the same store path.
    // r[verify store.gc.sweep-path-tenants+1]
    #[tokio::test]
    async fn sweep_deletes_path_tenants() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed path + tenant + path_tenants row. The row must be
        // STALE (outside the tenant's retention window) — a fresh row
        // is mark seed (f) and now resurrects via the recheck.
        let hash = StoreSeed::path("tenant-swept").seed(&db.pool).await;
        let tenant_id = TenantSeed::new("sweeper")
            .with_retention_hours(1)
            .seed(&db.pool)
            .await;
        sqlx::query(
            "INSERT INTO path_tenants (store_path_hash, tenant_id, first_referenced_at) \
             VALUES ($1, $2, now() - interval '2 hours')",
        )
        .bind(&hash)
        .bind(tenant_id)
        .execute(&db.pool)
        .await
        .unwrap();

        // Sweep.
        let stats = sweep(&db.pool, None, vec![hash.clone()], false, &no_shutdown())
            .await
            .unwrap()
            .stats;
        assert_eq!(stats.paths_deleted, 1);

        // path_tenants row ALSO gone (explicit DELETE, not CASCADE).
        let pt_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM path_tenants")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            pt_count, 0,
            "sweep should delete path_tenants rows for swept path (no FK CASCADE)"
        );
    }

    /// Mutual-reference cycles (A→B, B→A) and self-references (A→A)
    /// must be swept when both sides are in the unreachable set.
    /// Without the sweep_unreachable anti-join in the re-check, the
    /// re-check sees an intra-set referrer and skips both forever.
    // r[verify store.gc.sweep-cycle-reclaim]
    #[tokio::test]
    async fn sweep_reclaims_two_cycle() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // A↔B cycle: A references B, B references A.
        let path_a = test_store_path("cycle-a");
        let path_b = test_store_path("cycle-b");
        let hash_a = StoreSeed::raw_path(&path_a)
            .with_refs(&[&path_b])
            .seed(&db.pool)
            .await;
        let hash_b = StoreSeed::raw_path(&path_b)
            .with_refs(&[&path_a])
            .seed(&db.pool)
            .await;

        // Self-reference: C→C.
        let path_c = test_store_path("self-ref");
        let hash_c = StoreSeed::raw_path(&path_c)
            .with_refs(&[&path_c])
            .seed(&db.pool)
            .await;

        // Sweep all three. The re-check must exclude intra-batch
        // referrers → all three swept, none stuck at resurrected.
        let stats = sweep(
            &db.pool,
            None,
            vec![hash_a, hash_b, hash_c],
            false,
            &no_shutdown(),
        )
        .await
        .unwrap()
        .stats;
        assert_eq!(
            stats.paths_deleted, 3,
            "A↔B cycle + C self-ref all swept (intra-batch referrers excluded)"
        );
        assert_eq!(
            stats.paths_resurrected, 0,
            "no path should be stuck at resurrected — cycle members are \
             NOT genuine referrers"
        );

        // All narinfo gone.
        let narinfo_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM narinfo")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(narinfo_count, 0, "all three narinfo rows deleted");
    }

    /// If nobody references the path, sweep proceeds normally (no false-positive resurrection).
    #[tokio::test]
    async fn sweep_unreferenced_path_deleted() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = StoreSeed::path("unreferenced").seed(&db.pool).await;

        let stats = sweep(&db.pool, None, vec![hash], false, &no_shutdown())
            .await
            .unwrap()
            .stats;
        assert_eq!(stats.paths_deleted, 1);
        assert_eq!(stats.paths_resurrected, 0);
    }

    // r[verify store.gc.two-phase+2]
    /// Sweep a path WITH chunk_list (chunked storage): the sweep
    /// deletes the path rows only and leaves the chunks completely
    /// untouched (no decrement, no soft-delete, no outbox row); the
    /// following collect cycle is what soft-deletes + enqueues them
    /// once they are unreferenced and past grace.
    #[tokio::test]
    async fn sweep_leaves_chunks_to_collect_cycle() {
        use crate::backend::ChunkBackend;
        use crate::manifest::{Manifest, ManifestEntry};
        use std::sync::Arc;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = test_store_path("chunked");
        let sp_hash = path_hash(&path);

        // Seed two chunks, old enough to be outside the collect grace
        // window once the path is swept.
        let chunk_h1 = ChunkSeed::new(0xAA)
            .with_size(1000)
            .age_secs(3600)
            .seed(&db.pool)
            .await;
        let chunk_h2 = ChunkSeed::new(0xBB)
            .with_size(2000)
            .age_secs(3600)
            .seed(&db.pool)
            .await;

        // Seed narinfo + manifest (chunked: inline_blob NULL → StoreSeed
        // default). manifest_data with chunk_list seeded separately.
        let seeded = StoreSeed::raw_path(&path).seed(&db.pool).await;
        assert_eq!(seeded, sp_hash);
        let chunk_list = Manifest {
            entries: vec![
                ManifestEntry {
                    hash: chunk_h1,
                    size: 1000,
                },
                ManifestEntry {
                    hash: chunk_h2,
                    size: 2000,
                },
            ],
        }
        .serialize();
        sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
            .bind(&sp_hash)
            .bind(&chunk_list)
            .execute(&db.pool)
            .await
            .unwrap();

        let backend: Arc<dyn ChunkBackend> = mem_backend();
        let stats = sweep(
            &db.pool,
            Some(&backend),
            vec![sp_hash.clone()],
            false,
            &no_shutdown(),
        )
        .await
        .unwrap()
        .stats;

        assert_eq!(stats.paths_deleted, 1);
        assert_eq!(stats.chunks_deleted, 0, "the sweep reports no chunk work");
        assert_eq!(stats.s3_keys_enqueued, 0);
        assert_eq!(stats.bytes_freed, 0);

        // narinfo + manifest gone (CASCADE); chunks untouched.
        let narinfo_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM narinfo")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(narinfo_count, 0);
        let rows: Vec<(bool,)> = sqlx::query_as("SELECT deleted FROM chunks ORDER BY blake3_hash")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "the sweep leaves the chunk rows in place");
        for (deleted,) in &rows {
            assert!(!deleted, "the sweep no longer soft-deletes chunks");
        }
        let enqueued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(enqueued, 0, "the sweep no longer enqueues S3 keys");

        // The following collect cycle picks the now-unreferenced,
        // past-grace chunks up: soft-deleted + enqueued there.
        let report = super::super::collect::collect_cycle(
            &db.pool,
            Some(&backend),
            CHUNK_GRACE_SECS,
            super::super::collect::CollectMode::Live,
            None,
            &crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(report.victims_collected, 2);
        let (deleted_chunks, pending): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM chunks WHERE deleted), \
                    (SELECT COUNT(*) FROM pending_s3_deletes)",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(deleted_chunks, 2);
        assert_eq!(pending, 2);
    }

    /// bug_176: `rio_store_gc_sweep_paths_remaining` MUST be 0 after
    /// any exit. Seed 3 paths, cancel the shutdown token BEFORE sweep
    /// so the first batch-boundary check fires. Before the scopeguard,
    /// the gauge stayed at 3.0 (set at the per-batch boundary, never
    /// reset on Err(Shutdown)).
    #[tokio::test]
    async fn sweep_paths_remaining_reset_on_abort() {
        use rio_test_support::metrics::CountingRecorder;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let h1 = StoreSeed::path("rem-a").seed(&db.pool).await;
        let h2 = StoreSeed::path("rem-b").seed(&db.pool).await;
        let h3 = StoreSeed::path("rem-c").seed(&db.pool).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        let shutdown = rio_common::signal::Token::new();
        shutdown.cancel();
        let r = sweep(&db.pool, None, vec![h1, h2, h3], false, &shutdown).await;
        assert!(matches!(r, Err(SweepAbort::Shutdown)));

        assert_eq!(
            rec.gauge_value("rio_store_gc_sweep_paths_remaining{}"),
            Some(0.0),
            "gauge MUST be reset on abort; saw {:?}",
            rec.gauge_names()
        );
    }

    /// bug_111 + bug_331: `select_sweep_order` returns Y before its
    /// dep Z; dry-run on Y,Z (Y resurrected by live P→Y) reports
    /// `paths_deleted=0` (Z transitively resurrected via the
    /// committed closure_remove).
    // r[verify store.gc.sweep-referrer-order]
    // r[verify store.gc.dry-run+3]
    #[tokio::test]
    async fn sweep_referrer_first_and_dry_run_closure_survives() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Z leaf; Y→Z; live P→Y. Filler W to span SWEEP_BATCH_SIZE=2.
        let z = test_store_path("ord-z");
        let z_hash = StoreSeed::raw_path(&z).seed(&db.pool).await;
        let y = test_store_path("ord-y");
        let y_hash = StoreSeed::raw_path(&y)
            .with_refs(&[&z])
            .seed(&db.pool)
            .await;
        let w_hash = StoreSeed::path("ord-filler").seed(&db.pool).await;
        StoreSeed::path("ord-p")
            .with_refs(&[&y])
            .seed(&db.pool)
            .await;

        // Ordering: Y before Z (referrer-first). Probe via direct call.
        let mut conn = db.pool.acquire().await.unwrap();
        setup_sweep_unreachable(&mut conn, &[y_hash.clone(), z_hash.clone()])
            .await
            .unwrap();
        let order = select_sweep_order(&mut conn).await.unwrap();
        let iy = order.iter().position(|h| *h == y_hash).unwrap();
        let iz = order.iter().position(|h| *h == z_hash).unwrap();
        assert!(iy < iz, "Y (referrer) must precede Z (dep): {iy} < {iz}");
        drop(conn);

        // Dry-run with Z, W, Y: Y resurrects (P→Y), closure-removes Z;
        // savepoint commits the closure_remove → Z NOT counted in
        // batch N+1. Filler W is genuinely deleted. Before the fix,
        // dry-run rolled back the closure_remove and reported Z
        // deleted too.
        let stats = sweep(
            &db.pool,
            None,
            vec![z_hash.clone(), w_hash, y_hash.clone()],
            true,
            &no_shutdown(),
        )
        .await
        .unwrap()
        .stats;
        assert_eq!(stats.paths_resurrected, 2, "Y by P; Z by Y");
        assert_eq!(
            stats.paths_deleted, 1,
            "dry-run: only filler W deleted (Z transitively resurrected)"
        );

        // Nothing actually deleted (dry-run).
        let z_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM narinfo WHERE store_path_hash = $1)")
                .bind(&z_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(z_exists);
    }

    /// bug_122: diamond `C1→Y→Z, C2→Z` — Z must be at layer 2
    /// (longest path), strictly AFTER its referrer Y at layer 1. The
    /// previous `MIN(d)` CTE gave Z layer 1 (via C2), tying with Y;
    /// hash-order could place Z before Y across a batch boundary →
    /// mid-sweep `PutPath(P,[Y])` left live `Y→deleted Z`.
    // r[verify store.gc.sweep-referrer-order]
    #[tokio::test]
    async fn select_sweep_order_diamond_longest_path() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Names chosen so hash(Z) < hash(Y): old MIN(d) tied Y and Z
        // at depth 1, hash-tiebreak put Z FIRST → assertion fails on
        // d30227bd. (With "diamond-y"/"diamond-z" hash(Y) < hash(Z)
        // and the test was vacuous.)
        let z = test_store_path("diamond-z0");
        let z_hash = StoreSeed::raw_path(&z).seed(&db.pool).await;
        let y = test_store_path("diamond-y0");
        assert!(
            path_hash(&z) < path_hash(&y),
            "fixture invariant: hash(Z) < hash(Y) so MIN(d) tiebreak would mis-order"
        );
        let y_hash = StoreSeed::raw_path(&y)
            .with_refs(&[&z])
            .seed(&db.pool)
            .await;
        let c1_hash = StoreSeed::path("diamond-c1")
            .with_refs(&[&y])
            .seed(&db.pool)
            .await;
        let c2_hash = StoreSeed::path("diamond-c2")
            .with_refs(&[&z])
            .seed(&db.pool)
            .await;

        let mut conn = db.pool.acquire().await.unwrap();
        setup_sweep_unreachable(
            &mut conn,
            &[
                c1_hash.clone(),
                c2_hash.clone(),
                y_hash.clone(),
                z_hash.clone(),
            ],
        )
        .await
        .unwrap();
        let order = select_sweep_order(&mut conn).await.unwrap();
        let pos = |h: &Vec<u8>| order.iter().position(|x| x == h).unwrap();

        assert!(
            pos(&y_hash) < pos(&z_hash),
            "Y (referrer) must precede Z (dep) — longest-path layering, not MIN(d)"
        );
        assert!(
            pos(&c1_hash) < pos(&y_hash),
            "C1 (layer 0) before Y (layer 1)"
        );
        assert!(
            pos(&c2_hash) < pos(&z_hash),
            "C2 (layer 0) before Z (layer 2)"
        );
        assert_eq!(order.len(), 4);
    }

    /// bug_102: K stacked diamonds → previous `UNION ALL` + per-row
    /// `visited[]` CTE enumerated 2^K walks (exponential). Kahn is
    /// O(nodes+edges); 20 stacked diamonds (40 nodes, 60 edges) must
    /// complete well under 5s. On d30227bd this materialized 2^20 ≈ 1M
    /// rows × ~1.2KB visited[] each.
    #[tokio::test]
    async fn select_sweep_order_stacked_diamonds_linear() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Layer i has nodes (i,0) and (i,1); each references both
        // nodes of layer i+1. Bottom layer (K) is two leaf nodes.
        const K: usize = 20;
        let path = |i: usize, j: usize| test_store_path(&format!("stack-{i}-{j}"));
        let mut all_hashes = Vec::new();
        // Seed bottom-up so refs always point at already-seeded paths.
        for j in 0..2 {
            all_hashes.push(StoreSeed::raw_path(path(K, j)).seed(&db.pool).await);
        }
        for i in (0..K).rev() {
            let refs = [path(i + 1, 0), path(i + 1, 1)];
            for j in 0..2 {
                all_hashes.push(
                    StoreSeed::raw_path(path(i, j))
                        .with_refs(&refs)
                        .seed(&db.pool)
                        .await,
                );
            }
        }

        let mut conn = db.pool.acquire().await.unwrap();
        setup_sweep_unreachable(&mut conn, &all_hashes)
            .await
            .unwrap();
        let order = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            select_sweep_order(&mut conn),
        )
        .await
        .expect("select_sweep_order must be linear in |nodes|+|edges|, not 2^K")
        .unwrap();
        assert_eq!(order.len(), 2 * (K + 1));
    }

    /// Kahn layering: cycle members (indeg never reaches 0) sort
    /// LAST. Preserves the previous `COALESCE(d, INT_MAX)` semantics
    /// the within-batch `still_unreachable` probe relies on.
    #[tokio::test]
    async fn select_sweep_order_cycle_members_last() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // A↔B cycle; leaf C with no refs.
        let pa = test_store_path("ordcyc-a");
        let pb = test_store_path("ordcyc-b");
        let a_hash = StoreSeed::raw_path(&pa)
            .with_refs(&[&pb])
            .seed(&db.pool)
            .await;
        let b_hash = StoreSeed::raw_path(&pb)
            .with_refs(&[&pa])
            .seed(&db.pool)
            .await;
        let c_hash = StoreSeed::path("ordcyc-c").seed(&db.pool).await;

        let mut conn = db.pool.acquire().await.unwrap();
        setup_sweep_unreachable(&mut conn, &[a_hash.clone(), b_hash.clone(), c_hash.clone()])
            .await
            .unwrap();
        let order = select_sweep_order(&mut conn).await.unwrap();
        let pos = |h: &Vec<u8>| order.iter().position(|x| x == h).unwrap();
        assert_eq!(order.len(), 3);
        assert!(
            pos(&c_hash) < pos(&a_hash) && pos(&c_hash) < pos(&b_hash),
            "leaf C (layer 0) must precede cycle members A,B (no layer → last)"
        );
    }

    /// bug_161: `path_tenants` is mark seed (f) and IS written
    /// concurrently by the scheduler at merge time (all-cache-hit
    /// merge writes ONLY path_tenants — no pin, no narinfo). A fresh
    /// `path_tenants(X, B)` row landing in the mark→sweep window must
    /// resurrect X. Before the fix, the recheck covered only narinfo
    /// referrers + scheduler_live_pins; X was deleted along with B's
    /// fresh attribution.
    // r[verify store.gc.sweep-recheck+2]
    #[tokio::test]
    async fn recheck_resurrects_on_fresh_path_tenants() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // X: backdated past grace; tenant A's path_tenants row is
        // STALE (outside A's 1h retention). With no other roots, mark
        // would declare X unreachable.
        let x_hash = StoreSeed::path("pt-recheck")
            .created_hours_ago(720)
            .seed(&db.pool)
            .await;
        let tenant_a = TenantSeed::new("pt-a")
            .with_retention_hours(1)
            .seed(&db.pool)
            .await;
        sqlx::query(
            "INSERT INTO path_tenants (store_path_hash, tenant_id, first_referenced_at) \
             VALUES ($1, $2, now() - interval '2 hours')",
        )
        .bind(&x_hash)
        .bind(tenant_a)
        .execute(&db.pool)
        .await
        .unwrap();

        // — mark→sweep window: tenant B's all-cache-hit merge lands —
        // (writes ONLY path_tenants(X, B) at now()).
        let tenant_b = TenantSeed::new("pt-b")
            .with_retention_hours(168)
            .seed(&db.pool)
            .await;
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(&x_hash)
            .bind(tenant_b)
            .execute(&db.pool)
            .await
            .unwrap();

        // Direct recheck probe.
        let mut conn = db.pool.acquire().await.unwrap();
        setup_sweep_unreachable(&mut conn, std::slice::from_ref(&x_hash))
            .await
            .unwrap();
        let mut tx = conn.begin().await.unwrap();
        let live = recheck_has_live_referrer(&mut tx, &x_hash).await.unwrap();
        assert!(
            live,
            "fresh path_tenants(X, B) inside B's retention window must resurrect X"
        );
        tx.rollback().await.unwrap();
        drop(conn);

        // End-to-end: sweep([X]) → X resurrected, NOT deleted; B's
        // fresh attribution row survives.
        let stats = sweep(&db.pool, None, vec![x_hash.clone()], false, &no_shutdown())
            .await
            .unwrap()
            .stats;
        assert_eq!(stats.paths_resurrected, 1);
        assert_eq!(stats.paths_deleted, 0);

        let x_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM narinfo WHERE store_path_hash = $1)")
                .bind(&x_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(x_exists, "X's narinfo survives (resurrected, not swept)");

        let pt_b: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM path_tenants WHERE store_path_hash = $1 AND tenant_id = $2",
        )
        .bind(&x_hash)
        .bind(tenant_b)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(pt_b, 1, "B's fresh path_tenants row survives");
    }
}
