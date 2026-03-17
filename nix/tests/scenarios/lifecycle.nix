# Lifecycle scenario: scheduler recovery, GC, autoscaler, finalizer,
# Build-CRD reconciler flow, health-shared NOT_SERVING probe — all
# exercised against the k3s-full fixture.
#
# Ports phase3b sections S (recovery), C (GC), F (watch-dedup), T
# (health-shared) + phase3a autoscaler/finalizer/SSA-field-ownership/
# Build-CRD events+finalizer onto the 2-node k3s Helm-chart fixture.
# Unlike phase3b (control/worker/k8s/client as separate systemd
# VMs), everything here runs as PODS — closes the "production uses pod
# path, VM tests use systemd" gap for the reconciler/lease/autoscaler
# surface.
#
#
# Fragment architecture: this file returns { fragments, mkTest } instead
# of a single runNixOSTest. default.nix composes fragments into 5 parallel
# VM tests (core, ctrlrestart, recovery, reconnect, autoscale) — critical
# path ~8min vs the prior ~14min monolith. Each fragment is a Python
# `with subtest(...)` block; mkTest concatenates a prelude + the selected
# fragments + coverage epilogue into a testScript.
# Key adaptation: scheduler pods are minimal images (no shell, no curl).
# Metric scrapes go through apiserver pods/proxy (`kubectl get --raw`);
# grpcurl (needs TCP for mTLS) through `kubectl port-forward`.
# Scheduler has 2 replicas (podAntiAffinity spreads them
# across server+agent), so killing the leader means the STANDBY takes
# over — a strictly stronger recovery test than phase3b's single-instance
# restart.
#
# r[verify ctrl.build.watch-by-uid]
#   build-crd-flow asserts rio_controller_build_watch_spawns_total == 1
#   across multiple reconcile cycles of ONE Build CR (UID-keyed DashMap),
#   PLUS full apply→status→events→finalizer chain (phase3a:606-705).
#
# r[verify obs.metric.controller]
#   autoscaler scrapes rio_controller_scaling_decisions_total{direction="up"}
#   — exact name from observability.md controller metrics table.
#
# Caller (default.nix) constructs the fixture with autoscaler tuning via
# controller.extraEnv (see end-of-file comment block for exact extraValues):
#
#   fixture = k3sFull {
#     extraValues = {
#       "controller.extraEnv[0].name"  = "RIO_AUTOSCALER_POLL_SECS";
#       "controller.extraEnv[0].value" = "3";
#       "controller.extraEnv[1].name"  = "RIO_AUTOSCALER_SCALE_UP_WINDOW_SECS";
#       "controller.extraEnv[1].value" = "3";
#       "controller.extraEnv[2].name"  = "RIO_AUTOSCALER_MIN_INTERVAL_SECS";
#       "controller.extraEnv[2].value" = "3";
#       "controller.extraEnv[3].name"  = "RIO_AUTOSCALER_SCALE_DOWN_WINDOW_SECS";
#       "controller.extraEnv[3].value" = "10";
#     };
#   };
#
# r[verify ctrl.build.reconnect]
#   build-crd-reconnect kills the scheduler leader mid-build and asserts
#   the controller's drain_stream reconnects on non-terminal EOF (Ok(None)
#   from tonic on TCP FIN). Same bug class as gateway 06a1fa0.
#
# r[verify ctrl.build.sentinel]
#   build-crd-flow asserts status.buildId != "submitted" (the sentinel)
#   after phase reaches terminal. Proves the reconciler's idempotence
#   gate at build.rs:111-140 completed the sentinel→real-UUID transition.
#
# r[verify ctrl.probe.named-service]
#   health-shared probes with `-service rio.scheduler.SchedulerService`
#   (the named service, NOT empty-string) and asserts NOT_SERVING on
#   standby. scheduler/main.rs:380-392: set_not_serving only affects
#   the named service; if the K8s readinessProbe probed "" instead,
#   standby would pass readiness.
#
# r[verify ctrl.autoscale.skip-deleting]
#   finalizer subtest deletes the WorkerPool and waits ~300s for pod
#   termination. The autoscaler's 30s poll fires DURING that window;
#   scaling.rs:222 deletion_timestamp.is_some() skip-gate MUST fire
#   or the autoscaler would race the finalizer's scale-to-0.
#
# r[verify worker.cancel.cgroup-kill]
#   cancel-cgroup-kill deletes a Build CR mid-exec and asserts the
#   "build cancelled via cgroup.kill" log line. cgroup.rs:180 kill()
#   writes "1" to cgroup.kill → kernel SIGKILLs the tree. No other
#   test cancels a RUNNING build (build-crd-flow lets it complete;
#   recovery/reconnect kill the scheduler, build keeps running).
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns pki;
  drvs = import ../lib/derivations.nix { inherit pkgs; };
  protoset = import ../lib/protoset.nix { inherit pkgs; };

  # grpcurl not in k3sBase systemPackages (only curl+kubectl). Use the
  # store path directly — it's pulled into the VM closure by interpolation.
  grpcurl = "${pkgs.grpcurl}/bin/grpcurl";

  # mTLS client cert args. The fixture's PKI generates per-component
  # certs; the controller cert works as a generic client (rio checks
  # CA-signed, not CN). SANs include `localhost` so grpcurl-via-port-
  # forward (connects to localhost:19001, SNI=localhost) verifies.
  grpcurlTls =
    "-cacert ${pki}/ca.crt "
    + "-cert ${pki}/rio-controller/tls.crt "
    + "-key ${pki}/rio-controller/tls.key";

  # ── Test derivations ────────────────────────────────────────────────
  # Distinct markers so each build creates a fresh derivations row —
  # otherwise DAG-dedup would reuse an earlier build's result and the
  # "not a cache hit" assertions would be hollow.

  # Pin target for GC-sweep. Built FIRST (before recovery) so it's been
  # in PG long enough that gc-sweep's backdate targets a DIFFERENT row.
  pinDrv = drvs.mkTrivial { marker = "lifecycle-pin"; };

  # In-flight build for recovery. 90s sleep survives the leader-kill
  # window: lease TTL (~15s worst case for standby to detect) + standby's
  # recovery query (~1s) + re-dispatch latency (~5s). phase3b rationale
  # (phase3b.nix:85-99) applies verbatim — a shorter sleep lets the build
  # finish during the failover gap → PG has 0 non-terminal rows →
  # recovery loads nothing → hollow test.
  recoverySlowDrv = drvs.mkTrivial {
    marker = "lifecycle-recovery-slow";
    sleepSecs = 90;
  };

  # Post-recovery build. DIFFERENT marker than pinDrv so this is NOT a
  # cache hit — proves dispatch actually unblocked after LeaderAcquired →
  # recover_from_pg → recovery_complete.store(true). Also becomes the
  # backdate target for gc-sweep (unpinned, so sweep can delete it).
  recoveryDrv = drvs.mkTrivial { marker = "lifecycle-recovery"; };

  # build-crd-reconnect in-flight build. 60s sleep survives the kill
  # window (same rationale as recoverySlowDrv: lease TTL ~15s + standby
  # recovery ~1s + controller's drain_stream backoff 1/2/4s + WatchBuild
  # reconnect). Shorter than recoverySlowDrv because the controller's
  # reconnect is simpler than full scheduler recovery.
  reconnectDrv = drvs.mkTrivial {
    marker = "lifecycle-reconnect";
    sleepSecs = 60;
  };

  # Build CRD watch-dedup. 5s sleep → multiple BuildEvent cycles
  # (Pending → Building → log lines → Succeeded). Each cycle patches
  # status → watch event → reconcile. Without dedup, each reconcile
  # spawns a duplicate drain_stream task.
  watchDedupDrv = drvs.mkTrivial {
    marker = "lifecycle-watchdedup";
    sleepSecs = 5;
  };

  # controller-restart in-flight build. 30s sleep: controller pod
  # graceful delete (~3s) + Deployment spawns replacement (~10s) +
  # new controller's informer sync (~2s) + reconcile fires. The build
  # runs on the WORKER the whole time — only the controller's watch
  # task is interrupted.
  ctrlRestartDrv = drvs.mkTrivial {
    marker = "lifecycle-ctrl-restart";
    sleepSecs = 30;
  };

  # cancel-cgroup-kill in-flight build. 60s sleep: wait-for-Building
  # + find cgroup + assert it has procs + delete CR + wait for cgroup
  # gone. Shorter than recoverySlowDrv because cancel is direct
  # (controller→scheduler→worker RPC, no lease transition).
  cancelDrv = drvs.mkTrivial {
    marker = "lifecycle-cancel";
    sleepSecs = 60;
  };

  # Autoscaler queue pressure: 5 leaves, all independent, all Ready
  # immediately. With maxConcurrentBuilds=1 (vmtest-full.yaml), 1 runs
  # + 4 queue → compute_desired(4, target=2) = ceil(4/2) = 2 → STS
  # replicas 1→2. sleep 15 × 5 ≈ 75s sequential (pod-1 may never come
  # Ready on the 4GB agent VM) — long enough for ~3 poll cycles (3s
  # each, via controller.extraEnv) to see sustained pressure.
  #
  # Same ''${...}/''' escaping dance as phase3a.nix:79-81: the inner
  # .nix file reads ITS OWN let-bound `busybox` arg, not this Nix
  # evaluation's scope.
  autoscaleDrvFile = pkgs.writeText "lifecycle-autoscale.nix" ''
    { busybox }:
    let
      sh = "''${busybox}/bin/sh";
      bb = "''${busybox}/bin/busybox";
      mk = n: derivation {
        name = "rio-lifecycle-queue-''${toString n}";
        system = builtins.currentSystem;
        builder = sh;
        args = [ "-c" '''
          ''${bb} mkdir -p $out
          ''${bb} echo "queue pressure ''${toString n}" > $out/stamp
          ''${bb} sleep 15
        ''' ];
      };
    in {
      d1 = mk 1;
      d2 = mk 2;
      d3 = mk 3;
      d4 = mk 4;
      d5 = mk 5;
    }
  '';

  # gc-sweep's backdate+delete target. In the monolith, gc-sweep reused
  # `out_recovery` from the recovery subtest — convenient but coupled.
  # Fragment architecture: gc-sweep builds its own victim.
  gcVictimDrv = drvs.mkTrivial { marker = "lifecycle-gc-victim"; };

  # ── testScript prelude: bootstrap + Python helpers ────────────────────
  # Shared by all fragment compositions. start_all + waitReady (~4min on
  # k3s-full) + kubectlHelpers + metric-scrape defs + sshKeySetup + seed.
  # Pyflakes doesn't warn on unused function DEFS (only imports/locals),
  # so sparse splits that don't call every helper are fine.
  prelude = ''
    ${common.assertions}

    start_all()
    ${fixture.waitReady}

    ${fixture.kubectlHelpers}

    # ── Metrics-scrape helpers ────────────────────────────────────────
    # Scheduler/controller/store pods are minimal images (no sh, no curl).
    # Scrape via the apiserver's pods/proxy subresource — `kubectl get
    # --raw /api/v1/.../pods/{pod}:metrics/proxy/metrics`. Apiserver
    # proxies HTTP to the pod via kubelet. No local port bind, no
    # TIME_WAIT churn, no `sleep 2`.
    #
    # Prior port-forward approach: each sched_metric_wait retry spawned
    # a fresh pf on port 19091. After a long wait (settle-wait took
    # 100s in v18 ≈ 30+ retries), the port was in heavy TIME_WAIT and
    # subsequent calls failed bind for 60s+.
    #
    # NUMERIC ports (9091/9094), not named (`:metrics`): k3s
    # apiserver PANICS (nil-deref in normalizeLocation,
    # upgradeaware.go:173) on named-port proxy. Observed v20.
    # Fresh leader_pod() lookup per scrape — the leader CHANGES
    # across recovery.

    def proxy_url(pod, port, path="metrics"):
        return (
            f"/api/v1/namespaces/${ns}/pods/{pod}:{port}/proxy/{path}"
        )

    def sched_metrics():
        """One-shot scrape of the CURRENT scheduler leader's /metrics."""
        raw = k3s_server.succeed(
            f"k3s kubectl get --raw '{proxy_url(leader_pod(), 9091)}'"
        )
        return parse_prometheus(raw)

    def ctrl_metrics():
        """One-shot scrape of the controller pod's /metrics (port 9094)."""
        pod = kubectl(
            "get pods -l app.kubernetes.io/name=rio-controller "
            "-o jsonpath='{.items[0].metadata.name}'"
        ).strip()
        raw = k3s_server.succeed(
            f"k3s kubectl get --raw '{proxy_url(pod, 9094)}'"
        )
        return parse_prometheus(raw)

    # Shell-inline version for wait_until_succeeds (condition must be
    # shell-evaluable). Single kubectl call per retry — no background
    # process, no cleanup, no port. Retry rate is now limited only by
    # the NixOS test driver's poll interval (~1s) + apiserver RTT.
    def sched_metric_wait(condition, timeout=60):
        """Wait until the leader's /metrics satisfies a bash condition.
        `condition` is a pipe-fragment appended after `... | `."""
        k3s_server.wait_until_succeeds(
            "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
            "  -o jsonpath='{.spec.holderIdentity}') && "
            'test -n "$leader" && '
            "k3s kubectl get --raw "
            '"/api/v1/namespaces/${ns}/pods/$leader:9091/proxy/metrics" '
            f"| {condition}",
            timeout=timeout,
        )

    # grpcurl against the scheduler's gRPC port (9001) and store (9002).
    # mTLS (tls.enabled=true in vmtest-full.yaml) → present the
    # controller client cert from the fixture's PKI. port-forward is a
    # raw TCP tunnel through the apiserver — grpcurl does the TLS
    # handshake end-to-end with the pod. SNI=localhost (from grpcurl's
    # :authority derivation on -addr localhost); the server cert has
    # localhost in its SANs. `-max-time` bounds the RPC itself;
    # port-forward is killed by trap even if grpcurl hangs.
    def sched_grpc(payload, method):
        """TriggerGC etc. on the scheduler leader. Returns stdout+stderr."""
        leader = leader_pod()
        return k3s_server.succeed(
            f"k3s kubectl -n ${ns} port-forward {leader} 19001:9001 "
            f">/dev/null 2>&1 & pf=$!; "
            f"trap 'kill $pf 2>/dev/null' EXIT; sleep 2; "
            f"${grpcurl} ${grpcurlTls} -max-time 30 "
            f"-protoset ${protoset}/rio.protoset "
            f"-d '{payload}' localhost:19001 {method} 2>&1"
        )

    def store_grpc(payload, method):
        """PinPath/UnpinPath on the store. deploy/ port-forward picks any
        replica (there's only one). Returns stdout+stderr."""
        return k3s_server.succeed(
            f"k3s kubectl -n ${ns} port-forward deploy/rio-store 19002:9002 "
            f">/dev/null 2>&1 & pf=$!; "
            f"trap 'kill $pf 2>/dev/null' EXIT; sleep 2; "
            f"${grpcurl} ${grpcurlTls} -max-time 30 "
            f"-protoset ${protoset}/rio.protoset "
            f"-d '{payload}' localhost:19002 {method} 2>&1"
        )

    # ── SSH + seed ────────────────────────────────────────────────────
    # fixture.sshKeySetup (NOT common.sshKeySetup): patches the
    # rio-gateway-ssh Secret + rollout-restarts the gateway Deployment.
    # The common.nix version writes /var/lib/rio/gateway/authorized_keys
    # on a systemd host — wrong for a pod.
    ${fixture.sshKeySetup}
    ${common.seedBusybox "k3s-server"}

    # ── Build helper ──────────────────────────────────────────────────
    # client's programs.ssh.extraConfig routes `Host k3s-server` →
    # port 32222, user rio (mkClientNode in common.nix:348-356). So
    # `ssh-ng://k3s-server` hits the gateway NodePort.
    def build(drv_file, capture_stderr=True):
        cmd = (
            f"nix-build --no-out-link --store 'ssh-ng://k3s-server' "
            f"--arg busybox '(builtins.storePath ${common.busybox})' "
            f"{drv_file}"
        )
        if capture_stderr:
            cmd += " 2>&1"
        try:
            return client.succeed(cmd)
        except Exception:
            dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")
            raise
  '';

  # ── Subtest fragments ─────────────────────────────────────────────────
  # Each fragment is a `with subtest(...)` block + its comment banner.
  # Fragments are composed by mkTest in the order given by `subtests`.
  # Python variables flow between fragments at module scope (no `with`
  # scoping) — but in the split architecture, all fragments are now
  # self-contained (gc-sweep builds its own paths; the old `initial`
  # seed-subtest is gone).
  fragments = {
    health-shared = ''
      # ══════════════════════════════════════════════════════════════════
      # health-shared — standby NOT_SERVING, leader SERVING on plaintext port
      # ══════════════════════════════════════════════════════════════════
      # Ports phase3b section T (phase3b.nix:342-360 + 506-520) onto the
      # k3s-full fixture. In phase3b the standby window was ARTIFICIAL
      # (scheduler in STANDBY because kubeconfig didn't exist yet). Here
      # the standby is REAL: scheduler.replicas=2, one pod holds the Lease,
      # the other is a live standby.
      #
      # Proves: the plaintext health port (9101) shares its HealthReporter
      # with the mTLS port via health_service.clone(). Standby's
      # set_not_serving on the NAMED service is visible on BOTH ports — if
      # the reporter weren't shared, the plaintext port would default to
      # SERVING regardless of lease state.
      #
      # MUST run BEFORE recovery: recovery deletes the leader pod, which
      # disrupts the stable 2-replica state (the former standby becomes
      # leader, and the Deployment-spawned replacement pod may briefly
      # show SERVING-then-NOT_SERVING churn during its own startup).
      with subtest("health-shared: standby NOT_SERVING, leader SERVING (shared HealthReporter)"):
          # app.kubernetes.io/name=rio-scheduler is the Deployment's pod
          # selector (templates/_helpers.tpl rio.selectorLabels).
          all_sched = kubectl(
              "get pods -l app.kubernetes.io/name=rio-scheduler "
              "-o jsonpath='{.items[*].metadata.name}'"
          ).split()
          assert len(all_sched) == 2, (
              f"expected exactly 2 scheduler pods (replicas=2), got: {all_sched}"
          )
          leader = leader_pod()
          standby = next(p for p in all_sched if p != leader)
          print(f"health-shared: leader={leader}, standby={standby}")

          # ── STANDBY: NOT_SERVING on named service ─────────────────────
          # grpc-health-probe exits 1 for NOT_SERVING (phase3b.nix:348).
          # .fail() expects non-zero exit AND returns stdout+stderr.
          # Probe the NAMED service (rio.scheduler.SchedulerService), NOT
          # the empty-string default. scheduler/main.rs:380-392 comment:
          # set_not_serving only affects the named service; empty-string
          # stays SERVING forever after the first set_serving. If the K8s
          # readinessProbe probed "" instead, standby would pass readiness
          # and K8s would route to it. This probe with `-service ...` AND
          # the NOT_SERVING result together prove the named-service gate.
          k3s_server.succeed(
              f"k3s kubectl -n ${ns} port-forward {standby} 19101:9101 "
              f">/dev/null 2>&1 & echo $! > /tmp/pf-health.pid; sleep 2"
          )
          try:
              out = k3s_server.fail(
                  "grpc-health-probe -addr localhost:19101 "
                  "-service rio.scheduler.SchedulerService 2>&1"
              )
              assert "NOT_SERVING" in out, (
                  f"standby plaintext health should report NOT_SERVING "
                  f"(shared HealthReporter, lease-gated), got: {out!r}"
              )
          finally:
              k3s_server.execute(
                  "kill $(cat /tmp/pf-health.pid) 2>/dev/null; rm -f /tmp/pf-health.pid"
              )

          # ── LEADER: SERVING on named service ──────────────────────────
          # Exit 0 = SERVING. Leader acquired lease during waitReady →
          # LeaderAcquired → recover_from_pg → recovery_complete=true →
          # set_serving on named service.
          k3s_server.succeed(
              f"k3s kubectl -n ${ns} port-forward {leader} 19101:9101 "
              f">/dev/null 2>&1 & echo $! > /tmp/pf-health.pid; sleep 2"
          )
          try:
              k3s_server.succeed(
                  "grpc-health-probe -addr localhost:19101 "
                  "-service rio.scheduler.SchedulerService"
              )
          finally:
              k3s_server.execute(
                  "kill $(cat /tmp/pf-health.pid) 2>/dev/null; rm -f /tmp/pf-health.pid"
              )
          print("health-shared PASS: standby NOT_SERVING, leader SERVING "
                "(plaintext port shares reporter with mTLS port)")
    '';

    build-crd-flow = ''
      # ══════════════════════════════════════════════════════════════════
      # build-crd-flow — full reconciler chain: apply → status → events →
      #                  finalizer + watch-dedup (one task per Build UID)
      # ══════════════════════════════════════════════════════════════════
      # Ports phase3a Build-CRD reconciler exercise (phase3a.nix:606-705)
      # onto the pod-based fixture. phase3a ran the controller as systemd
      # with admin kubeconfig (ns=default); here the controller is a pod
      # with SA-scoped RBAC (ns=rio-system). FIRST time the full chain runs
      # against a real apiserver from a pod.
      #
      # Runs EARLY (before recovery's queue churn) for a clean baseline:
      # rio_controller_build_watch_spawns_total is a monotonic counter,
      # and this is the ONLY Build CR created in the whole test. If the
      # dedup is broken, the 5s-sleep build's status transitions (Pending
      # → Building → Succeeded, plus log-line patches) each trigger a
      # reconcile → each spawns a duplicate drain_stream → counter ≥3.
      #
      # Asserts (in order):
      #   1. status.phase reaches terminal
      #   2. status.buildId is a real UUID (NOT sentinel "submitted")
      #   3. K8s Events ≥2 on the Build object, one reason=Submitted
      #   4. watch_spawns_total == EXACTLY 1 (UID-keyed DashMap dedup)
      #   5. delete → finalizer cleanup() → CR gone (not stuck Terminating)
      with subtest("build-crd-flow: apply → status → events → finalizer + watch-dedup"):
          # Instantiate + copy the .drv closure (not outputs) to the store.
          # The Build reconciler's fetch_and_build_node reads the .drv from
          # rio-store to construct its DAG node.
          drv_path = client.succeed(
              "nix-instantiate "
              "--arg busybox '(builtins.storePath ${common.busybox})' "
              "${watchDedupDrv} 2>/dev/null"
          ).strip()
          client.succeed(
              f"nix copy --derivation --to 'ssh-ng://k3s-server' {drv_path}"
          )

          # Apply Build CR. Namespace = rio-system (the controller pod's
          # SA/ClusterRole watches here; phase3b used `default` because
          # its controller ran as systemd with the admin kubeconfig).
          k3s_server.succeed(
              "k3s kubectl apply -f - <<'EOF'\n"
              "apiVersion: rio.build/v1alpha1\n"
              "kind: Build\n"
              "metadata:\n"
              "  name: test-watch-dedup\n"
              "  namespace: ${ns}\n"
              "spec:\n"
              f"  derivation: {drv_path}\n"
              "  priority: 10\n"
              "EOF"
          )

          # Baseline: did the controller's Build reconciler even wake up
          # yet? default-workers-0 Ready (waitReady) proves the WorkerPool
          # watch synced. Build watch started at the same join!(), same
          # API group — if WorkerPool synced, Build should have too.
          # Printing this lets the timeout diag below show the DELTA.
          cm_base = ctrl_metrics()
          br_base = metric_value(cm_base,
              "rio_controller_reconcile_duration_seconds_count",
              '{reconciler="build"}') or 0.0
          wp_base = metric_value(cm_base,
              "rio_controller_reconcile_duration_seconds_count",
              '{reconciler="workerpool"}') or 0.0
          print(f"build-crd-flow: pre-wait reconcile counts: "
                f"build={br_base}, workerpool={wp_base}")

          # Terminal phase: 5s sleep + dispatch overhead. 180s: the
          # controller's Build informer has variable cache-sync time
          # — if its first watch attempt fires before the CRD is
          # Established, kube-rs backs off. Observed: 48s (v15), >90s
          # (v16). 180s absorbs the backoff variance.
          try:
              k3s_server.wait_until_succeeds(
                  "k3s kubectl -n ${ns} get build test-watch-dedup "
                  "-o jsonpath='{.status.phase}' | "
                  "grep -E '^(Succeeded|Completed|Cached)$'",
                  timeout=180,
              )
          except Exception:
              # DIAGNOSTIC: 2/2 coverage-full failures today (2026-03-16)
              # timed out here. Dump everything — controller logs, Build CR
              # status, reconcile-count delta, scheduler/store connectivity.
              # The "informer backoff" theory is DOUBTFUL: default-workers-0
              # Ready proves WorkerPool synced; Build watch joins the same
              # tokio::join!. Need the real answer.
              k3s_server.execute(
                  "echo '=== DIAG: controller pod logs (last 100) ===' >&2; "
                  "k3s kubectl -n ${ns} logs deploy/rio-controller --tail=100 >&2; "
                  "echo '=== DIAG: controller pod logs --previous (crashes?) ===' >&2; "
                  "k3s kubectl -n ${ns} logs deploy/rio-controller --previous --tail=50 >&2 || true; "
                  "echo '=== DIAG: test-watch-dedup Build CR ===' >&2; "
                  "k3s kubectl -n ${ns} get build test-watch-dedup -o yaml >&2; "
                  "echo '=== DIAG: Build CR events ===' >&2; "
                  "k3s kubectl -n ${ns} get events "
                  "--field-selector involvedObject.name=test-watch-dedup >&2; "
                  "echo '=== DIAG: controller pod state ===' >&2; "
                  "k3s kubectl -n ${ns} get pods -l app.kubernetes.io/name=rio-controller -o wide >&2; "
                  "echo '=== DIAG: scheduler + store pods ===' >&2; "
                  "k3s kubectl -n ${ns} get pods "
                  "-l 'app.kubernetes.io/name in (rio-scheduler,rio-store)' -o wide >&2"
              )
              cm_diag = ctrl_metrics()
              br_diag = metric_value(cm_diag,
                  "rio_controller_reconcile_duration_seconds_count",
                  '{reconciler="build"}') or 0.0
              # reconcile_errors_total has TWO labels (reconciler + error_kind);
              # dump all series rather than guessing the error_kind.
              err_diag = cm_diag.get("rio_controller_reconcile_errors_total", {})
              spawns_diag = metric_value(cm_diag,
                  "rio_controller_build_watch_spawns_total") or 0.0
              print(f"DIAG: build reconcile count {br_base} → {br_diag} "
                    f"(delta={br_diag - br_base}); "
                    f"reconcile_errors={err_diag!r}; "
                    f"watch_spawns={spawns_diag}")
              # delta=0  → Build informer never delivered the CR (backoff? watch dead?)
              # delta>0 + errors>0 → reconcile fired but errored (store/scheduler down?)
              # delta>0 + spawns=0 → reconcile fired but never reached submit_build
              # delta>0 + spawns=1 → submitted but scheduler/worker never completed it
              raise

          # ── status.buildId: real UUID, not sentinel ───────────────────
          # If the sentinel patch ran AFTER the watch task's first patch
          # (patch-ordering race), build_id would be stuck at "submitted".
          # The reconciler's idempotence gate at build.rs:111-140 keys on
          # `build_id != "submitted"` to skip re-submit. This assert proves
          # the sentinel→real-UUID transition completed. UUID regex checks
          # 8-4-4-4-12 hex shape (phase3a.nix:663-672).
          build_id = kubectl(
              "get build test-watch-dedup -o jsonpath='{.status.buildId}'"
          ).strip()
          print(f"build-crd-flow: build_id = {build_id}")
          assert build_id != "" and build_id != "submitted", (
              f"build_id stuck at sentinel/empty (patch-ordering race?): {build_id!r}"
          )
          import re
          assert re.match(r'^[0-9a-f]{8}-[0-9a-f]{4}-', build_id), (
              f"build_id not UUID-like: {build_id!r}"
          )

          # ── K8s Events: ≥2, at least one reason=Submitted ─────────────
          # Controller's Recorder emits Submitted (from apply()) + terminal
          # event (Building + Succeeded, or Building + Cached). Field
          # selector filters to THIS Build object (phase3a.nix:680-695).
          events_json = kubectl(
              "get events "
              "--field-selector involvedObject.name=test-watch-dedup,involvedObject.kind=Build "
              "-o json"
          )
          events = json.loads(events_json).get("items", [])
          event_reasons = [e.get("reason", "") for e in events]
          print(f"build-crd-flow: events = {event_reasons}")
          assert len(events) >= 2, (
              f"expected >=2 K8s Events on test-watch-dedup "
              f"(Submitted + terminal), got {len(events)}: {event_reasons}"
          )
          assert "Submitted" in event_reasons, (
              f"expected Submitted event from apply(), got: {event_reasons}"
          )

          # THE ASSERTION: exactly 1 watch spawn. Not ≥1, not "present" —
          # the whole point is that reconcile #2 onward hit the
          # ctx.watching.contains_key gate and DIDN'T spawn again.
          cm = ctrl_metrics()
          spawns = metric_value(cm, "rio_controller_build_watch_spawns_total") or 0.0
          assert spawns == 1.0, (
              f"expected exactly 1 watch spawn (UID dedup), got {spawns}\n"
              f"  all build_watch series: {cm.get('rio_controller_build_watch_spawns_total', {})!r}"
          )

          # Reconnect-path dedup verified via the METRIC above (spawns==1).
          # The log-grep pattern previously here (`kubectl logs | grep -c`)
          # reads the whole pod history — a Deployment roll would give a
          # fresh pod with 0 logs → false pass; a reorder after
          # build-crd-reconnect would poison it to "1" → false fail.
          # The metric is the same signal, tighter assertion.

          # ── Finalizer: delete → cleanup() → CR gone ───────────────────
          # --wait=false: don't block kubectl on the finalizer. Then poll
          # for the CR to be GONE — proves cleanup() ran (CancelBuild,
          # NotFound OK since already terminal) → finalizer removed → K8s
          # deleted the CR. Without the wait_until_succeeds, a stuck
          # finalizer (Terminating forever) would pass silently
          # (phase3a.nix:700-704).
          kubectl("delete build test-watch-dedup --wait=false")
          k3s_server.wait_until_succeeds(
              "! k3s kubectl -n ${ns} get build test-watch-dedup 2>/dev/null",
              timeout=15,
          )
          print(f"build-crd-flow PASS: build_id={build_id}, events={len(events)}, "
                f"spawns={spawns}, finalizer OK")
    '';

    controller-restart = ''
      # ══════════════════════════════════════════════════════════════════
      # controller-restart — reconcile sees real UUID, empty DashMap →
      # spawn_reconnect_watch (~65 lines in build.rs:164-193 + 346-409)
      # ══════════════════════════════════════════════════════════════════
      # DISTINCT from build-crd-reconnect below. That kills the SCHEDULER
      # — drain_stream sees Ok(None) (TCP FIN from pod graceful stop),
      # loops through its OWN reconnect body, increments
      # build_watch_reconnects_total. This kills the CONTROLLER — the
      # drain_stream task dies with the process. The NEW controller's
      # RECONCILER sees: is_real_uuid=true (Build CR has a real buildId),
      # is_terminal=false (phase=Building), watching.contains_key=false
      # (fresh DashMap). That's the gate at build.rs:156-193 — "this
      # Build was being watched by a controller that died". build-crd-
      # flow above deliberately asserts spawns==1 + reconnect_count==0
      # to prove the dedup works; THAT leaves the whole reconnect
      # apparatus at 0 lcov hits.
      #
      # Ordering: AFTER build-crd-flow (so spawns==1 proved dedup first),
      # BEFORE recovery (recovery kills the scheduler, which would
      # conflate the two reconnect mechanisms).
      with subtest("controller-restart: reconcile spawns reconnect_watch on fresh DashMap"):
          drv_path = client.succeed(
              "nix-instantiate "
              "--arg busybox '(builtins.storePath ${common.busybox})' "
              "${ctrlRestartDrv} 2>/dev/null"
          ).strip()
          client.succeed(
              f"nix copy --derivation --to 'ssh-ng://k3s-server' {drv_path}"
          )
          k3s_server.succeed(
              "k3s kubectl apply -f - <<'EOF'\n"
              "apiVersion: rio.build/v1alpha1\n"
              "kind: Build\n"
              "metadata:\n"
              "  name: test-ctrl-restart\n"
              "  namespace: ${ns}\n"
              "spec:\n"
              f"  derivation: {drv_path}\n"
              "  priority: 10\n"
              "EOF"
          )

          # Wait for Building — watch task running, buildId is a real
          # UUID. THIS is the state the new controller must find.
          # timeout=120 (not 60): same flannel-race dispatch lag the
          # terminal-phase wait below already budgets for. build-crd-flow
          # above got apply→terminal in ~17s this run, but its comment
          # documents >90s observed on cold informer cache. First-wait
          # here has no warm-cache guarantee (finalizer delete of
          # test-watch-dedup above may have triggered a watch resync).
          # Diagnostic dump on timeout: controller reconcile counts +
          # scheduler derivations_running + Build CR — distinguishes
          # "controller never reconciled" vs "reconciled but not dispatched"
          # vs "dispatched but phase not patched".
          try:
              k3s_server.wait_until_succeeds(
                  "k3s kubectl -n ${ns} get build test-ctrl-restart "
                  "-o jsonpath='{.status.phase}' | grep -qx Building",
                  timeout=120,
              )
          except Exception:
              k3s_server.execute(
                  "echo '=== DIAG: controller logs (reconcile events) ===' >&2; "
                  "k3s kubectl -n ${ns} logs deploy/rio-controller --tail=80 >&2; "
                  "echo '=== DIAG: Build CR (actual phase/buildId) ===' >&2; "
                  "k3s kubectl -n ${ns} get build test-ctrl-restart -o yaml >&2; "
                  "echo '=== DIAG: scheduler leader derivations_running ===' >&2; "
                  "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
                  "  -o jsonpath='{.spec.holderIdentity}') && "
                  "k3s kubectl get --raw "
                  "  /api/v1/namespaces/${ns}/pods/$leader:9091/proxy/metrics "
                  "| grep -E 'derivations_(running|queued)' >&2 || true"
              )
              raise

          # Graceful delete — NOT --force. SIGTERM → main() returns →
          # atexit → profraw flush. The OLD controller's profraw captures
          # the apply() path (build-crd-flow above) and some of this
          # subtest's initial reconcile. The NEW controller's profraw
          # captures spawn_reconnect_watch.
          ctrl_old = kubectl(
              "get pods -l app.kubernetes.io/name=rio-controller "
              "-o jsonpath='{.items[0].metadata.name}'"
          ).strip()
          print(f"controller-restart: deleting {ctrl_old}")
          kubectl(f"delete pod {ctrl_old} --grace-period=10")

          # Deployment spawns a replacement. Wait for it to be Ready
          # before checking anything — the reconcile fires after informer
          # sync, which needs the pod Running.
          k3s_server.wait_until_succeeds(
              "k3s kubectl -n ${ns} wait --for=condition=Available "
              "deploy/rio-controller --timeout=60s",
              timeout=90,
          )

          # Build STILL completes. spawn_reconnect_watch calls WatchBuild
          # with since_sequence from status.last_sequence — picks up
          # where the old drain_stream left off. Build was at ~10-15s of
          # its 30s sleep when we killed the controller; another ~15-20s
          # to go + reconnect slack. 120s not 90s: flannel subnet race
          # during bootstrap can delay worker pod start → dispatch lag.
          # Observed 2026-03-16: status.phase=Building but progress=0/1
          # at 90s — build never REACHED the worker. 30s more absorbs
          # that without masking a real reconnect-path failure (a
          # genuine hang shows progress=0/1 AND no worker assignment).
          try:
              k3s_server.wait_until_succeeds(
                  "k3s kubectl -n ${ns} get build test-ctrl-restart "
                  "-o jsonpath='{.status.phase}' | "
                  "grep -E '^(Succeeded|Completed|Cached)$'",
                  timeout=120,
              )
          except Exception:
              # Run 2 (2026-03-16): tail=80 was pure rustls/h2/tower DEBUG
              # noise — the INFO-level reconnect log from ~120s ago nowhere
              # near the tail. CR showed progress=0/1 lastSequence=1, but
              # those are WATCH-TASK-PATCHED fields: stale if reconnect
              # never fired, regardless of scheduler ground truth.
              # Distinguishes: (A) scheduler never dispatched vs (B) build
              # completed but reconnect_watch never consumed it.
              #   - ctrl spawns_total: 0 → reconcile never reached spawn
              #     path; ≥1 → reconnect fired (then check sched completed)
              #   - sched completed delta vs baseline: ≥1 → (B), 0 → (A)
              #   - sched queued>0 running=0: stuck in queue → (A)
              k3s_server.execute(
                  "echo '=== DIAG: NEW controller logs (non-DEBUG) ===' >&2; "
                  "k3s kubectl -n ${ns} logs deploy/rio-controller --since=5m "
                  "  | grep -vE '\"level\":\"DEBUG\"' >&2 || true; "
                  "echo '=== DIAG: NEW controller spawns_total ===' >&2; "
                  "ctrl=$(k3s kubectl -n ${ns} get pods "
                  "  -l app.kubernetes.io/name=rio-controller "
                  "  -o jsonpath='{.items[0].metadata.name}') && "
                  "k3s kubectl get --raw "
                  "  /api/v1/namespaces/${ns}/pods/$ctrl:9094/proxy/metrics "
                  "| grep -E 'build_watch_spawns|reconcile_duration.*build' >&2 || true; "
                  "echo '=== DIAG: scheduler leader derivation counters ===' >&2; "
                  "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
                  "  -o jsonpath='{.spec.holderIdentity}') && "
                  "k3s kubectl get --raw "
                  "  /api/v1/namespaces/${ns}/pods/$leader:9091/proxy/metrics "
                  "| grep -E 'derivations_(running|queued|completed)|builds_completed' >&2 || true; "
                  "echo '=== DIAG: Build CR ===' >&2; "
                  "k3s kubectl -n ${ns} get build test-ctrl-restart -o yaml >&2"
              )
              raise

          # NEW controller's log. `reconnecting WatchBuild` is the info!
          # at build.rs:~168 — fires ONLY from the is_real_uuid &&
          # !is_terminal && !watching branch. The OLD controller's logs
          # are gone (pod deleted), so this grep only sees the NEW one.
          # ≥1 not ==1: kube-rs reconcile can requeue before the watch
          # key is inserted, but the dedup at contains_key catches the
          # second attempt. ONE spawn, possibly one "skipping" debug.
          reconnect_log = int(k3s_server.succeed(
              "k3s kubectl -n ${ns} logs deploy/rio-controller | "
              "grep -c 'reconnecting WatchBuild' || echo 0"
          ).strip())
          assert reconnect_log >= 1, (
              "expected 'reconnecting WatchBuild' in NEW controller logs "
              "(spawn_reconnect_watch fires from reconcile on a Build "
              f"with real UUID + non-terminal + empty DashMap); got {reconnect_log}"
          )

          # spawns_total on the NEW controller. Exactly 1: the reconnect
          # path increments at build.rs:180. The NEW controller NEVER
          # hit apply()'s submit_build path (buildId was already real
          # when it reconciled) — so this 1 is PURELY from reconnect.
          # The OLD controller's spawns (from build-crd-flow's 1 +
          # this subtest's initial 1 = 2) are gone with its pod.
          cm_new = ctrl_metrics()
          spawns_new = metric_value(cm_new,
              "rio_controller_build_watch_spawns_total") or 0.0
          assert spawns_new == 1.0, (
              "expected exactly 1 spawn on NEW controller (reconnect "
              "path only — buildId was already set, never hit apply()'s "
              f"submit_build); got {spawns_new}"
          )

          # Cleanup. --wait=false + poll: same finalizer pattern.
          kubectl("delete build test-ctrl-restart --wait=false")
          k3s_server.wait_until_succeeds(
              "! k3s kubectl -n ${ns} get build test-ctrl-restart 2>/dev/null",
              timeout=15,
          )
          print(f"controller-restart PASS: reconnect_log={reconnect_log}, "
                f"spawns_new={spawns_new}")
    '';

    cancel-cgroup-kill = ''
      # ══════════════════════════════════════════════════════════════════
      # cancel-cgroup-kill — delete Build CR mid-exec → cgroup.kill="1"
      # ══════════════════════════════════════════════════════════════════
      # cgroup.rs:180 kill() writes "1" to cgroup.kill → kernel SIGKILLs
      # every PID in the tree. NO prior test cancels a RUNNING build —
      # build-crd-flow lets it complete, build-crd-reconnect/recovery kill
      # the scheduler/controller (build keeps running on the worker).
      #
      # Flow: delete Build CR → finalizer cleanup() → CancelBuild RPC →
      # scheduler dispatch Cancel to worker → runtime.rs try_cancel_build
      # → cgroup::kill_cgroup → fs::write(cgroup.kill, "1"). Log signal:
      # "build cancelled via cgroup.kill" at runtime.rs:197.
      with subtest("cancel-cgroup-kill: delete Build CR mid-exec → cgroup.kill"):
          drv_path = client.succeed(
              "nix-instantiate "
              "--arg busybox '(builtins.storePath ${common.busybox})' "
              "${cancelDrv} 2>/dev/null"
          ).strip()
          client.succeed(
              f"nix copy --derivation --to 'ssh-ng://k3s-server' {drv_path}"
          )
          k3s_server.succeed(
              "k3s kubectl apply -f - <<'EOF'\n"
              "apiVersion: rio.build/v1alpha1\n"
              "kind: Build\n"
              "metadata:\n"
              "  name: test-cancel\n"
              "  namespace: ${ns}\n"
              "spec:\n"
              f"  derivation: {drv_path}\n"
              "  priority: 10\n"
              "EOF"
          )

          # phase=Building → daemon spawned, cgroup created, sleep started.
          # timeout=120: same dispatch-lag variance as controller-restart.
          k3s_server.wait_until_succeeds(
              "k3s kubectl -n ${ns} get build test-cancel "
              "-o jsonpath='{.status.phase}' | grep -qx Building",
              timeout=120,
          )

          # Find the build's cgroup. sanitize_build_id(drv_path) = basename
          # with . → _, so the cgroup dir contains "lifecycle-cancel_drv".
          # Worker pod is DISTROLESS — no `find`/`wc`/`test` via `kubectl
          # exec`. Probe from the VM host instead: the worker's cgroup-ns
          # scopes its OWN /sys/fs/cgroup view, but the HOST sees the full
          # kubepods.slice/... tree. Same leaf dirname, longer path.
          # Resolve which node the pod is on (STS may schedule to either).
          worker_node = k3s_server.succeed(
              "k3s kubectl -n ${ns} get pod default-workers-0 "
              "-o jsonpath='{.spec.nodeName}'"
          ).strip()
          worker_vm = k3s_agent if worker_node == "k3s-agent" else k3s_server
          # -print -quit stops after first match (no `| head` SIGPIPE).
          cgroup_path = worker_vm.succeed(
              "find /sys/fs/cgroup -type d -name '*lifecycle-cancel_drv' "
              "-print -quit 2>/dev/null"
          ).strip()
          assert cgroup_path, (
              f"no cgroup dir matching *lifecycle-cancel_drv on {worker_node} — "
              "build not running, or sanitize_build_id changed?"
          )
          procs_before = int(worker_vm.succeed(
              f"wc -l < {cgroup_path}/cgroup.procs"
          ).strip())
          assert procs_before > 0, (
              f"cgroup.procs empty ({cgroup_path}) — build not actually "
              f"running in the cgroup?"
          )
          print(f"cancel-cgroup-kill: node={worker_node}, cgroup={cgroup_path}, "
                f"procs={procs_before}")

          # Delete → finalizer → CancelBuild → worker cgroup.kill.
          # --wait=false: the finalizer removes itself after CancelBuild
          # returns (or times out); don't block kubectl on that.
          kubectl("delete build test-cancel --wait=false")

          # PRIMARY assertion: cgroup REMOVED within 30s of delete. The
          # 60s sleep hasn't completed — so removal proves the cancel
          # chain ran: controller reconcile (deletionTimestamp) →
          # cleanup() → CancelBuild RPC (balanced, leader) → scheduler
          # dispatch Cancel → worker try_cancel_build → cgroup.kill="1"
          # → kernel SIGKILLs procs → BuildCgroup::Drop rmdirs. Kernel
          # rejects rmdir on non-empty cgroup, so gone ⇒ procs emptied.
          # Run 7: gone in <1.5s (via || echo 0 fallback on cgroup.procs
          # read — dir already rmdir'd by the time we polled).
          #
          # NOT checking `kubectl logs | grep 'cancelled via cgroup.kill'`:
          # the polling itself triggers kubelet "Failed when writing line
          # to log file, err=http2: stream closed" on the worker's log
          # file (runs 6+7 only, ~4-5s cadence from grep-poll start; runs
          # 4+5 never reached here). Worker emits the line (runtime.rs:197)
          # but kubelet's containerd log-read stream is disrupted under
          # TCG — not persisted to /var/log/pods/.../worker/0.log. Not a
          # rio bug; the cgroup-gone speed is conclusive.
          try:
              worker_vm.wait_until_succeeds(
                  f"! test -e {cgroup_path}",
                  timeout=30,
              )
          except Exception:
              procs_after = worker_vm.succeed(
                  f"cat {cgroup_path}/cgroup.procs 2>/dev/null | wc -l || echo gone"
              ).strip()
              k3s_server.execute(
                  "echo '=== DIAG: Build CR state (gone = finalizer ran) ===' >&2; "
                  "k3s kubectl -n ${ns} get build test-cancel -o yaml 2>&1 >&2; "
                  "echo '=== DIAG: worker logs (non-DEBUG, last 2m) ===' >&2; "
                  "k3s kubectl -n ${ns} logs default-workers-0 --since=2m "
                  "  | grep -vE '\"level\":\"DEBUG\"' | tail -40 >&2 || true; "
                  "echo '=== DIAG: controller logs (cleanup/CancelBuild) ===' >&2; "
                  "k3s kubectl -n ${ns} logs deploy/rio-controller --since=2m "
                  "  | grep -E 'cancel|cleanup|CancelBuild' >&2 || true"
              )
              print(f"cancel-cgroup-kill DIAG: procs_after={procs_after} "
                    f"(was {procs_before})")
              raise

          # Build CR gone --- finalizer cleanup() completed, removed itself.
          k3s_server.wait_until_succeeds(
              "! k3s kubectl -n ${ns} get build test-cancel 2>/dev/null",
              timeout=15,
          )
          print("cancel-cgroup-kill PASS: cgroup rmdir'd in <30s "
                "(sleep was 60s ⇒ killed not completed), CR gone")
    '';

    recovery = ''
      # ══════════════════════════════════════════════════════════════════
      # recovery — kill leader pod mid-build, standby takes over
      # ══════════════════════════════════════════════════════════════════
      # STRONGER than phase3b's single-instance `systemctl kill`: with
      # scheduler.replicas=2 (podAntiAffinity across server+agent), killing
      # the leader means the STANDBY acquires the lease. The standby's
      # recovery_total was 0 (standby never ran recover_from_pg —
      # LeaderAcquired never fired). After acquiring, it's exactly 1.
      #
      # Proves: lease TTL expiry detection → standby LeaderAcquired →
      # recover_from_pg loads REAL non-terminal rows from PG → dispatch
      # gate unblocks (if !is_leader || !recovery_complete → no-op).
      with subtest("recovery: kill leader mid-build, standby acquires + recovers"):
          # Baseline: boot-time leader already ran recovery once (its
          # LeaderAcquired fired during waitReady). This scrape goes to
          # the CURRENT leader — confirms recovery happened at all before
          # we trust any of the dispatch paths above.
          m = sched_metrics()
          boot_recovery = metric_value(m, "rio_scheduler_recovery_total",
                                       labels='{outcome="success"}')
          assert boot_recovery is not None and boot_recovery >= 1.0, (
              f"boot-time leader should have run recovery >=1 time, "
              f"got {boot_recovery!r}\n"
              f"  all recovery series: {m.get('rio_scheduler_recovery_total', {})!r}"
          )

          # Settle: q==0 AND r==0 BEFORE starting the slow build. The
          # derivations_running gauge is Tick-updated (scheduler default
          # ~10s; worker.rs:604-623). Without this baseline, the running
          # gauge might still show a stale count from pinDrv/watchDedup →
          # we'd kill the leader before the slow build is even dispatched
          # → PG has 0 non-terminal rows → assert fails. 120s: Tick is
          # 10s, each retry spawns a fresh port-forward (2s bind), and
          # port 19091 TIME_WAIT can eat a retry. Observed: v17 timed
          # out at 60s with connection-reset noise from the pf churn.
          sched_metric_wait(
              "awk '/^rio_scheduler_derivations_queued / {q=$2} "
              "/^rio_scheduler_derivations_running / {r=$2} "
              "END {exit !(q==0 && r==0)}'",
              timeout=120,
          )

          # Capture the pre-kill leader name. After `delete pod`, the
          # Deployment controller creates a NEW replacement pod with a
          # DIFFERENT name — so seeing leader_pod() return a different
          # value is positive proof the lease moved.
          old_leader = leader_pod()
          print(f"recovery: pre-kill leader = {old_leader}")

          # Backgrounded slow build. `nohup ... < /dev/null &` fully
          # detaches (no stdin read, no HUP on shell exit). client.execute
          # (not .succeed): returns immediately, no exit-code check.
          client.execute(
              "nohup nix-build --no-out-link "
              "--store 'ssh-ng://k3s-server' "
              "--arg busybox '(builtins.storePath ${common.busybox})' "
              "${recoverySlowDrv} "
              "> /tmp/recovery-slow.log 2>&1 < /dev/null &"
          )

          # Poll for dispatch (running ≥1). Settle-wait guaranteed
          # baseline 0, so a nonzero reading IS our slow build. 60s:
          # nix-build needs ~10-15s to reach dispatch (ssh-ng handshake
          # + SubmitBuild + DAG merge + dispatch on 10s Tick).
          sched_metric_wait(
              "grep -E '^rio_scheduler_derivations_running [1-9]'",
              timeout=60,
          )

          # PG snapshot BEFORE the kill. At kill time the worker's gRPC
          # stream is dead — it CANNOT report completion until a scheduler
          # is back. So the row is guaranteed non-terminal NOW. Checking
          # after recovery races with the build finishing (worker
          # reconnects → reports → status='completed' before the assert).
          # Same TERMINAL_STATUSES filter as load_nonterminal_derivations
          # (db.rs:625).
          #
          # psql_k8s (NOT psql): bitnami PG runs in a pod, not systemd.
          nonterminal = int(psql_k8s(k3s_server,
              "SELECT COUNT(*) FROM derivations "
              "WHERE status NOT IN "
              "('completed','poisoned','dependency_failed','cancelled')"
          ))
          assert nonterminal >= 1, (
              f"PG snapshot at kill time should have >=1 non-terminal drv "
              f"(slow build in-flight), got {nonterminal}"
          )
          print(f"recovery: PG has {nonterminal} non-terminal row(s) for recovery to load")

          # Kill the leader pod. --grace-period=0 --force: immediate
          # deletion, no SIGTERM drain. Simulates a node crash / OOMKill,
          # NOT graceful shutdown. The Deployment controller immediately
          # creates a replacement — but the STANDBY pod acquires the
          # lease first (it's already running, watching, probing;
          # replacement pod takes ~10-20s to reach Ready).
          kubectl(f"delete pod {old_leader} --grace-period=0 --force")

          # Standby acquires. Lease holderIdentity becomes a DIFFERENT
          # pod name. 60s timeout: lease TTL + acquire tick (~5s poll).
          # The Lease's holderIdentity briefly stays the old name until
          # the lease expires — that's why we can't just assert -n.
          k3s_server.wait_until_succeeds(
              f"test \"$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
              f"-o jsonpath='{{.spec.holderIdentity}}')\" != '{old_leader}'",
              timeout=60,
          )
          new_leader = leader_pod()
          assert new_leader != old_leader, (
              f"lease should move off killed pod: old={old_leader} new={new_leader}"
          )
          print(f"recovery: new leader = {new_leader}")

          # New leader ran recovery. EXACTLY 1.0: this pod was the
          # standby, it never ran recovery before (LeaderAcquired never
          # fired for it). Fresh acquisition → exactly one recovery. If
          # the replacement pod somehow won the lease race instead, same
          # thing — fresh process, first acquire, recovery_total = 1.
          #
          # wait_until_succeeds (not one-shot): recovery runs in the
          # LeaderAcquired handler, asynchronously after lease acquire.
          # There's a small window where lease moved but recovery hasn't
          # finished yet.
          sched_metric_wait(
              "grep -qx 'rio_scheduler_recovery_total{outcome=\"success\"} 1'",
              timeout=120,
          )

          # Worker re-registered with the new leader. Fresh scheduler
          # process = metrics reset → workers_active climbs back to 1
          # (or 2 if pod-1 from a later autoscaler run somehow exists —
          # it doesn't yet). ≥1 not ==1: the slow build's worker may
          # have briefly disconnected/reconnected during failover.
          sched_metric_wait(
              "grep -E '^rio_scheduler_workers_active [1-9]'",
              timeout=120,
          )

          # Post-recovery build. DIFFERENT marker → different output path
          # → NOT a cache hit. Proves dispatch is unblocked AFTER the
          # lease re-acquire + recover_from_pg sequence (if recovery
          # failed or never ran, dispatch_ready stays false forever).
          out_recovery = build("${recoveryDrv}", capture_stderr=False).strip()
          assert out_recovery.startswith("/nix/store/"), (
              f"post-recovery build should succeed: {out_recovery!r}"
          )

          # Re-check recovery_total is EXACTLY 1 at the end — proves
          # recovery ran exactly once in THIS leader's process lifetime
          # (no spurious re-acquires, no double-recovery bugs).
          m = sched_metrics()
          final_recovery = metric_value(m, "rio_scheduler_recovery_total",
                                        labels='{outcome="success"}')
          assert final_recovery == 1.0, (
              f"new leader should have recovery_total=1 (fresh process, one "
              f"acquire), got {final_recovery!r}\n"
              f"  all recovery series: {m.get('rio_scheduler_recovery_total', {})!r}"
          )

          # Drain the slow build before the next sections. 150s: up to
          # ~90s sleep remainder + re-dispatch overhead after failover +
          # ReconcileAssignments cross-check delay.
          sched_metric_wait(
              "awk '/^rio_scheduler_derivations_queued / {q=$2} "
              "/^rio_scheduler_derivations_running / {r=$2} "
              "END {exit !(q==0 && r==0)}'",
              timeout=150,
          )
          print(f"recovery PASS: standby took over, loaded {nonterminal} row(s), built {out_recovery}")
    '';

    build-crd-reconnect = ''
      # ══════════════════════════════════════════════════════════════════
      # build-crd-reconnect — controller drain_stream reconnects on EOF
      # ══════════════════════════════════════════════════════════════════
      # Regression guard for the Ok(None)-returns-unconditionally bug
      # (same class as gateway 06a1fa0). k8s pod kill = SIGTERM → graceful
      # drain → TCP FIN → tonic Ok(None), NOT Err(Transport). Before the
      # fix, drain_stream returned on ANY Ok(None), so a scheduler pod
      # kill mid-build froze the Build CR's status at "Building" forever.
      #
      # build-crd-flow above runs on a stable scheduler so it never hits
      # this path. recovery killed the leader once already; this kills
      # the NEW leader. The Deployment-spawned replacement + the original
      # standby are both available for failover.
      with subtest("build-crd-reconnect: status resumes after scheduler kill mid-build"):
          # Baseline reconnect count. The build-crd-flow Build CR hit
          # terminal before any scheduler restart, so its drain_stream
          # never reconnected. recovery's slow build went through ssh-ng
          # (no Build CR), so the controller was uninvolved. Expect 0.
          cm0 = ctrl_metrics()
          reconnects0 = metric_value(cm0, "rio_controller_build_watch_reconnects_total") or 0.0

          # Instantiate + upload .drv closure. Same pattern as build-crd-flow.
          drv_path = client.succeed(
              "nix-instantiate "
              "--arg busybox '(builtins.storePath ${common.busybox})' "
              "${reconnectDrv} 2>/dev/null"
          ).strip()
          client.succeed(
              f"nix copy --derivation --to 'ssh-ng://k3s-server' {drv_path}"
          )
          k3s_server.succeed(
              "k3s kubectl apply -f - <<'EOF'\n"
              "apiVersion: rio.build/v1alpha1\n"
              "kind: Build\n"
              "metadata:\n"
              "  name: test-reconnect\n"
              "  namespace: ${ns}\n"
              "spec:\n"
              f"  derivation: {drv_path}\n"
              "  priority: 10\n"
              "EOF"
          )

          # Wait for Building (not Pending): the build is dispatched and
          # the worker is actively running it. Started event fired, first
          # patch landed. If we kill while still Pending, build_id may
          # still be the "submitted" sentinel and WatchBuild can't work.
          k3s_server.wait_until_succeeds(
              "k3s kubectl -n ${ns} get build test-reconnect "
              "-o jsonpath='{.status.phase}' | grep -qx Building",
              timeout=60,
          )

          # Kill the CURRENT leader. Controller's drain_stream receives
          # Ok(None) (TCP FIN from graceful pod termination). Without the
          # fix, it returns; with the fix, it enters the reconnect loop.
          leader = leader_pod()
          print(f"build-crd-reconnect: killing leader {leader}")
          kubectl(f"delete pod {leader} --grace-period=0 --force")

          # Wait for the NEW leader's recovery to load the in-flight
          # build from PG. Without this, the controller's drain_stream
          # reconnects burn through MAX_RECONNECT on WatchBuild NotFound
          # (actor in-memory map is empty until recover_from_pg finishes —
          # 35597c0 made NotFound retryable but the retry budget is
          # finite). derivations_running ≥1 on the leader proves recovery
          # loaded the non-terminal row. sched_metric_wait re-resolves
          # leader_pod() each poll — picks up the new leader once it
          # acquires. 60s: lease TTL ≤1× (graceful delete step_down()
          # clears holder → standby steals on next 5s tick) + recovery
          # PG query + slack. This is STRUCTURAL, not a timeout bump —
          # moves the controller's retry budget out of the race window.
          sched_metric_wait(
              "awk '/^rio_scheduler_derivations_running / {exit !($2>=1)}'",
              timeout=60,
          )

          # THE ASSERTION: status reaches Succeeded. Controller reconnected
          # via WatchBuild on the new leader, which replayed events from
          # last_sequence. 180s: 60s build + lease TTL 15s + controller
          # backoff 1+2+4s + new leader's recover_from_pg + informer lag.
          # Before the fix: status stuck at Building, this times out.
          try:
              k3s_server.wait_until_succeeds(
                  "k3s kubectl -n ${ns} get build test-reconnect "
                  "-o jsonpath='{.status.phase}' | "
                  "grep -E '^(Succeeded|Completed|Cached)$'",
                  timeout=180,
              )
          except Exception:
              # DIAGNOSTIC: why didn't status reach Succeeded? Dump the
              # controller pod logs (drain_stream's reconnect warns) +
              # the Build CR's final status + metric.
              k3s_server.execute(
                  "echo '=== DIAG: controller pod logs (last 80) ===' >&2; "
                  "k3s kubectl -n ${ns} logs deploy/rio-controller --tail=80 >&2; "
                  "echo '=== DIAG: test-reconnect Build CR ===' >&2; "
                  "k3s kubectl -n ${ns} get build test-reconnect -o yaml >&2; "
                  "echo '=== DIAG: scheduler pods ===' >&2; "
                  "k3s kubectl -n ${ns} get pods -l app.kubernetes.io/name=rio-scheduler -o wide >&2; "
                  "echo '=== DIAG: lease holder ===' >&2; "
                  "k3s kubectl -n ${ns} get lease rio-scheduler-leader -o yaml >&2"
              )
              # Also scrape the reconnect metric — did drain_stream even
              # TRY to reconnect? Zero → my A.1 fix is broken.
              # Nonzero → reconnect fired, something downstream failed.
              cm_diag = ctrl_metrics()
              reconnects_diag = metric_value(cm_diag, "rio_controller_build_watch_reconnects_total") or 0.0
              print(f"DIAG: build_watch_reconnects_total = {reconnects_diag} "
                    f"(baseline was {reconnects0})")
              raise

          # Metric proof: reconnect counter incremented. drain_stream's
          # shared reconnect body fires this on every attempt (EOF or
          # Err). ≥1 not ==1: if the first WatchBuild hits the
          # still-draining old leader or the not-yet-leader standby,
          # drain_stream retries.
          cm = ctrl_metrics()
          reconnects = metric_value(cm, "rio_controller_build_watch_reconnects_total") or 0.0
          assert reconnects > reconnects0, (
              "expected build_watch_reconnects_total to increment "
              f"(was {reconnects0}, now {reconnects}) — drain_stream "
              "should have entered the reconnect loop on non-terminal EOF"
          )

          # Cleanup. --wait=false + poll: same finalizer pattern as
          # build-crd-flow.
          kubectl("delete build test-reconnect --wait=false")
          k3s_server.wait_until_succeeds(
              "! k3s kubectl -n ${ns} get build test-reconnect 2>/dev/null",
              timeout=15,
          )
          print(f"build-crd-reconnect PASS: reconnects {reconnects0}→{reconnects}")
    '';

    gc-dry-run = ''
      # ══════════════════════════════════════════════════════════════════
      # gc-dry-run — TriggerGC via AdminService proxy, sweep ROLLBACK
      # ══════════════════════════════════════════════════════════════════
      # Same RPC path as phase3b (scheduler → store → mark → sweep →
      # progress stream → proxy back), but -plaintext since vmtest-full
      # has tls.enabled=false. grace=24h → nothing past grace → empty
      # unreachable set → proves the stream runs end-to-end, NOT that
      # the for-batch loop body executes (that's gc-sweep's job).
      with subtest("gc-dry-run: TriggerGC completes, currentPath describes outcome"):
          result = sched_grpc(
              '{"dry_run": true, "grace_period_hours": 24}',
              "rio.admin.AdminService/TriggerGC",
          )
          # GCProgress stream: at least one message with isComplete=true.
          # grpcurl emits one PRETTY-PRINTED JSON object per stream message
          # (proto3 camelCase, multi-line with indented fields). Parse
          # structurally — substring match on "delete"/"path" is satisfied
          # by any error like "failed to delete, path unknown". raw_decode
          # loop handles the multi-line format AND any leading non-JSON
          # noise from port-forward's stderr (2>&1).
          dec = json.JSONDecoder()
          gc_msgs = []
          idx = result.find("{")
          while 0 <= idx < len(result):
              obj, idx = dec.raw_decode(result, idx)
              gc_msgs.append(obj)
              idx = result.find("{", idx)
          complete_msgs = [m for m in gc_msgs if m.get("isComplete")]
          assert complete_msgs, (
              f"expected at least one GCProgress with isComplete=true; "
              f"got {len(gc_msgs)} messages: {result[:500]}"
          )
          # currentPath describes the outcome (mark+sweep RAN, even if sweep
          # found nothing). Looking for "would delete" (dry-run phrasing)
          # in the final message's currentPath — NOT a substring match on
          # the whole stream blob.
          final = complete_msgs[-1]
          assert "would delete" in final.get("currentPath", "").lower(), (
              f"expected dry-run currentPath to say 'would delete'; "
              f"got: {final.get('currentPath')!r}"
          )
          print("gc-dry-run PASS: TriggerGC stream completed via AdminService proxy")
    '';

    build-crd-errors = ''
      # ══════════════════════════════════════════════════════════════════
      # build-crd-errors — InvalidSpec + cleanup never-submitted
      # ══════════════════════════════════════════════════════════════════
      # Two error paths with ONE Build CR: spec.derivation points to a
      # .drv NOT in rio-store → fetch_and_build_node GetPath NotFound
      # (build.rs:524) → Error::InvalidSpec → error_policy 300s requeue
      # (build.rs:481-483). Status never progresses past empty. Then
      # delete → cleanup() sees build_id.is_empty() → "never submitted"
      # early return (build.rs:431-432) — no CancelBuild RPC, finalizer
      # just removes itself.
      #
      # Skipping scheduler-unreachable (build.rs:463-464, ~2 lines):
      # needs scheduler scaled to 0 then back to 2, which disrupts the
      # autoscaler subtest below. Cost > value for 2 lines.
      with subtest("build-crd-errors: InvalidSpec → error_policy + cleanup never-submitted"):
          cm_before = ctrl_metrics()
          errs_before = cm_before.get("rio_controller_reconcile_errors_total", {})

          # Valid-looking store path (32-char nixbase32 hash) but NOT in
          # rio-store. fetch_and_build_node does GetPath → NotFound.
          k3s_server.succeed(
              "k3s kubectl apply -f - <<'EOF'\n"
              "apiVersion: rio.build/v1alpha1\n"
              "kind: Build\n"
              "metadata:\n"
              "  name: test-invalidspec\n"
              "  namespace: ${ns}\n"
              "spec:\n"
              "  derivation: /nix/store/00000000000000000000000000000000-nonexistent.drv\n"
              "  priority: 10\n"
              "EOF"
          )

          # error_policy fired → reconcile_errors_total{error_kind=
          # "invalid_spec"} incremented. Counter is multi-label; check
          # that SOME series under reconciler=build gained a count. 10s
          # slack: reconcile fires on informer delivery (~immediate) +
          # one retry.
          for _ in range(20):  # 10s at 0.5s poll
              cm = ctrl_metrics()
              errs = cm.get("rio_controller_reconcile_errors_total", {})
              grew = any(
                  'reconciler="build"' in labels and v > errs_before.get(labels, 0.0)
                  for labels, v in errs.items()
              )
              if grew:
                  break
              import time as _time
              _time.sleep(0.5)
          else:
              raise AssertionError(
                  f"reconcile_errors_total{{reconciler=build}} didn't grow "
                  f"in 10s; before={errs_before!r}, after={errs!r}"
              )

          # Status stays empty — InvalidSpec returns Err BEFORE any
          # status patch. buildId was never set, never even "submitted".
          bid = kubectl(
              "get build test-invalidspec -o jsonpath='{.status.buildId}'"
          ).strip()
          assert bid == "", (
              "buildId should be empty (InvalidSpec before any status "
              f"patch); got {bid!r}"
          )

          # Delete → cleanup() → build_id.is_empty() → "never submitted"
          # → return early, no CancelBuild. CR gone fast (no scheduler
          # round-trip, just finalizer self-remove).
          kubectl("delete build test-invalidspec --wait=false")
          k3s_server.wait_until_succeeds(
              "! k3s kubectl -n ${ns} get build test-invalidspec 2>/dev/null",
              timeout=10,
          )

          # Log evidence: "never submitted (nothing to cancel)" at
          # build.rs:431. Proves cleanup took the empty-buildId branch,
          # not the normal CancelBuild path.
          never_submitted = int(k3s_server.succeed(
              "k3s kubectl -n ${ns} logs deploy/rio-controller | "
              "grep -c 'never submitted' || echo 0"
          ).strip())
          assert never_submitted >= 1, (
              f"expected 'never submitted' log from cleanup(); got {never_submitted}"
          )
          print("build-crd-errors PASS: InvalidSpec → error_policy → "
                "cleanup never-submitted → CR gone")
    '';

    reconciler-replicas = ''
      # ══════════════════════════════════════════════════════════════════
      # reconciler-replicas — manual STS scale NOT stomped by reconciler
      # ══════════════════════════════════════════════════════════════════
      # Regression: the reconciler was reverting STS.spec.replicas to
      # spec.replicas.min on every reconcile (SSA with .force() re-claimed
      # the field from the autoscaler's field-manager). Simulate the
      # autoscaler by scaling directly — the .owns(StatefulSet) watch
      # fires → reconcile runs. If bug present, replicas reverts to 1
      # within the same reconcile that patches status.
      #
      # POSITIVE signal before NEGATIVE: wait for the reconciler's own
      # status patch (desiredReplicas=2) — proves the reconcile RAN —
      # THEN assert STS.spec.replicas is STILL 2. Replaces phase3a's
      # `time.sleep(5)` hope-the-reconcile-ran hack with a deterministic
      # gate (phase3a.nix:725-730).
      with subtest("reconciler-replicas: SSA field-ownership handoff preserves manual scale"):
          kubectl("scale statefulset default-workers --replicas=2")

          # Reconciler observed the change (via .owns watch), reconciled,
          # patched WorkerPool.status.desiredReplicas. This IS the
          # reconcile — if it were going to stomp replicas, it would have
          # done so in the same apply() call. Accept 1 OR 2: with the 10s
          # scale-down window (controller.extraEnv[3]), the autoscaler may
          # have already patched 2→1 before reconcile fires. Either way,
          # desiredReplicas reflecting a NON-STALE value proves reconcile
          # ran after our scale.
          k3s_server.wait_until_succeeds(
              "dr=$(k3s kubectl -n ${ns} get workerpool default "
              "-o jsonpath='{.status.desiredReplicas}'); "
              'test "$dr" = 1 -o "$dr" = 2',
              timeout=20,
          )

          # THE ACTUAL INVARIANT: reconciler's SSA patch does NOT claim
          # spec.replicas. managedFields records which manager owns each
          # field. If the rio-controller manager's fieldsV1 includes
          # f:replicas under f:spec, the reconciler is re-claiming it —
          # the regression. This check is autoscaler-agnostic: whether
          # replicas is 1 (autoscaler won) or 2 (we won), rio-controller
          # must NOT be the owner.
          #
          # grep -A50 bounds the managedFields entry scan (each manager's
          # entry is <50 lines). `! grep -q` inverts: PASS if f:replicas
          # NOT found under rio-controller. Previously checked value==2
          # which raced with the 10s-window autoscaler.
          k3s_server.succeed(
              "! k3s kubectl -n ${ns} get statefulset default-workers -o yaml | "
              "grep -A50 'manager: rio-controller' | "
              "grep -B50 -m1 '^  - apiVersion\\|^status:' | "
              "grep -q 'f:replicas'"
          )

          # Reset to 1 so autoscaler observes 1→2 (not 2→2 no-op).
          kubectl("scale statefulset default-workers --replicas=1")
          k3s_server.wait_until_succeeds(
              "test \"$(k3s kubectl -n ${ns} get workerpool default "
              "-o jsonpath='{.status.desiredReplicas}')\" = 1",
              timeout=20,
          )
          print("reconciler-replicas PASS: manual STS scale survived reconcile")
    '';

    autoscaler = ''
      # ══════════════════════════════════════════════════════════════════
      # autoscaler — queue pressure → STS replicas 1→2
      # ══════════════════════════════════════════════════════════════════
      # FIRST TIME scale_one() runs against a real apiserver. The
      # reconciler-replicas test proved the reconciler doesn't STOMP, but
      # the autoscaler ITSELF never patched anything until now. Proves:
      # (a) SSA patch body includes apiVersion+kind — without them the STS
      # patch 400s "apiVersion must be set" → warn log → autoscaler
      # silently never scales (phase3a.nix:819-823), (b) autoscaler patches
      # WorkerPool.status.lastScaleTime via its SEPARATE field-manager.
      #
      # Timing: controller.extraEnv sets poll/up-window/min-interval = 3s
      # (default 30s would mean ~60s to first scale — too slow). With
      # sustained queued≥3, first scale in ~6-12s.
      with subtest("autoscaler: queue pressure patches STS replicas 1→2"):
          # 5 builds, all leaves, one DAG submit. Backgrounded — script
          # polls metrics while builds run. No NIX_CONFIG: client VM's
          # nix.settings.experimental-features already set (mkClientNode).
          client.execute(
              "nohup nix-build --no-out-link "
              "--store 'ssh-ng://k3s-server' "
              "--arg busybox '(builtins.storePath ${common.busybox})' "
              "${autoscaleDrvFile} -A d1 -A d2 -A d3 -A d4 -A d5 "
              "> /tmp/autoscale-build.log 2>&1 < /dev/null &"
          )

          # Queue depth ≥3. With maxConcurrentBuilds=1, 1 runs + 4 queue.
          # ≥3 not ==4: first dispatch may happen between poll + gauge
          # update (Tick-lagged). ≥3 is enough for compute_desired=2. 40s
          # timeout: Tick latency + submit overhead.
          sched_metric_wait(
              "awk '/^rio_scheduler_derivations_queued / {print $2}' | "
              "grep -qE '^[3-9]|^[1-9][0-9]'",
              timeout=40,
          )

          # THE ASSERTION: STS replicas 1→2. EXACTLY 2 (not ≥2) — proves
          # the patch body was well-formed (apiVersion+kind present). A
          # malformed patch would leave replicas at 1 forever (400s are
          # swallowed as warn-logs by kube-rs). 60s: 3s poll + 3s
          # up-window + jitter + k3s VM latency.
          k3s_server.wait_until_succeeds(
              "test \"$(k3s kubectl -n ${ns} get statefulset default-workers "
              "-o jsonpath='{.spec.replicas}')\" = 2",
              timeout=60,
          )

          # WorkerPool.status.lastScaleTime set (RFC3339 string, non-
          # empty). Owned by the autoscaler's SSA field-manager (distinct
          # from the reconciler's — otherwise reconcile would clobber it
          # to None every cycle).
          k3s_server.wait_until_succeeds(
              "sc=$(k3s kubectl -n ${ns} get workerpool default "
              "-o jsonpath='{.status.lastScaleTime}'); "
              "test -n \"$sc\"",
              timeout=20,
          )

          # Scaling condition explains WHY replicas changed.
          kubectl(
              "get workerpool default "
              "-o jsonpath='{.status.conditions[?(@.type==\"Scaling\")].reason}' | "
              "grep -q ScaledUp"
          )

          # scaling_decisions_total{direction="up"} ≥1. Proves scale_one()
          # ran to COMPLETION (metric increments after the patch succeeds,
          # scaling.rs:257). Not exact — an earlier reconciler-replicas
          # reset to 1 MIGHT have triggered a scale-up race depending on
          # timing, so ≥1 is the honest bound.
          cm = ctrl_metrics()
          scale_up = metric_value(cm, "rio_controller_scaling_decisions_total",
                                  labels='{direction="up"}')
          assert scale_up is not None and scale_up >= 1.0, (
              f"expected scaling_decisions_total{{direction=\"up\"}} >= 1, "
              f"got {scale_up!r}\n"
              f"  all scaling series: {cm.get('rio_controller_scaling_decisions_total', {})!r}"
          )

          # Drain the 5 builds. Can't `wait` across shell sessions (each
          # .succeed is a fresh shell — the &-backgrounded nix-build is a
          # job in a DIFFERENT shell). Poll q==0 AND r==0. 5×15s ≈ 75s
          # sequential on pod-0 alone (pod-1 may never Ready on 4GB VM).
          # Build success irrelevant — we only need queue drained so
          # finalizer's acquire_many doesn't block.
          sched_metric_wait(
              "awk '/^rio_scheduler_derivations_queued / {q=$2} "
              "/^rio_scheduler_derivations_running / {r=$2} "
              "END {exit !(q==0 && r==0)}'",
              timeout=150,
          )

          # ── Scale-down: queue empty → STS replicas 2→1 ───────────────
          # With RIO_AUTOSCALER_SCALE_DOWN_WINDOW_SECS=10 (default 600s,
          # shortened via controller.extraEnv[3]), after queue=0 is stable
          # for 10s the autoscaler patches STS back to 1. This exercises
          # check_stabilization's Direction::Down arm + ScaledDown event
          # recorder path — both uncovered before this test.
          # 60s: 10s down-window + 3s poll + 3s min-interval + k3s latency.
          k3s_server.wait_until_succeeds(
              "test \"$(k3s kubectl -n ${ns} get statefulset default-workers "
              "-o jsonpath='{.spec.replicas}')\" = 1",
              timeout=60,
          )

          # direction="down" metric. ≥1: same honest-bound rationale as
          # scale_up above (reconciler-replicas reset MIGHT have raced).
          cm = ctrl_metrics()
          scale_down = metric_value(cm, "rio_controller_scaling_decisions_total",
                                    labels='{direction="down"}')
          assert scale_down is not None and scale_down >= 1.0, (
              'expected scaling_decisions_total{direction="down"} >= 1, '
              f"got {scale_down!r}\n"
              f"  all scaling series: {cm.get('rio_controller_scaling_decisions_total', {})!r}"
          )

          # ScaledDown K8s Event on the WorkerPool. The autoscaler's
          # recorder.publish path for Direction::Down (scaling.rs:359).
          events_down = kubectl(
              "get events "
              "--field-selector involvedObject.name=default,involvedObject.kind=WorkerPool,reason=ScaledDown "
              "-o name"
          ).strip()
          assert events_down, (
              "expected ScaledDown K8s Event on WorkerPool/default, got none"
          )
          print(f"autoscaler PASS: STS scaled 1→2→1, up={scale_up} down={scale_down}")
    '';

    gc-sweep = ''
      # ══════════════════════════════════════════════════════════════════
      # gc-sweep — PinPath + backdate + non-dry-run sweep PROVES commit
      # ══════════════════════════════════════════════════════════════════
      # THE test that proves sweep's for-batch loop body executes. With
      # all-in-grace paths, unreachable=vec![] → loop never runs → neither
      # commit NOR rollback fires → dry-run and non-dry-run are
      # indistinguishable. Backdating ONE path past grace makes it the
      # sole unreachable candidate → `pathsCollected: "1"` proves the
      # DELETE ran.
      #
      # Self-contained: builds its own pin-target + victim paths.
      # In the monolith, these came from the `initial` and `recovery`
      # subtests (ordering hack to avoid slow-build worker-slot block).
      # Split architecture: each fragment owns its data.
      with subtest("gc-sweep: PinPath + backdated sweep deletes EXACTLY 1"):
          # Build the two target paths. pinDrv = pinned, survives sweep.
          # gcVictimDrv = unpinned, backdated, deleted by sweep.
          out_pin = build("${pinDrv}", capture_stderr=False).strip()
          assert out_pin.startswith("/nix/store/"), f"pin build: {out_pin!r}"
          out_victim = build("${gcVictimDrv}", capture_stderr=False).strip()
          assert out_victim.startswith("/nix/store/"), f"victim build: {out_victim!r}"

          # Baseline BEFORE PinPath. The prior absolute-==1 check would
          # confusingly fail if anything earlier (fixture, controller
          # startup) inserts a gc_roots row, AND ==1 PASSES a no-op
          # PinPath if nothing else ever inserts either. Delta is robust.
          gc_roots_base = int(psql_k8s(k3s_server, "SELECT COUNT(*) FROM gc_roots"))

          # Pin out_pin. PinPath is rio.store.StoreAdminService on the
          # STORE (port 9002), NOT scheduler's AdminService (9001 — that
          # only has the TriggerGC proxy, not Pin/Unpin).
          #
          # JSON construction: f-string inside Python inside Nix.
          # {{ }} → Python sees { }. The store_path value is a /nix/store/
          # path — no embedded quotes, safe to single-quote wrap.
          store_grpc(
              f'{{"store_path": "{out_pin}", "source": "vm-lifecycle"}}',
              "rio.store.StoreAdminService/PinPath",
          )
          pin_after = int(psql_k8s(k3s_server, "SELECT COUNT(*) FROM gc_roots"))
          assert pin_after == gc_roots_base + 1, (
              f"PinPath should add exactly 1 gc_roots row; "
              f"before={gc_roots_base}, after={pin_after}"
          )

          # Backdate out_victim past grace. This is the path sweep will
          # delete. out_victim is unpinned (we only pinned out_pin) AND
          # unreferenced (worker uploads have references=vec![] — phase4
          # gap, see phase3b.nix:922-924). created_at = now() - 25h puts
          # it 1h past a 24h grace window.
          #
          # Single-quote SQL avoids bash-escaping (psql_k8s wraps in
          # double quotes). Python f-string interpolates the path.
          psql_k8s(k3s_server,
              f"UPDATE narinfo SET created_at = now() - interval '25 hours' "
              f"WHERE store_path = '{out_victim}'"
          )

          # Non-dry-run sweep. dry_run=false → sweep COMMITs. grace=24h →
          # ONLY out_victim is past grace → unreachable={out_victim}
          # → for-batch loop iterates once → DELETE → COMMIT.
          result = sched_grpc(
              '{"dry_run": false, "grace_period_hours": 24}',
              "rio.admin.AdminService/TriggerGC",
          )
          # proto3 JSON uint64 is a STRING ("1" not 1). pathsCollected
          # EXACTLY "1" is THE assertion: without backdate, it's 0 (or
          # absent — proto3 omits zero-value) and we've proven nothing
          # about the loop body. Same raw_decode loop as gc-dry-run.
          dec = json.JSONDecoder()
          gc_msgs = []
          idx = result.find("{")
          while 0 <= idx < len(result):
              obj, idx = dec.raw_decode(result, idx)
              gc_msgs.append(obj)
              idx = result.find("{", idx)
          complete_msgs = [m for m in gc_msgs if m.get("isComplete")]
          assert complete_msgs, (
              f"expected GCProgress.isComplete=true; got {len(gc_msgs)} "
              f"messages: {result[:500]}"
          )
          final = complete_msgs[-1]
          assert final.get("pathsCollected") == "1", (
              f"expected EXACTLY pathsCollected='1' (backdated victim "
              f"output); got {final.get('pathsCollected')!r}. Full: {final!r}"
          )

          # out_victim GONE — nix path-info MUST fail.
          client.fail(
              f"nix path-info --store 'ssh-ng://k3s-server' {out_victim}"
          )
          # out_pin still there — pin protected it.
          client.succeed(
              f"nix path-info --store 'ssh-ng://k3s-server' {out_pin}"
          )

          # UnpinPath round-trip (idempotent).
          store_grpc(
              f'{{"store_path": "{out_pin}"}}',
              "rio.store.StoreAdminService/UnpinPath",
          )
          unpin_after = int(psql_k8s(k3s_server, "SELECT COUNT(*) FROM gc_roots"))
          # Back to baseline — PinPath's row is gone. ==gc_roots_base NOT
          # ==0: if anything else ever pins, ==0 is wrong; if PinPath was
          # a no-op AND UnpinPath was a no-op, ==0 passes (both broke).
          assert unpin_after == gc_roots_base, (
              f"UnpinPath should restore gc_roots to baseline; "
              f"base={gc_roots_base}, now={unpin_after}"
          )
          print("gc-sweep PASS: pin protected, backdated path deleted, unpin round-trip OK")
    '';

    finalizer = ''
      # ══════════════════════════════════════════════════════════════════
      # finalizer — delete WorkerPool → pod gone → CR gone → workers=0
      # ══════════════════════════════════════════════════════════════════
      #   The autoscaler's 30s poll fires DURING this subtest (~300s wall
      #   time). scaling.rs:222 `if pool.metadata.deletion_timestamp.is_some()
      #   { skip }` MUST fire — otherwise the autoscaler would try to scale
      #   a being-deleted pool, racing the finalizer's scale-to-0. The test
      #   passing (pod gone + CR gone) proves the skip gate works.
      #
      # Runs LAST: deletes the only WorkerPool, so no workers exist after.
      # Finalizer's cleanup(): DrainWorker + scale STS → 0 + wait for pod
      # termination + remove finalizer. Queue is already empty (autoscaler
      # drained) → acquire_many succeeds immediately → no blocking.
      with subtest("finalizer: delete WorkerPool → drain → pod gone → CR gone"):
          # --wait=false: don't block kubectl on the finalizer. We assert
          # each stage with its own timeout so a hang points at the exact
          # stage (pod-gone vs CR-gone vs workers_active).
          kubectl("delete workerpool default --wait=false")

          # Pod gone. Proves: STS scaled to 0, SIGTERM drain exited
          # cleanly (no in-flight builds), finalizer removed (K8s could
          # GC the owned StatefulSet). Pod-1 from autoscaler is also gone
          # (same STS).
          #
          # 300s: vmtest-full.yaml terminationGracePeriodSeconds=180 +
          # 60s DRAIN_WAIT_SLOP + margin. STS terminates pods in reverse
          # ordinal — if pod-1 (autoscaler-spawned) never went Ready and
          # is stuck, pod-0 waits until pod-1's grace period SIGKILL
          # before starting its own termination. v24/v25 showed pod-1
          # Terminating 4m44s with the old 7200s grace.
          k3s_server.wait_until_succeeds(
              "! k3s kubectl -n ${ns} get pod default-workers-0 2>/dev/null",
              timeout=300,
          )

          # WorkerPool CR gone (finalizer removed → K8s deleted it).
          k3s_server.wait_until_succeeds(
              "! k3s kubectl -n ${ns} get workerpool default 2>/dev/null",
              timeout=30,
          )

          # Scheduler saw the disconnect. workers_active EXACTLY 0 — not
          # just ≤0 (gauge underflow would be a bug).
          sched_metric_wait(
              "grep -qx 'rio_scheduler_workers_active 0'",
              timeout=30,
          )
          print("finalizer PASS: WorkerPool deleted, pod drained, scheduler saw disconnect")
    '';

  };

  # ── Chain assertions ──────────────────────────────────────────────────
  # Eval-time guards against misordered subtests. finalizer needs
  # autoscaler to have scaled STS to 2 first (pod-1 for reverse-ordinal
  # termination coverage — v24/v25 regression). build-crd-flow must run
  # BEFORE cancel-cgroup-kill if both are present (spawns==1 assertion
  # expects it to be the FIRST Build CR).
  assertChains =
    subtests:
    let
      inherit (pkgs) lib;
      idx = name: lib.lists.findFirstIndex (s: s == name) (-1) subtests;
      has = name: builtins.elem name subtests;
    in
    assert lib.assertMsg (
      !(has "finalizer") || (has "autoscaler" && idx "autoscaler" < idx "finalizer")
    ) "lifecycle: finalizer requires autoscaler earlier (pod-1 reverse-ordinal coverage)";
    assert lib.assertMsg (
      !(has "build-crd-flow")
      || !(has "cancel-cgroup-kill")
      || idx "build-crd-flow" < idx "cancel-cgroup-kill"
    ) "lifecycle: build-crd-flow must precede cancel-cgroup-kill (spawns==1 assertion)";
    true;

  mkTest =
    {
      name,
      subtests,
      globalTimeout ? 900,
    }:
    assert assertChains subtests;
    pkgs.testers.runNixOSTest {
      name = "rio-lifecycle-${name}";
      inherit globalTimeout;
      inherit (fixture) nodes;
      testScript = ''
        ${prelude}
        ${pkgs.lib.concatMapStrings (s: fragments.${s} + "\n") subtests}
        ${common.collectCoverage fixture.pyNodeVars}
      '';
    };
in
{
  inherit fragments mkTest;
}
