# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # cache-readonly — a build cannot write the shared node cache
  # ══════════════════════════════════════════════════════════════════
  # passthrough-small's script attempted `echo poison >
  # /var/rio/cache/ab/test` from inside the sandbox and would have
  # exited 9 (before its probe marker) had the write succeeded. The
  # precise errno is environment-dependent (EROFS if the executor pod
  # maps the cache read-only, ENOENT if the sandbox does not map it at
  # all) — the security property is simply that the file cannot appear.
  # No build of its own: asserts on the evidence captured in
  # passthrough-small plus a host probe.
  with subtest("cache-readonly: sandbox write to /var/rio/cache cannot land"):
      assert "CASTORE-PROBE-READY" in pt_small_logs, (
          "cache-readonly: passthrough-small's script exited before its probe "
          "marker — the poison write into /var/rio/cache appears to have "
          f"SUCCEEDED (exit 9 path). Pod log tail:\n{pt_small_logs[-2000:]}"
      )
      k3s_agent.succeed("! test -e /var/rio/cache/ab/test")

      # Config-shape evidence (informational): how the executor pod
      # mounts the node caches, if at all. The host probe above is the
      # enforcement assertion.
      print(f"cache-readonly: executor volumeMounts: {pt_small_mounts.strip()}")
      print("cache-readonly PASS: poison file never appeared on the node")
''
