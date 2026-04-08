//! HTTP health endpoints for K8s probes.
//!
//! Builder has no gRPC server (it's a gRPC CLIENT of the scheduler),
//! so `tonic-health` doesn't fit like it does for scheduler/store.
//! Instead: plain HTTP `/healthz` + `/readyz` via the shared
//! [`rio_common::server::health_router`].
//!
//! The readiness flag flips to `true` on the first accepted heartbeat
//! — a builder that can't reach the scheduler is alive (don't restart;
//! restarting won't fix the network) but not ready (don't count as
//! capacity).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Shared readiness flag. Heartbeat loop writes, `/readyz` reads.
/// Relaxed ordering: pure signal, no other state shared between
/// writer and reader.
pub type ReadyFlag = Arc<AtomicBool>;

/// Spawn the health server on `addr`. Fire-and-forget via
/// `spawn_monitored`: if it dies (port conflict), K8s liveness fails
/// → pod restart → self-healing.
pub fn spawn_health_server(
    addr: std::net::SocketAddr,
    ready: ReadyFlag,
    shutdown: rio_common::signal::Token,
) {
    rio_common::server::spawn_axum(
        "health-server",
        addr,
        rio_common::server::health_router(ready),
        shutdown,
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for Router::oneshot

    use super::*;

    #[tokio::test]
    async fn readyz_tracks_flag() {
        let ready = Arc::new(AtomicBool::new(false));
        let app = rio_common::server::health_router(Arc::clone(&ready));

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

        // Heartbeat started failing → NOT READY (but liveness still OK).
        ready.store(false, Ordering::Relaxed);
        let resp = app
            .clone()
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
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
}
