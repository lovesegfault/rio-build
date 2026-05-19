# VM test wiring — withMinCpu/cpuHints/mkVmTests.
#
# mkVmTests builds the `vm-*` attrset for a given (workspace,
# dockerImages, coverage) triple. flake.nix calls it twice: once for
# the normal build (`vmTests`) and once for the
# coverage-instrumented build (`vmTestsCov`, see common.nix's
# LLVM_PROFILE_FILE wiring). Each test is wrapped with `withMinCpu`
# to set NIXBUILDNET_MIN_{CPU,MEM} for the remote builder.
#
# Extracted from flake.nix's perSystem `let` block. Lives next to
# nix/tests/default.nix (the `import ./.` callee) so the cpuHints
# table and the test definitions it indexes are in the same
# directory.
{
  pkgs,
  system,
  inputs,
}:
rec {
  # Request a minimum CPU allocation from the remote builder. Each
  # VM has `virtualisation.cores = 4` in common.nix; without
  # this, the builder's heuristic allocation can under-provision
  # (vm-scheduling-core once got 5 CPUs for 4 VMs → 16 vCPUs on 5
  # physical, 2 VMs fell back to TCG, worker1's kernel boot
  # starved at PCI enumeration → Shell disconnected flake).
  #
  # Floor of 64 vCPU / 128GB: prevents KVM contention across
  # concurrent VM-test builds on the same host. With ~60-190
  # CPU hosts, 64 vCPU floor caps at ~1-3 concurrent VM-test
  # builds per host (previously ~9 at old ×4 formula → up to
  # ~45 concurrent qemu KVM_CREATE_VM → some lose the race,
  # "failed to initialize kvm: Permission denied" → TCG
  # fallback or hard fail). 128GB floor ensures the k3s tests
  # (≈20GB peak) plus qemu+test-driver overhead have headroom.
  # cpuHints is still consulted for the ×4 formula when it
  # exceeds the floor (future >16-VM tests).
  withMinCpu =
    numVMs: test:
    let
      byVMs = numVMs * 4 + 1;
      cpuFloor = 64;
      memFloor = 131072;
    in
    test.overrideTestDerivation {
      NIXBUILDNET_MIN_CPU = toString (pkgs.lib.max byVMs cpuFloor);
      NIXBUILDNET_MIN_MEM = toString memFloor;
    };

  mkVmTests =
    {
      rio-workspace,
      dockerImages,
      coverage,
    }:
    let
      allTests = import ./. {
        inherit
          pkgs
          rio-workspace
          dockerImages
          system
          coverage
          ;
        rioModules = inputs.self.nixosModules;
        inherit (inputs) nixhelm;
        # Lix-client VM test (vm-protocol-warm-lix-standalone).
        # nixpkgs-packaged (substitutable from cache.nixos.org)
        # rather than building lix from source via a flake input.
        lixPackage = pkgs.lix;
      };
      # Per-test builder CPU hint. withMinCpu sets
      # NIXBUILDNET_MIN_CPU (numVMs × 4 + 1) to prevent
      # oversubscription → TCG fallback → qemu stall. Fallthrough:
      # 8 for -k3s suffix, else 4 (see mapAttrs below).
      cpuHints = {
        # 3 VMs (control+worker+client). Control is 4-core.
        vm-protocol-warm-standalone = 3;
        vm-protocol-warm-lix-standalone = 3;
        vm-protocol-cold-standalone = 3;
        # 5 VMs: control + wsmall1/wsmall2/wlarge + client.
        # Both scheduling splits boot the full 3-worker fixture.
        vm-scheduling-core-standalone = 5;
        vm-scheduling-disrupt-standalone = 5;
        # 3 VMs: control + worker + client.
        vm-security-standalone = 3;
        # 3 VMs: control + worker + client. Single-worker
        # standalone fixture (ca-cutoff chain is serial anyway).
        vm-ca-cutoff-standalone = 3;
        # 3 VMs: control + worker + client. toxiproxy runs as a
        # systemd unit on control, not a separate VM.
        vm-chaos-standalone = 3;
        # 5 VMs: control + worker1/2/3 + client.
        vm-observability-standalone = 5;
        # 3 VMs but k3s-server is 8-core 6GB + k3s-agent 8-core 4GB.
        # All lifecycle + leader-election splits boot the same
        # 2-node k3s fixture.
        vm-lifecycle-core-k3s = 8;
        vm-lifecycle-recovery-k3s = 8;
        vm-lifecycle-autoscale-k3s = 8;
        vm-le-stability-k3s = 8;
        vm-le-build-k3s = 8;
        # k3s nonpriv e2e (base_runtime_spec /dev/fuse +
        # cgroup rw-remount).
        vm-security-nonpriv-k3s = 8;
        # k3s + Cilium Gateway API + rio-dashboard nginx. curl via
        # nginx → Cilium Gateway → scheduler (tonic-web).
        vm-dashboard-k3s = 8;
        # k3s base fixture. rio-cli AdminService smoke.
        vm-cli-k3s = 8;
        # k3s base fixture. Worker egress NetworkPolicy enforce.
        vm-netpol-k3s = 8;
        # Same 2-node k3s fixture + bootstrap Job backoff.
        # Asserts PSA-restricted — NOT in vmTestsCov (see flake.nix removeAttrs).
        vm-lifecycle-prod-parity-k3s = 8;
        # k3s base fixture + KWOK Stage rules faking Karpenter.
        # §13b nodeclaim_pool reconciler e2e.
        vm-sla-sizing-kwok = 8;
      };
    in
    # Dead-entry guard: every cpuHints key must name a real test.
    # Before this assert, vm-lifecycle-bps-k3s and vm-fod-proxy-k3s
    # sat here for months after deletion — the old comment claimed a
    # "T539" check caught dead entries; that check never existed.
    # Gated on !coverage: vmTestsCov's allTests is a strict subset
    # (vm-dashboard-k3s is optionalAttrs-gated on dockerImages?
    # dashboard, absent in coverage mode); checking the full set
    # once is sufficient.
    assert
      coverage
      ||
        pkgs.lib.assertMsg (pkgs.lib.all (k: allTests ? ${k}) (pkgs.lib.attrNames cpuHints))
          "cpuHints has entries for tests not in nix/tests/default.nix: ${
            toString (pkgs.lib.filter (k: !(allTests ? ${k})) (pkgs.lib.attrNames cpuHints))
          }";
    (pkgs.lib.mapAttrs (
      name:
      withMinCpu (
        cpuHints.${name}
          # k3s fixture: 2-node cluster, k3s-server 8-core + k3s-agent
          # 8-core. Every -k3s test in the table is 8; encode that as
          # the suffix default so new -k3s tests don't fall through to
          # 4. Catchup-fix precedent: d6f74e27 + fa55ef13 both added
          # forgotten -k3s entries. Dead entries fail the assert above.
          or (if pkgs.lib.hasSuffix "-k3s" name then 8 else 4)
      )
    ) allTests);
}
