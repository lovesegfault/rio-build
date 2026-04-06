//! `GetCapacityManifest` RPC tests.
//!
//! Mirrors `admin/manifest.rs`. The bucketing math
//! (`Estimator::bucketed_estimate`) is thoroughly covered in
//! `estimator.rs` (7 tests); this file exercises the gRPC wiring.

use super::*;

/// Empty queue → empty response (not error). The controller's
/// manifest-diff reconciler handles empty manifests by using its
/// operator-configured floor for all capacity.
///
/// The populated-DAG omission tests (cold-start filtered, no-pname
/// filtered, Ready-only) are in `actor/tests/misc.rs` at
/// `capacity_manifest_omits_*` — direct-DagActor pattern via
/// `test_inject_ready` / `test_refresh_estimator`.
#[tokio::test]
async fn test_get_capacity_manifest_empty_queue() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let resp = svc
        .get_capacity_manifest(Request::new(GetCapacityManifestRequest::default()))
        .await?
        .into_inner();

    assert!(
        resp.estimates.is_empty(),
        "no DAG → no ready queue → empty manifest"
    );
    Ok(())
}
