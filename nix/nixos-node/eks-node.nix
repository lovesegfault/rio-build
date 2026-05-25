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
  options,
  pkgs,
  pins,
  ...
}:
let
  cfg = config.services.rio.eksNode;
  nodeadm = pkgs.callPackage ./nodeadm.nix { inherit pins; };
  ecr-credential-provider = pkgs.callPackage ./ecr-credential-provider.nix { inherit pins; };

  # The /var/rio fileSystems entry, factored out because it has to be
  # declared at TWO option paths (see the `fileSystems."/var/rio"`
  # comment below for what the mount is for):
  #   - `fileSystems."/var/rio"` for the real AMI;
  #   - `virtualisation.fileSystems."/var/rio"` for the qemu-vm test
  #     (nix/tests/nixos-node.nix). qemu-vm.nix replaces the WHOLE
  #     `fileSystems` attrset with `mkVMOverride` (a priority-10
  #     definition of the option, which discards every default-priority
  #     definition wholesale — not a per-attribute merge), so a mount
  #     declared only at the first path silently vanishes under the VM
  #     test and the prjquota assertion below fails.
  varRioFileSystem = {
    device = "/var/rio.img";
    fsType = "xfs";
    options = [
      "loop"
      "prjquota"
      "noatime"
      # Requires=+After= from var-rio.mount onto the image-creation
      # oneshot below.
      "x-systemd.requires=rio-var-rio-image.service"
    ];
  };

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

  # r[impl sec.pod.host-users-false]
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

    varRioSize = lib.mkOption {
      type = lib.types.str;
      default = "100G";
      example = "200G";
      description = ''
        Size of the sparse `/var/rio.img` backing file for the
        XFS-with-prjquota filesystem rio-mountd owns (shared backing
        cache, chunk cache, per-build staging, castore mountpoints).
        Sparse — root-volume blocks are only consumed as the caches
        fill, but a full /var/rio consumes this much of
        `karpenter.dataVolumeSize`, so keep
        `dataVolumeSize - varRioSize` above the largest pod
        ephemeral-storage request (see the dataVolumeSize budget note
        in infra/helm/rio-build/values.yaml). The P0571 LRU sweep
        keeps usage below this ceiling once it lands.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # ── /var/rio/staging MUST be on XFS with project quotas ──────────
    # rio-mountd (P0567) enforces per-build staging quotas with XFS
    # project quotas: `ioctl(FS_IOC_FSSETXATTR, {fsx_projid, PROJINHERIT})`
    # + `quotactl(Q_SETQUOTA)` on a mountd-assigned projid. Without
    # `prjquota` in the mount options the quotactl fails and every
    # build's staging writes are unbounded — a single build can fill
    # the node's disk. Asserted at module eval so a filesystem-layout
    # refactor that silently lands /var/rio/staging back on the ext4
    # root fails `node-ami-eval` instead of shipping an AMI with no
    # quota enforcement.
    assertions = [
      {
        assertion =
          let
            # The filesystem that will host /var/rio/staging is the
            # fileSystems entry with the longest mountPoint that is a
            # path-prefix of it.
            covers =
              fs: lib.hasPrefix (if fs.mountPoint == "/" then "/" else fs.mountPoint + "/") "/var/rio/staging/";
            stagingFs = lib.last (
              lib.sortOn (fs: lib.stringLength fs.mountPoint) (
                lib.filter covers (lib.attrValues config.fileSystems)
              )
            );
          in
          stagingFs.fsType == "xfs"
          && lib.any (o: lib.elem o stagingFs.options) [
            "prjquota"
            "pquota"
          ];
        message = ''
          services.rio.eksNode: /var/rio/staging must be hosted on an XFS
          filesystem mounted with the `prjquota` (or `pquota`) option —
          rio-mountd enforces per-build staging quotas via XFS project
          quotas, and without them staging writes are unbounded. Keep the
          `fileSystems."/var/rio"` XFS loopback declared in eks-node.nix,
          or move /var/rio onto another XFS filesystem mounted with
          prjquota.
        '';
      }
    ];

    # Host-side group owning /run/rio-mountd/mountd.sock (created 0660
    # root:rio-builder by rio-mountd). The gid is FIXED and must agree
    # with two other sites:
    #   - helm `mountd.allowedGid` → rio-mountd `--allowed-gid` (the
    #     SO_PEERCRED gate + the socket fchown)
    #   - the builder pod's group (P0559 wires the client; see the TODO
    #     at rio-controller/src/reconcilers/pool/pod.rs)
    # 990 matches nix/tests/scenarios/mountd.nix. NixOS allocates
    # dynamic system gids downward from 999 but skips explicitly-taken
    # ids, and ids.nix has no static 990, so this cannot collide.
    users.groups.rio-builder.gid = 990;

    # Cilium WireGuard transparent encryption (encryption.type=
    # wireguard in addons.tf). cilium-agent loads this on demand,
    # but having it in initrd avoids a node-Ready delay.
    boot.kernelModules = [ "wireguard" ];

    # ── /var/rio: XFS-with-prjquota for rio-mountd ───────────────────
    # The root fs is ext4 (amazon-image.nix) and carries no project-
    # quota metadata, so /var/rio gets its own XFS on a sparse loopback
    # backing file on the root volume. One filesystem covers all four
    # mountd-owned trees (cache, chunks, staging, castore): only
    # staging needs prjquota, but a dedicated fs also gives the P0571
    # LRU sweep a statvfs budget that isn't coupled to image-pull /
    # kubelet pressure on the root volume.
    #
    # Loopback (not a second EBS volume, not the instance-store RAID0):
    #   - works identically on every NodeClass (rio-default/rio-metal
    #     are EBS-only; rio-nvme's RAID0 is mounted at /var/lib/kubelet
    #     and shares its projid space with kubelet's ephemeral-storage
    #     quotas — reusing it would alias mountd's monotonic-from-1
    #     projids onto kubelet's);
    #   - no karpenter blockDeviceMappings / device-name churn.
    # The cost is the loop driver's extra page-cache copy on the
    # staging/cache write path. If that shows up in promote throughput,
    # the upgrade path is a dedicated gp3 volume formatted the same way
    # (the assertion above doesn't care where the XFS comes from).
    #
    # NOT `nofail`: a Ready builder node whose /var/rio mount failed
    # would run mountd against the unquota'd ext4 root underneath the
    # mountpoint (silent loss of the staging quota). Same fail-hard
    # rationale as kubelet's Requires=rio-nvme-mount below; Karpenter
    # replaces a node that never reaches Ready.
    fileSystems."/var/rio" = varRioFileSystem;
    # qemu-vm only (see the varRioFileSystem comment). The option does
    # not exist outside the VM-test composition, hence the guard.
    virtualisation = lib.optionalAttrs (options ? virtualisation.fileSystems) {
      fileSystems."/var/rio" = varRioFileSystem;
    };

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

        # ── rio-nvme-mount: oneshot, EARLY boot ──────────────────────
        # ADR-023 phase-10: stripe all instance-store NVMe into /dev/md0,
        # mkfs.xfs, mount at /var/lib/kubelet with prjquota so kubelet's
        # per-pod ephemeral-storage limit is enforced via XFS project
        # quotas (the default du-walk is unusable at NVMe write rates).
        #
        # Ordering is the load-bearing part. This unit MUST mount before
        # BOTH systemd-tmpfiles-setup (hardening.nix writes the seccomp
        # profiles into /var/lib/kubelet/seccomp/) AND nodeadm-init
        # (writes /var/lib/kubelet/kubeconfig) — otherwise the fresh
        # empty XFS overmounts and shadows them → kubelet can't register
        # / builder pods CreateContainerError on the Localhost profile.
        # That rules out delegating assembly to nodeadm: its LocalDisk
        # aspect would mkfs.ext4 + mount /dev/md0 itself, AND nodeadm-init
        # runs after tmpfiles. The rio-nvme EC2NodeClass DOES set
        # instanceStorePolicy: RAID0, but only so Karpenter's bin-pack sim
        # sees NVMe capacity — `nodeadm init --skip run` never executes
        # the local-disk aspect, so this unit owns the whole
        # mdadm→mkfs→mount chain.
        #
        # ConditionPathExistsGlob gates on the EC2 instance-store by-id
        # link: ebs-only nodes (rio-default/rio-metal NodeClass) skip
        # cleanly. Baked into the AMI because nodeadm only consumes the
        # NodeConfig MIME part — there is no shell userData on this
        # image (ADR-021).
        rio-nvme-mount = {
          description = "Mount instance-store NVMe RAID0 at /var/lib/kubelet (prjquota)";
          wantedBy = [ "sysinit.target" ];
          before = [
            "systemd-tmpfiles-setup.service"
            "nodeadm-init.service"
            "kubelet.service"
          ];
          # local-fs.target: fstab mounts done. udev coldplug of NON-fstab
          # NVMe is async — settle in-script before enumerating.
          after = [ "local-fs.target" ];
          unitConfig = {
            # Early boot: drop the implicit After=basic.target so this
            # can slot between local-fs and tmpfiles-setup.
            DefaultDependencies = false;
            ConditionPathExistsGlob = "/dev/disk/by-id/nvme-Amazon_EC2_NVMe_Instance_Storage*";
          };
          path = [
            pkgs.mdadm
            pkgs.xfsprogs
            pkgs.util-linux
            pkgs.systemd # udevadm
          ];
          script = ''
            set -euo pipefail
            # local-fs.target orders after fstab mounts, NOT udev coldplug
            # of non-fstab block devices; with DefaultDependencies=false
            # there is no implicit After=sysinit.target either.
            # ConditionPathExistsGlob needs ≥1 match, so on multi-NVMe
            # instances (c6id/m6id.32xl: 4 disks) udev may have created
            # some-but-not-all by-id symlinks → mdadm builds an undersized
            # stripe with no error → premature DiskPressure once Karpenter
            # bin-packs assuming full instanceStorePolicy:RAID0 capacity.
            udevadm settle
            # udev (≥v250) creates two by-id symlinks per NVMe namespace
            # (`…_<serial>` and `…_<serial>_<nsid>`); resolve and dedup so
            # mdadm doesn't get the same /dev/nvmeXn1 twice → EBUSY.
            mapfile -t devs < <(readlink -f /dev/disk/by-id/nvme-Amazon_EC2_NVMe_Instance_Storage* | sort -u)
            # Single-device families (e.g. m6id.large) skip md and format
            # the NVMe directly — mdadm RAID0 over one disk is pure
            # overhead.
            if [ "''${#devs[@]}" -eq 1 ]; then
              dev="''${devs[0]}"
            else
              mdadm --create /dev/md0 --run --level=0 --force \
                --raid-devices="''${#devs[@]}" "''${devs[@]}"
              dev=/dev/md0
            fi
            # Instance store is wiped on stop/terminate → always fresh.
            # -K: don't TRIM (instance-store NVMe is pre-zeroed; mkfs
            # discard adds ~30s on multi-TB stripes for nothing).
            mkfs.xfs -K -f "$dev"
            mkdir -p /var/lib/kubelet
            mount -o prjquota,noatime "$dev" /var/lib/kubelet
          '';
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
          };
        };

        # ── rio-var-rio-image: oneshot, before var-rio.mount ──────────
        # Creates and formats the sparse XFS backing file for the
        # declarative fileSystems."/var/rio" entry above. Pulled in by
        # the mount unit's `x-systemd.requires=`; a separate service
        # because mount units cannot run ExecStartPre.
        #
        # Idempotent across reboots: the EBS root persists over
        # stop/start, so a formatted image is detected (blkid) and left
        # alone — re-running mkfs would wipe the warm caches.
        rio-var-rio-image = {
          description = "Create the /var/rio XFS-prjquota backing image";
          before = [ "var-rio.mount" ];
          unitConfig = {
            # var-rio.mount is Before=local-fs.target, which is before
            # basic.target — the default service dependencies
            # (After=basic.target) would cycle.
            DefaultDependencies = false;
          };
          # The truncate is sparse so it doesn't need the grown root,
          # but mkfs.xfs writes the journal (~varRioSize/2048 of real
          # blocks) — order after the root partition+fs growth so a
          # `diskSize = "auto"`-built image with minimal slack can't
          # ENOSPC on first boot. Both After= edges are no-ops when the
          # unit doesn't exist (QEMU VM tests) or is skipped.
          after = [
            "growpart.service"
            "systemd-growfs-root.service"
          ];
          path = [
            pkgs.kmod
            pkgs.xfsprogs
            pkgs.util-linux
          ];
          script = ''
            set -euo pipefail
            # `mount -o loop` opens /dev/loop-control, which only
            # exists once the loop module is loaded. modules-load.d has
            # no ordering against local-fs.target, so load it here.
            modprobe loop
            [ -f /var/rio.img ] || truncate -s ${cfg.varRioSize} /var/rio.img
            # Already formatted (reboot of a persistent EBS root) →
            # keep the warm caches.
            blkid /var/rio.img >/dev/null || mkfs.xfs -q /var/rio.img
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
            # NVMe mount failure must be fail-HARD (same rationale as
            # the pause-import preStart below): a Ready node with
            # /var/lib/kubelet on root EBS gets bin-packed by Karpenter
            # against instanceStorePolicy:RAID0 capacity it doesn't
            # have → DiskPressure evictions, never replaced.
            # systemd.unit(5): Requires= on a Condition*-skipped unit
            # is satisfied (job result `condition`), so EBS-only
            # NodeClasses (rio-default/rio-metal) are unaffected.
            "rio-nvme-mount.service"
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
      #
      # /var/rio/{cache,chunks,staging,castore}: the four mountd-owned
      # trees (P0567/P0571). rio-mountd open()s them O_DIRECTORY at
      # startup and the mountd-ds.yaml hostPath mounts use
      # `type: Directory` so a missing dir is a loud scheduling failure,
      # not a kubelet-created root-owned surprise. tmpfiles-setup runs
      # After=local-fs.target, which waits for var-rio.mount (not
      # `nofail`), so these land on the XFS — never on the root fs
      # underneath the mountpoint.
      tmpfiles.rules = [
        "d /etc/kubernetes/manifests 0755 root root -"
        "d /etc/cni/net.d 0755 root root -"
        "d /opt/cni/bin 0755 root root -"
        "d /var/lib/kubelet 0755 root root -"
        "d /var/rio/cache 0755 root root -"
        "d /var/rio/chunks 0755 root root -"
        "d /var/rio/staging 0755 root root -"
        "d /var/rio/castore 0755 root root -"
      ];
    };
  };
}
