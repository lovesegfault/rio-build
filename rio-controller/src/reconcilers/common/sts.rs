//! Shared StatefulSet/PodSpec builder for executor pods (builders +
//! fetchers).
//!
//! Extracted from `builderpool/builders.rs` when ADR-019 introduced
//! the builder/fetcher split. The 600-line pod-spec — FUSE volumes,
//! wait-seccomp initContainer, pod-level seccomp, TLS mounts,
//! capability set, probes, coverage propagation — is role-agnostic.
//! Parameterizing on [`ExecutorStsParams`] (instead of `&BuilderPool`)
//! lets both reconcilers call the same builder.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    Affinity, Capabilities, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource,
    EnvVar, EnvVarSource, HTTPGetAction, HostPathVolumeSource, ObjectFieldSelector,
    PodAffinityTerm, PodAntiAffinity, PodSecurityContext, PodSpec, PodTemplateSpec, Probe,
    ResourceRequirements, SeccompProfile, SecretVolumeSource, SecurityContext, Toleration,
    TopologySpreadConstraint, Volume, VolumeMount, WeightedPodAffinityTerm,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::ObjectMeta;

use crate::crds::builderpool::SeccompProfileKind;
use crate::error::{Error, Result};

/// K8s extended-resource name exposed by the smarter-device-manager
/// DaemonSet (infra/helm/rio-build/templates/device-plugin.yaml).
/// The kubelet sees this in `resources.limits` and the device plugin
/// injects `/dev/fuse` into the container's device cgroup allowlist —
/// no hostPath volume needed, so `hostUsers: false` works (ADR-012).
const FUSE_DEVICE_RESOURCE: &str = "smarter-devices/fuse";

/// Pod label carrying the executor role. Scheduler routing, network
/// policies, and `kubectl get pods -l rio.build/role=fetcher` all
/// key on this.
pub const ROLE_LABEL: &str = "rio.build/role";

/// Pod label carrying the owning pool name. Finalizer cleanup lists
/// pods by this; ephemeral mode counts Jobs by it. Shared between
/// BuilderPool and FetcherPool reconcilers.
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

/// Executor role. Determines the `rio.build/role` label value and
/// gates role-specific pod-spec tweaks (readOnlyRootFilesystem,
/// seccomp profile name, RIO_EXECUTOR_KIND env).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExecutorRole {
    /// Airgapped, arbitrary-code builds. `rio-builders` namespace.
    Builder,
    /// Internet-facing, hash-checked FOD fetches. `rio-fetchers`
    /// namespace. Stricter seccomp + readOnlyRootFilesystem.
    Fetcher,
}

impl ExecutorRole {
    /// Value for the `rio.build/role` label and `RIO_EXECUTOR_KIND` env.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Builder => "builder",
            Self::Fetcher => "fetcher",
        }
    }
}

/// Upstream gRPC addresses injected into executor pod env: a
/// ClusterIP `addr` for single-channel mode plus an optional
/// headless-Service `balance_host` for health-aware p2c. Same shape
/// for scheduler and store; the env-var prefix differs at the
/// injection site below.
///
/// Type aliases (rather than two distinct structs) keep the
/// `SchedulerAddrs` / `StoreAddrs` names at every call site without
/// duplicating the field set.
#[derive(Clone)]
pub struct UpstreamAddrs {
    /// ClusterIP Service `host:port` (`RIO_{SCHEDULER,STORE}_ADDR`).
    pub addr: String,
    /// Headless Service hostname. `None` = env var NOT injected →
    /// executor falls back to single-channel.
    pub balance_host: Option<String>,
    pub balance_port: u16,
}

pub type SchedulerAddrs = UpstreamAddrs;
pub type StoreAddrs = UpstreamAddrs;

