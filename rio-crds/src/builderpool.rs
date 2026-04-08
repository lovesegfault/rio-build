//! BuilderPool CRD: spawns one-shot rio-builder Jobs.
//!
//! The reconciler polls `ClusterStatus.queued_derivations` and spawns
//! Jobs (ownerReference → GC on delete) up to `spec.maxConcurrent`.

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::{PoolSpecCommon, PoolStatusCommon, impl_common_deref};

/// Pod sizing mode. ADR-020.
///
/// `Static`: operator sets `spec.resources`, controller spawns Jobs
/// at those resources. ADR-015 behavior. Default.
///
/// `Manifest`: controller polls `GetCapacityManifest` and spawns Jobs
/// with per-derivation resources (P0503). `spec.resources` becomes the
/// cold-start FLOOR (used when the manifest omits a derivation — no
/// `build_history` sample yet).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
pub enum Sizing {
    #[default]
    Static,
    Manifest,
}

/// Spec for a builder pool. The derive generates a `BuilderPool`
/// struct with `.metadata`, `.spec` (this), `.status`.
///
/// `namespaced` because BuilderPools are per-namespace (multiple
/// tenants can have their own pools). Cluster-scoped would mean
/// one global set — too rigid.
///
/// Printer columns: what `kubectl get builderpools` shows. Ready/
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
    kind = "BuilderPool",
    namespaced,
    status = "BuilderPoolStatus",
    shortname = "bp",
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Desired","type":"integer","jsonPath":".status.desiredReplicas"}"#,
    printcolumn = r#"{"name":"Class","type":"string","jsonPath":".spec.sizeClass"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
