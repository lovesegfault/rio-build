# ami-variant-quota — the merged_bug_024 + merged_bug_045 end-to-end
# witness: rio-kubelet-mount (the settled-udev dispatcher) against
# the REAL per-AMI-variant by-id namespaces, with the partition
# layout AND the coldplug timing as TEST DIMENSIONS.
#
# Why this scenario exists (witness-population substitution, the
# round-13 process-clean class): the prior witnesses pinned
# quotaVolumeGlobs=[/dev/vdb] — a bare, partitionless virtio disk —
# so the enumeration's exclusion claim ("the root disk is excluded")
# was author-asserted over a namespace no fixture ever contained.
# udev mints one by-id link PER PARTITION beside every whole-disk
# link, and the population is VARIANT-DEPENDENT:
#
#   ami-bios (legacy+gpt, x86 .metal — I-205): partition 1 is the
#     never-mounted, filesystem-free bios_grub partition. Its by-id
#     link passes every side-effect filter (no children, no
#     mountpoint, no signature) — pre-fix every boot counted
#     n_bare=2 -> exit 1 -> kubelet Requires= hard-fail (NotReady/
#     reap churn for the whole x86-metal class), and with the quota
#     volume late/absent the n_bare=1 corner selected bios_grub for
#     mkfs.xfs -f.
#   ami (efi/UKI, everything else): partition 1 is the ESP, mounted
#     — the side-effect filters happened to hold there, which is why
#     the uefi fleet never churned. This node pins that the typed
#     predicate keeps the SAME winner (byte-stable, W13-E2).
#
# The EBS nodes boot the real nix/nixos-node module tree (mock IMDS,
# no AWS) with TWO serial-tagged virtio disks: a fake-root disk
# partitioned by rio-test-ami-layout to the PRODUCER's recipe (the
# locked nixpkgs make-disk-image.nix partition tables — the same
# recipes the deployed AMIs are built with), and a bare quota volume.
# udev itself mints the by-id whole-disk + -partN links (the
# namespace producer is the real one, not a hand-made symlink tree).
#
# Fixture-derivation MODE (CE-8/OQ-13 posture): the layout is
# hand-reproduced in-VM (building a full amazon-image per variant
# inside a VM test is not practical), and the DRIFT ASSERT below
# pins the mirrored parted lines against the locked
# make-disk-image.nix source — upstream layout drift flips this test
# instead of silently un-witnessing the exclusion claim.
#
# Budget (R17): three single-node module-tree VMs (no k3s, no rio
# binaries) booting in parallel; nixos-node-class boot is ~2-3min
# plus the 15s coldplug stalls. 1200s covers the tail under builder
# load.
{ pkgs }:
let
  # The producer's partition recipes (drift pin — read from the
  # LOCKED nixpkgs, the same source the deployed AMIs build from).
  diskImageRecipe = "${pkgs.path}/nixos/lib/make-disk-image.nix";

  fakeRootSerial = "RIOFAKEROOT";
  quotaSerial = "RIOQUOTA";

  # The merged_bug_045 timing dimension: a udev rule that stalls the
  # tagged disk's add event (IMPORT{program} sleep) so its by-id link
  # materializes SECONDS after the early units' job-start instants —
  # the deterministic form of the EC2 coldplug window. The dispatcher
  # settles through it (udevadm settle drains the stalled queue); a
  # regression to job-start-clock reads (a unit Condition, or a
  # pre-settle enumeration) goes red on these nodes.
  delayRule = serialGlob: extra: ''
    ACTION=="add", SUBSYSTEM=="block", KERNEL=="vd[a-z]", ATTRS{serial}=="${serialGlob}", IMPORT{program}="/bin/sh -c 'sleep 15'"${extra}
  '';

  mkNode =
    variant:
    {
      lib,
      pkgs,
      modulesPath,
      ...
    }:
    {
      imports = [
        ../../nixos-node
        # amazon-init declares virtualisation.amazon-init.enable which
        # nixos-node/default.nix mkDefault-disables; import so the
        # option resolves (same shape as nix/tests/nixos-node.nix).
        (modulesPath + "/virtualisation/amazon-init.nix")
      ];

      # QEMU boot fixups — minimal.nix strips initrd to Nitro-only;
      # the test VM needs virtio + 9p (mirrors nix/tests/nixos-node.nix).
      boot.initrd = {
        availableKernelModules = [
          "virtio_blk"
          "virtio_pci"
          "virtio_net"
          "9p"
          "9pnet_virtio"
        ];
        kernelModules = lib.mkOverride 40 [ ];
      };

      virtualisation = {
        memorySize = 2048;
        cores = 2;
        # kubelet's NodeAllocatable check needs headroom on the fs
        # hosting /var/lib/kubelet (the quota volume below).
        diskSize = 4096;
        emptyDiskImages = [
          # The fake-root disk: rio-test-ami-layout partitions it to
          # the variant's producer recipe. The serial makes udev mint
          # /dev/disk/by-id/virtio-RIOFAKEROOT (+ -partN per
          # partition) — the namespace member class the enumeration
          # must reject.
          {
            size = 512;
            driveConfig.deviceExtraOpts.serial = fakeRootSerial;
          }
          # The bare quota volume (the karpenter second-EBS stand-in).
          {
            size = 4096;
            driveConfig.deviceExtraOpts.serial = quotaSerial;
          }
        ];
      };

      # The production-shaped namespace: a whole-volume glob that
      # matches BOTH disks and (post-partitioning) the -partN links —
      # exactly the shape the fleet default
      # (nvme-Amazon_Elastic_Block_Store_vol*) has. The test
      # framework's own root disk is virtio-root: excluded by prefix,
      # like non-EBS devices in production.
      services.rio.eksNode.quotaVolumeGlobs = [ "/dev/disk/by-id/virtio-RIO*" ];

      # merged_bug_045 timing lane: the QUOTA volume's by-id link
      # materializes 15s late. The dispatcher's settle drains the
      # stalled event before the EBS branch enumerates; dropping the
      # settle (or re-keying jurisdiction on a job-start Condition
      # read) turns this node red with n_bare=0.
      services.udev.extraRules = delayRule quotaSerial "";

      # ── mock IMDS (verbatim shape from nix/tests/nixos-node.nix) ──
      systemd.services.mock-imds = {
        description = "Mock EC2 IMDSv2";
        wantedBy = [ "multi-user.target" ];
        before = [ "nodeadm-init.service" ];
        requiredBy = [ "nodeadm-init.service" ];
        serviceConfig = {
          ExecStartPre = "${lib.getExe' pkgs.iproute2 "ip"} addr replace 169.254.169.254/32 dev lo";
          ExecStart = "${pkgs.python3.interpreter} ${../fixtures/mock-imds.py}";
        };
      };

      # ── the variant layout, built to the PRODUCER's recipe ────────
      # Runs strictly before the dispatcher so the by-id namespace
      # already carries the partition links when the enumeration
      # walks it (the production state: the AMI ships partitioned).
      # The QUOTA link's 15s stall above is the separate
      # merged_bug_045 timing lane — this unit's own settle drains
      # it too, which only strengthens the ordering.
      systemd.services.rio-test-ami-layout = {
        description = "Partition the fake-root disk to the ${variant} AMI recipe";
        wantedBy = [ "sysinit.target" ];
        before = [ "rio-kubelet-mount.service" ];
        after = [ "local-fs.target" ];
        unitConfig.DefaultDependencies = false;
        path = [
          pkgs.parted
          pkgs.e2fsprogs
          pkgs.dosfstools
          pkgs.util-linux
          pkgs.systemd # udevadm
          pkgs.coreutils
        ];
        script =
          if variant == "bios" then
            ''
              set -euo pipefail
              udevadm settle
              root=$(readlink -f /dev/disk/by-id/virtio-${fakeRootSerial})
              # make-disk-image.nix "legacy+gpt" (drift-pinned in the
              # testScript): no-fs bios_grub 1-2MiB + ext4 root.
              parted --script "$root" -- \
                mklabel gpt \
                mkpart no-fs 1MiB 2MiB \
                set 1 bios_grub on \
                mkpart primary ext4 2MiB 100% \
                align-check optimal 2 \
                print
              udevadm settle
              mkfs.ext4 -q /dev/disk/by-id/virtio-${fakeRootSerial}-part2
              mkdir -p /mnt/fake-root
              mount /dev/disk/by-id/virtio-${fakeRootSerial}-part2 /mnt/fake-root
              udevadm settle
            ''
          else
            ''
              set -euo pipefail
              udevadm settle
              root=$(readlink -f /dev/disk/by-id/virtio-${fakeRootSerial})
              # make-disk-image.nix "efi" (drift-pinned in the
              # testScript): ESP fat32 8MiB-bootSize + ext4 root.
              parted --script "$root" -- \
                mklabel gpt \
                mkpart ESP fat32 8MiB 256MiB \
                set 1 boot on \
                align-check optimal 1 \
                mkpart primary ext4 256MiB 100% \
                align-check optimal 2 \
                print
              udevadm settle
              mkfs.vfat /dev/disk/by-id/virtio-${fakeRootSerial}-part1 >/dev/null
              mkfs.ext4 -q /dev/disk/by-id/virtio-${fakeRootSerial}-part2
              # Production mounts the ESP (uki-boot esp=/boot) and the
              # root fs — mirror BOTH mount states so the namespace's
              # filter-relevant facts match the live uefi fleet.
              mkdir -p /mnt/fake-root /mnt/fake-boot
              mount /dev/disk/by-id/virtio-${fakeRootSerial}-part2 /mnt/fake-root
              mount /dev/disk/by-id/virtio-${fakeRootSerial}-part1 /mnt/fake-boot
              udevadm settle
            '';
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
      };
    };
