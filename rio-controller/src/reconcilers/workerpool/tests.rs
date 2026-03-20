// r[verify ctrl.crd.workerpool]
// r[verify ctrl.reconcile.owner-refs]
// r[verify ctrl.drain.all-then-scale]
// r[verify ctrl.drain.sigterm]

use std::collections::BTreeMap;

use super::builders::*;
use super::*;
use crate::crds::workerpool::SeccompProfileKind;
use crate::fixtures::{ApiServerVerifier, Scenario, apply_ok_scenarios, test_sched_addrs};

/// Construct a minimal WorkerPool for builder tests. No K8s
/// interaction — pure struct-to-struct.
///
/// Delegates to the shared fixture. Local wrapper kept so the
/// 39 call sites in this file don't need a signature change.
fn test_wp() -> WorkerPool {
    crate::fixtures::test_workerpool("test-pool")
}

/// Shorthand for tests: builds with default scheduler/store
/// addrs and replicas=Some(min). Use `build_statefulset`
/// directly for tests that care about those params.
fn test_sts(wp: &WorkerPool) -> StatefulSet {
    build_statefulset(
        wp,
        wp.controller_owner_ref(&()).unwrap(),
        &test_sched_addrs(),
        "store:9002",
        Some(wp.spec.replicas.min),
    )
    .unwrap()
}

#[test]
fn statefulset_has_owner_reference() {
    let wp = test_wp();
    let sts = test_sts(&wp);

    let orefs = sts.metadata.owner_references.expect("ownerRef set");
    assert_eq!(orefs.len(), 1);
    assert_eq!(orefs[0].kind, "WorkerPool");
    assert_eq!(orefs[0].name, "test-pool");
    assert_eq!(orefs[0].controller, Some(true), "controller=true for GC");
}

#[test]
fn statefulset_security_context() {
    let wp = test_wp();
    let sts = test_sts(&wp);

    let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    let caps = container
        .security_context
        .as_ref()
        .unwrap()
        .capabilities
        .as_ref()
        .unwrap();
    let add = caps.add.as_ref().unwrap();
    assert!(add.contains(&"SYS_ADMIN".to_string()));
    assert!(add.contains(&"SYS_CHROOT".to_string()));
    // NOT privileged — capabilities are granular, privileged
    // disables seccomp. If someone adds privileged here, this
    // test fails.
    assert_eq!(
        container.security_context.as_ref().unwrap().privileged,
        None
    );
}

// r[verify worker.seccomp.localhost-profile]
#[test]
fn seccomp_default_is_runtime_default() {
    // spec.seccompProfile=None → builder emits RuntimeDefault at
    // the POD level. NOT Unconfined — an absent seccompProfile on
    // the pod spec would be implicitly Unconfined on most runtimes.
    // The builder must always emit SOMETHING.
    let wp = test_wp();
    assert!(wp.spec.seccomp_profile.is_none(), "test_wp() baseline");
    let sts = test_sts(&wp);

    let pod_sc = sts
        .spec
        .unwrap()
        .template
        .spec
        .unwrap()
        .security_context
        .expect("pod security_context set (privileged=None)");
    let prof = pod_sc.seccomp_profile.expect("seccompProfile set");
    assert_eq!(prof.type_, "RuntimeDefault");
    assert_eq!(prof.localhost_profile, None);
}

// r[verify worker.seccomp.localhost-profile]
#[test]
fn seccomp_localhost_emits_correct_security_context() {
    // spec.seccompProfile={type: Localhost, localhostProfile: rio-
    // worker.json} → pod securityContext carries exactly that.
    // The path is relative to /var/lib/kubelet/seccomp/ on the
    // node — the kubelet resolves it, not us.
    let mut wp = test_wp();
    wp.spec.seccomp_profile = Some(SeccompProfileKind {
        type_: "Localhost".into(),
        localhost_profile: Some("rio-worker.json".into()),
    });
    let sts = test_sts(&wp);

    let pod_sc = sts
        .spec
        .unwrap()
        .template
        .spec
        .unwrap()
        .security_context
        .expect("pod security_context set");
    let prof = pod_sc.seccomp_profile.expect("seccompProfile set");
    assert_eq!(prof.type_, "Localhost");
    assert_eq!(prof.localhost_profile, Some("rio-worker.json".into()));
}

// r[verify worker.seccomp.localhost-profile]
#[test]
fn seccomp_privileged_drops_profile() {
    // privileged=true disables seccomp at the runtime level. The
    // builder drops the POD securityContext entirely rather than
    // emit a profile that would be silently ignored — less
    // confusing `kubectl get -o yaml`. The spec field being set
    // doesn't change that.
    let mut wp = test_wp();
    wp.spec.privileged = Some(true);
    wp.spec.seccomp_profile = Some(SeccompProfileKind {
        type_: "Localhost".into(),
        localhost_profile: Some("rio-worker.json".into()),
    });
    let sts = test_sts(&wp);

    let pod = sts.spec.unwrap().template.spec.unwrap();
    assert!(
        pod.security_context.is_none(),
        "privileged disables seccomp — don't emit a dead profile"
    );
}

/// Localhost profile JSON — `infra/helm/rio-build/files/`. Compiled
/// in via include_str! so the test fails at BUILD time if the file
/// goes missing (rather than being a silent no-op at deploy time
/// when someone forgets to install it on nodes).
const SECCOMP_PROFILE_JSON: &str =
    include_str!("../../../../infra/helm/rio-build/files/seccomp-rio-worker.json");

#[test]
fn seccomp_profile_json_is_valid() {
    // The profile parses as JSON and is an ALLOWLIST (defaultAction
    // ERRNO). A denylist (defaultAction ALLOW + explicit ERRNO for
    // the 5 targets) would be a security REGRESSION vs RuntimeDefault
    // — it would re-enable the ~40 syscalls RuntimeDefault blocks
    // (kexec_load, open_by_handle_at, userfaultfd etc). K8s type:
    // Localhost REPLACES RuntimeDefault; it doesn't stack.
    let profile: serde_json::Value =
        serde_json::from_str(SECCOMP_PROFILE_JSON).expect("profile is valid JSON");

    assert_eq!(
        profile["defaultAction"], "SCMP_ACT_ERRNO",
        "allowlist — denylist would regress vs RuntimeDefault (see Audit B1 #12)"
    );
    assert_eq!(
        profile["defaultErrnoRet"], 1,
        "EPERM — the standard 'operation not permitted' errno"
    );

    // Collect every syscall that appears in any ALLOW block. The
    // 5 targets must be absent from ALL of them — defaultAction
    // ERRNO is what denies them. An explicit ERRNO block for them
    // would be harmless but redundant; absence from ALLOW is the
    // actual security property.
    let allowed: std::collections::HashSet<&str> = profile["syscalls"]
        .as_array()
        .expect("syscalls is array")
        .iter()
        .filter(|b| b["action"] == "SCMP_ACT_ALLOW")
        .flat_map(|b| b["names"].as_array().into_iter().flatten())
        .filter_map(|n| n.as_str())
        .collect();

    // The 5 denied syscalls — absent from allow set.
    for denied in [
        "ptrace",
        "bpf",
        "setns",
        "process_vm_readv",
        "process_vm_writev",
    ] {
        assert!(
            !allowed.contains(denied),
            "{denied} must be ABSENT from allow blocks (denied via defaultAction)"
        );
    }

    // Worker-critical syscalls — present in allow set. If any of
    // these regress the worker can't mount overlayfs / set up the
    // Nix sandbox. Regression guard for future profile edits.
    for needed in ["mount", "unshare", "chroot", "clone", "umount2"] {
        assert!(
            allowed.contains(needed),
            "{needed} must be ALLOWED (worker overlayfs/sandbox needs it)"
        );
    }
}

