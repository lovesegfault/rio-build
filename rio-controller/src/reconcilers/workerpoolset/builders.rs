//! Child object builders (pure: WorkerPoolSet + SizeClassSpec → WorkerPool).
//!
//! The reconciler in `mod.rs` SSA-applies the result. These stay
//! side-effect free so the unit tests below exercise the merge
//! logic without a mock apiserver.

use kube::{Resource, ResourceExt};

use crate::crds::workerpool::{Autoscaling, Replicas, WorkerPool, WorkerPoolSpec};
use crate::crds::workerpoolset::{SizeClassSpec, WorkerPoolSet};
use crate::error::{Error, Result};

/// Default replica floor when a SizeClassSpec leaves `min_replicas`
/// unset. 0 = scale-to-zero; the autoscaler (P0234) raises it when
/// queue depth warrants. Matches WorkerPool's semantics where
/// `replicas.min` is an explicit operator decision, not a magic
/// floor.
const DEFAULT_MIN_REPLICAS: i32 = 0;

/// Default replica ceiling when `max_replicas` is unset. 10 is a
/// conservative cap — large enough that a class won't starve under
/// normal load, small enough that a misconfigured WPS on a shared
/// cluster won't burn through it.
const DEFAULT_MAX_REPLICAS: i32 = 10;

/// Default `max_concurrent_builds` for child pools. PoolTemplate
/// deliberately omits this (it "scales with class size" per the
/// CRD doc), but `WorkerPoolSpec` has no default and CEL requires
/// `>= 1`. 4 is the workerpool test fixture default — reasonable
/// for most builds. A future plan can add per-class overrides.
const DEFAULT_MAX_CONCURRENT_BUILDS: i32 = 4;

/// Default FUSE cache size for child pools. Mirrors
/// `WorkerPoolSpec::default_fuse_cache_size` (same rationale:
/// PoolTemplate deliberately omits this).
const DEFAULT_FUSE_CACHE_SIZE: &str = "50Gi";

/// Child WorkerPool name: `{wps}-{class.name}`. The scheduler
/// routes by `size_class` (which equals `class.name`), not pool
/// name — so the pool name is for operator readability, not
/// dispatch. Keeping the class name as the suffix means `kubectl
/// get wp` shows the structure at a glance.
pub(crate) fn child_name(wps: &WorkerPoolSet, class: &SizeClassSpec) -> String {
    format!("{}-{}", wps.name_any(), class.name)
}

/// Same naming as [`child_name`] but taking bare strings. Used by
/// scaling module which has `class_name: &str` not a full
/// `SizeClassSpec`. Single source of truth for the `{wps}-{class}`
/// convention.
pub(crate) fn child_name_str(wps_name: &str, class_name: &str) -> String {
    format!("{wps_name}-{class_name}")
}

