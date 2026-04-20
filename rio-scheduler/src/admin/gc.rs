//! `AdminService.TriggerGC` implementation + the store-size background
//! refresher that feeds `ClusterStatus.store_size_bytes`.

use std::sync::Arc;

use futures_util::Stream;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;

use rio_common::grpc::StatusExt;
use tracing::debug;

use rio_proto::types::{GcProgress, GcRequest};

use crate::actor::{ActorCommand, ActorHandle, AdminQuery};

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
    rio_common::task::spawn_periodic(
        "store-size-refresh",
        std::time::Duration::from_secs(60),
        shutdown,
        move || {
            let pool = pool.clone();
            let size_clone = Arc::clone(&size_clone);
            async move {
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
        },
    );
    size
}

/// Proxy a `TriggerGC` call to the store's `StoreAdminService.TriggerGC`
/// after populating `extra_roots` from the scheduler's live builds.
///
/// Flow:
/// 1. `ActorCommand::Admin(AdminQuery::GcRoots` → collect expected_output_paths from all
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
    service_signer: Option<std::sync::Arc<rio_auth::hmac::HmacSigner>>,
    shutdown: rio_common::signal::Token,
    mut req: GcRequest,
) -> Result<ReceiverStream<Result<GcProgress, Status>>, Status> {
    // Step 1: collect extra_roots from the actor. Bypasses
    // backpressure — GC is operator-initiated, rare, and should work
    // even when the scheduler is saturated.
    let mut extra_roots = super::query_actor(actor, |reply| {
        ActorCommand::Admin(AdminQuery::GcRoots { reply })
    })
    .await?;

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
    // as connect_single (OnceLock rio_common::grpc::CLIENT_TLS).
    // Wrapped with ServiceTokenInterceptor — `r[store.admin.service-gate]`
    // requires `x-rio-service-token` on `TriggerGC`.
    let ch = rio_proto::client::connect_channel(store_addr)
        .await
        .status_unavailable("store admin connect failed")?;
    let mut store_admin = rio_proto::StoreAdminServiceClient::with_interceptor(
        ch,
        rio_auth::hmac::ServiceTokenInterceptor::new(service_signer, "rio-scheduler"),
    )
    .max_decoding_message_size(rio_common::grpc::max_message_size())
    .max_encoding_message_size(rio_common::grpc::max_message_size());

    // Step 3: proxy the call. The store's stream becomes OUR
    // stream — we wrap it in a forwarding task. inject_current
    // so the store's link_parent can stitch the trace chain.
    let mut tonic_req = tonic::Request::new(req);
    rio_proto::interceptor::inject_current(tonic_req.metadata_mut());
    // Preserve upstream code+message verbatim with a context prefix —
    // `.status_internal(..)` here squashed the store's
    // `InvalidArgument: too many extra_roots: N (max …)` into an
    // opaque INTERNAL.
    let store_stream = store_admin
        .trigger_gc(tonic_req)
        .await
        .map_err(|s| Status::new(s.code(), format!("store TriggerGC: {}", s.message())))?
        .into_inner();

    // Forward store's progress stream to the client. A small
    // channel + forwarding task: the store stream isn't
    // directly compatible with our TriggerGCStream type (we
    // declare it as ReceiverStream).
    let (tx, rx) = mpsc::channel::<Result<GcProgress, Status>>(8);
    tokio::spawn(forward_gc_progress(store_stream, tx, shutdown));

    Ok(ReceiverStream::new(rx))
}

/// Forward `GcProgress` items from `store_stream` to `tx`, exiting
/// promptly on `shutdown`. Extracted from the `tokio::spawn` body so the
/// shutdown-exit regression test
/// ([`tests::gc_tests::trigger_gc_forward_exits_on_shutdown`]) calls
/// PRODUCTION code — previously the test inlined a hand-copy and a
/// refactor that dropped the `shutdown.cancelled()` arm passed it.
///
/// Generic over the stream type: production wraps `tonic::Streaming`;
/// tests pass a `ReceiverStream`.
pub(super) async fn forward_gc_progress<S>(
    mut store_stream: S,
    tx: mpsc::Sender<Result<GcProgress, Status>>,
    shutdown: rio_common::signal::Token,
) where
    S: Stream<Item = Result<GcProgress, Status>> + Unpin,
{
    loop {
        // biased: check shutdown first. A store-side sweep can go
        // minutes between progress messages; without this arm the
        // task holds the store channel alive past main()'s
        // lease_loop.await.
        let msg = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                tracing::debug!("TriggerGC forward: shutdown, dropping store stream");
                break;
            }
            m = store_stream.next() => m,
        };
        match msg {
            Some(Ok(progress)) => {
                if tx.send(Ok(progress)).await.is_err() {
                    // Client disconnected. Let the store-side GC
                    // finish (it's already running); just stop
                    // forwarding.
                    break;
                }
            }
            None => {
                // Store stream EOF (GC complete). Drop tx → client
                // sees stream end.
                break;
            }
            Some(Err(e)) => {
                // Store error mid-stream. Forward the error; client
                // decides whether to retry.
                let _ = tx.send(Err(e)).await;
                break;
            }
        }
    }
}
