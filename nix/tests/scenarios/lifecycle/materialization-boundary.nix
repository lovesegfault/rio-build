# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # materialization-boundary — Phase B kind-boundary proof (flag-on,
  # builder-traffic half)
  # ══════════════════════════════════════════════════════════════════
  # Substitution-replacement campaign (PD-B10, the Phase A dormancy
  # fragment's flag-on successor): against a deployment whose builder
  # traffic HAS minted executions and written attempt rows
  # (cancel-cgroup-kill + build-timeout above), the materialization
  # cutover must keep build traffic strictly build-kind:
  #
  #   - the durable wanted relation IS written (>0 rows — design §6/
  #     AS-1 says every merge writes it flag-on; its non-zero-ness is
  #     itself the proof the flag-on merge path is live);
  #   - but NO materialization jobs exist (lifecycle builds have no
  #     upstream caches → nothing is substitutable → the probe routes
  #     everything to builders);
  #   - and NO ledger/execution/pin row carries a materialization
  #     kind/class — builder traffic mints only build-kind state.
  #
  # Ordering dependency: this fragment MUST run after the build
  # subtests (the subtests list places it last). Without prior builds
  # the non-vacuity precondition below fails loudly instead of letting
  # the kind-boundary clauses pass as a proof of nothing.
  #
  # Tracey: r[verify sched.materialize.job] / r[verify
  # sched.materialize.pinning] live at the default.nix subtests entry
  # (P0341 convention — marker at wiring point, not fragment header).
  with subtest("materialization-boundary: flag-on builder traffic stays build-kind"):
      # Non-vacuity precondition: real builder traffic exists. The
      # lifecycle-core builds (cancel-cgroup-kill, build-timeout) mint
      # pull executions; if none exist the clauses below are vacuous.
      execs = int(psql_k8s(k3s_server, "SELECT count(*) FROM drv_executions"))
      assert execs > 0, (
          "non-vacuity FAIL: zero drv_executions rows — this fragment "
          "must run after build subtests (cancel-cgroup-kill, "
          "build-timeout) so the build-kind clauses are tested against "
          "real builder traffic"
      )
      attempts = int(psql_k8s(k3s_server, "SELECT count(*) FROM drv_attempts"))
      pins = int(psql_k8s(k3s_server, "SELECT count(*) FROM scheduler_live_pins"))
      print(f"materialization-boundary: traffic = {execs} execution(s), "
            f"{attempts} attempt row(s), {pins} live pin(s)")

      # The durable wanted relation is being written (the clause that
      # INVERTS from the Phase A dormancy fragment): flag-on, every
      # merge writes one row per (build, node) pair (design §6/AS-1).
      wanted = int(psql_k8s(k3s_server, "SELECT count(*) FROM build_wanted_outputs"))
      assert wanted > 0, (
          "flag-on deployment wrote ZERO build_wanted_outputs rows — the "
          "merge-transaction wanted-relation write (design §6/AS-1) is "
          "not running; is the flag actually on?"
      )

      # The kind boundary: build traffic creates NO materialization
      # state. Lifecycle builds have no upstream caches, so nothing is
      # substitutable and no jobs may exist; and no ledger/execution/pin
      # row may carry a materialization kind/class.
      mat_rows = psql_k8s(
          k3s_server,
          "SELECT (SELECT count(*) FROM materialization_jobs)"
          " + (SELECT count(*) FROM drv_attempts WHERE outcome_class LIKE 'materialization%')"
          " + (SELECT count(*) FROM drv_executions WHERE attempt_kind = 'materialization')"
          " + (SELECT count(*) FROM scheduler_live_pins WHERE pin_kind = 'materialization')",
      )
      assert mat_rows == "0", (
          f"flag-on BUILD traffic created materialization state: {mat_rows} "
          f"row(s) across jobs/attempts/executions/pins — the kind boundary "
          f"(build traffic stays build-kind) is violated"
      )

      # The positive form of the kind clauses, asserted separately so a
      # violation names the table: every execution row carries the
      # build kind; every pin (when any exist) carries build_input.
      non_build_execs = psql_k8s(
          k3s_server,
          "SELECT count(*) FROM drv_executions WHERE attempt_kind <> 'build'",
      )
      assert non_build_execs == "0", (
          f"{non_build_execs} drv_executions row(s) carry a non-build "
          f"attempt_kind for builder traffic"
      )
      non_build_pins = psql_k8s(
          k3s_server,
          "SELECT count(*) FROM scheduler_live_pins WHERE pin_kind <> 'build_input'",
      )
      assert non_build_pins == "0", (
          f"{non_build_pins} scheduler_live_pins row(s) carry a "
          f"non-build_input pin_kind for builder traffic"
      )

      # The executor spawn condition reaches the deployed STORE pod:
      # the chart renders RIO_MATERIALIZATION__SCHEDULER_ADDR from
      # values (PD-D2 — the env IS the spawn condition; the coexistence
      # ENABLED flag is gone). The env-spec check catches the plumb
      # being silently dropped. Same kubectl-jsonpath pattern as
      # jwt-mount-present (the pod spec is what the kubelet used to
      # build the container).
      envs = kubectl(
          "get deploy rio-store -o jsonpath="
          "'{.spec.template.spec.containers[0].env}'",
          ns="${nsStore}",
      )
      assert "RIO_MATERIALIZATION__SCHEDULER_ADDR" in envs, (
          f"rio-store env spec is missing RIO_MATERIALIZATION__SCHEDULER_ADDR — "
          f"the helm env plumb was dropped: {envs!r}"
      )
      print(f"materialization-boundary PASS: {wanted} wanted row(s) written, "
            f"all {execs} executions build-kind, scheduler_addr plumbed")
''
