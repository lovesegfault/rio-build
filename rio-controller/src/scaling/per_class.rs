//! WPS per-class autoscaler: scale each `BuilderPoolSet` child
//! pool using PER-CLASS queue depth from `GetSizeClassStatus`.
//!
//! `scale_wps_class` is an inherent method on [`Autoscaler`]
//! (split-impl pattern, see P0356). Called from `tick()` in
//! [`standalone`] after the standalone loop runs — the two loops
//! share the same `ScaleState` map (keyed by `pool_key`) so
//! stabilization windows are consistent.
//!
//! [`Autoscaler`]: super::standalone::Autoscaler
//! [`standalone`]: super::standalone

use std::time::Instant;

use k8s_openapi::api::apps::v1::StatefulSet;
use kube::ResourceExt;
use kube::api::{Api, Patch, PatchParams};
use tracing::{debug, info, warn};

use rio_proto::types::GetSizeClassStatusResponse;

use crate::crds::builderpool::BuilderPool;
use crate::crds::builderpoolset::BuilderPoolSet;
use crate::reconcilers::common::sts::{ExecutorRole, sts_name};

use super::standalone::Autoscaler;
use super::{
    ChildLookup, Decision, ScaleState, WPS_AUTOSCALER_MANAGER, check_stabilization,
    compute_desired, find_wps_child, pool_key, sts_replicas_patch,
};

impl Autoscaler {
    /// Scale one WPS child pool using PER-CLASS queue depth.
    ///
    /// Looks up the class's `queued` from `GetSizeClassStatus`
    /// (not cluster-wide `ClusterStatus.queued_derivations`).
    /// Falls through to the child BuilderPool's own `autoscaling.
    /// target_value` / `replicas.{min,max}` — those were set by
    /// the WPS reconciler from `SizeClassSpec` (see
    /// `builderpoolset/builders.rs::build_child_builderpool`).
    ///
    /// Same stabilization mechanics as `scale_one` (shared
    /// `ScaleState` by pool key). Patches the child's StatefulSet
    /// `spec.replicas` with field manager `rio-controller-wps-
    /// autoscaler` — distinct from the standalone autoscaler's
    /// `rio-controller-autoscaler` so `kubectl get sts -o yaml |
    /// grep managedFields` shows which scaler owns the replica
    /// count.
    ///
    /// Skips children whose BuilderPool has `deletionTimestamp`
    /// (same finalizer-fight avoidance as the standalone loop).
    ///
    /// `pools`: the already-listed BuilderPools from `tick()`.
    /// We look up the child here rather than re-GETting — saves
    /// one apiserver call per class per tick.
    pub(super) async fn scale_wps_class(
        &mut self,
        wps: &BuilderPoolSet,
        class: &crate::crds::builderpoolset::SizeClassSpec,
        sc_resp: &GetSizeClassStatusResponse,
        pools: &[BuilderPool],
    ) {
        let child_name = crate::reconcilers::builderpoolset::builders::child_name(wps, class);
        let wps_ns = wps.namespace().unwrap_or_default();

        // r[impl ctrl.wps.autoscale]
        // Find the child BuilderPool in the already-listed set.
        // Two-key symmetry with `is_wps_owned`: the standalone-
        // pool loop skips pools WITH a WPS ownerRef; this loop
        // must skip pools WITHOUT. A name-match without ownerRef
        // means the pool was manually created (or created by
        // something else) with a colliding name — scaling it here
        // would fight the standalone loop. See P0374 for the flap
        // scenario this prevents.
        let child = match find_wps_child(wps, &class.name, pools) {
            ChildLookup::Found(c) => c,
            ChildLookup::NotCreated => {
                debug!(child = %child_name, "WPS child not yet created; skipping scale");
                return;
            }
            ChildLookup::NameCollision => {
                warn!(
                    child = %child_name,
                    "pool name matches {{wps}}-{{class}} but has no WPS ownerRef — \
                     not scaling per-class (would flap against standalone loop)"
                );
                return;
            }
        };

        // r[impl ctrl.autoscale.skip-deleting] — same for WPS children.
        if child.metadata.deletion_timestamp.is_some() {
            debug!(child = %child_name, "skipping: child is being deleted");
            return;
        }

        // Per-class queue depth. Missing class in the RPC response
        // = scheduler doesn't have size-class routing configured
        // for this name, or the response was empty (RPC failed,
        // default). Fall back to 0 → `compute_desired` returns
        // `min` → no spurious scale-up on a misconfigured class.
        let queued = sc_resp
            .classes
            .iter()
            .find(|c| c.name == class.name)
            // I-143: intersect with the child's systems — class-wide
            // `queued` over-counts other-arch work this pool can't
            // build. Falls back to scalar on empty map/systems.
            .map(|c| super::class_queued_for_systems(c, &child.spec.systems))
            .unwrap_or(0);

        // Bounds come from the CHILD BuilderPool's spec (which the
        // WPS reconciler set from SizeClassSpec). Reading from
        // the child rather than re-deriving from `class.*` keeps
        // this in sync with what the reconciler actually applied
        // (operator may have manually edited the child).
        //
        // queued is u64; compute_desired takes u32. Saturate —
        // a queue > 4 billion derivations is pathological but
        // in u64 range; don't wrap to 0 (would scale DOWN under
        // extreme load).
        let queued_u32 = queued.min(u32::MAX as u64) as u32;
        let desired = compute_desired(
            queued_u32,
            child.spec.autoscaling.target_value,
            child.spec.replicas.min,
            child.spec.replicas.max,
        );

        let key = pool_key(child);
        let sts_name = sts_name(&child_name, ExecutorRole::Builder);
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &wps_ns);

