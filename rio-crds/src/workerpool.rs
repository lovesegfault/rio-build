//! WorkerPool CRD: one StatefulSet of rio-worker pods.
//!
//! The reconciler creates/updates a StatefulSet owned by this CR
//! (ownerReference → GC on delete). Autoscaler patches
//! `StatefulSet.spec.replicas` based on `ClusterStatus.queued_derivations`.

use k8s_openapi::api::core::v1::{ResourceRequirements, Toleration};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Spec for a worker pool. The derive generates a `WorkerPool`
/// struct with `.metadata`, `.spec` (this), `.status`.
///
/// `namespaced` because WorkerPools are per-namespace (multiple
/// tenants can have their own pools). Cluster-scoped would mean
/// one global set — too rigid.
///
/// Printer columns: what `kubectl get workerpools` shows. Ready/
/// Desired at a glance is the main thing operators want.
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
    kind = "WorkerPool",
    namespaced,
    status = "WorkerPoolStatus",
    shortname = "wp",
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Desired","type":"integer","jsonPath":".status.desiredReplicas"}"#,
    printcolumn = r#"{"name":"Class","type":"string","jsonPath":".spec.sizeClass"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
// CEL: ephemeral=true requires replicas.min==0 AND maxConcurrentBuilds==1.
// replicas.min==0 because "pool size" is purely a concurrent-Job ceiling
// (replicas.max); there IS no standing set. A non-zero min would mean
// "always have N Jobs running" which isn't what ephemeral means — it's
// "spawn a Job when there's work." replicas.max > 0 so the ceiling is
// meaningful.
// maxConcurrentBuilds==1 because ephemeral's isolation guarantee is
// one-pod-per-build. A pod running N builds shares FUSE cache +
// overlayfs upper across those N — violates the "zero cross-build state"
// claim at security.md § Ephemeral Builders.
// r[impl ctrl.pool.ephemeral-single-build]
#[x_kube(
    validation = Rule::new(
        "!self.ephemeral || (self.replicas.min == 0 && self.replicas.max > 0 && self.maxConcurrentBuilds == 1)"
    ).message(
        "ephemeral:true requires replicas.min==0, replicas.max>0, and maxConcurrentBuilds==1 — ephemeral's one-pod-per-build isolation guarantee breaks if a pod runs multiple builds (shared FUSE cache + overlayfs upper); see security.md § Ephemeral Builders"
    )
)]
// r[impl ctrl.crd.host-users-network-exclusive]
// CEL: hostNetwork:true → privileged:true. Kubernetes rejects
// hostUsers:false + hostNetwork:true at admission (user-namespace
// UID remap is incompatible with the host netns). The non-privileged
// path sets hostUsers:false (ADR-012), so hostNetwork implies the
// privileged escape hatch. Rule reads: "NOT (hostNetwork set AND
// true) OR (privileged set AND true)" — equivalently hostNetwork
// → privileged. The CRD field is Option<bool> + skip_serializing_
// if_none, so absence is None (has() false) and the rule passes;
// Some(false) serializes as `hostNetwork: false` (has() true,
// value false → rule passes). Only Some(true) triggers the check.
//
// Rule::new(...).message(...) not bare string: bare string emits
// a rule with no message → apiserver falls back to "failed rule:
// {cel expr}" which is opaque to operators. The message tells them
// WHAT to do (set privileged:true or drop hostNetwork) and WHY
// (points at ADR-012). kube-derive injects `use kube::core::Rule`
// inside the generated json_schema fn, so Rule is in scope here
// without an explicit import.
#[x_kube(
    validation = Rule::new(
        "!(has(self.hostNetwork) && self.hostNetwork) || (has(self.privileged) && self.privileged)"
    ).message(
        "hostNetwork:true requires privileged:true — Kubernetes rejects hostUsers:false with hostNetwork:true at admission; the non-privileged path sets hostUsers:false (see ADR-012)"
    )
)]
pub struct WorkerPoolSpec {
    /// Replica bounds. Autoscaler clamps to [min, max].
    ///
    /// CEL on the struct (not this field) because it's a cross-field
    /// constraint. See `Replicas` below.
    pub replicas: Replicas,

