//! Client-side CEL evaluation of every `x-kubernetes-validations`
//! rule on the rio CRDs.
//!
//! The in-module `cel_rules_in_schema` / `cel_rules_render` tests
//! string-grep the emitted schema — they catch "attribute silently
//! dropped" but NOT "the CEL expression is wrong" (typo'd field,
//! inverted predicate, `has()` on a non-optional). Until kube 4.0
//! the only thing that ran the rules was the apiserver in
//! `vm-*-k3s`, so a broken rule would surface as a stuck VM test
//! (or worse, a rule that always passes, never).
//!
//! `kube::core::cel::validate_cel` walks the full CRD schema and
//! evaluates every `x-kubernetes-validations` block (top-level AND
//! nested) against a serialized instance. Each rule below has one
//! positive and one negative case; the negative case asserts the
//! rule's `message` (or for the two message-less SeccompProfileKind
//! rules, the rule expression) appears in the returned
//! `ValidationErrors`.

use std::collections::BTreeMap;

use kube::core::cel::{ValidationErrors, validate_cel};
use rio_crds::componentscaler::{
    ComponentScaler, ComponentScalerSpec, LoadThresholds, Replicas, Signal, TargetRef,
};
use rio_crds::pool::{ExecutorKind, Pool, PoolSpec, SeccompProfileKind};

/// Minimal valid `PoolSpec` for `kind` — every CEL rule on
/// `PoolSpec` passes. Each negative case mutates exactly one field
/// from this baseline so the asserted error is the ONLY violation.
fn pool_spec(kind: ExecutorKind) -> PoolSpec {
    PoolSpec {
        kind,
        image: "ghcr.io/rio-build/builder:latest".into(),
        systems: vec!["x86_64-linux".into()],
        max_concurrent: None,
        node_selector: None,
        tolerations: None,
        host_users: None,
        fuse_threads: None,
        features: vec![],
        image_pull_policy: None,
        privileged: None,
        seccomp_profile: None,
        host_network: None,
    }
}

/// Minimal valid `ComponentScalerSpec` — every nested CEL rule
/// (Replicas / TargetRef / LoadThresholds / loadEndpoint) passes.
fn cscaler_spec() -> ComponentScalerSpec {
    ComponentScalerSpec {
        target_ref: TargetRef {
            kind: "Deployment".into(),
            name: "rio-store".into(),
        },
        signal: Signal::SchedulerBuilders,
        replicas: Replicas { min: 2, max: 14 },
        seed_ratio: 50.0,
        load_endpoint: "rio-store-headless.rio-store:9002".into(),
        load_thresholds: LoadThresholds::default(),
    }
}

/// Unwrap-err helper that names which rule under test passed when it
/// should have failed.
#[track_caller]
fn expect_err<T>(res: Result<T, ValidationErrors>, which: &str) -> ValidationErrors {
    match res {
        Ok(_) => panic!("CEL rule `{which}` accepted an invalid instance"),
        Err(e) => e,
    }
}

/// Assert exactly one `ValidationError` was returned and its
/// `message` (or, for message-less rules, its `rule`) contains
/// `needle`. "Exactly one" so a baseline drift (the helper above
/// going stale and triggering a second rule) fails loudly here
/// rather than silently passing on the wrong message.
#[track_caller]
fn assert_one(errs: ValidationErrors, needle: &str) {
    let v = errs.into_vec();
    assert_eq!(
        v.len(),
        1,
        "expected exactly one violation matching {needle:?}, got {v:#?}",
    );
    let e = &v[0];
    assert!(
        e.message.contains(needle) || e.rule.contains(needle),
        "violation {e:?} does not mention {needle:?}",
    );
}

// ── PoolSpec ──────────────────────────────────────────────────────
//
// 9 spec-level rules + 2 nested SeccompProfileKind rules. The two
// baselines are the positive cases for the cluster of rules gated on
// that kind; per-rule positive coverage is implicit (every other
// rule passes when the negative case for one rule is tested in
// isolation, asserted by `assert_one`).

