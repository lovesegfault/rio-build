# kubelet-projquota — the kubelet-half END-TO-END witness (live_060-d,
# WO-S8-16): the producer chain witnessed from its PRODUCTION FEEDER.
#
# The existing quota-probe scenario assigns project IDs MANUALLY
# (`xfs_quota project -s`) — it witnesses the KERNEL quota mechanism
# and the classifier coupling, NOT the kubelet half (featureGates →
# projid assignment → emptyDir tracking), which is exactly the half
# the live fleet never had: 159/160 builder nodes EBS-only ext4, no
# prjquota, no gate, no projid registry — 2022/2022 completions with
# `peak_disk_bytes: None`, silently, both disk-sizing ladders dead.
#
# THIS scenario drives the chain with ZERO manual projid assignment:
#
#   provisioned node:   /var/lib/kubelet on xfs `-o prjquota` (the
#                       WO-S8-15 provisioning shape: a dedicated bare
#                       volume formatted+mounted before the kubelet
#                       starts — the same flow eks-node.nix's
#                       rio-kubelet-mount EBS branch runs on the
#                       fleet, minus EC2 device enumeration;
#                       nixos-node.nix pins the AMI-side unit itself)
#                       + the kubelet gate
#                       (LocalStorageCapacityIsolationFSQuotaMonitoring)
#                       + /etc/projects + /etc/projid. A REAL
#                       hostUsers:false pod with an emptyDir is
#                       scheduled BY KUBELET and writes its scratch;
#                       the PRODUCTION reader (quota_probe — the
#                       executor's exact classifier-input chain)
#                       against the kubelet-created emptyDir returns
#                       projid != none and quota_used = Some(bytes
#                       >= the write). This is the peak_disk_bytes
#                       producer chain end-to-end: the during-build
#                       1 Hz monitor max-tracks the same
#                       `quota::status` sample this asserts.
#
#   unprovisioned node: today's fleet reproduced in-VM — stock root
#                       fs, no gate, no projid registry. The same
#                       pod runs; the same reader returns
#                       quota_used = none. THE RED (W12-LI): the
#                       witness discriminates kubelet-half presence,
#                       which the manual-projid probe structurally
#                       cannot.
#
# Tier disclosure (honest): the completion-row threading
# (monitor max-track → BuildCompleted.peak_disk_bytes) is pinned at
# unit level in rio-builder; this scenario proves the link the fleet
# was missing — node config → kubelet projid assignment → production
# reader Some — against a real kubelet at the deployed minor.
#
# Budget (R17): two single-node k3s VMs booting in parallel, one
# busybox pod each, no rio pipeline. k3s server Ready ~60-120s on
# KVM + image import; 900s leaves builder-variance headroom per the
# catalogued coverage-mode class.
{
  pkgs,
  common,
}:
let
  inherit (common) rio-workspace;
  probe = "${rio-workspace}/bin/quota_probe";

  # Deterministic in-VM workload image (airgapped test: no registry).
  workloadImage = pkgs.dockerTools.buildImage {
    name = "rio-test/scratch-writer";
    tag = "v1";
    copyToRoot = [ pkgs.busybox ];
    config.Cmd = [
      "/bin/sh"
      "-c"
      # 64 MiB of scratch, then park: the witness reads the quota
      # while the pod is live (the keep-failed cleanup collapse is
      # quota-probe's cell 4 — out of scope here).
      "dd if=/dev/zero of=/scratch/fill bs=1M count=64 && touch /scratch/done && sleep 86400"
    ];
  };

  podManifest = pkgs.writeText "scratch-writer.yaml" ''
    apiVersion: v1
    kind: Pod
    metadata:
      name: scratch-writer
      namespace: default
    spec:
      # The builder pods' standing posture (sec.pod.host-users-false);
      # kubelet's fsquota assignment is userns-conditioned at the
      # deployed minor — quotas apply to user-namespaced pods.
      hostUsers: false
      restartPolicy: Never
      containers:
        - name: writer
          image: rio-test/scratch-writer:v1
          imagePullPolicy: Never
          volumeMounts:
            - name: scratch
              mountPath: /scratch
      volumes:
        - name: scratch
          emptyDir:
            sizeLimit: 1Gi
  '';

  k3sNode = provisioned: {
    virtualisation = {
      memorySize = 3072;
      cores = 2;
      diskSize = 6144;
      # The provisioned node's quota volume (the karpenter second-EBS
      # stand-in); the unprovisioned node gets none — today's fleet.
      emptyDiskImages = pkgs.lib.optionals provisioned [ 4096 ];
    };

    services.k3s = {
      enable = true;
      role = "server";
      images = [
        pkgs.k3s.airgap-images
        workloadImage
      ];
      extraFlags = [
        "--disable=traefik"
        "--disable=metrics-server"
        "--disable=servicelb"
      ]
      ++ pkgs.lib.optionals provisioned [
        # The kubelet half (live_060-a): the gate the fleet never
        # set. The drop-in carrier on the fleet is eks-node.nix's
        # 30-rio-fsquota.conf; k3s threads it as a kubelet arg.
        # NOTE: no `--kubelet-arg=v=N` here — k3s 1.35's embedded
        # kubelet crash-loops on a verbosity re-init ("the logging
        # configuration should not be changed after setting it
        # once"); the uid_map and projid-registry subtests below are
        # the diagnostic lanes instead.
        "--kubelet-arg=feature-gates=LocalStorageCapacityIsolationFSQuotaMonitoring=true"
      ];
      manifests.scratch-writer.source = podManifest;
    };

    # The provisioning shape (WO-S8-15, in-VM): a bare volume becomes
    # the prjquota kubelet root BEFORE the kubelet starts. Mirrors
    # rio-kubelet-mount's EBS-branch mkfs+mount flow; the EC2
    # enumeration half is pinned by nix/tests/nixos-node.nix against
    # the real unit.
    systemd.services.rio-quota-mount = pkgs.lib.mkIf provisioned {
      description = "Mount the quota volume at /var/lib/kubelet (prjquota)";
      wantedBy = [ "multi-user.target" ];
      before = [ "k3s.service" ];
      requiredBy = [ "k3s.service" ];
      path = [
        pkgs.xfsprogs
        pkgs.util-linux
      ];
      script = ''
        set -euo pipefail
        mkfs.xfs -f /dev/vdb
        mkdir -p /var/lib/kubelet
        mount -o prjquota,noatime /dev/vdb /var/lib/kubelet
      '';
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
    };

    # The projid registry half (live_060-a): kubelet's fsquota locks
    # BOTH files and silently reports quotas unsupported when either
    # is missing.
    systemd.tmpfiles.rules = pkgs.lib.optionals provisioned [
      "f /etc/projects 0644 root root -"
      "f /etc/projid 0644 root root -"
      # The FHS shim half (live_060-d, derived IN THIS WITNESS):
      # kubelet's quota applier shells out to /sbin/xfs_quota +
      # /usr/bin/lsattr at fixed paths; without them every other
      # precondition holds and assignment still silently declines.
      # eks-node.nix carries the fleet twin.
      "d /sbin 0755 root root -"
      "L+ /sbin/xfs_quota - - - - ${pkgs.xfsprogs}/bin/xfs_quota"
      "L+ /usr/bin/lsattr - - - - ${pkgs.e2fsprogs}/bin/lsattr"
    ];
  };