    /// Ephemeral mode: one pod per build assignment. Default false
    /// (StatefulSet, long-lived workers with locality). When true:
    /// controller spawns a K8s Job per dispatch-need, worker exits
    /// after one build, pod terminates, Job reaps. Zero cross-build
    /// contamination (fresh FUSE cache, fresh filesystem). Tradeoffs:
    /// cold-start per build (~10-30s), no locality (W_LOCALITY
    /// meaningless — every worker has an empty cache), pod churn.
    ///
    /// Job spawning is driven by the reconciler polling `ClusterStatus.
    /// queued_derivations` — when queued > 0 and active Jobs <
    /// `replicas.max`, spawn Jobs. `replicas.min` MUST be 0 (CEL
    /// enforced): there's no "standing set," only a concurrent-Job
    /// ceiling. `replicas.max` becomes that ceiling.
    ///
    /// See `r[ctrl.pool.ephemeral]` in `docs/src/components/controller.md`.
    #[serde(default)]
    pub ephemeral: bool,

    /// Autoscaling policy. `target_value` is queued-derivations-per-
    /// worker: scale up when `queued / active_workers > target`.
    pub autoscaling: Autoscaling,

    /// K8s resource requests/limits for the worker container.
    /// `None` = unbounded (cluster default). Operators should set
    /// this — unbounded workers on a shared node is a noisy-
    /// neighbor risk.
    ///
    /// schemars(schema_with): k8s-openapi types don't impl
    /// JsonSchema. `any_object` emits `type: object` +
    /// `x-kubernetes-preserve-unknown-fields: true` — the
    /// apiserver validates against its OWN schema (it knows
    /// ResourceRequirements), we just tell it "object, don't
    /// strip unknowns." `serde_json::Value` emitted `{}` which
    /// the apiserver REJECTS (`type: Required value`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::any_object")]
    pub resources: Option<ResourceRequirements>,

    /// Maximum concurrent builds per worker pod. Maps to
    /// `RIO_MAX_BUILDS` env var. CEL: must be >= 1 (0 builds =
    /// pointless worker).
    #[x_kube(validation = "self >= 1")]
    pub max_concurrent_builds: i32,

    /// FUSE cache size as a K8s Quantity string (e.g., "100Gi").
    /// Maps to an emptyDir sizeLimit + parsed to bytes for
    /// `RIO_FUSE_CACHE_SIZE_GB` env. String because Quantity is
    /// standard K8s (operators know "50Gi" syntax); the reconciler
    /// parses it.
    ///
    /// Why not u64 bytes directly: operators write "100Gi" in
    /// kustomize; making them write 107374182400 is hostile.
    #[serde(default = "default_fuse_cache_size")]
    pub fuse_cache_size: String,

    /// FUSE dispatcher thread count. Maps to `RIO_FUSE_THREADS`.
    /// `None` = worker default (4). Tune up for NAR-heavy build
    /// profiles where FUSE readahead is the bottleneck (visible
    /// as `rio_worker_fuse_read_latency_seconds` tail > 10ms
    /// with low CPU).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuse_threads: Option<u32>,

    /// FUSE passthrough mode (kernel handles reads directly, no
    /// userspace copy). Maps to `RIO_FUSE_PASSTHROUGH`. `None` =
    /// worker default (`true`). Set `false` only as a diagnostic
    /// escape hatch — disabling adds ~2x per-build latency.
    /// Requires kernel >= 6.9 + `CAP_SYS_ADMIN` (the worker
    /// container always has SYS_ADMIN for the FUSE mount itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuse_passthrough: Option<bool>,

