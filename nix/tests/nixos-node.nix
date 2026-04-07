# Boot the nix/nixos-node module tree under QEMU with a mocked IMDS so
# the EKS bootstrap path is exercised without AWS.
#
# Phase-1 had two bugs only catchable on live EC2: the nodeadm
# `-d kubelet` short-flag collision (parsed as global `-d/--development`
# bool, `kubelet` as a stray positional → "unexpected argument") and the
# T7b baseRuntimeSpec missing cwd/namespaces. The k3s VM tests caught
# the second; nothing caught the first because nothing ran nodeadm
# against a real NodeConfig outside EC2. This fixture closes that gap
# and gates Phase-2's boot-path changes (initrd-networkd, UKI, perlless)
# which are riskier.
#
# verify marker lives at the default.nix wiring point per the tracey
# convention; prose here is for humans.
{ pkgs }:
pkgs.testers.runNixOSTest {
  name = "rio-nixos-node";
  skipTypeCheck = true;
  globalTimeout = 600;

  # nix/nixos-node modules read `pins` (kernel minor, nodeadm rev) via
  # specialArgs in the AMI composition (flake.nix nodeAmi); thread it
  # the same way here so the same kernel derivation is shared (cache
  # hit on the ~40min structuredExtraConfig rebuild).
  node.specialArgs.pins = import ../pins.nix;

  nodes.node =
    {
      lib,
      pkgs,
      modulesPath,
      ...
    }:
    {
      imports = [
        ../nixos-node
        # nixos-node/default.nix sets virtualisation.amazon-init.enable
        # = mkDefault false; that option is declared by amazon-init.nix
        # which the AMI composition pulls in via amazon-image.nix.
        # Import it here so the option resolves (and stays disabled).
        (modulesPath + "/virtualisation/amazon-init.nix")
      ];

      # ── QEMU boot fixups ──────────────────────────────────────────────
      # minimal.nix strips initrd to Nitro-only (nvme via amazon-image's
      # availableKernelModules + includeDefaultModules=false). QEMU
      # needs virtio + the 9p share for the host /nix/store mount.
      boot.initrd = {
        availableKernelModules = [
          "virtio_blk"
          "virtio_pci"
          "virtio_net"
          "9p"
          "9pnet_virtio"
        ];
        # minimal.nix mkForces this to [] (Nitro autoloads from the
        # NVMe-rooted available set). qemu-vm relies on the test
        # framework's defaults; reopen with mkOverride below mkForce.
        kernelModules = lib.mkOverride 40 [ ];
      };
      # qemu-vm's direct-kernel boot doesn't go through GRUB; minimal.nix
      # mkForces timeout=0 which is harmless either way. The real
      # interaction is the 80-ec2-primary network: its `Name = "!eth*"`
      # match excludes the test framework's eth1 vlan, so the static
      # 192.168.* address the framework assigns is the only route — the
      # mocked IMDS is on lo, no DHCP needed.

      virtualisation = {
        memorySize = 2048;
        cores = 2;
      };

      # ── mock IMDS ─────────────────────────────────────────────────────
      systemd.services.mock-imds = {
        description = "Mock EC2 IMDSv2";
        wantedBy = [ "multi-user.target" ];
        before = [ "nodeadm-init.service" ];
        # nodeadm-init orders After=network.target only; bind it
        # explicitly so a mock-imds failure stops nodeadm-init from
        # entering its Restart=on-failure loop against a dead :80.
        requiredBy = [ "nodeadm-init.service" ];
        serviceConfig = {
          ExecStartPre = "${lib.getExe' pkgs.iproute2 "ip"} addr add 169.254.169.254/32 dev lo";
          ExecStart = "${pkgs.python3.interpreter} ${./fixtures/mock-imds.py}";
        };
      };
    };

  testScript = ''
    node.start()

    with subtest("mock IMDS reachable"):
        node.wait_for_unit("mock-imds.service")
        node.wait_until_succeeds(
            "${pkgs.curl}/bin/curl -fsS -X PUT "
            "-H 'X-aws-ec2-metadata-token-ttl-seconds: 21600' "
            "http://169.254.169.254/latest/api/token"
        )

    # ── KEY ASSERTION ────────────────────────────────────────────────
    # Catches `-d kubelet` class bugs: nodeadm parsed the NodeConfig
    # from mocked IMDS, enriched via instance-identity + meta-data/mac
    # + meta-data/local-ipv4, wrote /etc/eks/kubelet/environment +
    # /etc/kubernetes/kubelet/config.json, exited 0. Any flag-parse
    # regression, IMDS-shape mismatch, or new required endpoint shows
    # up here as a unit failure with the error in the journal.
    with subtest("nodeadm-init succeeds"):
        node.wait_for_unit("nodeadm-init.service")
        node.succeed("test -f /etc/eks/kubelet/environment")
        node.succeed("test -f /etc/kubernetes/kubelet/config.json")
        node.succeed("test -f /var/lib/kubelet/kubeconfig")
        node.succeed("test -f /etc/kubernetes/pki/ca.crt")
        node.succeed("grep -q -- '--node-labels=rio.build/vmtest=true' /etc/eks/kubelet/environment")

    with subtest("containerd up"):
        node.wait_for_unit("containerd.service")

    # T5: containerd's config is a build-time store path (no nodeadm
    # dep), so it MUST have started before nodeadm-init. Monotonic
    # ActiveEnterTimestamp comparison.
    with subtest("containerd started before nodeadm-init (T5 ordering)"):
        ctd = int(node.succeed(
            "systemctl show -P ActiveEnterTimestampMonotonic containerd.service"
        ).strip())
        nad = int(node.succeed(
            "systemctl show -P ActiveEnterTimestampMonotonic nodeadm-init.service"
        ).strip())
        assert ctd < nad, f"containerd ({ctd}) should activate before nodeadm-init ({nad})"

    # T7f: pick-base-runtime-spec ExecStartPre symlinks the -kvm spec
    # iff /dev/kvm is a chardev. The CI runner has KVM (nested), so
    # assert agreement rather than a fixed variant.
    with subtest("base-runtime-spec matches /dev/kvm presence (T7f)"):
        target = node.succeed("readlink /run/base-runtime-spec.json").strip()
        has_kvm = node.succeed("test -c /dev/kvm && echo y || echo n").strip() == "y"
        # base-runtime-spec.nix: withKvm=true → drv name "…-kvm.json".
        assert target.endswith("-kvm.json") == has_kvm, \
            f"/run/base-runtime-spec.json -> {target!r} (kvm={has_kvm})"

    with subtest("hardening sysctl applied"):
        node.succeed("sysctl -n user.max_user_namespaces | grep -qx 65536")
        node.succeed("test -f /var/lib/kubelet/seccomp/operator/rio-builder.json")

    # kubelet (Type=simple, Restart=always) loads NODEADM_KUBELET_ARGS
    # from /etc/eks/kubelet/environment, parses flags, loads
    # KubeletConfiguration + drop-ins, validates sysctls
    # (protectKernelDefaults=true → hardening.nix's vm.overcommit_memory
    # etc. must be present), then tries TLS-bootstrap to 127.0.0.1:6443.
    # There's no apiserver — kubelet either sits retrying or exits and
    # gets restarted. Either way the journal shows it got past config
    # load. Don't wait_for_unit: the active→failed→activating flap
    # races the test driver's poll.
    with subtest("kubelet starts under nodeadm-written config"):
        try:
            node.wait_until_succeeds(
                "journalctl -u kubelet.service "
                r"| grep -qE 'Started kubelet|Starting kubelet main sync loop|Attempting to register node'",
                timeout=60,
            )
        except Exception:
            print(node.succeed("journalctl -u kubelet.service --no-pager | tail -40"))
            raise
  '';
}
