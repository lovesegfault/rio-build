//! Pool CRD: spawns one-shot rio-builder Jobs (builder OR fetcher mode).
//!
//! D3 (legacy-sizer removal): single CRD for both executor kinds.
//! `BuilderPool` and `FetcherPool` were structurally identical after
//! size-classes were deleted — same binary, same scheduler, same Job
//! lifecycle, with `RIO_EXECUTOR_KIND` toggled. The remaining
//! difference (ADR-019 fetcher hardening) is a `match spec.kind` in
//! the reconciler's `executor_params` plus admission-time CEL on
//! the spec fields the hardening would otherwise silently override.
//!
//! `BuilderPoolSet` is gone: it existed solely to fan out one child
//! pool per size class.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Toleration;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Executor kind. Determines the `rio.build/role` label, the
/// `RIO_EXECUTOR_KIND` env, and (for `Fetcher`) the ADR-019 hardening
/// applied by the reconciler. Mirrors `rio_proto::types::ExecutorKind`
/// — defined here because rio-crds stays kube-only (no prost) and the
/// proto enum is i32-backed without a JsonSchema impl.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ExecutorKind {
    Builder,
    Fetcher,
}

/// Spec for a pool. The derive generates a `Pool` struct with
/// `.metadata`, `.spec` (this), `.status`.
///
/// `namespaced` because Pools are per-namespace (multiple tenants can
/// have their own pools). Printer columns: what `kubectl get pools`
/// shows — Kind/Ready/Desired at a glance is the main thing operators
/// want.
// r[impl ctrl.crd.pool]
///
/// `KubeSchema` alongside `CustomResource`: KubeSchema processes
/// `#[x_kube(validation)]` attrs into x-kubernetes-validations.
/// CustomResource alone ignores them — the schema would have no
/// CEL rules and the apiserver would accept invalid specs. The
/// two derives cooperate: CustomResource generates the full CRD
/// wrapper, KubeSchema handles the schema internals (including
/// the JsonSchema impl, so don't derive that separately).
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, KubeSchema)]
#[kube(
    group = "rio.build",
    version = "v1alpha1",
    kind = "Pool",
    namespaced,
    status = "PoolStatus",
    shortname = "pl",
    printcolumn = r#"{"name":"Kind","type":"string","jsonPath":".spec.kind"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Desired","type":"integer","jsonPath":".status.desiredReplicas"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
// `systems` non-empty. Builders AND fetchers execute derivation
// `builder` scripts, so the host system must match — a pool with no
// target systems accepts no work.
#[x_kube(
    validation = Rule::new("size(self.systems) > 0").message(
        "systems must be non-empty — a pool with no target systems accepts no work"
    )
)]
// r[impl ctrl.crd.host-users-network-exclusive]
// CEL: hostNetwork:true → privileged:true. Kubernetes rejects
// hostUsers:false + hostNetwork:true at admission (user-namespace
// UID remap is incompatible with the host netns). The non-privileged
// path sets hostUsers:false (ADR-012), so hostNetwork implies the
// privileged escape hatch. Only Some(true) triggers the check.
//
// Rule::new(...).message(...) not bare string: bare string emits
// a rule with no message → apiserver falls back to "failed rule:
// {cel expr}" which is opaque to operators.
#[x_kube(
    validation = Rule::new(
        "!(has(self.hostNetwork) && self.hostNetwork) || (has(self.privileged) && self.privileged)"
    ).message(
        "hostNetwork:true requires privileged:true — Kubernetes rejects hostUsers:false with hostNetwork:true at admission; the non-privileged path sets hostUsers:false (see ADR-012)"
    )
)]
// ── D3 fetcher CEL ────────────────────────────────────────────────
// Admission-time rejection (NOT silent reconciler override) of spec
// fields that ADR-019 forces for fetchers. The reconciler's
// `executor_params` Fetcher arm hardcodes the safe values regardless
// (belt-and-suspenders for pre-CEL specs the apiserver already
// accepted); these rules surface the misconfig at `kubectl apply`
// time so the operator KNOWS their spec is being ignored.
//
// `self.kind != 'Fetcher' || ...` reads: "Builder → pass; Fetcher →
// the field is unset OR explicitly false". `has()` is false for
// `None` (skip_serializing_if absent from the wire format).
#[x_kube(
    validation = Rule::new(
        "self.kind != 'Fetcher' || (!has(self.privileged) || !self.privileged)"
    ).message(
        "kind=Fetcher forbids privileged:true — fetchers face the open internet; the escape hatch stays closed (ADR-019)"
    )
)]
#[x_kube(
    validation = Rule::new(
        "self.kind != 'Fetcher' || (!has(self.hostNetwork) || !self.hostNetwork)"
    ).message(
        "kind=Fetcher forbids hostNetwork:true — fetchers run on dedicated nodes with pod networking (ADR-019)"
    )
)]
#[x_kube(
    validation = Rule::new(
        "self.kind != 'Fetcher' || (!has(self.hostUsers) || !self.hostUsers)"
    ).message(
        "kind=Fetcher forbids hostUsers:true — userns isolation is mandatory for FOD-only executors (ADR-019)"
    )
)]
#[x_kube(
    validation = Rule::new(
        "self.kind != 'Fetcher' || !has(self.seccompProfile)"
    ).message(
        "kind=Fetcher forbids seccompProfile — the reconciler forces Localhost operator/rio-fetcher.json (ADR-019)"
    )
)]
#[x_kube(
    validation = Rule::new(
        "self.kind != 'Fetcher' || (!has(self.fuseThreads) && !has(self.fusePassthrough))"
    ).message(
        "kind=Fetcher forbids FUSE tuning knobs — fetches are network-bound, not FUSE-bound"
    )
)]
#[x_kube(
    validation = Rule::new(
        "self.kind != 'Fetcher' || !has(self.daemonTimeoutSecs)"
    ).message(
        "kind=Fetcher forbids daemonTimeoutSecs — fetcher activeDeadlineSeconds is the bound; daemon timeout is builder-only"
    )
)]
pub struct PoolSpec {
    /// Builder or Fetcher. Required — there is no sensible default
    /// (the two have opposite network postures).
    pub kind: ExecutorKind,

