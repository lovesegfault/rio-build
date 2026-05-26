# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # eio-circuit-breaker — store outage mid-build: bounded EIO + breaker
  # ══════════════════════════════════════════════════════════════════
  # The build mounts and prefetches while the store is up, then sleeps a
  # 60 s window during which the test scales rio-store to 0 (observed
  # ~10-25 s). The six never-before-read 64 KiB inputs it cats
  # afterwards all fail their fetches. With the deployment scaled away
  # the ClusterIP has no endpoints and connects hang rather than reset,
  # so each failed open is bounded by jit_fetch_timeout (60 s) — not
  # instant, but never an unbounded hang — and after 5 consecutive
  # failures the fetch breaker opens (circuit_open = 1) so everything
  # after fails fast. The script swallows the failures — the build
  # itself is reclaimed by cancel, and the store is scaled back up
  # before the next subtest.
  #
  # The wait budget on the breaker gate is the EIO-bounded proof:
  # 60 s window + 5 × jit_fetch_timeout ≈ 360 s worst case before the
  # breaker opens; 540 s covers it with headroom, while a true hang
  # (an open that never returns) would still blow the gate.
  with subtest("eio-circuit-breaker: store outage trips the fetch breaker, opens stay bounded"):
      extras_args = " ".join(
          f"--arg e{i + 1} '(builtins.storePath {p})'" for i, p in enumerate(p_eio_extras)
      )
      drv_eb, build_eb = submit_drv("${eioBreakerDrv}", extra_args=extras_args)
      pod = castore_pod()
      # Mount + DAG prefetch done (store still up at that point).
      wait_worker_metric(
          pod,
          "grep -q '^rio_builder_castore_dag_prefetch_seconds_count'",
          timeout=300,
          ctx="eio-circuit-breaker mount",
      )

      t_down = time.time()
      kubectl("scale deploy/rio-store --replicas=0", ns="${nsStore}")
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsStore} get pods -l app.kubernetes.io/name=rio-store "
          "--no-headers 2>/dev/null | grep -q .",
          timeout=120,
      )
      print(f"eio-circuit-breaker: store scaled away in {time.time() - t_down:.0f}s "
            "(must be well under the script's 60s pre-read window)")

      # The breaker opening is the structural "five consecutive reads
      # failed, each within its fetch budget" signal. 540 s = 60 s
      # pre-read window + 5 × 60 s jit_fetch_timeout + headroom.
      wait_worker_metric(
          pod,
          "grep -Eq '^rio_builder_castore_fuse_circuit_open 1(\\.0+)?$'",
          timeout=540,
          ctx="eio-circuit-breaker breaker open",
      )

      m = worker_metrics(pod)
      n_eio = series(m, "rio_builder_castore_fuse_eio_total")
      n_miss_small = series(m, "rio_builder_castore_fuse_open_case_total", must=('case="miss_small"',))
      n_retries = series(m, "rio_builder_castore_fuse_fetch_retries_total")
      n_circuit = series(m, "rio_builder_castore_fuse_circuit_open")
      assert n_circuit == 1, (
          f"eio-circuit-breaker: circuit_open gauge = {n_circuit}, expected 1"
      )
      assert n_eio >= 5, (
          f"eio-circuit-breaker: only {n_eio} EIO replies for six failed opens "
          f"(expected ≥ 5)"
      )
      assert n_miss_small >= 5, (
          f"eio-circuit-breaker: only {n_miss_small} miss_small opens recorded; "
          f"open_case: {fam(m, 'rio_builder_castore_fuse_open_case_total')!r}"
      )
      print(
          f"eio-circuit-breaker: eio={n_eio}, miss_small={n_miss_small}, "
          f"transient retries={n_retries} (informational)"
      )

      finish_async(build_eb, "eio-circuit-breaker")

      # Bring the store back and wait until it serves again — the
      # remaining faults subtests depend on it.
      kubectl("scale deploy/rio-store --replicas=1", ns="${nsStore}")
      k3s_server.wait_until_succeeds(
          "k3s kubectl -n ${nsStore} wait --for=condition=Available "
          "deploy/rio-store --timeout=60s",
          timeout=240,
      )
      print("eio-circuit-breaker PASS: breaker opened, opens stayed bounded, store restored")
''