// r[verify sec.pod.fuse-device-plugin]
#[test]
fn statefulset_fuse_via_device_plugin_when_unprivileged() {
    // Default (privileged=None→false): NO hostPath /dev/fuse volume.
    // Instead, resources.limits has smarter-devices/fuse=1 and the
    // kubelet+device-plugin inject the device. This is the ADR-012
    // production path — enables hostUsers:false (hostPath /dev/fuse
    // is incompatible with idmap mounts).
    let wp = test_wp();
    let sts = test_sts(&wp);
    let pod = sts.spec.unwrap().template.spec.unwrap();

    // No dev-fuse volume (device plugin injects, no volume needed).
    assert!(
        !pod.volumes
            .as_ref()
            .unwrap()
            .iter()
            .any(|v| v.name == "dev-fuse"),
        "non-privileged path uses device plugin, not hostPath"
    );
    // No dev-fuse mount either.
    assert!(
        !pod.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .any(|m| m.name == "dev-fuse"),
    );

    // resources.limits has the FUSE device resource. kubelet sees
    // this → device plugin injects /dev/fuse + adds to device cgroup.
    let resources = pod.containers[0]
        .resources
        .as_ref()
        .expect("resources set (device plugin request)");
    let limits = resources.limits.as_ref().expect("limits set");
    assert_eq!(
        limits.get("smarter-devices/fuse").map(|q| q.0.as_str()),
        Some("1"),
        "one FUSE device per worker pod"
    );
    // K8s treats extended-resource limits as requests too, but we
    // set both for `kubectl get` clarity.
    let requests = resources.requests.as_ref().expect("requests set");
    assert_eq!(
        requests.get("smarter-devices/fuse").map(|q| q.0.as_str()),
        Some("1"),
    );
}

// r[verify sec.pod.fuse-device-plugin]
#[test]
fn statefulset_fuse_device_merges_with_operator_resources() {
    // Operator-supplied resources (cpu/memory/ephemeral) must be
    // PRESERVED when the builder adds the FUSE device request.
    // A naive overwrite would drop the operator's limits → unbounded
    // pod on a shared node (noisy neighbor).
    use k8s_openapi::api::core::v1::ResourceRequirements;
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

    let mut wp = test_wp();
    wp.spec.resources = Some(ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".into(), Quantity("2".into())),
            ("memory".into(), Quantity("4Gi".into())),
        ])),
        limits: Some(BTreeMap::from([("memory".into(), Quantity("8Gi".into()))])),
        ..Default::default()
    });
    let sts = test_sts(&wp);
    let pod = sts.spec.unwrap().template.spec.unwrap();

    let resources = pod.containers[0].resources.as_ref().unwrap();
    let limits = resources.limits.as_ref().unwrap();
    let requests = resources.requests.as_ref().unwrap();

    // Operator's memory limit preserved.
    assert_eq!(limits.get("memory").map(|q| q.0.as_str()), Some("8Gi"));
    // FUSE device merged in alongside.
    assert_eq!(
        limits.get("smarter-devices/fuse").map(|q| q.0.as_str()),
        Some("1")
    );
    // Operator's cpu/memory requests preserved.
    assert_eq!(requests.get("cpu").map(|q| q.0.as_str()), Some("2"));
    assert_eq!(requests.get("memory").map(|q| q.0.as_str()), Some("4Gi"));
}

// r[verify sec.pod.host-users-false]
#[test]
fn statefulset_host_users_false_when_unprivileged() {
    // hostUsers:false → K8s user-namespace isolation. Container UIDs
    // remapped to unprivileged host UIDs; CAP_SYS_ADMIN applies only
    // within the user namespace. The LOAD-BEARING defense-in-depth
    // layer (ADR-012 §User Namespace Isolation).
    let wp = test_wp();
    let sts = test_sts(&wp);
    let pod = sts.spec.unwrap().template.spec.unwrap();
    assert_eq!(
        pod.host_users,
        Some(false),
        "non-privileged → user-namespace isolation active"
    );
}

// r[verify sec.pod.host-users-false]
// r[verify ctrl.crd.host-users-network-exclusive]
#[test]
fn host_users_suppressed_when_host_network() {
    // K8s admission rejects hostUsers:false + hostNetwork:true (kubelet:
    // "hostUsers=false is not allowed when hostNetwork is set" — userns
    // UID remap is incompatible with the host netns). The CRD CEL rule
    // stops NEW specs from landing this combo; THIS test proves the
    // builder also suppresses hostUsers for OLD specs that predate the
    // CEL rule (CRD upgrades don't re-validate existing CRs).
    //
    // privileged=None (the default) → non-privileged path would normally
    // set hostUsers:false. hostNetwork:true must override that to None.
    let mut wp = test_wp();
    wp.spec.host_network = Some(true);
    wp.spec.privileged = None;
    let sts = test_sts(&wp);
    let pod = sts.spec.unwrap().template.spec.unwrap();

    assert_eq!(
        pod.host_users, None,
        "hostUsers must be suppressed when hostNetwork:true \
         (K8s admission rejects the combo)"
    );
    // Sanity: hostNetwork actually made it to the pod spec (the
    // gate checks wp.spec.host_network, not pod.host_network — this
    // confirms the field propagated).
    assert_eq!(pod.host_network, Some(true));
}

// r[verify sec.pod.host-users-false]
#[test]
fn host_users_false_when_neither_escape_hatch() {
    // Positive control: when NEITHER escape hatch is active
    // (privileged unset/false, hostNetwork unset/false), hostUsers
    // MUST be Some(false). This guards against the hostNetwork
    // suppression over-firing — if the gate reads `host_network ==
    // None` instead of `!= Some(true)`, an EXPLICIT Some(false)
    // would incorrectly suppress. Also catches a regression where
    // the !privileged check gets removed.
    //
    // Distinct from statefulset_host_users_false_when_unprivileged
    // above: that uses test_wp() defaults (host_network=None); this
    // explicitly exercises Some(false) to prove the gate is
    // value-sensitive not presence-sensitive.
    let mut wp = test_wp();
    wp.spec.host_network = Some(false);
    wp.spec.privileged = None;
    let sts = test_sts(&wp);
    let pod = sts.spec.unwrap().template.spec.unwrap();

    assert_eq!(
        pod.host_users,
        Some(false),
        "default path (no escape hatch) must set hostUsers:false — \
         suppression over-fired on hostNetwork:Some(false)"
    );
    // host_network Some(false) → pod spec None via .filter(|&h| h).
    // Not load-bearing for this test's hostUsers claim, but confirms
    // the fixture isn't accidentally hostNetwork:true.
    assert_eq!(pod.host_network, None);
}

