//! Tonic server startup boilerplate shared by scheduler/store/gateway main.rs.
//!
//! Extracted from three near-identical copies (scheduler:644, store:462,
//! gateway:329). Each was ~15L of the same `Server::builder().add_service(health)
//! .serve_with_shutdown(...).await` inside a `spawn_monitored`. Any tonic-health
//! upgrade or shutdown-signal change had to be three-way synced.

use std::net::SocketAddr;

use tokio_util::sync::CancellationToken;
use tonic_health::pb::health_server::HealthServer;
use tonic_health::server::HealthService;

use crate::task::spawn_monitored;

/// Spawn a plaintext tonic server with ONLY `grpc.health.v1.Health`, on a
/// dedicated port, sharing the SAME `HealthReporter` state as the caller's
/// main server.
///
/// **Why separate port:** K8s gRPC readiness probes can't do mTLS. When the
/// main port is mTLS, the probe needs a plaintext endpoint. The health_service
/// passed here is a `.clone()` of the one on the main port — cloning
/// `HealthServer<HealthService>` shares the underlying `Arc<RwLock<HashMap>>`
/// status map, so `set_serving()` / `set_not_serving()` on the reporter
/// propagates to BOTH ports. See `r[sched.health.shared-reporter]`.
///
/// **Why `cancelled_owned`:** the spawned task outlives the caller's stack
/// frame, so it needs owned access to the token. Pass the CHILD token (not
/// the parent) — health server should survive the drain window same as the
/// main server (K8s probe gets NOT_SERVING during drain, not ECONNREFUSED).
///
/// **Caller decides whether to call this.** Scheduler/store gate on
/// `server_tls.is_some()` (only need plaintext when main is mTLS). Gateway
/// always calls (its main listener is SSH, not tonic — health is always
/// separate).
pub fn spawn_health_plaintext(
    health_service: HealthServer<HealthService>,
    health_addr: SocketAddr,
    shutdown: CancellationToken,
) {
    tracing::info!(addr = %health_addr, "spawning plaintext health server for K8s probes");
    spawn_monitored("health-plaintext", async move {
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(health_service)
            .serve_with_shutdown(health_addr, shutdown.cancelled_owned())
            .await
        {
            tracing::error!(error = %e, "plaintext health server failed");
        }
    });
}
