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
use tracing::{info, instrument, warn};

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
        let req = request.into_inner();
        // Round 4 Z21: proto3 `optional uint32` — None = unset (use
        // default 2h), Some(0) = explicit zero grace. Prior code
        // treated 0 as "use default" → impossible to request zero
        // grace (useful for tests + pre-shutdown GC).
        let grace_hours = req.grace_period_hours.unwrap_or(2);

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
            // Round 4 Z7: scopeguard detaches on ANY exit not going
            // through gc_unlock — including task cancellation (client
            // drops the stream → tonic may abort this tokio::spawn) and
            // panics. detach() removes the connection from the pool;
            // dropping the detached connection closes it → PG releases
            // the session-scoped lock. Before this, cancel/panic left
            // the connection in the pool with the lock held → next
            // TriggerGC would get "already running" until sqlx recycled
            // that pooled connection (possibly hours).
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

            // --- Mark phase ---
            // Z2 hybrid: take GC_MARK_LOCK_ID EXCLUSIVE for the mark
            // CTE only (~1s for typical store). PutPath holds this
            // SHARED around placeholder→complete, so mark blocks until
            // no PutPath is mid-write. This guarantees the reference
            // graph seen by mark is consistent: no PutPath can add a
            // new reference to a path mark is about to declare dead.
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
        let req = request.into_inner();
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
                    Err(Status::internal(format!("pin failed: {e}")))
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
        let req = request.into_inner();
        use sha2::Digest;
        let hash: Vec<u8> = sha2::Sha256::digest(req.store_path.as_bytes()).to_vec();

        sqlx::query("DELETE FROM gc_roots WHERE store_path_hash = $1")
            .bind(&hash)
            .execute(&self.pool)
            .await
            .map_err(|e| Status::internal(format!("unpin failed: {e}")))?;

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

    /// X19: concurrent TriggerGC calls are serialized via advisory
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
}
