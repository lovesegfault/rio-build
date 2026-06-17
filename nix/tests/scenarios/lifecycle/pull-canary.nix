# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
#
# pull-canary: hosted ONLY by vm-pull-canary-k3s (its fixture layers
# values/vmtest-pull-canary.yaml, pinning the SLA probe deadline to the
# 180s floor so the establishment window fits the budget). Do not add
# this fragment to the lifecycle splits that use the plain vmtest-full
# fixture — the establishment arm would wait ~1h there.
#
# What the fragment proves (the T-1b.8/T-1b.9 slice of the 1b gate):
#
#   - Pull retry-feed (fold input): a scripted {success,
#     deterministic-failure} sequence on a Builder pool. The
#     failure leg (exit≠0 → ExecutorVariantFailure, sh-012 E3a) is
#     folded by the worker-report path as one `executor_variant`
#     worker-reported attempt row PER attempt (never a double charge —
#     exec_id is schema-unique, one row per distinct exec_id), retried
#     up to max_retries (default 2 → three attempts), the success leg
#     charges nothing, the failed derivation ends poisoned and the
#     client sees the failure. (The stream-baseline leg this used to
#     be compared against retired with the stream session machinery —
#     1c' deletion commit A; these are now absolute assertions on the
#     pull leg.)
#     Exclusion keying per AD2: the pull row is keyed by the attested
#     intent identity with source_node as the node key when the
#     controller-authoritative binding is reported. In this fixture
#     the per-pool reconciler ships no bound_intents, so source_node
#     stays NULL — printed, not asserted; the node-keyed exclusion
#     half of AD2 is covered by the T-1b.1/T-1b.2 unit and contract
#     batteries, not by this scenario.
#   - Cancel timing (T-1b.9): CancelBuild on a running pull-mode build
#     closes the attempt, ends the drv Cancelled, and the controller
#     foreground-deletes the Job on the closed-while-active edge; the
#     build cgroup, pod and Job are gone within 90s of the verdict and
#     the cancelled exec is charged nothing. Structurally well inside
#     the establishment window (the window is >= 120s report slack).
#   - Preempt timing (T-1b.9): patching DisruptionTarget=True on a
#     pull-mode pod makes the controller synthesize the preempted
#     report and foreground-delete the owning Job; pod+Job gone and the
#     attempt closed at the report fold within 90s. Per AD5 a
#     controller-initiated preempt of still-wanted work closes the
#     attempt UNCHARGED: exactly one terminal row for the preempted
#     exec, outcome class disconnected (the no-charge class), reason
#     preempted (the synthesized verdict) or worker_abort (the pod's
#     SIGTERM-abort report when the synthesized one loses the race),
#     the requeued drv still delivers its store path, and no second row
#     is ever minted for that exec.
#   - Establishment window: a pull-mode pod whose builder process is
#     SIGKILLed from the host produces a plain Error pod — no
#     SIGTERM-abort report, nothing the controller classifies — so the
#     attempt stays open and uncharged for the whole window and is then
#     established exactly once as executor_crash/unreported by the
#     sweep, only after deadline + report-slack; the derivation
#     requeues and the same client build still gets its store path.
#
# Out of scope here (documented carve-outs): the busy-bridge arm and
# the NotYetReady arm (no RIO_ORPHAN_REAP_GRACE_OVERRIDE_SECS in this
# fixture), the rollback-by-template-flip demonstration, and the
# small-fleet NoEligibleSource ending (needs the node-keyed exclusion,
# i.e. a controller-authoritative binding source this fixture lacks).
scope: with scope; ''
  import time

  # establishment_report_slack default (rio-scheduler/src/config.rs).
  PC_SLACK_SECS = 120

  def pc_count(sql):
      return int(psql_k8s(k3s_server, sql).strip() or "0")

  def pc_attempt_rows(marker):
      """All drv_attempts rows ever recorded for the marker drv."""
      return pc_count(
          "SELECT count(*) FROM drv_attempts a "
          "JOIN derivations d ON d.derivation_id = a.derivation_id "
          f"WHERE d.drv_path LIKE '%{marker}%'"
      )

  def pc_charge_facts(marker, want_n=1):
      """The ledger row(s) for marker, each as a list of fields:
      [outcome_class, termination_reason, reporting_party, executor_id,
      source_node, exec_id_is_intent_key, event_kind, exec_id].
      Asserts exactly want_n rows exist; returns the first row's
      fields (callers that need all rows use the second return)."""
      out = psql_k8s(k3s_server,
          "SELECT a.outcome_class, coalesce(a.termination_reason, '<none>'), "
          "a.reporting_party, coalesce(a.executor_id, '<none>'), "
          "coalesce(a.source_node, '<none>'), "
          "(a.executor_id = d.drv_hash)::text, a.event_kind, a.exec_id::text "
          "FROM drv_attempts a "
          "JOIN derivations d ON d.derivation_id = a.derivation_id "
          f"WHERE d.drv_path LIKE '%{marker}%'"
      ).strip()
      rows = [r for r in out.split("\n") if r.strip()]
      assert len(rows) == want_n, (
          f"expected exactly {want_n} ledger row(s) for {marker!r}, got {len(rows)}: {rows!r}"
      )
      parsed = [[f.strip() for f in r.split("|")] for r in rows]
      return parsed[0], parsed

  def pc_drv_status(marker):
      return psql_k8s(k3s_server,
          "SELECT status FROM derivations "
          f"WHERE drv_path LIKE '%{marker}%'"
      ).strip()

  def pc_exec_count(marker):
      """Executions minted for the marker drv (joined through
      assignments — drv_executions.drv_hash holds the log hash, not the
      DAG key). Every execution row is pull-minted: the pull
      transaction is the only writer."""
      return pc_count(
          "SELECT count(*) FROM drv_executions e "
          "JOIN assignments a ON a.exec_id = e.exec_id "
          "JOIN derivations d ON d.derivation_id = a.derivation_id "
          f"WHERE d.drv_path LIKE '%{marker}%'"
      )

  def pc_open(marker):
      """Open attempts for marker: active assignment joined to an
      execution with no terminal drv_attempts fill (the same view
      ListOpenAttempts serves, scoped to one drv)."""
      return pc_count(
          "SELECT count(*) FROM assignments a "
          "JOIN drv_executions e ON e.exec_id = a.exec_id "
          "JOIN derivations d ON d.derivation_id = a.derivation_id "
          "WHERE a.status IN ('pending','acknowledged') "
          f"AND d.drv_path LIKE '%{marker}%' "
          "AND NOT EXISTS (SELECT 1 FROM drv_attempts t "
          " WHERE t.exec_id = a.exec_id "
          " AND t.termination_reason IS NOT NULL)"
      )

  def pc_open_exec(marker):
      """exec_id of the currently-open attempt for marker (empty
      string when none)."""
      return psql_k8s(k3s_server,
          "SELECT a.exec_id FROM assignments a "
          "JOIN drv_executions e ON e.exec_id = a.exec_id "
          "JOIN derivations d ON d.derivation_id = a.derivation_id "
          "WHERE a.status IN ('pending','acknowledged') "
          f"AND d.drv_path LIKE '%{marker}%' "
          "AND NOT EXISTS (SELECT 1 FROM drv_attempts t "
          " WHERE t.exec_id = a.exec_id "
          " AND t.termination_reason IS NOT NULL)"
      ).strip()

  def pc_wait_open(marker, want, timeout, ctx):
      deadline = time.time() + timeout
      seen = -1
      while time.time() < deadline:
          seen = pc_open(marker)
          if seen == want:
              return
          time.sleep(3)
      raise AssertionError(
          f"pull-canary {ctx}: open pull attempts for {marker!r} stuck at "
          f"{seen}, wanted {want} within {timeout}s"
      )

  def pc_running_pod(timeout=240):
      """Wait for a Running pull-canary pod; return (name, node, vm)."""
      k3s_server.wait_until_succeeds(
          "test -n \"$(k3s kubectl -n ${nsBuilders} get pods "
          "-l rio.build/pool=pull-canary "
          "--field-selector=status.phase=Running -o name)\"",
          timeout=timeout,
      )
      name = k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} get pods "
          "-l rio.build/pool=pull-canary "
          "--field-selector=status.phase=Running "
          "-o jsonpath='{.items[0].metadata.name}'"
      ).strip()
      node = k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} get pod {name} "
          "-o jsonpath='{.spec.nodeName}'"
      ).strip()
      vm = k3s_agent if node == "k3s-agent" else k3s_server
      return name, node, vm

  def pc_owning_job(pod):
      return k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} get pod {pod} "
          "-o jsonpath='{.metadata.ownerReferences[0].name}'"
      ).strip()

  def pc_wait_build_cgroup(vm, marker, timeout=180):
      """Wait for the build cgroup of marker on the given node and
      return its host-side path — proves the build is genuinely
      in flight (daemon spawned, sleep started), not just pulled."""
      return vm.wait_until_succeeds(
          f"find /sys/fs/cgroup -type d -name '*{marker}_drv' "
          "-print -quit 2>/dev/null | grep .",
          timeout=timeout,
      ).strip()

  def pc_bg_build(drv_file, tag):
      """Background ssh-ng nix-build (keeps a gateway watcher attached
      so the orphan-watcher backstop never cancels long arms)."""
      client.succeed(
          "nix-build --no-out-link --store 'ssh-ng://k3s-server' "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          f"{drv_file} > /tmp/{tag}.out 2>&1 & "
          f"echo $! > /tmp/{tag}.pid"
      )

  def pc_bg_wait(tag, timeout):
      client.wait_until_succeeds(
          f"! kill -0 $(cat /tmp/{tag}.pid) 2>/dev/null",
          timeout=timeout,
      )
      return client.succeed(f"cat /tmp/{tag}.out").strip()

  # ══════════════════════════════════════════════════════════════════
  # Arm 1 — pull retry-feed: the scripted {success, failure} sequence
  # on a Builder pool; assert the fold input directly.
  # ══════════════════════════════════════════════════════════════════
  with subtest("pull-canary: pull-mode pool runs the scripted sequence (retry-feed)"):
      kubectl(
          "delete pool x86-64 --ignore-not-found --wait=true",
          ns="${nsBuilders}",
      )
      # Pod-level precondition only — same reasoning as the pull-mode
      # fragment banner: pull pods never register, so a leftover pod
      # from the fixture default pool is the only thing that could
      # steal the dispatch.
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get pods --no-headers 2>/dev/null | grep -q .",
          timeout=120,
      )
      k3s_server.succeed(
          "k3s kubectl apply -f - <<'EOF'\n"
          "apiVersion: rio.build/v1alpha1\n"
          "kind: Pool\n"
          "metadata:\n"
          "  name: pull-canary\n"
          "  namespace: ${nsBuilders}\n"
          "spec:\n"
          "  kind: Builder\n"
          "  maxConcurrent: 2\n"
          "  systems: [x86_64-linux]\n"
          "  image: rio-builder:dev\n"
          "  imagePullPolicy: Never\n"
          "  privileged: true\n"
          "  tolerations: null\n"
          "EOF"
      )

      ok_out = build("${pcPullOk}", timeout_wrap=420)
      assert "/nix/store/" in ok_out, (
          f"pull success leg must deliver a store path, got: {ok_out!r}"
      )
      fail_out = build("${pcPullFail}", expect_fail=True, timeout_wrap=420)
      print(f"pull-canary pull-fail client tail: {fail_out[-300:]!r}")

      assert pc_attempt_rows("pc-pull-ok") == 0, (
          "a clean pull build must charge nothing (the ledger records "
          "failures, never successes)"
      )
      assert pc_drv_status("pc-pull-ok") == "completed", (
          f"pull success leg should end completed, got {pc_drv_status('pc-pull-ok')!r}"
      )
      # sh-012 E3a: exit!=0 maps NixStatus::PermanentFailure ->
      # BuildResultStatus::ExecutorVariantFailure -> the kernel's E3a
      # arm, which retries up to max_retries (default 2 -> three
      # attempts) before Poison. PC_FAIL_ATTEMPTS is 1+max_retries.
      PC_FAIL_ATTEMPTS = 3
      pull_facts, pull_all = pc_charge_facts(
          "pc-pull-fail", want_n=PC_FAIL_ATTEMPTS
      )
      p_class, p_reason, p_party, p_exec, p_node, p_is_intent, p_kind = pull_facts[:7]
      # The fold-input assertions on the pull leg: an exit!=0 build
      # failure is one `executor_variant` worker-reported attempt row
      # per attempt (E3a), one charge per attempt with no
      # double-charge (one row per distinct exec_id), zero charges for
      # the success leg, and the client-visible
      # verdict matches. The exclusion KEY is the attested intent
      # identity (+ node key when the binding is reported), per AD2.
      for r in pull_all:
          assert r[0] == "executor_variant", (
              f"an exit!=0 pull-leg build failure must classify as an "
              f"executor_variant worker-reported attempt (sh-012 E3a), "
              f"got class {r[0]!r} (row: {r!r})"
          )
          assert r[6] == "attempt" and r[2] == "worker", (
              f"each pull failure must be one worker-reported attempt row, got: {r!r}"
          )
          assert r[5] == "true", (
              f"pull rows are keyed by the attested intent identity, got executor_id={r[3]!r}"
          )
      exec_ids = {r[7] for r in pull_all}
      assert len(exec_ids) == PC_FAIL_ATTEMPTS, (
          f"no double-charge: {PC_FAIL_ATTEMPTS} attempts must mint "
          f"{PC_FAIL_ATTEMPTS} distinct exec_ids, got {exec_ids!r}"
      )
      assert pc_exec_count("pc-pull-fail") >= 1, (
          f"the failure leg must have minted an execution via the pull "
          f"transaction, got {pc_exec_count('pc-pull-fail')}"
      )
      assert pc_drv_status("pc-pull-fail") == "poisoned", (
          f"the E3a-exhausted pull failure must poison the drv, "
          f"got {pc_drv_status('pc-pull-fail')!r}"
      )
      # AD2 carve-out (printed, not asserted): source_node is written
      # only from the controller-authoritative binding; the per-pool
      # reconciler ships no bound_intents in this fixture, so the
      # column stays NULL here. The node-keyed exclusion is covered by
      # the T-1b.1/T-1b.2 unit/contract batteries.
      print(f"pull-canary retry-feed: class {p_class}, reason {p_reason!r}, "
            f"pull key=intent identity, source_node={p_node!r}")
      total_rows = (
          pc_attempt_rows("pc-pull-ok")
          + pc_attempt_rows("pc-pull-fail")
      )
      assert total_rows == PC_FAIL_ATTEMPTS, (
          f"the scripted sequence must charge exactly once per failure-leg "
          f"attempt and never for the success leg, got {total_rows} rows total"
      )
      print("pull-canary retry-feed PASS: one executor_variant charge per "
            "E3a attempt, no double charges, the success charge-free")

  # ══════════════════════════════════════════════════════════════════
  # Arm 3 — cancel timing (T-1b.9): CancelBuild on a running pull build.
  # ══════════════════════════════════════════════════════════════════
  with subtest("pull-canary: pull-mode cancel is prompt, charge-free, and never waits for establishment"):
      cancel_drv_path, cancel_build_id = submit_single_drv("${pcCancelDrv}")
      print(f"pull-canary cancel: submitted {cancel_drv_path} build_id={cancel_build_id}")
      pc_wait_open("pc-cancel", 1, 300, "cancel arm")
      cancel_pod, cancel_node, cancel_vm = pc_running_pod()
      cancel_job = pc_owning_job(cancel_pod)
      cancel_exec = pc_open_exec("pc-cancel")
      cancel_cgroup = pc_wait_build_cgroup(cancel_vm, "pc-cancel")
      # The controller cancels only on the closed-while-active edge of
      # an attempt it has previously OBSERVED open (ListOpenAttempts is
      # read once per ~10s reconcile tick) — give it two ticks of open
      # evidence before the verdict.
      time.sleep(20)

      t0 = time.time()
      cancel_resp = sched_grpc(
          json.dumps({"buildId": cancel_build_id, "reason": "pull-canary-cancel"}),
          "rio.scheduler.SchedulerService/CancelBuild",
      )
      print(f"pull-canary cancel: CancelBuild -> {cancel_resp.strip()!r}")

      # Composite bound (AD5 structure): scheduler verdict + <=1
      # controller tick (10s) + foreground Job delete + SIGTERM abort.
      # 90s wall is generous against runner-load variance but far below
      # the establishment window.
      k3s_server.wait_until_succeeds(
          f"! k3s kubectl -n ${nsBuilders} get job {cancel_job} 2>/dev/null && "
          f"! k3s kubectl -n ${nsBuilders} get pod {cancel_pod} 2>/dev/null",
          timeout=90,
      )
      cancel_vm.wait_until_succeeds(f"! test -e {cancel_cgroup}", timeout=30)
      assert pc_open("pc-cancel") == 0, (
          "the cancelled attempt must have left the open-attempt view"
      )
      assert pc_drv_status("pc-cancel") == "cancelled", (
          f"the drv must end Cancelled, got {pc_drv_status('pc-cancel')!r}"
      )
      cancel_elapsed = time.time() - t0
      assert cancel_elapsed <= 90, (
          f"cancel verdict to job+pod+cgroup gone took {cancel_elapsed:.1f}s, "
          f"expected <= 90s"
      )
      # Charge-free and structurally not the establishment sweep: the
      # window is at least the 120s report slack, the verdict landed
      # well before that, and no ledger row exists at all for the
      # cancelled exec (establishment would have written one).
      assert cancel_elapsed < PC_SLACK_SECS, (
          f"the cancel path must not wait for the establishment window "
          f"(>= {PC_SLACK_SECS}s), took {cancel_elapsed:.1f}s"
      )
      cancel_rows = pc_count(
          f"SELECT count(*) FROM drv_attempts WHERE exec_id = '{cancel_exec}'"
      )
      assert cancel_rows == 0 and pc_attempt_rows("pc-cancel") == 0, (
          f"a scheduler-cancelled pull attempt is charge-free, got "
          f"{cancel_rows} rows for the exec / {pc_attempt_rows('pc-cancel')} for the drv"
      )
      print(f"pull-canary TIMING cancel: verdict -> job+pod+cgroup gone, attempt "
            f"closed, drv cancelled in {cancel_elapsed:.1f}s (bound 90s, "
            f"node {cancel_node}, exec {cancel_exec})")

  # ══════════════════════════════════════════════════════════════════
  # Arm 4 — preempt timing (T-1b.9): DisruptionTarget on a pull pod.
  # ══════════════════════════════════════════════════════════════════
  with subtest("pull-canary: DisruptionTarget preemption aborts promptly and the drv is not lost"):
      pc_bg_build("${pcPreemptDrv}", "pc-preempt")
      pc_wait_open("pc-preempt", 1, 300, "preempt arm")
      preempt_pod, preempt_node, preempt_vm = pc_running_pod()
      preempt_job = pc_owning_job(preempt_pod)
      # The requeued drv respawns a SAME-NAME Job (deterministic name);
      # "the preempted Job is gone" must therefore compare instances,
      # not names — capture the UID before the patch.
      preempt_job_uid = k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} get job {preempt_job} "
          "-o jsonpath='{.metadata.uid}'"
      ).strip()
      preempt_exec = pc_open_exec("pc-preempt")
      pc_wait_build_cgroup(preempt_vm, "pc-preempt")
      # One reconcile tick of open evidence (the cancel-successor arm
      # shares the watcher namespace; not strictly required for the
      # disruption path but keeps the arm insensitive to which one
      # deletes the Job first).
      time.sleep(12)

      t0 = time.time()
      k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} patch pod {preempt_pod} "
          "--subresource=status --type=strategic -p "
          "'{\"status\":{\"conditions\":[{\"type\":\"DisruptionTarget\","
          "\"status\":\"True\",\"reason\":\"PreemptionByVMTest\","
          "\"message\":\"pull-canary preempt arm\","
          "\"lastTransitionTime\":\"2026-01-01T00:00:00Z\"}]}}'"
      )
      print(f"pull-canary preempt: DisruptionTarget=True patched on {preempt_pod} "
            f"(node {preempt_node}, exec {preempt_exec})")

      # The controller must do the deletion (report-then-delete): pod
      # and owning Job INSTANCE gone within the same 90s composite
      # bound. By-UID, not by-name: the report fold requeues the
      # still-wanted drv and the spawn pass recreates a Job with the
      # SAME deterministic name within ~1 s of the finalizer — a
      # same-name successor is the drv being NOT lost (this arm's
      # other half), never a failure of the preempt deletion. The
      # by-name form raced that ~1 s window against a 2 s poll and
      # then waited on the successor's whole build instead.
      k3s_server.wait_until_succeeds(
          f"! k3s kubectl -n ${nsBuilders} get pod {preempt_pod} 2>/dev/null && "
          f"[ \"$(k3s kubectl -n ${nsBuilders} get job {preempt_job} "
          f"-o jsonpath='{{.metadata.uid}}' 2>/dev/null)\" != '{preempt_job_uid}' ]",
          timeout=90,
      )
      # The original attempt must be closed at the report fold — gone
      # from the open view (or already superseded by a fresh re-pull).
      closed = False
      while time.time() - t0 < 90:
          if pc_open_exec("pc-preempt") != preempt_exec:
              closed = True
              break
          time.sleep(2)
      preempt_elapsed = time.time() - t0
      assert closed, (
          f"the preempted attempt {preempt_exec} was still the open attempt "
          f"after {preempt_elapsed:.1f}s"
      )
      assert preempt_elapsed <= 90, (
          f"DisruptionTarget to pod+job gone + attempt closed took "
          f"{preempt_elapsed:.1f}s, expected <= 90s"
      )
      assert preempt_elapsed < PC_SLACK_SECS, (
          f"the preemption must be closed by the report fold, not the "
          f"establishment sweep (window >= {PC_SLACK_SECS}s), took {preempt_elapsed:.1f}s"
      )
      # Charge discipline (AD5): a controller-initiated preempt of
      # still-wanted work closes the attempt UNCHARGED — exactly one
      # terminal row for the preempted exec, always the no-charge
      # disconnected class, never an infra or executor_crash charge and
      # never a second row. The reason names whichever closer won the
      # race: "preempted" when the controller-synthesized verdict
      # closed it (the normal order — the report is sent before the
      # Job delete), "worker_abort" when the pod's own SIGTERM-abort
      # report landed first (e.g. the synthesized report failed
      # transiently and the abort beat its retry).
      preempt_rows = pc_count(
          f"SELECT count(*) FROM drv_attempts WHERE exec_id = '{preempt_exec}'"
      )
      preempt_class = psql_k8s(k3s_server,
          f"SELECT outcome_class FROM drv_attempts WHERE exec_id = '{preempt_exec}'"
      ).strip()
      preempt_reason = psql_k8s(k3s_server,
          "SELECT coalesce(termination_reason, '<none>') FROM drv_attempts "
          f"WHERE exec_id = '{preempt_exec}'"
      ).strip()
      assert preempt_rows == 1 and preempt_class == "disconnected", (
          f"the preempted exec must be closed exactly once with the uncharged "
          f"disconnected class, got {preempt_rows} row(s), class {preempt_class!r}"
      )
      assert preempt_reason in ("preempted", "worker_abort"), (
          f"the close must come from the synthesized verdict or the SIGTERM-abort "
          f"report, got termination_reason {preempt_reason!r}"
      )
      print(f"pull-canary preempt: exec {preempt_exec} closed uncharged as "
            f"{preempt_class}/{preempt_reason} (synthesized verdict or abort "
            f"report; never a charge, never a second row)")

      # The derivation is not lost: the same client build completes on a
      # fresh attempt, and the requeue itself added no further row.
      preempt_out = pc_bg_wait("pc-preempt", 300)
      assert "/nix/store/" in preempt_out, (
          f"the preempted drv should still produce a store path, got: {preempt_out!r}"
      )
      assert pc_attempt_rows("pc-preempt") == 1, (
          f"the requeue after preemption must add nothing beyond the uncharged "
          f"close, got {pc_attempt_rows('pc-preempt')} rows"
      )
      print(f"pull-canary TIMING preempt: DisruptionTarget -> pod+job gone, "
            f"attempt closed in {preempt_elapsed:.1f}s (bound 90s); rebuilt to a "
            f"delivered store path")

  # ══════════════════════════════════════════════════════════════════
  # Arm 5 — establishment window: an unreported pod death is charged
  # exactly once, and only after deadline + report-slack.
  # ══════════════════════════════════════════════════════════════════
  with subtest("pull-canary: unreported pod death is established only after deadline+slack"):
      pc_bg_build("${pcEstabDrv}", "pc-estab")
      pc_wait_open("pc-estab", 1, 300, "establishment arm")
      estab_pod, estab_node, estab_vm = pc_running_pod()
      estab_job = pc_owning_job(estab_pod)
      estab_exec = pc_open_exec("pc-estab")
      pc_wait_build_cgroup(estab_vm, "pc-estab")
      # The solved intent deadline (floored at the overlay's 180s probe
      # deadline) is what activeDeadlineSeconds renders from and what
      # the sweep adds the report slack to.
      job_deadline = int(k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} get job {estab_job} "
          "-o jsonpath='{.spec.activeDeadlineSeconds}'"
      ).strip() or "0")
      assert job_deadline >= 180, (
          f"the overlay pins the probe deadline to 180s, but the Job renders "
          f"activeDeadlineSeconds={job_deadline}"
      )

      # SIGKILL the builder process from the host (crictl -> host PID).
      # SIGKILL cannot be caught, so no SIGTERM-abort report fires; the
      # pod becomes a plain Error pod the controller does not classify;
      # the establishment sweep is the only closer.
      cid = estab_vm.succeed(
          f"k3s crictl ps -q --label io.kubernetes.pod.name={estab_pod} | head -1"
      ).strip()
      assert cid, f"no running container found for {estab_pod}"
      builder_pid = estab_vm.succeed(
          f"k3s crictl inspect {cid} | ${pkgs.jq}/bin/jq -r .info.pid"
      ).strip()
      assert builder_pid and builder_pid != "0", (
          f"crictl inspect returned a bad pid: {builder_pid!r}"
      )
      estab_vm.succeed(f"kill -9 {builder_pid}")
      print(f"pull-canary establishment: SIGKILLed builder pid {builder_pid} of "
            f"{estab_pod} on {estab_node} (job deadline {job_deadline}s, "
            f"slack {PC_SLACK_SECS}s)")
      k3s_server.wait_until_succeeds(
          f"phase=$(k3s kubectl -n ${nsBuilders} get pod {estab_pod} "
          "-o jsonpath='{.status.phase}' 2>/dev/null); "
          "test -z \"$phase\" || test \"$phase\" = Failed || test \"$phase\" = Succeeded",
          timeout=120,
      )

      def estab_age():
          out = psql_k8s(k3s_server,
              "SELECT EXTRACT(EPOCH FROM (now() - a.assigned_at)) "
              f"FROM assignments a WHERE a.exec_id = '{estab_exec}'"
          ).strip()
          assert out, f"assignment row for {estab_exec} disappeared"
          return float(out)

      # In-window probe: the window is the solved deadline plus the
      # 120s slack, so any attempt age below the slack alone is
      # provably inside it. The attempt must still be open and
      # uncharged there — the sweep must never fire inside the window.
      while estab_age() < 100:
          assert pc_open("pc-estab") == 1, (
              "the unreported death must stay an open attempt inside the window"
          )
          assert pc_attempt_rows("pc-estab") == 0, (
              "no charge may land inside the establishment window"
          )
          time.sleep(10)
      print(f"pull-canary establishment: still open and uncharged at attempt age "
            f"{estab_age():.0f}s (inside the window)")

      # Wait out the window (+ sweep cadence + headroom) for the charge.
      estab_budget = job_deadline + PC_SLACK_SECS + 150
      wait_deadline = time.time() + estab_budget
      while time.time() < wait_deadline:
          if pc_attempt_rows("pc-estab") >= 1:
              break
          time.sleep(10)
      estab_facts, _ = pc_charge_facts("pc-estab")
      e_class, e_reason, e_party = estab_facts[0], estab_facts[1], estab_facts[2]
      e_node, e_exec_col = estab_facts[4], estab_facts[7]
      assert e_class == "executor_crash" and e_reason == "unreported" and e_party == "scheduler", (
          f"the establishment charge must be executor_crash/unreported by the "
          f"scheduler, got: {estab_facts!r}"
      )
      assert e_exec_col == estab_exec, (
          f"the establishment charge must land on the killed exec {estab_exec}, "
          f"got {e_exec_col}"
      )
      estab_charge_age = float(psql_k8s(k3s_server,
          "SELECT EXTRACT(EPOCH FROM (t.occurred_at - a.assigned_at)) "
          "FROM drv_attempts t JOIN assignments a ON a.exec_id = t.exec_id "
          f"WHERE t.exec_id = '{estab_exec}'"
      ).strip())
      assert estab_charge_age >= PC_SLACK_SECS, (
          f"the establishment fired at attempt age {estab_charge_age:.0f}s, "
          f"inside the report slack ({PC_SLACK_SECS}s) — it must only fire after "
          f"deadline + slack"
      )
      # Structural form: the ESTABLISHED exec must have left the open
      # view (its terminal fill removes it). The requeued drv may
      # already have been re-pulled by a fresh pod by the time this
      # runs — that fresh attempt is the healthy respawn the arm later
      # relies on, not a leftover of the established one — so the
      # assertion keys on the established exec rather than demanding
      # an empty view for the drv.
      assert pc_open_exec("pc-estab") != estab_exec, (
          "the established attempt must have left the open-attempt view"
      )
      print(f"pull-canary TIMING establishment: charge landed at attempt age "
            f"{estab_charge_age:.0f}s (solved deadline {job_deadline}s + "
            f"slack {PC_SLACK_SECS}s; source_node on the charge: {e_node!r})")

      # The derivation is requeued, rebuilt under a fresh exec, and the
      # ledger keeps exactly the one establishment charge.
      estab_out = pc_bg_wait("pc-estab", 420)
      assert "/nix/store/" in estab_out, (
          f"the established drv should still produce a store path, got: {estab_out!r}"
      )
      assert pc_attempt_rows("pc-estab") == 1, (
          f"the rebuild after establishment must add no charge, got "
          f"{pc_attempt_rows('pc-estab')} rows"
      )
      print("pull-canary establishment PASS: charged exactly once, only after the "
            "window, and the work was not lost")

  # ══════════════════════════════════════════════════════════════════
  # Cleanup
  # ══════════════════════════════════════════════════════════════════
  with subtest("pull-canary: cleanup"):
      kubectl("delete pool pull-canary --wait=false", ns="${nsBuilders}")
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get pool pull-canary 2>/dev/null",
          timeout=30,
      )
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get pods "
          "-l rio.build/pool=pull-canary "
          "--no-headers 2>/dev/null | grep -q .",
          timeout=120,
      )
      print("pull-canary PASS: equivalence + cancel/preempt timing + establishment "
            "window all asserted")
''
