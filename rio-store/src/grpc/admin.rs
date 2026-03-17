//! StoreAdminService: TriggerGC + PinPath/UnpinPath.
//!
//! The scheduler's `AdminService.TriggerGC` proxies here after
//! populating `extra_roots` from live builds (via GcRoots actor
//! command). Separate service from StoreService so admin ops can
//! have separate RBAC (more privileged than PutPath).

use std::sync::Arc;

use sqlx::PgPool;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{error, info, instrument, warn};

use rio_proto::types::{GcProgress, GcRequest, PinPathRequest, PinPathResponse};

use crate::backend::chunk::ChunkBackend;
use crate::gc;
use crate::gc::{GC_LOCK_ID, GC_MARK_LOCK_ID};

/// StoreAdminService gRPC server.
pub struct StoreAdminServiceImpl {
    pool: PgPool,
    /// Chunk backend for sweep's key_for (enqueue to pending_s3_
    /// deletes). None = inline-only store, sweep does CASCADE
    /// delete only (no chunk refcounting).
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
}

impl StoreAdminServiceImpl {
    pub fn new(pool: PgPool, chunk_backend: Option<Arc<dyn ChunkBackend>>) -> Self {
        Self {
            pool,
            chunk_backend,
        }
    }
}

/// Default threshold for the empty-refs safety gate. 10% is
/// intentionally low: in a healthy post-fix store, empty-ref non-CA
/// paths should be ~0% (only genuinely ref-free outputs like static
/// binaries). 10% gives headroom for legitimate cases without allowing
/// a pre-fix store (where it'd be ~100%) through.
const GC_EMPTY_REFS_THRESHOLD_PCT: f64 = 10.0;

/// Safety gate: if more than `threshold_pct`% of COMPLETE narinfo rows
/// older than `grace_hours` have empty references AND no content
/// address, refuse GC. Protects against running GC on pre-refscan data
/// (worker upload.rs bug where references were never populated).
///
/// CA paths are excluded from the numerator (legitimately ref-free).
/// Paths inside the grace window are excluded entirely (they're
/// protected anyway; their ref-state doesn't matter for this sweep).
///
/// Schema note: narinfo column is `ca` (not `content_address`), and
/// `"references"` is a TEXT[] — empty array is `'{}'` in PG.
// r[impl store.gc.empty-refs-gate]
async fn check_empty_refs_gate(
    pool: &PgPool,
    grace_hours: u32,
    threshold_pct: f64,
) -> Result<(), Status> {
    let row: (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            count(*) FILTER (
                WHERE n."references" = '{}'
                  AND (n.ca IS NULL OR n.ca = '')
            ) AS empty_ref_non_ca,
            count(*) AS total
        FROM narinfo n
        JOIN manifests m USING (store_path_hash)
        WHERE m.status = 'complete'
          AND n.created_at < now() - make_interval(hours => $1::int)
        "#,
    )
    .bind(grace_hours as i32)
    .fetch_one(pool)
    .await
    .map_err(|e| {
        // Don't leak sqlx chain to client; log full detail server-side.
        warn!(error = %e, "empty-refs gate query failed");
        Status::internal("empty-refs gate query failed")
    })?;

    let (empty, total) = row;
    if total == 0 {
        return Ok(()); // nothing sweep-eligible anyway
    }
    let pct = (empty as f64 / total as f64) * 100.0;
    metrics::gauge!("rio_store_gc_empty_refs_pct").set(pct);

    if pct > threshold_pct {
        error!(
            empty_ref_non_ca = empty,
            total,
            pct,
            threshold_pct,
            "GC REFUSED: high empty-refs ratio — store likely contains pre-refscan data"
        );
        return Err(Status::failed_precondition(format!(
            "GC refused: {empty}/{total} ({pct:.1}%) of sweep-eligible paths have empty \
             references (threshold {threshold_pct}%) — worker upload.rs bug? \
             Run backfill first or use force=true to override."
        )));
    }

    if empty > 0 {
        warn!(
            empty_ref_non_ca = empty,
            total, pct, "GC proceeding with some empty-ref paths (below threshold)"
        );
    }
    Ok(())
}