// r[verify sec.pod.host-users-false]
// r[verify sec.pod.fuse-device-plugin]
#[test]
fn statefulset_privileged_escape_hatch_uses_hostpath() {
    // privileged=true → hostPath /dev/fuse fallback. No device
    // plugin resource, no hostUsers:false (both incompatible with
    // privileged containers). This is the k3s/kind escape hatch.
    let mut wp = test_wp();
    wp.spec.privileged = Some(true);
    let sts = test_sts(&wp);
    let pod = sts.spec.unwrap().template.spec.unwrap();

    // hostPath /dev/fuse volume present (escape hatch).
    let fuse_vol = pod
        .volumes
        .as_ref()
        .unwrap()
        .iter()
        .find(|v| v.name == "dev-fuse")
        .expect("/dev/fuse hostPath volume (privileged escape hatch)");
    let hp = fuse_vol.host_path.as_ref().expect("hostPath");
    assert_eq!(hp.path, "/dev/fuse");
    assert_eq!(hp.type_, Some("CharDevice".into()));

    // Mount present too.
    let mount = pod.containers[0]
        .volume_mounts
        .as_ref()
        .unwrap()
        .iter()
        .find(|m| m.name == "dev-fuse")
        .expect("dev-fuse mount");
    assert_eq!(mount.mount_path, "/dev/fuse");

    // hostUsers NOT set (privileged containers can't be user-namespaced).
    assert_eq!(
        pod.host_users, None,
        "privileged escape hatch skips hostUsers:false"
    );

    // NO device plugin resource (privileged bypasses device cgroup).
    // If operator set resources, pass through unchanged; here test_wp
    // has resources=None so the container resources should be None.
    assert!(
        pod.containers[0].resources.is_none(),
        "privileged path doesn't inject FUSE device resource"
    );
}

#[test]
fn statefulset_overlays_volume_mounted() {
    // RIO_OVERLAY_BASE_DIR points to /var/rio/overlays. If
    // there's no volume mount for it, it lands on the
    // container's root filesystem — which is overlayfs.
    // Overlayfs-as-upperdir can't create trusted.* xattrs →
    // every overlay mount fails with EINVAL. Regression guard.
    let wp = test_wp();
    let sts = test_sts(&wp);
    let pod = sts.spec.unwrap().template.spec.unwrap();

    let vol = pod
        .volumes
        .unwrap()
        .into_iter()
        .find(|v| v.name == "overlays")
        .expect("overlays volume must exist");
    assert!(
        vol.empty_dir.is_some(),
        "emptyDir → node's real disk (ext4/xfs with trusted.* xattr support)"
    );

    let mount = pod.containers[0]
        .volume_mounts
        .as_ref()
        .unwrap()
        .iter()
        .find(|m| m.name == "overlays")
        .expect("overlays volumeMount");
    assert_eq!(mount.mount_path, "/var/rio/overlays");
}

// r[verify ctrl.drain.disruption-target]
/// DisruptionTarget filter: Pod with `conditions[DisruptionTarget]=
/// True` → `Some(name)`. Anything else → `None`.
///
/// Tests the PURE filter, not the watcher loop (that's K8s-API
/// machinery tested at the VM tier). The loop calls
/// `admin.drain_worker(DrainWorkerRequest { force: true, ... })`
/// when this filter returns `Some` — the prod `force: true` caller
/// the 4 lying comments have been asserting exists.
#[test]
fn disruption_filter_true_returns_name() {
    use super::disruption::is_disruption_target;
    use k8s_openapi::api::core::v1::{PodCondition, PodStatus};

    let mut pod = Pod::default();
    pod.metadata.name = Some("default-workers-3".into());
    pod.status = Some(PodStatus {
        conditions: Some(vec![
            // Ready=True alongside DisruptionTarget — normal for a
            // pod that's running but about to be evicted. Filter
            // must NOT be distracted by other conditions.
            PodCondition {
                type_: "Ready".into(),
                status: "True".into(),
                ..Default::default()
            },
            PodCondition {
                type_: "DisruptionTarget".into(),
                status: "True".into(),
                reason: Some("EvictionByEvictionAPI".into()),
                message: Some("Eviction API: evicting pod to free resources".into()),
                ..Default::default()
            },
        ]),
        ..Default::default()
    });

    assert_eq!(is_disruption_target(&pod), Some("default-workers-3"));
}

#[test]
fn disruption_filter_false_or_absent_returns_none() {
    use super::disruption::is_disruption_target;
    use k8s_openapi::api::core::v1::{PodCondition, PodStatus};

    // No status at all (Pending pod, not yet scheduled).
    let mut no_status = Pod::default();
    no_status.metadata.name = Some("pending-0".into());
    assert_eq!(is_disruption_target(&no_status), None);

    // Status present, conditions=None. Rare but modeled as Option.
    let mut no_conds = Pod::default();
    no_conds.metadata.name = Some("boot-0".into());
    no_conds.status = Some(PodStatus::default());
    assert_eq!(is_disruption_target(&no_conds), None);

    // DisruptionTarget=False. K8s may briefly set this during an
    // eviction-probe that gets denied by the PDB (no budget). We
    // MUST NOT preempt — the pod isn't actually evicting.
    let mut denied = Pod::default();
    denied.metadata.name = Some("safe-0".into());
    denied.status = Some(PodStatus {
        conditions: Some(vec![PodCondition {
            type_: "DisruptionTarget".into(),
            status: "False".into(),
            ..Default::default()
        }]),
        ..Default::default()
    });
    assert_eq!(is_disruption_target(&denied), None);

    // Only Ready condition, no DisruptionTarget. The common case
    // (99% of watcher events are normal pod lifecycle transitions).
    let mut healthy = Pod::default();
    healthy.metadata.name = Some("worker-0".into());
    healthy.status = Some(PodStatus {
        conditions: Some(vec![PodCondition {
            type_: "Ready".into(),
            status: "True".into(),
            ..Default::default()
        }]),
        ..Default::default()
    });
    assert_eq!(is_disruption_target(&healthy), None);
}

