# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # manifest-pool — BuilderPool.spec.sizing=Manifest → per-bucket Jobs
  # ══════════════════════════════════════════════════════════════════
  # REQUIRES: no other workers alive (run AFTER ephemeral-pool, which
  # deletes both its own pool and the default x86-64 BuilderPoolSet). Same
  # reason as ephemeral-pool: another pool's reconciler picks up the
  # dispatch before reconcile_manifest's 10s tick spawns a Job,
  # leaving queued=0 → cold_start=0 → no spawn.
  #
  # Proves end-to-end what manifest_tests.rs CAN'T (pure-function
  # only — no apiserver):
  #   - apply() branches on spec.sizing=Manifest — dispatches to
  #     reconcile_manifest (no StatefulSet path)
  #   - reconcile_manifest polls GetCapacityManifest + ClusterStatus,
  #     computes cold_start deficit, spawns a Job (manifest.rs:
  #     298 — jobs_api.create succeeds, not the dark error path)
  #   - status_patch runs (manifest.rs:473) — rules out the :309→
  #     early-return dark path where non-409 create error returns
  #     BEFORE status is patched, leaving .status.replicas stale
  #   - Job carries rio.build/sizing=manifest + rio.build/
  #     {memory,cpu}-class labels (the inventory round-trip
  #     boundary)
  #   - Delete BuilderPool → ownerRef cascade GCs Jobs
  #
  # Cold-start floor path (not per-bucket diff): manifestDrv has no
  # build_history entry (never built before), so GetCapacityManifest
  # returns estimates=[] for it (scheduler/actor/mod.rs:1225 —
  # lookup_entry returns None → skip). reconcile_manifest sees
  # queued_total=1, estimates.len()=0 → cold_start=1 → spawns ONE
  # floor Job with memory-class=floor, cpu-class=floor. The per-
  # bucket diff logic is idempotency-proven by 18 unit tests; this
  # VM test covers the K8s I/O wiring shared by both paths.
  #
  # NOT proven here: scale-down (SCALE_DOWN_WINDOW even at the
  # autoscale fixture's 10s requires pod to heartbeat + report
  # busy=false via ListExecutors — full round-trip, follow-
  # on if this times in under ~150s). Failed-Job sweep (P0511) is
  # also follow-on (needs deliberate crash injection).
  with subtest("manifest-pool: sizing=Manifest, cold-start Job, status_patch"):
      # Precondition: no workers. ephemeral-pool's cleanup waits
      # ALL pool=ephemeral pods gone (not just CR gone — see that
      # fragment's comment re: SIGTERM-reconnect bounce, GHA run
      # 24012511360) and deleted the default x86-64 BuilderPoolSet.
      # wait_workers_zero (see prelude) bounds the
      # scheduler's view by the heartbeat-timeout fallback —
      # under GHA load the stream-EOF can be lost, falling back
      # to ~60s heartbeat-reap (GHA 24046508263).
      wait_workers_zero("manifest-pool precondition")

      # Apply manifest BuilderPool. Spec mirrors ephemeral-pool's
      # inline YAML (image:dev, tlsSecretName, privileged) except:
      # sizing=Manifest (not ephemeral:true), replicas.max=3.
      # replicas.min=0 is not CEL-enforced for Manifest (only for
      # ephemeral) but makes sense: no standing set, Job-based.
      k3s_server.succeed(
          "k3s kubectl apply -f - <<'EOF'\n"
          "apiVersion: rio.build/v1alpha1\n"
          "kind: BuilderPool\n"
          "metadata:\n"
          "  name: manifest\n"
          "  namespace: ${nsBuilders}\n"
          "spec:\n"
          "  sizing: Manifest\n"
          "  maxConcurrent: 3\n"
          "  systems: [x86_64-linux]\n"
          # Same rio-builder:dev tag + tlsSecretName rationale as
          # ephemeral-pool (see that fragment's comments).
          "  image: rio-builder:dev\n"
          "  imagePullPolicy: Never\n"
          "  tlsSecretName: rio-builder-tls\n"
          "  privileged: true\n"
          "  terminationGracePeriodSeconds: 60\n"
          "  nodeSelector: null\n"
          "  tolerations: null\n"
          "EOF"
      )

      # ── No StatefulSet ────────────────────────────────────────────
      # Regression guard: the reconciler creates Jobs only. One
      # reconcile tick (~3s with kube-runtime's fast CRD watch), then
      # assert. STS would be named `manifest-workers`.
      import time
      time.sleep(3)
      k3s_server.fail(
          "k3s kubectl -n ${nsBuilders} get sts manifest-workers 2>/dev/null"
      )
      k3s_server.fail(
          "k3s kubectl -n ${nsBuilders} get svc manifest-workers 2>/dev/null"
      )

      # ── Queue a derivation → cold_start deficit → Job spawn ───────
      # Background: foreground would block before we observe the Job.
      # manifestDrv is never-built-before → no build_history → the
      # scheduler's estimator.lookup_entry returns None → manifest
      # omits it → cold_start = queued_total - 0 = 1.
      client.succeed(
          "nix-build --no-out-link --store 'ssh-ng://k3s-server' "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          "${manifestDrv} > /tmp/mf1.out 2>&1 & "
          "echo $! > /tmp/mf1.pid"
      )

      # Job appears within: nix-build handshake (~5s) + queue +
      # reconcile_manifest tick (10s requeue) + jobs_api.create
      # (~1s). 45s margin. Label selector: sizing=manifest is set
      # by build_manifest_job (manifest.rs:1012); pool=manifest
      # from builders::labels(). Both together = THIS pool's
      # manifest Jobs, not stale ephemeral Jobs.
      k3s_server.wait_until_succeeds(
          "test -n \"$(k3s kubectl -n ${nsBuilders} get jobs "
          "-l rio.build/pool=manifest,rio.build/sizing=manifest "
          "-o name)\"",
          timeout=45,
      )
      mfjob = k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} get jobs "
          "-l rio.build/pool=manifest,rio.build/sizing=manifest "
          "-o jsonpath='{.items[0].metadata.name}'"
      ).strip()
      print(f"manifest: cold-start spawned Job {mfjob}")

      # ── r[ctrl.pool.manifest-labels]: class labels present ────────
      # bucket_labels(None) → ("floor","floor") for cold-start
      # (manifest.rs:566). The LABEL is what the inventory reads
      # back — presence proves the round-trip boundary is wired.
      # Value check: floor, not empty. A missing label → inventory
      # misses the Job → double-spawn next tick.
      mem_class = k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} get job {mfjob} "
          "-o jsonpath='{.metadata.labels.rio\\.build/memory-class}'"
      ).strip()
      cpu_class = k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} get job {mfjob} "
          "-o jsonpath='{.metadata.labels.rio\\.build/cpu-class}'"
      ).strip()
      assert mem_class == "floor", (
          f"cold-start Job should carry rio.build/memory-class=floor "
          f"(bucket_labels(None) sentinel); got {mem_class!r}. Missing "
          f"or wrong label → inventory_by_bucket misses this Job → "
          f"perpetual over-spawn."
      )
      assert cpu_class == "floor", (
          f"cold-start Job should carry rio.build/cpu-class=floor; "
          f"got {cpu_class!r}."
      )
      print(f"manifest-pool labels: memory-class={mem_class} "
            f"cpu-class={cpu_class} ✓")

      # ── r[ctrl.pool.manifest-reconcile]: status_patch ran ─────────
      # .status.replicas = active_total (manifest.rs:466) where
      # active_total is listed BEFORE spawn. Tick 1: list finds 0
      # Jobs, spawns 1, patches replicas=0. Tick 2 (10s later):
      # list finds 1 Job, patches replicas=1. This ASSERTS the
      # status_patch at :473 ran — ruling out the dark path where
      # jobs_api.create non-409 error at :309 returns BEFORE
      # status_patch, leaving the operator's `kubectl get bp` stale.
      # 60s: two 10s reconcile ticks + apiserver RTT + margin.
      k3s_server.wait_until_succeeds(
          "r=$(k3s kubectl -n ${nsBuilders} get builderpool manifest "
          "-o jsonpath='{.status.replicas}') && test -n \"$r\" && "
          "test \"$r\" -ge 1",
          timeout=60,
      )
      replicas = k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} get builderpool manifest "
          "-o jsonpath='{.status.replicas}'"
      ).strip()
      print(f"manifest-pool status: .status.replicas={replicas} "
            f"(status_patch ran, not the early-return dark path) ✓")

      # ── Build completes (manifest pod accepted + built it) ────────
      # 120s: same budget as ephemeral (pod schedule + FUSE + heart-
      # beat + dispatch + mkTrivial ~1s + CompletionReport). The
      # manifest pod does NOT exit after — it loops back to idle.
      client.wait_until_succeeds(
          "! kill -0 $(cat /tmp/mf1.pid) 2>/dev/null",
          timeout=180,
      )
      out_mf = client.succeed("cat /tmp/mf1.out").strip()
      assert "/nix/store/" in out_mf, (
          f"manifest build should have produced a store path, got: "
          f"{out_mf!r}"
      )

      # ── Runaway-spawn guard ───────────────────────────────────────
      # cold_start subtracts cold_start_supply (is_floor_job count).
      # One queued derivation → one floor Job, not one per tick.
      # Bound ≤2: same status-None race slop as ephemeral-pool.
      # A regression where compute_spawn_plan ignores supply would
      # spawn to ceiling (replicas.max=3) within 3 ticks.
      job_count = int(k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} get jobs "
          "-l rio.build/pool=manifest,rio.build/sizing=manifest "
          "-o name | wc -l"
      ).strip())
      assert job_count <= 2, (
          f"manifest Job count {job_count} > 2 for one queued "
          f"derivation — compute_spawn_plan should subtract "
          f"cold_start_supply from cold_start_demand. Regression "
          f"would spawn to replicas.max=3."
      )
      print(f"manifest-pool: job_count={job_count} ≤ 2 "
            f"(cold_start subtracts supply) ✓")

      # ── Cleanup: ownerRef cascade GCs Jobs ────────────────────────
      # Delete the BuilderPool. cleanup() branches on sizing==
      # Manifest (mod.rs:593) same as ephemeral — no STS scale-to-0.
      # ownerRef (set at build_manifest_job, manifest.rs:1034)
      # cascades: K8s GC deletes the Jobs once the CR is gone.
      kubectl("delete builderpool manifest --wait=false", ns="${nsBuilders}")
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get builderpool manifest 2>/dev/null",
          timeout=30,
      )
      # Job GC: ownerRef cascade. Not instant (K8s GC controller
      # tick), but proves the ownerRef is correct. 60s margin.
      k3s_server.wait_until_succeeds(
          "test -z \"$(k3s kubectl -n ${nsBuilders} get jobs "
          "-l rio.build/pool=manifest -o name 2>/dev/null)\"",
          timeout=60,
      )
      # ALL manifest-pool pods gone. Job-gone above does NOT imply
      # pods-gone (background-propagation delete; 180s grace).
      # Nothing currently chains after this subtest, but every
      # pool-deleting subtest waits its own pods gone so any future
      # subtest appended to the chain inherits a clean
      # workers_active==0 precondition without re-discovering the
      # d82a0046/24012511360 mechanism. 300s: 180s grace + margin.
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get pods "
          "-l rio.build/pool=manifest "
          "--no-headers 2>/dev/null | grep -q .",
          timeout=300,
      )
      print("manifest-pool PASS: sizing=Manifest → cold-start Job, "
            "labels present, status patched, ownerRef cascade cleanup")
''
