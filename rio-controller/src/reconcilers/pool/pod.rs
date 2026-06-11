//! Job pod-spec builder for executor pods (builders + fetchers).
//!
//! The 600-line pod-spec — FUSE volumes, pod-level seccomp, TLS
//! mounts, capability set, probes, coverage propagation — is
//! role-agnostic and reads `&Pool` directly. `spec.kind == Fetcher`
//! gates the ADR-019 hardening overrides (read-only rootfs, forced
//! Localhost seccomp, never-privileged, dedicated node selector). CEL
//! on the CRD rejects fetcher specs that try to set the overridden
//! fields at admission time; the overrides here are belt-and-
//! suspenders for pre-CEL specs the apiserver already accepted.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMapVolumeSource, Container, ContainerPort, DownwardAPIVolumeFile,
    DownwardAPIVolumeSource, EmptyDirVolumeSource, EnvVar, EnvVarSource, HTTPGetAction,
    HostPathVolumeSource, NodeSelectorRequirement, NodeSelectorTerm, ObjectFieldSelector,
    PodSecurityContext, PodSpec, Probe, SeccompProfile, SecurityContext, Toleration, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::ResourceExt;

use rio_crds::pool::{ExecutorKind, Pool, PoolSpec, SeccompProfileKind};

use crate::reconcilers::node_informer::HwClassConfig;

/// Nix `system-features` string that signals "this builder runs
/// qemu-kvm". When present in `spec.features`, the pod gets the
/// pool-static [`KVM_NODE_LABEL`] *toleration* (permissive — allows it
/// onto a §13b metal NodeClaim). `/dev/kvm` itself is injected by
/// containerd `base_runtime_spec` on every node
/// (`nix/base-runtime-spec.nix`); it ENXIOs on open() on non-`.metal`
/// hosts. *Restrictive* placement (binding the pod TO a metal node) is
/// the per-intent `node_affinity` carrying the metal class's labels
/// (`r[ctrl.pool.node-affinity-from-intent]`) — never a pool-static
/// `nodeSelector` (r33 bug_002). `wants_metal` keeps the literal `kvm`
/// as the cold-start floor; the union of `provides_features` over
/// kvm-tainted hw-classes (r31 bug_020) extends it.
pub(super) const KVM_FEATURE: &str = "kvm";

/// Node label/taint key stamped on metal nodes (§13c:
/// `scheduler.sla.hwClasses.metal-*.{labels,taints}`, via
/// `cover::build_nodeclaim`). `wants_metal` keys the
/// `provides_features` union on this *taint* key; `taints_routing_to`
/// keys the toleration set on it.
const KVM_NODE_LABEL: &str = "rio.build/kvm";

/// Builder idle-exit bound, rendered into the pod env as
/// `RIO_IDLE_SECS` (merged_bug_221 leg 2): pod env wins over image
/// env, so the effective value is pinned HERE — the orphan-reap grace
/// can then assert its headroom at compile time instead of carrying a
/// "MUST exceed" comment that nothing enforces. 120 s matches the
/// builder config default, so today's effective value is unchanged.
// r[impl ctrl.job.idle-render-coupled+2]
pub(super) const POOL_IDLE_EXIT_SECS: u64 = 120;

/// Round-10 bug_078 (triage-corrected close, leg (i)): the slack the
/// FORECAST idle bound adds above the intent's own eta. DERIVED from
/// the mint expiry formula (`mint_executor_tokens`: token expiry =
/// `deadline + eta + 300` — "a forecast-spawned pod's token covers
/// its boot horizon"; this is the SAME 300s horizon slack, applied to
/// the idle bound so the pod's patience and its credential agree on
/// what "the boot horizon" means). A metal forecast pod with
/// eta ∈ (120s, 600s] deterministically idle-exited at the flat 120s,
/// was StaleTerminal-reaped while still wanted, and stepped the
/// wedged-builder futility ladder toward sticky give-up — taxing or
/// permanently blocking the real spawn. VIOLABLE (R17): time axis —
/// the idle cost it can add is bounded by `eta + 300s` per forecast
/// pod (eta itself is lead-seed-bounded ≤ max lead_time_seed);
/// cost axis = that idle node-time, priced as the warm-hit trade the
/// §13b forecast exists to buy; population axis = forecast spawns
/// only (`ready == Some(false)`); size N/A. A const, not config —
/// the eta is the per-intent variable; the slack is the formula's.
pub(super) const FORECAST_IDLE_ETA_SLACK_SECS: u64 = 300;

/// The per-intent idle-exit bound (round-10 bug_078): Ready intents
/// keep the flat [`POOL_IDLE_EXIT_SECS`]; FORECAST intents
/// (`ready == Some(false)`) wait at least their own eta + the
/// boot-horizon slack — no forecast spawn carries an idle bound
/// shorter than the horizon it was spawned to cover (the invariant;
/// the mint expiry formula already computes the same horizon for the
/// token). Floored at the flat bound (an overdue-deps forecast with
/// eta≈0 keeps today's patience; the slack still covers the
/// dep-completion jitter that made it overdue).
pub(super) fn idle_exit_secs(intent: &rio_proto::types::SpawnIntent) -> u64 {
    if intent.ready == Some(false) {
        let eta = intent.eta_seconds.max(0.0).ceil() as u64;
        POOL_IDLE_EXIT_SECS.max(eta.saturating_add(FORECAST_IDLE_ETA_SLACK_SECS))
    } else {
        POOL_IDLE_EXIT_SECS
    }
}

/// The coupling the prose used to carry: a healthy idle pod
/// self-terminates (idle bound) well before the controller-side
/// orphan reap may fire. 60 s of slack covers exit/Job-Complete
/// propagation.
const _: () = assert!(
    super::job::ORPHAN_REAP_GRACE.as_secs() >= POOL_IDLE_EXIT_SECS + 60,
    "ORPHAN_REAP_GRACE must exceed the rendered RIO_IDLE_SECS plus propagation slack"
);

/// AD5 (P8): `terminationGracePeriodSeconds` for every executor pod —
/// a cast of the single-source grace constant the builder partitions
/// into its abort-drain + reserved-report slices
/// (`rio_common::transport::GraceBudget`). SIGTERM is an abort
/// (cgroup-kill + one bounded report attempt + log finalization), not
/// a drain. Pull is the only dispatch protocol (the `dispatchMode`
/// knob is retired), so this value is unconditional: there is no spec
/// override and no per-kind drain grace left.
///
/// (bug_228: this doc block and the cast allow live HERE, on the const
/// that casts — POOL_IDLE_EXIT_SECS was inserted between them once and
/// silently absorbed both; third insert-between instance this branch.)
#[allow(clippy::cast_possible_wrap)] // 45 ≪ i64::MAX
pub(super) const PULL_MODE_TGPS_SECS: i64 =
    rio_common::limits::PULL_MODE_TERMINATION_GRACE_SECS as i64;

/// Pod label carrying the executor role. Scheduler routing, network
/// policies, and `kubectl get pods -l rio.build/role=fetcher` all
/// key on this.
pub const ROLE_LABEL: &str = "rio.build/role";

/// Pod label carrying the owning pool name. Finalizer cleanup lists
/// pods by this; ephemeral mode counts Jobs by it.
pub const POOL_LABEL: &str = "rio.build/pool";

/// emptyDir mounts that make `readOnlyRootFilesystem: true` workable
/// for rio-builder. Each entry corresponds to a startup write path in
/// `rio-builder/src/main.rs` (see the audit comment there). Adding a
/// startup write? Add a row here — it feeds both the Volume list and
/// the VolumeMount list.
///
/// Tuple: `(name, mount_path, medium, size_limit)`.
const READ_ONLY_ROOT_MOUNTS: &[(&str, &str, Option<&str>, Option<&str>)] = &[
    // tempfile::tempdir() defaults to /tmp. Small tmpfs — fetchers
    // don't stage large artifacts here (NAR streams via upload.rs,
    // overlays use /var/rio/overlays).
    ("tmp", "/tmp", Some("Memory"), Some("64Mi")),
    // RIO_FUSE_MOUNT_POINT points here; main.rs create_dir_all would
    // hit EROFS without a mount. Actual store contents go via the
    // kernel FUSE layer — this is just the mountpoint directory.
    ("fuse-store", "/var/rio/fuse-store", None, None),
    // nix-daemon writes /nix/var/nix/{profiles,temproots,gcroots,...}
    // AND /nix/var/log/nix/drvs/. Mounted at /nix/var (not
    // /nix/var/nix) to cover both. main.rs chmods nix/ to 0755 and
    // creates nix/db/ at startup.
    ("nix-var", "/nix/var", None, None),
];

/// Default FUSE cache emptyDir sizeLimit for builder pods. Kubelet
/// evicts on overshoot. Pods are one-shot so the cache never outlives
/// one build's input closure.
///
/// Also added verbatim to the container's `ephemeral-storage`
/// request/limit by [`super::jobs`]. Kubelet sums disk-backed
/// emptyDirs against that limit, so a sizeLimit larger than the budget
/// evicts large-closure builds (chromium/LLVM-class) on the pod-level
/// limit before the volume-level one fires.
///
/// SAFE-MINIMUM fallback when [`BUILDER_FUSE_CACHE`] is unset (fits
/// ~21Gi-allocatable k3s nodes). Prod sets 50Gi via controller.toml
/// `[nodeclaim_pool].fuse_cache_bytes`; the CRD field is CEL-rejected
/// for Builder kind.
pub(crate) const BUILDER_FUSE_CACHE_BYTES: u64 = 8 * (1 << 30);

/// Set once at boot from `[nodeclaim_pool].fuse_cache_bytes` so
/// `fuse_cache_bytes()` for Builder pools, `intent_pod_footprint`'s
/// callers in `nodeclaim_pool` (FFD,
/// `cover_deficit`), and `apply_intent_resources` all read the SAME
/// value (§Simulator-shares-accounting). A per-Pool override would
/// make FFD predict a different ephemeral-storage footprint than the
/// pod actually stamps.
pub static BUILDER_FUSE_CACHE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Default FUSE cache emptyDir sizeLimit for fetcher pods, when
/// `[nodeclaim_pool].fetcher_fuse_cache_bytes` is unset.
///
/// A fetcher's FUSE cache holds the FOD's *input* closure — the fetch
/// script's runtime deps (curl/git/cargo/JDK + stdenv), not the
/// artifact it downloads (that lands in the overlay emptyDir, which is
/// sized from `disk_bytes` and grows via the reactive disk floor on
/// eviction). Fetch-script closures are bounded by the heaviest
/// fetcher toolchain in use, not by the download size, so this is a
/// static bound with no escalation path: it must comfortably cover the
/// worst toolchain (JDK/dotnet-class, ~1.5–2 GiB) but should not
/// inherit the builder budget, which is sized for arbitrary build-time
/// closures and dominates the fetcher pod's ephemeral-storage request
/// ~30× over what a FOD can ever touch.
pub(crate) const FETCHER_FUSE_CACHE_BYTES: u64 = 4 * (1 << 30);

/// Set once at boot from `[nodeclaim_pool].fetcher_fuse_cache_bytes`.
/// Same single-sourcing contract as [`BUILDER_FUSE_CACHE`], for
/// Fetcher pools/intents.
pub static FETCHER_FUSE_CACHE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Effective fetcher FUSE-cache budget: the boot-time config value, or
/// [`FETCHER_FUSE_CACHE_BYTES`] when unset. Shared by
/// [`fuse_cache_bytes`] (pool axis: emptyDir sizeLimit + the stamped
/// pod request) and `intent_pod_footprint` (intent axis: FFD fit-check
/// + `cover_deficit`'s NodeClaim sizing) so the two cannot drift.
pub(crate) fn fetcher_fuse_cache_bytes() -> u64 {
    *FETCHER_FUSE_CACHE
        .get()
        .unwrap_or(&FETCHER_FUSE_CACHE_BYTES)
}

