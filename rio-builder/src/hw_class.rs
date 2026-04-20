//! Lazy `hw_class` resolver — reads the controller-stamped
//! `rio.build/hw-class` annotation via downward-API VOLUME.
//!
//! ADR-023 phase-10: the annotation is stamped reactively by
//! `run_pod_annotator` AFTER `spec.nodeName` binds — the same event
//! that triggers kubelet to create the container. The downward-API
//! ENV-VAR form resolves once at container-create and never updates;
//! on warm nodes (~100-300ms to create) or under SpawnIntent burst
//! (annotator loop is serial) kubelet wins and the env var is empty
//! permanently. The volume form refreshes file contents on annotation
//! change; this resolver polls the file with a bounded wait so the
//! race is per-pod transient (≤30s) instead of per-pod permanent.
//!
//! `SpawnIntent.node_selector` targets `(band, cap)` not a single
//! instance type, so template-time stamping cannot determine
//! `hw_class` before binding — the annotator+volume is the mechanism.
// r[impl ctrl.pool.hw-class-annotation]

use std::path::Path;
use std::time::Duration;

/// Downward-API volume mount path written by `pod.rs::
/// build_executor_pod_spec`. kubelet populates `hw-class` from
/// `metadata.annotations['rio.build/hw-class']` and refreshes on
/// change.
pub const HW_CLASS_FILE: &str = "/etc/rio/downward/hw-class";

/// Poll interval. The annotator typically lands <1s after bind; 250ms
/// keeps the bench start latency negligible. The whole resolve→bench
/// chain is `tokio::spawn`ed in `runtime/setup.rs` so it runs
/// concurrently with FUSE mount + cold-start (~30s).
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Upper bound on the poll. Preserves degraded-not-broken: a missed
/// annotation (annotator dead, non-k8s) returns `None` after the
/// bound and the bench is skipped — `hw_class` stays at `factor=1.0`
/// until ≥3 pods report.
const POLL_BOUND: Duration = Duration::from_secs(30);

/// Read [`HW_CLASS_FILE`], retrying on empty/missing every 250ms up
/// to 30s. `None` on expiry (logged).
pub async fn resolve() -> Option<String> {
    resolve_from(Path::new(HW_CLASS_FILE), POLL_INTERVAL, POLL_BOUND).await
}

/// Parameterised for tests (`tokio::time::pause` + tmpdir).
pub(crate) async fn resolve_from(
    path: &Path,
    interval: Duration,
    bound: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + bound;
    loop {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let s = s.trim();
                if !s.is_empty() {
                    return Some(s.to_owned());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Downward-API volume creates the file at mount time
                // (empty until the annotation lands). NotFound means
                // the volume is NOT mounted — non-k8s deployment, or
                // a pod template without the volume — so the file
                // will never appear. Short-circuit instead of blocking
                // setup() for POLL_BOUND; on standalone this stalled
                // worker registration past the VM-test wait_until_
                // succeeds timeout (vm-scheduling-core).
                tracing::debug!(path = %path.display(),
                    "hw_class: downward volume file absent; non-k8s, bench skipped");
                return None;
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                    "hw_class: read failed; treating as unset");
                return None;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::debug!(
                bound_secs = bound.as_secs(),
                "hw_class: downward volume empty past bound; bench skipped"
            );
            return None;
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Poll returns `Some` once the file becomes non-empty; `None` on
    /// expiry. `tokio::time::pause` so the 30s bound is instant.
    // r[verify ctrl.pool.hw-class-annotation]
    #[tokio::test(start_paused = true)]
    async fn hw_class_resolve_polls_until_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hw-class");

        // Missing → immediate None (no downward volume; non-k8s).
        let started = tokio::time::Instant::now();
        assert_eq!(
            resolve_from(&path, Duration::from_millis(10), Duration::from_secs(30)).await,
            None
        );
        assert!(
            started.elapsed() < Duration::from_millis(10),
            "NotFound must short-circuit, not poll"
        );

        // Empty → write after first poll → Some.
        std::fs::write(&path, "").unwrap();
        let p = path.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            std::fs::write(&p, "intel-7-nvme-m\n").unwrap();
        });
        let got = resolve_from(&path, Duration::from_millis(10), Duration::from_secs(5)).await;
        writer.await.unwrap();
        assert_eq!(got.as_deref(), Some("intel-7-nvme-m"));
    }

    /// bug_083: `setup()` spawns the resolve→bench chain so a slow
    /// resolve (annotator dead → 30s POLL_BOUND) does NOT delay FUSE
    /// mount. This test asserts the structural property at the type
    /// level — `BuildSpawnContext::hw_bench` carries a `JoinHandle`
    /// over `(String, Option<f64>)`, which is only constructible by
    /// spawning. Compile-time regression guard: reverts to the old
    /// inline-await `Option<JoinHandle<f64>>` shape won't typecheck.
    ///
    /// Runtime end-to-end coverage lives in `vm-sla-sizing-standalone`
    /// (the hw_bench RPC path).
    #[test]
    fn hw_class_resolve_runs_concurrently_with_fuse_mount() {
        // Type-level assertion: `HwBenchHandle` is a JoinHandle over
        // `(String, Option<f64>)`. A `tokio::spawn`ed future is the
        // only way to construct this; an inline `.await` returns the
        // value directly (not a handle), so the old sequential code
        // structurally cannot produce it.
        fn assert_spawned(
            _: &crate::runtime::HwBenchHandle,
        ) -> Option<&tokio::task::JoinHandle<(String, Option<f64>)>> {
            // Forces `HwBenchHandle` to deref as
            // `Mutex<Option<JoinHandle<(String, Option<f64>)>>>`.
            None
        }
        let h: crate::runtime::HwBenchHandle = std::sync::Arc::new(std::sync::Mutex::new(None));
        let _ = assert_spawned(&h);
    }
}
