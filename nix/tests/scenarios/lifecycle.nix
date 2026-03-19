# Lifecycle scenario: scheduler recovery, GC, autoscaler, finalizer,
# health-shared NOT_SERVING probe — all exercised against the k3s-full
# fixture.
#
# Ports phase3b sections S (recovery), C (GC), T (health-shared) +
# phase3a autoscaler/finalizer/SSA-field-ownership onto the 2-node k3s
# Helm-chart fixture. Unlike phase3b (control/worker/k8s/client as
# separate systemd VMs), everything here runs as PODS — closes the
# "production uses pod path, VM tests use systemd" gap for the
# reconciler/lease/autoscaler surface.
#
#
# Fragment architecture: this file returns { fragments, mkTest } instead
# of a single runNixOSTest. default.nix composes fragments into 3 parallel
# VM tests (core, recovery, autoscale) — critical path ~8min vs the prior
# ~14min monolith. Each fragment is a Python
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
#   cancel-cgroup-kill calls CancelBuild via gRPC mid-exec and asserts
#   the cgroup is rmdir'd before the sleep completes. cgroup.rs:180
#   kill() writes "1" to cgroup.kill → kernel SIGKILLs the tree. No
#   other test cancels a RUNNING build (recovery kills the scheduler,
#   build keeps running on the worker).
#
# r[verify worker.upload.references-scanned]
#   refs-end-to-end builds a consumer derivation whose $out embeds a
#   dep's store path, then asserts PG narinfo."references" contains
#   that path. Proves RefScanSink → PutPath → PG end-to-end (not just
#   unit-level scanner correctness).
#
# r[verify worker.upload.deriver-populated]
#   refs-end-to-end asserts narinfo.deriver is the consumer's .drv path
#   (name-matched + .drv suffix). Before the phase4a fix, deriver was
#   always empty — upload.rs never plumbed it from the executor.
#
# r[verify store.gc.two-phase]
#   refs-end-to-end pins ONLY the consumer, backdates both paths past
#   grace, sweeps, and asserts the dep SURVIVES. Proves mark's recursive
#   CTE actually walks narinfo."references" — if it didn't, dep would
#   be unreachable (no pin, no inbound edge in the CTE) and swept. This
#   is the ONLY VM-level test of mark-follows-refs; gc-sweep's victim
#   has refs=[] by construction (mkTrivial output embeds no store paths).
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

  # cancel-cgroup-kill in-flight build. 60s sleep: wait-for-running
  # + find cgroup + assert it has procs + CancelBuild + wait for
  # cgroup gone. Shorter than recoverySlowDrv because cancel is
  # direct (gRPC→scheduler→worker RPC, no lease transition).
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

  # refs-end-to-end: two-stage build where consumer's $out embeds dep's
  # store path as a literal string. The worker's RefScanSink finds the
  # hash part during NAR dump → PutPath sends references=[dep] → PG
  # narinfo."references" is non-empty → GC mark's CTE walks it.
  #
  # dep's output (just "i am the dep payload" text, NO store paths) has
  # refs=[] — same as mkTrivial. Only consumer has a non-empty ref set.
  # This asymmetry is load-bearing: the GC-survival half of the test
  # pins ONLY consumer and asserts dep survives via the reference edge.
  #
  # Same ''${...}/''' escaping as autoscaleDrvFile: the inner .nix
  # reads its OWN let-bound busybox/dep, not this evaluation's scope.
  refsDrvFile = pkgs.writeText "lifecycle-refs.nix" ''
    { busybox }:
    let
      sh = "''${busybox}/bin/sh";
      bb = "''${busybox}/bin/busybox";
      dep = derivation {
        name = "rio-refs-dep";
        system = builtins.currentSystem;
        builder = sh;
        args = [ "-c" '''
          ''${bb} mkdir -p $out
          ''${bb} echo "i am the dep payload" > $out/payload
        ''' ];
      };
      consumer = derivation {
        name = "rio-refs-consumer";
        system = builtins.currentSystem;
        builder = sh;
        args = [ "-c" '''
          ''${bb} mkdir -p $out
          # This line embeds dep's FULL /nix/store/HASH-rio-refs-dep
          # into $out/script. RefScanSink (upload.rs) finds the 32-char
          # nixbase32 hash part during the pre-scan NAR dump.
          ''${bb} echo "source path: ''${dep}" > $out/script
          ''${bb} cat ''${dep}/payload >> $out/script
        ''' ];
      };
    in { inherit dep consumer; }
  '';

  # ── testScript prelude: bootstrap + Python helpers ────────────────────
  # Shared by all fragment compositions. start_all + waitReady (~4min on
  # k3s-full) + kubectlHelpers + metric-scrape defs + sshKeySetup + seed.
  # Pyflakes doesn't warn on unused function DEFS (only imports/locals),
  # so sparse splits that don't call every helper are fine.
  prelude = ''
    ${common.assertions}

    ${common.kvmPreopen}
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
          # set_serving on named service. DISTINCT local port (19102,
          # not 19101): the kill above leaves :19101 in TIME_WAIT for
          # ~60s, and port-forward without SO_REUSEADDR can't rebind —
          # it dies silently (stderr→/dev/null), probe gets conn-refused
          # → exit 2. sleep 2 doesn't help; TIME_WAIT outlasts it.
          k3s_server.succeed(
              f"k3s kubectl -n ${ns} port-forward {leader} 19102:9101 "
              f">/dev/null 2>&1 & echo $! > /tmp/pf-health.pid; sleep 2"
          )
          try:
              k3s_server.succeed(
                  "grpc-health-probe -addr localhost:19102 "
                  "-service rio.scheduler.SchedulerService"
              )
          finally:
              k3s_server.execute(
                  "kill $(cat /tmp/pf-health.pid) 2>/dev/null; rm -f /tmp/pf-health.pid"
              )
          print("health-shared PASS: standby NOT_SERVING, leader SERVING "
                "(plaintext port shares reporter with mTLS port)")
    '';

    cancel-cgroup-kill = ''
      # ══════════════════════════════════════════════════════════════════
      # cancel-cgroup-kill — gRPC CancelBuild mid-exec → cgroup.kill="1"
      # ══════════════════════════════════════════════════════════════════
      # cgroup.rs:180 kill() writes "1" to cgroup.kill → kernel SIGKILLs
      # every PID in the tree. NO prior test cancels a RUNNING build —
      # recovery kills the scheduler (build keeps running on the worker).
      #
      # P0294: the Build CR is gone. Cancel via gRPC CancelBuild
      # directly (the SAME RPC the old CR finalizer called). Flow:
      # CancelBuild RPC → scheduler dispatch Cancel to worker →
      # runtime.rs try_cancel_build → cgroup::kill_cgroup →
      # fs::write(cgroup.kill, "1"). Log signal: "build cancelled via
      # cgroup.kill" at runtime.rs:197.
      #
      # Submission is ALSO via gRPC (SubmitBuild, not ssh-ng://) so we
      # get the build_id back for CancelBuild. The ssh-ng:// path goes
      # through the gateway's Nix worker protocol — no build_id is
      # surfaced to the nix client. P0289's build-timeout port inherits
      # this gRPC-direct pattern.
      with subtest("cancel-cgroup-kill: gRPC CancelBuild mid-exec → cgroup.kill"):
          drv_path = client.succeed(
              "nix-instantiate "
              "--arg busybox '(builtins.storePath ${common.busybox})' "
              "${cancelDrv} 2>/dev/null"
          ).strip()
          client.succeed(
              f"nix copy --derivation --to 'ssh-ng://k3s-server' {drv_path}"
          )

          # SubmitBuild via gRPC. It streams BuildEvents; the first event
          # carries buildId. `-max-time 5` caps the stream read — the 60s
          # build won't finish in that window, grpcurl exits with
          # DeadlineExceeded. `|| true` swallows that exit code — the
          # build is already persisted in PG, the stream was just
          # observability. raw_decode parses the first pretty-printed
          # JSON object from the captured output.
          #
          # Port 19099 (not sched_grpc's 19001) to sidestep TIME_WAIT
          # contention — port-forward lacks SO_REUSEADDR, ~60s to rebind.
          #
          # Payload via json.dumps — NOT inline {{}} escaping. Nix
          # double-single-quote strings interpret triple-apostrophe as
          # an escape for literal-double-apostrophe, so Python f-triple-
          # quote syntax cannot be used here. json.dumps produces double-
          # quoted JSON, safe inside single-quoted shell `-d '...'`.
          #
          # DerivationNode requires drvHash + system (scheduler grpc/mod.rs
          # :379-407 validates). drvHash = drvPath for input-addressed
          # derivations (gateway translate.rs:361 does the same). system
          # is the VM platform. outputNames = ["out"] — mkTrivial's single
          # output. The gateway normally parses the .drv to populate
          # these; we're bypassing that, so we fill them statically.
          leader = leader_pod()
          submit_payload = json.dumps({
              "nodes": [{
                  "drvPath": drv_path,
                  "drvHash": drv_path,
                  "system": "${pkgs.stdenv.hostPlatform.system}",
                  "outputNames": ["out"],
              }],
              "edges": [],
          })
          submit_out = k3s_server.succeed(
              f"k3s kubectl -n ${ns} port-forward {leader} 19099:9001 "
              f">/dev/null 2>&1 & pf=$!; "
              f"trap 'kill $pf 2>/dev/null' EXIT; sleep 2; "
              f"${grpcurl} ${grpcurlTls} -max-time 5 "
              f"-protoset ${protoset}/rio.protoset "
              f"-d '{submit_payload}' "
              f"localhost:19099 rio.scheduler.SchedulerService/SubmitBuild "
              f"2>&1 || true"
          )
          brace = submit_out.find("{")
          assert brace >= 0, (
              f"no JSON in SubmitBuild output — submit failed? got: {submit_out[:500]!r}"
          )
          first_ev, _ = json.JSONDecoder().raw_decode(submit_out, brace)
          build_id = first_ev.get("buildId", "")
          assert build_id, (
              f"first BuildEvent missing buildId; got: {first_ev!r}"
          )
          print(f"cancel-cgroup-kill: submitted, build_id={build_id}")

          # Wait for the build's cgroup to appear — this IS the
          # "phase=Building" signal (daemon spawned, cgroup created,
          # sleep started). sanitize_build_id(drv_path) = basename with
          # . → _, so the cgroup dir contains "lifecycle-cancel_drv".
          #
          # Worker pod is DISTROLESS — no `find`/`wc`/`test` via `kubectl
          # exec`. Probe from the VM host instead: the worker's cgroup-ns
          # scopes its OWN /sys/fs/cgroup view, but the HOST sees the full
          # kubepods.slice/... tree. Same leaf dirname, longer path.
          # Resolve which node the pod is on (STS may schedule to either).
          #
          # timeout=120: dispatch-lag variance (flannel subnet race
          # observed 2026-03-16 delaying worker pod start).
          worker_node = k3s_server.succeed(
              "k3s kubectl -n ${ns} get pod default-workers-0 "
              "-o jsonpath='{.spec.nodeName}'"
          ).strip()
          worker_vm = k3s_agent if worker_node == "k3s-agent" else k3s_server
          # -print -quit stops after first match (no `| head` SIGPIPE).
          # `grep .` makes the command fail when find emits nothing (find
          # itself exits 0 on no-match), so wait_until_succeeds retries.
          cgroup_path = worker_vm.wait_until_succeeds(
              "find /sys/fs/cgroup -type d -name '*lifecycle-cancel_drv' "
              "-print -quit 2>/dev/null | grep .",
              timeout=120,
          ).strip()
          procs_before = int(worker_vm.succeed(
              f"wc -l < {cgroup_path}/cgroup.procs"
          ).strip())
          assert procs_before > 0, (
              f"cgroup.procs empty ({cgroup_path}) — build not actually "
              f"running in the cgroup?"
          )
          print(f"cancel-cgroup-kill: node={worker_node}, cgroup={cgroup_path}, "
                f"procs={procs_before}")

          # CancelBuild via gRPC — the replacement for "delete Build CR →
          # finalizer → CancelBuild". sched_grpc handles port-forward +
          # mTLS + protoset. Unary RPC, returns CancelBuildResponse.
          cancel_resp = sched_grpc(
              json.dumps({"buildId": build_id, "reason": "vm-test-cancel"}),
              "rio.scheduler.SchedulerService/CancelBuild",
          )
          print(f"cancel-cgroup-kill: CancelBuild → {cancel_resp.strip()!r}")

          # PRIMARY assertion: cgroup REMOVED within 30s. The 60s sleep
          # hasn't completed — so removal proves the cancel chain ran:
          # scheduler dispatch Cancel → worker try_cancel_build →
          # cgroup.kill="1" → kernel SIGKILLs procs → BuildCgroup::Drop
          # rmdirs. Kernel rejects rmdir on non-empty cgroup, so gone ⇒
          # procs emptied. Pre-P0294 observed: gone in <1.5s.
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
                  "echo '=== DIAG: worker logs (non-DEBUG, last 2m) ===' >&2; "
                  "k3s kubectl -n ${ns} logs default-workers-0 --since=2m "
                  "  | grep -vE '\"level\":\"DEBUG\"' | tail -40 >&2 || true; "
                  "echo '=== DIAG: scheduler leader logs (cancel dispatch) ===' >&2; "
                  "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
                  "  -o jsonpath='{.spec.holderIdentity}') && "
                  "k3s kubectl -n ${ns} logs $leader --since=2m "
                  "  | grep -iE 'cancel' >&2 || true"
              )
              print(f"cancel-cgroup-kill DIAG: procs_after={procs_after} "
                    f"(was {procs_before}), build_id={build_id}")
              raise

          print("cancel-cgroup-kill PASS: cgroup rmdir'd in <30s "
                "(sleep was 60s ⇒ killed not completed)")
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
          # force=true bypasses the empty-refs safety gate — mkTrivial
          # outputs embed no store-path strings, so the ref scanner
          # correctly finds refs=[] for every fixture path, tripping
          # the >10%-empty-refs precondition even on dry-run.
          result = sched_grpc(
              '{"dry_run": true, "grace_period_hours": 24, "force": true}',
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
          # unreferenced: gcVictimDrv is mkTrivial, its output is plain
          # text with no embedded store paths, so the ref scanner
          # correctly finds refs=[] — correct behavior for a leaf
          # derivation, not a gap (see line ~100 + ~225). created_at =
          # now() - 25h puts it 1h past a 24h grace window.
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
          #
          # force=true bypasses the empty-refs safety gate — mkTrivial
          # leaf outputs genuinely have refs=[] (scanner finds no store
          # paths, see comment above), which would otherwise trip the
          # gate (FailedPrecondition).
          result = sched_grpc(
              '{"dry_run": false, "grace_period_hours": 24, "force": true}',
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
          # ── path_tenants migration-applied smoke check ────────────────
          # Proves migration 012 applied in the k3s PG (table exists,
          # queryable). Expected count = 0: gc-sweep's build() uses the
          # default SSH key (empty comment, common.nix:393) → gateway
          # sends tenant_name="" → scheduler stores tenant_id=None →
          # completion.rs filter_map drops None → upsert never fires.
          #
          # TODO(P0207): wire a tenant-key build into gc-sweep so this
          # can assert >= 1 (proving the completion hook fires end-to-
          # end). P0207's retention test needs positive row counts
          # anyway -- empty path_tenants makes the retention arm a
          # no-op (see plan-0206 deps note).
          pt_count = int(psql_k8s(k3s_server,
              f"SELECT COUNT(*) FROM path_tenants "
              f"WHERE store_path_hash = sha256(convert_to('{out_pin}', 'UTF8'))"
          ))
          assert pt_count == 0, (
              f"gc-sweep builds are tenant-less (single-tenant mode); "
              f"path_tenants should have 0 rows for out_pin, got {pt_count}. "
              f"If >0: some OTHER build with a tenant key referenced this "
              f"path -- check test isolation."
          )

          print("gc-sweep PASS: pin protected, backdated path deleted, unpin round-trip OK")
    '';

    refs-end-to-end = ''
      # ══════════════════════════════════════════════════════════════════
      # refs-end-to-end — refscan → PG references → GC mark walks refs
      # ══════════════════════════════════════════════════════════════════
      # End-to-end test of the worker ref scanner → PG references →
      # GC mark-walks-refs chain. gc-sweep proves the DELETE path
      # (victim has refs=[] by construction — mkTrivial leaf, scanner
      # correctly finds nothing). This proves the SURVIVAL path:
      # consumer's refscan-populated references[] makes dep REACHABLE via
      # mark's recursive CTE → dep survives a sweep even though dep itself
      # is unpinned AND past grace AND has refs=[] (no outbound edges).
      #
      # Four assertions in sequence, each with its own failure signature:
      #   1. PG references contains dep → RefScanSink + PutPath work
      #   2. PG deriver is the .drv path → deriver plumbing works
      #   3. pin consumer + sweep → dep survives → mark CTE walks refs
      #   4. unpin + sweep → both gone → sweep commits when unreachable
      #
      # Self-contained. Runs after gc-sweep (both exercise TriggerGC) but
      # does NOT depend on gc-sweep's state: gc-sweep's out_pin is in-grace
      # (never backdated) and its out_victim is already deleted.
      with subtest("refs-end-to-end: refscan → PG → GC mark walks refs"):
          # Build dep FIRST to capture its output path. Building consumer
          # alone would also build dep (it's an input), but nix-build only
          # prints the top-level out path — we need dep's path for the PG
          # assertion. Second build cache-hits dep (DAG dedup on the .drv).
          out_dep = build("${refsDrvFile} -A dep", capture_stderr=False).strip()
          assert out_dep.startswith("/nix/store/"), f"dep: {out_dep!r}"
          out_consumer = build("${refsDrvFile} -A consumer", capture_stderr=False).strip()
          assert out_consumer.startswith("/nix/store/"), f"consumer: {out_consumer!r}"
          print(f"refs-e2e: dep={out_dep}")
          print(f"refs-e2e: consumer={out_consumer}")

          # ── 1. references landed in PG ────────────────────────────────
          # array_to_string: readable assert message if it fails (shows
          # the full refs list, not just a count). \"references\" in this
          # Python source → "references" after Python parses the escape →
          # psql_k8s re-escapes it for bash → PG sees the quoted keyword.
          refs_str = psql_k8s(k3s_server,
              "SELECT array_to_string(\"references\", ' ') FROM narinfo "
              f"WHERE store_path = '{out_consumer}'"
          )
          assert out_dep in refs_str, (
              f"consumer's references should contain dep path {out_dep!r}; "
              f"PG returned: {refs_str!r}. RefScanSink did not find the "
              "store-path hash embedded in $out/script, OR PutPath "
              "dropped the references field on the wire."
          )
          # cardinality ≥1: guards against array_to_string returning the
          # path from a DIFFERENT row via a wrong WHERE (e.g. empty WHERE
          # accidentally selecting the first row). Belt-and-suspenders.
          refs_len = int(psql_k8s(k3s_server,
              "SELECT cardinality(\"references\") FROM narinfo "
              f"WHERE store_path = '{out_consumer}'"
          ))
          assert refs_len >= 1, f"cardinality should be ≥1, got {refs_len}"

          # ── 2. deriver populated ──────────────────────────────────────
          # Name + .drv suffix. NOT the exact /nix/store/HASH-...drv path
          # (would need a nix-instantiate round-trip to learn the hash);
          # the name half is deterministic (derivation.name) and .drv
          # distinguishes it from an output path.
          deriver = psql_k8s(k3s_server,
              f"SELECT deriver FROM narinfo WHERE store_path = '{out_consumer}'"
          )
          assert deriver and "rio-refs-consumer" in deriver and deriver.endswith(".drv"), (
              "deriver should be the consumer .drv path "
              f"(/nix/store/HASH-rio-refs-consumer.drv), got: {deriver!r}"
          )

          # ── 3. GC survival: pin consumer, sweep, dep SURVIVES ─────────
          # Backdate BOTH past grace. Without this they'd be auto-roots
          # via the in-grace seed (created_at > now() - grace_hours) and
          # the sweep would be a no-op regardless of refs correctness.
          psql_k8s(k3s_server,
              "UPDATE narinfo SET created_at = now() - interval '25 hours' "
              f"WHERE store_path IN ('{out_dep}', '{out_consumer}')"
          )
          store_grpc(
              f'{{"store_path": "{out_consumer}", "source": "vm-refs-e2e"}}',
              "rio.store.StoreAdminService/PinPath",
          )
          # force=true: dep is sweep-eligible (past grace, unpinned, and
          # unreachable from any root EXCEPT via consumer's refs) and dep
          # has refs=[] (its output is plain text with zero store paths).
          # That's 1-of-N-eligible with empty refs → ≥10% → gate trips.
          # force bypasses the gate; the SURVIVAL assertion below is what
          # proves correctness, not the gate.
          result = sched_grpc(
              '{"dry_run": false, "grace_period_hours": 24, "force": true}',
              "rio.admin.AdminService/TriggerGC",
          )
          # raw_decode loop: grpcurl emits one pretty-printed JSON object
          # per GCProgress stream message. Same pattern as gc-sweep.
          dec = json.JSONDecoder()
          gc_msgs = []
          idx = result.find("{")
          while 0 <= idx < len(result):
              obj, idx = dec.raw_decode(result, idx)
              gc_msgs.append(obj)
              idx = result.find("{", idx)
          final = [m for m in gc_msgs if m.get("isComplete")][-1]
          # proto3 JSON omits zero-valued uint64 fields. pathsCollected=0
          # → the field is ABSENT, not "0". .get() with default "0".
          collected = final.get("pathsCollected", "0")
          assert collected == "0", (
              "with consumer pinned, dep should be reachable via the "
              "mark CTE's walk over narinfo.references → nothing swept; "
              f"got pathsCollected={collected!r}. Either the CTE is not "
              "following the references column, or dep's row was swept "
              f"despite being in the closure. Full: {final!r}"
          )
          # Direct PG check — stronger than pathsCollected. If the CTE
          # walked refs correctly, dep's narinfo row is still there.
          dep_still = psql_k8s(k3s_server,
              f"SELECT 1 FROM narinfo WHERE store_path = '{out_dep}'"
          )
          assert dep_still == "1", (
              "dep MUST survive sweep when consumer is pinned "
              "(reachable via references CTE walk); row is GONE. "
              "mark.rs CTE is not following references."
          )
          cons_still = psql_k8s(k3s_server,
              f"SELECT 1 FROM narinfo WHERE store_path = '{out_consumer}'"
          )
          assert cons_still == "1", "consumer (pinned root) must survive"

          # ── 4. unpin → sweep → both GONE ──────────────────────────────
          # Removes the only root covering these paths. Consumer is now
          # unreachable (no pin, past grace, nothing else references it).
          # Dep is unreachable (consumer was its only referrer; consumer
          # is unreachable). Sweep should collect EXACTLY these two —
          # every other path in PG (out_pin from gc-sweep, busybox seed,
          # earlier subtest outputs) is still in-grace.
          store_grpc(
              f'{{"store_path": "{out_consumer}"}}',
              "rio.store.StoreAdminService/UnpinPath",
          )
          result = sched_grpc(
              '{"dry_run": false, "grace_period_hours": 24, "force": true}',
              "rio.admin.AdminService/TriggerGC",
          )
          dec = json.JSONDecoder()
          gc_msgs = []
          idx = result.find("{")
          while 0 <= idx < len(result):
              obj, idx = dec.raw_decode(result, idx)
              gc_msgs.append(obj)
              idx = result.find("{", idx)
          final = [m for m in gc_msgs if m.get("isComplete")][-1]
          assert final.get("pathsCollected") == "2", (
              "after unpin, BOTH dep+consumer should be swept "
              "(unreachable + past grace); got pathsCollected="
              f"{final.get('pathsCollected')!r}. Full: {final!r}"
          )
          dep_gone = psql_k8s(k3s_server,
              f"SELECT COUNT(*) FROM narinfo WHERE store_path = '{out_dep}'"
          )
          assert dep_gone == "0", "dep should be swept after unpin"
          cons_gone = psql_k8s(k3s_server,
              f"SELECT COUNT(*) FROM narinfo WHERE store_path = '{out_consumer}'"
          )
          assert cons_gone == "0", "consumer should be swept after unpin"
          print("refs-end-to-end PASS: refscan→PG→mark-walks-refs proven")
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
  # termination coverage — v24/v25 regression).
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
    true;

  # Coverage-instrumented images are ~3-4× larger. The k3s containerd
  # tmpfs (fixtures/k3s-full.nix) removes the 3.3-5× builder-disk-write
  # variance that motivated the original +900s (076de36: lifecycle-core
  # cov rio-store gate hit 489s, >half the 900s budget on bootstrap).
  # Remaining variance is 9p reads + zstd decompress (CPU-bound). +300s
  # hedges for the residual tail. Additive so explicit overrides stack
  # (autoscale 1200 → 1500 in cov). Normal-mode CI budget unchanged.
  covTimeoutHeadroom = if common.coverage then 300 else 0;

  mkTest =
    {
      name,
      subtests,
      globalTimeout ? 900,
    }:
    assert assertChains subtests;
    pkgs.testers.runNixOSTest {
      name = "rio-lifecycle-${name}";
      skipTypeCheck = true;
      globalTimeout = globalTimeout + covTimeoutHeadroom;
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