    /// Container image ref. Required — there's no sensible default
    /// (depends on how operators build/tag).
    pub image: String,

    /// Target systems (e.g., `["x86_64-linux"]`). Builders AND
    /// fetchers execute derivation `builder` scripts, so the host
    /// system must match.
    pub systems: Vec<String>,

    /// Concurrent-Job ceiling. The reconciler spawns one Job per
    /// dispatch-need up to this many active at once. `None` = no
    /// controller-side cap — Karpenter's NodePool `limits.cpu` (and
    /// ultimately the AWS vCPU service quota) becomes the only gate.
    /// Excess Jobs sit Pending until nodes provision; reap-excess-
    /// pending trims them if demand drops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,

    /// Backstop `activeDeadlineSeconds` on Jobs. `None` = per-kind
    /// default (3600 for builders, 300 for fetchers).
    // r[impl ctrl.pool.ephemeral-deadline]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_seconds: Option<u32>,

    /// Node selector for the Job pod spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,

    /// Tolerations for the Job pod spec. Typed `Toleration` (not
    /// `serde_json::Value`); `any_object_array` passthrough because
    /// k8s-openapi types don't impl JsonSchema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::any_object_array")]
    pub tolerations: Option<Vec<Toleration>>,

    /// Explicit `hostUsers` override. `None` defaults to `hostUsers:
    /// false` (userns isolation per ADR-012). Set `true` for k3s/
    /// containerd deployments that don't chown the pod cgroup to the
    /// userns-mapped root UID. CEL-forbidden for `kind=Fetcher`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_users: Option<bool>,

    /// FUSE dispatcher thread count. Maps to `RIO_FUSE_THREADS`.
    /// `None` = worker default (4). Tune up for NAR-heavy build
    /// profiles where FUSE readahead is the bottleneck. Builder-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuse_threads: Option<u32>,

    /// FUSE passthrough mode (kernel handles reads directly, no
    /// userspace copy). Maps to `RIO_FUSE_PASSTHROUGH`. `None` =
    /// worker default (`true`). Set `false` only as a diagnostic
    /// escape hatch — disabling adds ~2x per-build latency.
    /// Requires kernel >= 6.9 + `CAP_SYS_ADMIN`. Builder-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuse_passthrough: Option<bool>,

    /// Timeout (seconds) for the local nix-daemon subprocess when
    /// the client didn't specify `BuildOptions.build_timeout`.
    /// Maps to `RIO_DAEMON_TIMEOUT_SECS`. `None` = worker default
    /// (7200 = 2h). Raise for pools running known-long builds
    /// (LLVM, chromium, full NixOS closure from cold cache).
    /// Builder-only.
    // WONTFIX: kept as `Option<u64>` rather than `Option<Duration>` —
    // schemars derives Duration as a `{secs,nanos}` object, breaking the
    // CRD OpenAPI schema unless paired with both an Option-aware serde
    // adapter and `#[schemars(with = "Option<u64>")]`. The only consumer
    // string-formats this into the env var anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_timeout_secs: Option<u64>,

