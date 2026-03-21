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

  # Diagnostic: stats /dev/kvm before each start to catch a udev
  # perm-reset. Hypothesis: m1's qemu KVM init triggers udev re-eval
  # → /dev/kvm resets to 660 → m2+ open() EACCES. Also tries to open
  # /dev/kvm from the test driver (same uid as qemu) to compare.
  kvm-probe-diag = pkgs.testers.runNixOSTest {
    name = "kvm-probe-diag";
    inherit nodes;
    testScript = ''
      # nonce: ${impurityNonce}
      import os, stat, subprocess, time
      t0 = time.monotonic()

      def kvm_stat():
          try:
              st = os.stat("/dev/kvm")
              mode = stat.S_IMODE(st.st_mode)
              try:
                  fd = os.open("/dev/kvm", os.O_RDWR)
                  os.close(fd)
                  can_open = "yes"
              except Exception as e:
                  can_open = f"no({type(e).__name__}:{e})"
              return f"mode={oct(mode)} uid={st.st_uid} gid={st.st_gid} can_open={can_open}"
          except Exception as e:
              return f"stat-failed: {e}"

      # Dump initial state
      print(f"KVM-PROBE-DIAG: t={time.monotonic()-t0:.1f}s initial: {kvm_stat()}")
      subprocess.run(["sh", "-c", "ls -la /dev/kvm; id; getent group $(stat -c %g /dev/kvm) || echo no-group"], check=False)

      for m in [m1, m2, m3, m4, m5]:
          before = kvm_stat()
          m.start()
          time.sleep(1.5)  # Let qemu init complete
          after = kvm_stat()
          changed = " <<<< CHANGED" if before != after else ""
          print(f"KVM-PROBE-DIAG: t={time.monotonic()-t0:.1f}s {m.name}: before[{before}] after[{after}]{changed}")

      ${checkKvm}
      print(f"KVM-PROBE-DIAG: done in {time.monotonic()-t0:.1f}s")
    '';
  };

  # Trace variant — 10ms-resolution mode poll during m1 startup.
  # Narrows the trigger window. Also straces m1's qemu to correlate.
  kvm-probe-trace = pkgs.testers.runNixOSTest {
    name = "kvm-probe-trace";
    nodes = {
      inherit (nodes) m1 m2;
    };
    testScript = ''
      # nonce: ${impurityNonce}
      import os, stat, subprocess, threading, time
      t0 = time.monotonic()
      def kvm_mode():
          try:
              return oct(stat.S_IMODE(os.stat("/dev/kvm").st_mode))
          except Exception:
              return "err"

      poll_log = []
      poll_stop = threading.Event()
      def poller():
          last = None
          while not poll_stop.is_set():
              m = kvm_mode()
              t = time.monotonic() - t0
              if m != last:
                  poll_log.append((t, m))
                  last = m
              time.sleep(0.01)
      threading.Thread(target=poller, daemon=True).start()

      # Check udev state + ps before any VM start
      subprocess.run(["sh", "-c", "ls -la /dev/kvm; id; echo ---procs---; ps -ef | wc -l; ps -ef | grep -iE 'udev|systemd' | head -10"], check=False)

      print(f"KVM-PROBE-TRACE: m1.start() at t={time.monotonic()-t0:.3f}s")
      m1.start()
      print(f"KVM-PROBE-TRACE: m1.start() returned at t={time.monotonic()-t0:.3f}s")
      time.sleep(4.0)

      print(f"KVM-PROBE-TRACE: m2.start() at t={time.monotonic()-t0:.3f}s")
      m2.start()
      time.sleep(2.0)

      poll_stop.set()
      time.sleep(0.1)
      for t, m in poll_log:
          print(f"KVM-PROBE-TRACE: mode-transition t={t:.3f}s mode={m}")

      # What processes are running now?
      subprocess.run(["sh", "-c", "ps -ef | grep -iE 'qemu|mke2fs|udev' | head -20"], check=False)

      for vm in [m1, m2]:
          vm.wait_for_unit("multi-user.target")
          out = vm.succeed("dmesg | grep -i hypervisor || echo NONE")
          print(f"KVM-PROBE-TRACE: {vm.name} got_kvm={'KVM' in out}")
    '';
  };
}
