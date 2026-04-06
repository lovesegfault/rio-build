//! Child object builders (pure: BuilderPoolSet + SizeClassSpec → BuilderPool).
//!
//! The reconciler in `mod.rs` SSA-applies the result. These stay
//! side-effect free so the unit tests below exercise the merge
//! logic without a mock apiserver.

use kube::{Resource, ResourceExt};

use crate::crds::builderpool::{Autoscaling, BuilderPool, BuilderPoolSpec, Replicas, Sizing};
use crate::crds::builderpoolset::{BuilderPoolSet, SizeClassSpec};
use crate::error::{Error, Result};

/// Default replica floor when a SizeClassSpec leaves `min_replicas`
/// unset. 0 = scale-to-zero; the autoscaler (P0234) raises it when
/// queue depth warrants. Matches BuilderPool's semantics where
/// `replicas.min` is an explicit operator decision, not a magic
/// floor.
const DEFAULT_MIN_REPLICAS: i32 = 0;

/// Default replica ceiling when `max_replicas` is unset. 10 is a
/// conservative cap — large enough that a class won't starve under
/// normal load, small enough that a misconfigured WPS on a shared
/// cluster won't burn through it.
const DEFAULT_MAX_REPLICAS: i32 = 10;

/// Default FUSE cache size for child pools. Mirrors
/// `BuilderPoolSpec::default_fuse_cache_size` (same rationale:
/// PoolTemplate deliberately omits this).
const DEFAULT_FUSE_CACHE_SIZE: &str = "50Gi";

/// Child BuilderPool name: `{wps}-{class.name}`. The scheduler
/// routes by `size_class` (which equals `class.name`), not pool
/// name — so the pool name is for operator readability, not
/// dispatch. Keeping the class name as the suffix means `kubectl
/// get wp` shows the structure at a glance.
pub(crate) fn child_name(wps: &BuilderPoolSet, class: &SizeClassSpec) -> String {
    format!("{}-{}", wps.name_any(), class.name)
}

/// Same naming as [`child_name`] but taking bare strings. Used by
/// scaling module which has `class_name: &str` not a full
/// `SizeClassSpec`. Single source of truth for the `{wps}-{class}`
/// convention.
pub(crate) fn child_name_str(wps_name: &str, class_name: &str) -> String {
    format!("{wps_name}-{class_name}")
}

