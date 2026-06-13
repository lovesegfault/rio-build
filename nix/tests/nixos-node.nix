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
        # live_060-a: the quota volume rio-kubelet-mount's EBS branch
        # provisions (the VM stand-in for the karpenter second EBS
        # mapping) — kubelet Requires= the mount, so the existing
        # kubelet-starts assertion is the integration proof. 4096:
        # the ephemeral-storage reservation check moves to THIS fs
        # (it hosts /var/lib/kubelet).
        emptyDiskImages = [ 4096 ];
      };

      # The VM's bare virtio disk is the quota volume (no EBS by-id
      # links under QEMU).
      services.rio.eksNode.quotaVolumeGlobs = [ "/dev/vdb" ];

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

    # live_060-a: the EBS-quota provisioning half, asserted from the
    # node config the fleet boots (the end-to-end kubelet-projid
    # witness is the dedicated k3s scenario; this pins the AMI side).
    # This QEMU node has no instance-store links, so the dispatcher's
    # settled classification takes the EBS branch (merged_bug_045:
    # one unit, one decision — the variant/timing battery is
    # vm-ami-variant-quota).
    with subtest("quota volume mounted prjquota at the kubelet root"):
        node.wait_for_unit("rio-kubelet-mount.service")
        out = node.succeed("findmnt -no FSTYPE,OPTIONS /var/lib/kubelet")
        assert "xfs" in out and "prjquota" in out, f"kubelet root not prjquota-xfs: {out!r}"

    with subtest("kubelet fsquota half present (gate + projid registry)"):
        import json
        gate = json.loads(node.succeed(
            "cat /etc/kubernetes/kubelet/config.json.d/30-rio-fsquota.conf"
        ))
        assert gate["featureGates"]["LocalStorageCapacityIsolationFSQuotaMonitoring"] is True, gate
        node.succeed("test -f /etc/projects")
        node.succeed("test -f /etc/projid")
        # The FHS shims kubelet's quota applier execs at fixed paths
        # (live_060-d: the witness-derived third silent-decline mode).
        node.succeed("test -x /sbin/xfs_quota")
        node.succeed("test -x /usr/bin/lsattr")

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
        # Yama descendants-only tracing is load-bearing for the seccomp
        # profile's ptrace/process_vm_readv allow (hardening.nix pins
        # the sysctl; the kernel default happens to match but is not a
        # contract). 0 would drop the confinement that makes the allow
        # acceptable; 2/3 would re-break sanitizer/debugger check
        # phases.
        node.succeed("sysctl -n kernel.yama.ptrace_scope | grep -qx 1")
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

    # bug_364 + 868c291e regression: 80-rio-mac-none had
    # OriginalName="*", which won the lexical sort for EVERY interface
    # including the primary ENI. systemd.link(5): exactly one .link
    # file applies; 80-rio-mac-none sets only MACAddressPolicy, so it
    # shadowed 99-default's NamePolicy → primary ENI kept kernel name
    # eth0 → 80-ec2-primary's Name=!eth* excluded it → no DHCP → node
    # never joined. Match is now narrowed to lxc*/cilium_*. Assert BOTH
    # directions via udevadm test-builtin (prints which .link matched).
    #
    # qemu-vm.nix + test-instrumentation.nix hard-set
    # usePredictableInterfaceNames=false (net.ifnames=0) so the actual
    # ens5 rename can't be observed here without breaking the test
    # framework's own eth1 vlan addressing; the .link-file selection IS
    # observable and is what determines the EC2 rename.
    with subtest("primary nic falls through to 99-default (NamePolicy intact)"):
        out = node.succeed(
            "SYSTEMD_LOG_LEVEL=debug udevadm test-builtin net_setup_link "
            "/sys/class/net/eth0 2>&1 || true"
        )
        # Debug output lists every .link as "Parsed configuration file";
        # match the "is applied" line (the one that won the sort).
        assert "/99-default.link is applied" in out, (
            f"primary nic should match systemd's 99-default.link "
            f"(NamePolicy=...slot path); on EC2 this renames eth0->ens5 "
            f"so 80-ec2-primary DHCPs it:\n{out}"
        )
        assert "80-rio-mac-none.link is applied" not in out, (
            f"80-rio-mac-none must NOT match the primary nic — would "
            f"shadow NamePolicy and break DHCP:\n{out}"
        )

    with subtest("cilium-pattern veth still gets 80-rio-mac-none"):
        node.succeed("ip link add lxc-probe type veth peer name cilium_probe")
        for dev in ("lxc-probe", "cilium_probe"):
            out = node.succeed(
                "SYSTEMD_LOG_LEVEL=debug udevadm test-builtin net_setup_link "
                f"/sys/class/net/{dev} 2>&1 || true"
            )
            assert "80-rio-mac-none.link is applied" in out, (
                f"expected rio .link (MACAddressPolicy=none) on {dev}:\n{out}"
            )
        node.succeed("ip link del lxc-probe")

    # bug_479 + merged_bug_045: local-fs.target does NOT order after
    # udev coldplug of non-fstab block devices, and a unit Condition
    # evaluates at job start on systemd's clock — so the dispatcher
    # carries NO Condition and settles IN-SCRIPT before classifying.
    # Assert structurally on the rendered script: settle precedes the
    # jurisdiction read (the instance-store glob) and the dispatcher
    # renders condition-free. `systemctl show -P ExecStart` gives the
    # store-path script; cat that.
    with subtest("rio-kubelet-mount settles udev before classifying, condition-free"):
        script = node.succeed(
            "cat $(systemctl show -P ExecStart rio-kubelet-mount.service "
            "| grep -oE '/nix/store/[^ ;]+')"
        )
        assert script.index("udevadm settle") < script.index(
            "nvme-Amazon_EC2_NVMe_Instance_Storage"
        ), f"udevadm settle missing or after the jurisdiction glob:\n{script}"
        conds = node.succeed(
            "systemctl show -p ConditionPathExistsGlob rio-kubelet-mount.service"
        ).strip()
        assert conds == "ConditionPathExistsGlob=", (
            f"the dispatcher grew a path Condition back - that is the "
            f"merged_bug_045 wrong-clock gate: {conds}"
        )

    # Mount failure must NOT be fail-open: with only before= ordering
    # + wantedBy=sysinit.target, tmpfiles would create
    # /var/lib/kubelet on root EBS, kubelet starts, node Ready,
    # Karpenter bin-packs against phantom RAID0 capacity (or the
    # disk-sizing producer rides a dead ext4 root — live_060). ONE
    # Requires= edge on the dispatcher covers both node classes.
    with subtest("rio-kubelet-mount failure blocks kubelet (fail-hard)"):
        deps = node.succeed("systemctl show -p Requires kubelet.service")
        assert "rio-kubelet-mount.service" in deps, (
            f"kubelet does not Requires=rio-kubelet-mount — a mount "
            f"failure would be fail-open (Ready node, dead producer). {deps}"
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
