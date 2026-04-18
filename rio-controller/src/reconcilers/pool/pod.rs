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
    Capabilities, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource, EnvVar,
    EnvVarSource, HostPathVolumeSource, ObjectFieldSelector, PodSecurityContext, PodSpec,
    SeccompProfile, SecurityContext, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::ResourceExt;

use rio_crds::pool::{ExecutorKind, Pool, SeccompProfileKind};

/// Nix `system-features` string that signals "this builder runs
/// qemu-kvm". When present in `spec.features`, the pod gets the
/// [`KVM_NODE_LABEL`] nodeSelector + toleration so it lands on the
/// metal NodePool. `/dev/kvm` itself is injected by containerd
/// `base_runtime_spec` on every node (`nix/base-runtime-spec.nix`);
/// it ENXIOs on open() on non-`.metal` hosts, so the nodeSelector is
/// what makes kvm builds work, not the device node's presence.
const KVM_FEATURE: &str = "kvm";

/// Node label set on the metal NodePool (values.yaml `karpenter.
/// nodePools[rio-builder-metal].labels`). Pairs with the same-key
/// taint.
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
/// CRD-side default is the SAFE MINIMUM (fits ~21Gi-allocatable k3s
/// nodes) so a Pool created via raw `kubectl apply` without
/// `spec.fuseCacheBytes` schedules anywhere. Prod gets 50Gi via the
/// helm chart's `poolDefaults.fuseCacheBytes`.
pub(super) const BUILDER_FUSE_CACHE_BYTES: u64 = 8 * (1 << 30);

/// Default FUSE cache size for fetchers. FODs are typically small
/// (source tarballs, git clones). Safe-minimum CRD default; prod
/// inherits the helm `poolDefaults.fuseCacheBytes` 50Gi.
pub(super) const FETCHER_FUSE_CACHE_BYTES: u64 = 4 * (1 << 30);

/// Per-pool FUSE cache budget. Drives BOTH the `fuse-cache` emptyDir
/// sizeLimit and the `ephemeral-storage` budget addend so they cannot
/// drift. `PoolSpec.fuse_cache_bytes` overrides the per-kind default;
/// helm-rendered Pools always set it (50Gi prod), so the consts above
/// only apply to non-helm Pools (vm-netpol misplaced-builder, etc.).
pub(super) fn fuse_cache_bytes(pool: &Pool) -> u64 {
    pool.spec.fuse_cache_bytes.unwrap_or(match pool.spec.kind {
        ExecutorKind::Builder => BUILDER_FUSE_CACHE_BYTES,
        ExecutorKind::Fetcher => FETCHER_FUSE_CACHE_BYTES,
    })
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

/// Pool advertises the `kvm` Nix system-feature → pod needs a host
/// with working `/dev/kvm` (metal nodeSelector + toleration). FODs
/// route by `is_fixed_output` alone, never by features, so fetchers
/// never want kvm. See `r[ctrl.pool.kvm-device]`.
#[inline]
fn wants_kvm(pool: &Pool) -> bool {
    !is_fetcher(pool) && pool.spec.features.iter().any(|f| f == KVM_FEATURE)
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
                // unused, AND `quota::peak_bytes()` (XFS prjquota)
                // returned None on tmpfs so `peak_disk_bytes` never
                // fitted (bug_074).
                Volume {
                    name: "overlays".into(),
                    empty_dir: Some(EmptyDirVolumeSource::default()),
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
            // features:[kvm] → land on the metal NodePool. Operator-set
            // value wins (or_insert) so a non-Karpenter cluster with a
            // different kvm-capable label can override. Unconditional
            // wrt privileged: even a privileged pod needs a host that
            // actually has /dev/kvm.
            if wants_kvm(pool) {
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
            if wants_kvm(pool) {
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
                // FODs route by is_fixed_output alone, not features —
                // fetchers always advertise empty.
                env(
                    "RIO_FEATURES",
                    &if fetcher {
                        String::new()
                    } else {
                        pool.spec.features.join(",")
                    },
                ),
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
                // ADR-023 phase-10 hw self-calibration. Controller's
                // node_informer pod-watcher stamps `rio.build/hw-class`
                // once `spec.nodeName` is set; builder reads it via
                // downward-API to key the hw_perf_samples insert.
                // Unset → kubelet resolves to "" → bench skipped.
                env_from_field("RIO_HW_CLASS", "metadata.annotations['rio.build/hw-class']"),
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
            // knob respectively).
            if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
                e.push(env(
                    "LLVM_PROFILE_FILE",
                    "/var/lib/rio/cov/rio-%p-%m.profraw",
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
}
