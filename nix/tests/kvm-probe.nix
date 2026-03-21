# KVM race validation probe.
#
# Hypothesis: concurrent qemu KVM_init calls race, some lose → TCG fallback.
# Previous investigation (project_kvmonly-multi-vm-break.md Finding 3) showed
# that Python open+CREATE_VM succeeds 5/5 concurrent, but qemu's kvm_init()
# (which does VCPU creation, memory slots, irqchip, MMU notifier) fails for
# SOME concurrent instances.
#
# This probe boots 5 trivial VMs two ways:
#   - kvm-probe-concurrent: start_all() (nixos-test default, races)
#   - kvm-probe-staggered: sequential start with 500ms delay between
#
# Each VM's kernel dmesg is scanned for "Hypervisor detected: KVM" and the
# result is printed per-VM. Run each variant N× and compare KVM-got ratios.
#
# Usage:
#   nix build --no-link -L .#kvm-probe-concurrent .#kvm-probe-staggered
#   grep "KVM-PROBE" <log> | sort | uniq -c
{
  pkgs,
  ...
}:
let
  # Trivial VM — boots, nothing more. 4 cores to match real test config.
  vm = {
    virtualisation.cores = 4;
    virtualisation.memorySize = 512;
    # Minimal — no rio services, no k3s. Just a bootable NixOS.
    environment.systemPackages = [ pkgs.coreutils ];
  };

  nodes = {
    m1 = vm;
    m2 = vm;
    m3 = vm;
    m4 = vm;
    m5 = vm;
  };

  # Per-VM KVM check: scan dmesg for the kernel's hypervisor detection.
  # If "Hypervisor detected: KVM" is present, the VM got KVM.
  # If absent (or "using TCG" is in qemu output), it fell to TCG.
  checkKvm = ''
    for m in [m1, m2, m3, m4, m5]:
        m.wait_for_unit("multi-user.target")
        out = m.succeed("dmesg | grep -i hypervisor || echo NONE")
        got_kvm = "KVM" in out
        print(f"KVM-PROBE: {m.name} got_kvm={got_kvm} dmesg={out.strip()}")
  '';

  # Impurity slot — the probe runner sed-replaces this. Nix comments
  # don't affect drv hash; a change INSIDE a string expression (interpolated
  # into testScript) does. See kvm-probe-run.sh.
  impurityNonce = "";

in
{
  kvm-probe-concurrent = pkgs.testers.runNixOSTest {
    name = "kvm-probe-concurrent";
    inherit nodes;
    testScript = ''
      # nonce: ${impurityNonce}
      import time
      t0 = time.monotonic()
      print("KVM-PROBE: start_all() at t=0")
      start_all()
      ${checkKvm}
      print(f"KVM-PROBE: concurrent done in {time.monotonic()-t0:.1f}s")
    '';
  };

  kvm-probe-staggered = pkgs.testers.runNixOSTest {
    name = "kvm-probe-staggered";
    inherit nodes;
    testScript = ''
      # nonce: ${impurityNonce}
      import time
      t0 = time.monotonic()
      print("KVM-PROBE: staggered start (500ms between)")
      for m in [m1, m2, m3, m4, m5]:
          m.start()
          print(f"KVM-PROBE: started {m.name} at t={time.monotonic()-t0:.1f}s")
          time.sleep(0.5)
      ${checkKvm}
      print(f"KVM-PROBE: staggered done in {time.monotonic()-t0:.1f}s")
    '';
  };
}