#[x_kube(
    validation = Rule::new("self.maxConcurrent > 0").message(
        "maxConcurrent must be > 0 — it is the concurrent-Job ceiling"
    )
)]
// `systems` non-empty. Lives at struct level (not field-level on
// `PoolSpecCommon.systems`) so the rule binds only to BuilderPool —
// FetcherPool currently has no such constraint and adding one would
// be a CRD schema change.
#[x_kube(
    validation = Rule::new("size(self.systems) > 0").message(
        "systems must be non-empty — a builder pool with no target systems accepts no work"
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
pub struct BuilderPoolSpec {
    /// Fields shared with `FetcherPoolSpec` (image, systems,
    /// max_concurrent, node placement, mTLS, …). Flattened — the
    /// rendered CRD has these as top-level spec properties.
    /// `Deref`/`DerefMut` below let call sites keep writing
    /// `wp.spec.image`.
    #[serde(flatten)]
    pub common: PoolSpecCommon,

    /// Pod sizing mode (ADR-020). `Static` = operator-set
    /// `spec.resources`. `Manifest` = controller polls
    /// `GetCapacityManifest`, spawns Jobs with per-derivation resources.
    /// `#[serde(default)]` + `#[default] Static` means existing YAMLs
    /// without `sizing:` parse unchanged.
    #[serde(default)]
    pub sizing: Sizing,

    // r[impl ctrl.pool.ephemeral-deadline]
    // `deadline_seconds` lives in `PoolSpecCommon`. Builder-side
    // default is 3600 (1h): long enough that a matched dispatch +
    // build completes; short enough that a wrong-pool spawn (queue
    // full of `x86_64-linux` work on an `aarch64-darwin` pool —
    // worker heartbeats, never matches dispatch) doesn't leak for
    // the life of the cluster. This bounds BUILD time too
    // (`backoffLimit: 0` means K8s can't tell "idle" from "busy on
    // 90min build"). Per-pool queue depth (the proper fix) is
    // deferred to phase5's ClusterStatus proto extension. See
    // `r[ctrl.pool.ephemeral-deadline]` in controller.md.
    /// The class's `cutoffSecs` (upper-bound predicted build duration).
    /// Stamped onto BuilderPoolSet children from `SizeClassSpec.cutoff_
    /// secs`; standalone BuilderPools may set it directly. When set and
    /// `deadline_seconds` is unset, Jobs derive `activeDeadlineSeconds
    /// = cutoff * DEADLINE_MULTIPLIER` instead of the flat 3600
    /// default --- per-class hung-build detection (I-200,
    /// `r[ctrl.ephemeral.per-class-deadline]`). f64 to match
    /// `SizeClassSpec.cutoff_secs` (EMA-smoothed cutoffs are
    /// fractional); the controller `ceil`s before casting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_class_cutoff_secs: Option<f64>,

    /// FUSE dispatcher thread count. Maps to `RIO_FUSE_THREADS`.
    /// `None` = worker default (4). Tune up for NAR-heavy build
    /// profiles where FUSE readahead is the bottleneck (visible
    /// as `rio_builder_fuse_read_latency_seconds` tail > 10ms
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

    /// Container imagePullPolicy. None = K8s default (IfNotPresent
    /// for tagged images, Always for `:latest`). Airgap/dev clusters
    /// (k3s with `ctr images import`) MUST set "IfNotPresent" or
    /// "Never" — `:latest` otherwise tries docker.io and fails.
    ///
    /// Controller-managed Jobs can't be kustomize-patched (the CRD
    /// is patched, not the generated Job), so this has to be a CRD
    /// field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pull_policy: Option<String>,

    /// Pod terminationGracePeriodSeconds. SIGTERM → executor drain
    /// (DrainExecutor + wait for in-flight builds to complete). After
    /// this many seconds: SIGKILL, builds lost.
    ///
    /// Default 7200 (2h) — nix builds can legitimately take that
    /// long (LLVM from cold ccache, full NixOS closure). Clusters
    /// with known-shorter builds (e.g., VM test fixtures with ≤90s
    /// sleeps) should set this lower so BuilderPool deletion doesn't
    /// stall on a stuck/never-Ready pod for 2h.
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
    /// docs/src/security.md `r[builder.seccomp.localhost-profile]`.
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
    // `host_users` lives in `PoolSpecCommon`. Builder-side semantics:
    // `None` = derived (`hostUsers: false` when `!privileged &&
    // !hostNetwork`, ADR-012). `Some(true)` = opt OUT of userns even
    // when non-privileged. When to set `true`: containerd with the
    // systemd cgroup driver may not chown the pod's cgroup to the
    // userns-mapped root UID (runc `Cgroup.OwnerUID` path) → worker's
    // `mkdir /sys/fs/cgroup/leaf` fails EACCES → CrashLoopBackOff.
    // Observed on k3s 1.35.2 (vm-security-nonpriv-k3s). Production
    // EKS/GKE with containerd 2.0+ and proper delegation should leave
    // this unset. See `nix/tests/fixtures/k3s-full.nix` worker-Ready
    // wait diagnostic.

    // fod_proxy_url removed per ADR-019: builders are airgapped; FODs
    // route to fetchers (FetcherPool) which have direct egress. The
    // Squid proxy is deleted — the FOD hash check is the integrity
    // boundary.
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
/// attrs.
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
    /// `infra/helm/rio-build/files/seccomp-rio-builder.json` that
    /// additionally denies ptrace/bpf/setns/process_vm_*.
    /// `Unconfined` — no filter (debugging ONLY; never production).
    ///
    /// `type_` with serde rename: `type` is a Rust keyword. K8s
    /// convention is the field is literally `type` in YAML.
    #[serde(rename = "type")]
    pub type_: String,

    /// Path relative to `/var/lib/kubelet/seccomp/`. REQUIRED when
    /// `type: Localhost`, FORBIDDEN otherwise (CEL enforces both).
    /// rio's profiles are written by systemd-tmpfiles on every node
    /// before kubelet starts (`nix/nixos-node/hardening.nix`) and land
    /// at `operator/{name}.json`; the chart's default builderPool
    /// value is `operator/rio-builder.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub localhost_profile: Option<String>,
}

/// BuilderPool status — reconciler writes, operators read.
///
/// `Default` because kube-rs initializes it to `None` → `Some(default())`
/// on first reconcile. All fields zero-value-is-meaningful (0 replicas
/// is a valid observed state).
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuilderPoolStatus {
    /// Active Job count. What's actually running (may lag desired
    /// during cold start).
    #[serde(default)]
    pub replicas: i32,
    /// `ready_replicas` / `desired_replicas` / `conditions`.
    /// Flattened — `kubectl` columns and the SSA status patch see
    /// `.status.readyReplicas`, not `.status.common.readyReplicas`.
    #[serde(flatten)]
    pub common: PoolStatusCommon,
}

impl_common_deref!(BuilderPoolSpec => PoolSpecCommon);
impl_common_deref!(BuilderPoolStatus => PoolStatusCommon);

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
        let crd = BuilderPool::crd();
        let yaml = serde_yml::to_string(&crd).expect("serializes");
        // Smoke check: the group/kind we configured are in there.
        assert!(yaml.contains("group: rio.build"));
        assert!(yaml.contains("kind: BuilderPool"));
        assert!(yaml.contains("shortNames"));
        assert!(yaml.contains("bp"));
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
        let crd = BuilderPool::crd();
        let json = serde_json::to_string(&crd).expect("serializes to JSON");
        assert!(
            json.contains("self.maxConcurrent > 0"),
            "maxConcurrent>0 CEL rule missing from schema"
        );
        assert!(
            json.contains("size(self.systems) > 0"),
            "systems non-empty CEL rule missing"
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
        // The Sizing enum itself is in the schema with both variants.
        // Guards against a JsonSchema derive dropping an enum variant
        // (wrong serde attr, etc).
        assert!(
            json.contains(r#""enum":["Static","Manifest"]"#)
                || json.contains(r#""enum":["Manifest","Static"]"#),
            "Sizing enum variants missing from schema"
        );
    }

    /// camelCase field renames applied. The K8s convention is
    /// camelCase in JSON/YAML; Rust is snake_case. serde rename_all
    /// bridges. If someone removes #[serde(rename_all)] from a
    /// nested struct, the schema has `fuse_threads` instead of
    /// `fuseThreads` — `kubectl apply` with camelCase YAML would
    /// silently ignore the field (K8s doesn't error on unknown
    /// fields by default).
    #[test]
    fn camel_case_renames() {
        let crd = BuilderPool::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        assert!(!json.contains("fuse_threads"));
        assert!(json.contains("maxConcurrent"));
        // P0223 seccomp: field + nested variant field both camelCase.
        assert!(json.contains("seccompProfile"));
        assert!(json.contains("localhostProfile"));
        // Batch E (plan 21): new worker-knob passthrough fields.
        assert!(json.contains("fuseThreads"));
        assert!(json.contains("fusePassthrough"));
        assert!(json.contains("daemonTimeoutSecs"));
        // P0347: ephemeral Job activeDeadlineSeconds knob.
        assert!(json.contains("deadlineSeconds"));
        assert!(json.contains("maxConcurrent"));
    }
}
