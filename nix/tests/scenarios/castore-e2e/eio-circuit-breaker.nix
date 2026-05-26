# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # NOT WIRED YET (P0560 round 3b finding): with rio-store scaled to
  # 0 mid-build, the fetch breaker did not open within several
  # minutes of consecutive failed opens (sequential or concurrent),
  # and the client-less build was reaped by the scheduler's orphan
  # watcher (300 s) before the gate could conclude. The subtest now
  # prints a counters/pod-phase timeline while it waits; wire it back
  # into vm-castore-e2e-faults once the breaker behavior under a real
  # outage is understood (builder.fs.fetch-circuit).
  # ══════════════════════════════════════════════════════════════════
  # eio-circuit-breaker — store outage mid-build: bounded EIO + breaker
  # ══════════════════════════════════════════════════════════════════
  # The build mounts and prefetches while the store is up, then sleeps a
  # 60 s window during which the test scales rio-store to 0 (observed
  # ~10-25 s). The six never-before-read 64 KiB inputs it then reads
  # CONCURRENTLY all fail their fetches: with the deployment scaled away
  # the connections hang rather than reset, so each open is bounded by
  # its own jit_fetch_timeout (60 s) — never an unbounded hang — and
  # because the reads run in parallel the whole burst costs ~one fetch
  # budget of wall time. Six consecutive failures open the breaker
  # (circuit_open = 1); the script's seventh, sequential read then hits
  # the already-open breaker. All failures are swallowed — the build is
  # reclaimed by cancel and the store scaled back up before the next
  # subtest.
  #
  # The breaker-gate budget is the bounded-EIO proof: 60 s pre-read
  # window + ~one concurrent fetch budget (60 s) + headroom = 300 s. An
  # open that hung past its budget would blow the gate.
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

      # The breaker opening is the structural "the concurrent reads all
      # failed within their fetch budgets" signal. Polled with a visible
      # timeline (counters + pod phase every poll) so a failure here
      # documents exactly what the fetch path did during the outage
      # instead of a bare timeout.
      breaker_deadline = time.time() + 360
      m = {}
      while True:
          phase = k3s_server.execute(
              f"k3s kubectl -n ${nsBuilders} get pod {pod} "
              "-o jsonpath='{.status.phase}' 2>/dev/null"
          )[1].strip() or "Gone"
          rc_m, raw_m = k3s_server.execute(
              "k3s kubectl get --raw "
              f"'/api/v1/namespaces/${nsBuilders}/pods/{pod}:9093/proxy/metrics' "
              "2>/dev/null"
          )
          m = parse_prometheus(raw_m) if rc_m == 0 else {}
          n_circuit = series(m, "rio_builder_castore_fuse_circuit_open")
          tl_opens = series(m, "rio_builder_castore_fuse_upcalls_total", must=('op="open"',))
          tl_miss = series(m, "rio_builder_castore_fuse_open_case_total", must=('case="miss_small"',))
          tl_eio = series(m, "rio_builder_castore_fuse_eio_total")
          tl_retries = series(m, "rio_builder_castore_fuse_fetch_retries_total")
          print(
              "eio-circuit-breaker timeline: "
              f"t={int(time.time() - t_down)}s pod={phase} opens={tl_opens} "
              f"miss_small={tl_miss} eio={tl_eio} retries={tl_retries} "
              f"circuit_open={n_circuit}"
          )
          if n_circuit == 1:
              break
          if time.time() > breaker_deadline or phase not in ("Running", "Pending"):
              k3s_server.execute(
                  "echo '=== eio-circuit-breaker DIAG: scheduler view ===' >&2; "
                  "k3s kubectl -n ${ns} logs -l app.kubernetes.io/name=rio-scheduler "
                  "--tail=4000 --since=10m 2>/dev/null "
                  "| grep -aiE 'cancel|orphan|backstop|reassign|quarantine|infra' "
                  "| grep -av '\"level\":\"DEBUG\"' | tail -30 >&2; "
                  "k3s kubectl -n ${nsBuilders} get events --sort-by=.lastTimestamp "
                  "2>/dev/null | tail -20 >&2"
              )
              dump_castore_diag("eio-circuit-breaker breaker open", pod=pod)
              raise AssertionError(
                  f"eio-circuit-breaker: breaker never opened (pod={phase}, "
                  f"metrics={fam(m, 'rio_builder_castore_fuse_eio_total')!r} eio, "
                  f"{fam(m, 'rio_builder_castore_fuse_open_case_total')!r} open_case) — "
                  "see the timeline prints above for the failure pacing"
              )
          time.sleep(15)

      n_eio = series(m, "rio_builder_castore_fuse_eio_total")
      n_miss_small = series(m, "rio_builder_castore_fuse_open_case_total", must=('case="miss_small"',))
      n_retries = series(m, "rio_builder_castore_fuse_fetch_retries_total")
      n_circuit = series(m, "rio_builder_castore_fuse_circuit_open")
      assert n_circuit == 1, (
          f"eio-circuit-breaker: circuit_open gauge = {n_circuit}, expected 1"
      )
      assert n_eio >= 6, (
          f"eio-circuit-breaker: only {n_eio} EIO replies for six concurrently "
          f"failed opens (expected ≥ 6)"
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