/// Every spec field the pod-spec builder reads. Both reconcilers
/// construct this from their respective CRD spec and call
/// [`build_executor_statefulset`].
///
/// Optional fields with `None` get the builder's compiled-in
/// default. The fetcher reconciler leaves most `None` — its spec
/// is minimal (replicas, image, systems, nodeSelector, tolerations,
/// resources).
pub struct ExecutorStsParams {
    // ── role-varying knobs (the ADR-019 diff) ────────────────────
    /// Builder or Fetcher. Sets the `rio.build/role` label and
    /// `RIO_EXECUTOR_KIND` env.
    pub role: ExecutorRole,
    /// `securityContext.readOnlyRootFilesystem` on the executor
    /// container. Fetchers: `true` (overlay upperdir is a tmpfs
    /// emptyDir; rootfs tampering blocked). Builders: `false`.
    // TODO(P0455): add the fetcher.sandbox.strict-seccomp impl
    // marker here once ADR-019 is in tracey spec_include.
    pub read_only_root_fs: bool,
    /// Extra env vars appended after the base set. Builders pass
    /// their tuning knobs (bloom, daemon_timeout, fuse_passthrough,
    /// size_class) here so the shared builder doesn't need fields
    /// for every BuilderPool-only knob.
    pub extra_env: Vec<EnvVar>,

    // ── metadata ─────────────────────────────────────────────────
    /// Pool CR name. STS name becomes `{pool_name}-{role}s`.
    pub pool_name: String,
    /// Pool CR namespace. STS + pods live here.
    pub namespace: String,

    // ── scheduling ───────────────────────────────────────────────
    pub node_selector: Option<BTreeMap<String, String>>,
    pub tolerations: Option<Vec<Toleration>>,
    /// Soft topology spread on hostname. `None` → true.
    pub topology_spread: Option<bool>,

    // ── image ────────────────────────────────────────────────────
    pub image: String,
    pub image_pull_policy: Option<String>,

    // ── capacity ─────────────────────────────────────────────────
    /// Comma-joined → `RIO_SYSTEMS`.
    pub systems: Vec<String>,
    /// Comma-joined → `RIO_FEATURES`. Empty for fetchers.
    pub features: Vec<String>,
    pub resources: Option<ResourceRequirements>,

    // ── FUSE ─────────────────────────────────────────────────────
    /// Parsed GB for `RIO_FUSE_CACHE_SIZE_GB`.
    pub fuse_cache_gb: u64,
    /// Raw Quantity for the emptyDir sizeLimit.
    pub fuse_cache_quantity: Quantity,
    pub fuse_threads: Option<u32>,

    // ── security ─────────────────────────────────────────────────
    /// `true` = full privileged (escape hatch — k3s/kind without
    /// the device plugin). Disables seccomp, hostUsers:false.
    pub privileged: bool,
    /// CRD seccomp profile. Converted via [`build_seccomp_profile`].
    pub seccomp_profile: Option<SeccompProfileKind>,
    /// Localhost seccomp profile is on disk before any pod schedules
    /// (P0541: Bottlerocket bootstrap container, EC2NodeClass userData
    /// `essential=true`). When `true` and the profile type is
    /// `Localhost`, skip the wait-seccomp initContainer + host-seccomp
    /// hostPath volumes — the file is guaranteed present, the wait is
    /// dead weight (5-15s × thousands of ephemeral Jobs/hour). The
    /// Localhost ENFORCEMENT (executor container's securityContext)
    /// stays in place; only the WAIT machinery is elided. See
    /// [`seccomp_preinstalled`].
    pub seccomp_preinstalled: bool,
    pub host_network: Option<bool>,
    /// Explicit override. `None` → derived from `!privileged &&
    /// !host_network`.
    pub host_users: Option<bool>,

    // ── misc ─────────────────────────────────────────────────────
    pub tls_secret_name: Option<String>,
    /// `None` → 7200s (2h).
    pub termination_grace_period_seconds: Option<i64>,
}

/// Labels for the StatefulSet, headless Service, pod template, and
/// PDB selector. Includes the ADR-019 `rio.build/role` label so
/// NetworkPolicies and `kubectl get pods -l rio.build/role=fetcher`
/// can target by role.
pub fn executor_labels(p: &ExecutorStsParams) -> BTreeMap<String, String> {
    BTreeMap::from([
        (POOL_LABEL.into(), p.pool_name.clone()),
        (ROLE_LABEL.into(), p.role.as_str().into()),
        ("app.kubernetes.io/name".into(), "rio-builder".into()),
        ("app.kubernetes.io/component".into(), p.role.as_str().into()),
        ("app.kubernetes.io/part-of".into(), "rio-build".into()),
    ])
}