in
pkgs.testers.runNixOSTest {
  name = "rio-ami-variant-quota";
  skipTypeCheck = true;
  globalTimeout = 1200;

  node.specialArgs.pins = import ../../pins.nix;

  nodes = {
    bios = mkNode "bios";
    uefi = mkNode "uefi";

    # The instance-store node class (merged_bug_045's other
    # jurisdiction): two serial-tagged disks aliased into the
    # PRODUCTION instance-store by-id namespace by a udev rule that
    # also delays their add events 15s — the deterministic coldplug
    # window. Pre-fix, this timing made the Condition pair fail BOTH
    # ways (the nvme unit skipped at job start before the links
    # existed; the EBS unit then either ran-and-refused, hard-blocking
    # kubelet, or skipped too when its own eval landed after the
    # links, booting kubelet Ready on an unquota'd root — the
    # strawman transcripts in the closing commit). Post-fix the
    # dispatcher settles through the stall, classifies instance-store
    # once, and the RAID0 branch owns the kubelet root.
    nvme =
      {
        lib,
        pkgs,
        modulesPath,
        ...
      }:
      {
        imports = [
          ../../nixos-node
          (modulesPath + "/virtualisation/amazon-init.nix")
        ];
        boot.initrd = {
          availableKernelModules = [
            "virtio_blk"
            "virtio_pci"
            "virtio_net"
            "9p"
            "9pnet_virtio"
          ];
          kernelModules = lib.mkOverride 40 [ ];
        };
        # mdadm RAID0 over the two "instance-store" disks.
        boot.kernelModules = [ "raid0" ];

        virtualisation = {
          memorySize = 2048;
          cores = 2;
          diskSize = 4096;
          emptyDiskImages = [
            {
              size = 2048;
              driveConfig.deviceExtraOpts.serial = "RIONVME0";
            }
            {
              size = 2048;
              driveConfig.deviceExtraOpts.serial = "RIONVME1";
            }
          ];
        };

        # Alias the tagged disks into the PRODUCTION instance-store
        # namespace (the default instanceStoreGlobs string is what the
        # dispatcher classifies on — exercised verbatim) AND delay
        # their add events: the jurisdiction evidence arrives on
        # udev's clock, 15s after every early unit's job-start
        # instant.
        services.udev.extraRules = delayRule "RIONVME?" '', SYMLINK+="disk/by-id/nvme-Amazon_EC2_NVMe_Instance_Storage_TEST_$attr{serial}"'';

        systemd.services.mock-imds = {
          description = "Mock EC2 IMDSv2";
          wantedBy = [ "multi-user.target" ];
          before = [ "nodeadm-init.service" ];
          requiredBy = [ "nodeadm-init.service" ];
          serviceConfig = {
            ExecStartPre = "${lib.getExe' pkgs.iproute2 "ip"} addr replace 169.254.169.254/32 dev lo";
            ExecStart = "${pkgs.python3.interpreter} ${../fixtures/mock-imds.py}";
          };
        };
      };
  };

  testScript = ''
    # ── drift pin (CE-8): the in-VM layouts mirror the locked nixpkgs
    # make-disk-image.nix recipes; if upstream changes a recipe, this
    # reds and the mirrored layout is re-derived — the witness never
    # silently detaches from the producer.
    recipe = open("${diskImageRecipe}").read()
    for needle in [
        'mkpart no-fs 1MiB 2MiB',
        'set 1 bios_grub on',
        'mkpart primary ext4 2MiB 100%',
        'mkpart ESP fat32 8MiB',
        'set 1 boot on',
    ]:
        assert needle in recipe, (
            "make-disk-image.nix no longer contains %r - the AMI "
            "partition recipe drifted; re-derive the fixture layout "
            "in this scenario" % needle
        )

    start_all()


    def check_node(node, variant):
        node.wait_for_unit("rio-test-ami-layout.service")

        with subtest(variant + ": the by-id namespace contains the adversarial members"):
            # The witness population is REAL: whole-disk link + one
            # link per partition (udev-minted, not hand-made) + the
            # bare quota volume. Without these the scenario would be
            # the old witness-population substitution again.
            node.succeed("test -e /dev/disk/by-id/virtio-${fakeRootSerial}")
            node.succeed("test -e /dev/disk/by-id/virtio-${fakeRootSerial}-part1")
            node.succeed("test -e /dev/disk/by-id/virtio-${fakeRootSerial}-part2")
            node.succeed("test -e /dev/disk/by-id/virtio-${quotaSerial}")

        with subtest(variant + ": quota volume selected, mounted prjquota at the kubelet root"):
            node.wait_for_unit("rio-kubelet-mount.service")
            out = node.succeed("findmnt -no FSTYPE,OPTIONS /var/lib/kubelet")
            assert "xfs" in out and "prjquota" in out, repr(out)
            src = node.succeed("findmnt -no SOURCE /var/lib/kubelet").strip()
            want = node.succeed("readlink -f /dev/disk/by-id/virtio-${quotaSerial}").strip()
            assert src == want, (
                "wrong winner: kubelet root is on %r, the quota volume is %r"
                % (src, want)
            )

        with subtest(variant + ": the rejection trail names the partition-link class"):
            log = node.succeed("journalctl -u rio-kubelet-mount.service --no-pager")
            assert "2 partition-link" in log, (
                "expected both -partN links rejected BY CLASS; trail:\n" + log
            )
            assert "1 partitioned (root/boot)" in log, (
                "expected the fake-root disk excluded by children; trail:\n" + log
            )

        with subtest(variant + ": kubelet starts (the Requires= consequence tier)"):
            node.wait_for_unit("kubelet.service")


    check_node(bios, "bios")

    with subtest("bios: bios_grub still carries NO filesystem signature"):
        # The n_bare=1 corner's structural witness: nothing ever
        # formats the bios_grub partition (pre-fix the quota-late
        # corner ran mkfs.xfs -f against it; selection is now
        # impossible by class).
        bios.succeed(
            "test -z \"$(blkid -o value -s TYPE /dev/disk/by-id/virtio-${fakeRootSerial}-part1 || true)\""
        )

    # W13-E2: the uefi variant's winner is byte-stable — the typed
    # predicate admits exactly the previous whole-disk winner there.
    check_node(uefi, "uefi")

    # W13-F: the instance-store jurisdiction under the delayed
    # coldplug window — ONE settled decision, the RAID0 branch.
    with subtest("nvme: dispatcher classifies instance-store on the settled view"):
        nvme.wait_for_unit("rio-kubelet-mount.service")
        out = nvme.succeed("findmnt -no FSTYPE,OPTIONS /var/lib/kubelet")
        assert "xfs" in out and "prjquota" in out, repr(out)
        src = nvme.succeed("findmnt -no SOURCE /var/lib/kubelet").strip()
        assert src == "/dev/md0", (
            "two instance-store disks must stripe into md0; kubelet root is on %r"
            % src
        )

    with subtest("nvme: jurisdiction decided once - the EBS branch never ran"):
        log = nvme.succeed("journalctl -u rio-kubelet-mount.service --no-pager")
        assert "quota-volume-select" not in log, (
            "the EBS branch ran on an instance-store node - two "
            "jurisdiction reads are back:\n" + log
        )

    with subtest("nvme: kubelet starts (the Requires= consequence tier)"):
        nvme.wait_for_unit("kubelet.service")

    # W13-F2: steady-state mounts byte-equivalent on both node
    # classes - same fstype, same prjquota option, the class-specific
    # source (md0 vs the quota volume) being the only difference.
    with subtest("both node classes converge on the same kubelet-root contract"):
        for machine in (bios, nvme):
            out = machine.succeed("findmnt -no FSTYPE,OPTIONS /var/lib/kubelet")
            assert "xfs" in out and "prjquota" in out and "noatime" in out, repr(out)
  '';
}