/// Build one child BuilderPool for one size class.
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
/// (BuilderPoolSpec.image is required). Other required-by-CEL
/// fields (`systems`) pass through verbatim — empty `systems`
/// surfaces as a 422 from the apiserver on apply, which is the
/// correct layer for that validation (the operator sees it in
/// `kubectl describe wps` conditions).
pub fn build_child_builderpool(wps: &BuilderPoolSet, class: &SizeClassSpec) -> Result<BuilderPool> {
    let template = &wps.spec.pool_template;

    if template.image.is_empty() {
        return Err(Error::InvalidSpec(
            "BuilderPoolSet.spec.poolTemplate.image is required".into(),
        ));
    }

    // Replicas: per-class bounds with conservative defaults. The
    // autoscaler (P0234) patches `StatefulSet.spec.replicas` within
    // these bounds via a distinct SSA field manager, so the child
    // BuilderPool reconciler's "omit replicas after first create"
    // semantics apply (it doesn't fight the autoscaler).
    let replicas = Replicas {
        min: class.min_replicas.unwrap_or(DEFAULT_MIN_REPLICAS),
        max: class.max_replicas.unwrap_or(DEFAULT_MAX_REPLICAS),
    };

    // Autoscaling: `target_value` from the class's
    // `target_queue_per_replica`. The BuilderPool autoscaler reads
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

    // EXHAUSTIVE by design: when BuilderPoolSpec gains a field,
    // E0063 here forces a decision — does PoolTemplate mirror it
    // (expose to WPS users) or hardcode a default (controller
    // concern)? This is the ONE production literal; test literals
    // delegate to crate::fixtures::test_workerpool_spec().
    let spec = BuilderPoolSpec {
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

        // --- Hardcoded (neither in template nor class; see consts) ---
        fuse_cache_size: DEFAULT_FUSE_CACHE_SIZE.into(),

        // --- Unset optional (use BuilderPool defaults) ---
        // Ephemeral mode propagates from PoolTemplate. Shared
        // across classes — pod lifecycle (Job-per-build vs STS)
        // is a deploy-wide decision. The ephemeral reconciler
        // consults per-class queue depth when `size_class` is
        // set, so each child only spawns for work classified
        // into its class.
        ephemeral: template.ephemeral.unwrap_or(false),
        // WPS is inherently size-class-based (ADR-015) — that IS
        // Sizing::Static. A Manifest-mode WPS would be a distinct
        // feature (controller would poll GetCapacityManifest per-WPS).
        // Until then, WPS children are always Static.
        sizing: Sizing::Static,
        // Deadline only applies to ephemeral Jobs. None →
        // controller default (3600s). A future SizeClassSpec
        // field could make this per-class (xlarge wants longer).
        ephemeral_deadline_seconds: None,
        image_pull_policy: None,
        fuse_threads: None,
        // bloom_expected_items (P0375): NOT in PoolTemplate — same
        // as fuse_threads/fuse_cache_size. Per builderpoolset.rs
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
            "child BuilderPool name '{name}' is {} chars, exceeds 63-char RFC-1123 DNS label limit. \
             Shorten the BuilderPoolSet name or the size-class name.",
            name.len()
        )));
    }
    let mut wp = BuilderPool::new(&name, spec);

    // `controller_owner_ref(&())`: the `&()` is DynamicType=() for
    // static CRDs (kube-rs type parameter). Returns None only if
    // metadata.uid or name is missing — impossible for an
    // apiserver-sourced WPS (set on every read). The .expect in
    // tests fakes uid; real usage can't hit None.
    //
    // Copied verbatim from builderpool/mod.rs:180-182 — type
    // inference here is unforgiving (a bare `()` without `&`
    // doesn't infer).
    wp.metadata.owner_references = Some(vec![wps.controller_owner_ref(&()).ok_or_else(|| {
        Error::InvalidSpec("BuilderPoolSet has no uid (not from apiserver?)".into())
    })?]);
    wp.metadata.namespace = wps.metadata.namespace.clone();

    Ok(wp)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::crds::builderpoolset::{BuilderPoolSetSpec, PoolTemplate};
    use k8s_openapi::api::core::v1::ResourceRequirements;
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

    /// Construct a test WPS with the given class names. Each class
    /// gets a dummy (empty) ResourceRequirements — the builder
    /// doesn't validate it (apiserver does on apply), so empty is
    /// fine for pure-struct unit tests.
    ///
    /// `pub(crate)`: the mock-apiserver tests in `mod.rs` reuse
    /// this (same pattern as `builderpool/tests/mod.rs` fixtures).
    pub(crate) fn test_wps_with_classes(names: &[&str]) -> BuilderPoolSet {
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
        let spec = BuilderPoolSetSpec {
            classes,
            pool_template: PoolTemplate {
                image: "rio-builder:test".into(),
                systems: vec!["x86_64-linux".into()],
                features: vec!["kvm".into()],
                node_selector: None,
                tolerations: None,
                seccomp_profile: None,
                privileged: None,
                host_network: None,
                host_users: None,
                ephemeral: None,
                tls_secret_name: None,
            },
            cutoff_learning: None,
        };
        let mut wps = BuilderPoolSet::new("test-wps", spec);
        // controller_owner_ref needs uid + name. Apiserver sets
        // these; tests fake them (same pattern as builderpool/tests.rs
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
            .map(|c| build_child_builderpool(&wps, c).expect("build ok"))
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
            assert_eq!(or.kind, "BuilderPoolSet");
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
            let child = build_child_builderpool(&wps, class).unwrap();
            assert_eq!(child.spec.image, "rio-builder:test");
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
            .map(|c| build_child_builderpool(&wps, c).unwrap())
            .collect();

        // min_replicas in the fixture: 1, 2, 3 (enumerate + 1).
        assert_eq!(children[0].spec.replicas.min, 1);
        assert_eq!(children[1].spec.replicas.min, 2);
        assert_eq!(children[2].spec.replicas.min, 3);
        // max_replicas: 5, 10, 15.
        assert_eq!(children[0].spec.replicas.max, 5);
        assert_eq!(children[2].spec.replicas.max, 15);
    }

    /// `PoolTemplate.ephemeral` propagates to every child. The
    /// default (None) yields STS-mode children (`ephemeral: false`,
    /// preserving pre-I-117 WPS behavior); `Some(true)` yields
    /// ephemeral children that spawn Jobs sized by their class's
    /// `resources`. CEL on the child requires `replicas.min == 0`
    /// for ephemeral — the fixture's `min_replicas: Some(i+1)`
    /// would violate that, so the ephemeral case clears it.
    #[test]
    fn template_ephemeral_propagates() {
        let mut wps = test_wps_with_classes(&["small", "large"]);
        // Default (None) → STS children.
        for class in &wps.spec.classes {
            let child = build_child_builderpool(&wps, class).unwrap();
            assert!(!child.spec.ephemeral, "None → ephemeral=false (STS)");
        }
        // Some(true) → ephemeral children. Clear min_replicas so the
        // child's CEL `ephemeral → replicas.min==0` would pass on
        // apply (not enforced here, but keep the fixture realistic).
        wps.spec.pool_template.ephemeral = Some(true);
        for class in wps.spec.classes.iter_mut() {
            class.min_replicas = None;
        }
        for class in &wps.spec.classes {
            let child = build_child_builderpool(&wps, class).unwrap();
            assert!(child.spec.ephemeral, "Some(true) → ephemeral=true");
            assert_eq!(child.spec.replicas.min, 0);
            // size_class still set — the ephemeral reconciler uses
            // this to filter queue depth per-class.
            assert_eq!(child.spec.size_class, class.name);
        }
    }

    /// I-119 regression: `class.resources` propagates to the child,
    /// AND mutating the BPS class resources yields a child with the
    /// NEW resources. The reconciler SSA-applies the result of this
    /// builder every reconcile (mod.rs `apply()`); SSA with the same
    /// field manager + force replaces the previously-owned subtree.
    /// So "rebuilt child has new resources" + "apply() always SSA-
    /// patches" (covered by `apply_ssa_patches_child_resources` in
    /// mod.rs) together prove update-on-change.
    ///
    /// Live symptom that prompted this: BPS edited `tiny.resources.
    /// requests.ephemeral-storage` 10Gi→2Gi; child `x86-64-tiny`
    /// stayed at 10Gi. The builder was correct; the test pins it.
    #[test]
    fn class_resources_propagate_on_rebuild() {
        let mut wps = test_wps_with_classes(&["tiny"]);
        let requests = |mem: &str| {
            Some(std::collections::BTreeMap::from([(
                "memory".to_string(),
                Quantity(mem.to_string()),
            )]))
        };

        // First build: 1Gi.
        wps.spec.classes[0].resources = ResourceRequirements {
            requests: requests("1Gi"),
            ..Default::default()
        };
        let child = build_child_builderpool(&wps, &wps.spec.classes[0]).unwrap();
        assert_eq!(
            child
                .spec
                .resources
                .as_ref()
                .and_then(|r| r.requests.as_ref())
                .and_then(|r| r.get("memory")),
            Some(&Quantity("1Gi".into())),
            "first build: class.resources → child.spec.resources"
        );

        // Mutate BPS class resources → 2Gi → rebuild.
        wps.spec.classes[0].resources = ResourceRequirements {
            requests: requests("2Gi"),
            ..Default::default()
        };
        let child = build_child_builderpool(&wps, &wps.spec.classes[0]).unwrap();
        assert_eq!(
            child
                .spec
                .resources
                .as_ref()
                .and_then(|r| r.requests.as_ref())
                .and_then(|r| r.get("memory")),
            Some(&Quantity("2Gi".into())),
            "rebuild after spec edit: child.spec.resources reflects NEW value"
        );

        // The SSA wire body (what apply() sends) must serialize the
        // resources too — apiVersion+kind for SSA, spec.resources
        // for the actual update. A `skip_serializing_if` accident on
        // BuilderPoolSpec.resources would make this silently no-op
        // on the apiserver while the struct-level assert above still
        // passed.
        let body = serde_json::to_value(&child).unwrap();
        assert_eq!(
            body.get("apiVersion").and_then(|v| v.as_str()),
            Some("rio.build/v1alpha1"),
            "SSA body needs GVK"
        );
        assert_eq!(
            body.pointer("/spec/resources/requests/memory")
                .and_then(|v| v.as_str()),
            Some("2Gi"),
            "SSA body carries spec.resources (not skipped/nulled)"
        );
    }

    /// Empty template.image is an InvalidSpec error, not a
    /// silently-empty child BuilderPool (which would fail CEL on
    /// apply with a cryptic apiserver 422).
    #[test]
    fn empty_image_errors() {
        let mut wps = test_wps_with_classes(&["small"]);
        wps.spec.pool_template.image = String::new();
        let result = build_child_builderpool(&wps, &wps.spec.classes[0]);
        assert!(
            matches!(result, Err(Error::InvalidSpec(_))),
            "empty image should error InvalidSpec, got: {result:?}"
        );
    }
}