/// Baseline Builder + Fetcher specs are accepted. This is the
/// positive case for every Pool CEL rule at once.
// r[verify ctrl.crd.pool+2]
#[test]
fn pool_baselines_valid() {
    validate_cel(&Pool::new("b", pool_spec(ExecutorKind::Builder)))
        .expect("baseline Builder spec passes every CEL rule");
    validate_cel(&Pool::new("f", pool_spec(ExecutorKind::Fetcher)))
        .expect("baseline Fetcher spec passes every CEL rule");
    // Builder with the privileged-path knobs that Fetcher CEL-forbids:
    // exercises the `kind != 'Fetcher' || …` left-disjunct.
    let mut full = pool_spec(ExecutorKind::Builder);
    full.privileged = Some(true);
    full.host_network = Some(true);
    full.fuse_threads = Some(8);
    full.features = vec!["kvm".into()];
    full.seccomp_profile = Some(SeccompProfileKind {
        type_: "RuntimeDefault".into(),
        localhost_profile: None,
    });
    full.node_selector = Some(BTreeMap::from([(
        "karpenter.sh/capacity-type".into(),
        "spot".into(),
    )]));
    validate_cel(&Pool::new("b", full))
        .expect("Builder with privileged-path knobs passes every CEL rule");
    // Fetcher may set nodeSelector[rio.build/fetcher] = "true"
    // explicitly (the rule allows the reconciler-owned value).
    let mut f = pool_spec(ExecutorKind::Fetcher);
    f.node_selector = Some(BTreeMap::from([(
        "rio.build/fetcher".into(),
        "true".into(),
    )]));
    validate_cel(&Pool::new("f", f))
        .expect("Fetcher with nodeSelector[rio.build/fetcher]=true passes");
}

#[test]
fn pool_systems_non_empty() {
    let mut s = pool_spec(ExecutorKind::Builder);
    s.systems = vec![];
    assert_one(
        expect_err(validate_cel(&Pool::new("p", s)), "systems non-empty"),
        "systems must be non-empty",
    );
}

// r[verify ctrl.crd.host-users-network-exclusive]
#[test]
fn pool_host_network_requires_privileged() {
    let mut s = pool_spec(ExecutorKind::Builder);
    s.host_network = Some(true);
    // privileged unset → violation
    assert_one(
        expect_err(validate_cel(&Pool::new("p", s)), "hostNetwork→privileged"),
        "hostNetwork:true requires privileged:true",
    );
    // privileged: Some(false) → still a violation (the rule checks
    // `has(self.privileged) && self.privileged`, not just `has`).
    let mut s = pool_spec(ExecutorKind::Builder);
    s.host_network = Some(true);
    s.privileged = Some(false);
    assert_one(
        expect_err(validate_cel(&Pool::new("p", s)), "hostNetwork→privileged"),
        "hostNetwork:true requires privileged:true",
    );
}

#[test]
fn pool_fetcher_forbids_privileged() {
    let mut s = pool_spec(ExecutorKind::Fetcher);
    s.privileged = Some(true);
    assert_one(
        expect_err(validate_cel(&Pool::new("p", s)), "Fetcher privileged"),
        "kind=Fetcher forbids privileged:true",
    );
    // Explicit false passes (right-disjunct `!self.privileged`).
    let mut s = pool_spec(ExecutorKind::Fetcher);
    s.privileged = Some(false);
    validate_cel(&Pool::new("p", s)).expect("Fetcher privileged:false passes");
}

#[test]
fn pool_fetcher_forbids_host_network() {
    let mut s = pool_spec(ExecutorKind::Fetcher);
    s.host_network = Some(true);
    // Triggers BOTH Fetcher-hostNetwork AND hostNetwork→privileged.
    let errs = expect_err(validate_cel(&Pool::new("p", s)), "Fetcher hostNetwork");
    let msgs: Vec<_> = errs.iter().map(|e| e.message.as_str()).collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("kind=Fetcher forbids hostNetwork:true")),
        "missing Fetcher-hostNetwork violation in {msgs:?}",
    );
    assert!(
        msgs.iter()
            .any(|m| m.contains("hostNetwork:true requires privileged:true")),
        "missing hostNetwork→privileged violation in {msgs:?}",
    );
    assert_eq!(msgs.len(), 2, "unexpected extra violations: {msgs:?}");
}