// r[verify ctrl.pdb.workers]
#[test]
fn pdb_has_correct_selector_and_max_unavailable() {
    // build_pdb produces maxUnavailable=1 with the SAME selector
    // as the STS → matches worker pods. ownerRef set so GC on
    // WorkerPool delete takes the PDB too.
    let wp = test_wp();
    let oref = wp.controller_owner_ref(&()).unwrap();
    let pdb = build_pdb(&wp, oref.clone());

    // Name: <pool>-pdb
    assert_eq!(pdb.metadata.name, Some("test-pool-pdb".into()));
    // ownerRef: controller=true for GC.
    let orefs = pdb.metadata.owner_references.expect("ownerRef set");
    assert_eq!(orefs.len(), 1);
    assert_eq!(orefs[0].kind, "WorkerPool");
    assert_eq!(orefs[0].controller, Some(true));

    let spec = pdb.spec.expect("spec");
    // maxUnavailable=1: at most one worker evicted at a time
    // during node drain. Builds on the evicting pod get
    // reassigned (DrainWorker force); rest keep working.
    assert_eq!(
        spec.max_unavailable,
        Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(1))
    );
    // minAvailable NOT set: maxUnavailable is stable regardless
    // of scale (works for 2 replicas or 200); minAvailable
    // would need to track spec.replicas.min.
    assert!(spec.min_available.is_none());

    // Selector matches pod labels (same as STS). If these
    // diverge, PDB protects nothing.
    let selector = spec
        .selector
        .expect("selector")
        .match_labels
        .expect("labels");
    assert_eq!(
        selector.get("rio.build/pool"),
        Some(&"test-pool".to_string())
    );
    assert_eq!(
        selector.get("app.kubernetes.io/name"),
        Some(&"rio-worker".to_string())
    );
}

#[test]
fn statefulset_tls_secret_mounted_when_set() {
    // spec.tlsSecretName set → volume + mount + 3 RIO_TLS__* env
    // vars. Unset → none of these (plaintext mode). Both paths
    // tested: the base test_wp has it unset; this test sets it.
    let mut wp = test_wp();
    wp.spec.tls_secret_name = Some("rio-worker-tls".into());
    let sts = test_sts(&wp);
    let pod = sts.spec.unwrap().template.spec.unwrap();

    // Volume: Secret source with the configured name.
    let tls_vol = pod
        .volumes
        .as_ref()
        .unwrap()
        .iter()
        .find(|v| v.name == "tls")
        .expect("tls volume should exist when tlsSecretName set");
    assert_eq!(
        tls_vol.secret.as_ref().unwrap().secret_name,
        Some("rio-worker-tls".into())
    );

    // Mount: /etc/rio/tls, read-only. cert-manager's Secret keys
    // (tls.crt, tls.key, ca.crt) appear as files here.
    let container = &pod.containers[0];
    let mount = container
        .volume_mounts
        .as_ref()
        .unwrap()
        .iter()
        .find(|m| m.name == "tls")
        .expect("tls mount");
    assert_eq!(mount.mount_path, "/etc/rio/tls");
    assert_eq!(mount.read_only, Some(true));

    // Env vars: the three RIO_TLS__* pointing at the mount.
    // Double-underscore is figment nesting (tls.cert_path).
    let envs: std::collections::HashMap<_, _> = container
        .env
        .as_ref()
        .unwrap()
        .iter()
        .filter_map(|e| e.value.as_ref().map(|v| (e.name.as_str(), v.as_str())))
        .collect();
    assert_eq!(
        envs.get("RIO_TLS__CERT_PATH"),
        Some(&"/etc/rio/tls/tls.crt")
    );
    assert_eq!(envs.get("RIO_TLS__KEY_PATH"), Some(&"/etc/rio/tls/tls.key"));
    assert_eq!(envs.get("RIO_TLS__CA_PATH"), Some(&"/etc/rio/tls/ca.crt"));
}

#[test]
fn statefulset_no_tls_when_unset() {
    // The default test_wp has tls_secret_name=None. No tls
    // volume, no mount, no RIO_TLS__* env — clean plaintext.
    let wp = test_wp();
    let sts = test_sts(&wp);
    let pod = sts.spec.unwrap().template.spec.unwrap();

    assert!(
        !pod.volumes
            .as_ref()
            .unwrap()
            .iter()
            .any(|v| v.name == "tls"),
        "no tls volume when tlsSecretName unset"
    );
    let container = &pod.containers[0];
    assert!(
        !container
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .any(|m| m.name == "tls"),
    );
    assert!(
        !container
            .env
            .as_ref()
            .unwrap()
            .iter()
            .any(|e| e.name.starts_with("RIO_TLS__")),
        "no RIO_TLS__* env when tlsSecretName unset"
    );
}

#[test]
fn statefulset_termination_grace() {
    let wp = test_wp();
    let sts = test_sts(&wp);
    let pod = sts.spec.unwrap().template.spec.unwrap();
    assert_eq!(
        pod.termination_grace_period_seconds,
        Some(7200),
        "2h for long nix builds (worker drain sequence runs within this)"
    );
    assert_eq!(
        pod.automount_service_account_token,
        Some(false),
        "workers use gRPC, not K8s API — no SA token needed"
    );
}

#[test]
fn statefulset_env_vars() {
    let wp = test_wp();
    let sts = test_sts(&wp);

    let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    let envs: BTreeMap<String, String> = container
        .env
        .as_ref()
        .unwrap()
        .iter()
        .filter_map(|e| e.value.clone().map(|v| (e.name.clone(), v)))
        .collect();

    assert_eq!(envs.get("RIO_SCHEDULER_ADDR"), Some(&"sched:9001".into()));
    assert_eq!(
        envs.get("RIO_STORE_ADDR"),
        Some(&"store:9002".into()),
        "from ctx.store_addr (NOT derived from scheduler — \
         different hostnames in kustomize base)"
    );
    assert_eq!(envs.get("RIO_MAX_BUILDS"), Some(&"4".into()));
    assert_eq!(envs.get("RIO_SIZE_CLASS"), Some(&"small".into()));
    assert_eq!(
        envs.get("RIO_FUSE_CACHE_SIZE_GB"),
        Some(&"50".into()),
        "50Gi parsed to 50 GB (binary → binary, integer)"
    );
    // systems + features: comma-sep strings. fixture wp has
    // systems=["x86_64-linux"], features=["kvm"]. CRD defines
    // them → reconciler passes them as env → worker's comma_vec
    // deserialize splits them.
    assert_eq!(
        envs.get("RIO_SYSTEMS"),
        Some(&"x86_64-linux".into()),
        "systems comma-joined → worker's comma_vec deserialize"
    );
    assert_eq!(
        envs.get("RIO_FEATURES"),
        Some(&"kvm".into()),
        "features comma-joined → worker's comma_vec deserialize"
    );

    // Worker tuning knobs (plan 21 Batch E): NOT injected when None
    // in the spec — figment layering means the worker's compiled-in
    // default wins. Injecting would pin the default at controller-
    // build time instead of worker-build time.
    assert!(
        !envs.contains_key("RIO_FUSE_THREADS"),
        "unset in spec → not injected → worker default"
    );
    assert!(!envs.contains_key("RIO_FUSE_PASSTHROUGH"));
    assert!(!envs.contains_key("RIO_DAEMON_TIMEOUT_SECS"));
    assert!(!envs.contains_key("RIO_BLOOM_EXPECTED_ITEMS"));

    // RIO_WORKER_ID uses fieldRef, not value — check separately.
    let worker_id = container
        .env
        .as_ref()
        .unwrap()
        .iter()
        .find(|e| e.name == "RIO_WORKER_ID")
        .unwrap();
    assert_eq!(worker_id.value, None, "not a literal value");
    assert_eq!(
        worker_id
            .value_from
            .as_ref()
            .unwrap()
            .field_ref
            .as_ref()
            .unwrap()
            .field_path,
        "metadata.name",
        "downward API: pod name (StatefulSet ordinal, unique)"
    );
}