in
pkgs.testers.runNixOSTest {
  name = "rio-kubelet-projquota";
  skipTypeCheck = true;
  # Budget for the tail, not the typical (the catalogued k3s
  # airgap-import variance class: serial image import under shared-
  # builder load; nodes boot SEQUENTIALLY below to halve peak I/O).
  globalTimeout = 2400 + common.covTimeoutHeadroom;

  nodes = {
    provisioned = k3sNode true;
    unprovisioned = k3sNode false;
  };

  testScript = ''
    ${common.assertions}

    ${common.kvmCheck}


    def probe_kv(node, path):
        """The production classifier-input chain (quota_probe) against
        a kubelet-created emptyDir; key=value grammar."""
        out = node.succeed("${probe} " + path)
        return dict(line.split("=", 1) for line in out.strip().splitlines())


    def wait_pod_wrote(node):
        node.wait_until_succeeds(
            "k3s kubectl get pod scratch-writer -o jsonpath='{.status.phase}' | grep -E 'Running|Succeeded'",
            timeout=420,
        )
        # The emptyDir as KUBELET created it — no manual projid
        # anywhere in this scenario (the live060-d discrimination).
        node.wait_until_succeeds(
            "ls /var/lib/kubelet/pods/*/volumes/kubernetes.io~empty-dir/scratch/done",
            timeout=180,
        )
        path = node.succeed(
            "dirname /var/lib/kubelet/pods/*/volumes/kubernetes.io~empty-dir/scratch/done"
        ).strip()
        return path


    # Sequential boot: two parallel k3s airgap imports double the
    # builder-disk pressure that already flakes this class; the nodes
    # are independent, so serialize.
    provisioned.start()
    provisioned.wait_for_unit("k3s.service")
    provisioned.wait_until_succeeds("k3s kubectl get node | grep -q ' Ready'", timeout=900)

    with subtest("provisioned: kubelet root is prjquota xfs (the WO-S8-15 shape)"):
        out = provisioned.succeed("findmnt -no FSTYPE,OPTIONS /var/lib/kubelet")
        assert "xfs" in out and "prjquota" in out, repr(out)

    with subtest("the pod is genuinely user-namespaced (the quota precondition)"):
        path = wait_pod_wrote(provisioned)
        uid_map = provisioned.succeed(
            "cat /proc/$(pgrep -f 'sleep 86400' | head -n1)/uid_map"
        ).split()
        # A host-users process maps 0 -> 0 over the full range; a
        # user-namespaced pod maps 0 -> a high host uid. kubelet
        # fsquota DECLINES SILENTLY (V3) for host-user pods, so this
        # assert turns that silence into a named failure.
        assert uid_map[1] != "0", (
            "the pod runs HOST-USERS despite hostUsers:false - kubelet "
            "fsquota will decline; uid_map: " + " ".join(uid_map)
        )

    with subtest("kubelet wrote the project registry (the assignment trace)"):
        # kubelet's fsquota appends one entry per assigned quota to
        # /etc/projects + /etc/projid; empty registries after a
        # quota-eligible pod = the assignment never ran (the silent
        # decline made loud).
        provisioned.succeed("test -s /etc/projects || (cat /etc/projects /etc/projid; exit 1)")

    with subtest("GREEN (W12-LI): kubelet assigns the projid; the production reader returns Some"):
        kv = probe_kv(provisioned, path)
        assert kv["projid"] not in ("none", "0"), (
            "kubelet did not assign a project ID to the emptyDir "
            "(the kubelet half is broken): " + repr(kv)
        )
        assert kv["quota_used"] != "none", (
            "the production reader returned None on a provisioned node: " + repr(kv)
        )
        used = int(kv["quota_used"])
        assert used >= 64 * 1024 * 1024, (
            "quota tracking missed the 64 MiB scratch write: " + repr(kv)
        )

    with subtest("RED reproduced (W12-LI): today's fleet shape yields None"):
        unprovisioned.start()
        unprovisioned.wait_for_unit("k3s.service")
        unprovisioned.wait_until_succeeds("k3s kubectl get node | grep -q ' Ready'", timeout=900)
        path = wait_pod_wrote(unprovisioned)
        kv = probe_kv(unprovisioned, path)
        assert kv["quota_used"] == "none", (
            "the un-provisioned node unexpectedly produced a quota "
            "sample - the red lost its discrimination: " + repr(kv)
        )
  '';
}
