# services.rio.eksNode — thin EKS-worker module.
#
# NOT nixpkgs services.kubernetes.kubelet: that module assumes a self-
# managed cluster (PKI generation, kubeconfig rendering, kubernetes.target
# ordering). Here ALL kubelet config is written at boot by nodeadm from
# Karpenter-supplied userData (cluster CA, endpoint, --node-labels,
# --register-with-taints, max-pods, providerID). The NixOS units just
# point at the files nodeadm wrote.
#
# Filesystem contract (nodeadm output, stable across AL2023 releases):
#   /etc/kubernetes/kubelet/config.json   KubeletConfiguration (+ .d/ drop-ins)
#   /etc/eks/kubelet/environment          NODEADM_KUBELET_ARGS=<all flags>
#   /etc/kubernetes/pki/ca.crt            cluster CA
#   /var/lib/kubelet/kubeconfig           kubeconfig (exec: aws-iam-authenticator)
#   /etc/eks/image-credential-provider/config.json
#
# containerd config is build-time static (containerd-config.nix) — nodeadm
# is invoked with `--daemon kubelet` and never touches /etc/containerd/.
{
  config,
  lib,
  pkgs,
  pins,
  ...
}:
let
  cfg = config.services.rio.eksNode;
  nodeadm = pkgs.callPackage ./nodeadm.nix { inherit pins; };
  ecr-credential-provider = pkgs.callPackage ./ecr-credential-provider.nix { inherit pins; };

  # The quota-volume selection, extracted to a file so the unit-tier
  # check (misc-checks.nix `quota-volume-select`) runs the SAME logic
  # against fixture by-id namespaces (merged_bug_024: the in-unit copy
  # counted the ami-bios bios_grub partition as a bare candidate —
  # n_bare=2, exit 1, kubelet Requires= hard-fail on every x86-metal
  # boot — and the quota-volume-late corner selected bios_grub for
  # mkfs.xfs -f; nothing exercised the by-id enumeration until the
  # selection became this testable unit). Needs lsblk + readlink on
  # PATH (the consuming unit provides util-linux + coreutils).
  quotaVolumeSelect = pkgs.writeShellScript "quota-volume-select" (
    builtins.readFile ./quota-volume-select.sh
  );

  # containerd-config.nix pins sandbox = "localhost/kubernetes/pause" and
  # expects the AMI bake to have pre-loaded it (templates/shared/runtime/
  # bin/cache-pause-container in the AL2023 builder). Build the pause binary
  # from the same kubernetes derivation kubelet comes from and wrap it as
  # a single-layer OCI tarball; kubelet preStart `ctr image import`s it
  # then `ctr image label`s it pinned so kubelet's image-GC won't evict.
  pauseImage = pkgs.dockerTools.buildImage {
    name = "localhost/kubernetes/pause";
    tag = "latest";
    copyToRoot = [ cfg.kubernetesPackage.pause ];
    config.Entrypoint = [ "/bin/pause" ];
  };

  pauseRef = "localhost/kubernetes/pause:latest";

  # /dev/{fuse,kvm} OCI base spec — both variants baked into the AMI;
  # containerd's ExecStartPre below (baseRuntimeSpec.pickExecStartPre)
  # picks based on host /dev/kvm presence and symlinks to
  # baseRuntimeSpec.runtimePath (the path containerd-config.nix points
  # base_runtime_spec at). Non-.metal nodes get the fuse-only spec, so
  # pods don't see a dead /dev/kvm mknod that fools `test -c /dev/kvm`
  # probes.
  baseRuntimeSpec = import ../base-runtime-spec.nix { inherit pkgs; };

  # r[impl sec.pod.host-users-false+3]
  containerdConfig = import ./containerd-config.nix {
    inherit lib pkgs pauseRef;
    runtimeSpecPath = baseRuntimeSpec.runtimePath;
  };
