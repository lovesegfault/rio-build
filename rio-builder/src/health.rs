//! HTTP health endpoints for K8s probes.
//!
//! Builder has no gRPC server (it's a gRPC CLIENT of the scheduler),
//! so `tonic-health` doesn't fit like it does for scheduler/store.
//! Instead: plain HTTP `/healthz` + `/readyz` via the shared
//! [`rio_common::server::health_router`].
//!
//! The readiness flag flips to `true` once the pull is accepted
//! (readiness = pulled/building) — a builder that can't reach the
//! scheduler is alive (don't restart; restarting won't fix the
//! network) but not ready (don't count as capacity).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Shared readiness flag. The pull loop writes, `/readyz` reads.
/// Relaxed ordering: pure signal, no other state shared between
/// writer and reader.
pub type ReadyFlag = Arc<AtomicBool>;

/// The builder's health router: the shared `/healthz` + `/readyz`
/// pair plus the live_056-b `/servingz` SERVING-state endpoint — 200
/// iff `serving_file` exists (the file `runtime::setup` writes once
/// `connect_upstreams` succeeds: post-connect, pre-first-pull). The
/// Job's readiness probe consumes `/servingz`, so Pod Ready ⟺ past
/// cold start and asking for work; `/readyz` stays the DIFFERENT
/// pulled/building axis (useful capacity). `serving_file` is a
/// parameter so the route law is testable without touching the
/// production path (`rio_common::k8s::BUILDER_SERVING_STATE_FILE`).
fn builder_health_router(ready: ReadyFlag, serving_file: std::path::PathBuf) -> axum::Router {
    rio_common::server::health_router(ready).route(
        "/servingz",
        axum::routing::get(async move || {
            if serving_file.exists() {
                axum::http::StatusCode::OK
            } else {
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            }
        }),
    )
}

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
        builder_health_router(
            ready,
            std::path::PathBuf::from(rio_common::k8s::BUILDER_SERVING_STATE_FILE),
        ),
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
            "before first heartbeat: not ready; Job pod not yet dispatchable"
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

    /// W9-CO, route faces (live_056-b): `/servingz` answers 503
    /// before the serving-state file exists (a wedged cold-start
    /// builder is NOT ready) and 200 once it does (a serving builder
    /// IS) — independent of `/readyz`'s pulled/building axis. Pre-fix
    /// red: the endpoint did not exist (404 — no probe COULD exist;
    /// I-114's readiness half), transcript in the commit body.
    #[tokio::test]
    async fn w9_co_servingz_tracks_the_serving_state_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let serving = dir.path().join("rio-serving");
        let ready = Arc::new(AtomicBool::new(false));
        let app = super::builder_health_router(Arc::clone(&ready), serving.clone());

        // Cold start (file absent): NOT serving — even if /readyz's
        // flag were set, the axes are independent.
        ready.store(true, Ordering::Relaxed);
        let resp = app
            .clone()
            .oneshot(Request::get("/servingz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "wedged cold start: never serving, pod stays NotReady"
        );

        // The builder connects and writes the file: serving.
        std::fs::write(&serving, b"serving\n").unwrap();
        let resp = app
            .clone()
            .oneshot(Request::get("/servingz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "serving builder is Ready");
    }
}
