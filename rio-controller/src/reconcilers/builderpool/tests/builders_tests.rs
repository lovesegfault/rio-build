//! Job pod-spec builder coverage + quantity parsing.
//!
//! Pure struct-to-struct tests — no K8s apiserver interaction.
//! Covers what `build_pod_spec`/`parse_quantity_to_gb` PRODUCE.

use super::*;

#[test]
fn job_pod_derives_arch_node_selector_from_systems() {
    // I-098: fixture has systems=[x86_64-linux] → kubernetes.io/arch=amd64
    let wp = test_wp();
    let ns = test_pod_spec(&wp)
        .node_selector
        .expect("node_selector set (arch derived)");
    assert_eq!(
        ns.get("kubernetes.io/arch").map(String::as_str),
        Some("amd64"),
        "arch derived from systems=[x86_64-linux]"
    );

    // explicit operator override (e.g., qemu-user pool) wins
    let mut wp = test_wp();
    wp.spec.node_selector = Some(
        [("kubernetes.io/arch".to_string(), "arm64".to_string())]
            .into_iter()
            .collect(),
    );
    let ns = test_pod_spec(&wp).node_selector.unwrap();
    assert_eq!(
        ns.get("kubernetes.io/arch").map(String::as_str),
        Some("arm64"),
        "explicit nodeSelector arch is preserved"
    );
}

