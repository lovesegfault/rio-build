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
# is invoked with `-d kubelet` and never touches /etc/containerd/.
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

  # containerd-config.nix pins sandbox = "localhost/kubernetes/pause" and
  # expects the AMI bake to have pre-loaded it (templates/shared/runtime/
  # bin/cache-pause-container in the AL2023 builder). Build the pause binary
  # from the same kubernetes derivation kubelet comes from and wrap it as
  # a single-layer OCI tarball; kubelet preStart `ctr image import`s it.
  # The CRI pinned label is set on import so kubelet's image-GC won't
  # evict it.
  pauseImage = pkgs.dockerTools.buildImage {
    name = "localhost/kubernetes/pause";
    tag = "latest";
    copyToRoot = [ cfg.kubernetesPackage.pause ];
    config.Entrypoint = [ "/bin/pause" ];
  };

  pauseRef = "localhost/kubernetes/pause:latest";
  # r[impl sec.pod.host-users-false]
  containerdConfig = import ./containerd-config.nix { inherit lib pkgs pauseRef; };
in
{
  options.services.rio.eksNode = {
    enable = lib.mkEnableOption "EKS worker node bootstrap via nodeadm";

    kubernetesPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.kubernetes;
      description = ''
        kubelet binary source. nixpkgs `kubernetes` tracks the version in
        `nix/pins.nix` `kubernetes_version` (both follow upstream minor).
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
  };

  config = lib.mkIf cfg.enable {
    # ── nix-ld: glibc shim for DaemonSet-delivered host binaries ─────
    # aws-node DaemonSet hostPath-copies a glibc-linked /opt/cni/bin/
    # aws-cni; CSI drivers (ebs-csi-node, fsx-csi when added) do the
    # same. nix-ld provides the /lib64/ld-linux* shim so these run
    # unmodified. Addons stay EKS-managed (AWS owns CVE/version-compat).
    # Boot-chain components (nodeadm, kubelet, runc, ecr-credential-
    # provider) remain nix-packaged. Without this, sandbox creation
    # fails: `Could not start dynamically linked executable: /opt/cni/
    # bin/aws-cni` (the nixpkgs stub-ld message).
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

    # CNI: vpc-cni's install initContainer drops the aws-vpc-cni binary
    # alongside the reference plugins under /opt/cni/bin (tmpfiles below
    # creates the dir). The .keep file ensures /etc/cni/net.d ships in
    # the image so the hostPath mount finds a real dir, not a tmpfs.
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
    };

    # ── networking: systemd-networkd (AL2023 parity), not dhcpcd ──────
    # nixpkgs amazon-image.nix defaults to dhcpcd. When vpc-cni attaches
    # a secondary ENI, dhcpcd DHCPs it and rewrites the default route /
    # drops the IMDS route — symptom: pods schedule for ~1 min, then
    # ipamd, ecr-credential-provider, ssm-agent all see `dial tcp
    # 169.254.169.254: i/o timeout`. AL2023 uses networkd with
    # ManageForeignRoutes=no (templates/al2023/runtime/rootfs/etc/systemd/
    # networkd.conf.d/80-release.conf): networkd leaves routes/rules it
    # didn't create alone, so vpc-cni's policy routing survives interface
    # churn. MACAddressPolicy=none is the vpc-cni #2103 fix (the udev
    # default `persistent` would rewrite secondary-ENI MACs and break
    # ip-rule matching). The 80-ec2 .network: DHCP on the PRIMARY ENI
    # only (PermanentMACAddress matches the boot-time NIC; secondaries
    # vpc-cni hot-attaches arrive later and don't match) — secondaries
    # are vpc-cni's to configure.
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
      links."99-vpc-cni" = {
        matchConfig.OriginalName = "*";
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
        linkConfig.RequiredForOnline = "routable";
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
                  # --local: containerd 2.x transfer-API path drops --label and
                  # handles multi-manifest ref.name annotations differently;
                  # --local forces the legacy client-side path which honours
                  # both (PLAN-PREBAKE Q1/Q6). Seed-import failure is degraded-
                  # but-functional, so log-warn rather than fail-hard — a
                  # corrupt seed shouldn't take the node out of the pool.
                  ${ctr} image import --local ${seed} \
                    || echo "<4>rio: seed import ${seed} failed; first-pod pull will be cold" >&2
                '') cfg.seedImages}
                # Pin both seed.local/…:prebaked refs. The label stops
                # kubelet's CRI image-GC from deleting the IMAGE RECORD; the
                # record's mere existence stops containerd's content-GC from
                # deleting the LAYER BLOBS (Q8 — gc.Scheduler walks image-
                # store refs, not labels). No content-label or lease needed.
                for ref in seed.local/rio-builder:prebaked seed.local/rio-fetcher:prebaked; do
                  ${ctr} image label "$ref" io.cri-containerd.pinned=pinned || true
                done
              '';
          };
        };

        # ── nodeadm-init: oneshot, before kubelet ─────────────────────
        # `init --skip run -d kubelet`: write kubelet config only, don't
        # systemctl-start it (nodeadm assumes AL2023 unit names; ours
        # differ). `-d kubelet` filters the daemon list so containerd's
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
            # Upstream marks `-d` "for testing"; if a future bump drops it,
            # the fallback is to remove the flag — nodeadm then writes a
            # harmless /etc/containerd/config.toml that nothing reads
            # (containerd's ExecStart points at the store-path config).
            ExecStart = "${lib.getExe nodeadm} init --skip run -d kubelet";
            # IMDS can be briefly unreachable at very early boot on some
            # instance families; nodeadm retries internally but a unit-
            # level retry is cheap insurance for the P1 spike.
            Restart = "on-failure";
            RestartSec = "5s";
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
              # kubernetes/pause).
              ${ctr} image import --label io.cri-containerd.pinned=pinned ${pauseImage} || true
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
      ];
    };
  };
}
