# KVM pre-open workaround for nixbuild.net builders.
#
# PROBLEM: builder's /etc/udev/rules.d/99-local.rules sets /dev/kvm to
# MODE=0660 GROUP=snix-qemu (empty group). Sandbox init2 chmods to 666
# (as root, before dropping privs), but the node is a bind-mount of the
# host's /dev/kvm — ANY concurrent build's qemu KVM_CREATE_VM triggers
# udev to re-apply 660. Under high concurrency the 666 state is
# microseconds-transient.
#
# WHY WE CAN'T FIX THIS "PROPERLY" FROM INSIDE THE SANDBOX
# (investigated 2026-03-22, see commit message for full detail):
#
#   1. chmod/chown — EPERM. Sandbox uid ≠ owner, no CAP_FOWNER.
#   2. setfacl — same EPERM (needs ownership or CAP_FOWNER). ACLs would
#      survive udev's MODE= reset (chmod doesn't clear ACL_USER entries,
#      and mask=group-bits=rw- would still grant rw), but we can't set one.
#   3. init2 fd-passing — init2 is Nix's sandbox init, not our code. No
#      hook to make it pre-open+pass an fd.
#   4. CAP_DAC_OVERRIDE — would bypass mode check entirely, but sandbox
#      has CapEff=0 after init2 drops privs (verified via /proc/self/status
#      diag below).
#   5. O_PATH + /proc/self/fd/N reopen — TESTED, FAILS. The magic-link
#      open re-checks inode permissions; O_PATH only bypasses path
#      traversal, not the final mode check.
#   6. mknod our own /dev/kvm — EPERM in userns (kernel refuses device
#      nodes from non-init-ns root).
#
# CEILING OF WHAT'S POSSIBLE: event-driven open. Every concurrent
# sandbox's init2 chmod creates a brief 666 window. inotify IN_ATTRIB
# fires with ~sub-ms latency (vs the old 100ms poll that missed windows
# entirely under load). We race: inotify fires → open() before the next
# KVM_CREATE_VM resets 660. Under high concurrency, init2 events are
# frequent, so a 120s watch is near-certain to catch one.
#
# PROPER FIX (infra, filed for later): either
#   (a) add sandbox uid to snix-qemu group on builders, OR
#   (b) builder-side service holds /dev/kvm open + setfacl on each reset, OR
#   (c) udev rule ACL grant instead of MODE (survives KVM_CREATE_VM uevent)
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
    import os as _os
    import stat as _stat
    import time as _t
    import select as _sel
    import ctypes as _ct

    _KVM_PRELOAD_FD = 200
    _KVM_SHIM = "${shim}/lib/kvm-preopen-shim.so"
    _KVM_WAIT_S = 120  # total watch budget — see header for why this is the ceiling

    # One-shot diagnostics: what uid/caps/ownership are we working with?
    # Answers investigation tasks 1+4; grep logs for "[kvm-preopen] diag".
    try:
        _kst = _os.stat("/dev/kvm")
        print(f"[kvm-preopen] diag: uid={_os.getuid()} euid={_os.geteuid()} "
              f"kvm_owner={_kst.st_uid}:{_kst.st_gid} "
              f"kvm_mode={oct(_stat.S_IMODE(_kst.st_mode))}")
        with open("/proc/self/status") as _f:
            for _ln in _f:
                if _ln.startswith(("Uid:", "Gid:", "CapEff:", "CapPrm:", "CapBnd:")):
                    print(f"[kvm-preopen] diag: {_ln.rstrip()}")
    except Exception as _e:
        print(f"[kvm-preopen] diag: stat/status read failed: {_e}")

    def _kvm_try_open():
        try:
            _fd = _os.open("/dev/kvm", _os.O_RDWR)
            _os.dup2(_fd, _KVM_PRELOAD_FD)
            _os.close(_fd)
            _os.set_inheritable(_KVM_PRELOAD_FD, True)
            return True
        except PermissionError:
            return False

    _kvm_preopen_ok = False
    _kvm_method = "none"
    _t0 = _t.monotonic()

    # Attempt 1: immediate open. init2 just chmod'd 666; if no concurrent
    # build has reset it yet, this succeeds.
    if _kvm_try_open():
        _kvm_preopen_ok = True
        _kvm_method = "immediate"

    # Attempt 2: inotify IN_ATTRIB watch on /dev DIRECTORY. Watching
    # /dev/kvm directly fails EACCES — inotify_add_watch requires read
    # perm on the target, and /dev/kvm is 660 with us not in-group (v30
    # log: "no 666-window in 120s (0.0s elapsed)" — instant fallthrough).
    # Directory watches fire IN_ATTRIB for contained entries; /dev is
    # 755 so readable. Filter events by name==b"kvm".
    #
    # Every concurrent sandbox's init2 chmod(666) fires an event; we
    # race open() before the next udev reset. Sub-ms event latency.
    import struct as _struct
    _inotify_ok = False
    if not _kvm_preopen_ok:
        _libc = _ct.CDLL(None, use_errno=True)
        _IN_ATTRIB = 0x00000004
        _EVT_HDR = _struct.Struct("iIII")  # wd, mask, cookie, len
        _ifd = _libc.inotify_init1(_os.O_NONBLOCK | _os.O_CLOEXEC)
        if _ifd >= 0:
            _wd = _libc.inotify_add_watch(_ifd, b"/dev", _IN_ATTRIB)
            if _wd >= 0:
                _inotify_ok = True
                # Re-check after watch install — event may have raced setup.
                if _kvm_try_open():
                    _kvm_preopen_ok = True
                    _kvm_method = "post-watch-recheck"
                _deadline = _t0 + _KVM_WAIT_S
                _events = 0
                _kvm_events = 0
                while not _kvm_preopen_ok and _t.monotonic() < _deadline:
                    _r, _, _ = _sel.select([_ifd], [], [], 1.0)
                    _saw_kvm = False
                    if _r:
                        # Drain and parse — filter to name==b"kvm" so we
                        # don't spin on unrelated /dev churn (pts, etc.).
                        try:
                            while True:
                                _buf = _os.read(_ifd, 4096)
                                _off = 0
                                while _off + _EVT_HDR.size <= len(_buf):
                                    _, _, _, _nlen = _EVT_HDR.unpack_from(_buf, _off)
                                    _name = _buf[_off+_EVT_HDR.size : _off+_EVT_HDR.size+_nlen]
                                    _events += 1
                                    if _name.rstrip(b"\x00") == b"kvm":
                                        _saw_kvm = True
                                        _kvm_events += 1
                                    _off += _EVT_HDR.size + _nlen
                        except BlockingIOError:
                            pass
                    # Try open on kvm event OR 1s tick (belt-and-suspenders
                    # against coalesced events or name-filter edge cases).
                    if (_saw_kvm or not _r) and _kvm_try_open():
                        _kvm_preopen_ok = True
                        _kvm_method = f"inotify(kvm-ev#{_kvm_events})" if _saw_kvm else "tick"
                print(f"[kvm-preopen] inotify: {_events} /dev events "
                      f"({_kvm_events} kvm) in {_t.monotonic()-_t0:.1f}s")
            else:
                print(f"[kvm-preopen] inotify_add_watch(/dev) failed "
                      f"(errno={_ct.get_errno()}) — tight-poll fallback")
            _os.close(_ifd)
        else:
            print(f"[kvm-preopen] inotify_init1 failed "
                  f"(errno={_ct.get_errno()}) — tight-poll fallback")

    # Attempt 3: tight-poll fallback. Runs if inotify setup failed at ANY
    # stage (init1 OR add_watch). stat() is cheap (~µs); when mode shows
    # world-rw, open immediately. 1ms sleep: ~1000 checks/s, keeps CPU
    # sane while still catching windows far shorter than v29's 100ms poll.
    if not _kvm_preopen_ok and not _inotify_ok:
        _deadline = _t0 + _KVM_WAIT_S
        while _t.monotonic() < _deadline:
            if _stat.S_IMODE(_os.stat("/dev/kvm").st_mode) & 0o006:
                if _kvm_try_open():
                    _kvm_preopen_ok = True
                    _kvm_method = "tight-poll"
                    break
            _t.sleep(0.001)

    _elapsed = _t.monotonic() - _t0
    if _kvm_preopen_ok:
        _m = oct(_stat.S_IMODE(_os.stat("/dev/kvm").st_mode))
        print(f"[kvm-preopen] /dev/kvm opened at fd={_KVM_PRELOAD_FD} "
              f"via {_kvm_method} after {_elapsed:.2f}s "
              f"(mode now {_m}), shim={_KVM_SHIM}")
    else:
        print(f"[kvm-preopen] WARNING: no /dev/kvm 666-window in "
              f"{_KVM_WAIT_S}s ({_elapsed:.1f}s elapsed) — shim fallback "
              f"will retry per-qemu. This means NO concurrent sandbox "
              f"started on this host in {_KVM_WAIT_S}s, which is unusual "
              f"under load. See kvm-preopen.nix header for infra-fix options.")

    # ALWAYS wrap Popen — even if preopen failed, the shim's constructor
    # has its own open fallback. LD_PRELOAD is set unconditionally;
    # KVM_PRELOAD_FD only if preopen succeeded.
    import subprocess as _sp
    _orig_popen = _sp.Popen

    class _KvmPopen(_orig_popen):  # type: ignore[misc]
        def __init__(self, args, **kw):
            cmd = args if isinstance(args, str) else " ".join(args) if isinstance(args, (list, tuple)) else ""
            is_vm_launch = kw.get("shell") and "run-" in cmd and "-vm" in cmd
            if is_vm_launch:
                env = dict(kw.get("env") or _os.environ)
                env["LD_PRELOAD"] = _KVM_SHIM + (
                    (":" + env["LD_PRELOAD"]) if env.get("LD_PRELOAD") else ""
                )
                if _kvm_preopen_ok:
                    env["KVM_PRELOAD_FD"] = str(_KVM_PRELOAD_FD)
                    pf = set(kw.get("pass_fds") or ())
                    pf.add(_KVM_PRELOAD_FD)
                    kw["pass_fds"] = tuple(pf)
                kw["env"] = env
                _inj = "fd+LD_PRELOAD" if _kvm_preopen_ok else "LD_PRELOAD(fallback)"
                print(f"[kvm-preopen] injecting {_inj} into: {cmd[:100]}")
            super().__init__(args, **kw)

    setattr(_sp, "Popen", _KvmPopen)
    print(f"[kvm-preopen] subprocess.Popen wrapped "
          f"({'fd-inherit' if _kvm_preopen_ok else 'shim-fallback-only'} mode)")
    # ── end kvm-preopen ──
  '';
}
