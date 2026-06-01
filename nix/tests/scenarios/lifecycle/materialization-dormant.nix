# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # materialization-dormant — Phase A dormancy criterion 2 (flag-off,
  # builder-traffic half)
  # ══════════════════════════════════════════════════════════════════
  # Substitution-replacement campaign: the same five-table zero-count
  # as the substitute scenario's subtest, but against a deployment
  # whose builder traffic HAS minted executions and written attempt
  # rows (cancel-cgroup-kill + build-timeout above) — proving the
  # traffic-dependent clauses non-vacuously: every execution row is
  # attempt_kind='build', every pin is pin_kind='build_input', and no
  # attempt carries a materialization_* class.
  #
  # Ordering dependency: this fragment MUST run after the build
  # subtests (the subtests list places it last). Without prior builds
  # the non-vacuity precondition below fails loudly instead of letting
  # the zero-counts pass as a proof of nothing.
  #
  # Tracey: r[verify sched.materialize.job] / r[verify
  # sched.materialize.pinning] live at the default.nix subtests entry
  # (P0341 convention — marker at wiring point, not fragment header).
  with subtest("materialization-dormant: builder traffic mints only build-kind rows"):
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
      print(f"materialization-dormant: traffic = {execs} execution(s), "
            f"{attempts} attempt row(s), {pins} live pin(s)")

      # The five-table zero-count (dormancy criterion 2): flag-off, NO
      # materialization state may exist anywhere.
      mat_rows = psql_k8s(
          k3s_server,
          "SELECT (SELECT count(*) FROM materialization_jobs)"
          " + (SELECT count(*) FROM build_wanted_outputs)"
          " + (SELECT count(*) FROM drv_attempts WHERE outcome_class LIKE 'materialization%')"
          " + (SELECT count(*) FROM drv_executions WHERE attempt_kind = 'materialization')"
          " + (SELECT count(*) FROM scheduler_live_pins WHERE pin_kind = 'materialization')",
      )
      assert mat_rows == "0", (
          f"flag-off deployment created materialization state: {mat_rows} "
          f"row(s) across the five dormancy tables/clauses (Phase A "
          f"criterion 2 violation)"
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
          f"attempt_kind on a flag-off deployment"
      )
      non_build_pins = psql_k8s(
          k3s_server,
          "SELECT count(*) FROM scheduler_live_pins WHERE pin_kind <> 'build_input'",
      )
      assert non_build_pins == "0", (
          f"{non_build_pins} scheduler_live_pins row(s) carry a "
          f"non-build_input pin_kind on a flag-off deployment"
      )

      # The flag really is off in the deployed PODS' environment: the
      # helm chart renders RIO_MATERIALIZATION__ENABLED from values
      # (Phase A default false in every values file). The env-spec
      # check also catches the plumb being silently dropped — the env
      # entry must EXIST and must not be "true". Same kubectl-jsonpath
      # pattern as jwt-mount-present (the pod spec is what the kubelet
      # used to build the container).
      for dep, dep_ns in [("rio-scheduler", "${ns}"), ("rio-store", "${nsStore}")]:
          envs = kubectl(
              f"get deploy {dep} -o jsonpath="
              f"'{{.spec.template.spec.containers[0].env}}'",
              ns=dep_ns,
          )
          assert "RIO_MATERIALIZATION__ENABLED" in envs, (
              f"{dep} env spec is missing RIO_MATERIALIZATION__ENABLED — "
              f"the helm env plumb was dropped: {envs!r}"
          )
          flag_on = '"name":"RIO_MATERIALIZATION__ENABLED","value":"true"'
          assert flag_on not in envs.replace(" ", ""), (
              f"{dep} unexpectedly enables materialization: {envs!r}"
          )
      print(f"materialization-dormant PASS: zero materialization rows, "
            f"all {execs} executions build-kind, flags off in both deployments")
''