    /// Timeout (seconds) for the local nix-daemon subprocess when
    /// the client didn't specify `BuildOptions.build_timeout`.
    /// Maps to `RIO_DAEMON_TIMEOUT_SECS`. `None` = worker default
    /// (7200 = 2h). Raise for pools running known-long builds
    /// (LLVM, chromium, full NixOS closure from cold cache).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_timeout_secs: Option<u64>,

    /// requiredSystemFeatures this pool advertises (e.g., "kvm",
    /// "big-parallel"). Worker's Nix config `system-features`.
    #[serde(default)]
    pub features: Vec<String>,

    /// Target systems (e.g., "x86_64-linux"). CEL: non-empty —
    /// a worker that builds nothing is a config error.
    #[x_kube(validation = "size(self) > 0")]
    pub systems: Vec<String>,

    /// Size class name. Maps to `RIO_SIZE_CLASS` env. Scheduler
    /// routes by this (classify() → matching-class workers).
    /// Empty = unclassified (scheduler with size_classes
    /// configured REJECTS unclassified workers — visible
    /// misconfig failure).
    ///
    /// Convention: pool name = size class name. Not enforced
    /// (you might have `small-x86` and `small-arm` pools both
    /// size_class="small").
    #[serde(default)]
    pub size_class: String,

    /// Container image ref. Required — there's no sensible
    /// default (depends on how operators build/tag).
    pub image: String,

    /// Container imagePullPolicy. None = K8s default (IfNotPresent
    /// for tagged images, Always for `:latest`). Airgap/dev clusters
    /// (k3s/kind with `ctr images import`) MUST set "IfNotPresent"
    /// or "Never" — `:latest` otherwise tries docker.io and fails.
    ///
    /// Controller-managed StatefulSets can't be kustomize-patched
    /// (the CRD is patched, not the generated STS), so this has to
    /// be a CRD field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pull_policy: Option<String>,

    /// Node selector for the StatefulSet pod spec. Common:
    /// `rio.build/worker: "true"` to confine to tainted nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,

    /// Tolerations for the StatefulSet pod spec. Pairs with
    /// node_selector: tolerate the `rio.build/worker:NoSchedule`
    /// taint so workers (and only workers) land on those nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::any_object_array")]
    pub tolerations: Option<Vec<Toleration>>,

    /// Pod terminationGracePeriodSeconds. SIGTERM → worker drain
    /// (DrainWorker + wait for in-flight builds to complete). After
    /// this many seconds: SIGKILL, builds lost. Also bounds the
    /// finalizer's cleanup() wait (grace + 60s slop for kubelet/STS
    /// status observation).
    ///
    /// Default 7200 (2h) — nix builds can legitimately take that
    /// long (LLVM from cold ccache, full NixOS closure). Clusters
    /// with known-shorter builds (e.g., VM test fixtures with ≤90s
    /// sleeps) should set this lower so WorkerPool deletion doesn't
    /// stall on a stuck/never-Ready pod for 2h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_grace_period_seconds: Option<i64>,

    /// Run the worker container privileged. None/false = the
    /// default granular caps (SYS_ADMIN + SYS_CHROOT), which is
    /// sufficient on most clusters. true = full privileged,
    /// which disables seccomp and grants ALL caps.
    ///
    /// When to set true: k3s/kind often have containerd seccomp
    /// profiles that block mount(2) even with SYS_ADMIN.
    /// Production on EKS/GKE with proper runtime config should
    /// NOT need this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privileged: Option<bool>,

