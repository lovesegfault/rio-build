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
      # qemu-vm's direct-kernel boot bypasses the loader entirely; uki-
      # boot.nix's external installHook is inert here. The real
      # interaction is the 80-ec2-primary network: its `Name = "!eth*"`
      # match excludes the test framework's eth1 vlan, so the static
      # 192.168.* address the framework assigns is the only route — the
      # mocked IMDS is on lo, no DHCP needed.

      virtualisation = {
        memorySize = 2048;
        cores = 2;
        # nodeadm's KubeletConfiguration reserves ~1.1 GiB ephemeral-
        # storage; the qemu-vm default ~1 GiB disk fails kubelet's
        # NodeAllocatable check ("reservation > capacity") and kubelet
        # exits 1. 4096 matches common.nix's worker default.
        diskSize = 4096;
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
          # `replace` (not `add`): idempotent if the unit restarts.
          ExecStartPre = "${lib.getExe' pkgs.iproute2 "ip"} addr replace 169.254.169.254/32 dev lo";
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

    # kubelet loads NODEADM_KUBELET_ARGS from /etc/eks/kubelet/
    # environment, parses flags, loads KubeletConfiguration + drop-ins,
    # validates sysctls (protectKernelDefaults=true → hardening.nix's
    # vm.overcommit_memory etc. must be present), starts the
    # ContainerManager (NodeAllocatable check passes — diskSize above),
    # then sits retrying registration to 127.0.0.1:6443. No apiserver →
    # registration never succeeds, but the process stays active.
    with subtest("kubelet starts under nodeadm-written config"):
        node.wait_for_unit("kubelet.service")

    with subtest("kubelet resolvConf points past systemd-resolved stub"):
        # Without this drop-in, kubelet copies the stub (127.0.0.53) into
        # dnsPolicy=Default pods and coredns forward-loops on itself.
        # Assert: drop-in present, its target exists with a non-loopback
        # nameserver, and the stub it bypasses DOES contain the loopback
        # (so removing the drop-in would reintroduce the bug).
        import json
        dropin = json.loads(node.succeed(
            "cat /etc/kubernetes/kubelet/config.json.d/10-rio-resolv-conf.conf"
        ))
        assert dropin["resolvConf"] == "/run/systemd/resolve/resolv.conf", dropin
        upstream = node.succeed("cat /run/systemd/resolve/resolv.conf")
        assert "127.0.0." not in upstream, f"upstream resolv.conf still loopback:\n{upstream}"
        assert "nameserver " in upstream, f"upstream resolv.conf has no nameserver:\n{upstream}"
        stub = node.succeed("cat /run/systemd/resolve/stub-resolv.conf")
        assert "127.0.0.53" in stub, f"stub no longer loopback (precondition changed):\n{stub}"

    # bug_364: 99-vpc-cni sorted AFTER systemd's built-in 99-default.link
    # (MACAddressPolicy=persistent catch-all) and never applied. Renamed
    # to 80-rio-mac-none. udevadm test-builtin prints which .link file
    # matched; eth0 is the qemu test framework's primary nic.
    with subtest("MACAddressPolicy=none .link sorts before systemd 99-default"):
        out = node.succeed(
            "SYSTEMD_LOG_LEVEL=debug udevadm test-builtin net_setup_link "
            "/sys/class/net/eth0 2>&1 || true"
        )
        assert "80-rio-mac-none.link" in out, f"expected rio .link to apply, got:\n{out}"

    # bug_479: local-fs.target does NOT order after udev coldplug of
    # non-fstab block devices. Multi-NVMe instances could enumerate a
    # partial set → undersized RAID0 stripe. The QEMU VM has no AWS
    # instance-store NVMe (ConditionPathExistsGlob → unit skipped), so
    # assert structurally on the rendered script: settle precedes glob.
    # `systemctl cat` shows ExecStart= as a store-path; cat that.
    with subtest("rio-nvme-mount settles udev before enumerating"):
        script = node.succeed(
            "cat $(systemctl show -P ExecStart rio-nvme-mount.service "
            "| grep -oE '/nix/store/[^ ;]+')"
        )
        assert script.index("udevadm settle") < script.index(
            "nvme-Amazon_EC2_NVMe_Instance_Storage"
        ), f"udevadm settle missing or after device glob:\n{script}"

    # rio-nvme-mount script failure (mdadm/mkfs/mount) must NOT be
    # fail-open: with only before= ordering + wantedBy=sysinit.target,
    # tmpfiles would create /var/lib/kubelet on root EBS, kubelet
    # starts, node Ready, Karpenter bin-packs against phantom RAID0
    # capacity. QEMU has no instance-store NVMe (Condition skips the
    # unit) so assert structurally on the rendered Requires=, then
    # prove kubelet started anyway — Condition-skip satisfies
    # Requires= (systemd.unit(5)), so EBS-only nodes are unaffected.
    with subtest("rio-nvme-mount failure blocks kubelet (fail-hard)"):
        deps = node.succeed("systemctl show -p Requires kubelet.service")
        assert "rio-nvme-mount.service" in deps, (
            f"kubelet does not Requires=rio-nvme-mount — NVMe mount "
            f"failure would be fail-open (Ready node on root EBS). {deps}"
        )
        node.succeed("systemctl is-active kubelet.service")

    # bug_054: pause import previously had `|| true` and lacked --local.
    # With sandbox=localhost/kubernetes/pause there is no registry
    # fallback, so a swallowed failure left a Ready-but-100%-failing
    # node. kubelet.service waited above; assert the import landed AND
    # the pinned label survived (the --local fix).
    with subtest("pause image imported and pinned"):
        node.succeed(
            "ctr -n k8s.io image ls "
            "| grep 'localhost/kubernetes/pause' "
            "| grep -q 'io.cri-containerd.pinned=pinned'"
        )
  '';
}