#[test]
fn pool_fetcher_forbids_seccomp_profile() {
    let mut s = pool_spec(ExecutorKind::Fetcher);
    s.seccomp_profile = Some(SeccompProfileKind {
        type_: "RuntimeDefault".into(),
        localhost_profile: None,
    });
    assert_one(
        expect_err(validate_cel(&Pool::new("p", s)), "Fetcher seccompProfile"),
        "kind=Fetcher forbids seccompProfile",
    );
}

#[test]
fn pool_fetcher_forbids_fuse_threads() {
    let mut s = pool_spec(ExecutorKind::Fetcher);
    s.fuse_threads = Some(8);
    assert_one(
        expect_err(validate_cel(&Pool::new("p", s)), "Fetcher fuseThreads"),
        "kind=Fetcher forbids fuseThreads",
    );
}

// r[verify ctrl.crd.fetcher-no-features+2]
#[test]
fn pool_fetcher_forbids_features() {
    let mut s = pool_spec(ExecutorKind::Fetcher);
    s.features = vec!["kvm".into()];
    assert_one(
        expect_err(validate_cel(&Pool::new("p", s)), "Fetcher features"),
        "kind=Fetcher forbids spec.features",
    );
}

// ── nodeSelector rules ────────────────────────────────────────────
//
// kube-cel 0.7.1+ (kube-rs/kube-cel#8) leaves `additionalProperties`
// map keys literal — matching apiserver semantics — so
// `'rio.build/fetcher' in self.nodeSelector` evaluates correctly
// client-side. These are real negative tests now; the old tripwire
// asserts that pinned the pre-0.7.1 escaping bug are gone.
// r[verify fetcher.node.dedicated+4]
#[test]
fn pool_fetcher_node_selector_reconciler_owned() {
    let mut s = pool_spec(ExecutorKind::Fetcher);
    s.node_selector = Some(BTreeMap::from([(
        "rio.build/fetcher".into(),
        "false".into(),
    )]));
    assert_one(
        expect_err(validate_cel(&Pool::new("p", s)), "Fetcher nodeSelector"),
        "kind=Fetcher: nodeSelector['rio.build/fetcher'] is reconciler-owned",
    );
}

#[test]
fn pool_builder_forbids_fetcher_node_selector() {
    let mut s = pool_spec(ExecutorKind::Builder);
    s.node_selector = Some(BTreeMap::from([(
        "rio.build/fetcher".into(),
        "true".into(),
    )]));
    assert_one(
        expect_err(validate_cel(&Pool::new("p", s)), "Builder nodeSelector"),
        "kind=Builder forbids nodeSelector['rio.build/fetcher']",
    );
}

// ── SeccompProfileKind (nested KubeSchema) ────────────────────────
//
// Reached via a Builder pool with `seccompProfile` set (Fetcher
// CEL-forbids the field). Two bare-string rules (no `.message()`),
// so the apiserver fallback `failed rule: {expr}` is the message —
// match on the rule expression.

// r[verify ctrl.crd.seccomp-cel]
#[test]
fn pool_seccomp_type_enum() {
    let mut s = pool_spec(ExecutorKind::Builder);
    s.seccomp_profile = Some(SeccompProfileKind {
        type_: "Garbage".into(),
        localhost_profile: None,
    });
    assert_one(
        expect_err(validate_cel(&Pool::new("p", s)), "seccomp type enum"),
        "self.type in ['RuntimeDefault', 'Localhost', 'Unconfined']",
    );
}

#[test]
fn pool_seccomp_localhost_iff_profile() {
    // Localhost requires localhostProfile.
    let mut s = pool_spec(ExecutorKind::Builder);
    s.seccomp_profile = Some(SeccompProfileKind {
        type_: "Localhost".into(),
        localhost_profile: None,
    });
    assert_one(
        expect_err(validate_cel(&Pool::new("p", s)), "Localhost→profile"),
        "self.type == 'Localhost' ? has(self.localhostProfile)",
    );
    // Non-Localhost forbids localhostProfile.
    let mut s = pool_spec(ExecutorKind::Builder);
    s.seccomp_profile = Some(SeccompProfileKind {
        type_: "RuntimeDefault".into(),
        localhost_profile: Some("operator/custom.json".into()),
    });
    assert_one(
        expect_err(validate_cel(&Pool::new("p", s)), "non-Localhost!profile"),
        "self.type == 'Localhost' ? has(self.localhostProfile)",
    );
    // Positive: Localhost + profile.
    let mut s = pool_spec(ExecutorKind::Builder);
    s.seccomp_profile = Some(SeccompProfileKind {
        type_: "Localhost".into(),
        localhost_profile: Some("operator/rio-builder.json".into()),
    });
    validate_cel(&Pool::new("p", s)).expect("Localhost + localhostProfile passes");
}