#[test]
fn job_pod_security_context() {
    let wp = test_wp();
    let pod = test_pod_spec(&wp);
    let container = &pod.containers[0];
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

// r[verify builder.seccomp.localhost-profile+2]
#[test]
fn seccomp_default_is_runtime_default() {
    // spec.seccompProfile=None → builder emits RuntimeDefault at
    // the POD level. NOT Unconfined — an absent seccompProfile on
    // the pod spec would be implicitly Unconfined on most runtimes.
    // The builder must always emit SOMETHING.
    let wp = test_wp();
    assert!(wp.spec.seccomp_profile.is_none(), "test_wp() baseline");
    let pod_sc = test_pod_spec(&wp)
        .security_context
        .expect("pod security_context set (privileged=None)");
    let prof = pod_sc.seccomp_profile.expect("seccompProfile set");
    assert_eq!(prof.type_, "RuntimeDefault");
    assert_eq!(prof.localhost_profile, None);
}

// r[verify builder.seccomp.localhost-profile+2]
#[test]
fn seccomp_localhost_emits_correct_security_context() {
    // spec.seccompProfile={type: Localhost, localhostProfile: ...}
    // splits across three places:
    //   - pod-level: RuntimeDefault (sandbox + initContainer can
    //     start before the profile file lands on the node)
    //   - wait-seccomp initContainer: polls hostPath for the file
    //   - worker container: Localhost (the actual enforcement)
    // See the spod-race comment in common/sts.rs.
    let mut wp = test_wp();
    wp.spec.seccomp_profile = Some(SeccompProfileKind {
        type_: "Localhost".into(),
        localhost_profile: Some("operator/rio-builder.json".into()),
    });
    let pod = test_pod_spec(&wp);

    // Pod-level: RuntimeDefault floor (NOT Localhost — sandbox
    // would fail CreateContainerConfigError if file missing).
    let pod_prof = pod
        .security_context
        .as_ref()
        .expect("pod security_context set")
        .seccomp_profile
        .as_ref()
        .expect("pod seccompProfile set");
    assert_eq!(pod_prof.type_, "RuntimeDefault");
    assert_eq!(pod_prof.localhost_profile, None);

    // wait-seccomp initContainer: present, mounts host-seccomp,
    // polls for the profile path.
    let inits = pod.init_containers.as_ref().expect("initContainers set");
    assert_eq!(inits.len(), 1);
    let wait = &inits[0];
    assert_eq!(wait.name, "wait-seccomp");
    assert_eq!(wait.image, Some(wp.spec.image.clone()));
    let cmd = wait.command.as_ref().expect("wait command");
    assert!(
        cmd.last().unwrap().contains("operator/rio-builder.json"),
        "wait loop references the requested profile path"
    );
    let wait_prof = wait
        .security_context
        .as_ref()
        .and_then(|sc| sc.seccomp_profile.as_ref())
        .expect("wait initContainer seccomp set");
    assert_eq!(wait_prof.type_, "RuntimeDefault");
    assert!(
        wait.volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .any(|m| m.name == "host-seccomp" && m.read_only == Some(true))
    );

    // host-seccomp hostPath volume present.
    assert!(pod.volumes.as_ref().unwrap().iter().any(|v| {
        v.name == "host-seccomp"
            && v.host_path
                .as_ref()
                .is_some_and(|h| h.path == "/var/lib/kubelet/seccomp")
    }));

    // Worker container: Localhost (the actual security boundary).
    let worker = pod
        .containers
        .iter()
        .find(|c| c.name == "builder")
        .expect("worker container");
    let worker_prof = worker
        .security_context
        .as_ref()
        .and_then(|sc| sc.seccomp_profile.as_ref())
        .expect("worker container seccompProfile set");
    assert_eq!(worker_prof.type_, "Localhost");
    assert_eq!(
        worker_prof.localhost_profile,
        Some("operator/rio-builder.json".into())
    );
}

#[test]
fn seccomp_non_localhost_no_init_container() {
    // RuntimeDefault/Unconfined: no wait-seccomp initContainer,
    // no host-seccomp volume. Pod-level carries the profile
    // directly (original behavior — no file dependency).
    for ty in ["RuntimeDefault", "Unconfined"] {
        let mut wp = test_wp();
        wp.spec.seccomp_profile = Some(SeccompProfileKind {
            type_: ty.into(),
            localhost_profile: None,
        });
        let pod = test_pod_spec(&wp);

        assert!(pod.init_containers.is_none(), "{ty}: no initContainer");
        assert!(
            !pod.volumes
                .as_ref()
                .unwrap()
                .iter()
                .any(|v| v.name == "host-seccomp"),
            "{ty}: no host-seccomp volume"
        );
        let pod_prof = pod
            .security_context
            .unwrap()
            .seccomp_profile
            .expect("pod seccomp set");
        assert_eq!(pod_prof.type_, ty);
        // Worker container: no container-level override.
        let worker = pod.containers.iter().find(|c| c.name == "builder").unwrap();
        assert!(
            worker
                .security_context
                .as_ref()
                .and_then(|sc| sc.seccomp_profile.as_ref())
                .is_none(),
            "{ty}: no container-level seccomp override"
        );
    }
}

// r[verify builder.seccomp.localhost-profile+2]
#[test]
fn seccomp_preinstalled_skips_wait_keeps_enforcement() {
    // P0541: Bottlerocket bootstrap container writes the profile
    // BEFORE kubelet starts. seccomp_preinstalled=true elides the
    // WAIT (initContainer + host-seccomp/host-spo hostPath volumes)
    // but the ENFORCEMENT (container-level Localhost) stays. Tested
    // via build_executor_pod_spec directly so the env-var read in
    // executor_params() doesn't need set_var (parallel-test-unsafe).
    use crate::reconcilers::common::pod::build_executor_pod_spec;
    let mut wp = test_wp();
    wp.spec.seccomp_profile = Some(SeccompProfileKind {
        type_: "Localhost".into(),
        localhost_profile: Some("operator/rio-builder.json".into()),
    });
    let mut params = executor_params_for_test(&wp);
    params.seccomp_preinstalled = true;
    let pod = build_executor_pod_spec(&params, &test_sched_addrs(), &test_store_addrs());

    // No wait-seccomp init.
    assert!(pod.init_containers.is_none(), "preinstalled: no wait init");
    // No host-seccomp / host-spo hostPath volumes.
    let vols = pod.volumes.as_ref().unwrap();
    for forbidden in ["host-seccomp", "host-spo"] {
        assert!(
            !vols.iter().any(|v| v.name == forbidden),
            "preinstalled: no {forbidden} volume"
        );
    }

    // Pod-level: still RuntimeDefault (sandbox can start regardless;
    // no behavior change here — the split between pod-level and
    // container-level is independent of the wait gate).
    let pod_prof = pod
        .security_context
        .as_ref()
        .and_then(|sc| sc.seccomp_profile.as_ref())
        .expect("pod seccomp set");
    assert_eq!(pod_prof.type_, "RuntimeDefault");

    // Worker container: Localhost — the ENFORCEMENT is unchanged.
    let worker = pod
        .containers
        .iter()
        .find(|c| c.name == "builder")
        .expect("worker container");
    let worker_prof = worker
        .security_context
        .as_ref()
        .and_then(|sc| sc.seccomp_profile.as_ref())
        .expect("worker container seccompProfile set");
    assert_eq!(worker_prof.type_, "Localhost");
    assert_eq!(
        worker_prof.localhost_profile,
        Some("operator/rio-builder.json".into())
    );

    // Sanity: same params with preinstalled=false DO emit the wait.
    params.seccomp_preinstalled = false;
    let pod = build_executor_pod_spec(&params, &test_sched_addrs(), &test_store_addrs());
    assert!(
        pod.init_containers.as_ref().is_some_and(|v| v.len() == 1),
        "preinstalled=false: wait-seccomp init present"
    );
}

// r[verify builder.seccomp.localhost-profile+2]
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
        localhost_profile: Some("rio-builder.json".into()),
    });
    let pod = test_pod_spec(&wp);
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
    include_str!("../../../../../infra/helm/rio-build/files/seccomp-rio-builder.json");

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
fn job_pod_no_fuse_hostpath_when_unprivileged() {
    // Default (privileged=None→false): NO hostPath /dev/fuse volume.
    // containerd base_runtime_spec injects the device node into every
    // pod's /dev (nix/base-runtime-spec.nix), so neither a volume nor
    // an extended-resource request is needed. This is the ADR-012
    // production path — enables hostUsers:false (hostPath /dev/fuse
    // is incompatible with idmap mounts).
    let wp = test_wp();
    let pod = test_pod_spec(&wp);

    // No dev-fuse volume (base_runtime_spec injects, no volume needed).
    assert!(
        !pod.volumes
            .as_ref()
            .unwrap()
            .iter()
            .any(|v| v.name == "dev-fuse"),
        "non-privileged path uses base_runtime_spec, not hostPath"
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

    // No extended-resource request. test_wp() has resources=None →
    // container resources stay None (controller adds nothing).
    assert!(
        pod.containers[0].resources.is_none(),
        "no rio.build/* extended resource — base_runtime_spec is unconditional"
    );
}

#[test]
fn job_pod_operator_resources_pass_through() {
    // Operator-supplied resources (cpu/memory/ephemeral) pass through
    // to the container unchanged. Controller used to merge in
    // rio.build/{fuse,kvm} extended resources here; that's gone (the
    // NodeOverlay-declared capacity was simulation-only and broke EKS
    // scheduling), so this is now a straight pass-through.
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
    let pod = test_pod_spec(&wp);

    let resources = pod.containers[0].resources.as_ref().unwrap();
    let limits = resources.limits.as_ref().unwrap();
    let requests = resources.requests.as_ref().unwrap();

    assert_eq!(limits.get("memory").map(|q| q.0.as_str()), Some("8Gi"));
    assert_eq!(requests.get("cpu").map(|q| q.0.as_str()), Some("2"));
    assert_eq!(requests.get("memory").map(|q| q.0.as_str()), Some("4Gi"));
    // Nothing the operator didn't put there.
    assert_eq!(limits.len(), 1, "no controller-injected limits");
    assert_eq!(requests.len(), 2, "no controller-injected requests");
}

// r[verify ctrl.builderpool.kvm-device]
#[test]
fn job_pod_kvm_feature_adds_nodeselector_toleration() {
    // Fixture wp has features=["kvm"]. The controller derives the
    // metal-NodePool scheduling constraints from that one feature
    // string — operator doesn't set them explicitly (mirrors I-098's
    // arch derivation from systems). /dev/kvm itself arrives via
    // containerd base_runtime_spec on every pod; the nodeSelector is
    // what makes it functional (only .metal hosts have working KVM).
    let wp = test_wp();
    assert!(wp.spec.features.iter().any(|f| f == "kvm"), "precondition");
    let pod = test_pod_spec(&wp);

    // rio.build/kvm=true nodeSelector — lands on the metal NodePool.
    // The I-098 arch selector survives alongside.
    let ns = pod.node_selector.as_ref().expect("nodeSelector set");
    assert_eq!(ns.get("rio.build/kvm").map(String::as_str), Some("true"));
    assert_eq!(
        ns.get("kubernetes.io/arch").map(String::as_str),
        Some("amd64"),
        "arch selector preserved"
    );

    // rio.build/kvm toleration appended (metal NodePool is tainted).
    let tols = pod.tolerations.as_ref().expect("tolerations set");
    let kvm_tol = tols
        .iter()
        .find(|t| t.key.as_deref() == Some("rio.build/kvm"))
        .expect("kvm toleration appended");
    assert_eq!(kvm_tol.value.as_deref(), Some("true"));
    assert_eq!(kvm_tol.effect.as_deref(), Some("NoSchedule"));
}

// r[verify ctrl.builderpool.kvm-device]
#[test]
fn job_pod_no_kvm_feature_no_kvm_wiring() {
    // The CONDITIONAL is the load-bearing part: a non-kvm pool must
    // NOT select rio.build/kvm (would Pending forever — only the
    // metal NodePool carries that label) and must NOT tolerate the
    // metal taint (would bin-pack cheap builds onto $$ metal).
    let mut wp = test_wp();
    wp.spec.features = vec!["big-parallel".into()];
    let pod = test_pod_spec(&wp);

    assert!(
        pod.node_selector
            .as_ref()
            .is_none_or(|ns| !ns.contains_key("rio.build/kvm")),
        "no kvm nodeSelector without features:[kvm]"
    );
    assert!(
        pod.tolerations
            .as_ref()
            .is_none_or(|t| !t.iter().any(|t| t.key.as_deref() == Some("rio.build/kvm"))),
        "no kvm toleration without features:[kvm]"
    );
}

// r[verify ctrl.builderpool.kvm-device]
#[test]
fn job_pod_kvm_privileged_keeps_selector() {
    // Privileged escape hatch still needs a host that HAS working
    // /dev/kvm — nodeSelector + toleration are unconditional wrt
    // privileged.
    let mut wp = test_wp();
    wp.spec.privileged = Some(true);
    let pod = test_pod_spec(&wp);

    assert_eq!(
        pod.node_selector
            .as_ref()
            .and_then(|ns| ns.get("rio.build/kvm"))
            .map(String::as_str),
        Some("true"),
        "privileged still needs metal placement"
    );
}

// r[verify sec.pod.host-users-false]
#[test]
fn job_pod_host_users_false_when_unprivileged() {
    // hostUsers:false → K8s user-namespace isolation. Container UIDs
    // remapped to unprivileged host UIDs; CAP_SYS_ADMIN applies only
    // within the user namespace. The LOAD-BEARING defense-in-depth
    // layer (ADR-012 §User Namespace Isolation).
    let wp = test_wp();
    let pod = test_pod_spec(&wp);
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
    let pod = test_pod_spec(&wp);

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
    let pod = test_pod_spec(&wp);

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
fn job_pod_privileged_escape_hatch_uses_hostpath() {
    // privileged=true → hostPath /dev/fuse fallback. No
    // hostUsers:false (incompatible with privileged containers).
    // This is the k3s/kind escape hatch.
    let mut wp = test_wp();
    wp.spec.privileged = Some(true);
    let pod = test_pod_spec(&wp);

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

    // Resources pass through unchanged; test_wp has resources=None so
    // the container resources stay None.
    assert!(pod.containers[0].resources.is_none());
}

#[test]
fn job_pod_overlays_volume_mounted() {
    // RIO_OVERLAY_BASE_DIR points to /var/rio/overlays. If
    // there's no volume mount for it, it lands on the
    // container's root filesystem — which is overlayfs.
    // Overlayfs-as-upperdir can't create trusted.* xattrs →
    // every overlay mount fails with EINVAL. Regression guard.
    let wp = test_wp();
    let pod = test_pod_spec(&wp);

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
/// `admin.drain_executor(DrainExecutorRequest { force: true, ... })`
/// when this filter returns `Some` — the prod `force: true` caller
/// the 4 lying comments have been asserting exists.
#[test]
fn disruption_filter_true_returns_name() {
    use k8s_openapi::api::core::v1::{PodCondition, PodStatus};

    let mut pod = Pod::default();
    pod.metadata.name = Some("rio-builder-3".into());
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

    assert_eq!(
        disruption::is_disruption_target(&pod),
        Some("rio-builder-3")
    );
}

#[test]
fn disruption_filter_false_or_absent_returns_none() {
    use k8s_openapi::api::core::v1::{PodCondition, PodStatus};

    // No status at all (Pending pod, not yet scheduled).
    let mut no_status = Pod::default();
    no_status.metadata.name = Some("pending-0".into());
    assert_eq!(disruption::is_disruption_target(&no_status), None);

    // Status present, conditions=None. Rare but modeled as Option.
    let mut no_conds = Pod::default();
    no_conds.metadata.name = Some("boot-0".into());
    no_conds.status = Some(PodStatus::default());
    assert_eq!(disruption::is_disruption_target(&no_conds), None);

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
    assert_eq!(disruption::is_disruption_target(&denied), None);

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
    assert_eq!(disruption::is_disruption_target(&healthy), None);
}

#[test]
fn job_pod_tls_secret_mounted_when_set() {
    // spec.tlsSecretName set → volume + mount + 3 RIO_TLS__* env
    // vars. Unset → none of these (plaintext mode). Both paths
    // tested: the base test_wp has it unset; this test sets it.
    let mut wp = test_wp();
    wp.spec.tls_secret_name = Some("rio-builder-tls".into());
    let pod = test_pod_spec(&wp);

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
        Some("rio-builder-tls".into())
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
fn job_pod_no_tls_when_unset() {
    // The default test_wp has tls_secret_name=None. No tls
    // volume, no mount, no RIO_TLS__* env — clean plaintext.
    let wp = test_wp();
    let pod = test_pod_spec(&wp);

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
fn job_pod_termination_grace() {
    let wp = test_wp();
    let pod = test_pod_spec(&wp);
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
fn job_pod_env_vars() {
    let wp = test_wp();
    let pod = test_pod_spec(&wp);
    let container = &pod.containers[0];
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
    assert_eq!(
        envs.get("RIO_STORE_BALANCE_HOST"),
        Some(&"store-headless".into()),
        "balance host injected when ctx.store_balance_host is Some"
    );
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

    // RIO_EXECUTOR_ID uses fieldRef, not value — check separately.
    // figment reads `executor_id` → prefix RIO_ → `RIO_EXECUTOR_ID`.
    let executor_id = container
        .env
        .as_ref()
        .unwrap()
        .iter()
        .find(|e| e.name == "RIO_EXECUTOR_ID")
        .unwrap();
    assert_eq!(executor_id.value, None, "not a literal value");
    assert_eq!(
        executor_id
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
fn job_pod_worker_knobs_injected_when_set() {
    let mut wp = test_wp();
    wp.spec.fuse_threads = Some(8);
    wp.spec.fuse_passthrough = Some(false);
    wp.spec.daemon_timeout_secs = Some(14400);
    let pod = test_pod_spec(&wp);
    let container = &pod.containers[0];
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

#[test]
fn job_pod_image_pull_policy_passthrough() {
    // None stays None — K8s applies its tag-based default
    // (IfNotPresent for non-:latest, Always for :latest).
    let wp = test_wp();
    let pod = test_pod_spec(&wp);
    let container = &pod.containers[0];
    assert_eq!(container.image_pull_policy, None);

    // Explicit value passes through. Airgap k3s/kind need
    // IfNotPresent or Never to use ctr-imported images.
    let mut wp = test_wp();
    wp.spec.image_pull_policy = Some("IfNotPresent".into());
    let pod = test_pod_spec(&wp);
    let container = &pod.containers[0];
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