        // Current from STS (same pattern as scale_one — reconciler
        // status may lag).
        let current = match sts_api.get_opt(&sts_name).await {
            Ok(Some(sts)) => sts.spec.and_then(|s| s.replicas).unwrap_or(0),
            Ok(None) => {
                debug!(child = %child_name, "STS not yet created; skipping scale");
                return;
            }
            Err(e) => {
                warn!(child = %child_name, error = %e, "failed to read child STS");
                return;
            }
        };

        let timing = self.timing;
        let state = self
            .states
            .entry(key.clone())
            .or_insert_with(|| ScaleState::new(current, timing.min_scale_interval));

        let decision = check_stabilization(state, current, desired, timing);

        match decision {
            Decision::Patch(direction) => {
                // SSA body is identical to `sts_replicas_patch` —
                // the FIELD MANAGER is what differs. The patch
                // body must still carry apiVersion+kind (SSA
                // requirement — see lang-gotchas 3a bug).
                let patch = sts_replicas_patch(desired);
                match sts_api
                    .patch(
                        &sts_name,
                        &PatchParams::apply(WPS_AUTOSCALER_MANAGER).force(),
                        &Patch::Apply(&patch),
                    )
                    .await
                {
                    Ok(_) => {
                        state.last_patch = Instant::now();
                        info!(
                            wps = %wps.name_any(),
                            class = %class.name,
                            child = %child_name,
                            from = current,
                            to = desired,
                            direction = direction.as_str(),
                            queued,
                            "scaled (per-class)"
                        );
                        metrics::counter!("rio_controller_scaling_decisions_total",
                            "direction" => direction.as_str())
                        .increment(1);
                    }
                    Err(e) => {
                        warn!(child = %child_name, error = %e, "per-class scale patch failed");
                    }
                }
            }
            Decision::Wait(reason) => {
                debug!(
                    child = %child_name,
                    current,
                    desired,
                    queued,
                    reason = reason.as_str(),
                    "waiting (per-class)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::apps::v1::StatefulSet;
    use kube::api::{Api, Patch, PatchParams};

    use super::super::tests::{test_wp_in_ns, test_wps};
    use super::super::{ChildLookup, WPS_AUTOSCALER_MANAGER, find_wps_child, sts_replicas_patch};

    /// A pool named `{wps}-{class}` but WITHOUT a WPS ownerRef must
    /// NOT be returned by `find_wps_child` — it's a name collision,
    /// not a WPS child. Without the is_wps_owned gate, both the
    /// standalone loop and the per-class loop would scale it → flap.
    ///
    /// This is the flap-prevention half of the asymmetric-keys bug.
    /// Load-bearing: `scale_wps_class` early-returns on
    /// `ChildLookup::NameCollision` (warn! + return), so no replica
    /// patch is issued. With only name-match (pre-P0374), this
    /// pool would be passed through to the STS-patch code → the
    /// per-class scaler fights the standalone scaler on the same
    /// `spec.replicas`.
    // r[verify ctrl.wps.autoscale]
    #[test]
    fn scale_wps_class_skips_name_collision_without_ownerref() {
        let wps = test_wps("prod", "rio", &["small"]);
        // Name matches `{wps}-{class}` shape, but NO owner_references
        // — a manually-created standalone pool that happens to
        // collide. `is_wps_owned` returns false → the standalone
        // loop scales it; the per-class loop must NOT.
        let colliding = test_wp_in_ns("prod-small", "rio");
        assert!(colliding.metadata.owner_references.is_none());

        match find_wps_child(&wps, "small", std::slice::from_ref(&colliding)) {
            ChildLookup::NameCollision => {} // expected — warn!, don't scale
            ChildLookup::Found(_) => panic!(
                "name-match pool without WPS ownerRef was returned as Found — \
                 would flap against standalone loop. is_wps_owned gate missing?"
            ),
            ChildLookup::NotCreated => panic!(
                "pool with matching name was classified NotCreated — \
                 name+ns match should be checked before ownerRef"
            ),
        }
    }

    // r[verify ctrl.wps.autoscale]
    /// The WPS autoscaler SSA-patches STS replicas with field
    /// manager `rio-controller-wps-autoscaler` — distinct from
    /// `rio-controller-autoscaler` (standalone pools) and
    /// `rio-controller` (BuilderPool reconciler). SSA tracks
    /// `managedFields` per manager; the apiserver uses this to
    /// merge ownership. The unit-test-level proof that SSA is
    /// engaged: `fieldManager=...` appears in the PATCH query
    /// string (merge-patch doesn't use that param). The
    /// end-to-end proof (actual `.metadata.managedFields` entry
    /// on the apiserver) is P0239's VM lifecycle test — a mock
    /// apiserver can't track managedFields.
    ///
    /// The patch body is the same `sts_replicas_patch` as the
    /// standalone autoscaler; the GVK assertion in
    /// `sts_replicas_patch_has_gvk` covers body shape. THIS
    /// test proves the DISTINCT field manager + the query-string
    /// that engages SSA.
    #[tokio::test]
    async fn wps_autoscaler_writes_via_ssa_field_manager() {
        use crate::fixtures::{ApiServerVerifier, Scenario};

        let (client, verifier) = ApiServerVerifier::new();

        // Expect a single PATCH to the child STS with the WPS
        // autoscaler's field manager in the query string. kube-rs
        // emits `force=true&fieldManager=...` (order stable since
        // PatchParams serde is struct-field order). The substring
        // match proves both: SSA engaged (fieldManager param —
        // merge-patch doesn't use it) AND force (last-write-wins
        // instead of conflict-abort on manager overlap).
        let guard = verifier.run(vec![Scenario::ok(
            http::Method::PATCH,
            "force=true&fieldManager=rio-controller-wps-autoscaler",
            serde_json::json!({
                "apiVersion": "apps/v1",
                "kind": "StatefulSet",
                "metadata": { "name": "test-wps-small-builder", "namespace": "rio" },
                "spec": { "replicas": 4 },
            })
            .to_string(),
        )]);

        let sts_api: Api<StatefulSet> = Api::namespaced(client, "rio");

        // Reuse the pure patch builder. The field manager is in
        // PatchParams, NOT the body — both halves must be right.
        let patch = sts_replicas_patch(4);
        sts_api
            .patch(
                "test-wps-small-builder",
                &PatchParams::apply(WPS_AUTOSCALER_MANAGER).force(),
                &Patch::Apply(&patch),
            )
            .await
            .expect("patch succeeds");

        // Proves the PATCH had the expected query string. The
        // verifier panics on mismatch (method/path), or the
        // outer 5s timeout fires if the call was never made.
        guard.verified().await;

        // Body-shape proof: apiVersion + kind MANDATORY for SSA.
        // Without these, the apiserver returns 400 and no
        // managedFields entry would ever be written.
        assert_eq!(
            patch.get("apiVersion").and_then(|v| v.as_str()),
            Some("apps/v1"),
            "SSA body without apiVersion → 400, no managedFields entry"
        );
        assert_eq!(
            patch.get("kind").and_then(|v| v.as_str()),
            Some("StatefulSet"),
            "SSA body without kind → 400"
        );
    }
}
