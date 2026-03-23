# KVM availability hard-fail gate.
#
# Verifies /dev/kvm is openable RDWR and a KVM_CREATE_VM ioctl succeeds.
# If not, fails IMMEDIATELY with a clear message — VM tests under TCG have
# different timing characteristics and produce false positives/negatives,
# so silent fallback is worse than a fast, legible fail.
#
# Scenarios prepend `${common.kvmCheck}` before start_all() in testScript.
''
  # ── KVM hard-fail gate (nix/tests/lib/kvm-check.nix) ──
  # If /dev/kvm isn't usable, fail NOW with a clear message instead of
  # silently falling back to TCG (wrong timing, false positives/negatives).
  import os as _os
  import fcntl as _fcntl
  import stat as _stat

  _KVM_CREATE_VM = 0xAE01  # _IO(KVMIO, 1), KVMIO=0xAE
  try:
      _s = _os.stat("/dev/kvm")
      _kvm_fd = _os.open("/dev/kvm", _os.O_RDWR | _os.O_CLOEXEC)
      _vm_fd = _fcntl.ioctl(_kvm_fd, _KVM_CREATE_VM, 0)
      _os.close(_vm_fd)
      _os.close(_kvm_fd)
      print(f"[kvm-check] OK: /dev/kvm mode="
            f"{oct(_stat.S_IMODE(_s.st_mode))} "
            f"owner={_s.st_uid}:{_s.st_gid}, KVM_CREATE_VM succeeded")
  except (FileNotFoundError, PermissionError, OSError) as _e:
      _s = None
      try:
          _s = _os.stat("/dev/kvm")
      except Exception:
          pass
      _diag = (f"mode={oct(_stat.S_IMODE(_s.st_mode))} "
               f"owner={_s.st_uid}:{_s.st_gid}" if _s else "stat-failed")
      raise RuntimeError(
          f"\n"
          f"═══════════════════════════════════════════════════════════\n"
          f"KVM NOT AVAILABLE — HARD FAIL\n"
          f"═══════════════════════════════════════════════════════════\n"
          f"  /dev/kvm: {_diag}\n"
          f"  error: {type(_e).__name__}: {_e}\n"
          f"\n"
          f"  VM tests require KVM. TCG fallback has different timing and\n"
          f"  produces false positives/negatives.\n"
          f"\n"
          f"  Check that this build host's /dev/kvm is accessible to the\n"
          f"  build sandbox — typically MODE=0666, or the sandbox uid in\n"
          f"  the owning group, or a udev rule granting access.\n"
          f"═══════════════════════════════════════════════════════════\n"
      ) from _e
  # ── end kvm-check ──
''
