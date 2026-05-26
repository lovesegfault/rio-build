# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # cache-readonly — a build cannot write the shared node cache
  # ══════════════════════════════════════════════════════════════════
  # passthrough-small's script attempted `echo poison >
  # /var/rio/cache/ab/test` from inside the sandbox and would have
  # exited 9 — before its warm_4k sentinel read — had the write
  # succeeded; that subtest already gated on the sentinel's cache
  # entry, so reaching this point proves the exit-9 branch was not
  # taken. The precise errno is environment-dependent (EROFS if the
  # executor pod maps the cache read-only, ENOENT if the sandbox does
  # not map it at all) — the security property is simply that the file
  # cannot appear. No build of its own: a host probe plus the evidence
  # captured in passthrough-small.
  with subtest("cache-readonly: sandbox write to /var/rio/cache cannot land"):
      k3s_agent.succeed("! test -e /var/rio/cache/ab/test")

      # Config-shape evidence (informational): how the executor pod
      # mounts the node caches, if at all. The host probe above is the
      # enforcement assertion; the sentinel gate in passthrough-small is
      # the in-sandbox half (the write attempt did not succeed).
      print(f"cache-readonly: executor volumeMounts: {pt_small_mounts.strip()}")
      print("cache-readonly PASS: poison file never appeared on the node")
''
