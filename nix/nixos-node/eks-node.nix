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
#   /etc/containerd/config.toml           full containerd config (NOT a drop-in)
#   /etc/containerd/base-runtime-spec.json
#   /etc/eks/image-credential-provider/config.json
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
  smarter-device-manager = pkgs.callPackage ./smarter-device-manager { inherit pins; };
  ecr-credential-provider = pkgs.callPackage ./ecr-credential-provider.nix { inherit pins; };

  # r[impl sec.pod.fuse-device-plugin]
  # Single source of truth for the fuse + kvm devicematch list. Mirrors
  # _helpers.tpl `rio.devicePluginConf` (chart-side, k3s DaemonSet path)
  # — the helm-lint `device-plugin-conf-parity` assertion diffs the two.
  # nummaxdevices: per-node ceiling. ^kvm$ matches only on .metal; the
  # plugin advertises 0 where /dev/kvm is absent.
  devicePluginConf = pkgs.writeText "conf.yaml" ''
    - devicematch: ^fuse$
      nummaxdevices: ${toString cfg.devicePlugin.fuseMaxDevices}
    - devicematch: ^kvm$
      nummaxdevices: ${toString cfg.devicePlugin.kvmMaxDevices}
  '';

  # nodeadm hard-codes sandbox = "localhost/kubernetes/pause" and expects
  # the AMI bake to have pre-loaded it (templates/shared/runtime/bin/
  # cache-pause-container in the AL2023 builder). Build the pause binary
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

  # r[impl sec.pod.host-users-false]
  # cgroup_writable=true is the ADR-012 §3 unblock for hostUsers:false —
  # Bottlerocket couldn't set this (no arbitrary containerd TOML). With
  # it, runc chowns the pod cgroup to the userns root so the worker's
  # `mkdir /sys/fs/cgroup/leaf` succeeds inside the userns. values.yaml
  # builderPoolDefaults/fetcherDefaults flip back to hostUsers:false on
  # the strength of this. Lives in a drop-in (nodeadm owns the main
  # config.toml; its template is patched to `imports` this dir).
  containerdDropIn = pkgs.writeText "10-rio.toml" ''
    version = 3
    [plugins.'io.containerd.cri.v1.runtime']
    cgroup_writable = true
  '';
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

    devicePlugin = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Run smarter-device-manager as a host systemd unit. Replaces the
          Bottlerocket static-pod path: no registry pull, no kubelet-
          schedules-its-own-dependency loop. Registers on
          /var/lib/kubelet/device-plugins/kubelet.sock and advertises
          smarter-devices/{fuse,kvm}. The Karpenter NodeOverlay STILL
          declares synthetic capacity (cold-start bin-packing happens
          before any node — and therefore this unit — exists).
        '';
      };
      fuseMaxDevices = lib.mkOption {
        type = lib.types.ints.positive;
        default = 100;
        description = "Per-node smarter-devices/fuse ceiling.";
      };
      kvmMaxDevices = lib.mkOption {
        type = lib.types.ints.positive;
        default = 100;
        description = "Per-node smarter-devices/kvm ceiling (metal only).";
      };
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
        OCI tarballs to `ctr image import` before kubelet starts, so
        static pods don't round-trip a registry. Pattern cribbed from
        nixpkgs `services.kubernetes.kubelet.seedDockerImages`.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # ── containerd ────────────────────────────────────────────────────
    # nodeadm OWNS /etc/containerd/config.toml — it writes the whole file
    # (sandbox image, base-runtime-spec, CNI dirs, runc BinaryName,
    # SystemdCgroup) from a template every boot. The nixpkgs
    # `virtualisation.containerd` module would point ExecStart at a
    # build-time store path and ignore nodeadm's file, so use a thin
    # bespoke unit instead. Our one TOML addition (cgroup_writable, see
    # `containerdDropIn` above) goes in config.d/; nodeadm's template is
    # patched (nodeadm.nix postPatch) to `imports` that dir.
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
      "containerd/config.d/10-rio.toml".source = containerdDropIn;
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
        # ── containerd: nodeadm-configured ────────────────────────────
        containerd = {
          description = "containerd (EKS, nodeadm-configured)";
          wantedBy = [ "multi-user.target" ];
          after = [
            "network.target"
            "nodeadm-init.service"
          ];
          requires = [ "nodeadm-init.service" ];
          path = [
            pkgs.containerd
            pkgs.runc
            pkgs.iptables
          ];
          serviceConfig = {
            Slice = "runtime.slice";
            ExecStart = "${pkgs.containerd}/bin/containerd --config /etc/containerd/config.toml";
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

        # ── nodeadm-init: oneshot, before containerd/kubelet ──────────
        # `init --skip run`: write configs, don't try to systemctl-start
        # kubelet (nodeadm assumes AL2023 unit names; ours differ).
        nodeadm-init = {
          description = "EKS node bootstrap (nodeadm)";
          wantedBy = [ "multi-user.target" ];
          before = [
            "containerd.service"
            "kubelet.service"
          ];
          after = [ "network-online.target" ];
          wants = [ "network-online.target" ];
          # nodeadm shells out to `containerd --version` / `kubelet
          # --version` for telemetry fields and probes a few AL2023 paths.
          # PATH covers the binaries; tmpfiles below covers the path probes.
          path = [
            nodeadm
            cfg.kubernetesPackage
            pkgs.containerd
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
            ExecStart = "${lib.getExe nodeadm} init --skip run";
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
          # FORWARD=DROP. Then seed images: pause MUST land before kubelet
          # creates its first sandbox (nodeadm pins sandbox=localhost/
          # kubernetes/pause — there is no registry to fall back to).
          preStart = ''
            ${lib.getExe' pkgs.iptables "iptables"} -P FORWARD ACCEPT -w 5
            ${lib.concatMapStringsSep "\n" (
              img:
              "${pkgs.containerd}/bin/ctr -n k8s.io image import --label io.cri-containerd.pinned=pinned ${img} || true"
            ) ([ pauseImage ] ++ cfg.seedImages)}
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

        # ── smarter-device-manager (host unit, not pod) ─────────────────
        # After=kubelet: the plugin's Register RPC dials /var/lib/kubelet/
        # device-plugins/kubelet.sock, which kubelet creates on startup.
        # partOf=kubelet: a kubelet restart bounces the plugin so it re-
        # registers on the fresh socket (the binary's fsnotify watch only
        # covers socket DELETION, not the inode swap kubelet does on a
        # clean restart). -config points at a store path — the conf is
        # immutable per-AMI; no /etc indirection needed.
        smarter-device-manager = lib.mkIf cfg.devicePlugin.enable {
          description = "smarter-device-manager (fuse + kvm extended resources)";
          wantedBy = [ "multi-user.target" ];
          after = [ "kubelet.service" ];
          partOf = [ "kubelet.service" ];
          serviceConfig = {
            ExecStart = "${lib.getExe smarter-device-manager} -logtostderr -v=0 -config=${devicePluginConf}";
            # Registers a Unix socket under /var/lib/kubelet/device-plugins/
            # then serves Allocate RPCs. No state of its own; restart is
            # cheap (kubelet re-queries ListAndWatch).
            Restart = "always";
            RestartSec = "5s";
            # Host /dev access. The plugin only stat()s + advertises; the
            # actual device-node injection into pod cgroups is kubelet's
            # job (DevicePlugin Allocate response → CRI). No CAP_SYS_ADMIN
            # needed here.
            ProtectSystem = "strict";
            ReadWritePaths = [ "/var/lib/kubelet/device-plugins" ];
            PrivateTmp = true;
          };
        };
      };

      # ── path shims + writable dirs nodeadm/aws-node expect ──────────
      # nodeadm hardcodes a couple of AL2023 paths it probes (not
      # writes); vpc-cni's aws-node DaemonSet hostPath-mounts /opt/cni/
      # bin and /etc/cni/net.d and writes there. Both must exist + be
      # writable.
      tmpfiles.rules = [
        "d /etc/containerd/config.d 0755 root root -"
        "d /etc/kubernetes/manifests 0755 root root -"
        "d /etc/cni/net.d 0755 root root -"
        "d /opt/cni/bin 0755 root root -"
        "d /var/lib/kubelet 0755 root root -"
        "d /var/lib/kubelet/device-plugins 0755 root root -"
        # AL2023 path shims. nodeadm probes /usr/bin/containerd; its
        # containerd template hard-codes BinaryName=/usr/sbin/runc (no
        # env override — runtime_config.go defaultRuntimeBinaryPath).
        # Symlink rather than patch: keeps nodeadm.nix's diff to the one
        # `imports` splice, and the containerd v2 shim PATH-resolves runc
        # for everything except the explicit BinaryName.
        "L+ /usr/bin/containerd - - - - ${pkgs.containerd}/bin/containerd"
        "L+ /usr/sbin/runc - - - - ${lib.getExe pkgs.runc}"
      ];
    };
  };
}
