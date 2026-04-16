# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # ephemeral-pool — ephemeral BuilderPool → Job/build
  # ══════════════════════════════════════════════════════════════════
  # REQUIRES: no other workers alive. Subtest deletes the default
  # x86-64 Pool first so it's reconciler
  # doesn't steal dispatch.
  #
  # Proves end-to-end:
  #   - reconciler polls ClusterStatus + spawns Jobs when queued > 0
  #   - worker exits after one build → Job Complete →
  #     ttlSecondsAfterFinished reaps
  #   - second build → fresh Job (zero cross-build state)
  #
  # NOT proven here: the actual isolation property (tenant A can't
  # poison tenant B's cache). That'd need two tenants building
  # overlapping closures with one malicious. The "fresh pod = fresh
  # emptyDir" property is structural — K8s guarantees it.
  with subtest("ephemeral-pool: Job spawned, pod reaped, second build = new Job"):
      # Precondition: no other BuilderPool may serve this subtest's
      # builds. Delete the default `x86-64` BuilderPoolSet here (which
      # cascades to its `x86-64-tiny` child), otherwise that pool's
      # ephemeral reconciler ALSO sees queued>0 and spawns — build 2
      # may dispatch there, leaving no `rio.build/pool=ephemeral` Job.
      kubectl(
          "delete pool x86-64 --ignore-not-found --wait=true",
          ns="${nsBuilders}",
      )
      # wait_workers_zero (see prelude) bounds the scheduler's view
      # by the heartbeat-timeout fallback — I-195 closed the
      # coverage-mode SIGTERM-reconnect re-register but also removed
      # the second-chance stream-EOF; under GHA load the single EOF
      # can be lost, falling back to ~60s heartbeat-reap.
      wait_workers_zero("ephemeral-pool precondition")

      # ── CEL: hostNetwork without privileged rejected ──────────────
      # ctrl.crd.host-users-network-exclusive — K8s rejects
      # hostUsers:false with hostNetwork:true at admission; the
      # non-privileged path sets hostUsers:false (ADR-012).
      assert_cel_rejects(
          "hostnet-unprivileged",
          "  hostNetwork: true\n"
          "  maxConcurrent: 4\n"
          "  systems: [x86_64-linux]\n"
          "  image: rio-builder",
          "hostNetwork:true requires privileged:true",
      )

      # Apply ephemeral BuilderPool. Spec mirrors vmtest-full.yaml's
      # default pool (image, privileged, resources, grace) except:
      # ephemeral=true, replicas.min=0 (CEL enforced), max=4.
      # Heredoc via stdin: kubectl apply -f - with EOF. The YAML
      # is inline so the test doc is self-contained (no external
      # fixture file to drift).
      k3s_server.succeed(
          "k3s kubectl apply -f - <<'EOF'\n"
          "apiVersion: rio.build/v1alpha1\n"
          "kind: BuilderPool\n"
          "metadata:\n"
          "  name: ephemeral\n"
          "  namespace: ${nsBuilders}\n"
          "spec:\n"
          "  maxConcurrent: 4\n"
          "  systems: [x86_64-linux]\n"
          # rio-builder:dev — MUST match the ref from nix/docker.nix
          # vmTestSeed (tag = "dev"). Bare "rio-builder" normalizes
          # to :latest which the airgap import doesn't have →
          # ErrImageNeverPull. The Helm-rendered default pool gets
          # this via the rio.image helper (global.image.tag); inline
          # YAML here must spell it out.
          "  image: rio-builder:dev\n"
          "  imagePullPolicy: Never\n"
          "  privileged: true\n"
          "  terminationGracePeriodSeconds: 60\n"
          "  nodeSelector: null\n"
          "  tolerations: null\n"
          "EOF"
      )

      # ── No StatefulSet ────────────────────────────────────────────
      # Regression guard: the reconciler creates Jobs only. Give it
      # one reconcile tick (CRD watch fires immediately on create),
      # then assert no StatefulSet exists for this pool.
      import time
      time.sleep(3)  # one reconcile tick (kube-runtime is fast)
      k3s_server.fail(
          "k3s kubectl -n ${nsBuilders} get sts ephemeral-workers 2>/dev/null"
      )
      # Headless Service also skipped.
      k3s_server.fail(
          "k3s kubectl -n ${nsBuilders} get svc ephemeral-workers 2>/dev/null"
      )

      # ── Status patched by reconcile_ephemeral ─────────────────────
      # desiredReplicas = spec.replicas.max (the concurrent-Job
      # ceiling). reconcile_ephemeral runs on first apply even with
      # queued=0 — it patches status then requeues at 10s.
      k3s_server.wait_until_succeeds(
          "test \"$(k3s kubectl -n ${nsBuilders} get builderpool ephemeral "
          "-o jsonpath='{.status.desiredReplicas}')\" = 4",
          timeout=180,
      )

      # ── Build 1: Job spawned, completes, pod reaped ───────────────
      # Submit via the backgrounded nix-build pattern (same as
      # recoverySlowDrv) — foreground would block before we can
      # observe the Job. Background, assert Job appears, wait for
      # nix-build exit.
      client.succeed(
          "nix-build --no-out-link --store 'ssh-ng://k3s-server' "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          "${ephemeralDrv1} > /tmp/eph1.out 2>&1 & "
          "echo $! > /tmp/eph1.pid"
      )

      # Job appears within: nix-build handshake (~5s) + queue +
      # reconcile_ephemeral tick (10s) + K8s create (~1s). 45s
      # margin. The Job label `rio.build/pool=ephemeral` comes
      # from builders::labels() — same label cleanup() lists by.
      k3s_server.wait_until_succeeds(
          "test -n \"$(k3s kubectl -n ${nsBuilders} get jobs "
          "-l rio.build/pool=ephemeral -o name)\"",
          timeout=45,
      )
      job1 = k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} get jobs "
          "-l rio.build/pool=ephemeral "
          "-o jsonpath='{.items[0].metadata.name}'"
      ).strip()
      print(f"ephemeral: build 1 spawned Job {job1}")

      # nix-build completes. 120s: Job pod schedule (~5s) + container
      # pull from local registry (~2s, image already loaded) + FUSE
      # mount + cgroup (~5s) + heartbeat accepted (~10s tick) +
      # dispatch + build (mkTrivial ~1s) + CompletionReport + worker
      # exit. ~30-40s typical; 120s margin.
      client.wait_until_succeeds(
          "! kill -0 $(cat /tmp/eph1.pid) 2>/dev/null",
          timeout=180,
      )
      out1 = client.succeed("cat /tmp/eph1.out").strip()
      assert "/nix/store/" in out1, (
          f"build 1 should have produced a store path, got: {out1!r}"
      )

      # Pod goes Succeeded (worker exited 0 after its one build).
      # Filter by job-name label: .items[0] on all pods would pick
      # whichever sorts first (possibly a newer Job's pod if the
      # reconciler spawned another). Filter to job1's pod specifically.
      # TTL race: ttlSecondsAfterFinished=60 may have reaped job1
      # (and its pod) by the time we check — the preceding out1
      # store-path assertion already proves the build succeeded, so
      # accept "pod gone" (empty phase) as success here.
      k3s_server.wait_until_succeeds(
          "p=\"$(k3s kubectl -n ${nsBuilders} get pods "
          f"-l job-name={job1} "
          "-o jsonpath='{.items[0].status.phase}' 2>/dev/null)\"; "
          "test \"$p\" = Succeeded -o -z \"$p\"",
          timeout=180,
      )

      # ── Runaway-spawn guard ───────────────────────────────────────
      # spawn_count (ephemeral.rs) subtracts active Jobs from queued
      # — one queued derivation spawns ONE Job, not one per 10s tick
      # until ceiling. Bound ≤2: 1 for the build + 1 slop for a
      # status-patch-then-list race (a fresh Job may briefly show
      # status=None before the Job controller populates it). Pre-fix
      # this hit 4 (ceiling) under KVM-speed; a regression to the
      # old queued.min(headroom) formula would trip this assertion.
      job_count = int(k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} get jobs "
          "-l rio.build/pool=ephemeral -o name | wc -l"
      ).strip())
      assert job_count <= 2, (
          f"ephemeral Job count {job_count} > 2 for a single queued "
          f"derivation — spawn_count should subtract active Jobs "
          f"(queued.saturating_sub(active)), not spawn every tick. "
          f"Pre-fix this hit replicas.max=4."
      )
      print(f"ephemeral-pool: job_count={job_count} ≤ 2 (spawn_count subtracts active)")

      # ── Build 2: fresh Job (not reusing build 1's pod) ────────────
      # ASSERTION is name-based, not count-based: a Job whose name
      # ≠ job1 must exist after build 2. Count-based (the prior
      # `jobs_after >= jobs_before` form) is brittle two ways:
      #
      #   1. I-183 reap_excess_pending can delete job1 BETWEEN
      #      build 1 completing and this point — if k3s's Job
      #      controller lags syncing `status.ready=1` past the 10s
      #      REAP_PENDING_GRACE, job1 looks Pending at queued=0 →
      #      reaped. Then jobs_before=0 → comparison vacuous.
      #      Hit locally 1/2 at 7307d0f6 (job_count=0 right after
      #      pod phase=Succeeded — Job gone, pod lingering via
      #      background-propagation delete).
      #   2. ttlSecondsAfterFinished=JOB_TTL_SECS (600s, was 60s
      #      when this comment was first written) — not a factor
      #      at current TTL, but count-based is fragile to it.
      #
      # job1's spawn is already proven above (captured by name);
      # no precondition self-assert needed. Foreground build —
      # we don't need to observe mid-flight state.
      out2 = build("${ephemeralDrv2}")
      assert "/nix/store/" in out2, (
          f"build 2 should have produced a store path, got: {out2!r}"
      )

      job_names = k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} get jobs "
          "-l rio.build/pool=ephemeral "
          "-o jsonpath='{.items[*].metadata.name}'"
      ).split()
      # build 2 just finished → its Job is < REAP_PENDING_GRACE old
      # AND < JOB_TTL_SECS since completion → present regardless of
      # whether job1 was reaped.
      assert any(n != job1 for n in job_names), (
          f"expected a Job ≠ {job1!r} after build 2 (fresh ephemeral "
          f"Job per build), got {job_names!r}. If build 2 produced "
          f"output without a new Job, it was served by a REUSED pod "
          f"— ephemeral mode is broken."
      )

      # ── Cleanup ───────────────────────────────────────────────────
      # Delete the pool. cleanup() removes the finalizer
      # immediately; in-flight Jobs finish naturally and
      # ownerRef GC deletes them.
      kubectl("delete builderpool ephemeral --wait=false", ns="${nsBuilders}")
      # CR gone quickly — finalizer removed on first cleanup() call.
      # 30s is generous; should be <5s in practice.
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get builderpool ephemeral 2>/dev/null",
          timeout=30,
      )
      # ALL ephemeral-pool pods gone. CR-gone above does NOT imply
      # pods-gone: ownerRef cascade deletes the Jobs, Jobs delete
      # their pods, but each pod gets terminationGracePeriodSeconds
      # =180. reconcile_ephemeral may have spawned a second Job
      # (job_count ≤ 2 asserted above) whose pod is mid-connect
      # when SIGTERM lands → builder's `continue 'reconnect` loop
      # (main.rs:388) re-registers → workers_active bounces 0→1
      # for up to 180s. GHA run 24012511360: a follow-on subtest's
      # sched_metric_wait('workers_active 0', 90s) precondition
      # blew with an ephemeral pod still Terminating. Every subtest
      # that deletes a pool MUST wait its own pods gone before the
      # next subtest's workers_active==0 precondition is sound.
      #
      # 300s: 180s grace + ownerRef-GC controller tick + margin.
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get pods "
          "-l rio.build/pool=ephemeral "
          "--no-headers 2>/dev/null | grep -q .",
          timeout=300,
      )
      print("ephemeral-pool PASS: no STS, Job spawned per build, "
            "pod Succeeded, fresh Job for build 2")
''
