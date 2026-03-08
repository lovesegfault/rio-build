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
    /// `grace_period_hours`: 0 = use default (2). Protects paths
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
        let grace_hours = if req.grace_period_hours == 0 {
            2 // default
        } else {
            req.grace_period_hours
        };

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
            // --- Mark phase ---
            let unreachable =
                match gc::mark::compute_unreachable(&pool, grace_hours, &req.extra_roots).await {
                    Ok(u) => u,
                    Err(e) => {
                        warn!(error = %e, "TriggerGC: mark phase failed");
                        let _ = tx
                            .send(Err(Status::internal(format!("mark phase: {e}"))))
                            .await;
                        return;
                    }
                };

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
                // FK violation (path not in narinfo) or connection
                // error. Surface as NotFound or Internal based on
                // the error type. sqlx doesn't expose FK-violation
                // cleanly, so just check the message.
                let msg = e.to_string();
                if msg.contains("foreign key") || msg.contains("violates") {
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
