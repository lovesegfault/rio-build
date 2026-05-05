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
    DownwardAPIVolumeSource, EmptyDirVolumeSource, EnvVar, EnvVarSource, HostPathVolumeSource,
    NodeSelectorRequirement, NodeSelectorTerm, ObjectFieldSelector, PodSecurityContext, PodSpec,
    SeccompProfile, SecurityContext, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::ResourceExt;

use rio_crds::pool::{ExecutorKind, Pool, PoolSpec, SeccompProfileKind};

use crate::reconcilers::node_informer::HwClassConfig;

/// Nix `system-features` string that signals "this builder runs
/// qemu-kvm". When present in `spec.features`, the pod gets the
/// [`KVM_NODE_LABEL`] nodeSelector + toleration so it lands on a §13b
/// metal NodeClaim. `/dev/kvm` itself is injected by containerd
/// `base_runtime_spec` on every node (`nix/base-runtime-spec.nix`);
/// it ENXIOs on open() on non-`.metal` hosts, so the nodeSelector is
/// what makes kvm builds work, not the device node's presence.
/// `wants_metal` keeps it as the cold-start floor; the union of
/// `provides_features` over kvm-tainted hw-classes (r31 bug_020)
/// extends it.
pub(super) const KVM_FEATURE: &str = "kvm";

/// Node label stamped on metal nodes (§13c: `scheduler.sla.hwClasses.
/// metal-*.labels`, via `cover::build_nodeclaim`). Pairs with the
/// same-key taint (`metal-*.taints`); `wants_metal` keys the
/// `provides_features` union on this taint key.
const KVM_NODE_LABEL: &str = "rio.build/kvm";

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

/// Default FUSE cache size for fetchers. FODs are typically small
/// (source tarballs, git clones). Safe-minimum CRD default; prod
/// inherits the helm `poolDefaults.fuseCacheBytes` 50Gi.
pub(super) const FETCHER_FUSE_CACHE_BYTES: u64 = 4 * (1 << 30);

/// Set once at boot from `[nodeclaim_pool].fuse_cache_bytes` so
/// `fuse_cache_bytes()` for Builder pools, `intent_pod_footprint`'s
/// callers in `nodeclaim_pool` (FFD,
/// `cover_deficit`), and `apply_intent_resources` all read the SAME
/// value (§Simulator-shares-accounting). A per-Pool override would
/// make FFD predict a different ephemeral-storage footprint than the
/// pod actually stamps.
pub static BUILDER_FUSE_CACHE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Per-pool FUSE cache budget. Drives BOTH the `fuse-cache` emptyDir
/// sizeLimit and the `ephemeral-storage` budget addend so they cannot
/// drift. Builder pools single-source from [`BUILDER_FUSE_CACHE`]
/// (= `[nodeclaim_pool].fuse_cache_bytes`) so FFD/cover/stamp agree;
/// `PoolSpec.fuse_cache_bytes` is CEL-rejected for Builder kind, and
/// pre-CEL CRs are ignored here with a Warning event
/// (`DEGRADE_CHECKS::BuilderFuseCacheBytesIgnored`). Fetcher pools may
/// override per-pool — Fetcher doesn't go through `nodeclaim_pool`.
pub(super) fn fuse_cache_bytes(pool: &Pool) -> u64 {
    match pool.spec.kind {
        ExecutorKind::Builder => *BUILDER_FUSE_CACHE
            .get()
            .unwrap_or(&BUILDER_FUSE_CACHE_BYTES),
        ExecutorKind::Fetcher => pool
            .spec
            .fuse_cache_bytes
            .unwrap_or(FETCHER_FUSE_CACHE_BYTES),
    }
}

/// Upstream gRPC addresses injected into executor pod env: a
/// ClusterIP `addr` for single-channel mode plus an optional
/// headless-Service `balance_host` for health-aware p2c. Same shape
/// for scheduler and store; the env-var prefix differs at the
/// injection site below. Shared with each binary's `Config`
/// (figment-deserialized) — see [`rio_common::config::UpstreamAddrs`].
pub use rio_common::config::UpstreamAddrs;