/// Build one child WorkerPool for one size class.
///
/// Merges PoolTemplate (shared across classes) with per-class
/// fields (`SizeClassSpec`). Fields present in NEITHER get
/// hardcoded defaults (documented as `DEFAULT_*` consts above).
///
/// `ownerReferences` ties the child to the parent WPS with
/// `controller=true` — K8s GC deletes the child when the WPS is
/// deleted. The reconciler's `cleanup()` also explicitly deletes
/// for deterministic timing (see mod.rs), but ownerRef is the
/// fallback for the "controller crashed during cleanup" case.
///
/// # Errors
///
/// `Error::InvalidSpec` if `PoolTemplate.image` is empty
/// (WorkerPoolSpec.image is required). Other required-by-CEL
/// fields (`systems`) pass through verbatim — empty `systems`
/// surfaces as a 422 from the apiserver on apply, which is the
/// correct layer for that validation (the operator sees it in
/// `kubectl describe wps` conditions).
pub fn build_child_workerpool(wps: &WorkerPoolSet, class: &SizeClassSpec) -> Result<WorkerPool> {
    let template = &wps.spec.pool_template;

    if template.image.is_empty() {
        return Err(Error::InvalidSpec(
            "WorkerPoolSet.spec.poolTemplate.image is required".into(),
        ));
    }

    // Replicas: per-class bounds with conservative defaults. The
    // autoscaler (P0234) patches `StatefulSet.spec.replicas` within
    // these bounds via a distinct SSA field manager, so the child
    // WorkerPool reconciler's "omit replicas after first create"
    // semantics apply (it doesn't fight the autoscaler).
    let replicas = Replicas {
        min: class.min_replicas.unwrap_or(DEFAULT_MIN_REPLICAS),
        max: class.max_replicas.unwrap_or(DEFAULT_MAX_REPLICAS),
    };

    // Autoscaling: `target_value` from the class's
    // `target_queue_per_replica`. The WorkerPool autoscaler reads
    // this, so per-class scaling "just works" once the child exists.
    // Per-class STATUS aggregation uses GetSizeClassStatus plumbing
    // in scaling::per_class::scale_wps_class (orthogonal concern).
    let autoscaling = Autoscaling {
        metric: "queueDepth".into(),
        // `target_queue_per_replica` is Option<u32>; Autoscaling.
        // target_value is i32. Cast is safe (queue-per-replica
        // values are tiny — 1-20 range). Default 5 mirrors
        // `default_target_queue()` in the CRD.
        target_value: class.target_queue_per_replica.unwrap_or(5) as i32,
    };

    // EXHAUSTIVE by design: when WorkerPoolSpec gains a field,
    // E0063 here forces a decision — does PoolTemplate mirror it
    // (expose to WPS users) or hardcode a default (controller
    // concern)? This is the ONE production literal; test literals
    // delegate to crate::fixtures::test_workerpool_spec().
    let spec = WorkerPoolSpec {
        // --- Per-class (SizeClassSpec) ---
        replicas,
        autoscaling,
        resources: Some(class.resources.clone()),
        // `size_class` is what the scheduler matches. Setting it to
        // the class name is the contract — `r[sched.classify.*]`
        // routes by this string.
        size_class: class.name.clone(),

        // --- Shared (PoolTemplate) ---
        image: template.image.clone(),
        systems: template.systems.clone(),
        features: template.features.clone(),
        node_selector: template.node_selector.clone(),
        tolerations: template.tolerations.clone(),
        seccomp_profile: template.seccomp_profile.clone(),
        privileged: template.privileged,
        host_network: template.host_network,
        host_users: template.host_users,
        tls_secret_name: template.tls_secret_name.clone(),
        fod_proxy_url: template.fod_proxy_url.clone(),

        // --- Hardcoded (neither in template nor class; see consts) ---
        max_concurrent_builds: DEFAULT_MAX_CONCURRENT_BUILDS,
        fuse_cache_size: DEFAULT_FUSE_CACHE_SIZE.into(),

        // --- Unset optional (use WorkerPool defaults) ---
        // Ephemeral mode is WPS-incompatible (per-class long-lived
        // pools are the point). If a future "ephemeral size class"
        // use case emerges, add it to SizeClassSpec, not here.
        ephemeral: false,
        // Deadline only applies to ephemeral Jobs; WPS children
        // are never ephemeral (see above), so always None.
        ephemeral_deadline_seconds: None,
        image_pull_policy: None,
        fuse_threads: None,
        // bloom_expected_items (P0375): NOT in PoolTemplate — same
        // as fuse_threads/fuse_cache_size. Per workerpoolset.rs
        // PoolTemplate comment: knobs that scale WITH class size
        // are deliberately omitted. Bloom capacity arguably scales
        // with pool longevity, not class size — but the decision
        // heuristic (if fuse_threads isn't in PoolTemplate, bloom
        // isn't either) keeps the template surface minimal. Child
        // pools use worker compile-time default (50k).
        bloom_expected_items: None,
        fuse_passthrough: None,
        daemon_timeout_secs: None,
        termination_grace_period_seconds: None,
        topology_spread: None,
    };

    let name = child_name(wps, class);
    // RFC-1123 DNS label limit: Kubernetes resource names ≤63 chars.
    // wps.name_any() and class.name are each apiserver-validated, but
    // the concatenation `{wps}-{class}` can overflow. A 40-char WPS +
    // 30-char class = 71 chars → cryptic 422 deep in apply path.
    // Validate here for a clear InvalidSpec condition on the WPS.
    if name.len() > 63 {
        return Err(Error::InvalidSpec(format!(
            "child WorkerPool name '{name}' is {} chars, exceeds 63-char RFC-1123 DNS label limit. \
             Shorten the WorkerPoolSet name or the size-class name.",
            name.len()
        )));
    }
    let mut wp = WorkerPool::new(&name, spec);

    // `controller_owner_ref(&())`: the `&()` is DynamicType=() for
    // static CRDs (kube-rs type parameter). Returns None only if
    // metadata.uid or name is missing — impossible for an
    // apiserver-sourced WPS (set on every read). The .expect in
    // tests fakes uid; real usage can't hit None.
    //
    // Copied verbatim from workerpool/mod.rs:180-182 — type
    // inference here is unforgiving (a bare `()` without `&`
    // doesn't infer).
    wp.metadata.owner_references = Some(vec![wps.controller_owner_ref(&()).ok_or_else(|| {
        Error::InvalidSpec("WorkerPoolSet has no uid (not from apiserver?)".into())
    })?]);
    wp.metadata.namespace = wps.metadata.namespace.clone();

    Ok(wp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crds::workerpoolset::{PoolTemplate, WorkerPoolSetSpec};
    use k8s_openapi::api::core::v1::ResourceRequirements;

    /// Construct a test WPS with the given class names. Each class
    /// gets a dummy (empty) ResourceRequirements — the builder
    /// doesn't validate it (apiserver does on apply), so empty is
    /// fine for pure-struct unit tests.
    fn test_wps_with_classes(names: &[&str]) -> WorkerPoolSet {
        let classes: Vec<SizeClassSpec> = names
            .iter()
            .enumerate()
            .map(|(i, n)| SizeClassSpec {
                name: (*n).to_string(),
                // Cutoffs monotonically increasing (matches the
                // "smallest covering" classify contract, but the
                // builder doesn't actually read cutoff_secs — it's
                // a scheduler-side field).
                cutoff_secs: (i as f64 + 1.0) * 60.0,
                min_replicas: Some(i as i32 + 1),
                max_replicas: Some((i as i32 + 1) * 5),
                target_queue_per_replica: Some(5),
                resources: ResourceRequirements::default(),
            })
            .collect();
        let spec = WorkerPoolSetSpec {
            classes,
            pool_template: PoolTemplate {
                image: "rio-worker:test".into(),
                systems: vec!["x86_64-linux".into()],
                features: vec!["kvm".into()],
                node_selector: None,
                tolerations: None,
                seccomp_profile: None,
                privileged: None,
                host_network: None,
                host_users: None,
                tls_secret_name: None,
                fod_proxy_url: None,
            },
            cutoff_learning: None,
        };
        let mut wps = WorkerPoolSet::new("test-wps", spec);
        // controller_owner_ref needs uid + name. Apiserver sets
        // these; tests fake them (same pattern as workerpool/tests.rs
        // test_wp()).
        wps.metadata.uid = Some("wps-uid-456".into());
        wps.metadata.namespace = Some("rio".into());
        wps
    }

    /// Exit criterion: 3-class WPS yields 3 children with correct
    /// names, ownerRef UID + controller=true, and size_class set.
    #[test]
    fn three_class_wps_yields_three_children_with_owner_ref() {
        let wps = test_wps_with_classes(&["small", "medium", "large"]);
        let children: Vec<_> = wps
            .spec
            .classes
            .iter()
            .map(|c| build_child_workerpool(&wps, c).expect("build ok"))
            .collect();

        assert_eq!(children.len(), 3);
        for (i, child) in children.iter().enumerate() {
            let class = &wps.spec.classes[i];
            // Name = {wps}-{class.name}.
            assert_eq!(
                child.name_any(),
                format!("test-wps-{}", class.name),
                "child name convention: {{wps}}-{{class}}"
            );
            // ownerRef → WPS UID, controller=true. K8s GC deletes
            // the child when the WPS goes away.
            let or = &child
                .metadata
                .owner_references
                .as_ref()
                .expect("ownerRef set")[0];
            assert_eq!(or.uid, *wps.metadata.uid.as_ref().unwrap());
            assert_eq!(or.controller, Some(true), "controller=true for GC");
            assert_eq!(or.kind, "WorkerPoolSet");
            // size_class drives scheduler routing — must match
            // class.name.
            assert_eq!(child.spec.size_class, class.name);
            // Namespace propagates (namespaced CRD).
            assert_eq!(
                child.metadata.namespace.as_deref(),
                Some("rio"),
                "child inherits WPS namespace"
            );
        }
    }

    /// Template fields propagate to every child identically.
    /// The "shared across classes" guarantee — if one child got
    /// a different image, the whole point of PoolTemplate breaks.
    #[test]
    fn template_fields_propagate() {
        let wps = test_wps_with_classes(&["small", "large"]);
        for class in &wps.spec.classes {
            let child = build_child_workerpool(&wps, class).unwrap();
            assert_eq!(child.spec.image, "rio-worker:test");
            assert_eq!(child.spec.systems, vec!["x86_64-linux".to_string()]);
            assert_eq!(child.spec.features, vec!["kvm".to_string()]);
            // Resources come from the CLASS, not template.
            assert!(child.spec.resources.is_some());
        }
    }

    /// Per-class fields (replicas, autoscaling.target_value) differ
    /// across children — proves the merge isn't accidentally
    /// stamping one class's values onto all.
    #[test]
    fn per_class_fields_diverge() {
        let wps = test_wps_with_classes(&["small", "medium", "large"]);
        let children: Vec<_> = wps
            .spec
            .classes
            .iter()
            .map(|c| build_child_workerpool(&wps, c).unwrap())
            .collect();

        // min_replicas in the fixture: 1, 2, 3 (enumerate + 1).
        assert_eq!(children[0].spec.replicas.min, 1);
        assert_eq!(children[1].spec.replicas.min, 2);
        assert_eq!(children[2].spec.replicas.min, 3);
        // max_replicas: 5, 10, 15.
        assert_eq!(children[0].spec.replicas.max, 5);
        assert_eq!(children[2].spec.replicas.max, 15);
    }

    /// Empty template.image is an InvalidSpec error, not a
    /// silently-empty child WorkerPool (which would fail CEL on
    /// apply with a cryptic apiserver 422).
    #[test]
    fn empty_image_errors() {
        let mut wps = test_wps_with_classes(&["small"]);
        wps.spec.pool_template.image = String::new();
        let result = build_child_workerpool(&wps, &wps.spec.classes[0]);
        assert!(
            matches!(result, Err(Error::InvalidSpec(_))),
            "empty image should error InvalidSpec, got: {result:?}"
        );
    }
}
