//! HTTP health endpoints for K8s probes.
//!
//! Worker has no gRPC server (it's a gRPC CLIENT of the scheduler),
//! so `tonic-health` doesn't fit like it does for scheduler/store.
//! Instead: plain HTTP `/healthz` + `/readyz` via axum, matching the
//! K8s conventions the controller component also uses.
//!
//! | Route | K8s probe | Meaning |
//! |---|---|---|
//! | `/healthz` | liveness | Process is alive. Always 200 after mount. |
//! | `/readyz` | readiness | Scheduler connection is healthy (heartbeat accepted). |
//!
//! The distinction matters: a worker whose heartbeat is failing
//! (scheduler unreachable, network partition) is ALIVE (don't restart
//! it — restarting won't fix the network) but NOT READY (don't count
//! it as capacity in the Service's load balancing, though workers
//! aren't Service-load-balanced anyway — they connect TO the scheduler,
//! not vice versa; readiness here is mainly for `kubectl get pods`
//! visibility and StatefulSet rollout gating).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::{Router, extract::State, http::StatusCode, routing::get};

/// Shared readiness flag. Heartbeat loop writes, /readyz reads.
///
/// Starts `false`: NOT READY until the first heartbeat comes back
/// `accepted`. That's the right gate — a worker that can't reach the
/// scheduler (DNS not resolved yet, scheduler not up) shouldn't pass
/// readiness. The StatefulSet rollout waits for this before moving
/// to the next pod.
///
/// `Relaxed` ordering: this is a pure signal with no
/// synchronization-with semantics. The heartbeat loop and the axum
/// handler don't share any other state. Relaxed is correct and
/// cheapest.
pub type ReadyFlag = Arc<AtomicBool>;

/// Build the router. Caller binds + serves.
///
/// Separate from `spawn_health_server` so tests can use axum's
/// `Router::oneshot` without a real TCP listener.
pub fn router(ready: ReadyFlag) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(ready)
}

/// Spawn the health server on `addr`. Fire-and-forget via
/// `spawn_monitored`: if it dies (port conflict), K8s liveness
/// fails → pod restart → self-healing.
pub fn spawn_health_server(addr: std::net::SocketAddr, ready: ReadyFlag) {
    let router = router(ready);
    rio_common::task::spawn_monitored("health-server", async move {
        tracing::info!(addr = %addr, "starting HTTP health server");
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                if let Err(e) = axum::serve(listener, router).await {
                    tracing::error!(error = %e, "health server failed");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, addr = %addr, "health server bind failed");
            }
        }
    });
}

/// Liveness: always 200. Reaching this handler proves the process
/// is alive and the tokio runtime is responsive. The only way this
/// doesn't return 200 is if the process is dead (K8s sees no
/// response → liveness fail → restart).
///
/// We don't gate on anything: liveness is "is the process OK" not
/// "is the process USEFUL." A worker with a broken FUSE mount is
/// still alive; restarting it might fix the mount. A worker that
/// can't reach the scheduler is still alive; restarting won't fix
/// the network. Liveness = minimum viable signal.
async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Readiness: 200 if heartbeat is healthy, 503 otherwise.
///
/// 503 Service Unavailable, not 500: the worker itself is fine,
/// its DEPENDENCY (scheduler) is unavailable. K8s treats any
/// non-2xx as NOT READY so the code doesn't matter operationally,
/// but 503 is semantically correct and helps humans reading logs.
async fn readyz(State(ready): State<ReadyFlag>) -> StatusCode {
    if ready.load(Ordering::Relaxed) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for Router::oneshot

    #[tokio::test]
    async fn healthz_always_ok() {
        // ready flag doesn't matter for liveness — that's the point.
        let ready = Arc::new(AtomicBool::new(false));
        let app = router(ready);

        let resp = app
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "liveness is unconditional — process responding = alive"
        );
    }

    #[tokio::test]
    async fn readyz_tracks_flag() {
        let ready = Arc::new(AtomicBool::new(false));
        let app = router(ready.clone());

        // Before first heartbeat: NOT READY.
        let resp = app
            .clone()
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "before first heartbeat: not ready; StatefulSet rollout waits"
        );

        // Heartbeat succeeded → READY.
        ready.store(true, Ordering::Relaxed);
        let resp = app
            .clone()
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "heartbeat accepted → ready");

        // Heartbeat started failing (scheduler unreachable) → NOT READY.
        // The worker stays alive (liveness passes) but is removed from
        // the ready set. In practice, workers don't load-balance via
        // Service (they connect out), so this is mainly for kubectl
        // visibility + rollout gating. Still correct to flip.
        ready.store(false, Ordering::Relaxed);
        let resp = app
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "heartbeat failing → not ready (but still live — no restart)"
        );
    }
}