// ── ComponentScalerSpec + nested ──────────────────────────────────
//
// 1 spec-level rule + 3 nested (Replicas, TargetRef, LoadThresholds).

// r[verify ctrl.crd.componentscaler]
#[test]
fn cscaler_baseline_valid() {
    validate_cel(&ComponentScaler::new("s", cscaler_spec()))
        .expect("baseline ComponentScaler spec passes every CEL rule");
}

#[test]
fn cscaler_replicas_min_le_max() {
    let mut s = cscaler_spec();
    s.replicas = Replicas { min: 5, max: 2 };
    assert_one(
        expect_err(
            validate_cel(&ComponentScaler::new("s", s)),
            "replicas min<=max",
        ),
        "replicas must satisfy 0 <= min <= max",
    );
    // min < 0 also violates.
    let mut s = cscaler_spec();
    s.replicas = Replicas { min: -1, max: 2 };
    assert_one(
        expect_err(
            validate_cel(&ComponentScaler::new("s", s)),
            "replicas min>=0",
        ),
        "replicas must satisfy 0 <= min <= max",
    );
}

#[test]
fn cscaler_target_ref_deployment_only() {
    let mut s = cscaler_spec();
    s.target_ref.kind = "StatefulSet".into();
    assert_one(
        expect_err(
            validate_cel(&ComponentScaler::new("s", s)),
            "targetRef.kind",
        ),
        "targetRef.kind must be 'Deployment'",
    );
}

#[test]
fn cscaler_load_thresholds_ordered() {
    for (lo, hi, why) in [
        (0.9, 0.3, "low >= high"),
        (0.0, 0.8, "low > 0.0"),
        (0.3, 1.5, "high <= 1.0"),
    ] {
        let mut s = cscaler_spec();
        s.load_thresholds = LoadThresholds { low: lo, high: hi };
        assert_one(
            expect_err(
                validate_cel(&ComponentScaler::new("s", s)),
                &format!("loadThresholds {why}"),
            ),
            "loadThresholds must satisfy 0.0 < low < high <= 1.0",
        );
    }
}

#[test]
fn cscaler_load_endpoint_host_port() {
    for bad in [
        "rio-store-headless.rio-store",
        ":9002",
        "rio-store-headless.rio-store:abc",
    ] {
        let mut s = cscaler_spec();
        s.load_endpoint = bad.into();
        assert_one(
            expect_err(
                validate_cel(&ComponentScaler::new("s", s)),
                &format!("loadEndpoint {bad:?}"),
            ),
            "loadEndpoint must be host:port",
        );
    }
}

// ── Well-formedness of every rule ─────────────────────────────────
//
// kube-cel reports a CEL parse error as
// ErrorKind::CompilationFailure (the rule never runs — closed-fail).
// The baseline `*_valid` tests above WOULD catch this (a compile
// error on any rule fails every instance), but this test names the
// failure mode explicitly: a typo in a CEL expression should fail
// HERE with the bad rule named, not as a confusing "baseline spec
// failed" message.
#[test]
fn all_rules_compile() {
    use kube::core::cel::ErrorKind;
    for (name, errs) in [
        (
            "Pool",
            validate_cel(&Pool::new("p", pool_spec(ExecutorKind::Builder))),
        ),
        (
            "ComponentScaler",
            validate_cel(&ComponentScaler::new("s", cscaler_spec())),
        ),
    ] {
        if let Err(e) = errs {
            for v in &e {
                assert!(
                    matches!(v.kind, ErrorKind::ValidationFailure),
                    "{name}: CEL rule did not evaluate cleanly ({:?}): {} — {}",
                    v.kind,
                    v.rule,
                    v.message,
                );
            }
            panic!("{name} baseline failed validation: {e:?}");
        }
    }
}