/// Worker tuning knobs: when set in the spec, they ARE injected.
/// Complements the `statefulset_env_vars` assertions above (which
/// check the unset → not-injected case).
#[test]
fn statefulset_worker_knobs_injected_when_set() {
    let mut wp = test_wp();
    wp.spec.fuse_threads = Some(8);
    wp.spec.fuse_passthrough = Some(false);
    wp.spec.daemon_timeout_secs = Some(14400);
    let sts = test_sts(&wp);

    let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    let envs: BTreeMap<String, String> = container
        .env
        .as_ref()
        .unwrap()
        .iter()
        .filter_map(|e| e.value.clone().map(|v| (e.name.clone(), v)))
        .collect();

    assert_eq!(envs.get("RIO_FUSE_THREADS"), Some(&"8".into()));
    assert_eq!(
        envs.get("RIO_FUSE_PASSTHROUGH"),
        Some(&"false".into()),
        "figment bool parse accepts true/false (rio-common config.rs test)"
    );
    assert_eq!(envs.get("RIO_DAEMON_TIMEOUT_SECS"), Some(&"14400".into()));
}

// r[verify ctrl.pool.bloom-knob]
/// Bloom capacity knob: when set in the spec, `RIO_BLOOM_EXPECTED_ITEMS`
/// is injected. The env var is what P0288 wired into worker figment
/// layering (`config.rs:138`). u64→string because figment parses the
/// env string as usize on the worker side.
#[test]
fn bloom_expected_items_env_injected_when_set() {
    let mut wp = test_wp();
    wp.spec.bloom_expected_items = Some(200_000);
    let sts = test_sts(&wp);

    let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    let envs: BTreeMap<String, String> = container
        .env
        .as_ref()
        .unwrap()
        .iter()
        .filter_map(|e| e.value.clone().map(|v| (e.name.clone(), v)))
        .collect();

    assert_eq!(
        envs.get("RIO_BLOOM_EXPECTED_ITEMS"),
        Some(&"200000".into()),
        "bloom_expected_items set → inject env var for worker figment"
    );
}

/// Bloom capacity knob: unset in the spec → NOT injected. The worker's
/// `Config::default` (None → 50k compile-time fallback in Cache::new)
/// wins. Same drift-avoidance as the other optional tuning knobs.
#[test]
fn bloom_expected_items_env_not_injected_when_unset() {
    let wp = test_wp(); // spec.bloom_expected_items = None
    let sts = test_sts(&wp);

    let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    let envs: BTreeMap<String, String> = container
        .env
        .as_ref()
        .unwrap()
        .iter()
        .filter_map(|e| e.value.clone().map(|v| (e.name.clone(), v)))
        .collect();

    assert!(
        !envs.contains_key("RIO_BLOOM_EXPECTED_ITEMS"),
        "unset → not injected → worker compile-time 50k default wins"
    );
}

#[test]
fn statefulset_replicas_starts_at_min() {
    let wp = test_wp();
    let sts = test_sts(&wp);
    assert_eq!(
        sts.spec.unwrap().replicas,
        Some(2),
        "initial create: set to spec.replicas.min"
    );
}

/// replicas=None → field omitted from the SSA patch → the
/// autoscaler's field-manager ownership is preserved.
/// Without this, every reconcile would revert the autoscaler's
/// scaling decision to min (SSA .force() takes ownership of
/// every field in the patch).
///
/// k8s-openapi's custom Serialize impl skips Option::None
/// fields, so None → field absent → SSA leaves it alone.
#[test]
fn statefulset_replicas_omitted_when_none() {
    let wp = test_wp();
    let sts = build_statefulset(
        &wp,
        wp.controller_owner_ref(&()).unwrap(),
        &test_sched_addrs(),
        "store:9002",
        None,
    )
    .unwrap();

    assert_eq!(
        sts.spec.as_ref().unwrap().replicas,
        None,
        "subsequent reconciles: replicas=None → autoscaler owns it"
    );

    // And the crucial part: serialized JSON doesn't contain
    // the field. SSA semantics: absent field = "I don't
    // manage this." Verifies k8s-openapi's skip-None-on-
    // serialize behavior hasn't regressed.
    let json = serde_json::to_string(&sts).unwrap();
    assert!(
        !json.contains("\"replicas\""),
        "replicas must be absent from serialized JSON for SSA to \
         preserve autoscaler ownership. Found in: {json}"
    );
}

#[test]
fn statefulset_image_pull_policy_passthrough() {
    // None stays None — K8s applies its tag-based default
    // (IfNotPresent for non-:latest, Always for :latest).
    let wp = test_wp();
    let sts = test_sts(&wp);
    let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    assert_eq!(container.image_pull_policy, None);

    // Explicit value passes through. Airgap k3s/kind need
    // IfNotPresent or Never to use ctr-imported images.
    let mut wp = test_wp();
    wp.spec.image_pull_policy = Some("IfNotPresent".into());
    let sts = test_sts(&wp);
    let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    assert_eq!(container.image_pull_policy.as_deref(), Some("IfNotPresent"));
}

// ----- quantity parsing -----

#[test]
fn quantity_binary_suffix() {
    assert_eq!(parse_quantity_to_gb("50Gi").unwrap(), 50);
    assert_eq!(parse_quantity_to_gb("100Gi").unwrap(), 100);
    assert_eq!(
        parse_quantity_to_gb("1Ti").unwrap(),
        1024,
        "1 TiB = 1024 GiB"
    );
    // Mi rounds DOWN to GB. 2048 MiB = 2 GiB exactly.
    assert_eq!(parse_quantity_to_gb("2048Mi").unwrap(), 2);
    // 1500 MiB = 1.46 GiB → rounds down to 1.
    assert_eq!(
        parse_quantity_to_gb("1500Mi").unwrap(),
        1,
        "rounds DOWN (cache limit: better under than over → kubelet eviction)"
    );
}

#[test]
fn quantity_decimal_suffix() {
    // 100G = 100 * 10^9 bytes = 93.13 GiB → 93.
    assert_eq!(parse_quantity_to_gb("100G").unwrap(), 93);
    // 50G = 46.56 GiB → 46.
    assert_eq!(parse_quantity_to_gb("50G").unwrap(), 46);
}

#[test]
fn quantity_raw_bytes() {
    // 107374182400 = 100 * 1024^3 = 100 GiB exactly.
    assert_eq!(parse_quantity_to_gb("107374182400").unwrap(), 100);
}