    /// requiredSystemFeatures this pool advertises (e.g., "kvm",
    /// "big-parallel"). Worker's Nix config `system-features`.
    /// Fetchers don't advertise features — FODs route by
    /// is_fixed_output alone.
    #[serde(default)]
    pub features: Vec<String>,

    /// Container imagePullPolicy. None = K8s default (IfNotPresent
    /// for tagged images, Always for `:latest`). Airgap/dev clusters
    /// (k3s with `ctr images import`) MUST set "IfNotPresent" or
    /// "Never" — `:latest` otherwise tries docker.io and fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pull_policy: Option<String>,

    /// Pod terminationGracePeriodSeconds. SIGTERM → executor drain
    /// (DrainExecutor + wait for in-flight builds to complete). After
    /// this many seconds: SIGKILL, builds lost.
    ///
    /// `None` = per-kind default (7200 builders, 600 fetchers — nix
    /// builds can legitimately take 2h; fetches are short). Clusters
    /// with known-shorter builds should set this lower so Pool
    /// deletion doesn't stall on a stuck/never-Ready pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_grace_period_seconds: Option<i64>,

    /// Run the worker container privileged. None/false = the
    /// default granular caps (SYS_ADMIN + SYS_CHROOT), which is
    /// sufficient on most clusters. true = full privileged,
    /// which disables seccomp and grants ALL caps.
    ///
    /// When to set true: clusters whose containerd seccomp profile
    /// blocks mount(2) even with SYS_ADMIN and whose nodes don't
    /// pre-install the Localhost profile. Production on EKS/GKE
    /// with proper runtime config should NOT need this.
    /// CEL-forbidden for `kind=Fetcher`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privileged: Option<bool>,

    /// Seccomp profile kind. `None`/`RuntimeDefault` = the runtime's
    /// default filter. `Localhost` = a profile JSON installed at
    /// `/var/lib/kubelet/seccomp/<localhost_profile>` on every node.
    /// `Unconfined` = no filter (debugging only).
    ///
    /// Ignored when `privileged: true` — privileged disables seccomp
    /// at the runtime level. CEL-forbidden for `kind=Fetcher` (the
    /// reconciler forces `Localhost operator/rio-fetcher.json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp_profile: Option<SeccompProfileKind>,

    /// Use the node's network namespace (`hostNetwork: true`).
    /// None/false = pod has its own netns (the default).
    ///
    /// Constraint: `hostNetwork: true` requires `privileged: true`.
    /// CEL-forbidden for `kind=Fetcher`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_network: Option<bool>,
}

/// Seccomp profile selector — mirrors K8s `SeccompProfile` shape.
///
/// Struct not enum: kube-core's structural schema rewriter REJECTS
/// Rust enums where variants carry different data. The
/// type/localhostProfile coupling is enforced by CEL instead of the
/// Rust type system — the apiserver is the gate, which is where it
/// matters (operators apply YAML, not Rust).
// r[impl ctrl.crd.seccomp-cel]
#[derive(Clone, Debug, Serialize, Deserialize, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(
    validation = "self.type in ['RuntimeDefault', 'Localhost', 'Unconfined']",
    validation = "self.type == 'Localhost' ? has(self.localhostProfile) : !has(self.localhostProfile)"
)]
pub struct SeccompProfileKind {
    /// `RuntimeDefault` / `Localhost` / `Unconfined`. K8s convention
    /// is the field is literally `type` in YAML.
    #[serde(rename = "type")]
    pub type_: String,

    /// Path relative to `/var/lib/kubelet/seccomp/`. REQUIRED when
    /// `type: Localhost`, FORBIDDEN otherwise (CEL enforces both).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub localhost_profile: Option<String>,
}

