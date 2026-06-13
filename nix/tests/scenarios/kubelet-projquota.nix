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
#   provisioned node,   live_063 — the FOURTH decline mode's missing
#   hostUsers:true:     cell (the one that would have caught the 0/1912
#                       silence pre-deploy): same provisioned node, the
#                       PRODUCTION posture (drift-pinned to values.yaml
#                       poolDefaults — never hand-written). kubelet
#                       refuses SupportsQuotas for host-user pods, so
#                       the cell asserts the BUILDER-MINTED path:
#                       quota.rs ensure_project_quota self-assigns a
#                       projid from the builder-owned range below
#                       kubelet's allocator, and the unchanged
#                       production reader returns Some.
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

  # live_063 (the fourth decline mode's posture pin): the production
  # pool spec the hostUsers:true cell's posture DERIVES from. The
  # testScript drift-asserts the manifest posture against
  # poolDefaults.hostUsers in this file — a hand-written posture here
  # is exactly how live_063 happened (the wave-12 witness ran
  # hostUsers:false while every production pool ran true, and the
  # provisioned × true cell never existed).
  prodValues = ../../../infra/helm/rio-build/values.yaml;

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

  # The SIZED pod mirrors the production overlays shape (bug_065): an
  # emptyDir sizeLimit BELOW the container's ephemeral-storage limit.
  # kubelet assigns the volume's project quota = MIN(pod ephemeral
  # limit, emptyDir sizeLimit) — the sizeLimit side — so the hard
  # limit the production reader returns is the STAMPED solve-axis
  # product, not the fuse+log-inflated pod limit. The denomination
  # subtest below pins exactly that MIN.
  podManifest = pkgs.writeText "scratch-writer.yaml" ''
    apiVersion: v1
    kind: Pod
    metadata:
      name: scratch-writer
      namespace: default
    spec:
      # The USERNS posture — what kubelet's fsquota covers (assignment
      # is userns-conditioned at the deployed minor). This is the
      # sec.pod.host-users-false DEFERRED target, NOT the production
      # builders' standing posture (they run hostUsers:true until
      # P0560 — the live_063 cell below covers that half; this
      # comment's previous "standing posture" claim was the same rot
      # the three live_063 homes carried).
      hostUsers: false
      restartPolicy: Never
      containers:
        - name: writer
          image: rio-test/scratch-writer:v1
          imagePullPolicy: Never
          resources:
            limits:
              # Sized so BOTH pods' requests fit the 4 GiB quota
              # volume's allocatable (kubelet reserves ~1.1 GiB):
              # 1280Mi + 1Gi requests < ~2.9 GiB.
              ephemeral-storage: 1280Mi
          volumeMounts:
            - name: scratch
              mountPath: /scratch
      volumes:
        - name: scratch
          emptyDir:
            sizeLimit: 768Mi
  '';

  # The UNSIZED pod is the LATENT denomination cell bug_065 closes
  # against: an emptyDir with NO sizeLimit falls back to the POD
  # ephemeral limit as its project quota — on a builder pod that
  # would be disk*h + fuse(50Gi) + 1Gi, the foreign-units hard limit
  # the corroboration band refuses. The production stamp
  # (apply_intent_resources) makes this cell unreachable for the
  # overlays volume; this pod witnesses that the fallback is REAL at
  # the deployed kubelet minor, so the stamp stays load-bearing
  # (upstream behavior drift flips this test, the OQ-13 posture).
  unsizedPodManifest = pkgs.writeText "scratch-writer-unsized.yaml" ''
    apiVersion: v1
    kind: Pod
    metadata:
      name: scratch-writer-unsized
      namespace: default
    spec:
      hostUsers: false
      restartPolicy: Never
      containers:
        - name: writer
          image: rio-test/scratch-writer:v1
          imagePullPolicy: Never
          resources:
            limits:
              ephemeral-storage: 1Gi
          volumeMounts:
            - name: scratch
              mountPath: /scratch
      volumes:
        - name: scratch
          emptyDir: {}
  '';

  # live_063 — the FOURTH decline mode's witness pod: the PRODUCTION
  # posture (hostUsers:true, drift-pinned to the pool spec below) on
  # the PROVISIONED node. kubelet 1.36 refuses SupportsQuotas for
  # host-user pods, so with modes 1-3 healthy the ONLY Some-path is
  # the builder-minted projid (quota.rs ensure_project_quota, invoked
  # here via `quota_probe --ensure` — the production acquisition face
  # standing in for setup_overlay; the setup_overlay→ensure threading
  # is pinned at unit level in rio-builder, the same tier split as the
  # completion-row threading disclosed above). Privilege shape mirrors
  # the production builder pod (root + CAP_SYS_ADMIN, pool/pod.rs):
  # under hostUsers:true the pod IS in the init userns — exactly the
  # jurisdiction where the kernel permits FS_IOC_FSSETXATTR projid
  # changes (their refusal outside it is WHY kubelet's userns-only
  # half and this half partition the posture space).
  #
  # `sleep 86399` (not 86400): the uid_map subtests pgrep their pod's
  # parked process by its distinct sleep duration — two pods park on
  # this node.
  hostUsersPodManifest = pkgs.writeText "scratch-writer-hostusers.yaml" ''
    apiVersion: v1
    kind: Pod
    metadata:
      name: scratch-writer-hostusers
      namespace: default
    spec:
      hostUsers: true
      restartPolicy: Never
      containers:
        - name: writer
          image: rio-test/scratch-writer:v1
          imagePullPolicy: Never
          command:
            - /bin/sh
            - -c
            - "${probe} --ensure /scratch && dd if=/dev/zero of=/scratch/fill bs=1M count=64 && touch /scratch/done && sleep 86399"
          securityContext:
            capabilities:
              add: ["SYS_ADMIN"]
          volumeMounts:
            - name: scratch
              mountPath: /scratch
            - name: nix-store
              mountPath: /nix/store
              readOnly: true
      volumes:
        - name: scratch
          emptyDir:
            sizeLimit: 1Gi
        - name: nix-store
          hostPath:
            path: /nix/store
            type: Directory
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
      manifests = {
        scratch-writer.source = podManifest;
      }
      // pkgs.lib.optionalAttrs provisioned {
        # The denomination cells need the second pod only where
        # quotas exist.
        scratch-writer-unsized.source = unsizedPodManifest;
        # The live_063 cell's pod likewise runs only where the
        # provisioning half holds — the unprovisioned twin keeps
        # reproducing today's fleet with the original pod alone.
        scratch-writer-hostusers.source = hostUsersPodManifest;
      };
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


    def wait_pod_wrote(node, pod="scratch-writer"):
        node.wait_until_succeeds(
            "k3s kubectl get pod " + pod + " -o jsonpath='{.status.phase}' | grep -E 'Running|Succeeded'",
            timeout=420,
        )
        # Resolve the emptyDir BY POD UID (three pods share the volume
        # name); the dir is the one KUBELET created — no manual
        # projid anywhere in this scenario (the live060-d
        # discrimination).
        uid = node.succeed(
            "k3s kubectl get pod " + pod + " -o jsonpath='{.metadata.uid}'"
        ).strip()
        path = "/var/lib/kubelet/pods/" + uid + "/volumes/kubernetes.io~empty-dir/scratch"
        node.wait_until_succeeds("ls " + path + "/done", timeout=180)
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
        path = wait_pod_wrote(provisioned, "scratch-writer")
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

    with subtest("the kubelet quota is the NON-ENFORCING sentinel (bug_065 cells)"):
        # THE DERIVATION-CORRECTING WITNESS: kubelet's DESIRED volume
        # quota is min(pod ephemeral limit, emptyDir sizeLimit)
        # (AddPodToVolume), but at the deployed minors AssignQuota
        # writes the -1 NON-ENFORCING sentinel for any positive
        # desired size (quota_linux.go: usage tracking for eviction,
        # never kernel enforcement). Consequence chain the scheduler's
        # corroboration band is derived against: the worker-visible
        # hard limit on kubelet-quota'd nodes is the sentinel (reads
        # ~u64::MAX through the 1KiB-block saturation), the worker
        # DiskFull classifier cannot fire there, and the band refuses
        # sentinel-armed claims by construction. BOTH cells (sized
        # 768Mi-under-1280Mi and unsized 1Gi-pod-limit) pin the
        # sentinel: if kubelet ever starts enforcing the desired size
        # (or stops tracking usage), one of these asserts flips and
        # the bug_065 derivation re-opens.
        sentinel = str((1 << 64) - 1)
        kv = probe_kv(provisioned, path)
        assert kv["quota_limit"] == sentinel, (
            "the sized emptyDir's quota limit is no longer the "
            "non-enforcing sentinel - kubelet's enforcement posture "
            "changed; re-derive the corroboration band denomination "
            "(bug_065): " + repr(kv)
        )
        upath = wait_pod_wrote(provisioned, pod="scratch-writer-unsized")
        ukv = probe_kv(provisioned, upath)
        assert ukv["quota_limit"] == sentinel, (
            "the unsized emptyDir's quota limit is no longer the "
            "non-enforcing sentinel - kubelet's enforcement posture "
            "changed; re-derive the corroboration band denomination "
            "(bug_065): " + repr(ukv)
        )
        # Usage tracking is REAL on both (the quota's actual job).
        assert int(ukv["quota_used"]) >= 64 * 1024 * 1024, (
            "the unsized pod's usage is untracked: " + repr(ukv)
        )

    with subtest("live_063: the witness posture derives from the production pool spec"):
        # The R31' fixture-derivation face: hostUsers:true in the
        # scratch-writer-hostusers manifest is NOT hand-chosen — it is
        # pinned to the production pool defaults. When production
        # flips to false (P0560 deletes the I-186 pin), this red
        # forces the cell's re-derivation instead of letting the
        # witness rot against a stale posture (live_063 inverted:
        # the wave-12 witness hand-wrote false while the fleet ran
        # true, and the production cell never existed).
        prod = None
        in_defaults = False
        for line in open("${prodValues}"):
            if line.rstrip() == "poolDefaults:":
                in_defaults = True
                continue
            if in_defaults and line.strip() and not line.startswith(" "):
                break
            if in_defaults and line.strip().startswith("hostUsers:"):
                prod = line.split(":", 1)[1].strip()
                break
        assert prod == "true", (
            "production poolDefaults.hostUsers is " + repr(prod) + " but this "
            "witness cell runs hostUsers:true - re-derive the cell (the "
            "builder-minted path is posture-conditional)"
        )

    with subtest("live_063 (the missing cell): provisioned x hostUsers:true -> builder-minted Some"):
        path2 = wait_pod_wrote(provisioned, "scratch-writer-hostusers")
        # The inverse posture precondition: this pod is genuinely
        # HOST-user (identity uid_map), i.e. exactly the posture
        # kubelet's fsquota refuses — any Some below is the builder's.
        uid_map = provisioned.succeed(
            "cat /proc/$(pgrep -f 'sleep 86399' | head -n1)/uid_map"
        ).split()
        assert uid_map[0] == "0" and uid_map[1] == "0", (
            "the pod is user-namespaced despite hostUsers:true - the cell "
            "is not testing the production posture; uid_map: " + " ".join(uid_map)
        )
        kv = probe_kv(provisioned, path2)
        # THE live_063 red, verbatim shape: pre-fix (no builder mint)
        # this emptyDir has projid 0 and the production reader returns
        # None on a FULLY PROVISIONED node — the fourth decline mode.
        assert kv["quota_used"] != "none", (
            "THE LIVE_063 RED: provisioned x hostUsers:true still yields "
            "None - kubelet declined (host-user pod) and no builder mint "
            "fired: " + repr(kv)
        )
        assert int(kv["quota_used"]) >= 64 * 1024 * 1024, (
            "quota tracking missed the 64 MiB scratch write through the "
            "builder-minted projid: " + repr(kv)
        )
        # The id sits in the builder-owned range [2^19, 2^20) — below
        # kubelet's 1048576+ allocator BY CONSTRUCTION (quota.rs's
        # compile-time range assert; this is its kernel-coupled echo).
        projid = int(kv["projid"])
        assert 524288 <= projid < 1048576, (
            "projid outside the builder-owned range - collision "
            "discipline broken or kubelet unexpectedly assigned: " + repr(kv)
        )
        # The mint trace: the production acquisition face reported a
        # FRESH mint (not an observed kubelet id) inside the pod.
        logs = provisioned.succeed("k3s kubectl logs scratch-writer-hostusers")
        assert "ensure=minted" in logs, (
            "the production ensure face did not report a mint: " + repr(logs)
        )

    with subtest("RED reproduced (W12-LI): today's fleet shape yields None"):
        unprovisioned.start()
        unprovisioned.wait_for_unit("k3s.service")
        unprovisioned.wait_until_succeeds("k3s kubectl get node | grep -q ' Ready'", timeout=900)
        path = wait_pod_wrote(unprovisioned, "scratch-writer")
        kv = probe_kv(unprovisioned, path)
        assert kv["quota_used"] == "none", (
            "the un-provisioned node unexpectedly produced a quota "
            "sample - the red lost its discrimination: " + repr(kv)
        )
  '';
}