#[test]
fn quantity_suffix_order_gi_before_g() {
    // If we matched "G" before "Gi", "100Gi" would strip "G"
    // leaving "100i" which doesn't parse. The SUFFIXES const
    // is ordered to prevent this. If someone reorders it,
    // this test catches it.
    assert_eq!(
        parse_quantity_to_gb("100Gi").unwrap(),
        100,
        "Gi matched, not G"
    );
}

#[test]
fn quantity_invalid_rejected() {
    assert!(matches!(
        parse_quantity_to_gb("not-a-number"),
        Err(Error::InvalidSpec(_))
    ));
    assert!(matches!(
        parse_quantity_to_gb("100Pi"),
        Err(Error::InvalidSpec(_))
    ));
    // Empty
    assert!(matches!(
        parse_quantity_to_gb(""),
        Err(Error::InvalidSpec(_))
    ));
    // Negative and non-finite rejected.
    assert!(matches!(
        parse_quantity_to_gb("-5Gi"),
        Err(Error::InvalidSpec(_))
    ));
    assert!(matches!(
        parse_quantity_to_gb("infGi"),
        Err(Error::InvalidSpec(_))
    ));
}

/// Decimal quantities (K8s Quantity allows them). u64 parse would
/// reject "1.5Gi" even though it's a valid K8s Quantity; f64 parse
/// + floor-to-u64 accepts it.
#[test]
fn quantity_decimal_fraction() {
    // 1.5 GiB = 1.5 * 1024^3 = 1610612736 bytes → 1 GB (floor).
    assert_eq!(parse_quantity_to_gb("1.5Gi").unwrap(), 1);
    // 2.5 GiB → 2 GB (floor).
    assert_eq!(parse_quantity_to_gb("2.5Gi").unwrap(), 2);
    // 0.5 GiB = 512 MiB → 0 GB (floor). Operator probably wants
    // at least 1 GB but the spec says 0.5Gi so we honor it.
    assert_eq!(parse_quantity_to_gb("0.5Gi").unwrap(), 0);
    // 1.5Ti = 1536 GiB.
    assert_eq!(parse_quantity_to_gb("1.5Ti").unwrap(), 1536);
}

#[test]
fn quantity_invalidspec_from_statefulset() {
    let mut wp = test_wp();
    wp.spec.fuse_cache_size = "garbage".into();
    let result = build_statefulset(
        &wp,
        wp.controller_owner_ref(&()).unwrap(),
        &test_sched_addrs(),
        "store:9002",
        Some(1),
    );
    match result {
        Err(Error::InvalidSpec(msg)) => {
            assert!(msg.contains("fuseCacheSize"));
            assert!(msg.contains("garbage"));
        }
        other => panic!("expected InvalidSpec with helpful message, got {other:?}"),
    }
}

// =========================================================
// Mock-apiserver integration tests
//
// These test the WIRING: apply() calls Service PATCH then
// StatefulSet PATCH then WorkerPool/status PATCH, in that
// order, with server-side-apply params. The builder tests
// above cover WHAT gets patched; these cover WHEN/HOW.
// =========================================================

fn test_ctx(client: kube::Client) -> Arc<Ctx> {
    let recorder = kube::runtime::events::Recorder::new(
        client.clone(),
        kube::runtime::events::Reporter {
            controller: "rio-controller-test".into(),
            instance: None,
        },
    );
    // Unreachable --- apply() doesn't touch the scheduler, and
    // cleanup() treats RPC failure as best-effort skip. connect_lazy
    // defers the TCP connect until the first RPC, which fails fast on
    // port 1 (never listened on).
    let dead_ch = tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
    Arc::new(Ctx {
        client,
        admin: rio_proto::AdminServiceClient::new(dead_ch),
        scheduler_addr: "http://127.0.0.1:1".into(),
        store_addr: "http://127.0.0.1:1".into(),
        scheduler_balance_host: None,
        scheduler_balance_port: 9001,
        recorder,
    })
}

/// apply() hits Service → StatefulSet → WorkerPool/status,
/// server-side apply all three. Wrong order or missing call
/// → verifier panics.
#[tokio::test]
async fn apply_patches_in_order() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);
    let wp = Arc::new(test_wp());

    // sts_exists=false: first-create path (STS GET → 404 →
    // initial_replicas=Some(min)).
    let guard = verifier.run(apply_ok_scenarios("test-pool", "rio", 2, false));

    // Call apply() directly (not reconcile() — finalizer()
    // would do its own GET + PATCH of metadata.finalizers
    // first, which adds scenarios we don't care about here).
    let action = apply(wp, &ctx).await.expect("apply succeeds");

    // Requeue in 5m — the fallback re-reconcile.
    assert_eq!(action, Action::requeue(Duration::from_secs(300)));

    guard.verified().await;
}

/// Server-side apply params in the query string. SSA is
/// what makes reconcile idempotent — if we accidentally
/// switch to PUT or merge-patch, this fails.
#[tokio::test]
async fn apply_uses_server_side_apply() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);
    let wp = Arc::new(test_wp());

    // Custom scenarios that assert fieldManager=rio-controller
    // in the query string (SSA puts it there; merge patch
    // doesn't). path_contains is a substring match so
    // embedding the query param works.
    let guard = verifier.run(vec![
        Scenario::ok(
            http::Method::PATCH,
            "fieldManager=rio-controller",
            serde_json::json!({"metadata":{"name":"test-pool-workers"}}).to_string(),
        ),
        // PDB PATCH — same fieldManager assertion.
        Scenario::ok(
            http::Method::PATCH,
            "fieldManager=rio-controller",
            serde_json::json!({"metadata":{"name":"test-pool-pdb"}}).to_string(),
        ),
        // GET before STS PATCH (replicas-ownership check).
        // 404 → first-create → replicas set to min.
        Scenario {
            method: http::Method::GET,
            path_contains: "/statefulsets/test-pool-workers",
            body_contains: None,
            status: 404,
            body_json: serde_json::json!({
                "kind":"Status","apiVersion":"v1",
                "status":"Failure","reason":"NotFound","code":404,
            })
            .to_string(),
        },
        Scenario::ok(
            http::Method::PATCH,
            "fieldManager=rio-controller",
            serde_json::json!({
                "metadata":{"name":"test-pool-workers"},
                "status":{"replicas":0}
            })
            .to_string(),
        ),
        Scenario::ok(
            http::Method::PATCH,
            "workerpools/test-pool/status",
            serde_json::json!({
                "apiVersion":"rio.build/v1alpha1","kind":"WorkerPool",
                "metadata":{"name":"test-pool"},
                "spec":{"replicas":{"min":1,"max":1},"autoscaling":{"metric":"x","targetValue":1},
                    "maxConcurrentBuilds":1,"fuseCacheSize":"1Gi","features":[],
                    "systems":["x"],"sizeClass":"x","image":"x"}
            })
            .to_string(),
        ),
    ]);

    apply(wp, &ctx).await.expect("apply succeeds");
    guard.verified().await;
}