    /// Seccomp profile kind. `None`/`RuntimeDefault` = the runtime's
    /// default filter (blocks ~40 syscalls: kexec_load, userfaultfd,
    /// open_by_handle_at etc). `Localhost` = a profile JSON installed
    /// at `/var/lib/kubelet/seccomp/<localhost_profile>` on every node
    /// — rio ships one at `infra/helm/rio-build/files/seccomp-rio-
    /// worker.json` that additionally denies `ptrace`, `bpf`, `setns`,
    /// `process_vm_{read,write}v` (syscalls a sandbox escapee with
    /// CAP_SYS_ADMIN would otherwise have). `Unconfined` = no filter
    /// (debugging only).
    ///
    /// Localhost is PRODUCTION-ONLY: the node-level profile install
    /// (DaemonSet + hostPath or node-prep script) is outside the
    /// controller's scope. VM test fixtures use RuntimeDefault. See
    /// docs/src/security.md `r[worker.seccomp.localhost-profile]`.
    ///
    /// Ignored when `privileged: true` — privileged disables seccomp
    /// at the runtime level regardless of what profile is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp_profile: Option<SeccompProfileKind>,

    /// Use the node's network namespace (`hostNetwork: true`).
    /// None/false = pod has its own netns (the default, CNI-
    /// assigned IP). true = pod shares the node's IP + DNS +
    /// /etc/hosts.
    ///
    /// When to set true: VM tests where scheduler/store run on
    /// a separate VM reachable by node hostname but not cluster
    /// DNS; or bare-metal where pod networking adds unwanted
    /// latency (worker → store is NAR-heavy).
    ///
    /// Caveat: hostNetwork pods can't use containerPort-based
    /// Services. The worker doesn't serve anything inbound
    /// (metrics/health are scraped from the node), so this
    /// doesn't break it.
    ///
    /// Constraint: `hostNetwork: true` requires `privileged: true`.
    /// Kubernetes rejects `hostUsers: false` + `hostNetwork: true`
    /// at admission (user-namespace remap is incompatible with the
    /// host netns). The non-privileged path sets `hostUsers: false`
    /// (ADR-012), so hostNetwork implies the privileged escape
    /// hatch. CRD CEL validation enforces this — see the
    /// `host-users-network-exclusive` marker in controller.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_network: Option<bool>,

    /// mTLS client cert Secret name. When set, the controller
    /// mounts this Secret at `/etc/rio/tls/` and sets the
    /// `RIO_TLS__CERT_PATH`/`KEY_PATH`/`CA_PATH` env vars.
    ///
    /// The Secret must have keys `tls.crt`, `tls.key`, `ca.crt`
    /// (cert-manager's standard output for a Certificate with a
    /// CA issuer). In the prod overlay, this is `rio-worker-tls`
    /// (see cert-manager.yaml).
    ///
    /// Unset = plaintext gRPC (dev mode). The worker's TlsConfig
    /// defaults to empty → load_client_tls returns None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_secret_name: Option<String>,

    /// Spread worker pods across nodes. `None`/`Some(true)` (the
    /// default) sets `topologySpreadConstraints` with `maxSkew: 1`
    /// on `kubernetes.io/hostname` (soft — `whenUnsatisfiable:
    /// ScheduleAnyway`) + soft `podAntiAffinity`. `Some(false)` =
    /// no spread (all pods can land on one node; useful for
    /// single-node dev clusters where spread would just be noise).
    ///
    /// Soft (not hard) because a node drain evicting all but one
    /// would temporarily make hard-spread unsatisfiable → pods
    /// stuck Pending. Soft lets them schedule then re-spread on
    /// the next autoscaler action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topology_spread: Option<bool>,

    /// Forward proxy URL for fixed-output derivation (FOD) fetches.
    /// When set, the worker injects `http_proxy`/`https_proxy` env
    /// vars into the nix-daemon spawn IF the build is an FOD.
    /// Non-FOD builds never get proxy env (they don't need network).
    ///
    /// Typical: `http://rio-fod-proxy:3128` (the Squid deployment
    /// from infra/helm/rio-build/templates/fod-proxy.yaml). It allowlists
    /// known source hosts (nixos.org, github, crates.io etc) and
    /// denies everything else — defense against FODs fetching from
    /// arbitrary attacker-controlled URLs.
    ///
    /// Unset = FODs have direct internet (if NetworkPolicy allows
    /// it, which it doesn't by default in prod overlay). Dev mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fod_proxy_url: Option<String>,
}

