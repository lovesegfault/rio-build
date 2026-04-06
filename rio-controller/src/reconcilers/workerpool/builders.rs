//! Object builders (pure: WorkerPool → K8s objects)

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    Affinity, Capabilities, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource,
    EnvVar, EnvVarSource, HTTPGetAction, HostPathVolumeSource, ObjectFieldSelector,
    PodAffinityTerm, PodAntiAffinity, PodSecurityContext, PodSpec, PodTemplateSpec, Probe,
    SeccompProfile, SecretVolumeSource, SecurityContext, Service, ServicePort, ServiceSpec,
    TopologySpreadConstraint, Volume, VolumeMount, WeightedPodAffinityTerm,
};
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::ResourceExt;
use kube::api::ObjectMeta;

use crate::crds::workerpool::{SeccompProfileKind, WorkerPool};
use crate::error::{Error, Result};

/// Scheduler addresses injected into worker pod env. Bundled as
/// a struct because threading 3+ `&str` params through
/// `build_statefulset` → `build_pod_spec` → `build_container`
/// is noisy, and these always travel together.
#[derive(Clone)]
pub(super) struct SchedulerAddrs {
    /// ClusterIP Service `host:port` (`RIO_SCHEDULER_ADDR`).
    /// Used for the DrainWorker one-shot at SIGTERM and as
    /// the TLS verify domain's authority source.
    pub addr: String,
    /// Headless Service hostname (`RIO_SCHEDULER_BALANCE_HOST`).
    /// `None` = env var NOT injected → worker's figment sees
    /// `Option<String>::None` → single-channel fallback.
    pub balance_host: Option<String>,
    /// Headless Service port (`RIO_SCHEDULER_BALANCE_PORT`).
    pub balance_port: u16,
}

/// Labels applied to the StatefulSet, Service, and pods.
///
/// `rio.build/pool`: the WorkerPool name. The finalizer's
/// cleanup() lists pods by this label to DrainWorker each one.
/// Ephemeral mode's `reconcile_ephemeral` lists Jobs by this
/// same label to count active-vs-ceiling.
///
/// `app.kubernetes.io/*`: standard K8s recommended labels.
/// Dashboards and `kubectl get pods -l` queries expect these.
pub(super) fn labels(wp: &WorkerPool) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("rio.build/pool".into(), wp.name_any()),
        ("app.kubernetes.io/name".into(), "rio-worker".into()),
        ("app.kubernetes.io/component".into(), "worker".into()),
        ("app.kubernetes.io/part-of".into(), "rio-build".into()),
    ])
}