/// Map a single-arch nix `systems` list to a `kubernetes.io/arch`
/// nodeSelector value. `None` when the list is empty, multi-arch, or
/// `builtin`-only — those pools deliberately float across arches and
/// rely on rio-builder's startup arch check (I-098 part B) instead.
pub(super) fn nix_systems_to_k8s_arch(systems: &[String]) -> Option<&'static str> {
    let mut arch: Option<&'static str> = None;
    for s in systems {
        let a = match s.split_once('-').map(|(a, _)| a).unwrap_or(s.as_str()) {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "i686" => "386",
            "armv7l" | "armv6l" => "arm",
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

/// Cluster-wide knob: Localhost seccomp profiles are written to
/// `/var/lib/kubelet/seccomp/operator/` BEFORE kubelet starts (P0541
/// Bottlerocket bootstrap container, EC2NodeClass userData). Both
/// reconcilers read this once at params-construction time so the
/// pod-spec builder stays pure (testable without env mutation).
///
/// Not a per-pool field: distribution is per-NODE (one EC2NodeClass),
/// not per-pool. Exposing it on the CRD would let an operator set
/// `seccompPreinstalled: true` on a pool whose nodes don't have the
/// bootstrap container — pods CrashLoopBackOff with "seccomp profile
/// not found" at sandbox creation. The controller-level env var keeps
/// the knob aligned with the deploy that wires the bootstrap.
pub fn seccomp_preinstalled() -> bool {
    std::env::var("RIO_SECCOMP_PREINSTALLED").is_ok_and(|v| v == "true")
}

/// STS name for a given pool. `rio-{role}-{pool_name}` — e.g. pool
/// `x86-64` → `rio-builder-x86-64`, fetcher class `tiny` →
/// `rio-fetcher-tiny`. Ephemeral Jobs use the same prefix with a
/// random suffix.
pub fn sts_name(pool_name: &str, role: ExecutorRole) -> String {
    format!("{NAME_PREFIX}-{}-{pool_name}", role.as_str())
}

/// Ephemeral Job name. `rio-{role}-{pool_name}-{6-char-suffix}` —
/// same prefix as the STS so logs/metrics group naturally by role.
pub fn ephemeral_job_name(pool_name: &str, role: ExecutorRole, suffix: &str) -> String {
    format!("{NAME_PREFIX}-{}-{pool_name}-{suffix}", role.as_str())
}

/// Build the executor StatefulSet.
///
/// `replicas`: `Some(n)` to claim SSA ownership (first create),
/// `None` to omit from the patch (subsequent reconciles — lets
/// the autoscaler's field manager own it).
pub fn build_executor_statefulset(
    p: &ExecutorStsParams,
    oref: OwnerReference,
    scheduler: &SchedulerAddrs,
    store: &StoreAddrs,
    replicas: Option<i32>,
) -> StatefulSet {
    let name = sts_name(&p.pool_name, p.role);
    let labels = executor_labels(p);

    StatefulSet {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(p.namespace.clone()),
            owner_references: Some(vec![oref]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(StatefulSetSpec {
            service_name: Some(name),
            // Parallel: all pods create/delete at once instead of
            // ordinal-by-ordinal. Executors are stateless (no ordinal-
            // dependent data) so ordering buys nothing. With OrderedReady
            // (the default), scaling 2 to 50 means STS creates pod-N only
            // after pod-(N-1) is Ready; on a cold cluster that is
            // ~60s/pod (Karpenter provision + boot), so 50 pods = ~48min.
            // Parallel: all 50 go Pending at once, Karpenter bursts
            // ~25 nodes, 50 builders in 2-3min (I-018, EKS stress test).
            //
            // Immutable field: flipping on an existing STS requires
            // delete+recreate. The reconciler handles first-create; a
            // manual kubectl delete sts --cascade=orphan triggers
            // recreate with the new policy while keeping pods alive.
            pod_management_policy: Some("Parallel".to_string()),
            // None on subsequent reconciles → field omitted from
            // SSA patch → autoscaler's ownership preserved.
            replicas,
            selector: LabelSelector {
                match_labels: Some(labels.clone()),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(build_executor_pod_spec(p, scheduler, store)),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The pod spec. `pub` so `builderpool::ephemeral::build_job` can
/// reuse it — the ephemeral Job pod is the same executor container
/// with one extra env var.
pub fn build_executor_pod_spec(
    p: &ExecutorStsParams,
    scheduler: &SchedulerAddrs,
    store: &StoreAddrs,
) -> PodSpec {
    // cgroup handling: we do NOT hostPath-mount /sys/fs/cgroup.
    // See builderpool/builders.rs pre-extraction commentary for the
    // full cgroupns-vs-hostPath reasoning; short version: containerd
    // cgroup-namespaces the container, and with privileged the
    // namespaced mount is RW — no hostPath needed, and a hostPath
    // would clobber host systemd.

    let labels = executor_labels(p);
    let spread_enabled = p.topology_spread.unwrap_or(true);
    let privileged = p.privileged;
    // Localhost seccomp: profile lives on node disk. Either:
    //   - reconciled by SPO's spod DaemonSet from a SeccompProfile CR
    //     (legacy path; spod may schedule after this pod on a fresh
    //     Karpenter node → wait-seccomp initContainer needed), OR
    //   - written by a Bottlerocket bootstrap container BEFORE kubelet
    //     starts (P0541; `seccomp_preinstalled=true` → no wait needed).
    // Gated on !privileged (privileged disables seccomp at runtime).
    let seccomp_localhost = (!privileged)
        .then_some(p.seccomp_profile.as_ref())
        .flatten()
        .filter(|k| k.type_ == "Localhost")
        .and_then(|k| k.localhost_profile.as_deref());
    // The wait-seccomp init + host-seccomp/host-spo hostPath volumes
    // are the WAIT machinery — only emitted when the profile path
    // existence is racy (SPO). The Localhost ENFORCEMENT on the
    // executor container (below) reads `seccomp_localhost` directly.
    let seccomp_wait = seccomp_localhost.filter(|_| !p.seccomp_preinstalled);

    PodSpec {
        containers: vec![build_executor_container(p, scheduler, store)],

        // wait-seccomp initContainer: polls the hostPath-mounted
        // kubelet seccomp dir until the Localhost profile lands.
        // Uses the executor image (has busybox). RuntimeDefault at
        // container-level so the initContainer can start before the
        // profile file lands; the actual Localhost enforcement is on
        // the executor container.
        init_containers: seccomp_wait.map(|profile| {
            vec![Container {
                name: "wait-seccomp".into(),
                image: Some(p.image.clone()),
                image_pull_policy: p.image_pull_policy.clone(),
                // busybox-static so the loop has `sleep` regardless of
                // the builder image's PATH (which is set for nix-daemon
                // + fuse + util-linux only — sh's builtins suffice for
                // `test`/`echo`, but `sleep` is external).
                command: Some(vec![
                    "/bin/busybox".into(),
                    "sh".into(),
                    "-c".into(),
                    // `test -s` (non-empty), not `test -f` (exists):
                    // SPO's spod writes via tmp+rename (saveProfileOnDisk
                    // → atomic.WriteFile) so a partial is unlikely, but
                    // a truncated profile loads with a partial ALLOW
                    // list → cryptic EACCES persisting until a new
                    // cgroup path. -s costs nothing.
                    format!(
                        "until test -s /host-seccomp/{profile}; do \
                         echo 'waiting for seccomp profile {profile}...'; \
                         busybox sleep 2; done"
                    ),
                ]),
                security_context: Some(SecurityContext {
                    seccomp_profile: Some(SeccompProfile {
                        type_: "RuntimeDefault".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                volume_mounts: Some(vec![
                    VolumeMount {
                        name: "host-seccomp".into(),
                        mount_path: "/host-seccomp".into(),
                        read_only: Some(true),
                        ..Default::default()
                    },
                    // Mount the symlink TARGET at the same path it has
                    // on the host so the absolute symlink resolves
                    // inside the container too.
                    VolumeMount {
                        name: "host-spo".into(),
                        mount_path: "/var/lib/security-profiles-operator".into(),
                        read_only: Some(true),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }]
        }),

        host_network: p.host_network.filter(|&h| h),
        dns_policy: p
            .host_network
            .filter(|&h| h)
            .map(|_| "ClusterFirstWithHostNet".into()),

        // r[impl sec.pod.host-users-false]
        // User-namespace isolation. See ADR-012. Incompatible with
        // privileged, hostNetwork, and hostPath /dev/fuse. The
        // spec.hostUsers override handles containerd<2.1 cgroup
        // ownership issues (cgroup_writable knob).
        host_users: p
            .host_users
            .or_else(|| (!privileged && p.host_network != Some(true)).then_some(false)),

        // Pod-level seccomp. RuntimeDefault when Localhost is
        // requested (so the sandbox+initContainer can start before
        // the profile file lands); the Localhost enforcement moves
        // to the executor container's SecurityContext.
        security_context: if !privileged {
            Some(PodSecurityContext {
                seccomp_profile: Some(if seccomp_localhost.is_some() {
                    SeccompProfile {
                        type_: "RuntimeDefault".into(),
                        ..Default::default()
                    }
                } else {
                    build_seccomp_profile(p.seccomp_profile.as_ref())
                }),
                ..Default::default()
            })
        } else {
            None
        },

        // Soft topology spread + anti-affinity. Node drain can make
        // hard spread unsatisfiable; soft lets pods schedule then
        // re-spread later.
        topology_spread_constraints: spread_enabled.then(|| {
            vec![TopologySpreadConstraint {
                max_skew: 1,
                topology_key: "kubernetes.io/hostname".into(),
                when_unsatisfiable: "ScheduleAnyway".into(),
                label_selector: Some(LabelSelector {
                    match_labels: Some(labels.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            }]
        }),
        affinity: spread_enabled.then(|| Affinity {
            pod_anti_affinity: Some(PodAntiAffinity {
                preferred_during_scheduling_ignored_during_execution: Some(vec![
                    WeightedPodAffinityTerm {
                        weight: 100,
                        pod_affinity_term: PodAffinityTerm {
                            label_selector: Some(LabelSelector {
                                match_labels: Some(labels.clone()),
                                ..Default::default()
                            }),
                            topology_key: "kubernetes.io/hostname".into(),
                            ..Default::default()
                        },
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        }),

        volumes: Some({
            let mut v = vec![
                // FUSE cache. emptyDir = local ephemeral storage,
                // wiped on pod restart. sizeLimit enforced by
                // kubelet.
                Volume {
                    name: "fuse-cache".into(),
                    empty_dir: Some(EmptyDirVolumeSource {
                        size_limit: Some(p.fuse_cache_quantity.clone()),
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
                // work, rootfs tampering does not. ADR-019 §Sandbox
                // hardening specs a tmpfs emptyDir; Memory medium
                // gives that (writes hit RAM, not disk — faster
                // for short-lived FOD fetches, and the pod's memory
                // limit bounds it).
                Volume {
                    name: "overlays".into(),
                    empty_dir: Some(if p.read_only_root_fs {
                        EmptyDirVolumeSource {
                            medium: Some("Memory".into()),
                            ..Default::default()
                        }
                    } else {
                        EmptyDirVolumeSource::default()
                    }),
                    ..Default::default()
                },
            ];
            if p.read_only_root_fs {
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
            // /dev/fuse: device plugin path (non-privileged) needs no
            // volume — kubelet+plugin inject the device node via
            // resources.limits. Privileged escape hatch uses hostPath.
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
            if seccomp_wait.is_some() {
                // SPO's non-root-enabler symlinks /var/lib/kubelet/
                // seccomp/operator → /var/lib/security-profiles-operator
                // (absolute). wait-seccomp's `test -s` can't follow it
                // unless the target dir is ALSO mounted. Kubelet (host-
                // side) resolves the symlink fine for localhostProfile.
                v.push(Volume {
                    name: "host-seccomp".into(),
                    host_path: Some(HostPathVolumeSource {
                        path: "/var/lib/kubelet/seccomp".into(),
                        type_: Some("DirectoryOrCreate".into()),
                    }),
                    ..Default::default()
                });
                v.push(Volume {
                    name: "host-spo".into(),
                    host_path: Some(HostPathVolumeSource {
                        path: "/var/lib/security-profiles-operator".into(),
                        type_: Some("DirectoryOrCreate".into()),
                    }),
                    ..Default::default()
                });
            }
            if let Some(secret) = &p.tls_secret_name {
                v.push(Volume {
                    name: "tls".into(),
                    secret: Some(SecretVolumeSource {
                        secret_name: Some(secret.clone()),
                        ..Default::default()
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

        automount_service_account_token: Some(false),
        termination_grace_period_seconds: Some(p.termination_grace_period_seconds.unwrap_or(7200)),
        node_selector: {
            let mut ns = p.node_selector.clone().unwrap_or_default();
            // I-098: a pool with systems=[x86_64-linux] landed pods on an
            // arm64 node (fallback NodePool unconstrained) — builder
            // registers as x86_64 from RIO_SYSTEMS, scheduler dispatches
            // x86_64 drvs, nix-daemon refuses (host is aarch64). Derive
            // kubernetes.io/arch from systems so karpenter constrains arch.
            // Builder-only: fetchers run `builtin` (arch-agnostic) and
            // benefit from cheaper Graviton; rio-builder's startup arch
            // check is the safety net for builders that slip through.
            if p.role == ExecutorRole::Builder
                && let Some(arch) = nix_systems_to_k8s_arch(&p.systems)
            {
                ns.entry("kubernetes.io/arch".into()).or_insert(arch.into());
            }
            if ns.is_empty() { None } else { Some(ns) }
        },
        tolerations: p.tolerations.clone(),

        ..Default::default()
    }
}

/// The executor container.
fn build_executor_container(
    p: &ExecutorStsParams,
    scheduler: &SchedulerAddrs,
    store: &StoreAddrs,
) -> Container {
    let privileged = p.privileged;

    Container {
        name: p.role.as_str().into(),
        image: Some(p.image.clone()),
        command: Some(vec!["/bin/rio-builder".into()]),
        image_pull_policy: p.image_pull_policy.clone(),
        env: Some({
            let mut e = vec![
                env("RIO_SCHEDULER_ADDR", &scheduler.addr),
                env("RIO_STORE_ADDR", &store.addr),
                env("RIO_FUSE_CACHE_SIZE_GB", &p.fuse_cache_gb.to_string()),
                env("RIO_FUSE_MOUNT_POINT", "/var/rio/fuse-store"),
                env("RIO_FUSE_CACHE_DIR", "/var/rio/cache"),
                env("RIO_OVERLAY_BASE_DIR", "/var/rio/overlays"),
                env("RIO_LOG_FORMAT", "json"),
                env("RIO_SYSTEMS", &p.systems.join(",")),
                env("RIO_FEATURES", &p.features.join(",")),
                // Executor self-identification. figment reads
                // `executor_id` → prefix RIO_ → `RIO_EXECUTOR_ID`.
                // StatefulSet pods are `<sts-name>-<ordinal>` —
                // unique, stable.
                env_from_field("RIO_EXECUTOR_ID", "metadata.name"),
                // Role discriminator. rio-builder's `RIO_EXECUTOR_
                // KIND` gates the FOD-vs-non-FOD refusal (ADR-019
                // §Executor enforcement — a builder receiving a FOD
                // returns WrongKind without spawning).
                env("RIO_EXECUTOR_KIND", p.role.as_str()),
            ];
            if let Some(host) = &scheduler.balance_host {
                e.push(env("RIO_SCHEDULER_BALANCE_HOST", host));
                e.push(env(
                    "RIO_SCHEDULER_BALANCE_PORT",
                    &scheduler.balance_port.to_string(),
                ));
            }
            if let Some(host) = &store.balance_host {
                e.push(env("RIO_STORE_BALANCE_HOST", host));
                e.push(env(
                    "RIO_STORE_BALANCE_PORT",
                    &store.balance_port.to_string(),
                ));
            }
            if let Some(n) = p.fuse_threads {
                e.push(env("RIO_FUSE_THREADS", &n.to_string()));
            }
            if p.tls_secret_name.is_some() {
                e.push(env("RIO_TLS__CERT_PATH", "/etc/rio/tls/tls.crt"));
                e.push(env("RIO_TLS__KEY_PATH", "/etc/rio/tls/tls.key"));
                e.push(env("RIO_TLS__CA_PATH", "/etc/rio/tls/ca.crt"));
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
            // Role-specific extras: builders pass size_class,
            // bloom_expected_items, daemon_timeout_secs,
            // fuse_passthrough here. Fetchers pass nothing.
            e.extend(p.extra_env.iter().cloned());
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
            if p.read_only_root_fs {
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
            if p.tls_secret_name.is_some() {
                m.push(VolumeMount {
                    name: "tls".into(),
                    mount_path: "/etc/rio/tls".into(),
                    read_only: Some(true),
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
            // so the sandbox+initContainer can start before the
            // profile file lands.
            seccomp_profile: p
                .seccomp_profile
                .as_ref()
                .filter(|k| k.type_ == "Localhost")
                .map(|_| build_seccomp_profile(p.seccomp_profile.as_ref())),
            // Fetcher hardening: rootfs tampering blocked. The
            // overlay upperdir (tmpfs emptyDir) is still writable.
            // ADR-019 §Sandbox hardening.
            read_only_root_filesystem: p.read_only_root_fs.then_some(true),
            ..Default::default()
        }),

        // r[impl sec.pod.fuse-device-plugin]
        // Operator resources + device-plugin FUSE request. Privileged
        // escape hatch: no FUSE resource (hostPath + privileged
        // bypasses device cgroup).
        resources: if privileged {
            p.resources.clone()
        } else {
            let mut r = p.resources.clone().unwrap_or_default();
            let fuse = (FUSE_DEVICE_RESOURCE.to_string(), Quantity("1".into()));
            r.limits
                .get_or_insert_with(BTreeMap::new)
                .insert(fuse.0.clone(), fuse.1.clone());
            r.requests
                .get_or_insert_with(BTreeMap::new)
                .insert(fuse.0, fuse.1);
            Some(r)
        },

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

        liveness_probe: Some(http_probe("/healthz", 9193, 10, 3)),
        readiness_probe: Some(http_probe("/readyz", 9193, 5, 3)),
        startup_probe: Some(http_probe("/healthz", 9193, 4, 30)),

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

fn http_probe(path: &str, port: i32, period: i32, failure_threshold: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.into()),
            port: IntOrString::Int(port),
            ..Default::default()
        }),
        period_seconds: Some(period),
        failure_threshold: Some(failure_threshold),
        ..Default::default()
    }
}

/// Parse a K8s Quantity string to gigabytes (integer, rounded DOWN).
///
/// Handles common cases: "100Gi" (binary), "100G" (decimal),
/// "107374182400" (raw bytes), "1.5Gi" (decimal mantissa). Rounding
/// DOWN: better to under-report cache ceiling than over (kubelet
/// evicts on overshoot).
pub fn parse_quantity_to_gb(q: &str) -> Result<u64> {
    let q = q.trim();
    // Suffixes in DECREASING length order so "Gi" matches before "G".
    const SUFFIXES: &[(&str, u64)] = &[
        ("Gi", 1024 * 1024 * 1024),
        ("Mi", 1024 * 1024),
        ("Ki", 1024),
        ("Ti", 1024 * 1024 * 1024 * 1024),
        ("G", 1_000_000_000),
        ("M", 1_000_000),
        ("K", 1_000),
        ("T", 1_000_000_000_000),
    ];

    for (suffix, mult) in SUFFIXES {
        if let Some(num) = q.strip_suffix(suffix) {
            let n: f64 = num.trim().parse().map_err(|_| {
                Error::InvalidSpec(format!(
                    "fuseCacheSize {q:?}: {num:?} before suffix is not a number"
                ))
            })?;
            if n < 0.0 || !n.is_finite() {
                return Err(Error::InvalidSpec(format!(
                    "fuseCacheSize {q:?}: must be a non-negative finite number"
                )));
            }
            let bytes_f = n * *mult as f64;
            if bytes_f > u64::MAX as f64 {
                return Err(Error::InvalidSpec(format!(
                    "fuseCacheSize {q:?}: overflows u64"
                )));
            }
            return Ok(bytes_f.floor() as u64 / (1024 * 1024 * 1024));
        }
    }

    let bytes: u64 = q.parse().map_err(|_| {
        Error::InvalidSpec(format!(
            "fuseCacheSize {q:?}: not a recognized quantity \
             (expected e.g. '100Gi', '50G', or raw bytes)"
        ))
    })?;
    Ok(bytes / (1024 * 1024 * 1024))
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
        assert_eq!(nix_systems_to_k8s_arch(&s(&["i686-linux"])), Some("386"));
        assert_eq!(nix_systems_to_k8s_arch(&s(&["armv7l-linux"])), Some("arm"));
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