in
{
  options.services.rio.eksNode = {
    enable = lib.mkEnableOption "EKS worker node bootstrap via nodeadm";

    kubernetesPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.kubernetes;
      description = ''
        kubelet binary source. nixpkgs `kubernetes` tracks the version in
        `nix/pins.toml` `[cluster] kubernetes_version` (both follow
        upstream minor).
      '';
    };

    # Escape hatch: extra static-pod manifests (e.g. node-local debug
    # tooling). Empty in the production AMI.
    staticPods = lib.mkOption {
      type = lib.types.attrsOf lib.types.path;
      default = { };
      description = ''
        Kubelet static-pod manifests, keyed by name. Written to
        `/etc/kubernetes/manifests/<name>.json` (nodeadm sets
        `staticPodPath` to that dir in the KubeletConfiguration).
      '';
    };

    seedImages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      description = ''
        OCI-archive tarballs to `ctr -n k8s.io image import --local`
        via the containerd-seed-warm oneshot (runs concurrent with
        kubelet TLS-bootstrap, not before it). Layer blobs land in
        containerd's content store; the seed.local/…:prebaked refs are
        pinned so kubelet image-GC and containerd content-GC can't
        reclaim the blobs before any pod has referenced them via its
        real ECR ref. The seed refs themselves are never pulled —
        they're GC roots only. See r[infra.node.prebake-layer-warm].
      '';
    };

    quotaVolumeGlobs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_vol*" ];
      description = ''
        Device globs rio-kubelet-mount's EBS branch enumerates to find
        the dedicated kubelet quota volume on EBS-only nodes
        (live_060: the karpenter EC2NodeClass attaches it as the
        second EBS mapping). Selection is by TYPED device class
        (quota-volume-select.sh, merged_bug_024): per-partition by-id
        links (`*-part[0-9]*`) are rejected by name, every candidate
        must read `lsblk TYPE == disk`, the root disk is excluded by
        its partition children, mounted disks by their mountpoints;
        exactly one bare whole-disk candidate must remain. VM tests
        point this at their virtio disk.
      '';
    };

    instanceStoreGlobs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "/dev/disk/by-id/nvme-Amazon_EC2_NVMe_Instance_Storage*" ];
      description = ''
        Device globs whose post-settle match classifies the node as
        instance-store and names the NVMe RAID0 member set
        (rio-kubelet-mount's jurisdiction dispatch, merged_bug_045:
        ONE decision on the settled udev view — never a unit
        `Condition*=`, which systemd evaluates on its job-start clock
        while by-id links materialize on udev's coldplug clock). VM
        tests point this at serial-tagged virtio disks.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # /var/rio provisioning is load-bearing for every build node: the
    # builder pods and the rio-mountd DaemonSet hostPath-mount
    # /var/rio/* with type Directory, so a node image without the
    # rio-{kubelet,ebs}-mount units and the tmpfiles dirs joins the
    # cluster and then wedges every build pod in ContainerCreating
    # (kubelet: "hostPath type check failed: /var/rio is not a
    # directory" — observed live 2026-06-12 when an AMI from a tree
    # predating this provisioning was deployed). node-ami-eval
    # instantiates this config in the CI gate, so this assertion turns
    # "someone refactored the provisioning away" into an eval failure
    # instead of a shipped AMI.
    assertions = [
      {
        assertion =
          config.systemd.services ? rio-kubelet-mount
          && config.systemd.services ? rio-ebs-mount
          && lib.any (lib.hasPrefix "d /var/rio ") config.systemd.tmpfiles.rules;
        message = ''
          EKS node image must provision /var/rio: rio-kubelet-mount +
          rio-ebs-mount units and the "d /var/rio" tmpfiles rules
          (builder/mountd hostPath mounts hard-require the directory;
          see eks-node.nix).
        '';
      }
    ];

    # Cilium WireGuard transparent encryption (encryption.type=
    # wireguard in addons.tf). cilium-agent loads this on demand,
    # but having it in initrd avoids a node-Ready delay.
    boot.kernelModules = [ "wireguard" ];

    # ── nix-ld: glibc shim for DaemonSet-delivered host binaries ─────
    # cilium DaemonSet hostPath-copies a glibc-linked /opt/cni/bin/
    # cilium-cni; CSI drivers (ebs-csi-node, fsx-csi when added) do
    # the same. nix-ld provides the /lib64/ld-linux* shim so these run
    # unmodified. Addons stay helm-managed (upstream owns CVE/version-
    # compat). Boot-chain components (nodeadm, kubelet, runc, ecr-
    # credential-provider) remain nix-packaged. Without this, sandbox
    # creation fails: `Could not start dynamically linked executable:
    # /opt/cni/bin/cilium-cni` (the nixpkgs stub-ld message).
    programs.nix-ld.enable = true;

    # ── containerd ────────────────────────────────────────────────────
    # Config is a build-time store path (containerd-config.nix). Every
    # value nodeadm's template would fill is constant for this AMI, so
    # there's no reason to wait on nodeadm's IMDS round-trip — containerd
    # starts at local-fs.target. Still a bespoke unit rather than nixpkgs
    # `virtualisation.containerd`: that module renders TOML via
    # `pkgs.formats.toml` which can't express the v3 single-quoted plugin
    # keys and pulls in the OCI image module.
    environment.systemPackages = [
      pkgs.containerd
      pkgs.runc
    ];

    # CNI: cilium's install-cni-binaries initContainer drops the
    # cilium-cni binary under /opt/cni/bin (tmpfiles below creates
    # the dir). The .keep file ensures /etc/cni/net.d ships in the
    # image so the hostPath mount finds a real dir, not a tmpfs.
    environment.etc = {
      "cni/net.d/.keep".text = "";
      # nodeadm's GetKubeletVersion() reads this (regex `v[0-9]+…`) before
      # falling back to `exec kubelet --version` — saves a fork during init.
      "eks/kubelet-version.txt".text = "v${cfg.kubernetesPackage.version}";
      # kubelet defaults registryPullQPS=5, registryBurst=10. Ephemeral
      # builders spawn in waves (hundreds on a fresh node within
      # seconds); each pod's IfNotPresent check triggers a manifest
      # pull (small — ~2 KB; layer blobs are prebake-warm). 5/s →
      # `pull QPS exceeded` → ErrImagePull → ImagePullBackOff. ECR's
      # own limit is 20 TPS/account/region for GetDownloadUrlForLayer,
      # 1000 TPS for BatchGetImage — 50/100 here is well under.
      "kubernetes/kubelet/config.json.d/20-rio-registry-qps.conf".text = builtins.toJSON {
        apiVersion = "kubelet.config.k8s.io/v1beta1";
        kind = "KubeletConfiguration";
        registryPullQPS = 50;
        registryBurst = 100;
      };
      # NixOS symlinks /etc/resolv.conf → systemd-resolved's stub
      # (`nameserver 127.0.0.53`). kubelet's default is to copy that into
      # pods with `dnsPolicy: Default` — coredns is one. Its Corefile
      # `forward . /etc/resolv.conf` then loops to itself → plugin/loop
      # FATAL → CrashLoopBackOff. Point kubelet at the upstream list
      # instead (VPC resolver, `10.42.0.2`). Matches kubeadm's
      # systemd-resolved handling.
      "kubernetes/kubelet/config.json.d/10-rio-resolv-conf.conf".text = builtins.toJSON {
        apiVersion = "kubelet.config.k8s.io/v1beta1";
        kind = "KubeletConfiguration";
        resolvConf = "/run/systemd/resolve/resolv.conf";
      };
      # live_060-a, the KUBELET HALF of the project-quota chain — the
      # half nothing set before: with this gate on AND /var/lib/
      # kubelet on a prjquota filesystem, kubelet assigns a project
      # ID to every emptyDir of a USER-NAMESPACED pod and the kernel
      # tracks usage O(1); rio-builder's quota.rs then reads it via
      # FS_IOC_FSGETXATTR + quotactl_fd. Derived at kubernetes 1.36
      # (the deployed kubelet): the gate exists and quota assignment
      # is userns-conditioned (kubelet refuses SupportsQuotas for
      # host-user pods); UserNamespacesSupport is on by default.
      # THE BUILDER PODS ARE NOT USER-NAMESPACED (live_063): they run
      # hostUsers:true — I-186 FUSE passthrough, pinned until P0560;
      # sec.pod.host-users-false is the DEFERRED target, and this
      # comment's previous claim that it was the standing posture was
      # one of live_063's three contradiction homes (56/56 provisioned
      # nodes, 0/1912 completions with evidence, in plain sight). So
      # kubelet's half covers other userns pods only; for the builder
      # pods rio-builder self-assigns its projid at overlay setup
      # (quota.rs ensure_project_quota, the builder-owned range below
      # kubelet's 1048576+ allocator). The gate stays on regardless:
      # it is the userns half of the chain and is inert for host-user
      # pods.
      # Without the gate kubelet falls back to ~60s du walks and
      # peak_disk_bytes is None forever — the live_060 silence.
      "kubernetes/kubelet/config.json.d/30-rio-fsquota.conf".text = builtins.toJSON {
        apiVersion = "kubelet.config.k8s.io/v1beta1";
        kind = "KubeletConfiguration";
        featureGates.LocalStorageCapacityIsolationFSQuotaMonitoring = true;
      };
    };

    # ── networking: systemd-networkd (AL2023 parity), not dhcpcd ──────
    # nixpkgs amazon-image.nix defaults to dhcpcd. dhcpcd would DHCP any
    # hot-attached interface and rewrite the default route / drop the
    # IMDS route — symptom: ecr-credential-provider, ssm-agent see `dial
    # tcp 169.254.169.254: i/o timeout`. AL2023 uses networkd with
    # ManageForeignRoutes=no: networkd leaves routes/rules it didn't
    # create alone. Under cilium cluster-pool IPAM there are NO
    # secondary ENIs (no vpc-cni ipamd), but cilium creates cilium_host
    # /cilium_net/cilium_wg0 host devices and per-pod lxc* veths —
    # ManageForeignRoutes=no + MACAddressPolicy=none keep networkd from
    # touching their routes/MACs. The 80-ec2 .network: DHCP on the
    # PRIMARY ENI only (cilium devices/veths are excluded by Kind/Name).
    networking = {
      useNetworkd = true;
      useDHCP = false;
      dhcpcd.enable = false;
    };
    systemd.network = {
      enable = true;
      config.networkConfig = {
        ManageForeignRoutes = false;
        ManageForeignRoutingPolicyRules = false;
      };
      # Prefix < "99-default" (systemd's built-in catch-all).
      # systemd.link(5) sorts ALL .link files lexically across /etc and
      # /lib; /etc only masks /lib on IDENTICAL filenames, and exactly
      # ONE .link file applies per interface (first match wins). The
      # AL2023 port originally named this 99-default (same-name mask); a
      # later "clarity" rename to 99-vpc-cni sorted AFTER and silently
      # never applied — cilium's lxc*/cilium_* veths got
      # MACAddressPolicy=persistent, a known cilium datapath breaker.
      #
      # OriginalName MUST be narrowed to cilium-created virtuals only.
      # 868c291e had `OriginalName = "*"`: that won the sort for the
      # PRIMARY ENI too, and because this file sets only
      # MACAddressPolicy (no NamePolicy), it shadowed 99-default's
      # `NamePolicy=keep kernel database onboard slot path` — primary
      # ENI stayed `eth0` instead of `ens5`, 80-ec2-primary's
      # `Name=!eth*` excluded it, no DHCP, node never joined. Secondary
      # ENIs (eni*) have hardware MACs and don't exist under
      # cluster-pool IPAM anyway; lxc*/cilium_* covers every interface
      # cilium creates (per-pod veths, lxc_health, cilium_host/_net/
      # _wg0/_geneve).
      links."80-rio-mac-none" = {
        matchConfig.OriginalName = "lxc* cilium_*";
        linkConfig.MACAddressPolicy = "none";
      };
      # DHCP the boot-time ENI; ignore hot-attached secondaries.
      networks."80-ec2-primary" = {
        matchConfig = {
          Type = "ether";
          # Primary ENI is the only ether device present when udev first
          # runs; secondaries are hot-plugged by vpc-cni post-kubelet.
          # `Kind=!*` excludes veth/vlan/bridge; `Name=!eth*` excludes
          # the vpc-cni-renamed secondaries (ipamd renames to ethN).
          Kind = "!*";
          Name = "!eth* !veth*";
        };
        networkConfig = {
          DHCP = "yes";
          # vpc-cni adds policy-routing rules; don't let a re-DHCP wipe
          # the addresses/routes ipamd installed on the primary either.
          KeepConfiguration = "yes";
          # VPC guarantees address uniqueness; kernel DAD holds the link
          # in `configuring` for ~1–2 s, blocking network-online.target →
          # nodeadm-init. AL2023 70-eks.network sets the same.
          IPv6DuplicateAddressDetection = 0;
        };
        # Cluster is ip_family=ipv6. Do NOT use RequiredFamilyForOnline=ipv4.
        linkConfig = {
          RequiredForOnline = "routable";
          # AWS VPC supports 9001-byte jumbo end-to-end; AL2023 picks this
          # up via DHCPv4 option 26, but on ipv6_native subnets there is
          # no DHCPv4 lease and the VPC RA MTU option does not reliably
          # apply before cilium-agent's auto-detect runs. Without this the
          # ENI stays at 1500 → cilium derives cilium_wg0=1420 but leaves
          # cilium_geneve=cilium_host=1500 → every full-size pod packet is
          # dropped at wg0 egress → TCP cwnd:2 → ~1 MB/s GetPath ceiling
          # (misdiagnosed twice as h2-level: c987e564, 43714578). The
          # explicit cilium MTU in addons.tf is the structural fix for the
          # geneve>wg0 inversion; this brings the underlay to jumbo so the
          # eventual pod-MTU bump has headroom.
          MTUBytes = 9001;
        };
        dhcpV4Config.UseRoutes = true;
        dhcpV6Config = {
          UseDelegatedPrefix = false;
          # Don't wait for an RA before soliciting DHCPv6 — the VPC
          # router's RA cadence adds variable latency.
          WithoutRA = "solicit";
        };
      };
    };

    systemd = {
      # AL2023 cgroup layout nodeadm assumes: kubeReservedCgroup=/runtime
      # (→ runtime.slice under cgroupDriver=systemd), systemReservedCgroup
      # =/system (→ system.slice, exists by default). containerd + kubelet
      # live under runtime.slice so kubelet's `--runtime-cgroups=/runtime.
      # slice/containerd.service` and the kubeReserved accounting both
      # resolve. Without this kubelet refuses to start ("Failed to
      # enforce Kube Reserved Cgroup Limits … cgroup [runtime] does not
      # exist").
      slices.runtime = {
        description = "Kubernetes and container runtime";
        wantedBy = [ "multi-user.target" ];
      };

      services = {
        containerd = {
          description = "containerd (EKS, build-time configured)";
          wantedBy = [ "multi-user.target" ];
          # No nodeadm dep — config is a store path. local-fs is enough.
          after = [ "local-fs.target" ];
          path = [
            pkgs.containerd
            pkgs.runc
            pkgs.iptables
          ];
          serviceConfig = {
            Slice = "runtime.slice";
            ExecStartPre = [ baseRuntimeSpec.pickExecStartPre ];
            ExecStart = "${pkgs.containerd}/bin/containerd --config ${containerdConfig}";
            Type = "notify";
            Delegate = "yes";
            KillMode = "process";
            Restart = "always";
            RestartSec = "5";
            LimitNPROC = "infinity";
            LimitCORE = "infinity";
            LimitNOFILE = "infinity";
            TasksMax = "infinity";
            OOMScoreAdjust = -999;
          };
        };

        # r[impl infra.node.prebake-layer-warm]
        # Seed import runs CONCURRENT with kubelet TLS-bootstrap+register
        # (~5–15 s), not serially before it. The ~3 s zstd unpack fits
        # inside that window. Lose-the-race fallback: containerd resolves
        # the ECR manifest and pulls every layer cold — degraded, not
        # broken (same as a stale-AMI delta pull today).
        containerd-seed-warm = lib.mkIf (cfg.seedImages != [ ]) {
          description = "Warm containerd content store with prebaked seed layers";
          wantedBy = [ "multi-user.target" ];
          after = [ "containerd.service" ];
          requires = [ "containerd.service" ];
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
            ExecStart =
              let
                ctr = "${pkgs.containerd}/bin/ctr -n k8s.io";
              in
              pkgs.writeShellScript "seed-warm" ''
                set -u
                ${lib.concatMapStringsSep "\n" (seed: ''
                  # Transfer-API import (no --local): mkSeed gzips the OCI
                  # tar (mask store-path string scanning), and --local
                  # rejects gzip ("invalid tar header"). The original Q1/Q6
                  # rationales for --local no longer apply — seeds are
                  # single-manifest and the pin label is applied separately
                  # below (same shape as the pause-image import). Seed-
                  # import failure is degraded-but-functional, so log-warn
                  # rather than fail-hard — a corrupt seed shouldn't take
                  # the node out of the pool.
                  ${ctr} image import ${seed} \
                    || echo "<4>rio: seed import ${seed} failed; first-pod pull will be cold" >&2
                '') cfg.seedImages}
                # Pin every seed.local/… ref just imported. The label stops
                # kubelet's CRI image-GC from deleting the IMAGE RECORD; the
                # record's mere existence stops containerd's content-GC from
                # deleting the LAYER BLOBS (Q8 — gc.Scheduler walks image-
                # store refs, not labels). No content-label or lease needed.
                # Derived from `ctr image ls` so it can never diverge from
                # cfg.seedImages — a hardcoded ref list here once silently
                # un-pinned a renamed image (image-GC evicted the warm).
                for ref in $(${ctr} image ls -q | ${pkgs.gnugrep}/bin/grep '^seed\.local/'); do
                  ${ctr} image label "$ref" io.cri-containerd.pinned=pinned \
                    || echo "<4>rio: seed pin $ref failed" >&2
                done
              '';
          };
        };

        # ── rio-kubelet-mount: oneshot, EARLY boot — the settled-udev
        # jurisdiction dispatcher (merged_bug_045) ────────────────────
        # ONE unit owns the prjquota kubelet-root mount for BOTH
        # storage classes (ADR-023 phase-10 instance-store RAID0;
        # live_060-a EBS quota volume — kubelet's per-pod
        # ephemeral-storage enforcement needs XFS project quotas, and
        # rio-builder's quota.rs is the disk-sizing producer that was
        # dead on 159/160 EBS-only nodes booting ext4-root).
        #
        # Its predecessors (rio-nvme-mount / rio-ebs-quota-mount)
        # partitioned jurisdiction with a complementary
        # ConditionPathExistsGlob pair on the instance-store by-id
        # link — but systemd evaluates Conditions at JOB START, on its
        # own clock, while by-id links materialize on udev's coldplug
        # clock, and each script's in-script `udevadm settle` sat
        # BELOW the gate where it could not repair the decision. Two
        # independent reads of one unsettled predicate cannot
        # atomically partition one responsibility: a late link
        # hard-blocked kubelet on one timing (the EBS unit ran on an
        # instance-store node, found no quota volume, exit 1) and
        # booted it Ready-unquota'd on the other (BOTH units skipped —
        # the live_060 silent mode the Requires= wiring exists to
        # prevent). The dispatcher settles FIRST, classifies ONCE on
        # the settled view, runs exactly one branch.
        #
        # Ordering is the load-bearing part (unchanged from the
        # predecessors): MUST mount before BOTH systemd-tmpfiles-setup
        # (hardening.nix writes the seccomp profiles into
        # /var/lib/kubelet/seccomp/) AND nodeadm-init (writes
        # /var/lib/kubelet/kubeconfig) — otherwise the fresh empty XFS
        # overmounts and shadows them → kubelet can't register /
        # builder pods CreateContainerError on the Localhost profile.
        # That rules out delegating assembly to nodeadm: its LocalDisk
        # aspect would mkfs.ext4 + mount /dev/md0 itself, AND
        # nodeadm-init runs after tmpfiles. The rio-nvme EC2NodeClass
        # DOES set instanceStorePolicy: RAID0, but only so Karpenter's
        # bin-pack sim sees NVMe capacity — `nodeadm init --skip run`
        # never executes the local-disk aspect, so this unit owns the
        # whole chain. Baked into the AMI because nodeadm only
        # consumes the NodeConfig MIME part — there is no shell
        # userData on this image (ADR-021).
        # r[impl infra.node.kubelet-prjquota+1]
        # r[impl sys.gate.static-cadence-witness]
        rio-kubelet-mount = {
          description = "Mount /var/lib/kubelet with prjquota (settled-udev dispatch: instance-store RAID0 or EBS quota volume)";
          wantedBy = [ "sysinit.target" ];
          before = [
            "systemd-tmpfiles-setup.service"
            "nodeadm-init.service"
            "kubelet.service"
          ];
          # local-fs.target: fstab mounts done. udev coldplug of
          # non-fstab block devices is async — the settle below IS the
          # jurisdiction gate's clock alignment.
          after = [ "local-fs.target" ];
          unitConfig = {
            # Early boot: drop the implicit After=basic.target so this
            # can slot between local-fs and tmpfiles-setup. NO
            # Condition*= lines — a Condition evaluates on systemd's
            # job-start clock against evidence on udev's clock (the
            # merged_bug_045 wrong-clock gate); the classification
            # happens in-script, after settle.
            DefaultDependencies = false;
          };
          path = [
            pkgs.mdadm
            pkgs.xfsprogs
            pkgs.util-linux
            pkgs.systemd # udevadm
            pkgs.nvme-cli # quota-volume-select.sh ec2_bdev_name (ADR-022 3-EBS disambiguation)
          ];
          script = ''
            set -euo pipefail
            # The evidence's own clock: drain udev's coldplug queue
            # BEFORE reading any device evidence. On multi-NVMe
            # instances (c6id/m6id.32xl: 4 disks) udev may otherwise
            # have created some-but-not-all by-id symlinks → an
            # undersized stripe with no error → premature DiskPressure
            # once Karpenter bin-packs assuming full RAID0 capacity.
            udevadm settle

            # Classify the node ONCE on the settled view:
            # instance-store links present → the NVMe branch owns
            # /var/lib/kubelet; none → the EBS quota-volume branch.
            # Partition links are rejected by name class here too (the
            # merged_bug_024 device-class law): a partitioned
            # instance-store device must never feed mdadm both the
            # disk and its partitions.
            shopt -s nullglob
            is_links=()
            for g in ${lib.escapeShellArgs cfg.instanceStoreGlobs}; do
              for m in $g; do
                case "$m" in *-part[0-9]*) continue ;; esac
                is_links+=("$m")
              done
            done
            shopt -u nullglob

            if [ "''${#is_links[@]}" -gt 0 ]; then
              # ── instance-store branch (ADR-023 phase-10) ──────────
              # udev (≥v250) creates two by-id symlinks per NVMe
              # namespace (`…_<serial>` and `…_<serial>_<nsid>`);
              # resolve and dedup so mdadm doesn't get the same
              # /dev/nvmeXn1 twice → EBUSY.
              mapfile -t devs < <(readlink -f "''${is_links[@]}" | sort -u)
              # Single-device families (e.g. m6id.large) skip md and
              # format the NVMe directly — mdadm RAID0 over one disk
              # is pure overhead.
              if [ "''${#devs[@]}" -eq 1 ]; then
                dev="''${devs[0]}"
              else
                mdadm --create /dev/md0 --run --level=0 --force \
                  --raid-devices="''${#devs[@]}" "''${devs[@]}"
                dev=/dev/md0
              fi
              # Instance store is wiped on stop/terminate → always
              # fresh. -K: don't TRIM (instance-store NVMe is
              # pre-zeroed; mkfs discard adds ~30s on multi-TB stripes
              # for nothing).
              mkfs.xfs -K -f "$dev"
            else
              # ── EBS quota-volume branch (live_060-a) ──────────────
              # Find the dedicated quota EBS volume the karpenter
              # EC2NodeClass attaches (second mapping, /dev/xvdb) by
              # TYPED device class (quota-volume-select.sh,
              # merged_bug_024: partition links rejected by name,
              # candidates must read lsblk TYPE == disk; the
              # misc-checks unit tier runs the same file against the
              # per-AMI-variant by-id namespaces). EBS persists across
              # stop/start: an existing XFS signature mounts as-is; a
              # foreign signature REFUSES (fail-closed); only a bare
              # volume is formatted.
              dev=$(${quotaVolumeSelect} ${lib.escapeShellArgs cfg.quotaVolumeGlobs})
              sig=$(blkid -o value -s TYPE "$dev" || true)
              case "$sig" in
                xfs) ;; # persisted volume from a previous boot
                "")
                  mkfs.xfs -f "$dev"
                  ;;
                *)
                  echo "rio-kubelet-mount: $dev carries a foreign '$sig' filesystem — refusing to clobber" >&2
                  exit 1
                  ;;
              esac
            fi
            mkdir -p /var/lib/kubelet
            mount -o prjquota,noatime "$dev" /var/lib/kubelet
            if [ "''${#is_links[@]}" -gt 0 ]; then
              # ADR-022 P0567: rio-mountd enforces the per-build staging
              # cap as an XFS project quota under /var/rio/staging, so
              # /var/rio must sit on a prjquota filesystem. A bind mount
              # of a subdir inherits the superblock's quota support
              # (mountd uses quotactl_fd, no block-device path needed)
              # and puts the node content caches on the instance-store
              # stripe where build I/O belongs. Subdirs come from
              # tmpfiles, which this unit orders before.
              #
              # Instance-store branch ONLY: on EBS-only nodes
              # rio-ebs-mount (below) provides the prjquota /var/rio from
              # the dedicated /dev/xvdc volume the rio-default/rio-metal
              # EC2NodeClasses attach — binding here would make
              # rio-ebs-mount's findmnt check see XFS and exit 0 with
              # xvdc never mounted. An eval-time assert cannot see the
              # runtime fs, so mountd still fails the first Mount loudly
              # (quota.rs::apply_project_quota) if neither path ran.
              mkdir -p /var/lib/kubelet/.rio /var/rio
              mount --bind /var/lib/kubelet/.rio /var/rio
            fi
          '';
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
          };
        };

        # ── rio-ebs-mount: oneshot, EARLY boot ───────────────────────
        # Counterpart of rio-kubelet-mount's instance-store /var/rio bind
        # for nodes WITHOUT instance-store NVMe (rio-default/rio-metal
        # EC2NodeClasses): the chart attaches a dedicated EBS volume at
        # /dev/xvdc (templates/karpenter.yaml, sized by
        # karpenter.rioVolumeSize) and this unit formats it XFS and
        # mounts it at /var/rio with prjquota, so rio-mountd's per-build
        # staging quota (quota.rs::apply_project_quota — fails the Mount
        # on a non-prjquota fs) works on every default node class, not
        # just the NVMe-backed ones. /var/lib/kubelet sits on the xvdb
        # quota volume here (rio-kubelet-mount's EBS branch) — only the
        # castore caches + staging move.
        #
        # No Condition*= gate: unit-level path conditions are evaluated
        # before udev coldplug finishes, so a false skip would silently
        # leave /var/rio on the unquota'd root fs (every Mount rejected).
        # Instead the script decides on evidence: it exits 0 when
        # /var/rio already sits on an XFS mount (rio-kubelet-mount's
        # instance-store branch did its job) or when DMI says the host is
        # not Amazon EC2 (the QEMU VM test). On EC2 a usable dedicated
        # volume MUST be found, otherwise the unit fails and blocks
        # kubelet (Requires= below) — a node that never joins is reaped
        # by Karpenter, which is louder and cheaper than joining and
        # infra-failing every build at the mountd handshake. Chart and
        # AMI therefore roll together (`xtask k8s -p eks up` does both).
        rio-ebs-mount = {
          description = "Mount the dedicated EBS volume at /var/rio (prjquota XFS)";
          wantedBy = [ "sysinit.target" ];
          before = [
            "systemd-tmpfiles-setup.service"
            "kubelet.service"
          ];
          # systemd-udev-trigger: with DefaultDependencies=false there
          # is no implicit ordering against udev coldplug, so on the
          # legacy-BIOS metal image this unit could start before the
          # trigger has even queued the block-device events — `udevadm
          # settle` then returns instantly and the by-id/by-label globs
          # match nothing. Wants= pulls the trigger in if it isn't part
          # of the transaction yet.
          after = [
            "local-fs.target"
            "systemd-udev-trigger.service"
            "rio-kubelet-mount.service"
          ];
          wants = [ "systemd-udev-trigger.service" ];
          unitConfig.DefaultDependencies = false;
          path = [
            pkgs.xfsprogs
            pkgs.util-linux
            pkgs.systemd # udevadm
          ];
          script = ''
            set -euo pipefail
            # Evidence first: if /var/rio already sits on an XFS mount,
            # rio-kubelet-mount's instance-store branch did its job and
            # this unit has nothing to manage. Checking the outcome
            # instead of the instance-store by-id glob means a coldplug
            # race in the dispatcher surfaces as the loud failure below
            # instead of a silent fall-through onto the root fs.
            if [ "$(findmnt -no FSTYPE /var/rio || true)" = xfs ]; then
              echo "/var/rio already on an XFS mount (rio-kubelet-mount); nothing to do"
              exit 0
            fi
            # Only EC2 hosts get the fail-hard treatment below. QEMU
            # (the VM test) and other non-EC2 boots have no dedicated
            # volume to manage and must not be blocked from booting.
            sys_vendor=$(cat /sys/class/dmi/id/sys_vendor 2>/dev/null || echo unknown)
            if [ "$sys_vendor" != "Amazon EC2" ]; then
              echo "DMI sys_vendor '$sys_vendor' is not Amazon EC2; not managing /var/rio"
              exit 0
            fi
            # Same coldplug caveat as rio-kubelet-mount: by-id/by-label
            # symlinks for non-fstab block devices appear asynchronously.
            udevadm settle
            # Prefer the label this unit wrote on a previous boot: EBS
            # persists (unlike instance store), so a node reprovisioned
            # onto the same volume reuses it — and its cache contents —
            # without re-running the device selection.
            target=""
            if [ -e /dev/disk/by-label/rio-var-rio ]; then
              target=$(readlink -f /dev/disk/by-label/rio-var-rio)
            else
              # Whole-disk EBS devices only (skip -part* and the
              # duplicate per-namespace by-id links).
              mapfile -t ebs < <(
                for link in /dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_*; do
                  [ -e "$link" ] || continue
                  dev=$(readlink -f "$link")
                  [ "$(lsblk -dno TYPE "$dev")" = disk ] && echo "$dev"
                done | sort -u
              )
              # Never touch the volume backing / nor the kubelet quota
              # volume rio-kubelet-mount already claimed (xvdb) — the
              # /var/rio volume is the remaining one (xvdc).
              root_disk=$(lsblk -no PKNAME "$(findmnt -nvo SOURCE /)" | head -n1)
              kubelet_disk=$(basename "$(findmnt -nvo SOURCE /var/lib/kubelet)" 2>/dev/null || true)
              for dev in "''${ebs[@]}"; do
                [ "$(basename "$dev")" = "$root_disk" ] && continue
                [ -n "$kubelet_disk" ] && [ "$(basename "$dev")" = "$kubelet_disk" ] && continue
                target="$dev"
                break
              done
            fi
            if [ -z "$target" ]; then
              echo "Amazon EC2 host with no dedicated /var/rio volume — the rio-default/" >&2
              echo "rio-metal EC2NodeClass must map a third EBS volume at /dev/xvdc" >&2
              echo "(helm karpenter.rioVolumeSize); refusing to run castore builds off" >&2
              echo "the unquota'd root filesystem" >&2
              exit 1
            fi
            # EBS persists across reboots (unlike instance store): only
            # format a blank device, reuse our own XFS, refuse anything
            # else rather than wiping a volume we don't recognize.
            fstype=$(blkid -o value -s TYPE "$target" || true)
            case "$fstype" in
              "") mkfs.xfs -L rio-var-rio "$target" ;;
              xfs) ;;
              *)
                echo "unexpected filesystem '$fstype' on $target; refusing to format" >&2
                exit 1
                ;;
            esac
            mkdir -p /var/rio
            mount -o prjquota,noatime "$target" /var/rio
          '';
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
          };
        };

        # ── nodeadm-init: oneshot, before kubelet ─────────────────────
        # `init --skip run --daemon kubelet`: write kubelet config only, don't
        # systemctl-start it (nodeadm assumes AL2023 unit names; ours
        # differ). `--daemon kubelet` filters the daemon list so containerd's
        # Configure() never runs — its config is build-time static now.
        nodeadm-init = {
          description = "EKS node bootstrap (nodeadm)";
          wantedBy = [ "multi-user.target" ];
          before = [ "kubelet.service" ];
          # nodeadm's IMDS client retries with backoff (aws-sdk-go
          # default); network.target is "networkd started", not "link
          # routable". The ~1–2 s wait-online gap is wasted when nodeadm
          # would just retry through it anyway. Restart=on-failure below
          # is the belt to this suspender.
          after = [ "network.target" ];
          # nodeadm shells out to `kubelet --version` (or reads /etc/eks/
          # kubelet-version.txt — populated above) and probes a few
          # AL2023 paths. tmpfiles below covers the path probes.
          path = [
            nodeadm
            cfg.kubernetesPackage
            pkgs.iproute2
          ];
          # nodeadm stat()s ecr-credential-provider before writing the
          # kubelet flags file and hard-fails if absent — there's no
          # `--skip` for it. Point it at the store binary; nodeadm then
          # writes --image-credential-provider-bin-dir=<its dirname>.
          environment.ECR_CREDENTIAL_PROVIDER_BIN_PATH = lib.getExe ecr-credential-provider;
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
            # Upstream marks `--daemon` "for testing"; if a future bump
            # drops it, the fallback is to remove the flag — nodeadm then
            # writes a harmless /etc/containerd/config.toml that nothing
            # reads (containerd's ExecStart points at the store-path
            # config). Long form required: short `-d` collides with the
            # global `-d/--development` bool and parses `kubelet` as a
            # stray positional.
            ExecStart = "${lib.getExe nodeadm} init --skip run --daemon kubelet";
            # IMDS can be briefly unreachable at very early boot on some
            # instance families; nodeadm retries internally but a unit-
            # level retry is cheap insurance for the P1 spike.
            Restart = "on-failure";
            RestartSec = "5s";
          };
        };

        # ── primary-ipv6-init: oneshot, before kubelet ────────────────
        # NLB target-type=instance + ip-address-type=dualstack registers
        # instances in an IPv6 target group, which requires each ENI to
        # have a PRIMARY IPv6 (not just the secondary the VPC assigns).
        # Neither EC2NodeClass nor managed-nodegroup launch templates can
        # set primary_ipv6 declaratively (EKS wraps user LTs and ignores
        # NetworkInterfaces). This AMI has no cloud-init/amazon-init
        # (default.nix disables both) so an EC2NodeClass userData shell
        # part is never executed — nodeadm-init only consumes the
        # NodeConfig MIME part. Set the flag here via IMDS +
        # `curl --aws-sigv4` (no awscli in the AMI); node IAM has
        # ec2:ModifyNetworkInterfaceAttribute (infra/eks/karpenter.tf).
        primary-ipv6-init = {
          description = "Set ENI primary IPv6 for NLB dualstack instance targets";
          wantedBy = [ "multi-user.target" ];
          # Ordered-before kubelet so the ENI is fixed before the node
          # registers and aws-lbc adds it as an NLB target. kubelet does
          # NOT Requires= this — failure here must not block node join.
          before = [ "kubelet.service" ];
          after = [ "network.target" ];
          path = [
            pkgs.curl
            pkgs.jq
          ];
          # IMDS via the v6 endpoint — httpProtocolIPv6: enabled in
          # EC2NodeClass MetadataOptions (karpenter.yaml). Hop-limit 1
          # lets the host netns through (token-PUT response is hop 0);
          # pod token-PUT responses TTL-expire across host→veth→pod.
          # AWS does NOT auto-set IsPrimaryIpv6 on ipv6_native subnets,
          # so this oneshot stays load-bearing for NLB dualstack
          # instance-target registration.
          script = ''
            set -uo pipefail
            imds() { curl -sf -H "X-aws-ec2-metadata-token: $TOKEN" "http://[fd00:ec2::254]/latest/meta-data/$1"; }
            TOKEN=$(curl -sf -X PUT 'http://[fd00:ec2::254]/latest/api/token' \
              -H "X-aws-ec2-metadata-token-ttl-seconds: 60")
            MAC=$(imds mac)
            ENI=$(imds "network/interfaces/macs/$MAC/interface-id")
            REGION=$(imds placement/region)
            ROLE=$(imds iam/security-credentials/ | head -n1)
            CREDS=$(imds "iam/security-credentials/$ROLE")
            AK=$(jq -r .AccessKeyId <<<"$CREDS")
            SK=$(jq -r .SecretAccessKey <<<"$CREDS")
            ST=$(jq -r .Token <<<"$CREDS")
            curl -sSf --aws-sigv4 "aws:amz:$REGION:ec2" \
              --user "$AK:$SK" \
              -H "X-Amz-Security-Token: $ST" \
              "https://ec2.$REGION.amazonaws.com/?Action=ModifyNetworkInterfaceAttribute&NetworkInterfaceId=$ENI&EnablePrimaryIpv6=true&Version=2016-11-15"
          '';
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
            # IMDS or the EC2 API can be briefly unreachable at very
            # early boot; retry a few times, then give up — the node
            # still joins, the NLB target is unhealthy until
            # `systemctl restart primary-ipv6-init` or node replace.
            Restart = "on-failure";
            RestartSec = "10s";
          };
        };

        # ── kubelet: thin unit, all config from nodeadm output ──────────
        # AL2023 parity: ExecStart is `kubelet $NODEADM_KUBELET_ARGS` —
        # nodeadm writes EVERY flag (--config, --kubeconfig, --node-ip,
        # --hostname-override, --cloud-provider, --node-labels,
        # --image-credential-provider-*, --runtime-cgroups) into
        # /etc/eks/kubelet/environment. Duplicating any of them here
        # risks drift when nodeadm bumps.
        kubelet = {
          description = "Kubernetes kubelet (EKS, nodeadm-configured)";
          wantedBy = [ "multi-user.target" ];
          after = [
            "nodeadm-init.service"
            "containerd.service"
          ];
          requires = [
            "nodeadm-init.service"
            "containerd.service"
            # The kubelet-root mount must be fail-HARD (same rationale
            # as the pause-import preStart below), in BOTH directions:
            # a Ready node with /var/lib/kubelet on root EBS gets
            # bin-packed by Karpenter against instanceStorePolicy:
            # RAID0 capacity it doesn't have → DiskPressure evictions,
            # never replaced; an EBS-only node whose quota volume is
            # missing must stay NotReady loud, never Ready with a dead
            # disk producer (live_060). ONE Requires= edge — the
            # dispatcher classifies the node in-script on the settled
            # udev view, so there is no Condition*-skip lane and no
            # both-skip window (merged_bug_045).
            "rio-kubelet-mount.service"
            # Same fail-HARD rationale for the /var/rio EBS path: a
            # Ready node whose /var/rio sits on the unquota'd root fs
            # has every build rejected at the mountd Mount handshake —
            # better to never join than to join and burn infra retries.
            # The unit exits 0 (not condition-skip) on instance-store
            # nodes and non-EC2 boots, so this Requires= only bites
            # when the dedicated xvdc volume is genuinely missing or
            # unusable.
            "rio-ebs-mount.service"
          ];
          path = [
            # kubeconfig exec-auth (nodeadm.nix patches the template to
            # use this instead of `aws eks get-token` — ~20 MB Go vs
            # ~500 MB Python; sub-100 ms vs ~1 s per token refresh).
            pkgs.aws-iam-authenticator
            pkgs.util-linux # mount/umount (volume plugins)
            pkgs.iproute2
            pkgs.iptables
            pkgs.conntrack-tools
            pkgs.ethtool
            pkgs.socat
            pkgs.coreutils
          ];
          # AL2023 sets `iptables -P FORWARD ACCEPT` so pod↔pod traffic
          # via the vpc-cni veth pairs isn't dropped by the kernel default
          # FORWARD=DROP. Seed-image import is NOT here — it's the
          # containerd-seed-warm oneshot above, concurrent with kubelet.
          preStart =
            let
              ctr = "${pkgs.containerd}/bin/ctr -n k8s.io";
            in
            ''
              ${lib.getExe' pkgs.iptables "iptables"} -P FORWARD ACCEPT -w 5
              # pause MUST land before kubelet's first sandbox; no registry
              # fallback (containerd-config.nix pins sandbox=localhost/
              # kubernetes/pause). NO `|| true`: import failure → preStart
              # exits non-zero → Restart=always re-runs preStart in 10s;
              # node stays NotReady until pause lands. A Ready-but-100%-
              # sandbox-failing node is strictly worse than NotReady.
              #
              # Label SEPARATELY: containerd 2.x transfer-API import drops
              # --label silently; --local honours it but rejects gzipped
              # docker-archive (dockerTools.buildImage default) with
              # "invalid tar header". Import via transfer-API (handles
              # gzip), then `ctr image label` — same shape as seed-warm.
              ${ctr} image import ${pauseImage}
              ${ctr} image label ${pauseRef} io.cri-containerd.pinned=pinned
            ''
            + lib.optionalString (cfg.staticPods != { }) ''
              mkdir -p /etc/kubernetes/manifests
              ${lib.concatMapStringsSep "\n" (
                name: "ln -sf ${cfg.staticPods.${name}} /etc/kubernetes/manifests/${name}.json"
              ) (lib.attrNames cfg.staticPods)}
            '';
          serviceConfig = {
            Slice = "runtime.slice";
            EnvironmentFile = "/etc/eks/kubelet/environment";
            ExecStart = "${cfg.kubernetesPackage}/bin/kubelet $NODEADM_KUBELET_ARGS";
            Restart = "always";
            RestartSec = "10s";
            RestartForceExitStatus = "SIGPIPE";
            KillMode = "process";
            CPUAccounting = true;
            MemoryAccounting = true;
          };
        };

      };

      # ── writable dirs nodeadm/aws-node expect ───────────────────────
      # nodeadm hardcodes a couple of AL2023 paths it probes (not
      # writes); vpc-cni's aws-node DaemonSet hostPath-mounts /opt/cni/
      # bin and /etc/cni/net.d and writes there. Both must exist + be
      # writable.
      tmpfiles.rules = [
        "d /etc/kubernetes/manifests 0755 root root -"
        "d /etc/cni/net.d 0755 root root -"
        "d /opt/cni/bin 0755 root root -"
        "d /var/lib/kubelet 0755 root root -"
        # live_060-a: kubelet's fsquota project registry — pkg/volume/
        # util/fsquota locks BOTH files and silently reports quotas
        # unsupported when either is missing (AL2023 ships them;
        # NixOS does not). The other half of the kubelet wiring.
        "f /etc/projects 0644 root root -"
        "f /etc/projid 0644 root root -"
        # live_060-d (empirically derived in the kubelet-projquota
        # witness): kubelet's quota applier SHELLS OUT at fixed FHS
        # paths — quotaCmds = {/sbin,/usr/sbin,/bin}/xfs_quota and
        # lsattrCmd = /usr/bin/lsattr (k8s pkg/volume/util/fsquota/
        # common/quota_common_linux_impl.go) — none of which exist on
        # NixOS, so assignment fails AFTER every other precondition
        # holds: the third silent-decline mode, invisible without the
        # witness. AL2023 ships both; these shims are the NixOS
        # equivalent.
        "d /sbin 0755 root root -"
        "L+ /sbin/xfs_quota - - - - ${pkgs.xfsprogs}/bin/xfs_quota"
        "L+ /usr/bin/lsattr - - - - ${pkgs.e2fsprogs}/bin/lsattr"
        # ADR-022 P0567: rio-mountd's working tree, hostPath-mounted by
        # the mountd DaemonSet (type: Directory — the pod refuses to
        # start if the AMI didn't create these). rio-kubelet-mount (or
        # rio-ebs-mount on nodes without instance store) runs first and
        # puts /var/rio on a prjquota XFS, so these land on the
        # instance-store stripe / the dedicated EBS volume. Root 0755:
        # mountd chowns only the per-build staging subdirs it creates.
        "d /var/rio 0755 root root -"
        "d /var/rio/castore 0755 root root -"
        "d /var/rio/staging 0755 root root -"
        "d /var/rio/cache 0755 root root -"
        "d /var/rio/chunks 0755 root root -"
      ];
    };
  };
}