/// Seccomp profile selector — mirrors K8s `SeccompProfile` shape.
///
/// Struct not enum: kube-core's structural schema rewriter REJECTS
/// Rust enums where variants carry different data (the generated
/// oneOf has per-variant subschemas whose shared `type` property
/// has non-identical schemas — K8s structural schemas forbid that;
/// see kube-core/src/schema.rs hoist_subschema_properties). k8s-
/// openapi's own `SeccompProfile` is a struct for the same reason.
/// The type/localhostProfile coupling is enforced by CEL instead
/// of the Rust type system — the apiserver is the gate, which is
/// where it matters (operators apply YAML, not Rust).
///
/// CRD YAML matches `pod.spec.securityContext.seccompProfile`
/// EXACTLY — operators can copy-paste between the two.
///
/// `KubeSchema` not `JsonSchema`: nested struct with `#[x_kube]`
/// attrs (same as `Replicas` below).
#[derive(Clone, Debug, Serialize, Deserialize, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(
    validation = "self.type in ['RuntimeDefault', 'Localhost', 'Unconfined']",
    validation = "self.type == 'Localhost' ? has(self.localhostProfile) : !has(self.localhostProfile)"
)]
pub struct SeccompProfileKind {
    /// `RuntimeDefault` — the runtime's default filter (~40 syscalls
    /// blocked). `Localhost` — a profile JSON at `/var/lib/kubelet/
    /// seccomp/<localhostProfile>` on the node; rio ships one at
    /// `infra/helm/rio-build/files/seccomp-rio-worker.json` that
    /// additionally denies ptrace/bpf/setns/process_vm_*.
    /// `Unconfined` — no filter (debugging ONLY; never production).
    ///
    /// `type_` with serde rename: `type` is a Rust keyword. K8s
    /// convention is the field is literally `type` in YAML.
    #[serde(rename = "type")]
    pub type_: String,

    /// Path relative to `/var/lib/kubelet/seccomp/`. REQUIRED when
    /// `type: Localhost`, FORBIDDEN otherwise (CEL enforces both).
    /// rio's profile is `rio-worker.json` — install the file at
    /// `/var/lib/kubelet/seccomp/rio-worker.json` on every node
    /// BEFORE applying the WorkerPool, or the pod fails
    /// CreateContainerError.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub localhost_profile: Option<String>,
}