/// Pool status — reconciler writes, operators read.
///
/// `Default` because kube-rs initializes it to `None` →
/// `Some(default())` on first reconcile. All fields zero-value-is-
/// meaningful (0 replicas is a valid observed state).
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PoolStatus {
    /// Active Job count. What's actually running (may lag desired
    /// during cold start).
    #[serde(default)]
    pub replicas: i32,
    /// Jobs whose pod has passed readinessProbe (heartbeating to
    /// scheduler).
    #[serde(default)]
    pub ready_replicas: i32,
    /// Concurrent-Job target the reconciler is converging on.
    #[serde(default)]
    pub desired_replicas: i32,
    /// Standard K8s Conditions. Currently one type:
    /// `SchedulerUnreachable` (status=True when the reconciler's
    /// `GetSpawnIntents` RPC fails — disambiguates "scheduler idle,
    /// queued=0" from "scheduler down, queued unknown").
    #[serde(default)]
    #[schemars(schema_with = "crate::any_object_array")]
    pub conditions: Vec<Condition>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    /// The CRD serializes without panic. Catches:
    /// - schemars derive blowing up on a weird type
    /// - kube's CustomResourceExt::crd() not handling our #[kube]
    ///   attrs (e.g., printcolumn with bad JSON)
    #[test]
    fn crd_serializes() {
        let crd = Pool::crd();
        let yaml = serde_yml::to_string(&crd).expect("serializes");
        assert!(yaml.contains("group: rio.build"));
        assert!(yaml.contains("kind: Pool"));
        assert!(yaml.contains("shortNames"));
        assert!(yaml.contains("pl"));
    }

    /// CEL validation rules are present in the generated schema.
    /// Test for the literal rule strings — a `#[x_kube(validation)]`
    /// attribute silently dropped (typo in the attr name, wrong
    /// derive) would pass compilation but produce a schema without
    /// the constraint.
    #[test]
    fn cel_rules_in_schema() {
        let crd = Pool::crd();
        let json = serde_json::to_string(&crd).expect("serializes to JSON");
        assert!(
            json.contains("size(self.systems) > 0"),
            "systems non-empty CEL rule missing"
        );
        // r[verify ctrl.crd.host-users-network-exclusive]
        assert!(
            json.contains("!(has(self.hostNetwork) && self.hostNetwork)"),
            "hostNetwork→privileged CEL rule missing from schema"
        );
        assert!(
            json.contains("hostNetwork:true requires privileged:true"),
            "hostNetwork→privileged CEL rule has no message"
        );
        // r[verify ctrl.crd.pool]
        // D3: fetcher hardening rules. Admission-time rejection of
        // spec fields ADR-019 forces — the reconciler's belt-and-
        // suspenders override is `pool/mod.rs::executor_params`.
        for (rule, msg) in [
            (
                "self.kind != 'Fetcher' || (!has(self.privileged) || !self.privileged)",
                "kind=Fetcher forbids privileged:true",
            ),
            (
                "self.kind != 'Fetcher' || (!has(self.hostNetwork) || !self.hostNetwork)",
                "kind=Fetcher forbids hostNetwork:true",
            ),
            (
                "self.kind != 'Fetcher' || (!has(self.hostUsers) || !self.hostUsers)",
                "kind=Fetcher forbids hostUsers:true",
            ),
            (
                "self.kind != 'Fetcher' || !has(self.seccompProfile)",
                "kind=Fetcher forbids seccompProfile",
            ),
            (
                "self.kind != 'Fetcher' || !has(self.daemonTimeoutSecs)",
                "kind=Fetcher forbids daemonTimeoutSecs",
            ),
        ] {
            assert!(json.contains(rule), "fetcher CEL rule missing: {rule}");
            assert!(json.contains(msg), "fetcher CEL rule has no message: {msg}");
        }
        // r[verify ctrl.crd.seccomp-cel]
        // Nested KubeSchema CEL propagates.
        assert!(
            json.contains("self.type in ['RuntimeDefault', 'Localhost', 'Unconfined']"),
            "SeccompProfileKind type-enum CEL rule missing"
        );
    }

    /// camelCase field renames applied. The K8s convention is
    /// camelCase in JSON/YAML; Rust is snake_case. serde rename_all
    /// bridges. If someone removes `#[serde(rename_all)]`, `kubectl
    /// apply` with camelCase YAML silently ignores the field.
    #[test]
    fn camel_case_renames() {
        let crd = Pool::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        assert!(!json.contains("\"fuse_threads\""));
        assert!(json.contains("maxConcurrent"));
        assert!(json.contains("seccompProfile"));
        assert!(json.contains("localhostProfile"));
        assert!(json.contains("fuseThreads"));
        assert!(json.contains("fusePassthrough"));
        assert!(json.contains("daemonTimeoutSecs"));
        assert!(json.contains("deadlineSeconds"));
        assert!(json.contains("nodeSelector"));
        assert!(json.contains("readyReplicas"));
        assert!(json.contains("desiredReplicas"));
        // Required list: `kind` + `image` + `systems` (every other
        // field is Option/defaulted).
        assert!(
            json.contains(r#""required":["image","kind","systems"]"#),
            "spec.required drifted"
        );
    }
}