/// cleanup with STS already gone → 404 → short-circuit.
/// Proves the "STS not found → done" branch doesn't hang
/// on the poll loop.
#[tokio::test]
async fn cleanup_tolerates_missing_statefulset() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);
    let wp = Arc::new(test_wp());

    // Phase 1: pod list (empty — no pods to drain). Phase
    // 2: STS scale PATCH → 404. cleanup() short-circuits.
    // No phase 3 GET.
    let guard = verifier.run(vec![
        // Pod list. Empty — cleanup skips the DrainWorker
        // loop (scheduler unreachable anyway with port 1).
        Scenario::ok(
            http::Method::GET,
            "/pods?&labelSelector=rio.build",
            serde_json::json!({"apiVersion":"v1","kind":"PodList","items":[]}).to_string(),
        ),
        // STS PATCH → 404. K8s 404 body is a Status object.
        Scenario {
            method: http::Method::PATCH,
            path_contains: "/statefulsets/test-pool-workers",
            body_contains: None,
            status: 404,
            body_json: serde_json::json!({
                "apiVersion":"v1","kind":"Status","status":"Failure",
                "reason":"NotFound","code":404,
                "message":"statefulsets.apps \"test-pool-workers\" not found"
            })
            .to_string(),
        },
    ]);

    let action = cleanup(wp, &ctx).await.expect("cleanup tolerates 404");
    assert_eq!(action, Action::await_change());

    guard.verified().await;
}

/// cleanup poll loop: first GET shows replicas=1, second
/// shows 0 → break. Proves we poll `status.replicas` not
/// `readyReplicas` (the latter would see 0 immediately on
/// a terminating pod — wrong).
#[tokio::test(start_paused = true)]
async fn cleanup_polls_until_replicas_zero() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);
    let wp = Arc::new(test_wp());

    // start_paused + tokio::time means the 5s sleep between
    // polls auto-advances. Without it, this test would take
    // 5 real seconds.
    let sts_with = |replicas: i32| {
        serde_json::json!({
            "apiVersion":"apps/v1","kind":"StatefulSet",
            "metadata":{"name":"test-pool-workers"},
            "status":{"replicas": replicas, "readyReplicas": 0}
        })
        .to_string()
    };

    let guard = verifier.run(vec![
        Scenario::ok(
            http::Method::GET,
            "/pods?&labelSelector=rio.build",
            serde_json::json!({"apiVersion":"v1","kind":"PodList","items":[]}).to_string(),
        ),
        Scenario::ok(
            http::Method::PATCH,
            "/statefulsets/test-pool-workers",
            sts_with(1),
        ),
        // First poll: replicas=1 (pod still terminating).
        // readyReplicas=0 but we DON'T break — proves we
        // read the right field.
        Scenario::ok(
            http::Method::GET,
            "/statefulsets/test-pool-workers",
            sts_with(1),
        ),
        // Second poll: replicas=0. Break.
        Scenario::ok(
            http::Method::GET,
            "/statefulsets/test-pool-workers",
            sts_with(0),
        ),
    ]);

    let action = cleanup(wp, &ctx).await.expect("cleanup completes");
    assert_eq!(action, Action::await_change());

    guard.verified().await;
}