/// Headless Service. `clusterIP: None` + no ports (workers don't
/// serve). Exists purely to satisfy StatefulSet's `serviceName`.
pub(super) fn build_headless_service(wp: &WorkerPool, oref: OwnerReference) -> Service {
    let name = format!("{}-workers", wp.name_any());
    let labels = labels(wp);
    Service {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: wp.namespace(),
            owner_references: Some(vec![oref]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            // clusterIP: None = headless. Pod DNS records are
            // created, no load balancer VIP.
            cluster_ip: Some("None".into()),
            selector: Some(labels),
            // StatefulSet wants a port even for headless. Metrics
            // port is as good as any — it's exposed on every pod.
            ports: Some(vec![ServicePort {
                name: Some("metrics".into()),
                port: 9093,
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The StatefulSet. All the worker pod spec details live here —
/// security context, volumes, env, probes, termination grace.
///
/// `replicas`: `Some(n)` to claim SSA ownership (first create),
/// `None` to omit from the patch (subsequent reconciles — lets
/// the autoscaler's field manager own it).
///
/// Returns `Err(InvalidSpec)` if `fuse_cache_size` doesn't parse
/// as a K8s Quantity. That's the one operator-supplied string
/// we need to validate.
pub(super) fn build_statefulset(
    wp: &WorkerPool,
    oref: OwnerReference,
    scheduler: &SchedulerAddrs,
    store_addr: &str,
    replicas: Option<i32>,
) -> Result<StatefulSet> {
    let name = format!("{}-workers", wp.name_any());
    let labels = labels(wp);

    // Parse fuseCacheSize. K8s Quantity accepts "50Gi", "100G",
    // "107374182400" etc. We store the raw Quantity for the
    // emptyDir sizeLimit, and also extract GB for the worker's
    // RIO_FUSE_CACHE_SIZE_GB env var (it wants an integer).
    //
    // Quantity parsing is just "is it a valid string" — the
    // k8s-openapi type is a String wrapper. Real validation
    // happens at the apiserver. But we need the numeric value
    // for the env var, so parse it ourselves.
    let cache_quantity = Quantity(wp.spec.fuse_cache_size.clone());
    let cache_gb = parse_quantity_to_gb(&wp.spec.fuse_cache_size)?;

    Ok(StatefulSet {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: wp.namespace(),
            owner_references: Some(vec![oref]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(StatefulSetSpec {
            // serviceName MUST match the headless Service above.
            // StatefulSet controller uses it for pod DNS.
            service_name: Some(name),
            // None on subsequent reconciles → field omitted from
            // SSA patch → autoscaler's ownership preserved. See
            // apply() for the existence check.
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
                spec: Some(build_pod_spec(
                    wp,
                    scheduler,
                    store_addr,
                    cache_gb,
                    cache_quantity,
                )),
            },
            ..Default::default()
        }),
        ..Default::default()
    })
}

// r[impl ctrl.pdb.workers]
/// PodDisruptionBudget for this pool. `maxUnavailable: 1` so at most
/// one worker is evicted at a time during node drain — builds in
/// flight on the evicting pod get reassigned (DrainWorker force), the
/// rest of the pool keeps working.
///
/// PDB matches the SAME labels as the STS (and therefore the pods).
/// The finalizer doesn't explicitly delete this — ownerRef GC handles
/// it (same as the Service).
pub(super) fn build_pdb(wp: &WorkerPool, oref: OwnerReference) -> PodDisruptionBudget {
    let name = format!("{}-pdb", wp.name_any());
    let labels = labels(wp);
    PodDisruptionBudget {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: wp.namespace(),
            owner_references: Some(vec![oref]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(PodDisruptionBudgetSpec {
            // maxUnavailable not minAvailable: minAvailable would
            // need to track spec.replicas.min and update on CRD
            // edit. maxUnavailable=1 is stable regardless of
            // scale — "evict one at a time" works for 2 replicas
            // or 200.
            max_unavailable: Some(IntOrString::Int(1)),
            selector: Some(LabelSelector {
                match_labels: Some(labels),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The pod spec. Separate fn because it's the bulk of the
/// StatefulSet complexity and it's useful to test in isolation.
///
/// `pub(super)` (not module-private) so `ephemeral::build_job`
/// can reuse it — the ephemeral Job pod is the same worker
/// container with one extra env var (`RIO_EPHEMERAL=1`). Better
/// to reuse and append than duplicate the volume/security/env
/// block.
pub(super) fn build_pod_spec(
    wp: &WorkerPool,
    scheduler: &SchedulerAddrs,
    store_addr: &str,
    cache_gb: u64,
    cache_quantity: Quantity,
) -> PodSpec {
    // cgroup handling: we do NOT hostPath-mount /sys/fs/cgroup.
    // containerd cgroup-namespaces the container by default:
    // /proc/self/cgroup shows 0::/ and /sys/fs/cgroup is the
    // NAMESPACED view (container's cgroup appears as root).
    // With privileged, that namespaced mount is RW (runc default).
    //
    // If we hostPath-mounted the HOST's /sys/fs/cgroup over it:
    // /proc/self/cgroup STILL shows 0::/ (cgroupns is independent
    // of mount ns), but /sys/fs/cgroup now shows the HOST root.
    // The worker's ns-root-detection would mkdir+move into
    // /sys/fs/cgroup/leaf/ on the HOST — clobbering host systemd.
    //
    // Instead: worker's delegated_root() detects the ns-root case
    // and moves itself into a /leaf/ sub-cgroup WITHIN the
    // namespaced view. Safe (writes stay in the container's
    // cgroup subtree). privileged → rw → writes succeed.

    let labels = labels(wp);
    // topology_spread: default true (None → true). Soft spread
    // on hostname — avoid all pods on one node without blocking
    // scheduling when that's impossible.
    let spread_enabled = wp.spec.topology_spread.unwrap_or(true);

    PodSpec {
        containers: vec![build_container(wp, scheduler, store_addr, cache_gb)],
        // hostNetwork from spec. None → K8s default (false).
        // Some(false) → explicit false (same effect). Some(true)
        // → pod shares node netns. `filter`: PodSpec field is
        // None (not Some(false)) when unset — less diff noise
        // in `kubectl get -o yaml`.
        host_network: wp.spec.host_network.filter(|&h| h),

        // dnsPolicy: hostNetwork pods default to `Default` (node's
        // /etc/resolv.conf) instead of `ClusterFirst` → can't
        // resolve K8s Service names (rio-scheduler, rio-store).
        // `ClusterFirstWithHostNet` fixes that. Only set when
        // hostNetwork is true; else K8s default (ClusterFirst)
        // is fine.
        dns_policy: wp
            .spec
            .host_network
            .filter(|&h| h)
            .map(|_| "ClusterFirstWithHostNet".into()),

        // seccompProfile at POD level (applies to all containers +
        // init containers). Skipped when privileged — privileged
        // disables seccomp at the runtime level anyway, and setting
        // it would be confusing noise in `kubectl get -o yaml`.
        security_context: if wp.spec.privileged != Some(true) {
            Some(PodSecurityContext {
                seccomp_profile: Some(build_seccomp_profile(wp.spec.seccomp_profile.as_ref())),
                ..Default::default()
            })
        } else {
            None
        },

        // Topology spread + anti-affinity. Both soft: node drain
        // temporarily makes hard spread unsatisfiable (only 1
        // viable node for N pods) → Pending. Soft lets them
        // schedule then re-spread later.
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
                // Preferred not required: same soft-spread
                // reasoning. weight=100 (max) so K8s scheduler
                // tries hard before giving up.
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
                // /dev/fuse character device. hostPath not because
                // we need a file from the host, but because it's a
                // DEVICE node that must be the same major/minor as
                // the host's. CharDevice type makes K8s verify it's
                // actually a char device (catches a node without
                // FUSE compiled in).
                Volume {
                    name: "dev-fuse".into(),
                    host_path: Some(HostPathVolumeSource {
                        path: "/dev/fuse".into(),
                        type_: Some("CharDevice".into()),
                    }),
                    ..Default::default()
                },
                // FUSE cache. emptyDir = local ephemeral storage,
                // wiped on pod restart. sizeLimit enforced by the
                // kubelet (evicts pod if exceeded). For persistent
                // cache across restarts, operators can override
                // with a PVC via kustomize — we don't model that
                // in the CRD (too many backend-specific knobs).
                Volume {
                    name: "fuse-cache".into(),
                    empty_dir: Some(EmptyDirVolumeSource {
                        size_limit: Some(cache_quantity),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                // Overlay upperdir/workdir. MUST be a real filesystem
                // (ext4/xfs via emptyDir on the node's disk), NOT the
                // container's root — containerd's root is overlayfs,
                // and overlayfs-as-upperdir can't create trusted.*
                // xattrs → mount fails with EINVAL. emptyDir gives us
                // the kubelet's local disk (/var/lib/kubelet/pods/...),
                // which is ext4/xfs on every sane node. No sizeLimit:
                // overlays are per-build, cleaned on Drop; unbounded
                // growth = a leak bug that sizeLimit would mask with
                // a pod eviction (worse debugging).
                Volume {
                    name: "overlays".into(),
                    empty_dir: Some(EmptyDirVolumeSource::default()),
                    ..Default::default()
                },
            ];
            // mTLS Secret mount. Only when spec.tlsSecretName is set.
            // The Secret's tls.crt/tls.key/ca.crt are cert-manager's
            // standard output keys (see infra/helm/rio-build/templates/
            // cert-manager.yaml). Mounted at /etc/rio/tls/; env vars
            // below point the worker's TlsConfig there.
            if let Some(secret) = &wp.spec.tls_secret_name {
                v.push(Volume {
                    name: "tls".into(),
                    secret: Some(SecretVolumeSource {
                        secret_name: Some(secret.clone()),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
            // nix.conf ConfigMap mount. The `rio-nix-conf` ConfigMap
            // (infra/helm/rio-build/templates/configmaps.yaml) overrides
            // in WORKER_NIX_CONF. Operators customize experimental-
            // features etc without image rebuild. setup_nix_conf in
            // the executor checks /etc/rio/nix.conf first.
            //
            // `optional: true`: if the ConfigMap doesn't exist (dev
            // cluster, forgot to apply base), the mount is empty and
            // setup_nix_conf falls back to the const. Without
            // optional, the pod would be stuck ContainerCreating.
            v.push(Volume {
                name: "nix-conf".into(),
                config_map: Some(ConfigMapVolumeSource {
                    name: "rio-nix-conf".into(),
                    optional: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            });
            // Coverage propagation: see the LLVM_PROFILE_FILE env
            // push in build_container below. DirectoryOrCreate lets
            // K8s make the dir with 0755 root — the common.nix
            // tmpfiles rule also creates it, whichever runs first
            // wins (both are 0755 root, idempotent).
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

        // Security context. SYS_ADMIN for mount (FUSE, overlayfs,
        // CLONE_NEWNS in the executor's pre_exec). SYS_CHROOT
        // for nix-daemon's sandbox pivot_root. NOT privileged —
        // that disables seccomp entirely. Capabilities are
        // granular.
        //
        // This is the PRODUCTION default. VM tests override with
        // privileged: true because containerd's device cgroup
        // doesn't include /dev/fuse by default (needs
        // smarter-device-manager or similar in prod; privileged
        // is the test shortcut).
        //
        // Container-level not pod-level: PodSecurityContext
        // doesn't have capabilities; Container's does.
        // (security_context on PodSpec is for UID/fsGroup etc.)

        // automountServiceAccountToken: workers talk to the
        // scheduler via gRPC, not the K8s API. No token needed.
        // Defense-in-depth: a compromised worker can't use the
        // pod's SA to read secrets.
        automount_service_account_token: Some(false),

        // Default 2 hours — nix builds can legitimately take that
        // long (LLVM from cold ccache, NixOS closures). SIGTERM →
        // worker's drain sequence (DrainWorker + wait for in-flight).
        // After grace period: SIGKILL, builds lost. Overridable via
        // spec for clusters with known-shorter builds.
        termination_grace_period_seconds: Some(
            wp.spec.termination_grace_period_seconds.unwrap_or(7200),
        ),

        node_selector: wp.spec.node_selector.clone(),
        tolerations: wp.spec.tolerations.clone(),

        ..Default::default()
    }
}

/// The worker container.
fn build_container(
    wp: &WorkerPool,
    scheduler: &SchedulerAddrs,
    store_addr: &str,
    cache_gb: u64,
) -> Container {
    Container {
        name: "worker".into(),
        image: Some(wp.spec.image.clone()),
        // Unconditional: overrides the image Entrypoint. For the
        // per-component rio-worker image this is a no-op (Entrypoint
        // is already rio-worker). For the VM-test rio-all aggregate
        // image (no Entrypoint), this selects the right binary.
        // buildLayeredImage symlinks rio-workspace/bin/* into /bin/.
        command: Some(vec!["/bin/rio-worker".into()]),
        // imagePullPolicy: K8s defaults to Always for :latest,
        // IfNotPresent otherwise. Airgap clusters need an explicit
        // IfNotPresent/Never — kustomize can't patch the generated
        // STS (only the CRD), so the spec has this knob.
        image_pull_policy: wp.spec.image_pull_policy.clone(),
        env: Some(vec![
            env("RIO_SCHEDULER_ADDR", &scheduler.addr),
            // From ctx.store_addr — NOT derived from scheduler_addr.
            // The kustomize base uses different Service names
            // (rio-scheduler vs rio-store); a port-replace would
            // yield "rio-scheduler:9002" which doesn't exist.
            env("RIO_STORE_ADDR", store_addr),
            env("RIO_MAX_BUILDS", &wp.spec.max_concurrent_builds.to_string()),
            env("RIO_FUSE_CACHE_SIZE_GB", &cache_gb.to_string()),
            env("RIO_FUSE_MOUNT_POINT", "/var/rio/fuse-store"),
            env("RIO_FUSE_CACHE_DIR", "/var/rio/cache"),
            env("RIO_OVERLAY_BASE_DIR", "/var/rio/overlays"),
            env("RIO_LOG_FORMAT", "json"),
            // size_class: always set (empty string is the Config
            // default anyway).
            env("RIO_SIZE_CLASS", &wp.spec.size_class),
            // systems + features: comma-sep strings; worker's
            // comma_vec deserialize helper splits them. systems
            // is CEL-validated non-empty (crds/workerpool.rs:97),
            // so join() always produces at least one element.
            // features may be empty (empty string → comma_vec
            // filters to empty vec, same as unset).
            env("RIO_SYSTEMS", &wp.spec.systems.join(",")),
            env("RIO_FEATURES", &wp.spec.features.join(",")),
            // RIO_WORKER_ID from pod name via downward API.
            // StatefulSet pods are `<sts-name>-<ordinal>`, e.g.,
            // `default-workers-0` — unique, stable. Two pools
            // can't collide (different sts names).
            env_from_field("RIO_WORKER_ID", "metadata.name"),
        ])
        .map(|mut e| {
            // Conditionally append TLS env vars. Using map-over-
            // Some keeps the builder linear (vs an if-push dance
            // with a mutable Vec, which would need `let mut env_vec
            // = vec![...]; if tls { env_vec.push(...) }; Some(
            // env_vec)`). The Option::map pattern reads cleaner.
            // Headless Service for health-aware balanced routing.
            // Worker DNS-resolves this to pod IPs, health-probes
            // grpc.health.v1, routes to the leader. On scheduler
            // failover, BuildExecution reconnects via the balanced
            // channel; running builds continue. NOT injected when
            // None — worker's Option<String> stays None → single-
            // channel fallback. Injecting an empty string would
            // give Some("") which fails DNS resolve.
            if let Some(host) = &scheduler.balance_host {
                e.push(env("RIO_SCHEDULER_BALANCE_HOST", host));
                e.push(env(
                    "RIO_SCHEDULER_BALANCE_PORT",
                    &scheduler.balance_port.to_string(),
                ));
            }
            // Worker tuning knobs (plan 21 Batch E). Only inject when
            // explicitly set in the CRD — `None` means the worker's
            // compiled-in default wins (figment layering: env unset
            // → Config::default value). Injecting the default here
            // would pin it at controller-build time, not worker-build
            // time — subtle drift if they're built from different
            // commits.
            if let Some(n) = wp.spec.fuse_threads {
                e.push(env("RIO_FUSE_THREADS", &n.to_string()));
            }
            if let Some(p) = wp.spec.fuse_passthrough {
                // figment bool parse accepts "true"/"false" (see
                // rio-common/src/config.rs bool test).
                e.push(env(
                    "RIO_FUSE_PASSTHROUGH",
                    if p { "true" } else { "false" },
                ));
            }
            if let Some(s) = wp.spec.daemon_timeout_secs {
                e.push(env("RIO_DAEMON_TIMEOUT_SECS", &s.to_string()));
            }
            if wp.spec.tls_secret_name.is_some() {
                // Paths match the volume mount below and cert-
                // manager's standard Secret key names.
                e.push(env("RIO_TLS__CERT_PATH", "/etc/rio/tls/tls.crt"));
                e.push(env("RIO_TLS__KEY_PATH", "/etc/rio/tls/tls.key"));
                e.push(env("RIO_TLS__CA_PATH", "/etc/rio/tls/ca.crt"));
            }
            // FOD proxy URL. The worker's daemon spawn checks
            // is_fixed_output before injecting http_proxy/
            // https_proxy — non-FOD builds never see this even
            // though the env var is set on every container.
            if let Some(url) = &wp.spec.fod_proxy_url {
                e.push(env("RIO_FOD_PROXY_URL", url));
            }
            // Coverage propagation (test-only): if the controller
            // is running under -Cinstrument-coverage (VM test
            // coverage mode sets LLVM_PROFILE_FILE in
            // controllerEnv), inject the same template into the
            // pod. Prod controllers don't set this env var, so
            // prod pods are unaffected. The hostPath volume below
            // surfaces profraws to the k8s node's
            // /var/lib/rio/cov for collectCoverage.
            if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
                e.push(env(
                    "LLVM_PROFILE_FILE",
                    "/var/lib/rio/cov/rio-%p-%m.profraw",
                ));
            }
            // Propagate the controller's RUST_LOG verbatim. The helm
            // chart sets global.logLevel → RUST_LOG on all rio-* pods;
            // passing it through here makes workers follow the same
            // knob. Verbatim means per-crate filters work as written:
            // "info,rio_worker=debug" → worker at debug, deps at info.
            if let Ok(level) = std::env::var("RUST_LOG") {
                e.push(env("RUST_LOG", &level));
            }
            e
        }),

        volume_mounts: Some({
            let mut m = vec![
                VolumeMount {
                    name: "dev-fuse".into(),
                    mount_path: "/dev/fuse".into(),
                    ..Default::default()
                },
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
            if wp.spec.tls_secret_name.is_some() {
                m.push(VolumeMount {
                    name: "tls".into(),
                    mount_path: "/etc/rio/tls".into(),
                    read_only: Some(true),
                    ..Default::default()
                });
            }
            // nix-conf: mount the ConfigMap as a DIRECTORY (no
            // subPath). setup_nix_conf reads /etc/rio/nix-conf/
            // nix.conf. With optional=true + missing ConfigMap,
            // K8s mounts an empty dir → read("dir/nix.conf")
            // gets clean ENOENT → fallback to WORKER_NIX_CONF.
            //
            // Directory mount (no subPath): subPath="nix.conf"
            // creates an empty dir/file at the mount point when
            // the ConfigMap is missing → read() returns
            // IsADirectory or Ok(vec![]) instead of NotFound →
            // setup_nix_conf writes empty nix.conf → Nix defaults
            // → substitute=true → airgap DNS hang. Directory mount
            // + missing ConfigMap → clean ENOENT → fallback works.
            m.push(VolumeMount {
                name: "nix-conf".into(),
                mount_path: "/etc/rio/nix-conf".into(),
                read_only: Some(true),
                ..Default::default()
            });
            // Coverage: mount the hostPath. See the env push above.
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
            // Granular caps are the default. privileged=true
            // overrides — it disables seccomp and grants ALL
            // caps (the capabilities list below becomes
            // irrelevant but harmless). Set BOTH: if an
            // operator flips privileged back to false later,
            // the caps are still there and the pod keeps
            // working.
            privileged: wp.spec.privileged.filter(|&p| p),
            capabilities: Some(Capabilities {
                add: Some(vec!["SYS_ADMIN".into(), "SYS_CHROOT".into()]),
                ..Default::default()
            }),
            ..Default::default()
        }),

        resources: wp.spec.resources.clone(),

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

        // Probes. HTTP /healthz + /readyz.
        //
        // Liveness: /healthz, always 200. If it fails, the
        // process is dead (not responding) → restart.
        //
        // Readiness: /readyz, 200 after first heartbeat accepted.
        // NOT_READY → not counted in Service endpoints (though
        // workers aren't load-balanced anyway — mainly for
        // `kubectl get pods` visibility + rollout gating).
        //
        // Startup: /healthz with long failureThreshold. The
        // worker needs time to mount FUSE + do cgroup setup.
        // 120s total (30 × 4s) per controller.md. Until startup
        // passes, liveness/readiness are suppressed — prevents
        // restart loops during slow boots.
        liveness_probe: Some(http_probe("/healthz", 9193, 10, 3)),
        readiness_probe: Some(http_probe("/readyz", 9193, 5, 3)),
        startup_probe: Some(http_probe("/healthz", 9193, 4, 30)),

        ..Default::default()
    }
}

// ----- small builder helpers --------------------------------------------------

/// Translate the CRD's `SeccompProfileKind` to a k8s-openapi
/// `SeccompProfile`. The two are nearly identical by design (same
/// YAML shape — see the `SeccompProfileKind` doc comment); this
/// is type-conversion, not logic.
///
/// `None` → `RuntimeDefault`. The field is `Option` so operators
/// don't have to set it (most deployments use RuntimeDefault), but
/// the builder always emits SOMETHING — a pod with no seccompProfile
/// at all is `Unconfined` on most runtimes, which we never want
/// implicitly.
///
/// Unknown `type_` values → `RuntimeDefault` as a safe floor. In
/// practice CEL at the apiserver rejects unknowns before we ever
/// see them (see the `x_kube(validation)` on `SeccompProfileKind`),
/// so this arm is dead outside of direct unit tests bypassing the
/// apiserver. Defensive because seccomp is a security control —
/// fail-closed (RuntimeDefault is the floor, never fall through to
/// Unconfined on a typo).
// r[impl worker.seccomp.localhost-profile]
fn build_seccomp_profile(kind: Option<&SeccompProfileKind>) -> SeccompProfile {
    match kind.map(|k| k.type_.as_str()) {
        Some("Localhost") => SeccompProfile {
            type_: "Localhost".into(),
            // CEL guarantees localhost_profile is Some when
            // type=Localhost. clone() not as_deref().map(Into::into)
            // — Option<String>→Option<String> is just clone.
            localhost_profile: kind.and_then(|k| k.localhost_profile.clone()),
        },
        Some("Unconfined") => SeccompProfile {
            type_: "Unconfined".into(),
            ..Default::default()
        },
        // None, Some("RuntimeDefault"), and any CEL-bypassing
        // unknowns: the safe default.
        _ => SeccompProfile {
            type_: "RuntimeDefault".into(),
            ..Default::default()
        },
    }
}

pub(super) fn env(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: Some(value.into()),
        ..Default::default()
    }
}

/// Downward API: env var from pod metadata field.
fn env_from_field(name: &str, field_path: &str) -> EnvVar {
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
/// K8s Quantity is complex (binary vs decimal suffixes, fixed-
/// point). We handle the common cases operators actually write:
/// "100Gi" (binary), "100G" (decimal), "107374182400" (raw bytes).
///
/// Rounding DOWN: the worker's cache LRU uses this as a ceiling.
/// Better to under-report (cache a bit below emptyDir limit) than
/// over (kubelet evicts the pod when cache fills to what we
/// THOUGHT was the limit but is actually over).
///
/// Doesn't handle milli ("100m") or exponential ("1e9") —
/// nobody writes cache sizes that way. Returns InvalidSpec with
/// a helpful message.
pub(super) fn parse_quantity_to_gb(q: &str) -> Result<u64> {
    let q = q.trim();
    // Suffixes in DECREASING length order — "Gi" before "G" so
    // strip_suffix doesn't match "G" on "100Gi" leaving "100i".
    // Each tuple: (suffix, bytes-per-unit).
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
            // Parse as f64 to accept decimal quantities like
            // "1.5Gi" (K8s Quantity allows these). Compute in f64,
            // then floor to u64 bytes before the GB division.
            // "1.5Gi" → 1.5 * 1024^3 = 1610612736 bytes → 1 GB.
            // A u64 parse would reject decimals → CRD apply fails
            // for a valid K8s Quantity.
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
            // Integer division rounds down.
            return Ok(bytes_f.floor() as u64 / (1024 * 1024 * 1024));
        }
    }

    // No suffix → raw bytes.
    let bytes: u64 = q.parse().map_err(|_| {
        Error::InvalidSpec(format!(
            "fuseCacheSize {q:?}: not a recognized quantity \
             (expected e.g. '100Gi', '50G', or raw bytes)"
        ))
    })?;
    Ok(bytes / (1024 * 1024 * 1024))
}