type TriggerGcStream = ReceiverStream<Result<GcProgress, Status>>;

#[tonic::async_trait]
impl rio_proto::StoreAdminService for StoreAdminServiceImpl {
    type TriggerGCStream = TriggerGcStream;

    /// Two-phase GC: mark (recursive CTE from roots) + sweep
    /// (DELETE CASCADE + chunk decrement + pending_s3_deletes).
    /// Streams progress updates: one message after mark (scanned
    /// count), one after sweep (collected + bytes), final with
    /// is_complete=true.
    ///
    /// `grace_period_hours`: None = default (2h). Some(0) = zero
    /// grace (explicit). Protects paths
    /// created in the last N hours from collection even if not
    /// yet referenced.
    ///
    /// `extra_roots`: scheduler-populated live-build output paths.
    /// May not be in narinfo yet (worker hasn't uploaded) — mark's
    /// CTE handles this gracefully (unnest of non-existent path
    /// = 0 rows, the root itself stays in reachable).
    ///
    /// `dry_run`: compute stats, ROLLBACK sweep tx. Operator sees
    /// "would delete N paths, free M bytes" without committing.
    #[instrument(skip(self, request), fields(rpc = "TriggerGC"))]
    async fn trigger_gc(
        &self,
        request: Request<GcRequest>,
    ) -> Result<Response<Self::TriggerGCStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Bound extra_roots BEFORE spawning. Mark runs under
        // GC_MARK_LOCK_ID exclusive; a 10M-element array stalls the CTE
        // on unnest() and blocks every PutPath (shared side of same lock)
        // for the duration. Reuse MAX_BATCH_PATHS — 10k live-build outputs
        // is already implausible; GcRoots actor sends ~tens.
        rio_common::grpc::check_bound(
            "extra_roots",
            req.extra_roots.len(),
            crate::grpc::MAX_BATCH_PATHS,
        )?;
        // Syntactically valid store paths only. Not-in-narinfo is fine
        // (in-flight outputs, mark's seed-d handles it); garbage strings
        // are not — the CTE join on store_path = store_path against junk
        // is dead weight at best, a false match at worst.
        for root in &req.extra_roots {
            crate::grpc::validate_store_path(root)?;
        }

        // proto3 `optional uint32`: None = unset (use default 2h),
        // Some(0) = explicit zero grace (useful for tests + pre-shutdown GC).
        // Distinguishing these matters — treating 0 as "use default"
        // would make zero-grace impossible to request.
        //
        // Clamp at the gRPC boundary too (mark.rs also clamps — defense in
        // depth against callers that bypass admin.rs). u32 > i32::MAX wraps
        // negative → make_interval(hours => negative) → grace protects
        // nothing. 24*365 = one-year ceiling; log shows effective value.
        let grace_hours = req.grace_period_hours.unwrap_or(2).min(24 * 365);

        info!(
            dry_run = req.dry_run,
            grace_hours,
            extra_roots = req.extra_roots.len(),
            "TriggerGC: starting mark phase"
        );

        // Channel for streaming progress. spawn the actual GC
        // so this handler returns quickly and the client gets
        // progress updates as they happen.
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let pool = self.pool.clone();
        let chunk_backend = self.chunk_backend.clone();