/// migrate_finalizer with stale resourceVersion gets 409, not stomp.
///
/// Scenario: we read finalizers=[OLD], meanwhile a foreign controller
/// adds `example.com/cleanup` (bumping rv). Our merge-patch with the
/// stale resourceVersion MUST be rejected (409 Conflict) and surface
/// as `Error::Conflict` — not succeed and silently drop the foreign
/// finalizer.
///
/// Before the fix: the merge-patch omitted resourceVersion, so the
/// apiserver accepted it unconditionally and set finalizers=[NEW],
/// stomping `example.com/cleanup` that arrived in the window.
///
/// Mutation check: remove `"resourceVersion": rv` from the patch
/// body in migrate_finalizer → the `body_contains` assertion below
/// fails (the mock doesn't see rv in the PATCH body).
#[tokio::test]
async fn migrate_finalizer_conflicts_on_stale_resource_version() {
    let (client, verifier) = ApiServerVerifier::new();
    let api: Api<WorkerPool> = Api::namespaced(client, "rio");

    // The verifier asserts TWO things here:
    //   1. The PATCH body contains `"resourceVersion":"42"` — proves
    //      migrate_finalizer carries the rv it read from `obj`. This
    //      is what makes the apiserver's 409 meaningful (without rv,
    //      merge-patch always succeeds).
    //   2. When the apiserver returns 409, the code maps it to
    //      `Error::Conflict` — proving the error path requeues
    //      instead of bubbling as a generic kube error.
    let guard = verifier.run(vec![Scenario {
        method: http::Method::PATCH,
        path_contains: "/workerpools/test-pool",
        // serde_json emits compact JSON — no space after colon.
        // Asserting the EXACT rv=42 (not just "resourceVersion")
        // proves we read it from obj.meta(), not a hardcoded value.
        body_contains: Some(r#""resourceVersion":"42""#),
        status: 409,
        body_json: serde_json::json!({
            "kind": "Status", "apiVersion": "v1",
            "status": "Failure", "reason": "Conflict", "code": 409,
            "message": "the object has been modified; please apply \
                        your changes to the latest version and try again",
        })
        .to_string(),
    }]);

    // WorkerPool with OLD_FINALIZER + rv=42. This is the STALE
    // snapshot — the "real" apiserver state has rv=43 after a
    // foreign controller added its finalizer.
    let mut wp = test_wp();
    wp.metadata.finalizers = Some(vec![OLD_FINALIZER.into()]);
    wp.metadata.resource_version = Some("42".into());

    let result = migrate_finalizer(&api, &wp, OLD_FINALIZER, FINALIZER).await;

    assert!(
        matches!(result, Err(Error::Conflict(_))),
        "migrate_finalizer should map 409 → Error::Conflict, got {result:?}"
    );

    guard.verified().await;
}

/// Happy path: old finalizer present, rv matches, patch succeeds.
///
/// Proves the no-conflict path still works after adding the
/// resourceVersion lock — the rv is carried in the body AND the
/// 200 response flows through to `Ok(Some(Action::await_change()))`.
#[tokio::test]
async fn migrate_finalizer_happy_path() {
    let (client, verifier) = ApiServerVerifier::new();
    let api: Api<WorkerPool> = Api::namespaced(client, "rio");

    // The mock's 200 stands in for the apiserver accepting the rv.
    // body_contains asserts rv is SENT (same mutation-check
    // coverage as the conflict test).
    let guard = verifier.run(vec![Scenario {
        method: http::Method::PATCH,
        path_contains: "/workerpools/test-pool",
        body_contains: Some(r#""resourceVersion":"7""#),
        status: 200,
        // Response body: the patched WorkerPool. migrate_finalizer
        // doesn't inspect it (just checks for a 200 vs 409), but
        // kube's deserializer needs a valid WorkerPool shape.
        body_json: serde_json::json!({
            "apiVersion": "rio.build/v1alpha1",
            "kind": "WorkerPool",
            "metadata": {
                "name": "test-pool", "namespace": "rio",
                "resourceVersion": "8",
                "finalizers": [FINALIZER],
            },
            "spec": {
                "replicas": { "min": 1, "max": 1 },
                "autoscaling": { "metric": "x", "targetValue": 1 },
                "maxConcurrentBuilds": 1,
                "fuseCacheSize": "1Gi",
                "features": [],
                "systems": ["x86_64-linux"],
                "sizeClass": "small",
                "image": "x",
            },
        })
        .to_string(),
    }]);

    let mut wp = test_wp();
    wp.metadata.finalizers = Some(vec![OLD_FINALIZER.into()]);
    wp.metadata.resource_version = Some("7".into());

    let action = migrate_finalizer(&api, &wp, OLD_FINALIZER, FINALIZER)
        .await
        .expect("no-conflict path succeeds");
    assert_eq!(
        action,
        Some(Action::await_change()),
        "should short-circuit and await the watch event"
    );

    guard.verified().await;
}

/// Old finalizer absent → no patch, return None. Proves the
/// idempotent no-op path doesn't issue any apiserver call (the
/// verifier would time out if it did — zero scenarios).
#[tokio::test]
async fn migrate_finalizer_noop_when_old_absent() {
    let (client, verifier) = ApiServerVerifier::new();
    let api: Api<WorkerPool> = Api::namespaced(client, "rio");

    // No scenarios: migrate_finalizer MUST NOT call the apiserver.
    let guard = verifier.run(vec![]);

    // Only the new finalizer — migration already done.
    let mut wp = test_wp();
    wp.metadata.finalizers = Some(vec![FINALIZER.into()]);
    wp.metadata.resource_version = Some("7".into());

    let action = migrate_finalizer(&api, &wp, OLD_FINALIZER, FINALIZER)
        .await
        .expect("noop path succeeds");
    assert_eq!(action, None, "old absent → None, caller proceeds");

    guard.verified().await;
}

// -------------------------------------------------------------------
// Coverage propagation (builders.rs LLVM_PROFILE_FILE check).
//
// figment::Jail serializes env access (global mutex) so parallel
// tests don't see each other's set_env/remove_var. Same pattern
// as rio-scheduler/src/lease.rs tests.
// -------------------------------------------------------------------

#[test]
#[allow(clippy::result_large_err)] // figment::Error is large, API-fixed
fn coverage_propagated_when_controller_env_set() -> figment::error::Result<()> {
    figment::Jail::expect_with(|jail| {
        jail.set_env("LLVM_PROFILE_FILE", "/var/lib/rio/cov/ctrl-%p-%m.profraw");

        let wp = test_wp();
        let sts = test_sts(&wp);
        let spec = sts.spec.unwrap().template.spec.unwrap();
        let container = &spec.containers[0];

        // Env var injected with the canonical pod-side template (NOT
        // the controller's own — the pod writes to the same mount
        // path, but %p/%m expand inside the pod).
        let envs = container.env.as_ref().expect("env vars");
        let profile_env = envs
            .iter()
            .find(|e| e.name == "LLVM_PROFILE_FILE")
            .expect("LLVM_PROFILE_FILE env injected");
        assert_eq!(
            profile_env.value.as_deref(),
            Some("/var/lib/rio/cov/rio-%p-%m.profraw")
        );

        // Volume: hostPath to /var/lib/rio/cov on the k8s node.
        let volumes = spec.volumes.as_ref().expect("volumes");
        let cov_vol = volumes
            .iter()
            .find(|v| v.name == "cov")
            .expect("cov volume");
        let hp = cov_vol.host_path.as_ref().expect("hostPath");
        assert_eq!(hp.path, "/var/lib/rio/cov");
        assert_eq!(hp.type_.as_deref(), Some("DirectoryOrCreate"));

        // Mount at the same path inside the container.
        let mounts = container.volume_mounts.as_ref().expect("mounts");
        let cov_mount = mounts.iter().find(|m| m.name == "cov").expect("cov mount");
        assert_eq!(cov_mount.mount_path, "/var/lib/rio/cov");

        Ok(())
    });
    Ok(())
}

#[test]
#[allow(clippy::result_large_err)]
fn rust_log_propagated_verbatim() -> figment::error::Result<()> {
    figment::Jail::expect_with(|jail| {
        jail.set_env("RUST_LOG", "info,rio_worker=debug");

        let wp = test_wp();
        let sts = test_sts(&wp);
        let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];

        let envs = container.env.as_ref().expect("env vars");
        let rust_log = envs
            .iter()
            .find(|e| e.name == "RUST_LOG")
            .expect("RUST_LOG env injected");
        assert_eq!(
            rust_log.value.as_deref(),
            Some("info,rio_worker=debug"),
            "verbatim — per-crate EnvFilter directives preserved"
        );

        Ok(())
    });
    Ok(())
}

#[test]
#[allow(clippy::result_large_err)]
fn rust_log_absent_when_controller_env_unset() -> figment::error::Result<()> {
    figment::Jail::expect_with(|_jail| {
        // SAFETY: Jail's global mutex serializes env access.
        unsafe {
            std::env::remove_var("RUST_LOG");
        }

        let wp = test_wp();
        let sts = test_sts(&wp);
        let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];

        let envs = container.env.as_ref().expect("env vars");
        assert!(
            !envs.iter().any(|e| e.name == "RUST_LOG"),
            "RUST_LOG should be absent — worker falls back to binary's info default"
        );

        Ok(())
    });
    Ok(())
}

#[test]
#[allow(clippy::result_large_err)]
fn coverage_absent_when_controller_env_unset() -> figment::error::Result<()> {
    figment::Jail::expect_with(|_jail| {
        // SAFETY: Jail serializes env access via a global mutex — no
        // other thread touches LLVM_PROFILE_FILE while we're here.
        // std::env::remove_var is unsafe in Rust 2024 for the general
        // "env is process-global" race reason; Jail's lock is the
        // synchronization that makes it safe here.
        unsafe {
            std::env::remove_var("LLVM_PROFILE_FILE");
        }

        let wp = test_wp();
        let sts = test_sts(&wp);
        let spec = sts.spec.unwrap().template.spec.unwrap();
        let container = &spec.containers[0];

        // NO env var, volume, or mount — prod pods are clean.
        let envs = container.env.as_ref().expect("env vars");
        assert!(
            !envs.iter().any(|e| e.name == "LLVM_PROFILE_FILE"),
            "coverage env should be absent in normal mode"
        );
        let volumes = spec.volumes.as_ref().expect("volumes");
        assert!(
            !volumes.iter().any(|v| v.name == "cov"),
            "cov volume should be absent"
        );
        let mounts = container.volume_mounts.as_ref().expect("mounts");
        assert!(
            !mounts.iter().any(|m| m.name == "cov"),
            "cov mount should be absent"
        );

        Ok(())
    });
    Ok(())
}