// ── per-kind effective values ────────────────────────────────────
//
// The builder/fetcher diff is small: fetchers force ADR-019 hardening
// (read-only rootfs, never privileged, Localhost seccomp, dedicated
// node placement, hostUsers:false default, no features) regardless of
// spec. Each helper below returns the effective value for one
// pod-spec field. Builder reads spec verbatim; Fetcher overrides.

#[inline]
fn is_fetcher(pool: &Pool) -> bool {
    pool.spec.kind == ExecutorKind::Fetcher
}

/// FODs route by `is_fixed_output` alone, not features — fetchers
/// always advertise empty. Single chokepoint so the spawn-decision
/// query (`jobs::queued_for_pool`) and the spawned worker's
/// `RIO_FEATURES` cannot diverge: a non-empty `features` on a Fetcher
/// would otherwise hit the I-181 ∅-guard at scheduler
/// `snapshot.rs:221` and filter out every featureless FOD → fetcher
/// pool never spawns → all fetches stall silently.
// r[impl ctrl.crd.fetcher-no-features]
#[inline]
pub(super) fn effective_features(spec: &PoolSpec) -> Vec<String> {
    if spec.kind == ExecutorKind::Fetcher {
        Vec::new()
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

/// ADR-019 §Node isolation: fetchers land on dedicated nodes via the
/// `rio.build/fetcher=true:NoSchedule` taint + matching selector. If
/// the operator supplies their own, honor those instead — lets them
/// override for dev clusters without dedicated node pools.
// r[impl fetcher.node.dedicated]
fn effective_node_selector(pool: &Pool) -> Option<BTreeMap<String, String>> {
    if is_fetcher(pool) {
        pool.spec.node_selector.clone().or_else(|| {
            Some(BTreeMap::from([(
                "rio.build/node-role".into(),
                "fetcher".into(),
            )]))
        })
    } else {
        pool.spec.node_selector.clone()
    }
}

fn effective_tolerations(pool: &Pool) -> Option<Vec<Toleration>> {
    if is_fetcher(pool) {
        pool.spec.tolerations.clone().or_else(|| {
            Some(vec![Toleration {
                key: Some("rio.build/fetcher".into()),
                operator: Some("Exists".into()),
                effect: Some("NoSchedule".into()),
                ..Default::default()
            }])
        })
    } else {
        pool.spec.tolerations.clone()
    }
}

/// Pool advertises a feature that routes drvs to a metal hw-class →
/// pod needs the metal nodeSelector + toleration. FODs route by
/// `is_fixed_output` alone, never by features, so fetchers never want
/// metal. See `r[ctrl.pool.kvm-device]`.
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
/// (otherwise every kvm Pool's pod would lose the metal nodeSelector
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
// r[impl ctrl.pod.arch-selector]
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
pub fn job_name(pool_name: &str, role: ExecutorKind, suffix: &str) -> String {
    format!("{NAME_PREFIX}-{}-{pool_name}-{suffix}", role.as_str())
}

/// The Job pod spec — shared by both pool kinds.
// r[impl ctrl.pool.fetcher-hardening]
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
        // r[impl ctrl.pod.tgps-default]
        // Fetchers: 10min — fetches are short. Builders: spec or 2h.
        termination_grace_period_seconds: Some(
            pool.spec
                .termination_grace_period_seconds
                .unwrap_or(if fetcher { 600 } else { 7200 }),
        ),
        node_selector: {
            let mut ns = effective_node_selector(pool).unwrap_or_default();
            // r[impl ctrl.pod.arch-selector]
            // I-098: a pool with systems=[x86_64-linux] landed pods on an
            // arm64 node (fallback NodePool unconstrained) — builder
            // registers as x86_64 from RIO_SYSTEMS, scheduler dispatches
            // x86_64 drvs, nix-daemon refuses (host is aarch64). Derive
            // kubernetes.io/arch from systems so karpenter constrains arch.
            // Builder-only: fetchers run `builtin` (arch-agnostic) and
            // benefit from cheaper Graviton; rio-builder's startup arch
            // check is the safety net for builders that slip through.
            if !fetcher && let Some(arch) = nix_systems_to_k8s_arch(&pool.spec.systems) {
                ns.entry("kubernetes.io/arch".into()).or_insert(arch.into());
            }
            // r[impl ctrl.pool.kvm-device]
            // features route to a kvm-tainted hw-class → land on the
            // metal NodePool. Operator-set value wins (or_insert) so a
            // non-Karpenter cluster with a different kvm-capable label
            // can override. Unconditional wrt privileged: even a
            // privileged pod needs a host that actually has /dev/kvm.
            if wants_metal(pool, hw_config) {
                ns.entry(KVM_NODE_LABEL.into()).or_insert("true".into());
            }
            if ns.is_empty() { None } else { Some(ns) }
        },
        tolerations: {
            let mut t = effective_tolerations(pool).unwrap_or_default();
            // r[impl ctrl.pool.kvm-device]
            // Metal NodePool is tainted rio.build/kvm=true:NoSchedule so
            // non-kvm builders don't bin-pack onto $$ metal. Append
            // (don't replace) — the operator-set rio.build/builder
            // toleration must survive.
            if wants_metal(pool, hw_config) {
                t.push(Toleration {
                    key: Some(KVM_NODE_LABEL.into()),
                    operator: Some("Equal".into()),
                    value: Some("true".into()),
                    effect: Some("NoSchedule".into()),
                    ..Default::default()
                });
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
                // Single source: `effective_features` (Fetcher → []).
                env("RIO_FEATURES", &effective_features(&pool.spec).join(",")),
                // Executor self-identification. figment reads
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
            ];
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
        // request. kvm placement is the nodeSelector + toleration above
        // (r[ctrl.pool.kvm-device]). resources are stamped per-intent
        // by `jobs::apply_intent_resources` AFTER this builder runs.
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

        // No liveness/readiness/startup probes: executor pods are
        // one-shot Jobs. activeDeadlineSeconds bounds hangs; nothing
        // routes via Service (no readiness gate); and a CPU-pegged
        // build would fail a 1s-timeout liveness probe → kubelet
        // SIGKILL mid-build → FUSE/overlay torn down → nix-daemon EIO
        // (I-114).
        ..Default::default()
    }
}

// ── small helpers ────────────────────────────────────────────────────

/// Translate CRD `SeccompProfileKind` → k8s-openapi `SeccompProfile`.
/// `None` and unknown types → `RuntimeDefault` (fail-closed — never
/// fall through to Unconfined on a typo).
// r[impl builder.seccomp.localhost-profile+2]
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
        // r[verify ctrl.pod.arch-selector]
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

    // r[verify ctrl.pool.fetcher-hardening]
    // r[verify ctrl.crd.fetcher-no-features]
    /// `effective_features` is the single chokepoint: Fetcher → []
    /// regardless of spec; Builder → verbatim. Both `RIO_FEATURES`
    /// (worker capabilities) and `queued_for_pool` (spawn-decision
    /// query) read it, so they cannot diverge. Before the chokepoint,
    /// `poolDefaults.features:["kvm"]` deep-merged onto Fetcher entries
    /// → I-181 ∅-guard filtered every featureless FOD → fetcher pool
    /// never spawned.
    #[test]
    fn effective_features_empty_for_fetcher() {
        let mut fetcher = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
        fetcher.spec.features = s(&["kvm", "big-parallel"]);
        assert!(
            effective_features(&fetcher.spec).is_empty(),
            "Fetcher: features forced empty regardless of spec"
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
            Some(""),
            "RIO_FEATURES empty for Fetcher (same chokepoint)"
        );
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
    #[allow(clippy::result_large_err)] // figment::Error is 208B, API-fixed
    fn coverage_profraw_path_per_pod_unique() {
        // figment Jail serializes env access across parallel tests
        // (same pattern as the lease-config tests).
        figment::Jail::expect_with(|jail| {
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
}