/// Replica bounds with cross-field CEL.
///
/// `KubeSchema` (NOT `JsonSchema`) — the kube-rs derive that
/// processes `#[x_kube(validation)]` attributes and emits
/// x-kubernetes-validations into the generated schema. It also
/// implements JsonSchema internally (the two conflict if both
/// derived). CustomResource auto-processes kube attrs on the
/// top-level Spec; nested structs need KubeSchema explicitly.
///
/// Why min/max instead of a single replicas field: autoscaler
/// needs BOUNDS, not a fixed number. Operator says "2-20"; the
/// autoscaler picks within that based on queue depth.
#[derive(Deserialize, Serialize, Clone, Debug, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = "self.min <= self.max")]
pub struct Replicas {
    /// Floor. Autoscaler never scales below this, even with empty
    /// queue. Keeps a warm pool for fast dispatch when builds
    /// arrive (cold start = minutes of pod scheduling + FUSE warm).
    pub min: i32,
    /// Ceiling. Cost control — don't burn through the cluster
    /// under a pathological burst.
    pub max: i32,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Autoscaling {
    /// Metric driving scale decisions. Currently only "queueDepth"
    /// (scheduler's ClusterStatus.queued_derivations). String not
    /// enum so future metrics don't need a CRD version bump —
    /// unknown metric is a RECONCILE error (surfaces in
    /// .status.conditions), not a schema rejection.
    #[serde(default = "default_metric")]
    pub metric: String,
    /// Scale up when `queued_derivations / active_workers >
    /// target_value`. "5" means "scale up when there are more
    /// than 5 queued builds per worker." Lower = more aggressive
    /// scaling (more pods, lower queue latency, higher cost).
    #[serde(default = "default_target")]
    pub target_value: i32,
}

/// WorkerPool status — reconciler writes, operators read.
///
/// `Default` because kube-rs initializes it to `None` → `Some(default())`
/// on first reconcile. All fields zero-value-is-meaningful (0 replicas
/// is a valid observed state).
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerPoolStatus {
    /// Observed StatefulSet.status.replicas. What's actually
    /// running (may lag desired during rollout).
    #[serde(default)]
    pub replicas: i32,
    /// Observed StatefulSet.status.readyReplicas. Passed
    /// readinessProbe = heartbeat accepted.
    #[serde(default)]
    pub ready_replicas: i32,
    /// What the autoscaler WANTS. Clamped to [min, max]. May
    /// differ from StatefulSet.spec.replicas during the
    /// stabilization window (we want 8 but haven't patched yet).
    #[serde(default)]
    pub desired_replicas: i32,
    /// Last time replicas was actually patched (for stabilization
    /// window computation). kube 3.0: Time = jiff::Timestamp
    /// wrapper, not chrono.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub last_scale_time: Option<Time>,
    /// Standard K8s Conditions. Single `Scaling` type with reason
    /// ScaledUp / ScaledDown / UnknownMetric (status=False for
    /// errors). Helps operators see WHY replicas is what it is
    /// ("ScaledUp" with a message showing from/to; "UnknownMetric"
    /// when spec.autoscaling.metric is unsupported).
    #[serde(default)]
    #[schemars(schema_with = "crate::any_object_array")]
    pub conditions: Vec<Condition>,
}

// ----- serde defaults --------------------------------------------------------
// Functions because serde default needs fn() -> T, not const.

fn default_fuse_cache_size() -> String {
    "50Gi".into()
}

fn default_metric() -> String {
    "queueDepth".into()
}

fn default_target() -> i32 {
    5
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    /// The CRD serializes without panic. Catches:
    /// - schemars derive blowing up on a weird type
    /// - kube's CustomResourceExt::crd() not handling our #[kube]
    ///   attrs (e.g., printcolumn with bad JSON)
    ///
    /// Doesn't validate the OpenAPI schema is CORRECT — K8s
    /// apiserver does that. Just "it produces output."
    #[test]
    fn crd_serializes() {
        let crd = WorkerPool::crd();
        let yaml = serde_yml::to_string(&crd).expect("serializes");
        // Smoke check: the group/kind we configured are in there.
        assert!(yaml.contains("group: rio.build"));
        assert!(yaml.contains("kind: WorkerPool"));
        assert!(yaml.contains("shortNames"));
        assert!(yaml.contains("wp"));
    }

    /// CEL validation rules are present in the generated schema.
    /// kube-rs emits them under `x-kubernetes-validations` in
    /// the relevant schema property. Test for the literal rule
    /// strings — a #[x_kube(validation)] attribute silently
    /// dropped (typo in the attr name, wrong derive) would pass
    /// compilation but produce a schema without the constraint.
    /// The apiserver would then accept `min=10, max=5`.
    #[test]
    fn cel_rules_in_schema() {
        let crd = WorkerPool::crd();
        let json = serde_json::to_string(&crd).expect("serializes to JSON");
        // The three #[x_kube(validation)] rules, verbatim.
        assert!(
            json.contains("self.min <= self.max"),
            "Replicas min<=max CEL rule missing from schema"
        );
        assert!(
            json.contains("self >= 1"),
            "max_concurrent_builds >= 1 CEL rule missing"
        );
        assert!(
            json.contains("size(self) > 0"),
            "systems non-empty CEL rule missing"
        );
        // P0296 ephemeral: cross-field constraint on the spec struct.
        // The rule must be emitted at the WorkerPoolSpec schema level,
        // not on the `ephemeral` field itself (it references
        // self.replicas.{min,max} + self.maxConcurrentBuilds).
        assert!(
            json.contains("!self.ephemeral || (self.replicas.min == 0"),
            "ephemeral CEL rule missing from schema"
        );
        // P0354: the same ephemeral rule now ALSO constrains
        // maxConcurrentBuilds==1. A pod running N builds shares FUSE
        // cache + overlayfs upper across those N — breaks the
        // one-pod-per-build isolation claim.
        assert!(
            json.contains("self.maxConcurrentBuilds == 1"),
            "ephemeral maxConcurrentBuilds CEL clause missing from schema"
        );
        // r[verify ctrl.crd.host-users-network-exclusive]
        // hostNetwork→privileged CEL rule (P0359). Cross-field at the
        // spec struct level (references self.hostNetwork + self.
        // privileged). Also check the message — Rule::new().message()
        // should emit `message:` alongside `rule:` in the
        // x-kubernetes-validations entry. A bare-string validation
        // would only emit `rule:` — the assertion on the message
        // text catches a regression back to the bare form.
        assert!(
            json.contains("!(has(self.hostNetwork) && self.hostNetwork)"),
            "hostNetwork→privileged CEL rule missing from schema"
        );
        assert!(
            json.contains("hostNetwork:true requires privileged:true"),
            "hostNetwork→privileged CEL rule has no message — \
             Rule::new().message() may have been replaced with bare string"
        );
    }

    /// Serde default for `ephemeral`: false. A WorkerPool YAML without
    /// the field must NOT accidentally become ephemeral (Job-per-build)
    /// — that's opt-in behavior. `#[serde(default)]` on a bool gives
    /// `false`; this test pins that.
    // r[verify ctrl.pool.ephemeral]
    #[test]
    fn ephemeral_defaults_false() {
        // Deserialize a minimal spec with ephemeral OMITTED. If serde
        // default changes (or someone swaps to Option<bool>), this
        // catches it before a cluster upgrade silently flips every
        // existing WorkerPool to Job mode.
        let json = serde_json::json!({
            "replicas": {"min": 1, "max": 5},
            "autoscaling": {},
            "maxConcurrentBuilds": 1,
            "systems": ["x86_64-linux"],
            "image": "rio-worker:test"
        });
        let spec: WorkerPoolSpec = serde_json::from_value(json).expect("deserializes");
        assert!(
            !spec.ephemeral,
            "ephemeral must default to false — opt-in security mode, \
             not default behavior"
        );
    }

    /// camelCase field renames applied. The K8s convention is
    /// camelCase in JSON/YAML; Rust is snake_case. serde rename_all
    /// bridges. If someone removes #[serde(rename_all)] from a
    /// nested struct, the schema has `max_concurrent_builds`
    /// instead of `maxConcurrentBuilds` — `kubectl apply` with
    /// camelCase YAML would silently ignore the field (K8s
    /// doesn't error on unknown fields by default).
    #[test]
    fn camel_case_renames() {
        let crd = WorkerPool::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        assert!(
            json.contains("maxConcurrentBuilds"),
            "camelCase rename missing — kubectl with camelCase YAML would \
             silently ignore the field"
        );
        assert!(!json.contains("max_concurrent_builds"));
        assert!(json.contains("fuseCacheSize"));
        assert!(json.contains("targetValue"));
        // P0223 seccomp: field + nested variant field both camelCase.
        assert!(json.contains("seccompProfile"));
        assert!(json.contains("localhostProfile"));
        // Batch E (plan 21): new worker-knob passthrough fields.
        assert!(json.contains("fuseThreads"));
        assert!(json.contains("fusePassthrough"));
        assert!(json.contains("daemonTimeoutSecs"));
    }
}
