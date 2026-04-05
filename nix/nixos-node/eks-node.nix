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
#   /etc/kubernetes/kubelet/config.json   KubeletConfiguration
#   /etc/eks/kubelet/environment          KUBELET_ARGS=--node-labels=…
#   /etc/kubernetes/pki/ca.crt            cluster CA
#   /var/lib/kubelet/kubeconfig           bootstrap kubeconfig
#   /etc/containerd/config.d/*.toml       sandbox image, CNI conf dir
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
    # nixpkgs module writes /etc/containerd/config.toml from `settings`.
    # nodeadm writes a DROP-IN at /etc/containerd/config.d/ (sandbox
    # image, CNI dir). The `imports` key here makes containerd merge
    # both. SystemdCgroup=true is required for cgroup v2 + the kubelet
    # cgroupDriver nodeadm configures.
    #
    # r[impl sec.pod.host-users-false]
    # cgroup_writable=true is the ADR-012 §3 unblock for hostUsers:false
    # — Bottlerocket couldn't set this (no arbitrary containerd TOML).
    # With it, runc chowns the pod cgroup to the userns root so the
    # worker's `mkdir /sys/fs/cgroup/leaf` succeeds inside the userns.
    # values.yaml builderPoolDefaults/fetcherDefaults flip back to
    # hostUsers:false on the strength of this.
    virtualisation.containerd = {
      enable = true;
      settings = {
        version = 2;
        imports = [ "/etc/containerd/config.d/*.toml" ];
        plugins."io.containerd.grpc.v1.cri" = {
          cgroup_writable = true;
          containerd.runtimes.runc.options.SystemdCgroup = true;
        };
      };
    };

    # CNI: vpc-cni's install initContainer drops the aws-vpc-cni binary
    # alongside the reference plugins under /opt/cni/bin (tmpfiles below
    # creates the dir). The .keep file ensures /etc/cni/net.d ships in
    # the image so the hostPath mount finds a real dir, not a tmpfs.
    environment.etc."cni/net.d/.keep".text = "";

    systemd = {
      services = {
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
            pkgs.util-linux # mount/umount (volume plugins)
            pkgs.iproute2
            pkgs.iptables
            pkgs.conntrack-tools
            pkgs.ethtool
            pkgs.socat
            pkgs.cni-plugins
          ];
          environment.PATH = lib.mkForce (
            lib.makeBinPath [
              pkgs.cni-plugins
              pkgs.coreutils
              pkgs.iproute2
              pkgs.iptables
            ]
          );
          preStart =
            lib.optionalString (cfg.seedImages != [ ]) ''
              ${lib.concatMapStringsSep "\n" (
                img: "${pkgs.containerd}/bin/ctr -n k8s.io image import ${img} || true"
              ) cfg.seedImages}
            ''
            + lib.optionalString (cfg.staticPods != { }) ''
              mkdir -p /etc/kubernetes/manifests
              ${lib.concatMapStringsSep "\n" (
                name: "ln -sf ${cfg.staticPods.${name}} /etc/kubernetes/manifests/${name}.json"
              ) (lib.attrNames cfg.staticPods)}
            '';
          serviceConfig = {
            # `-` prefix: file may not exist until nodeadm-init has run;
            # Requires= ordering guarantees it does, but keep the tolerance
            # for the P2 VM test's mocked-IMDS path.
            EnvironmentFile = "-/etc/eks/kubelet/environment";
            ExecStart = lib.concatStringsSep " " [
              "${cfg.kubernetesPackage}/bin/kubelet"
              "--config=/etc/kubernetes/kubelet/config.json"
              "--kubeconfig=/var/lib/kubelet/kubeconfig"
              "--container-runtime-endpoint=unix:///run/containerd/containerd.sock"
              "$KUBELET_ARGS"
            ];
            Restart = "always";
            RestartSec = "10s";
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
        # nodeadm checks for /usr/bin/containerd (AL2023 layout). NixOS
        # has no /usr/bin; symlink to the wrapped binary so the probe
        # passes.
        "L+ /usr/bin/containerd - - - - ${pkgs.containerd}/bin/containerd"
      ];
    };
  };
}
