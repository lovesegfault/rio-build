//! `AdminService.TriggerGC` implementation + the store-size background
//! refresher that feeds `ClusterStatus.store_size_bytes`.

use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;
use tracing::debug;

use rio_proto::types::{GcProgress, GcRequest};

use crate::actor::{ActorCommand, ActorHandle};

/// Spawn a background task that refreshes `store_size_bytes` every 60s
/// via a PG query on the shared store DB. Scheduler already has the pool
/// (same database as the store). Follows the `scheduler_live_pins`
/// cross-layer precedent.
pub fn spawn_store_size_refresh(
    pool: PgPool,
    shutdown: rio_common::signal::Token,
) -> Arc<std::sync::atomic::AtomicU64> {
    let size = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let size_clone = Arc::clone(&size);
    rio_common::task::spawn_monitored("store-size-refresh", async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::debug!("store-size-refresh shutting down");
                    break;
                }
                _ = interval.tick() => {}
            }
            match sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(SUM(nar_size), 0)::bigint FROM narinfo",
            )
            .fetch_one(&pool)
            .await
            {
                Ok(bytes) => {
                    size_clone.store(bytes.max(0) as u64, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "store_size refresh failed");
                }
            }
        }
    });
    size
}

/// Proxy a `TriggerGC` call to the store's `StoreAdminService.TriggerGC`
/// after populating `extra_roots` from the scheduler's live builds.
///
/// Flow:
/// 1. `ActorCommand::GcRoots` → collect expected_output_paths from all
///    non-terminal derivations. These may not be in narinfo yet (worker
///    hasn't uploaded); the store's mark phase includes them as root
///    seeds so in-flight outputs aren't collected.
/// 2. Connect to store, call TriggerGC with the populated extra_roots +
///    client's dry_run/grace_period.
/// 3. Proxy the store's GCProgress stream back to the client.
///
/// If store is unreachable: UNAVAILABLE (not UNIMPLEMENTED — the RPC IS
/// implemented, store is just down). Client retries.
pub(super) async fn trigger_gc(
    actor: &ActorHandle,
    store_addr: &str,
    shutdown: rio_common::signal::Token,
    mut req: GcRequest,
) -> Result<ReceiverStream<Result<GcProgress, Status>>, Status> {
    // Step 1: collect extra_roots from the actor. send_unchecked
    // bypasses backpressure — GC is operator-initiated, rare,
    // and should work even when the scheduler is saturated.
    let mut extra_roots = actor
        .query_unchecked(|reply| ActorCommand::GcRoots { reply })
        .await
        .map_err(crate::grpc::SchedulerGrpc::actor_error_to_status)?;

    // Merge with any client-provided extra_roots (unusual but
    // allowed — maybe the client has additional pins).
    req.extra_roots.append(&mut extra_roots);
    let extra_count = req.extra_roots.len();

    debug!(
        dry_run = req.dry_run,
        grace_hours = ?req.grace_period_hours,
        extra_roots = extra_count,
        "proxying TriggerGC to store with live-build roots"
    );

    // Step 2: connect to store admin service. Same TLS config
    // as connect_store (OnceLock CLIENT_TLS).
    let mut store_admin = rio_proto::client::connect_store_admin(store_addr)
        .await
        .map_err(|e| Status::unavailable(format!("store admin connect failed: {e}")))?;

    // Step 3: proxy the call. The store's stream becomes OUR
    // stream — we wrap it in a forwarding task. inject_current
    // so the store's link_parent can stitch the trace chain.
    let mut tonic_req = tonic::Request::new(req);
    rio_proto::interceptor::inject_current(tonic_req.metadata_mut());
    let store_stream = store_admin
        .trigger_gc(tonic_req)
        .await
        .map_err(|e| Status::internal(format!("store TriggerGC failed: {e}")))?
        .into_inner();

    // Forward store's progress stream to the client. A small
    // channel + forwarding task: the store stream isn't
    // directly compatible with our TriggerGCStream type (we
    // declare it as ReceiverStream).
    let (tx, rx) = mpsc::channel::<Result<GcProgress, Status>>(8);
    tokio::spawn(async move {
        let mut store_stream = store_stream;
        loop {
            // biased: check shutdown first. A store-side sweep
            // can go minutes between progress messages; without
            // this arm the task holds the store channel alive
            // past main()'s lease_loop.await.
            let msg = tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    tracing::debug!("TriggerGC forward: shutdown, dropping store stream");
                    break;
                }
                m = store_stream.message() => m,
            };
            match msg {
                Ok(Some(progress)) => {
                    if tx.send(Ok(progress)).await.is_err() {
                        // Client disconnected. Let the store-
                        // side GC finish (it's already running);
                        // just stop forwarding.
                        break;
                    }
                }
                Ok(None) => {
                    // Store stream EOF (GC complete). Drop tx
                    // → client sees stream end.
                    break;
                }
                Err(e) => {
                    // Store error mid-stream. Forward the error;
                    // client decides whether to retry.
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }
        }
    });

    Ok(ReceiverStream::new(rx))
}