        tokio::spawn(async move {
            // --- Concurrency guard: pg_try_advisory_lock ---
            // Two TriggerGC calls → two concurrent mark+sweep.
            // Correctness is OK (FOR UPDATE + rows_affected checks
            // in sweep) but it wastes work, produces misleading
            // stats (GC2 finds everything already swept), and
            // creates lock contention. One-at-a-time via advisory
            // lock; second caller gets an immediate "already
            // running" response.
            //
            // Session-level advisory locks are CONNECTION-scoped;
            // pool.acquire() holds one connection for lock/unlock.
            // If we let the connection return to the pool between
            // lock and unlock, the unlock would go to a DIFFERENT
            // connection → no-op, lock held until connection
            // recycles (leak). Acquiring explicitly prevents that.
            let mut lock_conn = match pool.acquire().await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "TriggerGC: pool acquire for advisory lock failed");
                    let _ = tx
                        .send(Err(Status::internal(format!("pool acquire: {e}"))))
                        .await;
                    return;
                }
            };
            let lock_acquired: bool = match sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(GC_LOCK_ID)
                .fetch_one(&mut *lock_conn)
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "TriggerGC: advisory lock query failed");
                    let _ = tx
                        .send(Err(Status::internal(format!("advisory lock: {e}"))))
                        .await;
                    return;
                }
            };
            if !lock_acquired {
                info!("TriggerGC: another GC is already running, returning early");
                let _ = tx
                    .send(Ok(GcProgress {
                        paths_scanned: 0,
                        paths_collected: 0,
                        bytes_freed: 0,
                        is_complete: true,
                        current_path: "already running (concurrent GC in progress)".into(),
                    }))
                    .await;
                return;
            }
            // lock_conn held for the whole GC; explicit unlock at
            // the end via gc_unlock.
            //
            // scopeguard detaches on ANY exit not going through gc_unlock —
            // including task cancellation (client drops the stream → tonic
            // may abort this tokio::spawn) and panics. detach() removes the
            // connection from the pool; dropping the detached connection
            // closes it → PG releases the session-scoped lock.
            //
            // Without this, cancel/panic would leave the connection in the
            // pool with the lock held → next TriggerGC gets "already running"
            // until sqlx recycles that pooled connection (possibly hours).
            //
            // gc_unlock DEFUSES the scopeguard (ScopeGuard::into_inner)
            // and explicitly unlocks + returns conn to pool (cheaper
            // than detach on the happy path).
            let lock_conn = scopeguard::guard(lock_conn, |c| {
                let _ = c.detach();
            });
            async fn gc_unlock(
                conn: scopeguard::ScopeGuard<
                    sqlx::pool::PoolConnection<sqlx::Postgres>,
                    impl FnOnce(sqlx::pool::PoolConnection<sqlx::Postgres>),
                >,
            ) {
                let mut conn = scopeguard::ScopeGuard::into_inner(conn);
                if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
                    .bind(GC_LOCK_ID)
                    .execute(&mut *conn)
                    .await
                {
                    warn!(error = %e, "TriggerGC: advisory unlock failed");
                }
            }

            // r[impl store.gc.empty-refs-gate]
            // Safety gate: refuse if >threshold% of sweep-eligible narinfo
            // have empty refs (pre-refscan data). Runs AFTER advisory lock
            // so two gated requests don't race on the check. Skip if
            // force=true, but still log so the override is visible.
            if !req.force {
                if let Err(e) =
                    check_empty_refs_gate(&pool, grace_hours, GC_EMPTY_REFS_THRESHOLD_PCT).await
                {
                    let _ = tx.send(Err(e)).await;
                    gc_unlock(lock_conn).await;
                    return;
                }
            } else {
                warn!("TriggerGC: force=true — bypassing empty-refs safety gate");
            }

            // --- Mark phase ---
            // Mark-vs-PutPath lock: take GC_MARK_LOCK_ID EXCLUSIVE for the mark
            // CTE only (~1s for typical store). PutPath takes this SHARED,
            // transaction-scoped — only ~ms around the placeholder insert
            // (NOT held for the full upload; the placeholder narinfo carries
            // its references from commit, so the upload itself is unprotected
            // by design). Mark blocks until no placeholder-insert tx is in
            // flight. This guarantees the reference graph seen by mark is
            // consistent: no PutPath can add a new reference to a path mark
            // is about to declare dead.
            //
            // Uses a SEPARATE connection (mark_lock_conn) from
            // lock_conn (GC_LOCK_ID) — both are session-scoped, but
            // keeping them on separate connections means we can drop
            // mark_lock_conn immediately after mark returns (releasing
            // the PutPath-blocking lock early) while GC_LOCK_ID stays
            // held through sweep.
            let mark_lock_conn = match pool.acquire().await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "TriggerGC: pool acquire for mark lock failed");
                    let _ = tx
                        .send(Err(Status::internal(format!("mark lock acquire: {e}"))))
                        .await;
                    gc_unlock(lock_conn).await;
                    return;
                }
            };
            // scopeguard: if mark fails or task is cancelled, the
            // connection is DETACHED (not returned to pool) → PG
            // auto-releases the session-scoped lock on connection
            // close. On success we explicitly unlock + defuse below.
            let mut mark_lock_guard = scopeguard::guard(mark_lock_conn, |c| {
                // detach() removes from pool; dropping the detached
                // Connection closes it → PG releases session lock.
                let _ = c.detach();
            });

            // Acquire exclusive. Blocks until no PutPath holds shared.
            if let Err(e) = sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(GC_MARK_LOCK_ID)
                .execute(&mut **mark_lock_guard)
                .await
            {
                warn!(error = %e, "TriggerGC: mark advisory lock query failed");
                let _ = tx
                    .send(Err(Status::internal(format!("mark lock: {e}"))))
                    .await;
                gc_unlock(lock_conn).await;
                return; // mark_lock_guard detached by scopeguard
            }

            let unreachable =
                match gc::mark::compute_unreachable(&pool, grace_hours, &req.extra_roots).await {
                    Ok(u) => u,
                    Err(e) => {
                        warn!(error = %e, "TriggerGC: mark phase failed");
                        let _ = tx
                            .send(Err(Status::internal(format!("mark phase: {e}"))))
                            .await;
                        gc_unlock(lock_conn).await;
                        return; // mark_lock_guard detached by scopeguard
                    }
                };

            // Mark done — release the mark lock EXPLICITLY (early,
            // before sweep) and defuse the scopeguard. PutPath can now
            // proceed; sweep's per-path re-check handles any race.
            let mut mark_lock_conn = scopeguard::ScopeGuard::into_inner(mark_lock_guard);
            if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(GC_MARK_LOCK_ID)
                .execute(&mut *mark_lock_conn)
                .await
            {
                warn!(error = %e, "TriggerGC: mark advisory unlock failed (continuing — conn drop will release)");
            }
            drop(mark_lock_conn); // returns to pool (lock already released)

            // Progress after mark: scanned count. We don't have
            // a "total paths" count cheaply (would need COUNT(*)
            // on narinfo), so paths_scanned = unreachable count
            // (what mark found). Not ideal but informative.
            let _ = tx
                .send(Ok(GcProgress {
                    paths_scanned: unreachable.len() as u64,
                    paths_collected: 0,
                    bytes_freed: 0,
                    is_complete: false,
                    current_path: "mark complete, starting sweep".into(),
                }))
                .await;

            info!(
                unreachable = unreachable.len(),
                "TriggerGC: mark complete, starting sweep"
            );

            // --- Sweep phase ---
            let stats =
                match gc::sweep::sweep(&pool, chunk_backend.as_ref(), unreachable, req.dry_run)
                    .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, "TriggerGC: sweep phase failed");
                        let _ = tx
                            .send(Err(Status::internal(format!("sweep phase: {e}"))))
                            .await;
                        gc_unlock(lock_conn).await;
                        return;
                    }
                };

            // Final progress: complete with stats.
            let _ = tx
                .send(Ok(GcProgress {
                    paths_scanned: stats.paths_deleted, // reuse for "found unreachable"
                    paths_collected: stats.paths_deleted,
                    bytes_freed: stats.bytes_freed,
                    is_complete: true,
                    current_path: if req.dry_run {
                        format!(
                            "dry run: would delete {} paths, {} chunks, free {} bytes",
                            stats.paths_deleted, stats.chunks_deleted, stats.bytes_freed
                        )
                    } else {
                        format!(
                            "complete: {} paths deleted, {} chunks, {} S3 keys enqueued, {} bytes freed",
                            stats.paths_deleted, stats.chunks_deleted,
                            stats.s3_keys_enqueued, stats.bytes_freed
                        )
                    },
                }))
                .await;

            gc_unlock(lock_conn).await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// Pin a store path to gc_roots. FK ensures the path exists
    /// in narinfo (CASCADE DELETE removes the pin if narinfo
    /// goes — manual intervention, schema reset).
    #[instrument(skip(self), fields(rpc = "PinPath", path = %request.get_ref().store_path))]
    async fn pin_path(
        &self,
        request: Request<PinPathRequest>,
    ) -> Result<Response<PinPathResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();
        crate::grpc::validate_store_path(&req.store_path)?;
        // Compute store_path_hash from the path. narinfo keys on
        // SHA-256 of the store path string.
        use sha2::Digest;
        let hash: Vec<u8> = sha2::Sha256::digest(req.store_path.as_bytes()).to_vec();

        // INSERT into gc_roots. FK to narinfo: if the path isn't
        // in narinfo, PG rejects with a foreign-key violation.
        // ON CONFLICT DO UPDATE: re-pin updates the source/
        // timestamp (idempotent — pinning an already-pinned path
        // is fine).
        match sqlx::query(
            r#"
            INSERT INTO gc_roots (store_path_hash, source)
            VALUES ($1, $2)
            ON CONFLICT (store_path_hash) DO UPDATE
                SET source = EXCLUDED.source, pinned_at = now()
            "#,
        )
        .bind(&hash)
        .bind(&req.source)
        .execute(&self.pool)
        .await
        {
            Ok(_) => {
                info!(path = %req.store_path, source = %req.source, "pinned");
                Ok(Response::new(PinPathResponse {
                    success: true,
                    message: String::new(),
                }))
            }
            Err(e) => {
                // FK violation (path not in narinfo) → NotFound.
                // Anything else → Internal. Use SQLSTATE 23503
                // (foreign_key_violation) — stable across locales
                // and PG versions, unlike string matching.
                let is_fk_violation = e
                    .as_database_error()
                    .and_then(|d| d.code())
                    .is_some_and(|c| c == "23503");
                if is_fk_violation {
                    Ok(Response::new(PinPathResponse {
                        success: false,
                        message: format!("path not in store: {}", req.store_path),
                    }))
                } else {
                    // Log the full sqlx chain server-side; don't leak it.
                    // Same pattern as grpc::internal_error (mod.rs).
                    Err(crate::grpc::internal_error("PinPath: insert gc_roots", e))
                }
            }
        }
    }

    /// Unpin: DELETE from gc_roots. Idempotent — unpinning a
    /// non-pinned path is a no-op (DELETE rowcount 0).
    #[instrument(skip(self), fields(rpc = "UnpinPath", path = %request.get_ref().store_path))]
    async fn unpin_path(
        &self,
        request: Request<PinPathRequest>,
    ) -> Result<Response<PinPathResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();
        crate::grpc::validate_store_path(&req.store_path)?;
        use sha2::Digest;
        let hash: Vec<u8> = sha2::Sha256::digest(req.store_path.as_bytes()).to_vec();

        sqlx::query("DELETE FROM gc_roots WHERE store_path_hash = $1")
            .bind(&hash)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::grpc::internal_error("UnpinPath: delete gc_roots", e))?;

        info!(path = %req.store_path, "unpinned");
        Ok(Response::new(PinPathResponse {
            success: true,
            message: String::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::StoreAdminService;
    use rio_test_support::TestDb;

    /// PinPath for a path NOT in narinfo → FK violation → success=
    /// false with "path not in store" message. Verifies the SQLSTATE
    /// 23503 check (replacing the old string-match on "foreign key",
    /// which is locale/version-dependent).
    #[tokio::test]
    async fn pin_path_fk_violation_returns_not_found() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // Pin a path that doesn't exist in narinfo. The FK to
        // narinfo.store_path_hash will reject with SQLSTATE 23503.
        let req = Request::new(PinPathRequest {
            store_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-doesnt-exist".into(),
            source: "test".into(),
        });
        let resp = svc.pin_path(req).await.expect("returns Ok(response)");
        let resp = resp.into_inner();
        assert!(!resp.success, "FK violation → success=false");
        assert!(
            resp.message.contains("path not in store"),
            "message should say path not in store, got: {}",
            resp.message
        );
    }

    #[tokio::test]
    async fn pin_path_roundtrip() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // Seed narinfo so the FK passes.
        let path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-exists";
        use sha2::Digest;
        let hash: Vec<u8> = sha2::Sha256::digest(path.as_bytes()).to_vec();
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             VALUES ($1, $2, $3, 42)",
        )
        .bind(&hash)
        .bind(path)
        .bind(vec![0u8; 32])
        .execute(&db.pool)
        .await
        .unwrap();

        // Pin.
        let resp = svc
            .pin_path(Request::new(PinPathRequest {
                store_path: path.into(),
                source: "test-roundtrip".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.success, "pin should succeed");

        // Verify gc_roots has the row.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM gc_roots")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1, "gc_roots has 1 row");

        // Unpin.
        let resp = svc
            .unpin_path(Request::new(PinPathRequest {
                store_path: path.into(),
                source: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.success, "unpin should succeed");

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM gc_roots")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "gc_roots empty after unpin");
    }

    /// Concurrent TriggerGC calls are serialized via advisory
    /// lock. Second call returns immediately with "already running".
    #[tokio::test]
    async fn trigger_gc_advisory_lock_serializes() {
        use futures_util::StreamExt;
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // Acquire the advisory lock MANUALLY on a held connection,
        // simulating an in-flight GC. Then call TriggerGC and
        // verify it returns "already running".
        let mut lock_conn = db.pool.acquire().await.unwrap();
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(GC_LOCK_ID)
            .fetch_one(&mut *lock_conn)
            .await
            .unwrap();
        assert!(locked, "initial manual lock should succeed");

        // TriggerGC should see lock held → return "already running".
        let resp = svc
            .trigger_gc(Request::new(GcRequest {
                dry_run: true,
                grace_period_hours: Some(24),
                extra_roots: vec![],
                force: false,
            }))
            .await
            .unwrap();
        let mut stream = resp.into_inner();
        let msg = stream.next().await.expect("one message").unwrap();
        assert!(msg.is_complete, "should be complete immediately");
        assert!(
            msg.current_path.contains("already running"),
            "should report already running, got: {}",
            msg.current_path
        );

        // Release manual lock; next TriggerGC should proceed.
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(GC_LOCK_ID)
            .execute(&mut *lock_conn)
            .await
            .unwrap();
        drop(lock_conn);

        let resp = svc
            .trigger_gc(Request::new(GcRequest {
                dry_run: true,
                grace_period_hours: Some(24),
                extra_roots: vec![],
                force: false,
            }))
            .await
            .unwrap();
        let mut stream = resp.into_inner();
        // Empty store → mark finds nothing → one "mark complete"
        // progress + one final. Drain to completion.
        let mut last = None;
        while let Some(msg) = stream.next().await {
            last = Some(msg.unwrap());
        }
        let last = last.expect("at least one message");
        assert!(last.is_complete, "final message should be complete");
        assert!(
            !last.current_path.contains("already running"),
            "should NOT be 'already running' after lock released"
        );
    }

    /// T2: extra_roots bounded at MAX_BATCH_PATHS (10_000). 10_001 → reject.
    #[tokio::test]
    async fn trigger_gc_rejects_oversized_extra_roots() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        // Syntactically valid paths (validate_store_path passes each).
        let roots: Vec<String> = (0..crate::grpc::MAX_BATCH_PATHS + 1)
            .map(|i| rio_test_support::fixtures::test_store_path(&format!("r{i}")))
            .collect();
        let err = svc
            .trigger_gc(Request::new(GcRequest {
                extra_roots: roots,
                dry_run: true,
                grace_period_hours: Some(24),
                force: false,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            err.message().contains("extra_roots"),
            "message should name the field, got: {}",
            err.message()
        );
    }

    /// T2: extra_roots rejects syntactically invalid store paths.
    #[tokio::test]
    async fn trigger_gc_rejects_malformed_extra_root() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        let err = svc
            .trigger_gc(Request::new(GcRequest {
                extra_roots: vec!["not-a-store-path".into()],
                dry_run: true,
                grace_period_hours: Some(24),
                force: false,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    /// T5: PinPath rejects malformed store paths (path traversal,
    /// garbage) with INVALID_ARGUMENT, BEFORE touching the DB.
    #[tokio::test]
    async fn pin_path_rejects_malformed() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        let err = svc
            .pin_path(Request::new(PinPathRequest {
                store_path: "../../etc/passwd".into(),
                source: "t".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    /// T5: UnpinPath rejects malformed store paths.
    #[tokio::test]
    async fn unpin_path_rejects_malformed() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        let err = svc
            .unpin_path(Request::new(PinPathRequest {
                store_path: "not-a-path".into(),
                source: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    /// T5: PinPath sqlx errors are scrubbed — generic message only.
    /// Force a non-FK DB error by closing the pool first.
    #[tokio::test]
    async fn pin_path_internal_error_is_scrubbed() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
        // Close the pool: all acquires fail with PoolClosed, which is
        // NOT a FK violation (23503), so it hits the internal_error path.
        db.pool.close().await;
        let err = svc
            .pin_path(Request::new(PinPathRequest {
                store_path: rio_test_support::fixtures::test_store_path("p"),
                source: "t".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Internal);
        // The message must NOT contain sqlx/postgres internals.
        assert!(
            !err.message().to_lowercase().contains("pool"),
            "message leaked pool detail: {}",
            err.message()
        );
        assert!(
            !err.message().to_lowercase().contains("sqlx"),
            "message leaked sqlx: {}",
            err.message()
        );
        // internal_error() emits this generic string.
        assert_eq!(err.message(), "storage operation failed");
    }

    // --- GC empty-refs safety gate (remediation 02) ---

    /// Seed a narinfo+manifests row pair for gate tests.
    /// `created_at` is forced into the past so the row is sweep-eligible
    /// under any non-zero grace window. `ca` and `references` are the
    /// two axes the gate pivots on.
    async fn seed_narinfo_for_gate(pool: &PgPool, name: &str, refs: &[String], ca: Option<&str>) {
        use sha2::Digest;
        let path = rio_test_support::fixtures::test_store_path(name);
        let hash: Vec<u8> = sha2::Sha256::digest(path.as_bytes()).to_vec();
        // created_at = 7 days ago — comfortably past any test grace.
        sqlx::query(
            r#"INSERT INTO narinfo
               (store_path_hash, store_path, nar_hash, nar_size, "references", ca, created_at)
               VALUES ($1, $2, $3, 42, $4, $5, now() - interval '7 days')"#,
        )
        .bind(&hash)
        .bind(&path)
        .bind(vec![0u8; 32])
        .bind(refs)
        .bind(ca)
        .execute(pool)
        .await
        .expect("seed narinfo");
        // Manifest must be 'complete' — gate query joins on this.
        sqlx::query("INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'complete')")
            .bind(&hash)
            .execute(pool)
            .await
            .expect("seed manifest");
    }

    /// Drain the TriggerGC stream to the first Err, or return the final
    /// Ok(GcProgress) if no Err arrives. GC gate failures arrive as an
    /// Err(Status) over the stream (the handler returns Ok(stream) then
    /// the spawned task sends Err).
    async fn drain_gc_stream(
        stream: &mut (impl futures_util::Stream<Item = Result<GcProgress, Status>> + Unpin),
    ) -> Result<GcProgress, Status> {
        use futures_util::StreamExt;
        let mut last_ok = None;
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(p) => last_ok = Some(p),
                Err(e) => return Err(e),
            }
        }
        Ok(last_ok.expect("stream produced at least one message"))
    }

    /// r[verify store.gc.empty-refs-gate]
    /// Store with >10% empty-ref non-CA paths past grace → gate refuses
    /// with FailedPrecondition. Message names the ratio + suggests force.
    #[tokio::test]
    async fn gc_refuses_on_high_empty_ref_ratio() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // 8 empty-ref non-CA + 2 with-refs = 80% empty. Well above 10%.
        for i in 0..8 {
            seed_narinfo_for_gate(&db.pool, &format!("empty-{i}"), &[], None).await;
        }
        let good_ref = vec![rio_test_support::fixtures::test_store_path("dep")];
        for i in 0..2 {
            seed_narinfo_for_gate(&db.pool, &format!("good-{i}"), &good_ref, None).await;
        }

        let resp = svc
            .trigger_gc(Request::new(GcRequest {
                dry_run: true,
                grace_period_hours: Some(1),
                extra_roots: vec![],
                force: false,
            }))
            .await
            .expect("handler returns Ok(stream)");
        let mut stream = resp.into_inner();

        let err = drain_gc_stream(&mut stream)
            .await
            .expect_err("gate should refuse");
        assert_eq!(
            err.code(),
            tonic::Code::FailedPrecondition,
            "gate returns FailedPrecondition, got {:?}: {}",
            err.code(),
            err.message()
        );
        // Message contains ratio + force hint.
        let msg = err.message();
        assert!(
            msg.contains("GC refused"),
            "message should say GC refused: {msg}"
        );
        assert!(
            msg.contains("8/10") && msg.contains("80.0%"),
            "message should show 8/10 (80.0%) ratio: {msg}"
        );
        assert!(
            msg.contains("force=true"),
            "message should hint force override: {msg}"
        );
    }

    /// r[verify store.gc.empty-refs-gate]
    /// Same high-ratio setup, but `force=true` bypasses the gate.
    /// GC proceeds to mark+sweep (dry-run so nothing actually deleted).
    #[tokio::test]
    async fn gc_force_overrides_empty_refs_gate() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // 100% empty-ref non-CA.
        for i in 0..5 {
            seed_narinfo_for_gate(&db.pool, &format!("empty-{i}"), &[], None).await;
        }

        let resp = svc
            .trigger_gc(Request::new(GcRequest {
                dry_run: true, // don't actually sweep, just verify gate is bypassed
                grace_period_hours: Some(1),
                extra_roots: vec![],
                force: true,
            }))
            .await
            .expect("handler returns Ok(stream)");
        let mut stream = resp.into_inner();

        // Gate bypassed → mark+sweep runs → stream ends with Ok(complete).
        let final_msg = drain_gc_stream(&mut stream)
            .await
            .expect("force=true should bypass gate and complete");
        assert!(
            final_msg.is_complete,
            "force=true should let GC complete, got: {final_msg:?}"
        );
        assert!(
            !final_msg.current_path.contains("already running"),
            "should not be the advisory-lock early-return path"
        );
    }

    /// r[verify store.gc.empty-refs-gate]
    /// CA paths with empty refs are NOT counted toward the threshold
    /// (fetchurl outputs etc. legitimately have no runtime deps).
    /// 100% of paths have empty refs but they're all CA → gate passes.
    #[tokio::test]
    async fn gc_gate_excludes_ca_paths_from_ratio() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // All empty-ref but all CA. Numerator = 0, pct = 0%.
        for i in 0..5 {
            seed_narinfo_for_gate(
                &db.pool,
                &format!("ca-{i}"),
                &[],
                Some("fixed:r:sha256:abc"),
            )
            .await;
        }

        let resp = svc
            .trigger_gc(Request::new(GcRequest {
                dry_run: true,
                grace_period_hours: Some(1),
                extra_roots: vec![],
                force: false,
            }))
            .await
            .expect("handler returns Ok(stream)");
        let mut stream = resp.into_inner();

        let final_msg = drain_gc_stream(&mut stream)
            .await
            .expect("CA-only store should pass gate");
        assert!(final_msg.is_complete);
    }

    /// r[verify store.gc.empty-refs-gate]
    /// Paths INSIDE the grace window don't count toward the ratio
    /// (they're protected from sweep anyway). Seed recent empty-ref
    /// paths + use a large grace → denominator = 0 → gate passes.
    #[tokio::test]
    async fn gc_gate_excludes_paths_inside_grace() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

        // seed_narinfo_for_gate sets created_at = 7 days ago.
        // With grace = 30 days (720h), they're INSIDE grace → excluded.
        for i in 0..5 {
            seed_narinfo_for_gate(&db.pool, &format!("recent-{i}"), &[], None).await;
        }

        let resp = svc
            .trigger_gc(Request::new(GcRequest {
                dry_run: true,
                grace_period_hours: Some(720), // 30 days > 7 days
                extra_roots: vec![],
                force: false,
            }))
            .await
            .expect("handler returns Ok(stream)");
        let mut stream = resp.into_inner();

        let final_msg = drain_gc_stream(&mut stream)
            .await
            .expect("paths inside grace don't trip gate");
        assert!(final_msg.is_complete);
    }
}
