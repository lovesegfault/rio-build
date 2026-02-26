//! Build executor with FUSE store for rio-build.
//!
//! Receives build assignments from the scheduler, runs builds using
//! nix-daemon within an overlayfs+FUSE environment, and uploads
//! results to the store.
//!
//! # Architecture
//!
//! ```text
//! rio-worker binary
//! +-- gRPC clients
//! |   +-- WorkerService.BuildExecution (bidi stream to scheduler)
//! |   +-- WorkerService.Heartbeat (periodic to scheduler)
//! |   +-- StoreService (fetch inputs, upload outputs)
//! +-- FUSE daemon (fuse/)
//! |   +-- Mount /nix/store via fuser 0.17
//! |   +-- lookup/getattr -> StoreService.QueryPathInfo
//! |   +-- read/readdir -> SSD cache or StoreService.GetPath
//! |   +-- LRU cache on local SSD (cache.rs)
//! +-- Build executor (executor.rs)
//! |   +-- Overlay management (overlay.rs)
//! |   +-- Synthetic DB generation (synth_db.rs)
//! |   +-- Log streaming (log_stream.rs)
//! |   +-- Output upload (upload.rs)
//! +-- Heartbeat loop (10s interval)
//! ```

pub mod executor;
pub mod fuse;
pub mod log_stream;
pub mod overlay;
pub mod synth_db;
pub mod upload;

use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

use rio_proto::types::{HeartbeatRequest, ResourceUsage};

/// Build a heartbeat request, populating `running_builds` from the shared tracker.
///
/// Extracted for testability — the heartbeat loop in main.rs calls this.
pub async fn build_heartbeat_request(
    worker_id: &str,
    system: &str,
    max_builds: u32,
    running: &Arc<RwLock<HashSet<String>>>,
) -> HeartbeatRequest {
    let current: Vec<String> = running.read().await.iter().cloned().collect();
    HeartbeatRequest {
        worker_id: worker_id.to_string(),
        running_builds: current,
        resources: Some(ResourceUsage::default()),
        local_paths: None,
        system: system.to_string(),
        supported_features: Vec::new(),
        max_builds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_heartbeat_reports_running_builds() {
        let running = Arc::new(RwLock::new(HashSet::new()));
        running
            .write()
            .await
            .insert("/nix/store/foo.drv".to_string());
        running
            .write()
            .await
            .insert("/nix/store/bar.drv".to_string());

        let req = build_heartbeat_request("worker-1", "x86_64-linux", 2, &running).await;
        assert_eq!(req.running_builds.len(), 2);
        assert!(
            req.running_builds
                .contains(&"/nix/store/foo.drv".to_string())
        );
        assert!(
            req.running_builds
                .contains(&"/nix/store/bar.drv".to_string())
        );
        assert_eq!(req.worker_id, "worker-1");
        assert_eq!(req.system, "x86_64-linux");
        assert_eq!(req.max_builds, 2);
    }

    #[tokio::test]
    async fn test_heartbeat_empty_running_builds() {
        let running = Arc::new(RwLock::new(HashSet::new()));
        let req = build_heartbeat_request("worker-1", "x86_64-linux", 1, &running).await;
        assert!(req.running_builds.is_empty());
    }
}