/// Per-pool FUSE cache budget. Drives BOTH the `fuse-cache` emptyDir
/// sizeLimit and the `ephemeral-storage` budget addend so they cannot
/// drift. Pools single-source from the per-kind boot-time value
/// ([`BUILDER_FUSE_CACHE`] = `[nodeclaim_pool].fuse_cache_bytes`,
/// [`FETCHER_FUSE_CACHE`] = `[nodeclaim_pool].fetcher_fuse_cache_bytes`)
/// so FFD/cover/stamp agree (§Simulator-shares-accounting).
/// `PoolSpec.fuse_cache_bytes` is CEL-rejected for both kinds; pre-CEL
/// CRs are ignored here with a Warning event
/// (`DEGRADE_CHECKS::*FuseCacheBytesIgnored`).
///
/// r35 merged_bug_024: §13e routed Fetcher Pools through
/// `nodeclaim_pool` — FFD/cover read the config for fetcher cells too,
/// so a per-Pool Fetcher override would diverge the FFD fit-check from
/// the stamped pod request (the same drift mb_035 closed for Builder).
/// The per-KIND split keeps that property: `intent_pod_footprint`
/// selects on `SpawnIntent.kind`, this selects on `pool.spec.kind`, and
/// the scheduler's intent filter guarantees the two agree for every
/// intent a pool spawns.
pub(super) fn fuse_cache_bytes(pool: &Pool) -> u64 {
    match pool.spec.kind {
        ExecutorKind::Builder => *BUILDER_FUSE_CACHE
            .get()
            .unwrap_or(&BUILDER_FUSE_CACHE_BYTES),
        ExecutorKind::Fetcher => fetcher_fuse_cache_bytes(),
    }
}

/// Upstream gRPC addresses injected into executor pod env: a
/// ClusterIP `addr` for single-channel mode plus an optional
/// headless-Service `balance_host` for health-aware p2c. Same shape
/// for scheduler and store; the env-var prefix differs at the
/// injection site below. Shared with each binary's `Config`
/// (config-loader-deserialized) — see [`rio_common::config::UpstreamAddrs`].
pub use rio_common::config::UpstreamAddrs;

// ── per-kind effective values ────────────────────────────────────
//
// The builder/fetcher diff is small: fetchers force ADR-019 hardening
// (read-only rootfs, never privileged, Localhost seccomp, dedicated
// node placement via per-intent affinity AND a pool-static
// `rio.build/fetcher` nodeSelector — the latter is the last-resort
// restrictive constraint for `system="builtin"` FODs whose
// `hw_class_names=[]` carries no per-intent affinity — hostUsers:false
// default, derived `[fetcher]` features) regardless of spec. Each
// helper below returns the effective value for one pod-spec field.
// Builder reads spec verbatim; Fetcher overrides.

#[inline]
fn is_fetcher(pool: &Pool) -> bool {
    pool.spec.kind == ExecutorKind::Fetcher
}

/// §13e: Fetcher Pools advertise `[fetcher]`, NOT empty. FODs route by
/// `effective_features(state) = [fetcher]` at the scheduler chokepoint
/// (`r[sched.sla.fod-feature-derivation+3]`); the I-181 ∅-guard requires
/// the Pool's features to match. Divergence fails CLOSED — a bug that
/// makes `effective_features(spec) ≠ [fetcher]` means the fetcher Pool
/// never spawns (loud), not that FODs route to builder cells (silent).
/// Single chokepoint so the spawn-decision query (`jobs::queued_for_pool`)
/// and the spawned worker's `RIO_FEATURES` cannot diverge. The declared
/// `spec.features` is ignored — the CEL admission rule rejects a
/// non-empty value, and this override is the belt-and-suspenders for
/// pre-CEL specs.
// r[impl ctrl.crd.fetcher-no-features+2]
#[inline]
pub(crate) fn effective_features(spec: &PoolSpec) -> Vec<String> {
    if spec.kind == ExecutorKind::Fetcher {
        vec![rio_common::k8s::FETCHER_FEATURE.to_string()]
    } else {
        spec.features.clone()
    }
}

/// Fetchers face the open internet — never privileged. Builders honor
/// the spec escape hatch.
#[inline]
fn effective_privileged(pool: &Pool) -> bool {
    !is_fetcher(pool) && pool.spec.privileged == Some(true)
}

/// ADR-019 §Sandbox hardening: fetchers force the stricter Localhost
/// profile (`operator/rio-fetcher.json` — extra denies for ptrace/bpf/
/// setns/process_vm_*/keyctl/add_key) written by systemd-tmpfiles on
/// every node before kubelet starts.
// r[impl fetcher.sandbox.strict-seccomp]
fn effective_seccomp(pool: &Pool) -> Option<SeccompProfileKind> {
    if is_fetcher(pool) {
        Some(SeccompProfileKind {
            type_: "Localhost".into(),
            localhost_profile: Some("operator/rio-fetcher.json".into()),
        })
    } else {
        pool.spec.seccomp_profile.clone()
    }
}

/// Default `hostUsers: false` for fetchers (ADR-019 userns isolation),
/// but HONOR the spec override. k3s containerd doesn't chown the pod
/// cgroup under hostUsers:false → rio-builder's `mkdir
/// /sys/fs/cgroup/leaf` EACCES → exit 1 in <200ms (vmtest-full-
/// nonpriv.yaml). The k3s VM tests set `hostUsers: true`; production
/// EKS (containerd 2.0+) gets the default `false`. Forcing Some(false)
/// here (Phase-7 first cut) made fetcher pods unrunnable on every CI
/// fixture.
#[inline]
fn effective_host_users(pool: &Pool) -> Option<bool> {
    if is_fetcher(pool) {
        pool.spec.host_users.or(Some(false))
    } else {
        pool.spec.host_users
    }
}

/// §13e: pool-static fetcher nodeSelector RESTORED (B4, post-B2.3 deletion).
///
/// The §13d "redundancy is the bug" lesson over-applied: it warned against
/// a pool-static restrictive constraint that CAN diverge from the per-intent
/// affinity (the kvm nodeSelector keyed on `pool.spec.features`, while the
/// per-intent affinity keys on `intent.hw_class_names ∩ {metal-*}`; those
/// CAN disagree for a multi-feature Pool). The fetcher nodeSelector keys on
/// `pool.spec.kind == Fetcher` — a Pool-level invariant that the per-intent
/// affinity (`intent.hw_class_names ∩ {fetcher-*}`) is a PROJECTION of, not
/// an INDEPENDENT opinion of. They agree by construction. The pool-static
/// constraint is needed for `system="builtin"` FODs whose `hw_class_names`
/// is empty (no arch → no cells → no per-intent affinity) — without it,
/// a `builtins.fetchurl` pod can schedule onto any untainted node.
///
/// Uses the §13e taint-key (`FETCHER_TAINT_KEY = rio.build/fetcher`), NOT
/// the deleted `rio.build/node-role` convention. The selector key is a
/// Rust const; the matching NodeClaim *label* key is helm config
/// (`[sla.hw_classes.fetcher-*].labels`, propagated through
/// `cover::build_nodeclaim`'s `hw.labels` extend). They are NOT
/// structurally single-sourced — `helm/20-fetcher-feature-routing.sh`
/// is the cross-layer guard that keeps them aligned. A typo in either
/// is a permanently-Pending pod with an affinity no Node satisfies.
// r[impl fetcher.node.dedicated+4]
// r[impl ctrl.pool.fetcher-affinity-from-intent+5]
pub(super) fn effective_node_selector(pool: &Pool) -> Option<BTreeMap<String, String>> {
    if is_fetcher(pool) {
        // r35 bug_044 (§Permissive-restrictive asymmetry): the
        // pool-static fetcher constraint is RESTRICTIVE and
        // load-bearing for `system="builtin"` FODs whose
        // `hw_class_names=[]` carries no per-intent affinity. The
        // operator selector ADDS constraints (AZ pin, instance type),
        // it does not remove this one. The pre-r35 `or_else` (replace)
        // let an AZ pin co-tenant an open-egress fetcher with
        // rio-controller's node — dropping the constraint removes the
        // lateral-movement boundary. `effective_tolerations` (r37
        // bug_001) is the merge-not-replace dual: missing the
        // toleration deadlocks against THIS unconditional nodeSelector.
        // The functions are pair-coupled — every key this fn ADDS
        // requires a matching toleration there.
        //
        // UNCONDITIONAL `insert` — never `or_insert_with`. If the
        // operator sets `{rio.build/fetcher: "false"}`, the entry
        // exists and `or_insert_with` would PRESERVE it: the constraint
        // is silently weakened and the pod escapes the dedicated taint.
        // The constraint is universal; the operator cannot weaken it.
        // The CEL guard rejects the misconfig at admission for new
        // specs; this is the belt-and-suspenders for pre-CEL specs.
        let mut ns = pool.spec.node_selector.clone().unwrap_or_default();
        ns.insert(rio_common::k8s::FETCHER_TAINT_KEY.into(), "true".into());
        Some(ns)
    } else {
        pool.spec.node_selector.clone()
    }
}

/// Maps a `NodeTaint` proto to a kube `Toleration`. The §13d chokepoint
/// for taint→toleration projection — `effective_tolerations` (pool-static
/// fetcher arm), `build_executor_pod_spec` (metal-toleration block),
/// `apply_intent_resources` (per-intent hwClass tolerations), and any
/// future caller MUST route through this so the cover-taint↔toleration
/// round-trip and the per-intent dedup (`pod_t.contains`) see
/// byte-identical `Toleration` values. Same `pub(super)` shape as
/// [`proto_term_to_k8s`].
pub(super) fn taint_to_toleration(nt: rio_proto::types::NodeTaint) -> Toleration {
    Toleration {
        key: Some(nt.key),
        operator: Some("Equal".into()),
        value: Some(nt.value),
        effect: Some(nt.effect),
        ..Default::default()
    }
}

