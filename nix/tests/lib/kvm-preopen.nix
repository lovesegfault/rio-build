# KVM pre-open workaround for nixbuild.net builders.
#
# Problem: builder's /etc/udev/rules.d/99-local.rules sets /dev/kvm to
# MODE=0660 GROUP=snix-qemu (empty group). Sandbox init2 chmods to 666,
# but udev re-applies 660 shortly after the first qemu's KVM init.
# Subsequent qemu opens fail EACCES → TCG fallback.
#
# Workaround: test driver opens /dev/kvm ONCE while it's 666 (init2 just
# chmod'd it), passes the fd through to every qemu child. An LD_PRELOAD
# shim intercepts qemu's open("/dev/kvm") and dups the inherited fd.
#
# This module provides:
#   - shim: the .so built from kvm-preopen-shim.c
#   - preamble: Python snippet for the testScript that does the pre-open
#     and monkey-patches the test driver's Popen call
{
  pkgs,
}:
rec {
  # LD_PRELOAD shim — intercepts open("/dev/kvm"), dups KVM_PRELOAD_FD.
  shim = pkgs.runCommandCC "kvm-preopen-shim" { } ''
    mkdir -p $out/lib
    $CC -shared -fPIC -O2 -ldl \
      -o $out/lib/kvm-preopen-shim.so \
      ${./kvm-preopen-shim.c}
  '';

  # testScript preamble. Must run BEFORE any machine.start() or start_all().
  #
  # Monkey-patches test_driver.machine.StartCommand.run to:
  #   1. include the pre-opened fd in pass_fds (survives Popen's close_fds)
  #   2. set KVM_PRELOAD_FD + LD_PRELOAD in the qemu environment
  #
  # The fd is dup'd to slot 200 for a stable reference across the
  # subprocess chain (bash → run-vm script → qemu).
  preamble = ''
    # ── KVM pre-open workaround (nix/tests/lib/kvm-preopen.nix) ──
    import os

    _KVM_PRELOAD_FD = 200
    _KVM_SHIM = "${shim}/lib/kvm-preopen-shim.so"

    # Retry the open — /dev/kvm flip-flops 666↔660 with ~2s period across
    # concurrent sandboxes on the same host. init2 chmods to 666 right
    # before the test driver starts, but another build's qemu may trigger
    # the 660 reset in the gap. Poll for up to 30s to catch a 666 window.
    import time as _t
    _kvm_preopen_ok = False
    for _attempt in range(300):
        try:
            _kvm_raw = os.open("/dev/kvm", os.O_RDWR)
            os.dup2(_kvm_raw, _KVM_PRELOAD_FD)
            os.close(_kvm_raw)
            os.set_inheritable(_KVM_PRELOAD_FD, True)
            _kvm_preopen_ok = True
            print(f"[kvm-preopen] /dev/kvm opened at fd={_KVM_PRELOAD_FD} (attempt {_attempt+1}), shim={_KVM_SHIM}")
            break
        except PermissionError:
            _t.sleep(0.1)
        except Exception as _e:
            print(f"[kvm-preopen] WARNING: open(/dev/kvm) failed with non-EACCES: {_e}")
            break
    if not _kvm_preopen_ok:
        print("[kvm-preopen] WARNING: open(/dev/kvm) EACCES for 30s straight — qemu will fall through to normal open (TCG)")

    if _kvm_preopen_ok:
        # Patch subprocess.Popen directly — the test driver's StartCommand.run
        # calls Popen with shell=True and no pass_fds. We inject the fd +
        # env vars for any Popen that spawns a run-*-vm command. setattr
        # avoids mypy's method-assign check.
        import subprocess as _sp
        _orig_popen = _sp.Popen

        class _KvmPopen(_orig_popen):  # type: ignore[misc]
            def __init__(self, args, **kw):
                # Detect qemu launches: shell=True + command contains "run-"
                # and "-vm" (the test driver passes /nix/store/...-run-<m>-vm).
                # Conservative: inject on any shell command mentioning -vm.
                # LD_PRELOAD is harmless for non-qemu (shim passes through).
                cmd = args if isinstance(args, str) else " ".join(args) if isinstance(args, (list, tuple)) else ""
                is_vm_launch = kw.get("shell") and "run-" in cmd and "-vm" in cmd
                if is_vm_launch:
                    env = dict(kw.get("env") or os.environ)
                    env["KVM_PRELOAD_FD"] = str(_KVM_PRELOAD_FD)
                    env["LD_PRELOAD"] = _KVM_SHIM + (
                        (":" + env["LD_PRELOAD"]) if env.get("LD_PRELOAD") else ""
                    )
                    kw["env"] = env
                    pf = set(kw.get("pass_fds") or ())
                    pf.add(_KVM_PRELOAD_FD)
                    kw["pass_fds"] = tuple(pf)
                    print(f"[kvm-preopen] injecting fd+LD_PRELOAD into: {cmd[:100]}")
                super().__init__(args, **kw)

        setattr(_sp, "Popen", _KvmPopen)
        print("[kvm-preopen] subprocess.Popen wrapped (run-*-vm launches get fd+LD_PRELOAD)")
    # ── end kvm-preopen ──
  '';
}