/// §13e (mirrors r33 bug_011 for metal): pool-static structural
/// toleration (fetcher OR builder) reads `taints_routing_to(...)` /
/// the literal kind taint, and is MERGED with `pool.spec.tolerations`,
/// never replaced.
///
/// **r37 bug_001 (§Permissive-restrictive asymmetry — fetcher arm) +
/// r38 bug_027 (sibling — builder arm):** both kinds carry a
/// kind-derived structural taint on every cover-minted NodeClaim
/// (`rio.build/fetcher` from `taints_routing_to`, `rio.build/builder`
/// from `cover.rs::builder_taint()`). Helm `mergeOverwrite` REPLACES
/// list-typed `pool.spec.tolerations`, so an operator setting any
/// tolerations override (an AZ taint, an audit taint, or `[]`) drops
/// the structural toleration. The pod is still pinned to the tainted
/// nodes by `effective_node_selector` (fetcher) or per-intent
/// `nodeAffinity` (builder) — toleration gone + affinity required =
/// permanent Pending with no warn/metric. The merge is now
/// UNCONDITIONAL — both arms append the structural set with dedup.
///
/// No CEL guard needed: tolerations are purely additive — there is no
/// value the operator can set that breaks scheduling once the merge
/// covers both arms.
// r[impl ctrl.pool.intent-tolerations]
// r[impl ctrl.pool.fetcher-tolerations]
// r[impl ctrl.pool.builder-tolerations]
fn effective_tolerations(pool: &Pool, hw: &HwClassConfig) -> Option<Vec<Toleration>> {
    let mut t = pool.spec.tolerations.clone().unwrap_or_default();
    let mut structural: Vec<Toleration> = if is_fetcher(pool) {
        let mut tols: Vec<Toleration> = hw
            .taints_routing_to(rio_common::k8s::FETCHER_TAINT_KEY)
            .into_iter()
            .map(taint_to_toleration)
            .collect();
        if tols.is_empty() {
            // Cold-start: no fetcher hwClass loaded yet. Literal floor
            // (mirrors `wants_metal`'s `kvm` fallback — fail-OPEN).
            tols.push(Toleration {
                key: Some(rio_common::k8s::FETCHER_TAINT_KEY.into()),
                operator: Some("Exists".into()),
                effect: Some("NoSchedule".into()),
                ..Default::default()
            });
        }
        tols
    } else {
        // Builder: every cover-minted builder NodeClaim carries
        // `rio.build/builder=true:NoSchedule` (cover.rs::builder_taint()).
        // The builder taint is NOT in hwClass config (cover.rs:448-452
        // TODO — it's hardcoded in cover, not declared per-class), so
        // `taints_routing_to(BUILDER_TAINT_KEY)` is always empty.
        // Use the literal toleration. When the cover.rs TODO is closed
        // (single-source through hwClass config), this arm collapses
        // into a single `taints_routing_to(BUILDER_TAINT_KEY)` call.
        vec![Toleration {
            key: Some(rio_common::k8s::BUILDER_TAINT_KEY.into()),
            operator: Some("Equal".into()),
            value: Some("true".into()),
            effect: Some("NoSchedule".into()),
            ..Default::default()
        }]
    };
    for tol in structural.drain(..) {
        if !t.contains(&tol) {
            t.push(tol);
        }
    }
    Some(t)
}

/// Pool advertises a feature that routes drvs to a metal hw-class →
/// pod needs the metal *toleration* (permissive). Fetcher Pools'
/// `effective_features` is forced to `[fetcher]`
/// (`r[ctrl.crd.fetcher-no-features+2]`, §13e) — never `kvm` — so the
/// metal-routing predicate can never fire for them; short-circuit on
/// `is_fetcher`. See `r[ctrl.pool.kvm-device+2]`.
///
/// **§Permissive-restrictive asymmetry (r33 bug_002):** this predicate
/// gates the toleration only, never a `nodeSelector`. A pool-static
/// nodeSelector is a *restrictive* constraint that must be UNIVERSAL
/// over the Pool's intents — but `wants_metal` keys on
/// `pool.spec.features`, which is *existential* (some intent from this
/// Pool may need metal). When a feature is shared by metal + non-metal
/// classes, an intent legitimately routes to the non-metal cell and the
/// pool-static nodeSelector contradicts the per-intent affinity →
/// permanent Pending + `reap_idle` mint→reap loop. Restrictive
/// placement is `intent.node_affinity` only — the same source `cover`
/// reads, so they cannot drift.
///
/// §Partition-single-source (r31 bug_020): routing keys on
/// `features_compatible(required, provides_features)`, so the
/// toleration gate must read the SAME `provides_features` map. The
/// hardcoded `f == "kvm"` check broke when bug_007 added
/// `nixos-test` to metal `providesFeatures` — a Pool with
/// `features: ["nixos-test"]` (no `"kvm"`) routed to metal but had
/// no taint toleration, so its pods sat permanently Pending. The set
/// is `{"kvm"} ∪ ⋃_{h: kvm-tainted} provides_features(h)`: the
/// literal floor is fail-OPEN under a not-yet-loaded `hw_config`
/// (otherwise every kvm Pool's pod would lose the metal toleration
/// in the cold-start window — strictly worse than the bug); the
/// union extends it to whatever ELSE routes to a kvm-tainted class.
fn wants_metal(pool: &Pool, hw: &HwClassConfig) -> bool {
    if is_fetcher(pool) {
        return false;
    }
    if pool.spec.features.iter().any(|f| f == KVM_FEATURE) {
        return true;
    }
    let routable = hw.features_routing_to_taint(KVM_NODE_LABEL);
    pool.spec.features.iter().any(|f| routable.contains(f))
}

/// Labels for Job + pod template. Includes the ADR-019
/// `rio.build/role` label so
/// NetworkPolicies and `kubectl get pods -l rio.build/role=fetcher`
/// can target by role.
pub fn executor_labels(pool: &Pool) -> BTreeMap<String, String> {
    let kind = pool.spec.kind;
    BTreeMap::from([
        (POOL_LABEL.into(), pool.name_any()),
        (ROLE_LABEL.into(), kind.as_str().into()),
        ("app.kubernetes.io/name".into(), "rio-builder".into()),
        // D3a: cluster-wide netpols select on this label (ns-agnostic
        // airgap). Value is `rio-builder` / `rio-fetcher` — matches
        // the helm component naming convention.
        (
            "app.kubernetes.io/component".into(),
            kind.component_label().into(),
        ),
        ("app.kubernetes.io/part-of".into(), "rio-build".into()),
    ])
}

/// Proto → k8s-openapi `NodeSelectorTerm`. Field-by-field copy — the
/// proto message in `admin_types.proto` deliberately mirrors the k8s
/// shape (including `match_expressions` only; `match_fields` is unused
/// by the §13a admissible-set encoding). A `From` impl would violate
/// the orphan rule (both types foreign to rio-controller), so this
/// free fn — same pattern as [`super::executor_kind_to_proto`] — is
/// the single conversion point.
pub(super) fn proto_term_to_k8s(t: &rio_proto::types::NodeSelectorTerm) -> NodeSelectorTerm {
    NodeSelectorTerm {
        match_expressions: Some(
            t.match_expressions
                .iter()
                .map(|r| NodeSelectorRequirement {
                    key: r.key.clone(),
                    operator: r.operator.clone(),
                    values: if r.values.is_empty() {
                        None
                    } else {
                        Some(r.values.clone())
                    },
                })
                .collect(),
        ),
        match_fields: None,
    }
}

/// AD2: render the intent's `excluded_nodes` (the scheduler's
/// node-keyed exclusion set) as REQUIRED node anti-affinity — a
/// `kubernetes.io/hostname NotIn [...]` requirement ANDed into every
/// existing required `nodeSelectorTerm` (terms are OR'd, so appending a
/// separate term would weaken the existing placement instead of
/// constraining it), or a single new term when the intent carried no
/// affinity. Empty `excluded_nodes` is a no-op, so intents without
/// exclusions render byte-identical to today.
// r[impl sched.dispatch.fleet-exhaust+5]
pub(super) fn apply_excluded_nodes_anti_affinity(pod_spec: &mut PodSpec, excluded: &[String]) {
    if excluded.is_empty() {
        return;
    }
    let requirement = NodeSelectorRequirement {
        key: "kubernetes.io/hostname".into(),
        operator: "NotIn".into(),
        values: Some(excluded.to_vec()),
    };
    let node_affinity = pod_spec
        .affinity
        .get_or_insert_with(Default::default)
        .node_affinity
        .get_or_insert_with(Default::default);
    let required = node_affinity
        .required_during_scheduling_ignored_during_execution
        .get_or_insert_with(|| k8s_openapi::api::core::v1::NodeSelector {
            node_selector_terms: Vec::new(),
        });
    if required.node_selector_terms.is_empty() {
        required
            .node_selector_terms
            .push(NodeSelectorTerm::default());
    }
    for term in &mut required.node_selector_terms {
        term.match_expressions
            .get_or_insert_with(Vec::new)
            .push(requirement.clone());
    }
}

/// Map a nix `systems` list to the `kubernetes.io/arch` nodeSelector value
/// of a *host* that can run all of them natively. `None` when the list is
/// empty, spans incompatible host arches, or is `builtin`-only — those
/// pools deliberately float and rely on rio-builder's startup arch check
/// (I-098 part B) instead.
///
/// 32-bit guest systems map to their 64-bit host (i686→amd64,
/// armv7l→arm64): a pool advertising `[x86_64-linux, i686-linux]` via
/// `extra-platforms` must land on amd64, and no cloud provider offers
/// 386/arm nodes anyway.
// r[impl ctrl.pod.arch-selector+2]
pub(super) fn nix_systems_to_k8s_arch(systems: &[String]) -> Option<&'static str> {
    let mut arch: Option<&'static str> = None;
    for s in systems {
        let a = match s.split_once('-').map(|(a, _)| a).unwrap_or(s.as_str()) {
            "x86_64" | "i686" => "amd64",
            "aarch64" | "armv7l" | "armv6l" => "arm64",
            "builtin" => continue,
            _ => return None,
        };
        match arch {
            None => arch = Some(a),
            Some(prev) if prev == a => {}
            Some(_) => return None,
        }
    }
    arch
}

/// Fixed product prefix for executor resource names (I-104). Pool name
/// is the disambiguating SUFFIX (typically arch: `x86-64`, `aarch64`).
const NAME_PREFIX: &str = "rio";

/// Job name. `rio-{role}-{pool_name}-{6-char-suffix}` — logs/metrics
/// group naturally by role+pool prefix.
// r[impl ctrl.pool.spawn-once]
pub fn job_name(pool_name: &str, role: ExecutorKind, suffix: &str) -> String {
    format!("{NAME_PREFIX}-{}-{pool_name}-{suffix}", role.as_str())
}

/// The Job pod spec — shared by both pool kinds.
// r[impl ctrl.pool.fetcher-hardening+3]
pub fn build_executor_pod_spec(
    pool: &Pool,
    scheduler: &UpstreamAddrs,
    store: &UpstreamAddrs,
    hw_config: &HwClassConfig,
) -> PodSpec {
    // cgroup handling: we do NOT hostPath-mount /sys/fs/cgroup.
    // See builderpool/builders.rs pre-extraction commentary for the
    // full cgroupns-vs-hostPath reasoning; short version: containerd
    // cgroup-namespaces the container, and with privileged the
    // namespaced mount is RW — no hostPath needed, and a hostPath
    // would clobber host systemd.

    let fetcher = is_fetcher(pool);
    let privileged = effective_privileged(pool);
    let seccomp = effective_seccomp(pool);
    // Fetchers: rootfs tampering blocked (overlay upperdir is a tmpfs
    // emptyDir). Builders: false. ADR-019 §Sandbox hardening.
    let read_only_root_fs = fetcher;
    let host_network = if fetcher {
        None
    } else {
        pool.spec.host_network
    };
    // Localhost seccomp: profile lives on node disk, written by
    // systemd-tmpfiles BEFORE kubelet starts on every supported target
    // (NixOS AMI: nix/nixos-node/hardening.nix; k3s VM tests:
    // fixtures/k3s-full.nix). The file is guaranteed present before any
    // pod schedules, so no wait machinery is needed — only the
    // pod-level/container-level split (sandbox uses RuntimeDefault, the
    // executor container enforces Localhost). Gated on !privileged
    // (privileged disables seccomp at runtime).
    let seccomp_localhost = (!privileged)
        .then_some(seccomp.as_ref())
        .flatten()
        .filter(|k| k.type_ == "Localhost")
        .is_some();

    PodSpec {
        containers: vec![build_executor_container(
            pool,
            scheduler,
            store,
            privileged,
            read_only_root_fs,
            seccomp.as_ref(),
        )],

        host_network: host_network.filter(|&h| h),
        dns_policy: host_network
            .filter(|&h| h)
            .map(|_| "ClusterFirstWithHostNet".into()),

        // r[impl sec.pod.host-users-false]
        // User-namespace isolation. See ADR-012. Incompatible with
        // privileged, hostNetwork, and hostPath /dev/fuse. The
        // spec.hostUsers override handles containerd<2.1 cgroup
        // ownership issues (cgroup_writable knob).
        host_users: effective_host_users(pool)
            .or_else(|| (!privileged && host_network != Some(true)).then_some(false)),

        // Pod-level seccomp. RuntimeDefault when Localhost is
        // requested — the pod sandbox (pause container) doesn't need
        // pivot_root, and keeping it on RuntimeDefault means a missing
        // profile surfaces as the executor container's
        // CreateContainerError (with the profile path in the message)
        // instead of a generic sandbox CreatePodSandBoxError. The
        // Localhost enforcement is on the executor container's
        // SecurityContext.
        security_context: if !privileged {
            Some(PodSecurityContext {
                seccomp_profile: Some(if seccomp_localhost {
                    SeccompProfile {
                        type_: "RuntimeDefault".into(),
                        ..Default::default()
                    }
                } else {
                    build_seccomp_profile(seccomp.as_ref())
                }),
                ..Default::default()
            })
        } else {
            None
        },

        volumes: Some({
            let mut v = vec![
                // FUSE cache. emptyDir = local ephemeral storage,
                // wiped on pod restart. sizeLimit enforced by
                // kubelet.
                Volume {
                    name: "fuse-cache".into(),
                    empty_dir: Some(EmptyDirVolumeSource {
                        size_limit: Some(Quantity(fuse_cache_bytes(pool).to_string())),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                // Overlay upperdir/workdir. MUST be a real
                // filesystem (not the container's overlayfs root).
                // emptyDir gives us the kubelet's local disk.
                //
                // When `read_only_root_fs` is true (fetchers), this
                // is the ONLY writable path — overlay writes still
                // work, rootfs tampering does not. Disk-backed for
                // BOTH kinds: ADR-019 originally specced tmpfs for
                // fetchers ("FOD fetches are short, fits in pod
                // memory limit"), but under ADR-023 `limits.memory`
                // is SLA-computed from RSS alone while overlay writes
                // are budgeted under `ephemeral-storage` from
                // `disk_bytes` — `medium: Memory` made a 6+ GiB
                // unpack OOM the pod while the disk reservation sat
                // unused, AND `quota::current_bytes()` (XFS prjquota)
                // returned None on tmpfs so `peak_disk_bytes` never
                // fitted (bug_074).
                Volume {
                    name: "overlays".into(),
                    empty_dir: Some(EmptyDirVolumeSource::default()),
                    ..Default::default()
                },
                // r[impl ctrl.pool.hw-class-annotation]
                // Downward-API VOLUME (not env var): kubelet refreshes
                // file contents on annotation change. The annotation is
                // stamped reactively by `run_pod_annotator` AFTER
                // `spec.nodeName` binds — the same event that triggers
                // kubelet to create the container. With the env-var
                // form kubelet resolves once at container-create and
                // never updates; on warm nodes (~100-300ms to create)
                // or under SpawnIntent burst the annotator loses and
                // `RIO_HW_CLASS=""` permanently. The volume + bounded
                // poll in `rio_builder::hw_class::resolve` makes the
                // race per-pod transient instead of permanent.
                Volume {
                    name: "downward".into(),
                    downward_api: Some(DownwardAPIVolumeSource {
                        items: Some(vec![DownwardAPIVolumeFile {
                            path: "hw-class".into(),
                            field_ref: Some(ObjectFieldSelector {
                                field_path: "metadata.annotations['rio.build/hw-class']".into(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ];
            if read_only_root_fs {
                for (name, _, medium, size_limit) in READ_ONLY_ROOT_MOUNTS {
                    v.push(Volume {
                        name: (*name).into(),
                        empty_dir: Some(EmptyDirVolumeSource {
                            medium: medium.map(Into::into),
                            size_limit: size_limit.map(|s| Quantity(s.into())),
                        }),
                        ..Default::default()
                    });
                }
            }
            // r[impl sec.pod.fuse-device-plugin]
            // /dev/fuse: non-privileged path needs no volume —
            // containerd base_runtime_spec mknods the device node
            // unconditionally on every pod (nix/base-runtime-spec.nix).
            // Privileged escape hatch uses hostPath.
            if privileged {
                v.push(Volume {
                    name: "dev-fuse".into(),
                    host_path: Some(HostPathVolumeSource {
                        path: "/dev/fuse".into(),
                        type_: Some("CharDevice".into()),
                    }),
                    ..Default::default()
                });
            }
            // nix.conf ConfigMap. `optional: true` so a missing
            // ConfigMap mounts an empty dir → setup_nix_conf falls
            // back to WORKER_NIX_CONF.
            v.push(Volume {
                name: "nix-conf".into(),
                config_map: Some(ConfigMapVolumeSource {
                    name: "rio-nix-conf".into(),
                    optional: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            });
            // Coverage propagation: test-only hostPath when the
            // controller is running under -Cinstrument-coverage.
            if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
                v.push(Volume {
                    name: "cov".into(),
                    host_path: Some(HostPathVolumeSource {
                        path: "/var/lib/rio/cov".into(),
                        type_: Some("DirectoryOrCreate".into()),
                    }),
                    ..Default::default()
                });
            }
            v
        }),

        // `rio-builder` / `rio-fetcher` SA (helm rbac.yaml renders both
        // unconditionally). Functionally inert (automount:false, no RBAC
        // bindings) — set so `kubectl describe pod` shows the role-SA
        // not `default`, and so future per-role IRSA annotations attach
        // to the right SA without a controller change.
        service_account_name: Some(pool.spec.kind.component_label().into()),
        automount_service_account_token: Some(false),
        // r[impl ctrl.pod.tgps-default+4]
        // The AD5 abort grace (45 s), unconditional: SIGTERM is an
        // abort, not a drain. The 2 h / 600 s drain graces and the
        // operator spec override existed for the stream path's
        // finish-if-you-can semantics, which retired with the
        // dispatch-mode knob.
        termination_grace_period_seconds: Some(PULL_MODE_TGPS_SECS),
        node_selector: {
            let mut ns = effective_node_selector(pool).unwrap_or_default();
            // r[impl ctrl.pod.arch-selector+2]
            // I-098: a pool with systems=[x86_64-linux] landed pods on an
            // arm64 node (fallback NodePool unconstrained) — builder
            // registers as x86_64 from RIO_SYSTEMS, scheduler dispatches
            // x86_64 drvs, nix-daemon refuses (host is aarch64). Derive
            // kubernetes.io/arch from systems so karpenter constrains arch.
            // Fetchers run `builtin` (arch-agnostic) AND arch-typed FODs
            // from `pool.spec.systems`; rio-builder's startup arch check
            // (now applied to Fetcher kind too — r35 bug_039) is the
            // safety net for misplaced executors of either kind.
            //
            // r35 bug_039: §13e dropped the helm-static fetcher arch
            // nodeSelector and `validate_host_arch` skipped Fetcher;
            // both compensations gone → x86-64-fetcher pod on arm64
            // CrashLoopBackOff'd forever (kubelet does not reschedule
            // on container exit). Arch is UNIVERSAL — applies to
            // fetcher Pools too. `nix_systems_to_k8s_arch` skips
            // `builtin`, so a `["builtin"]`-only Pool stays
            // arch-agnostic (no pin).
            //
            // Why is THIS pool-static restrictive constraint safe and
            // a kvm one was not (r33 bug_002)? Because
            // `nix_systems_to_k8s_arch` returns `Some` iff ALL of
            // `pool.spec.systems` map to one host arch — the constraint
            // is UNIVERSAL over the Pool's intents (every drv this Pool
            // services is for that arch). `pool.spec.features` is
            // existential, not universal: an intent may need only one
            // of the Pool's features and that one feature may route to
            // a non-metal class. Don't generalize this pattern to a
            // feature-gated nodeSelector — it's the §Permissive-
            // restrictive asymmetry.
            if let Some(arch) = nix_systems_to_k8s_arch(&pool.spec.systems) {
                ns.entry("kubernetes.io/arch".into()).or_insert(arch.into());
            }
            // No pool-static kvm nodeSelector (r33 bug_002). kvm
            // placement is per-intent only: `intent.node_affinity`
            // (`jobs.rs::apply_intent_resources`,
            // `r[ctrl.pool.node-affinity-from-intent]`) carries the
            // metal `def.labels` (incl. `rio.build/kvm`) for fitted
            // intents; `fallback_cell` (`nodeclaim_pool/mod.rs`) is
            // feature- AND ceiling-aware so the cold-start
            // `hw_class_names=[]` case mints a metal cell when one can
            // host the intent — and returns `None` (caller emits
            // `no_hosting_class`) when none can. A pool-static
            // `nodeSelector{rio.build/kvm}` gated on `pool.spec.features`
            // is a SECOND opinion that contradicts the intent's affinity
            // whenever a routable feature is shared by metal + non-metal
            // hwClasses: scheduler routes to the cheaper non-metal cell,
            // cover mints a non-metal node, the nodeSelector excludes it
            // → permanent Pending + `reap_idle` mint→reap loop.
            //
            // Cost of the deletion: a featured `hw_class_names=[]`
            // cold-start pod has no per-intent affinity to bind it to
            // metal. Two triggers, both operator-attributable:
            // (a) over-ask — `--cores=N` exceeds every per-class
            //     ceiling, `reference_hw_class_for_system` and
            //     `fallback_cell` both return `None` → no cell minted →
            //     pod Pending, `no_hosting_class` warns;
            // (b) feature with zero providing classes — `fallback_cell`'s
            //     `features_compatible` filter returns `None` → same
            //     `no_hosting_class` warn → no node minted → pod Pending.
            // Without the deleted nodeSelector a stray non-metal node
            // (minted for OTHER intents) could host the pod, which then
            // fails on missing `/dev/kvm` → CrashLoopBackOff (not
            // Pending). Same operator root cause, different `kubectl get
            // pods` shape — both observable, both surfaced by
            // `unroutable_features_total` / `no_hosting_class`. Strictly
            // better than the silent mint→reap deadlock the nodeSelector
            // caused.
            if ns.is_empty() { None } else { Some(ns) }
        },
        tolerations: {
            let mut t = effective_tolerations(pool, hw_config).unwrap_or_default();
            // r[impl ctrl.pool.kvm-device+2]
            // Metal NodeClaims are tainted (cover::build_nodeclaim reads
            // `[sla.hw_classes.$h].taints`) so non-kvm builders don't
            // bin-pack onto $$ metal. Permissive — over-firing when
            // `wants_metal` mis-predicts is harmless (an unused
            // toleration). Cold-start fallback for `hw_class_names=[]`
            // intents whose per-intent toleration loop produced nothing.
            // Derive the taint set from the SAME map `cover` reads
            // (r33 bug_011); literal kvm floor is fail-OPEN under
            // unloaded config (mirrors `wants_metal`'s literal `"kvm"`
            // floor). Append-dedup so the structural
            // `rio.build/builder` toleration (now injected by
            // `effective_tolerations`, not operator-supplied — r38
            // bug_027) survives.
            if wants_metal(pool, hw_config) {
                let mut metal_tols: Vec<Toleration> = hw_config
                    .taints_routing_to(KVM_NODE_LABEL)
                    .into_iter()
                    .map(taint_to_toleration)
                    .collect();
                let floor = Toleration {
                    key: Some(KVM_NODE_LABEL.into()),
                    operator: Some("Equal".into()),
                    value: Some("true".into()),
                    effect: Some("NoSchedule".into()),
                    ..Default::default()
                };
                if !metal_tols.contains(&floor) {
                    metal_tols.push(floor);
                }
                for tol in metal_tols {
                    if !t.contains(&tol) {
                        t.push(tol);
                    }
                }
            }
            if t.is_empty() { None } else { Some(t) }
        },

        ..Default::default()
    }
}

/// The executor container. `privileged` / `read_only_root_fs` /
/// `seccomp` are passed in (already computed by the caller) so the
/// pod-level and container-level views can't drift.
fn build_executor_container(
    pool: &Pool,
    scheduler: &UpstreamAddrs,
    store: &UpstreamAddrs,
    privileged: bool,
    read_only_root_fs: bool,
    seccomp: Option<&SeccompProfileKind>,
) -> Container {
    let fetcher = is_fetcher(pool);

    Container {
        name: pool.spec.kind.as_str().into(),
        image: Some(pool.spec.image.clone()),
        command: Some(vec!["/bin/rio-builder".into()]),
        image_pull_policy: pool.spec.image_pull_policy.clone(),
        env: Some({
            let mut e = vec![
                env("RIO_SCHEDULER__ADDR", &scheduler.addr),
                env("RIO_STORE__ADDR", &store.addr),
                env("RIO_FUSE_MOUNT_POINT", "/var/rio/fuse-store"),
                env("RIO_FUSE_CACHE_DIR", "/var/rio/cache"),
                env("RIO_OVERLAY_BASE_DIR", "/var/rio/overlays"),
                env("RIO_LOG_FORMAT", "json"),
                env("RIO_SYSTEMS", &pool.spec.systems.join(",")),
                // Single source: `effective_features` (Fetcher → [fetcher]).
                env("RIO_FEATURES", &effective_features(&pool.spec).join(",")),
                // Executor self-identification. The RIO_ env layer reads
                // `executor_id` → prefix RIO_ → `RIO_EXECUTOR_ID`.
                // Job pods are `<job-name>-<suffix>` — unique per
                // pod (one build, one id).
                env_from_field("RIO_EXECUTOR_ID", "metadata.name"),
                // ADR-023 hw_class join: CompletionReport carries
                // spec.nodeName so the scheduler can resolve the node's
                // instance type when recording build-history samples.
                env_from_field("RIO_NODE_NAME", "spec.nodeName"),
                // ADR-023 SpawnIntent match key. Reads the pod
                // annotation set by `build_job` when spawning from a
                // SpawnIntent. Absent annotation (recovery path) →
                // kubelet resolves to "" → builder maps to None. Set
                // unconditionally so the env list is role-agnostic.
                env_from_field(
                    "RIO_INTENT_ID",
                    "metadata.annotations['rio.build/intent-id']",
                ),
                // ADR-023 phase-10 hw self-calibration: `rio.build/
                // hw-class` is exposed via the `downward` VOLUME (see
                // r[ctrl.pool.hw-class-annotation]), not an env var —
                // env-var form resolves once at container-create and
                // races `run_pod_annotator`. Builder reads
                // `/etc/rio/downward/hw-class` with a bounded poll.
                // Role discriminator. rio-builder's `RIO_EXECUTOR_
                // KIND` gates the FOD-vs-non-FOD refusal (ADR-019
                // §Executor enforcement — a builder receiving a FOD
                // returns WrongKind without spawning).
                env("RIO_EXECUTOR_KIND", pool.spec.kind.as_str()),
                // Idle-exit bound (merged_bug_221 leg 2): rendered
                // from POOL_IDLE_EXIT_SECS so the orphan-reap grace
                // const-asserts its headroom against the value pods
                // actually run with (pod env wins over image env).
                env("RIO_IDLE_SECS", "120"),
            ];
            // No dispatch-protocol discriminator: pull is the only
            // delivery path (the builder binary ignores a stray
            // RIO_DISPATCH_MODE env, and nothing renders one anymore —
            // the knob retired with the stream path).
            if let Some(host) = &scheduler.balance_host {
                e.push(env("RIO_SCHEDULER__BALANCE_HOST", host));
                e.push(env(
                    "RIO_SCHEDULER__BALANCE_PORT",
                    &scheduler.balance_port.to_string(),
                ));
            }
            if let Some(host) = &store.balance_host {
                e.push(env("RIO_STORE__BALANCE_HOST", host));
                e.push(env(
                    "RIO_STORE__BALANCE_PORT",
                    &store.balance_port.to_string(),
                ));
            }
            // Builder-only tuning knobs. Fetchers leave all unset.
            if !fetcher {
                if let Some(n) = pool.spec.fuse_threads {
                    e.push(env("RIO_FUSE_THREADS", &n.to_string()));
                }
                if let Some(p) = pool.spec.fuse_passthrough {
                    e.push(env(
                        "RIO_FUSE_PASSTHROUGH",
                        if p { "true" } else { "false" },
                    ));
                }
            }
            // Coverage + RUST_LOG passthrough (test-only / operator
            // knob respectively). `$(RIO_EXECUTOR_ID)` (downward-API
            // metadata.name, defined ABOVE so kubelet's dependent-var
            // expansion applies) disambiguates per-pod: the `cov`
            // hostPath is shared across all executor pods on a node,
            // each runs PID 1 → `%p`→1, same image → `%m` identical.
            // `%h` is NOT used — `host_network=true` (builder pools
            // may set it) makes the pod hostname the NODE hostname.
            if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
                e.push(env(
                    "LLVM_PROFILE_FILE",
                    "/var/lib/rio/cov/rio-$(RIO_EXECUTOR_ID)-%p-%m.profraw",
                ));
            }
            if let Ok(level) = std::env::var("RUST_LOG") {
                e.push(env("RUST_LOG", &level));
            }
            e
        }),

        volume_mounts: Some({
            let mut m = vec![
                VolumeMount {
                    name: "fuse-cache".into(),
                    mount_path: "/var/rio/cache".into(),
                    ..Default::default()
                },
                VolumeMount {
                    name: "overlays".into(),
                    mount_path: "/var/rio/overlays".into(),
                    ..Default::default()
                },
                VolumeMount {
                    name: "downward".into(),
                    mount_path: "/etc/rio/downward".into(),
                    read_only: Some(true),
                    ..Default::default()
                },
            ];
            if read_only_root_fs {
                for (name, mount_path, _, _) in READ_ONLY_ROOT_MOUNTS {
                    m.push(VolumeMount {
                        name: (*name).into(),
                        mount_path: (*mount_path).into(),
                        ..Default::default()
                    });
                }
            }
            if privileged {
                m.push(VolumeMount {
                    name: "dev-fuse".into(),
                    mount_path: "/dev/fuse".into(),
                    ..Default::default()
                });
            }
            m.push(VolumeMount {
                name: "nix-conf".into(),
                mount_path: "/etc/rio/nix-conf".into(),
                read_only: Some(true),
                ..Default::default()
            });
            if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
                m.push(VolumeMount {
                    name: "cov".into(),
                    mount_path: "/var/lib/rio/cov".into(),
                    ..Default::default()
                });
            }
            m
        }),

        security_context: Some(SecurityContext {
            privileged: privileged.then_some(true),
            capabilities: Some(Capabilities {
                // nix-daemon sandbox cap set. See builderpool/
                // builders.rs pre-extraction commentary for the
                // per-cap rationale (SETUID/GID for nixbld drop,
                // NET_ADMIN for lo up in newns, SETPCAP for the
                // inheritable-caps dance post-CVE-2022-24769, etc).
                add: Some(vec![
                    "SYS_ADMIN".into(),
                    "SYS_CHROOT".into(),
                    "SETUID".into(),
                    "SETGID".into(),
                    "NET_ADMIN".into(),
                    "CHOWN".into(),
                    "DAC_OVERRIDE".into(),
                    "KILL".into(),
                    "FOWNER".into(),
                    "SETPCAP".into(),
                ]),
                ..Default::default()
            }),
            // allowPrivilegeEscalation=true: runc's no_new_privs
            // clears ambient caps on exec, breaking nix-daemon's
            // pivot_root. k8s defaults to true when CAP_SYS_ADMIN
            // is present, but PSA may override — be explicit.
            allow_privilege_escalation: Some(true),
            // Container-level seccomp: set ONLY when Localhost is
            // requested. Pod-level stays RuntimeDefault in that case
            // (see build_executor_pod_spec security_context).
            seccomp_profile: seccomp
                .filter(|k| k.type_ == "Localhost")
                .map(|_| build_seccomp_profile(seccomp)),
            // Fetcher hardening: rootfs tampering blocked. The
            // overlay upperdir (tmpfs emptyDir) is still writable.
            // ADR-019 §Sandbox hardening.
            read_only_root_filesystem: read_only_root_fs.then_some(true),
            ..Default::default()
        }),

        // /dev/{fuse,kvm} arrive via containerd base_runtime_spec on
        // every pod (nix/base-runtime-spec.nix) — no extended-resource
        // request. kvm placement is the per-intent nodeAffinity
        // (r[ctrl.pool.node-affinity-from-intent]) plus the pool-static
        // toleration above (r[ctrl.pool.kvm-device+2]) — never a
        // pool-static nodeSelector (r33 bug_002). resources are stamped
        // per-intent by `jobs::apply_intent_resources` AFTER this
        // builder runs.
        resources: None,

        ports: Some(vec![
            ContainerPort {
                name: Some("metrics".into()),
                container_port: 9093,
                ..Default::default()
            },
            ContainerPort {
                name: Some("health".into()),
                container_port: 9193,
                ..Default::default()
            },
        ]),

        // live_056-b: READINESS wired to the builder's SERVING state
        // — httpGet /servingz on the builder's own health server,
        // which answers 200 iff the serving-state file exists
        // (rio_common::k8s::BUILDER_SERVING_STATE_FILE, written once
        // connect_upstreams succeeds: post-connect, pre-first-pull).
        // Pod Ready ⟺ past cold start and asking for work — a
        // policy-blackholed builder is visibly NotReady for exactly
        // the un-served window (W9-CO), and JobStatus.ready /
        // PoolStatus.ready_replicas finally mean what they document.
        // The is_pending_job reap boundary IMPROVES: started-but-not-
        // serving reads ready==0 (reapable-pending) — safe, because a
        // builder with no scheduler channel cannot hold an assignment,
        // and the pulled-while-probe-lagged sliver is covered by the
        // live-pod recheck + the delete chokepoint's attempt veto.
        // Mechanism note (divergence recorded): an exec test on the
        // file was rejected — the builder image ships no coreutils/
        // shell (nix + fuse3 + util-linuxMinimal only), so an exec
        // probe could never pass; the in-tree HealthClient prober
        // (rio-proto/src/client/balance.rs `probe`) is the cited
        // CLIENT-side precedent and stays untouched — kubelet's
        // httpGet against the existing health port is the pod-side
        // form. The probe is flap-free by construction: the file is
        // created once and never removed for the pod's one-shot life.
        readiness_probe: Some(Probe {
            http_get: Some(HTTPGetAction {
                path: Some("/servingz".into()),
                port: k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::String(
                    "health".into(),
                ),
                ..Default::default()
            }),
            // Fast flip (the reap boundary consumes this): 2s period,
            // first probe immediately; one success marks Ready.
            period_seconds: Some(2),
            timeout_seconds: Some(1),
            failure_threshold: Some(3),
            success_threshold: Some(1),
            ..Default::default()
        }),

        // No LIVENESS/STARTUP probes: executor pods are one-shot
        // Jobs, kill-wired by EXIT (sys.guard.kill-wired-isolated —
        // the doctrine rule this folklore graduated into; the
        // cold-start budget exits nonzero, activeDeadlineSeconds
        // bounds hangs) and a CPU-pegged build would fail a
        // 1s-timeout liveness probe → kubelet SIGKILL mid-build →
        // FUSE/overlay torn down → nix-daemon EIO (I-114's liveness
        // half STANDS; only its readiness half is revisited above).
        ..Default::default()
    }
}

// ── small helpers ────────────────────────────────────────────────────

/// Translate CRD `SeccompProfileKind` → k8s-openapi `SeccompProfile`.
/// `None` and unknown types → `RuntimeDefault` (fail-closed — never
/// fall through to Unconfined on a typo).
// r[impl builder.seccomp.localhost-profile+3]
pub fn build_seccomp_profile(kind: Option<&SeccompProfileKind>) -> SeccompProfile {
    match kind.map(|k| k.type_.as_str()) {
        Some("Localhost") => SeccompProfile {
            type_: "Localhost".into(),
            localhost_profile: kind.and_then(|k| k.localhost_profile.clone()),
        },
        Some("Unconfined") => SeccompProfile {
            type_: "Unconfined".into(),
            ..Default::default()
        },
        _ => SeccompProfile {
            type_: "RuntimeDefault".into(),
            ..Default::default()
        },
    }
}

pub fn env(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: Some(value.into()),
        ..Default::default()
    }
}

/// Downward API: env var from pod metadata field.
pub fn env_from_field(name: &str, field_path: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value_from: Some(EnvVarSource {
            field_ref: Some(ObjectFieldSelector {
                field_path: field_path.into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // r[verify ctrl.pod.tgps-default+4]
    /// Every pool renders the AD5 abort grace (45 s) and no
    /// dispatch-mode discriminator: pull is the only delivery
    /// protocol, so there is nothing to select — the stream era's
    /// drain graces (2 h builders / 600 s fetchers / the spec
    /// override) and the `RIO_DISPATCH_MODE` env are gone.
    #[test]
    fn pod_template_renders_abort_grace_and_no_dispatch_discriminator() {
        let scheduler = crate::fixtures::test_sched_addrs();
        let store = crate::fixtures::test_store_addrs();
        let hw = crate::reconcilers::node_informer::HwClassConfig::default();
        let render = |pool: &Pool| build_executor_pod_spec(pool, &scheduler, &store, &hw);

        let builder = crate::fixtures::test_pool("plain", rio_crds::pool::ExecutorKind::Builder);
        let fetcher = crate::fixtures::test_pool("fetch", rio_crds::pool::ExecutorKind::Fetcher);
        for pool in [&builder, &fetcher] {
            let spec = render(pool);
            assert_eq!(
                spec.termination_grace_period_seconds,
                Some(PULL_MODE_TGPS_SECS),
                "every executor pod carries the AD5 abort grace ({:?})",
                pool.spec.kind
            );
            assert!(
                spec.containers[0]
                    .env
                    .as_ref()
                    .unwrap()
                    .iter()
                    .all(|e| e.name != "RIO_DISPATCH_MODE"),
                "the RIO_DISPATCH_MODE discriminator is retired — no pod renders it ({:?})",
                pool.spec.kind
            );
        }
    }

    #[test]
    fn nix_systems_to_k8s_arch_mapping() {
        assert_eq!(
            nix_systems_to_k8s_arch(&s(&["x86_64-linux"])),
            Some("amd64")
        );
        assert_eq!(
            nix_systems_to_k8s_arch(&s(&["aarch64-linux"])),
            Some("arm64")
        );
        // 32-bit guests map to their 64-bit host arch
        assert_eq!(nix_systems_to_k8s_arch(&s(&["i686-linux"])), Some("amd64"));
        assert_eq!(
            nix_systems_to_k8s_arch(&s(&["armv7l-linux"])),
            Some("arm64")
        );
        // r[verify ctrl.pod.arch-selector+2]
        // extra-platforms pool: i686 alongside x86_64 → still amd64
        assert_eq!(
            nix_systems_to_k8s_arch(&s(&["x86_64-linux", "i686-linux"])),
            Some("amd64")
        );
        assert_eq!(
            nix_systems_to_k8s_arch(&s(&["aarch64-linux", "armv7l-linux"])),
            Some("arm64")
        );
        // builtin is ignored
        assert_eq!(
            nix_systems_to_k8s_arch(&s(&["x86_64-linux", "builtin"])),
            Some("amd64")
        );
        // builtin-only → no constraint (fetcher pools)
        assert_eq!(nix_systems_to_k8s_arch(&s(&["builtin"])), None);
        // multi-arch → no constraint
        assert_eq!(
            nix_systems_to_k8s_arch(&s(&["x86_64-linux", "aarch64-linux"])),
            None
        );
        assert_eq!(
            nix_systems_to_k8s_arch(&s(&["i686-linux", "aarch64-linux"])),
            None
        );
        // same arch twice (e.g. x86_64-linux + x86_64-darwin) → still constrains
        assert_eq!(
            nix_systems_to_k8s_arch(&s(&["x86_64-linux", "x86_64-darwin"])),
            Some("amd64")
        );
        // unknown → no constraint
        assert_eq!(nix_systems_to_k8s_arch(&s(&["riscv64-linux"])), None);
        assert_eq!(nix_systems_to_k8s_arch(&[]), None);
    }

    // r[verify ctrl.pool.fetcher-hardening+3]
    // r[verify ctrl.crd.fetcher-no-features+2]
    /// `effective_features` is the single chokepoint: Fetcher →
    /// `[fetcher]` regardless of spec; Builder → verbatim. Both
    /// `RIO_FEATURES` (worker capabilities) and `queued_for_pool`
    /// (spawn-decision query) read it, so they cannot diverge. §13e
    /// inverted the rule from `[]` → `[fetcher]` so the bidirectional
    /// ∅-guard partitions FOD cells from builder cells; a Fetcher Pool
    /// with a stale declared `["kvm"]` (pre-CEL spec) would otherwise
    /// hit the I-181 ∅-guard and never spawn.
    #[test]
    fn effective_features_fetcher_for_fetcher() {
        let mut fetcher = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
        fetcher.spec.features = s(&["kvm", "big-parallel"]);
        assert_eq!(
            effective_features(&fetcher.spec),
            vec![rio_common::k8s::FETCHER_FEATURE.to_string()],
            "Fetcher Pools advertise [fetcher] regardless of declared features"
        );

        let mut builder = crate::fixtures::test_pool("b", ExecutorKind::Builder);
        builder.spec.features = s(&["kvm", "big-parallel"]);
        assert_eq!(
            effective_features(&builder.spec),
            s(&["kvm", "big-parallel"]),
            "Builder: features verbatim"
        );

        // The spawned worker's `RIO_FEATURES` env reads the same value.
        let c = build_executor_container(
            &fetcher,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            false,
            true,
            None,
        );
        let rio_features = c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "RIO_FEATURES")
            .expect("RIO_FEATURES present");
        assert_eq!(
            rio_features.value.as_deref(),
            Some(rio_common::k8s::FETCHER_FEATURE),
            "RIO_FEATURES = [fetcher] for Fetcher (same chokepoint)"
        );
    }

    // r[verify ctrl.job.idle-render-coupled+2]
    /// merged_bug_221 leg 2: the pod spec renders `RIO_IDLE_SECS`
    /// from `POOL_IDLE_EXIT_SECS` (pod env wins over image env), so
    /// the orphan-grace const-assert checks the value pods actually
    /// run with.
    #[test]
    fn pod_renders_idle_exit_seconds() {
        let pool = crate::fixtures::test_pool("p", ExecutorKind::Builder);
        let c = build_executor_container(
            &pool,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            false,
            true,
            None,
        );
        let idle = c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "RIO_IDLE_SECS")
            .expect("RIO_IDLE_SECS rendered into the pod env");
        assert_eq!(
            idle.value.as_deref(),
            Some("120"),
            "rendered value == POOL_IDLE_EXIT_SECS (the const-assert anchor)"
        );
        assert_eq!(POOL_IDLE_EXIT_SECS, 120);
    }

    /// `LLVM_PROFILE_FILE` carries `$(RIO_EXECUTOR_ID)` for per-pod
    /// disambiguation: the `cov` hostPath is shared across all executor
    /// pods on a node, each runs PID 1 → `%p`→1, same image → `%m`
    /// identical. Without the per-pod token, concurrent executors
    /// overwrite each other's profraw. Kubelet's dependent-var
    /// expansion requires `RIO_EXECUTOR_ID` to be defined EARLIER in
    /// the env vec — the index assertion is structural (catches a
    /// future reorder that would silently break expansion).
    #[test]
    fn coverage_profraw_path_per_pod_unique() {
        // rio_test_support::Jail serializes env access across parallel
        // tests (same pattern as the lease-config tests).
        rio_test_support::Jail::expect_with(|jail| {
            jail.set_env("LLVM_PROFILE_FILE", "/dev/null");
            let pool = crate::fixtures::test_pool("p", ExecutorKind::Builder);
            let c = build_executor_container(
                &pool,
                &crate::fixtures::test_sched_addrs(),
                &crate::fixtures::test_store_addrs(),
                false,
                true,
                None,
            );
            let env = c.env.as_ref().unwrap();
            let idx = |name: &str| {
                env.iter()
                    .position(|e| e.name == name)
                    .unwrap_or_else(|| panic!("env {name} present"))
            };
            let prof_idx = idx("LLVM_PROFILE_FILE");
            let exec_idx = idx("RIO_EXECUTOR_ID");
            let prof = &env[prof_idx];
            assert!(
                prof.value
                    .as_deref()
                    .is_some_and(|v| v.contains("$(RIO_EXECUTOR_ID)")),
                "LLVM_PROFILE_FILE carries per-pod token; got {:?}",
                prof.value
            );
            assert!(
                exec_idx < prof_idx,
                "RIO_EXECUTOR_ID (idx {exec_idx}) must precede \
                 LLVM_PROFILE_FILE (idx {prof_idx}) for kubelet \
                 dependent-var expansion"
            );
            Ok(())
        });
    }

    /// §13d toleration axis (r31 bug_020): `wants_metal` derives the
    /// metal nodeSelector / toleration gate from the SAME
    /// `provides_features` map the scheduler routes against. Pre-fix
    /// the gate open-coded `f == "kvm"` — a Pool with
    /// `features: ["nixos-test"]` (no `"kvm"`) routed to metal via
    /// `features_compatible(["nixos-test"], ["kvm","nixos-test"])`
    /// but had no kvm taint toleration → permanently Pending pods.
    #[test]
    fn wants_metal_keys_on_kvm_tainted_provides_features() {
        use rio_proto::types::{HwClassLabels, NodeTaint};
        let kvm_taint = || NodeTaint {
            key: KVM_NODE_LABEL.into(),
            value: "true".into(),
            effect: "NoSchedule".into(),
        };
        let hw = HwClassConfig::default();
        hw.set(
            [
                (
                    "metal-x86".into(),
                    HwClassLabels {
                        taints: vec![kvm_taint()],
                        provides_features: s(&["kvm", "nixos-test"]),
                        ..Default::default()
                    },
                ),
                (
                    "mid-ebs-x86".into(),
                    HwClassLabels {
                        // No taint, no provides_features — can't
                        // contribute to the routable set.
                        ..Default::default()
                    },
                ),
            ]
            .into(),
            (192, 1536 << 30),
        );

        let mut p = crate::fixtures::test_pool("nt", ExecutorKind::Builder);

        // Pre-fix bug_020: this is the case that broke. Pool with only
        // `nixos-test` (no `kvm`) routes to metal but the literal
        // gate returned false → no toleration → permanently Pending.
        p.spec.features = s(&["nixos-test"]);
        assert!(
            wants_metal(&p, &hw),
            "Pool features routing to a kvm-tainted hwClass via \
             provides_features must get the metal toleration"
        );

        // Literal `kvm` still works (and is the cold-start floor).
        p.spec.features = s(&["kvm"]);
        assert!(wants_metal(&p, &hw));

        // Feature that routes to NO tainted class → no metal.
        p.spec.features = s(&["big-parallel"]);
        assert!(!wants_metal(&p, &hw));

        // Fetcher never wants metal regardless of features.
        let mut f = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
        f.spec.features = s(&["kvm"]);
        assert!(!wants_metal(&f, &hw));
    }

    /// `wants_metal` falls back to the literal `"kvm"` check under an
    /// empty (not-yet-loaded) `HwClassConfig`. Fail-OPEN: degrading
    /// `features:["kvm"]` Pools to no-metal-toleration on cold-start
    /// (the entire load-failure window) would be strictly worse than
    /// the bug — the pre-fix literal gate worked for `["kvm"]` always.
    #[test]
    fn wants_metal_falls_back_to_literal_kvm_when_config_unloaded() {
        let hw = HwClassConfig::default();
        let mut p = crate::fixtures::test_pool("k", ExecutorKind::Builder);
        p.spec.features = s(&["kvm"]);
        assert!(
            wants_metal(&p, &hw),
            "literal kvm floor survives empty HwClassConfig"
        );
        // The *extension* (nixos-test → metal) only works once
        // hw_config is loaded — that's the degraded-but-not-regressed
        // contract.
        p.spec.features = s(&["nixos-test"]);
        assert!(!wants_metal(&p, &hw));
    }

    /// §Permissive-restrictive asymmetry (r33 bug_002): when a feature
    /// is shared by metal AND non-metal hwClasses, the per-intent
    /// affinity may legitimately route to the non-metal cell.
    /// A pool-static `nodeSelector{rio.build/kvm}` would CONTRADICT
    /// that affinity → permanent Pending + `reap_idle` mint→reap loop.
    /// Restrictive constraints must derive from `intent.hw_class_names`
    /// (the same source `cover` reads), never from `pool.spec.features`.
    // r[verify ctrl.pool.kvm-device+2]
    #[test]
    fn wants_metal_does_not_force_node_selector_on_shared_feature() {
        use rio_proto::types::{HwClassLabels, NodeTaint};
        let kvm_taint = || NodeTaint {
            key: KVM_NODE_LABEL.into(),
            value: "true".into(),
            effect: "NoSchedule".into(),
        };
        let hw = HwClassConfig::default();
        hw.set(
            [
                (
                    "metal-x86".into(),
                    HwClassLabels {
                        taints: vec![kvm_taint()],
                        provides_features: s(&["kvm", "nixos-test"]),
                        ..Default::default()
                    },
                ),
                (
                    // The shared-feature case: a NON-metal class also
                    // provides nixos-test. The scheduler may route an
                    // intent here → cover mints a non-metal node.
                    "mid-ebs-x86".into(),
                    HwClassLabels {
                        provides_features: s(&["nixos-test"]),
                        ..Default::default()
                    },
                ),
            ]
            .into(),
            (192, 1536 << 30),
        );

        let mut p = crate::fixtures::test_pool("nt", ExecutorKind::Builder);
        p.spec.features = s(&["nixos-test"]);
        assert!(wants_metal(&p, &hw), "predicate fires (permissive arm)");

        let spec = build_executor_pod_spec(
            &p,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            &hw,
        );
        // Structural invariant: no pool-static restrictive nodeSelector.
        // The per-intent affinity is the ONLY restrictive mechanism.
        assert!(
            spec.node_selector
                .as_ref()
                .is_none_or(|ns| !ns.contains_key(KVM_NODE_LABEL)),
            "pool-static path must NOT force a kvm nodeSelector — it \
             contradicts the per-intent affinity on shared features"
        );
        // The permissive arm (toleration) still fires.
        assert!(
            spec.tolerations
                .as_ref()
                .is_some_and(|t| t.iter().any(|t| t.key.as_deref() == Some(KVM_NODE_LABEL))),
            "pool-static toleration still appended (permissive, safe)"
        );
    }

    /// §Partition-single-source (r33 bug_011): the pool-static
    /// toleration set derives from `taints_routing_to(KVM_NODE_LABEL)`
    /// — the same `[sla.hw_classes.$h].taints` map `cover` reads — so
    /// a metal class growing a second taint (e.g. `rio.build/secure-boot`)
    /// gets its toleration automatically. Pre-fix the pool-static arm
    /// hardcoded `kvm=true:NoSchedule`, leaving the cold-start
    /// `hw_class_names=[]` pod unable to tolerate the second taint →
    /// permanently Pending.
    // r[verify ctrl.pool.kvm-device+2]
    #[test]
    fn wants_metal_toleration_derives_from_taints_routing_to() {
        use rio_proto::types::{HwClassLabels, NodeTaint};
        let hw = HwClassConfig::default();
        hw.set(
            [(
                "metal-x86".into(),
                HwClassLabels {
                    taints: vec![
                        NodeTaint {
                            key: KVM_NODE_LABEL.into(),
                            value: "true".into(),
                            effect: "NoSchedule".into(),
                        },
                        NodeTaint {
                            key: "rio.build/secure-boot".into(),
                            value: "true".into(),
                            effect: "NoSchedule".into(),
                        },
                    ],
                    provides_features: s(&["kvm"]),
                    ..Default::default()
                },
            )]
            .into(),
            (192, 1536 << 30),
        );

        let mut p = crate::fixtures::test_pool("k", ExecutorKind::Builder);
        p.spec.features = s(&["kvm"]);
        let spec = build_executor_pod_spec(
            &p,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            &hw,
        );
        let tols = spec.tolerations.as_ref().expect("tolerations set");
        let has = |k: &str| tols.iter().any(|t| t.key.as_deref() == Some(k));
        assert!(has(KVM_NODE_LABEL), "kvm toleration");
        assert!(
            has("rio.build/secure-boot"),
            "second taint's toleration derived from taints_routing_to"
        );
    }

    /// §13e B4: the per-intent nodeAffinity from `cells_to_selector_terms`
    /// composes with the pool-static `nodeSelector{rio.build/fetcher: true}`
    /// (restored in B4 after the B2.3 deletion). The OLD convention's key
    /// `rio.build/node-role` MUST stay absent — only the §13e taint-key
    /// (`FETCHER_TAINT_KEY`) is wired to NodeClaim labels via
    /// `cover::build_nodeclaim`; a `node-role` selector would never match
    /// any node.
    // r[verify ctrl.pool.fetcher-affinity-from-intent+5]
    // r[verify fetcher.node.dedicated+4]
    #[test]
    fn fetcher_pod_no_legacy_node_role_selector() {
        use rio_proto::types::{HwClassLabels, NodeLabelMatch, NodeTaint};
        let hw = HwClassConfig::default();
        hw.set(
            [(
                "fetcher-x86".into(),
                HwClassLabels {
                    labels: vec![NodeLabelMatch {
                        key: rio_common::k8s::FETCHER_TAINT_KEY.into(),
                        value: "true".into(),
                    }],
                    taints: vec![NodeTaint {
                        key: rio_common::k8s::FETCHER_TAINT_KEY.into(),
                        value: "true".into(),
                        effect: "NoSchedule".into(),
                    }],
                    provides_features: s(&[rio_common::k8s::FETCHER_FEATURE]),
                    ..Default::default()
                },
            )]
            .into(),
            (192, 1536 << 30),
        );
        let p = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
        let spec = build_executor_pod_spec(
            &p,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            &hw,
        );
        // Structural invariant: the deleted `rio.build/node-role`
        // convention does not reappear. Pool-static placement uses
        // the §13e taint-key only.
        assert!(
            spec.node_selector
                .as_ref()
                .is_none_or(|ns| !ns.contains_key("rio.build/node-role")),
            "deleted rio.build/node-role convention must not reappear in the \
             fetcher nodeSelector (§13e); got {:?}",
            spec.node_selector
        );
        // The permissive arm (toleration) still fires — derived from the
        // SAME taints_routing_to map cover reads.
        assert!(
            spec.tolerations.as_ref().is_some_and(|t| t
                .iter()
                .any(|t| t.key.as_deref() == Some(rio_common::k8s::FETCHER_TAINT_KEY))),
            "pool-static fetcher toleration still appended (permissive, safe)"
        );
    }

    /// §13e B4: a `system="builtin"` FOD (e.g., `builtins.fetchurl`) has
    /// `hw_class_names=[]` — `system_to_k8s_arch("builtin")` returns
    /// `None`, `reference_hw_class_for_system` early-returns, `bypass_cells`
    /// falls into `[]`, so the intent ships with no per-intent
    /// `nodeAffinity`. The pool-static `nodeSelector{rio.build/fetcher:
    /// true}` is the SOLE restrictive placement for this case. Without
    /// it, a builtin FOD pod has only the toleration and can schedule
    /// onto any untainted node (e.g., `rio-general` control-plane) —
    /// defense-in-depth weakening (CNP holds) + control-plane resource
    /// contention.
    ///
    /// The pod is built with no intent (cold-start / builtin FOD path) —
    /// `build_executor_pod_spec` does not see a per-intent affinity, so
    /// the only restrictive placement it CAN stamp is the pool-static
    /// nodeSelector.
    // r[verify ctrl.pool.fetcher-affinity-from-intent+5]
    // r[verify fetcher.node.dedicated+4]
    #[test]
    fn builtin_fod_pod_has_pool_static_fetcher_node_selector() {
        // Default (unloaded) hwClass config — the cold-start path:
        // there is no fetcher hwClass to derive an affinity from.
        let hw = HwClassConfig::default();
        let p = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
        let spec = build_executor_pod_spec(
            &p,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            &hw,
        );
        let ns = spec
            .node_selector
            .expect("fetcher pod must have a nodeSelector");
        assert_eq!(
            ns.get(rio_common::k8s::FETCHER_TAINT_KEY)
                .map(String::as_str),
            Some("true"),
            "fetcher pod must have pool-static rio.build/fetcher nodeSelector — \
             the per-intent affinity is empty for system=builtin FODs"
        );
        // The deleted node-role convention must not have come back along.
        assert!(
            !ns.contains_key("rio.build/node-role"),
            "deleted rio.build/node-role convention must not reappear: {ns:?}"
        );
    }

    /// §13e B4 + r35 bug_044: an operator-supplied
    /// `pool.spec.node_selector` MERGES with the pool-static fetcher
    /// constraint. The operator's keys survive AND `rio.build/fetcher`
    /// is unconditionally present. The pre-r35 `or_else` shape let an
    /// operator AZ pin REPLACE the dedicated-node constraint —
    /// §Permissive-restrictive asymmetry: this constraint is
    /// restrictive and load-bearing for `system="builtin"` FODs whose
    /// `hw_class_names=[]` carries no per-intent affinity; dropping it
    /// removes the lateral-movement boundary.
    // r[verify ctrl.pool.fetcher-affinity-from-intent+5]
    // r[verify fetcher.node.dedicated+4]
    #[test]
    fn fetcher_pod_spec_node_selector_merges_with_pool_static_default() {
        let hw = HwClassConfig::default();
        let mut p = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
        p.spec.node_selector = Some(BTreeMap::from([(
            "topology.kubernetes.io/zone".into(),
            "us-east-2a".into(),
        )]));
        let spec = build_executor_pod_spec(
            &p,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            &hw,
        );
        let ns = spec
            .node_selector
            .expect("operator-supplied nodeSelector must survive");
        assert_eq!(
            ns.get("topology.kubernetes.io/zone").map(String::as_str),
            Some("us-east-2a"),
            "operator-supplied AZ pin survives the merge"
        );
        assert_eq!(
            ns.get(rio_common::k8s::FETCHER_TAINT_KEY)
                .map(String::as_str),
            Some("true"),
            "operator selector ADDS to the pool-static fetcher \
             constraint, never replaces — the dedicated-node boundary \
             is unconditional: {ns:?}"
        );
    }

    /// r35 bug_044: the pool-static fetcher constraint cannot be
    /// WEAKENED by the operator. If the operator sets
    /// `nodeSelector{rio.build/fetcher: "false"}`, `effective_node_selector`
    /// overrides it to `"true"` (UNCONDITIONAL `insert`, not
    /// `or_insert_with` — `or_insert_with` would preserve the
    /// operator's `"false"` and the pod would escape the dedicated
    /// taint). The CEL guard rejects this misconfig at admission for
    /// new specs; this is the controller-side belt-and-suspenders for
    /// pre-CEL specs the apiserver already accepted.
    // r[verify fetcher.node.dedicated+4]
    #[test]
    fn fetcher_pod_spec_node_selector_cannot_weaken_pool_static_default() {
        let hw = HwClassConfig::default();
        let mut p = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
        p.spec.node_selector = Some(BTreeMap::from([(
            rio_common::k8s::FETCHER_TAINT_KEY.into(),
            "false".into(),
        )]));
        let spec = build_executor_pod_spec(
            &p,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            &hw,
        );
        let ns = spec
            .node_selector
            .expect("fetcher pod must have a nodeSelector");
        assert_eq!(
            ns.get(rio_common::k8s::FETCHER_TAINT_KEY)
                .map(String::as_str),
            Some("true"),
            "operator-set rio.build/fetcher=false must be overridden — \
             the pool-static constraint is universal, not weakenable: {ns:?}"
        );
    }

    /// r37 bug_001 (§Permissive-restrictive asymmetry, sibling of r35
    /// bug_044): a Fetcher Pool with operator-set tolerations MUST keep
    /// the auto-injected fetcher taint toleration. `effective_node_selector`
    /// (bug_044 fix) unconditionally pins the pod to `rio.build/fetcher:
    /// true` nodes — every one of which carries `rio.build/fetcher:
    /// NoSchedule`. A pod with the nodeSelector but not the toleration is
    /// permanently Pending and there is no warn/metric.
    ///
    /// This is the pair-coupling invariant: every nodeSelector key
    /// `effective_node_selector` ADDS (vs the spec) corresponds to a taint
    /// on the matching nodes, so the effective tolerations MUST include a
    /// toleration for that taint — for all spec values.
    // r[verify ctrl.pool.fetcher-tolerations]
    #[test]
    fn fetcher_pod_operator_tolerations_merge_not_replace() {
        let hw = HwClassConfig::default();
        let mut p = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
        let custom = Toleration {
            key: Some("custom.example/audit".into()),
            operator: Some("Exists".into()),
            effect: Some("NoSchedule".into()),
            ..Default::default()
        };
        p.spec.tolerations = Some(vec![custom.clone()]);
        let spec = build_executor_pod_spec(
            &p,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            &hw,
        );
        let tols = spec.tolerations.expect("fetcher pod must have tolerations");
        assert!(tols.contains(&custom), "operator toleration preserved");
        assert!(
            tols.iter()
                .any(|t| t.key.as_deref() == Some(rio_common::k8s::FETCHER_TAINT_KEY)),
            "fetcher taint toleration always present — pool-static \
             nodeSelector (bug_044) pins to tainted nodes; dropping the \
             toleration is permanent Pending: {tols:?}"
        );
        // Pair invariant: the nodeSelector key the controller adds requires
        // a corresponding toleration in the same pod spec.
        let ns = spec.node_selector.expect("fetcher pod has nodeSelector");
        assert!(
            ns.contains_key(rio_common::k8s::FETCHER_TAINT_KEY)
                && tols
                    .iter()
                    .any(|t| t.key.as_deref() == Some(rio_common::k8s::FETCHER_TAINT_KEY)),
            "every pool-static nodeSelector key with a matching taint must \
             have a toleration"
        );

        // `Some(vec![])` (operator explicitly sets `tolerations: []`) is
        // the most surprising trigger: the pre-fix `.or_else()` short-
        // circuits on ANY `Some(_)`, including the empty list — the
        // operator thinks they're clearing tolerations, but the result is
        // permanent Pending. The merge handles it the same as
        // `Some(vec![T])`: `unwrap_or_default()` yields `vec![]`, fetcher
        // tolerations are appended unconditionally.
        let mut p_empty = crate::fixtures::test_pool("f-empty", ExecutorKind::Fetcher);
        p_empty.spec.tolerations = Some(vec![]);
        let spec_empty = build_executor_pod_spec(
            &p_empty,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            &hw,
        );
        let tols_empty = spec_empty
            .tolerations
            .expect("fetcher pod must have tolerations even with explicit empty operator set");
        assert!(
            tols_empty
                .iter()
                .any(|t| t.key.as_deref() == Some(rio_common::k8s::FETCHER_TAINT_KEY)),
            "fetcher taint toleration present when operator sets `tolerations: []`: \
             {tols_empty:?}"
        );
    }

    /// r38 bug_027 (§Permissive-restrictive asymmetry — sibling of r37
    /// bug_001 for the builder arm): builder pod operator tolerations
    /// MERGE with the structural `rio.build/builder` toleration, not
    /// replace.
    ///
    /// This is the pair-coupling invariant for the builder arm: every
    /// cover-minted builder NodeClaim carries
    /// `rio.build/builder=true:NoSchedule` (`cover.rs::builder_taint()`),
    /// and the per-intent `nodeAffinity`
    /// (`r[ctrl.pool.node-affinity-from-intent]`) pins the pod to those
    /// nodes — so the effective tolerations MUST include the builder
    /// toleration for all spec values, or the pod is permanently
    /// Pending with no warn/metric.
    // r[verify ctrl.pool.builder-tolerations]
    #[test]
    fn builder_pod_operator_tolerations_merge_not_replace() {
        let hw = HwClassConfig::default();
        let mut p = crate::fixtures::test_pool("b", ExecutorKind::Builder);
        let custom = Toleration {
            key: Some("custom.example/audit".into()),
            operator: Some("Exists".into()),
            effect: Some("NoSchedule".into()),
            ..Default::default()
        };
        p.spec.tolerations = Some(vec![custom.clone()]);
        let tols = effective_tolerations(&p, &hw).expect("builder pod tolerations");
        assert!(tols.contains(&custom), "operator toleration preserved");
        assert!(
            tols.iter()
                .any(|t| t.key.as_deref() == Some(rio_common::k8s::BUILDER_TAINT_KEY)),
            "builder taint toleration always present — per-intent \
             nodeAffinity pins to cover-minted nodes carrying \
             rio.build/builder:NoSchedule; dropping the toleration is \
             permanent Pending: {tols:?}"
        );

        // `Some(vec![])` — operator explicitly clears tolerations.
        // The most surprising trigger: the pre-fix builder arm returned
        // `Some(vec![])` and the pod was permanently Pending.
        let mut p_empty = crate::fixtures::test_pool("b-empty", ExecutorKind::Builder);
        p_empty.spec.tolerations = Some(vec![]);
        let tols_empty = effective_tolerations(&p_empty, &hw).expect("builder pod tolerations");
        assert!(
            tols_empty
                .iter()
                .any(|t| t.key.as_deref() == Some(rio_common::k8s::BUILDER_TAINT_KEY)),
            "builder taint toleration present when operator sets `tolerations: []`"
        );
    }

    /// r35 bug_039: a Fetcher Pool with arch-typed `systems` (e.g.,
    /// `["x86_64-linux", "builtin"]` for `pkgs.fetchurl` FODs) gets the
    /// `kubernetes.io/arch` nodeSelector from `nix_systems_to_k8s_arch`,
    /// same as a Builder Pool. §13e dropped the helm-static fetcher
    /// arch nodeSelector AND `validate_host_arch` skipped Fetcher —
    /// both compensations gone meant an `x86-64-fetcher` worker could
    /// land on an arm64 fetcher node and CrashLoopBackOff forever
    /// (kubelet does not reschedule on container exit). A
    /// `["builtin"]`-only Pool stays arch-agnostic (no pin) —
    /// `nix_systems_to_k8s_arch` skips `builtin`.
    // r[verify ctrl.pod.arch-selector+2]
    // r[verify sched.dispatch.fod-builtin-any-arch+2]
    #[test]
    fn fetcher_pod_arch_selector_from_systems() {
        let hw = HwClassConfig::default();

        // Arch-typed Fetcher Pool: `systems=["x86_64-linux", "builtin"]`
        // resolves to a single host arch → arch pin.
        let mut p = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
        p.spec.systems = s(&["x86_64-linux", "builtin"]);
        let spec = build_executor_pod_spec(
            &p,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            &hw,
        );
        let ns = spec
            .node_selector
            .expect("arch-typed Fetcher pod must have a nodeSelector");
        assert_eq!(
            ns.get("kubernetes.io/arch").map(String::as_str),
            Some("amd64"),
            "Fetcher Pool with arch-typed systems must pin arch — \
             validate_host_arch failing at boot does not reschedule: {ns:?}"
        );
        // The pool-static fetcher constraint is still present (merge).
        assert_eq!(
            ns.get(rio_common::k8s::FETCHER_TAINT_KEY)
                .map(String::as_str),
            Some("true"),
        );

        // `["builtin"]`-only Fetcher Pool: arch-agnostic, no arch pin.
        let mut p = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
        p.spec.systems = s(&["builtin"]);
        let spec = build_executor_pod_spec(
            &p,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            &hw,
        );
        let ns = spec
            .node_selector
            .expect("builtin-only Fetcher pod still has the pool-static nodeSelector");
        assert!(
            !ns.contains_key("kubernetes.io/arch"),
            "builtin-only Fetcher Pool must stay arch-agnostic: {ns:?}"
        );
    }

    /// Invariant: with `HwClassConfig::default()` (config not loaded)
    /// and `features=["kvm"]`, the literal `kvm=true:NoSchedule`
    /// toleration floor survives. Fail-OPEN — same contract `wants_metal`
    /// keeps for the predicate.
    // r[verify ctrl.pool.kvm-device+2]
    #[test]
    fn wants_metal_toleration_floor_when_hw_unloaded() {
        let hw = HwClassConfig::default();
        let mut p = crate::fixtures::test_pool("k", ExecutorKind::Builder);
        p.spec.features = s(&["kvm"]);
        let spec = build_executor_pod_spec(
            &p,
            &crate::fixtures::test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
            &hw,
        );
        let tols = spec.tolerations.as_ref().expect("tolerations set");
        let kvm = tols
            .iter()
            .find(|t| t.key.as_deref() == Some(KVM_NODE_LABEL))
            .expect("literal kvm toleration floor under unloaded config");
        assert_eq!(kvm.value.as_deref(), Some("true"));
        assert_eq!(kvm.effect.as_deref(), Some("NoSchedule"));
    }
}
