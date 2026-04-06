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
# obs.metric.controller — verify marker at default.nix:subtests[autoscaler]
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
# ctrl.probe.named-service — verify marker at default.nix:subtests[health-shared]
#   health-shared probes with `-service rio.scheduler.SchedulerService`
#   (the named service, NOT empty-string) and asserts NOT_SERVING on
#   standby. scheduler/main.rs (r[impl ctrl.probe.named-service]):
#   set_not_serving only affects the named service. This proves the
#   CLIENT-SIDE BALANCER constraint via grpc-health-probe CLI — NOT
#   the K8s readinessProbe (which is tcpSocket, doesn't probe gRPC
#   health at all).
#
# ctrl.autoscale.skip-deleting — verify marker at default.nix:subtests[finalizer]
#   finalizer subtest deletes the BuilderPool and waits ~300s for pod
#   termination. The autoscaler's 30s poll fires DURING that window;
#   scaling.rs:222 deletion_timestamp.is_some() skip-gate MUST fire
#   or the autoscaler would race the finalizer's scale-to-0.
#
# worker.cancel.cgroup-kill — verify marker at default.nix:subtests[cancel-cgroup-kill]
#   cancel-cgroup-kill calls CancelBuild via gRPC mid-exec and asserts
#   the cgroup is rmdir'd before the sleep completes. cgroup.rs:180
#   kill() writes "1" to cgroup.kill → kernel SIGKILLs the tree. No
#   other test cancels a RUNNING build (recovery kills the scheduler,
#   build keeps running on the worker).
#
# worker.cgroup.kill-on-teardown — verify marker at default.nix:subtests[build-timeout]
# worker.timeout.no-reassign — verify marker at default.nix:subtests[build-timeout]
#   build-timeout submits via gRPC SubmitBuild with buildTimeout=5 against
#   a 30s sleep. The timeout fires mid-build → run_daemon_build returns
#   → executor/mod.rs:764 build_cgroup.kill() fires unconditionally →
#   drain → Drop rmdirs. Asserts cgroup GONE (kernel rejects rmdir on
#   non-empty, so gone ⇒ builder killed ⇒ kill-on-teardown ran) + a
#   second build of the SAME drv succeeds (no EEXIST — leak is closed).
#   Distinct from cancel-cgroup-kill: that tests runtime.rs's explicit
#   CancelSignal path; this tests the executor's post-daemon teardown.
#
# worker.upload.references-scanned — verify marker at default.nix:subtests[refs-end-to-end]
#   refs-end-to-end builds a consumer derivation whose $out embeds a
#   dep's store path, then asserts PG narinfo."references" contains
#   that path. Proves RefScanSink → PutPath → PG end-to-end (not just
#   unit-level scanner correctness).
#
# worker.upload.deriver-populated — verify marker at default.nix:subtests[refs-end-to-end]
#   refs-end-to-end asserts narinfo.deriver is the consumer's .drv path
#   (name-matched + .drv suffix). Before the phase4a fix, deriver was
#   always empty — upload.rs never plumbed it from the executor.
#
# store.gc.two-phase — verify marker at default.nix:subtests[refs-end-to-end]
#   refs-end-to-end pins ONLY the consumer, backdates both paths past
#   grace, sweeps, and asserts the dep SURVIVES. Proves mark's recursive
#   CTE actually walks narinfo."references" — if it didn't, dep would
#   be unreachable (no pin, no inbound edge in the CTE) and swept. This
#   is the ONLY VM-level test of mark-follows-refs; gc-sweep's victim
#   has refs=[] by construction (mkTrivial output embeds no store paths).
#
# store.gc.tenant-retention — verify marker at default.nix:subtests[gc-sweep]
#   gc-sweep tail: backdates out_tenant's narinfo past global grace but
#   leaves path_tenants.first_referenced_at inside the tenant's 168h
#   retention window → sweep collects 0 (seed f protects it). Then
#   backdates first_referenced_at past retention too → sweep collects 1.
#   Proves tenant retention EXTENDS global grace (the spec's "floor"
#   semantics) end-to-end with completion-hook-produced rows.
#
# ctrl.drain.disruption-target — verify marker at default.nix:subtests[disruption-drain]
#   disruption-drain submits a 120s-sleep build, evicts default-builders-0
#   via the K8s eviction API (sets status.conditions[DisruptionTarget]=
#   True), and asserts the controller's watcher fires DrainWorker
#   {force:true}. The pod self-drain (SIGTERM, force=false) is the
#   fallback; the watcher runs FIRST. Log signal:
#   "DisruptionTarget: DrainWorker force=true" in controller logs.
#
# ctrl.pool.ephemeral — verify marker at default.nix:subtests[ephemeral-pool]
#   ephemeral-pool: applies WorkerPoolSpec.ephemeral=true; asserts NO
#   StatefulSet (apply() took the ephemeral branch, mod.rs:118-132);
#   asserts status.desiredReplicas == replicas.max (reconcile_ephemeral
#   ran and patched status, ephemeral.rs:220-228). The full
#   Job-spawn-on-queue path requires running after finalizer (no STS
#   worker to steal the dispatch) — proven in the vm-lifecycle-ephemeral
#   composition (see default.nix), not inline here. The structural
#   assertions (no-STS, status-patched, cleanup-immediate) ARE
#   self-contained.
#
# ctrl.pdb.workers — verify marker at default.nix:subtests[pdb-ownerref]
#   pdb-ownerref: build_pdb (builders.rs:178) produces `{pool}-pdb`
#   with maxUnavailable=1 + ownerReferences[0]→BuilderPool. Asserts
#   against the fixture's `default` pool: `default-pdb` exists,
#   spec.maxUnavailable=1, ownerRef[0].kind=BuilderPool, GC'd on
#   BuilderPool delete (ownerRef cascade). Unit test tests.rs:550
#   proves the struct shape; this proves the reconciler SSA-applies it
#   AND K8s GC honors the ownerRef end-to-end.
#
# ctrl.wps.reconcile — verify marker at default.nix:subtests[wps-lifecycle]
# ctrl.wps.autoscale — verify marker at default.nix:subtests[wps-lifecycle]
#   wps-lifecycle: apply a 3-class WPS → BuilderPoolSet reconciler
#   (builderpoolset/mod.rs:131) creates 3 child WorkerPools named
#   `{wps}-{class}` with sizeClass=class.name + ownerReferences[0]→WPS
#   + controller=true. Delete WPS → finalizer cleanup explicitly
#   deletes each child (mod.rs:375), ownerRef GC as fallback. The
#   autoscale marker piggybacks on the child spec: each child carries
#   autoscaling.targetValue from class.targetQueuePerReplica
#   (builders.rs:92-99) — the per-class autoscaler reads this. That
#   wiring is verified structurally here; the scale-up/down mechanics
#   are covered by scaling.rs unit tests (r[verify ctrl.wps.autoscale]
#   there).
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture)
    ns
    nsStore
    nsBuilders
    nsFetchers
    pki
    ;
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

  # In-flight build for recovery. 60s sleep survives the leader-kill
  # window: lease TTL (~15s worst case for standby to detect) + standby's
  # recovery query (~1s) + re-dispatch latency (~5s). phase3b rationale
  # (phase3b.nix:85-99) applies verbatim — a shorter sleep lets the build
  # finish during the failover gap → PG has 0 non-terminal rows →
  # recovery loads nothing → hollow test.
  #
  # 60s (was 90s): the FUSE circuit breaker's wall_clock_trip default is
  # 90s (rio-builder/src/fuse/circuit.rs DEFAULT_WALL_CLOCK_TRIP). With
  # sleepSecs=90 the gap between the slow build's input fetch and the
  # post-recovery recoveryDrv's input fetch is ~95-100s, tripping the
  # circuit → EIO → handle_infrastructure_failure retry loop → test
  # timeout. At 60s the gap is ~65-70s, safely under. Under KVM the
  # failover window is <5s (observed: lease-moved 0.2s, recovery 1.8s),
  # so 60s still amply survives the gap.
  recoverySlowDrv = drvs.mkTrivial {
    marker = "lifecycle-recovery-slow";
    sleepSecs = 60;
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

  # build-timeout victim. sleepSecs=30 vs buildTimeout=5 — wide gap so
  # neither TCG dispatch lag (timeout may fire at 8-12s wall) nor the
  # scheduler's 10s tick granularity lets the sleep finish first. Same
  # marker-in-drvname pattern so the cgroup dir is findable from the
  # VM host (sanitize_build_id: ".drv" → "_drv").
  timeoutDrv = drvs.mkTrivial {
    marker = "lifecycle-timeout";
    sleepSecs = 30;
  };

  # disruption-drain in-flight build. 120s sleep: must survive the
  # kubectl-eviction + watcher-fire + log-check window (~30s). Shorter
  # than 2h grace but long enough that "reassign in seconds" (which
  # DrainWorker force=true enables) vs "burn 2h grace" is OBSERVABLY
  # different.
  disruptionDrv = drvs.mkTrivial {
    marker = "lifecycle-disruption";
    sleepSecs = 120;
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

  # ephemeral-pool: two builds with DISTINCT markers. Same DAG-dedup
  # reasoning as pinDrv/recoveryDrv — the second build must be a fresh
  # derivation, not a cache hit, so reconcile_ephemeral's ClusterStatus
  # poll sees queued > 0 again and spawns a SECOND Job.
  ephemeralDrv1 = drvs.mkTrivial { marker = "lifecycle-ephemeral-1"; };
  ephemeralDrv2 = drvs.mkTrivial { marker = "lifecycle-ephemeral-2"; };

  # manifest-pool: one build, distinct marker. Cold-start path: this
  # derivation has no build_history entry → GetCapacityManifest omits
  # it → reconcile_manifest's cold_start count = queued_total -
  # manifest.len() = 1 → spawns ONE floor Job. No seeding needed.
  manifestDrv = drvs.mkTrivial { marker = "lifecycle-manifest-1"; };

  # gc-sweep's path_tenants proof. Distinct marker so DAG-dedup doesn't
  # reuse pinDrv/gcVictimDrv (those were built with the empty-comment
  # key → tenant_id=None → completion hook's filter_map drops → upsert
  # never fires). Fresh derivation = fresh build = completion runs.
  tenantDrv = drvs.mkTrivial { marker = "lifecycle-gc-tenant"; };

  # store-rollout: two builds, one before and one after a store
  # Deployment rollout restart. Distinct markers → distinct derivations
  # → each SubmitBuild triggers a fresh FindMissingPaths cache-check
  # against the scheduler's long-held store_client channel.
  rolloutPreDrv = drvs.mkTrivial { marker = "lifecycle-rollout-pre"; };
  rolloutPostDrv = drvs.mkTrivial { marker = "lifecycle-rollout-post"; };

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

    ${common.kvmCheck}
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

    # Negative-apply a deliberately-invalid BuilderPool spec. CRD CEL
    # rules (rio-crds/src/builderpool.rs x_kube validations) are
    # cross-field constraints that fire at kubectl-apply admission.
    # --dry-run=server sends to the apiserver (CEL evaluates) without
    # persisting. fail() asserts non-zero exit; the message-assert
    # proves it failed at the RIGHT rule — not, say, a schema error or
    # the wrong CEL rule. Quoted heredoc (<<'EOF') prevents shell
    # expansion inside the spec body.
    def assert_cel_rejects(name, spec_body, expected_msg):
        """spec_body is the YAML body UNDER `spec:` (2-space leading
        indent, no trailing newline on the last line). expected_msg
        is a substring of the CEL rule's .message() at builderpool.rs."""
        result = k3s_server.fail(
            "k3s kubectl apply --dry-run=server -f - 2>&1 <<'EOF'\n"
            "apiVersion: rio.build/v1alpha1\n"
            "kind: BuilderPool\n"
            f"metadata:\n  name: {name}\n  namespace: ${nsBuilders}\n"
            f"spec:\n{spec_body}\n"
            "EOF"
        )
        assert expected_msg in result, (
            f"CEL should reject {name!r} with {expected_msg!r} in the "
            f"message, got: {result!r}. If the apply succeeded or "
            f"failed for a different reason, the CEL rule at "
            f"rio-crds/src/builderpool.rs isn't in the deployed CRD — "
            f"check `helm template | grep x-kubernetes-validations`."
        )
        print(f"{name}: CEL rejected with {expected_msg!r} ✓")

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
            f"k3s kubectl -n ${nsStore} port-forward deploy/rio-store 19002:9002 "
            f">/dev/null 2>&1 & pf=$!; "
            f"trap 'kill $pf 2>/dev/null' EXIT; sleep 2; "
            f"${grpcurl} ${grpcurlTls} -max-time 30 "
            f"-protoset ${protoset}/rio.protoset "
            f"-d '{payload}' localhost:19002 {method} 2>&1"
        )

    # Port-allocation counter for submit_build_grpc. Each call gets
    # a unique local port (19100, 19101, …) to sidestep TIME_WAIT
    # contention — port-forward lacks SO_REUSEADDR, ~60s to rebind.
    # sched_grpc above uses 19001 (safe — single-call lifetime);
    # store_grpc uses 19002. SubmitBuild calls stack within one
    # subtest (build-timeout does submit→timeout→retry-submit).
    _submit_port = iter(range(19100, 19200))

    def submit_build_grpc(payload: dict, max_time: int = 5) -> str:
        """SubmitBuild via port-forward + grpcurl. Returns buildId.

        `payload` is the SubmitBuildRequest dict (json.dumps'd internally).
        `max_time` caps the stream read — build usually won't finish,
        grpcurl exits DeadlineExceeded; `|| true` swallows. The build
        is persisted on receipt; stream is observability only.

        Raises AssertionError if no BuildEvent JSON in output (submit
        failed before streaming) or first event lacks buildId.
        """
        port = next(_submit_port)
        leader = leader_pod()
        out = k3s_server.succeed(
            f"k3s kubectl -n ${ns} port-forward {leader} {port}:9001 "
            f">/dev/null 2>&1 & pf=$!; "
            f"trap 'kill $pf 2>/dev/null' EXIT; sleep 2; "
            f"${grpcurl} ${grpcurlTls} -max-time {max_time} "
            f"-protoset ${protoset}/rio.protoset "
            f"-d '{json.dumps(payload)}' "
            f"localhost:{port} rio.scheduler.SchedulerService/SubmitBuild "
            f"2>&1 || true"
        )
        brace = out.find("{")
        assert brace >= 0, (
            f"no JSON in SubmitBuild output — submit failed? got: {out[:500]!r}"
        )
        first_ev, _ = json.JSONDecoder().raw_decode(out, brace)
        build_id = first_ev.get("buildId", "")
        assert build_id, (
            f"first BuildEvent missing buildId; got: {first_ev!r}"
        )
        return build_id

    def grpcurl_json_stream(out: str) -> list[dict]:
        """Parse grpcurl's concatenated-JSON output (one pretty-printed
        object per stream message). Returns list of dicts. Empty input →
        empty list. Leading non-JSON (warnings, kubectl port-forward
        chatter from 2>&1) is skipped by seeking to the first `{`;
        inter-object gaps re-seek to the next `{`."""
        dec, objs = json.JSONDecoder(), []
        idx = out.find("{")
        while 0 <= idx < len(out):
            obj, idx = dec.raw_decode(out, idx)
            objs.append(obj)
            idx = out.find("{", idx)
        return objs

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
    ${common.mkBuildHelperV2 {
      gatewayHost = "k3s-server";
      dumpLogsExpr = ''dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")'';
    }}
  '';

  # ── Subtest fragments ─────────────────────────────────────────────────
  # Each fragment is a `with subtest(...)` block + its comment banner.
  # Fragments are composed by mkTest in the order given by `subtests`.
  # Python variables flow between fragments at module scope (no `with`
  # scoping) — but in the split architecture, all fragments are now
  # self-contained (gc-sweep builds its own paths; the old `initial`
  # seed-subtest is gone).
  fragments = {
    jwt-mount-present = ''
      # ══════════════════════════════════════════════════════════════════
      # jwt-mount-present — ConfigMap/Secret mounted, env var set
      # ══════════════════════════════════════════════════════════════════
      # Proves: with jwt.enabled=true, scheduler+store pods have the
      # rio-jwt-pubkey ConfigMap mounted at /etc/rio/jwt AND
      # RIO_JWT__KEY_PATH env set. Gateway has the rio-jwt-signing
      # Secret at /etc/rio/jwt.
      #
      # This closes the gap P0349 assumed was closed but wasn't: the
      # ConfigMap OBJECT existed (jwt-pubkey-configmap.yaml), the
      # volumeMount didn't. Without the mount, cfg.jwt.key_path stays
      # None → interceptor inert → every JWT passes unverified. Silent
      # fail-open when the operator thought jwt.enabled=true meant
      # enforcement.
      #
      # kubectl-get-jsonpath, NOT kubectl-exec. The rio-all image has
      # no coreutils (docker.nix baseContents = cacert+tzdata only —
      # minimal by design). `printenv`/`cat` aren't there. The Pod
      # spec (env + volumeMounts + volumes) IS the proof the Helm
      # template rendered the mount; K8s guarantees that if the Pod
      # is Running (waitReady proved this), the ConfigMap was mounted.
      # A bad mount = Pod stuck Pending with FailedMount event.
      #
      # The stronger proof — "scheduler successfully LOADED the key" —
      # is implicit: rio-scheduler/src/main.rs and rio-store/src/main.rs
      # call load_and_wire_jwt(...)? with `?` propagation. A bad key =
      # process exits non-zero = CrashLoopBackOff = waitReady never
      # returns. The prelude's waitReady already proved scheduler+store+
      # gateway are all Running, which means key-load succeeded in each.
      # (The gateway side loads the SIGNING seed, not the pubkey — same
      # fail-fast pattern via the same ?-propagation.)
      #
      # Precondition only: interceptor VERIFY behaviour is covered by
      # rust tests (jwt_interceptor.rs::tests); this proves the Helm
      # wiring → K8s Pod spec → container filesystem chain is intact.
      #
      # Tracey: r[verify sec.jwt.pubkey-mount] lives at the default.nix
      # subtests entry (P0341 convention — marker at wiring point, not
      # fragment header).
      with subtest("jwt-mount-present: scheduler+store+gateway have key mount + env"):
          # ── ConfigMap exists with content ─────────────────────────────
          # Template renders it, but WAS it applied? Was the content
          # non-empty? (empty → parse fails → scheduler CrashLoops →
          # waitReady catches that, but an explicit check is clearer).
          pubkey_cm = kubectl(
              "get cm rio-jwt-pubkey -o jsonpath='{.data.ed25519_pubkey}'"
          ).strip()
          assert len(pubkey_cm) >= 40, (
              f"rio-jwt-pubkey ConfigMap data too short: {pubkey_cm!r}. "
              f"32-byte key base64'd = 44 chars."
          )

          # ── Per-pod: env var + volumeMount + volume ───────────────────
          # Inspect the Pod spec (what K8s created from the Deployment
          # template). deploy/{name} resolves to the Deployment's Pod
          # template; `get pod -l ... -o jsonpath` reads a live pod.
          # Either proves the same thing; the pod spec is what the
          # kubelet actually used to build the container.
          for dep, dep_ns, vol_name, key_path in [
              ("rio-scheduler", "${ns}", "jwt-pubkey", "/etc/rio/jwt/ed25519_pubkey"),
              ("rio-store", "${nsStore}", "jwt-pubkey", "/etc/rio/jwt/ed25519_pubkey"),
              ("rio-gateway", "${ns}", "jwt-signing", "/etc/rio/jwt/ed25519_seed"),
          ]:
              # env: RIO_JWT__KEY_PATH set to the expected path.
              envs = kubectl(
                  f"get deploy {dep} -o jsonpath="
                  f"'{{.spec.template.spec.containers[0].env}}'",
                  ns=dep_ns,
              )
              assert key_path in envs and "RIO_JWT__KEY_PATH" in envs, (
                  f"{dep} missing RIO_JWT__KEY_PATH={key_path} in env "
                  f"spec: {envs!r}"
              )

              # volumeMounts: named mount at /etc/rio/jwt.
              mounts = kubectl(
                  f"get deploy {dep} -o jsonpath="
                  f"'{{.spec.template.spec.containers[0].volumeMounts}}'",
                  ns=dep_ns,
              )
              assert vol_name in mounts and "/etc/rio/jwt" in mounts, (
                  f"{dep} missing {vol_name} volumeMount at "
                  f"/etc/rio/jwt: {mounts!r}"
              )

              # volumes: entry exists. For scheduler/store it's a
              # configMap ref; gateway it's a secret ref. Just
              # checking the name — the ref type is template-defined
              # (_helpers.tpl), and helm-lint's yq check already
              # asserts the ref shape.
              vols = kubectl(
                  f"get deploy {dep} -o jsonpath="
                  f"'{{.spec.template.spec.volumes}}'",
                  ns=dep_ns,
              )
              assert vol_name in vols, (
                  f"{dep} missing {vol_name} volume: {vols!r}"
              )

          # ── Pods Running = key loaded successfully ────────────────────
          # waitReady already proved this, but make the inference
          # explicit. load_jwt_pubkey is fail-fast (`.await?` in
          # main.rs) — if the mounted key were invalid (bad base64,
          # wrong length, not a curve point), the process would exit
          # → CrashLoopBackOff → waitReady hangs. This subtest
          # reaching here = all three parsed their key OK.
          # ADR-019: store moved to rio-store namespace.
          for dep, dep_ns in [
              ("rio-scheduler", "${ns}"),
              ("rio-store", "${nsStore}"),
              ("rio-gateway", "${ns}"),
          ]:
              phase = kubectl(
                  f"get pods -l app.kubernetes.io/name={dep} "
                  f"-o jsonpath='{{.items[0].status.phase}}'",
                  ns=dep_ns,
              ).strip()
              assert phase == "Running", (
                  f"{dep} pod not Running (phase={phase!r}) — "
                  f"load_jwt_pubkey likely failed (bad mount content?)"
              )
          print(
              "jwt-mount-present PASS: all 3 deployments have "
              "RIO_JWT__KEY_PATH + mount + volume; pods Running "
              "= load_jwt_pubkey succeeded"
          )
    '';

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
          # the empty-string default. scheduler/main.rs at
          # r[impl ctrl.probe.named-service]: set_not_serving only
          # affects the named service; empty-string stays SERVING forever
          # after the first set_serving. This proves the CLIENT-SIDE
          # BALANCER constraint (the K8s readinessProbe is tcpSocket —
          # it doesn't probe gRPC health). This probe with `-service ...`
          # AND the NOT_SERVING result together prove the named-service
          # gate.
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

          # SubmitBuild via gRPC — submit_build_grpc handles port-forward
          # + mTLS + grpcurl + JSON-parse + buildId-extract. Port is
          # auto-allocated from the 19100+ iterator (TIME_WAIT-safe).
          #
          # DerivationNode requires drvHash + system (scheduler grpc/mod.rs
          # :379-407 validates). drvHash = drvPath for input-addressed
          # derivations (gateway translate.rs:361 does the same). system
          # is the VM platform. outputNames = ["out"] — mkTrivial's single
          # output. The gateway normally parses the .drv to populate
          # these; we're bypassing that, so we fill them statically.
          build_id = submit_build_grpc({
              "nodes": [{
                  "drvPath": drv_path,
                  "drvHash": drv_path,
                  "system": "${pkgs.stdenv.hostPlatform.system}",
                  "outputNames": ["out"],
              }],
              "edges": [],
          })
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
              "k3s kubectl -n ${nsBuilders} get pod default-builders-0 "
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
                  "k3s kubectl -n ${nsBuilders} logs default-builders-0 --since=2m "
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

    build-timeout = ''
      # ══════════════════════════════════════════════════════════════════
      # build-timeout — gRPC buildTimeout < sleep → TimedOut, cgroup cleaned
      # ══════════════════════════════════════════════════════════════════
      # Post-P0294: no Build CR. Submit via gRPC SubmitBuild with
      # buildTimeout=5 directly. The value flows two places:
      #   (1) scheduler per-build timeout (actor/worker.rs:597) — checked
      #       on Tick (10s here), fires cancel_build_derivations
      #   (2) worker per-derivation daemon timeout (executor/mod.rs:567 →
      #       stderr_loop.rs:126 tokio::time::timeout) — fires TimedOut
      # Either way run_daemon_build returns (Ok(TimedOut) or Err on
      # cancel-killed daemon), and the executor FALLS THROUGH to line
      # 764: build_cgroup.kill() + drain + Drop rmdirs. THAT is the
      # kill-on-teardown path under test.
      #
      # This is DISTINCT from cancel-cgroup-kill above: cancel-cgroup-kill
      # tests runtime.rs try_cancel_build (explicit CancelBuild RPC).
      # build-timeout tests executor/mod.rs:764 (post-daemon teardown).
      # Both write cgroup.kill; different call sites, different r[impl].
      #
      # sleepSecs=30 vs buildTimeout=5: wide gap for TCG dispatch lag.
      # Under TCG the timeout may fire at ~8-12s wall-clock; sleep is
      # nowhere near done. Narrower gaps flake.
      with subtest("build-timeout: gRPC buildTimeout < sleep → cgroup cleaned, no EEXIST"):
          drv_path = client.succeed(
              "nix-instantiate "
              "--arg busybox '(builtins.storePath ${common.busybox})' "
              "${timeoutDrv} 2>/dev/null"
          ).strip()
          client.succeed(
              f"nix copy --derivation --to 'ssh-ng://k3s-server' {drv_path}"
          )

          # SubmitBuild via gRPC — submit_build_grpc handles the
          # port-forward + mTLS + JSON-parse + buildId-extract
          # boilerplate. buildTimeout is SubmitBuildRequest field 6
          # (types.proto:655, camelCase for grpcurl). max_time=3 caps
          # the stream read well under buildTimeout=5 so we always
          # capture the first BuildEvent before timeout races in.
          build_id = submit_build_grpc({
              "nodes": [{
                  "drvPath": drv_path,
                  "drvHash": drv_path,
                  "system": "${pkgs.stdenv.hostPlatform.system}",
                  "outputNames": ["out"],
              }],
              "edges": [],
              "buildTimeout": 5,
          }, max_time=3)
          print(f"build-timeout: submitted, build_id={build_id}")

          # ── Assertion 1: cgroup appeared + non-empty (precondition). ──
          # Without this, the cgroup-gone assert below proves nothing
          # (could vanish for any reason). sanitize_build_id: basename
          # with . → _, so cgroup dir ends "lifecycle-timeout_drv".
          # Probe from VM host (worker pod is distroless, no `find`).
          # `| grep .` fails on empty (find exits 0 on no-match) so
          # wait_until_succeeds retries.
          worker_node = k3s_server.succeed(
              "k3s kubectl -n ${nsBuilders} get pod default-builders-0 "
              "-o jsonpath='{.spec.nodeName}'"
          ).strip()
          worker_vm = k3s_agent if worker_node == "k3s-agent" else k3s_server
          cgroup_path = worker_vm.wait_until_succeeds(
              "find /sys/fs/cgroup -type d -name '*lifecycle-timeout_drv' "
              "-print -quit 2>/dev/null | grep .",
              timeout=120,
          ).strip()
          procs = int(worker_vm.succeed(
              f"wc -l < {cgroup_path}/cgroup.procs"
          ).strip())
          assert procs > 0, (
              f"cgroup.procs empty ({cgroup_path}) — build not actually "
              f"running in the cgroup; kill-on-teardown assert vacuous"
          )
          print(f"build-timeout: cgroup={cgroup_path}, procs={procs}")

          # ── Assertion 2: cgroup GONE (kill-on-teardown fired). ──────
          # Kernel rejects rmdir on non-empty cgroup (EBUSY), so gone ⇒
          # procs drained ⇒ cgroup.kill fired. Without the teardown fix,
          # this would EBUSY-leak: the sleep-30 builder is a grandchild
          # (nix-daemon forked it), daemon.kill() doesn't reach it, and
          # only ~10-20s have elapsed (sleep not done). The explicit
          # build_cgroup.kill() + drain-poll at executor/mod.rs:764-796
          # is what makes rmdir succeed.
          #
          # timeout=60: buildTimeout=5 + 10s scheduler tick granularity
          # + daemon-teardown latency + TCG headroom.
          try:
              worker_vm.wait_until_succeeds(
                  f"! test -e {cgroup_path}",
                  timeout=60,
              )
          except Exception:
              procs_after = worker_vm.succeed(
                  f"cat {cgroup_path}/cgroup.procs 2>/dev/null | wc -l "
                  f"|| echo gone"
              ).strip()
              k3s_server.execute(
                  "echo '=== DIAG: worker logs (last 2m, non-DEBUG) ===' >&2; "
                  "k3s kubectl -n ${nsBuilders} logs default-builders-0 --since=2m "
                  "  | grep -vE '\"level\":\"DEBUG\"' | tail -40 >&2 || true"
              )
              print(f"build-timeout DIAG: procs_after={procs_after} "
                    f"(was {procs}), build_id={build_id}")
              raise
          print(f"build-timeout: cgroup {cgroup_path} removed "
                f"(builder killed, rmdir succeeded)")

          # ── Assertion 3: timeout observed (no-reassign). ─────────────
          # Either scheduler's per-build timeout or worker's daemon
          # timeout won the race — both are terminal-no-reassign. The
          # scheduler metric rio_scheduler_build_timeouts_total is the
          # less racy check: it increments when actor/worker.rs:606
          # fires. With tick=10s and buildTimeout=5, it will fire by
          # T+~15s (first tick where elapsed > 5) unless the build
          # already reached a terminal state via the worker path (in
          # which case the worker reported BuildResultStatus::TimedOut,
          # which is also permanent-no-reassign per types.proto:278).
          # We check EITHER incremented — both prove no-reassign.
          m = sched_metrics()
          sched_timeouts = metric_value(
              m, "rio_scheduler_build_timeouts_total"
          ) or 0.0
          # 9093 = worker metrics port (config.rs:163, builders.rs:742).
          # 9091 is the SCHEDULER's — original test copy-pasted the wrong port.
          worker_metrics = k3s_server.succeed(
              "k3s kubectl -n ${ns} get --raw "
              "/api/v1/namespaces/${nsBuilders}/pods/default-builders-0:9093/proxy/metrics"
          )
          timed_out_line = [
              l for l in worker_metrics.splitlines()
              if 'rio_builder_builds_total' in l
              and 'outcome="timed_out"' in l
              and not l.startswith('#')
          ]
          worker_timed_out = (
              float(timed_out_line[0].rsplit(' ', 1)[1])
              if timed_out_line else 0.0
          )
          assert sched_timeouts >= 1 or worker_timed_out >= 1, (
              f"neither scheduler_build_timeouts_total "
              f"({sched_timeouts}) nor worker outcome=timed_out "
              f"({worker_timed_out}) incremented — timeout didn't fire?"
          )
          print(f"build-timeout: sched_timeouts={sched_timeouts}, "
                f"worker_timed_out={worker_timed_out}")

          # ── Assertion 4: same drv, second build succeeds. ────────────
          # Proves the leak really is closed, not just "rmdir warned".
          # Without kill-on-teardown: BuildCgroup::create → mkdir →
          # EEXIST (leaked cgroup from attempt 1 still has the sleep-30
          # process). With the fix: clean slate.
          #
          # No buildTimeout this time — let the 30s sleep complete.
          # submit_build_grpc handles port-allocation + swallow-
          # DeadlineExceeded. We don't need completion — just successful
          # re-dispatch + cgroup recreation (no EEXIST). The helper
          # asserts buildId was returned; EEXIST would surface before
          # any BuildEvent and fail that assert with the error output.
          #
          # DEPENDS ON: scheduler re-dispatching terminal-state drvs.
          # The DAG node goes terminal after TimedOut; resubmit must
          # clear and re-queue. fix-resubmit-terminal lands that path.
          submit_build_grpc({
              "nodes": [{
                  "drvPath": drv_path,
                  "drvHash": drv_path,
                  "system": "${pkgs.stdenv.hostPlatform.system}",
                  "outputNames": ["out"],
              }],
              "edges": [],
          }, max_time=3)
          # Cgroup reappeared — concrete proof no EEXIST in
          # BuildCgroup::create. Same dirname (same drv_path → same
          # sanitize_build_id). `| grep .` so wait_until_succeeds retries.
          cgroup_retry = worker_vm.wait_until_succeeds(
              "find /sys/fs/cgroup -type d -name '*lifecycle-timeout_drv' "
              "-print -quit 2>/dev/null | grep .",
              timeout=120,
          ).strip()
          print(f"build-timeout PASS: same-drv re-dispatched, "
                f"cgroup recreated at {cgroup_retry} (no EEXIST leak)")
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

          # Standby acquires. Lease holderIdentity becomes a DIFFERENT,
          # NON-EMPTY pod name. 60s timeout: lease TTL + acquire tick
          # (~5s poll). Two transient states to reject:
          #   (a) holderIdentity stays old name until lease expires
          #       (so != check, not just -n)
          #   (b) under KVM, --grace-period=0 --force deletes the pod so
          #       fast that holderIdentity is briefly EMPTY before the
          #       standby claims it (observed: 0.2s window) — without
          #       the -n guard, "" != old_leader is trivially true and
          #       new_leader below captures the empty string.
          k3s_server.wait_until_succeeds(
              "h=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
              "-o jsonpath='{.spec.holderIdentity}') && "
              f"test -n \"$h\" && test \"$h\" != '{old_leader}'",
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
          # ~60s sleep remainder + re-dispatch overhead after failover +
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
          # by any error like "failed to delete, path unknown".
          gc_msgs = grpcurl_json_stream(result)
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
          kubectl("scale statefulset default-builders --replicas=2", ns="${nsBuilders}")

          # Reconciler observed the change (via .owns watch), reconciled,
          # patched BuilderPool.status.desiredReplicas. This IS the
          # reconcile — if it were going to stomp replicas, it would have
          # done so in the same apply() call. Accept 1 OR 2: with the 10s
          # scale-down window (controller.extraEnv[3]), the autoscaler may
          # have already patched 2→1 before reconcile fires. Either way,
          # desiredReplicas reflecting a NON-STALE value proves reconcile
          # ran after our scale.
          k3s_server.wait_until_succeeds(
              "dr=$(k3s kubectl -n ${nsBuilders} get builderpool default "
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
              "! k3s kubectl -n ${nsBuilders} get statefulset default-builders -o yaml | "
              "grep -A50 'manager: rio-controller' | "
              "grep -B50 -m1 '^  - apiVersion\\|^status:' | "
              "grep -q 'f:replicas'"
          )

          # Reset to 1 so autoscaler observes 1→2 (not 2→2 no-op).
          kubectl("scale statefulset default-builders --replicas=1", ns="${nsBuilders}")
          k3s_server.wait_until_succeeds(
              "test \"$(k3s kubectl -n ${nsBuilders} get builderpool default "
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
      # BuilderPool.status.lastScaleTime via its SEPARATE field-manager.
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
              "test \"$(k3s kubectl -n ${nsBuilders} get statefulset default-builders "
              "-o jsonpath='{.spec.replicas}')\" = 2",
              timeout=60,
          )

          # BuilderPool.status.lastScaleTime set (RFC3339 string, non-
          # empty). Owned by the autoscaler's SSA field-manager (distinct
          # from the reconciler's — otherwise reconcile would clobber it
          # to None every cycle).
          k3s_server.wait_until_succeeds(
              "sc=$(k3s kubectl -n ${nsBuilders} get builderpool default "
              "-o jsonpath='{.status.lastScaleTime}'); "
              "test -n \"$sc\"",
              timeout=20,
          )

          # Scaling condition explains WHY replicas changed.
          kubectl(
              "get builderpool default "
              "-o jsonpath='{.status.conditions[?(@.type==\"Scaling\")].reason}' | "
              "grep -q ScaledUp",
              ns="${nsBuilders}",
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
              "test \"$(k3s kubectl -n ${nsBuilders} get statefulset default-builders "
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

          # ScaledDown K8s Event on the BuilderPool. The autoscaler's
          # recorder.publish path for Direction::Down (scaling.rs:359).
          events_down = kubectl(
              "get events "
              "--field-selector involvedObject.name=default,involvedObject.kind=BuilderPool,reason=ScaledDown "
              "-o name",
              ns="${nsBuilders}",
          ).strip()
          assert events_down, (
              "expected ScaledDown K8s Event on BuilderPool/default, got none"
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
          # about the loop body.
          gc_msgs = grpcurl_json_stream(result)
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
          # ── path_tenants end-to-end: tenant-key build → upsert fires ──
          # Proves the completion hook (completion.rs r[impl sched.gc.
          # path-tenants-upsert]) fires end-to-end in the k3s fixture:
          # SSH key comment → gateway tenant_name → scheduler resolves
          # UUID → builds.tenant_id → completion filter_map YIELDS →
          # upsert_path_tenants INSERTs.
          #
          # Also proves hash-encoding compat: scheduler writes
          # sha2::Sha256::digest(path.as_bytes()) (db.rs:650, raw
          # 32-byte Vec<u8> → BYTEA); this query reads
          # sha256(convert_to(path, 'UTF8')) (PG builtin, raw 32-byte
          # bytea). Same input bytes → same digest → same BYTEA. If
          # either side hex-encoded, this would be 0 forever.
          #
          # The earlier builds (out_pin/out_victim) used the default
          # key (empty comment → tenant_id=None → upsert skipped) —
          # path_tenants is currently empty. We set up a tenant key
          # now, bounce the gateway to load it, and do ONE build.

          # Tenant key with non-empty comment. Gateway's
          # load_authorized_keys parses the comment as tenant_name.
          client.succeed(
              "ssh-keygen -t ed25519 -N ''' -C 'gc-tenant-test' "
              "-f /root/.ssh/id_gc_tenant"
          )
          default_pub = client.succeed("cat /root/.ssh/id_ed25519.pub").strip()
          tenant_pub = client.succeed("cat /root/.ssh/id_gc_tenant.pub").strip()

          # Re-patch the Secret with BOTH keys. The default key must
          # stay — refs-end-to-end runs next and uses build() (default
          # key). printf '%s\n%s\n' writes both on separate lines;
          # pubkeys are base64+space+comment, no single-quotes → safe
          # in shell single-quotes.
          k3s_server.succeed(
              f"printf '%s\n%s\n' '{default_pub}' '{tenant_pub}' "
              f"> /tmp/ak_both && "
              "k3s kubectl -n ${ns} create secret generic rio-gateway-ssh "
              "--from-file=authorized_keys=/tmp/ak_both "
              "--dry-run=client -o yaml | k3s kubectl apply -f -"
          )

          # Scale-bounce gateway 0→1 — see fixture.bounceGatewayForSecret
          # (k3s-full.nix) for the SecretManager reflector-refcount
          # rationale. Inlined here (not interpolated via nix) because the
          # fixture is written for 4-space Python indent (sshKeySetup's
          # level) and nix heredoc-strip doesn't re-indent on
          # interpolation — splicing at 10-space indent breaks Python.
          k3s_server.succeed(
              "k3s kubectl -n ${ns} scale deploy/rio-gateway --replicas=0"
          )
          k3s_server.wait_until_succeeds(
              "! k3s kubectl -n ${ns} get pods "
              "-l app.kubernetes.io/name=rio-gateway "
              "--no-headers 2>/dev/null | grep -q .",
              timeout=90,
          )
          k3s_server.succeed(
              "k3s kubectl -n ${ns} scale deploy/rio-gateway --replicas=1"
          )
          k3s_server.wait_until_succeeds(
              "k3s kubectl -n ${ns} rollout status deploy/rio-gateway --timeout=60s",
              timeout=90,
          )
          # kube-proxy endpoint sync lag — poll SSH banner. Same
          # rationale as fixture.sshKeySetup (k3s-full.nix): nc -z
          # only proves kube-proxy has a DNAT rule, not that the
          # gateway's SSH accept loop is responding end-to-end.
          # Banner grep proves the full chain. `|| true` guards
          # pipefail against nc's idle-timeout exit.
          client.wait_until_succeeds(
              "(${pkgs.netcat}/bin/nc -w2 k3s-server 32222 "
              "</dev/null 2>&1 || true) | grep -q ^SSH-",
              timeout=30,
          )

          # Seed tenant row (FK: path_tenants.tenant_id →
          # tenants.tenant_id). INSERT…RETURNING via psql_k8s (-qtA).
          tenant_uuid = psql_k8s(k3s_server,
              "INSERT INTO tenants (tenant_name) VALUES ('gc-tenant-test') "
              "RETURNING tenant_id"
          )
          k3s_server.log(f"path_tenants: seeded tenant gc-tenant-test = {tenant_uuid}")

          # SSH Host alias for the tenant key. common.nix:362: ?ssh-key=
          # URL param is unreliable across Nix versions; the Host alias
          # in /root/.ssh/config overrides IdentityFile while inheriting
          # nothing (explicit User/Port). programs.ssh.extraConfig went
          # to /etc/ssh/ssh_config (system); per-user config wins.
          #
          # IdentitiesOnly yes: without it ssh offers ~/.ssh/id_ed25519
          # (default key, empty comment) FIRST, gateway accepts THAT →
          # tenant_id=None → upsert never fires. The build succeeds
          # silently with the wrong identity.
          client.succeed(
              "cat >> /root/.ssh/config << 'EOF'\n"
              "Host k3s-server-tenant\n"
              "  HostName k3s-server\n"
              "  User rio\n"
              "  Port 32222\n"
              "  IdentityFile /root/.ssh/id_gc_tenant\n"
              "  IdentitiesOnly yes\n"
              "  StrictHostKeyChecking no\n"
              "  UserKnownHostsFile /dev/null\n"
              "EOF"
          )

          # Build tenantDrv via the tenant-key alias. Fresh marker →
          # fresh derivation → fresh build (no DAG-dedup). Completion
          # sees tenant_id=Some(uuid) → upsert fires. store_url routes
          # through the k3s-server-tenant ssh_config alias (tenant key
          # via IdentitiesOnly); strip_to_store_path (the default under
          # capture_stderr=True) skips SSH known_hosts warnings.
          out_tenant = build("${tenantDrv}",
                             store_url="ssh-ng://k3s-server-tenant")
          assert out_tenant.startswith("/nix/store/"), (
              f"tenant-key build should produce store path: {out_tenant!r}"
          )
          assert "lifecycle-gc-tenant" in out_tenant, (
              f"wrong drv (DAG-dedup?): {out_tenant!r}"
          )

          # THE assertion. ≥1 not ==1: composite PK is (hash, tenant),
          # and this is the only tenant in PG, so it's exactly 1 in
          # practice. ≥1 per plan-0206 T5 spec — future test paths
          # sharing this output shouldn't break us. Query matches on
          # BOTH hash and tenant_id to prove the row is OURS (not a
          # stray from some other tenant).
          pt_count = int(psql_k8s(k3s_server,
              f"SELECT COUNT(*) FROM path_tenants "
              f"WHERE store_path_hash = sha256(convert_to('{out_tenant}', 'UTF8')) "
              f"AND tenant_id = '{tenant_uuid}'"
          ))
          assert pt_count >= 1, (
              f"path_tenants should have >=1 row for out_tenant after "
              f"tenant-key build (completion hook fires upsert); got "
              f"{pt_count}. Check: gateway loaded id_gc_tenant? scheduler "
              f"resolved 'gc-tenant-test'? completion.rs filter_map yielded?"
          )
          print(f"path_tenants PASS: {pt_count} row(s) for {out_tenant} "
                f"tenant={tenant_uuid} — completion hook + hash compat proven")

          # ── tenant retention extends global grace (mark CTE seed f) ──
          # Reuses out_tenant + tenant_uuid from the upsert proof above.
          # tenant row at :1232 used DEFAULT gc_retention_hours=168 (7d).
          # out_tenant is mkTrivial → refs=[] → no transitive protection.
          # path_tenants.first_referenced_at is ~now() (just upserted).
          #
          # Two-phase proof:
          #   1. CONTROL (survives): backdate narinfo.created_at past 24h
          #      grace. first_referenced_at stays at ~now() → inside 168h
          #      → seed (f) protects → pathsCollected=0.
          #   2. EXPIRED (swept): backdate first_referenced_at past 168h
          #      too. Both windows expired → seed (f) no longer fires →
          #      pathsCollected=1.
          #
          # The survival case IS the spec assertion: tenant retention
          # EXTENDS global grace. Without seed (f), step 1 would collect
          # out_tenant (past grace, no pin, no refs, no other seed).

          # Backdate narinfo past grace. 25h past a 24h grace.
          # first_referenced_at stays fresh — the completion hook wrote
          # it moments ago.
          psql_k8s(k3s_server,
              f"UPDATE narinfo SET created_at = now() - interval '25 hours' "
              f"WHERE store_path = '{out_tenant}'"
          )

          # TriggerGC grace=24h. out_tenant is past global grace but
          # inside tenant retention. Everything else (out_pin, busybox
          # seed, etc.) is still in-grace (built this run, created_at
          # ~now()). So the unreachable set is {} — IF seed (f) works.
          # Without seed (f): unreachable={out_tenant}, pathsCollected=1
          # → nix path-info fails → THIS assertion catches it.
          def trigger_gc_get_collected():
              out = sched_grpc(
                  '{"dry_run": false, "grace_period_hours": 24, "force": true}',
                  "rio.admin.AdminService/TriggerGC",
              )
              msgs = grpcurl_json_stream(out)
              finals = [m for m in msgs if m.get("isComplete")]
              assert finals, f"no isComplete in GC stream: {out[:500]}"
              # proto3 omits zero-value uint64; .get → "0" default.
              return finals[-1].get("pathsCollected", "0")

          collected_control = trigger_gc_get_collected()
          assert collected_control == "0", (
              f"tenant retention CONTROL: out_tenant past global grace "
              f"but inside 168h tenant window — seed (f) must protect it. "
              f"Got pathsCollected={collected_control!r}. If this is '1', "
              f"mark CTE seed (f) isn't firing — check path_tenants JOIN."
          )
          # Belt-and-suspenders: path still resolvable.
          client.succeed(
              f"nix path-info --store 'ssh-ng://k3s-server' {out_tenant}"
          )
          print("tenant-retention CONTROL PASS: out_tenant past global "
                "grace but inside tenant window → survived sweep")

          # Now expire the tenant window too. 200h > 168h retention.
          # Match on BOTH hash and tenant_id — same specificity as the
          # upsert-proof query at :1290 (defensive against future test
          # paths sharing this output via a different tenant).
          psql_k8s(k3s_server,
              f"UPDATE path_tenants "
              f"SET first_referenced_at = now() - interval '200 hours' "
              f"WHERE store_path_hash = sha256(convert_to('{out_tenant}', 'UTF8')) "
              f"AND tenant_id = '{tenant_uuid}'"
          )

          # Both windows expired. No pin, no refs, no other seed →
          # out_tenant is the sole unreachable path → pathsCollected=1.
          collected_expired = trigger_gc_get_collected()
          assert collected_expired == "1", (
              f"tenant retention EXPIRED: both global grace AND tenant "
              f"window expired — out_tenant must be swept. Got "
              f"pathsCollected={collected_expired!r}. If '0', seed (f) "
              f"WHERE clause may be inverted or retention default drifted."
          )
          client.fail(
              f"nix path-info --store 'ssh-ng://k3s-server' {out_tenant}"
          )
          print("tenant-retention EXPIRED PASS: both windows expired → "
                "out_tenant swept")

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
          gc_msgs = grpcurl_json_stream(result)
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
          gc_msgs = grpcurl_json_stream(result)
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

    ephemeral-pool = ''
      # ══════════════════════════════════════════════════════════════════
      # ephemeral-pool — WorkerPoolSpec.ephemeral=true → no STS, Job/build
      # ══════════════════════════════════════════════════════════════════
      # REQUIRES: no STS workers alive (run AFTER finalizer, which
      # deletes the default pool). Otherwise the STS worker picks up
      # dispatches before reconcile_ephemeral's 10s tick spawns a Job.
      #
      # Proves end-to-end:
      #   - apply() branches on spec.ephemeral (mod.rs:118-132) — no STS
      #   - reconcile_ephemeral polls ClusterStatus + spawns Jobs when
      #     queued > 0 (ephemeral.rs:107-215)
      #   - Job pod has RIO_EPHEMERAL=1 → worker exits after one build
      #     (main.rs single-shot gate)
      #   - pod terminates → Job Complete → ttlSecondsAfterFinished reaps
      #   - second build → fresh Job (zero cross-build state)
      #
      # NOT proven here: the actual isolation property (tenant A can't
      # poison tenant B's cache). That'd need two tenants building
      # overlapping closures with one malicious. The "fresh pod = fresh
      # emptyDir" property is structural — K8s guarantees it.
      with subtest("ephemeral-pool: no STS, Job spawned, pod reaped, second build = new Job"):
          # Precondition: no STS workers. The finalizer fragment (run
          # before this) deletes the default pool. 90s: finalizer drain
          # + scheduler disconnect-detect + metric-update can lag under
          # KVM-speed test ordering or TCG slowness.
          sched_metric_wait(
              "grep -qx 'rio_scheduler_workers_active 0'",
              timeout=90,
          )

          # ── CEL: ephemeral + maxConcurrentBuilds>1 rejected ───────────
          # ctrl.pool.ephemeral-single-build — one-pod-per-build isolation
          # breaks if a pod runs N builds (shared FUSE cache + overlayfs upper).
          assert_cel_rejects(
              "ephemeral-bad-maxbuilds",
              "  ephemeral: true\n"
              "  replicas: {min: 0, max: 4}\n"
              "  autoscaling: {metric: queueDepth, targetValue: 2}\n"
              "  maxConcurrentBuilds: 4\n"
              "  fuseCacheSize: 5Gi\n"
              "  systems: [x86_64-linux]\n"
              "  image: rio-all",
              "maxConcurrentBuilds==1",
          )

          # ── CEL: ephemeralDeadlineSeconds without ephemeral rejected ──
          # ctrl.pool.ephemeral-deadline — field tunes Job's
          # activeDeadlineSeconds; STS pools have no Jobs.
          assert_cel_rejects(
              "sts-with-deadline",
              "  ephemeral: false\n"
              "  ephemeralDeadlineSeconds: 7200\n"
              "  replicas: {min: 1, max: 4}\n"
              "  autoscaling: {metric: queueDepth, targetValue: 2}\n"
              "  maxConcurrentBuilds: 1\n"
              "  fuseCacheSize: 5Gi\n"
              "  systems: [x86_64-linux]\n"
              "  image: rio-all",
              "ephemeralDeadlineSeconds",
          )

          # ── CEL: hostNetwork without privileged rejected ──────────────
          # ctrl.crd.host-users-network-exclusive — K8s rejects
          # hostUsers:false with hostNetwork:true at admission; the
          # non-privileged path sets hostUsers:false (ADR-012).
          assert_cel_rejects(
              "hostnet-unprivileged",
              "  hostNetwork: true\n"
              "  replicas: {min: 0, max: 4}\n"
              "  autoscaling: {metric: queueDepth, targetValue: 2}\n"
              "  maxConcurrentBuilds: 1\n"
              "  fuseCacheSize: 5Gi\n"
              "  systems: [x86_64-linux]\n"
              "  image: rio-all",
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
              "  ephemeral: true\n"
              "  replicas: {min: 0, max: 4}\n"
              "  autoscaling: {metric: queueDepth, targetValue: 2}\n"
              "  maxConcurrentBuilds: 1\n"
              "  fuseCacheSize: 5Gi\n"
              "  systems: [x86_64-linux]\n"
              # rio-all:dev — MUST match the tag from nix/docker.nix
              # (tag = "dev"). Bare "rio-all" normalizes to :latest which
              # the airgap import doesn't have → ErrImageNeverPull. The
              # Helm-rendered default pool gets this via the rio.image
              # helper (global.image.tag); inline YAML here must spell it
              # out.
              "  image: rio-all:dev\n"
              "  imagePullPolicy: Never\n"
              # tlsSecretName: vmtest-full.yaml sets tls.enabled=true, so
              # the scheduler requires mTLS on its gRPC port. The Helm-
              # rendered default pool gets this via `{{- if .Values.tls.
              # enabled }} tlsSecretName: rio-builder-tls {{- end }}`
              # (templates/builderpool.yaml:37-39); inline YAML here must
              # spell it out. Without it, builders.rs skips the RIO_TLS__*
              # env + tls volume → ephemeral worker connects plaintext →
              # TLS handshake fails → never heartbeats → build stuck
              # queued forever.
              "  tlsSecretName: rio-builder-tls\n"
              "  privileged: true\n"
              "  terminationGracePeriodSeconds: 60\n"
              "  nodeSelector: null\n"
              "  tolerations: null\n"
              "EOF"
          )

          # ── No StatefulSet ────────────────────────────────────────────
          # The reconciler's apply() branches on spec.ephemeral BEFORE
          # the STS/Service/PDB block. Give it one reconcile tick (CRD
          # watch fires immediately on create), then assert. If an STS
          # ever appears for this pool, the branch didn't fire.
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
              timeout=120,
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

          # RIO_EPHEMERAL=1 on the Job's pod spec. This is the load-
          # bearing env var: without it the worker loops forever.
          # jsonpath into Job.spec.template (not pod — pod name is
          # Job-generated, less stable for the query).
          eph_env = k3s_server.succeed(
              f"k3s kubectl -n ${nsBuilders} get job {job1} "
              "-o jsonpath='{.spec.template.spec.containers[0].env[?(@.name==\"RIO_EPHEMERAL\")].value}'"
          ).strip()
          assert eph_env == "1", (
              f"RIO_EPHEMERAL must be '1' on ephemeral Job pod; got "
              f"{eph_env!r}. Without it, worker never exits → Job never "
              f"completes → pod leaked."
          )

          # nix-build completes. 120s: Job pod schedule (~5s) + container
          # pull from local registry (~2s, image already loaded) + FUSE
          # mount + cgroup (~5s) + heartbeat accepted (~10s tick) +
          # dispatch + build (mkTrivial ~1s) + CompletionReport + worker
          # exit. ~30-40s typical; 120s margin.
          client.wait_until_succeeds(
              "! kill -0 $(cat /tmp/eph1.pid) 2>/dev/null",
              timeout=120,
          )
          out1 = client.succeed("cat /tmp/eph1.out").strip()
          assert "/nix/store/" in out1, (
              f"build 1 should have produced a store path, got: {out1!r}"
          )

          # Pod goes Succeeded (worker exited 0 after its one build).
          # Filter by job-name label: .items[0] on all pods would pick
          # whichever sorts first (possibly a newer Job's pod if the
          # reconciler spawned another). Filter to job1's pod specifically.
          k3s_server.wait_until_succeeds(
              "test \"$(k3s kubectl -n ${nsBuilders} get pods "
              f"-l job-name={job1} "
              "-o jsonpath='{.items[0].status.phase}')\" = Succeeded",
              timeout=120,
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
          # ttlSecondsAfterFinished=60 may not have reaped job1 yet
          # (depends on K8s TTL controller tick). That's fine — the
          # ASSERTION is that a NEW Job appears, not that job1 is gone.
          # Count Jobs before build 2, expect +1 after.
          jobs_before = int(k3s_server.succeed(
              "k3s kubectl -n ${nsBuilders} get jobs "
              "-l rio.build/pool=ephemeral -o name | wc -l"
          ).strip())
          # Precondition self-assert BEFORE the check it guards:
          # jobs_before must be ≥1, otherwise the >= check below is
          # vacuous (0 >= 0 passes even if nothing happened). Firing
          # this BEFORE build 2 means failure messages point at the
          # right problem (build 1 never spawned) instead of the
          # wrong one (build 2 comparison).
          assert jobs_before >= 1, (
              f"PRECONDITION: jobs_before must be ≥1 (build 1 should "
              f"have spawned one); got {jobs_before}. If 0, build 1 "
              f"never reached the Job-spawn path."
          )

          # Foreground build this time — we don't need to observe
          # mid-flight state, just that it completes.
          out2 = build("${ephemeralDrv2}")
          assert "/nix/store/" in out2, (
              f"build 2 should have produced a store path, got: {out2!r}"
          )

          jobs_after = int(k3s_server.succeed(
              "k3s kubectl -n ${nsBuilders} get jobs "
              "-l rio.build/pool=ephemeral -o name | wc -l"
          ).strip())
          # >= not ==: job1 might have been TTL-reaped during build 2
          # (60s ttl, build 2 takes ~30-40s). jobs_after >= jobs_before
          # proves a NEW Job was created (if job1 was reaped AND no new
          # Job, count would DROP).
          #
          # Alternative assertion (stricter, but timing-sensitive):
          # fetch all Job names, assert the set grew. This weaker form
          # is robust to TTL-reap timing.
          assert jobs_after >= jobs_before, (
              f"expected ≥{jobs_before} Jobs after build 2 (new Job "
              f"spawned, possibly old one reaped), got {jobs_after}. "
              f"If jobs_after < jobs_before, build 2 was served by a "
              f"REUSED pod — ephemeral mode is broken."
          )

          # ── Cleanup ───────────────────────────────────────────────────
          # Delete the ephemeral pool. cleanup() branches on
          # spec.ephemeral and returns immediately (no STS scale-to-0,
          # no DrainWorker loop). In-flight Jobs finish naturally;
          # ownerRef GC deletes them.
          kubectl("delete builderpool ephemeral --wait=false", ns="${nsBuilders}")
          # CR gone quickly — finalizer removed on first cleanup() call.
          # 30s is generous; should be <5s in practice.
          k3s_server.wait_until_succeeds(
              "! k3s kubectl -n ${nsBuilders} get builderpool ephemeral 2>/dev/null",
              timeout=30,
          )
          print("ephemeral-pool PASS: no STS, Job spawned per build, "
                "pod Succeeded, fresh Job for build 2")
    '';

    manifest-pool = ''
      # ══════════════════════════════════════════════════════════════════
      # manifest-pool — BuilderPool.spec.sizing=Manifest → per-bucket Jobs
      # ══════════════════════════════════════════════════════════════════
      # REQUIRES: no STS workers alive (run AFTER ephemeral-pool, which
      # deletes its own pool; finalizer already deleted the default pool
      # earlier in the chain). Same reason as ephemeral-pool: an STS
      # worker picks up the dispatch before reconcile_manifest's 10s
      # tick spawns a Job, leaving queued=0 → cold_start=0 → no spawn.
      #
      # Proves end-to-end what manifest_tests.rs CAN'T (pure-function
      # only — no apiserver):
      #   - apply() branches on spec.sizing=Manifest (mod.rs:361) — no
      #     STS, dispatches to reconcile_manifest
      #   - reconcile_manifest polls GetCapacityManifest + ClusterStatus,
      #     computes cold_start deficit, spawns a Job (manifest.rs:
      #     298 — jobs_api.create succeeds, not the dark error path)
      #   - status_patch runs (manifest.rs:473) — rules out the :309→
      #     early-return dark path where non-409 create error returns
      #     BEFORE status is patched, leaving .status.replicas stale
      #   - Job carries rio.build/sizing=manifest + rio.build/
      #     {memory,cpu}-class labels (manifest.rs:1012-1015 — the
      #     inventory round-trip boundary)
      #   - Job pod has NO RIO_EPHEMERAL env (manifest.rs:980 does NOT
      #     set it) → worker loops, doesn't exit-after-one-build
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
      # running_builds=0 via ListExecutors — full round-trip, follow-
      # on if this times in under ~150s). Failed-Job sweep (P0511) is
      # also follow-on (needs deliberate crash injection).
      with subtest("manifest-pool: sizing=Manifest, cold-start Job, status_patch, no RIO_EPHEMERAL"):
          # Precondition: no STS workers, no lingering ephemeral Jobs
          # holding a heartbeat. ephemeral-pool ended with kubectl
          # delete builderpool ephemeral --wait=false; its Jobs
          # ownerRef-GC + ttlSecondsAfterFinished=60. workers_active
          # drops once the scheduler's disconnect-detect fires. 90s:
          # same margin as ephemeral-pool's precondition.
          sched_metric_wait(
              "grep -qx 'rio_scheduler_workers_active 0'",
              timeout=90,
          )

          # ── CEL: sizing=Manifest + maxConcurrentBuilds>1 rejected ─────
          # ctrl.pool.manifest-single-build — per-derivation resource
          # fit breaks with concurrent builds on one pod (ADR-020).
          assert_cel_rejects(
              "manifest-bad-maxbuilds",
              "  sizing: Manifest\n"
              "  replicas: {min: 0, max: 3}\n"
              "  autoscaling: {metric: queueDepth, targetValue: 2}\n"
              "  maxConcurrentBuilds: 4\n"
              "  fuseCacheSize: 5Gi\n"
              "  systems: [x86_64-linux]\n"
              "  image: rio-all",
              "sizing:Manifest requires maxConcurrentBuilds==1",
          )

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
              "  replicas: {min: 0, max: 3}\n"
              "  autoscaling: {metric: queueDepth, targetValue: 2}\n"
              "  maxConcurrentBuilds: 1\n"
              "  fuseCacheSize: 5Gi\n"
              "  systems: [x86_64-linux]\n"
              # Same rio-all:dev tag + tlsSecretName rationale as
              # ephemeral-pool (see that fragment's comments).
              "  image: rio-all:dev\n"
              "  imagePullPolicy: Never\n"
              "  tlsSecretName: rio-builder-tls\n"
              "  privileged: true\n"
              "  terminationGracePeriodSeconds: 60\n"
              "  nodeSelector: null\n"
              "  tolerations: null\n"
              "EOF"
          )

          # ── No StatefulSet (sizing=Manifest branches before STS) ──────
          # mod.rs:361 checks sizing==Manifest BEFORE the STS/Service/PDB
          # block. One reconcile tick (~3s with kube-runtime's fast CRD
          # watch), then assert. STS would be named `manifest-workers`.
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

          # ── r[ctrl.pool.manifest-long-lived]: NO RIO_EPHEMERAL env ────
          # Ephemeral Jobs set RIO_EPHEMERAL=1 so the worker exits after
          # one build. Manifest pods loop (ADR-020 §Decision ¶4 — amortize
          # pod-start cost over multiple same-size builds). jsonpath
          # filter on the env array: if RIO_EPHEMERAL is NOT in the list,
          # the filter returns empty. That's the assertion.
          eph_env = k3s_server.succeed(
              f"k3s kubectl -n ${nsBuilders} get job {mfjob} "
              "-o jsonpath='{.spec.template.spec.containers[0].env[?(@.name==\"RIO_EPHEMERAL\")].value}'"
          ).strip()
          assert eph_env == "", (
              f"manifest Job pod must NOT have RIO_EPHEMERAL env (long-"
              f"lived loop, not exit-after-one); got {eph_env!r}. If set, "
              f"build_manifest_job is wrongly sharing ephemeral's env path."
          )
          print("manifest-pool long-lived: no RIO_EPHEMERAL env ✓")

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
              timeout=120,
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
          print("manifest-pool PASS: sizing=Manifest → cold-start Job, "
                "labels present, no RIO_EPHEMERAL, status patched, "
                "ownerRef cascade cleanup")
    '';

    disruption-drain = ''
      # ══════════════════════════════════════════════════════════════════
      # disruption-drain — pod eviction → DisruptionTarget → force=true
      # ══════════════════════════════════════════════════════════════════
      # P0285: makes the 4 lying comments TRUE. Before this watcher,
      # DrainWorker{force:true} had ZERO prod callers — both construction
      # sites set force:false. The watcher (rio-controller/src/reconcilers/
      # builderpool/disruption.rs) observes the K8s-set DisruptionTarget
      # condition and calls force=true.
      #
      # Flow: K8s eviction API → pod.status.conditions[DisruptionTarget]
      # =True → watcher's applied_objects() stream fires → is_disruption_
      # target() → admin.drain_worker(force:true) → scheduler actor
      # handle_drain_worker → if force { to_reassign.drain(); send
      # CancelSignal each; reassign_derivations }.
      #
      # Log signal is the CONTROLLER's "DisruptionTarget: DrainWorker
      # force=true" info! at disruption.rs — cleanest single-grep proof
      # the watcher fired. The scheduler's "sent CancelSignal for
      # force-drain (preemption)" at worker.rs:254 is secondary (only
      # fires if running_builds was non-empty, which this test
      # arranges).
      #
      # Runs LAST in core: the eviction deletes default-builders-0. The
      # STS recreates it (~120s FUSE-mount+warm), but core has no
      # subsequent subtests needing a ready worker.

      # P518: 60s was tight under coverage-mode instrumentation
      # slowdown (mc=67 merge-183 hit it post-P0512's ~12s manifest-pool
      # subtest; subsequent merge-185 green — one-off). covTimeoutHeadroom
      # (common.nix:80) pads globalTimeout but not INTERNAL Python
      # wait-loops. 120 under coverage gives the watcher's applied_objects
      # stream + gRPC RTT + JSON log flush the same 2× slack ratio.
      _drain_deadline_s = ${if common.coverage then "120" else "60"}

      with subtest("disruption-drain: eviction → DisruptionTarget → DrainWorker force=true"):
          # Start a 120s build so running_builds is non-empty when
          # eviction hits. ssh-ng:// → gateway → SubmitBuild → Ready
          # → dispatch to default-builders-0 (the only worker). Back-
          # grounded — script proceeds while build runs.
          client.execute(
              "nohup nix-build --no-out-link "
              "--store 'ssh-ng://k3s-server' "
              "--arg busybox '(builtins.storePath ${common.busybox})' "
              "${disruptionDrv} > /tmp/disruption-build.log 2>&1 < /dev/null &"
          )

          # Wait for dispatch: scheduler's running gauge ≥1. 60s:
          # ssh-ng connect + gateway translate + Submit + actor Tick
          # + dispatch lag (flannel subnet race can add ~10s).
          sched_metric_wait(
              "awk '/^rio_scheduler_derivations_running / {print $2}' | "
              "grep -qE '^[1-9]'",
              timeout=60,
          )
          print("disruption-drain: build dispatched, triggering eviction")

          # Evict default-builders-0 via the K8s eviction subresource.
          # This is what `kubectl drain` calls under the hood — but
          # targeted at ONE pod instead of draining a whole node (which
          # would evict scheduler/store too and destabilize the test).
          #
          # The PDB (maxUnavailable=1) allows this: with 1 replica, 1
          # can be evicted (budget is met trivially). K8s sets
          # DisruptionTarget=True on the pod BEFORE deletion — that
          # status update is what the watcher observes.
          #
          # `|| true`: eviction returns 201 Created; kubectl-delete-
          # shaped exit handling sometimes reports non-zero depending
          # on shell plumbing. We assert the controller log below, not
          # this command's exit code.
          k3s_server.succeed(
              "printf '%s' "
              "'{\"apiVersion\":\"policy/v1\",\"kind\":\"Eviction\","
              "\"metadata\":{\"name\":\"default-builders-0\",\"namespace\":\"${nsBuilders}\"}}' "
              "| k3s kubectl create --raw "
              "'/api/v1/namespaces/${nsBuilders}/pods/default-builders-0/eviction' -f - "
              "|| true"
          )

          # THE ASSERTION: controller logged the watcher-fire.
          # disruption.rs:info!("DisruptionTarget: DrainWorker force=true").
          # 30s: watcher stream is applied_objects() with default_
          # backoff — Pod status update lands within one watch event
          # (~sub-second) + gRPC RTT to scheduler + JSON log flush.
          # The grep is anchored on "DisruptionTarget" (unique to the
          # watcher — no other component logs that word) AND "force=
          # true" (proving this is the watcher's call, not the pod's
          # SIGTERM force=false self-drain).
          # Read /var/log/pods directly (not kubectl logs): kubectl logs
          # fails with "http2: stream closed" under poll-loop load. Check
          # both nodes (controller replicas=2, podAntiAffinity). nullglob
          # + cat: safe if no matching files on a node. Poll both nodes
          # each iteration — the watcher may fire on either replica.
          import time
          deadline = time.time() + _drain_deadline_s
          found = False
          while time.time() < deadline:
              for node in [k3s_server, k3s_agent]:
                  rc, _ = node.execute(
                      "shopt -s nullglob; "
                      "cat /var/log/pods/${ns}_rio-controller-*/controller/*.log "
                      "2>/dev/null | grep -q 'DisruptionTarget.*force=true'"
                  )
                  if rc == 0:
                      found = True
                      break
              if found:
                  break
              time.sleep(2)
          assert found, (
              "controller never logged 'DisruptionTarget: DrainWorker "
              f"force=true' within {_drain_deadline_s}s on either node"
          )

          # SECONDARY: scheduler saw force=true and preempted. "sent
          # CancelSignal for force-drain" at actor/worker.rs:254 fires
          # iff running_builds was non-empty (we arranged it). The
          # scheduler log is JSON; grep for the message substring.
          #
          # Leader lookup fresh (recovery subtest may have changed it;
          # core doesn't run recovery, but leader_pod() is idempotent).
          # Same /var/log/pods approach as primary — kubectl logs has
          # http2:stream-closed under poll load. Scheduler replicas=2,
          # either node may have the leader.
          deadline = time.time() + _drain_deadline_s
          found = False
          while time.time() < deadline:
              for node in [k3s_server, k3s_agent]:
                  rc, _ = node.execute(
                      "shopt -s nullglob; "
                      "cat /var/log/pods/${ns}_rio-scheduler-*/scheduler/*.log "
                      "2>/dev/null | grep -q 'force-drain'"
                  )
                  if rc == 0:
                      found = True
                      break
              if found:
                  break
              time.sleep(2)
          assert found, (
              f"scheduler never logged 'force-drain' within {_drain_deadline_s}s on either node"
          )

          print("disruption-drain PASS: watcher fired DrainWorker force=true, "
                "scheduler preempted in-flight build")
    '';

    finalizer = ''
      # ══════════════════════════════════════════════════════════════════
      # finalizer — delete BuilderPool → pod gone → CR gone → workers=0
      # ══════════════════════════════════════════════════════════════════
      #   The autoscaler's 30s poll fires DURING this subtest (~300s wall
      #   time). scaling.rs:222 `if pool.metadata.deletion_timestamp.is_some()
      #   { skip }` MUST fire — otherwise the autoscaler would try to scale
      #   a being-deleted pool, racing the finalizer's scale-to-0. The test
      #   passing (pod gone + CR gone) proves the skip gate works.
      #
      # Runs LAST: deletes the only BuilderPool, so no workers exist after.
      # Finalizer's cleanup(): DrainWorker + scale STS → 0 + wait for pod
      # termination + remove finalizer. Queue is already empty (autoscaler
      # drained) → acquire_many succeeds immediately → no blocking.
      with subtest("finalizer: delete BuilderPool → drain → pod gone → CR gone"):
          # --wait=false: don't block kubectl on the finalizer. We assert
          # each stage with its own timeout so a hang points at the exact
          # stage (pod-gone vs CR-gone vs workers_active).
          kubectl("delete builderpool default --wait=false", ns="${nsBuilders}")

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
              "! k3s kubectl -n ${nsBuilders} get pod default-builders-0 2>/dev/null",
              timeout=300,
          )

          # BuilderPool CR gone (finalizer removed → K8s deleted it).
          k3s_server.wait_until_succeeds(
              "! k3s kubectl -n ${nsBuilders} get builderpool default 2>/dev/null",
              timeout=30,
          )

          # Scheduler saw the disconnect. workers_active EXACTLY 0 — not
          # just ≤0 (gauge underflow would be a bug).
          sched_metric_wait(
              "grep -qx 'rio_scheduler_workers_active 0'",
              timeout=30,
          )
          print("finalizer PASS: BuilderPool deleted, pod drained, scheduler saw disconnect")
    '';

    pdb-ownerref = ''
      # ══════════════════════════════════════════════════════════════════
      # pdb-ownerref — `{pool}-pdb` exists, ownerRef→BuilderPool, GC'd
      # ══════════════════════════════════════════════════════════════════
      # Proves build_pdb (builders.rs:178) produces a real K8s PDB when
      # the BuilderPool reconciler SSA-applies it. The fixture's `default`
      # BuilderPool (vmtest-full.yaml) comes up → reconciler creates
      # `default-pdb` with maxUnavailable=1 + ownerRef[0]=BuilderPool.
      # Delete `default` → finalizer drains + removes → K8s ownerRef GC
      # cascade takes the PDB.
      #
      # Runs FIRST in the wps split (fresh fixture, `default` BuilderPool
      # intact). Disruptive: deletes `default` — subsequent fragments
      # must not need it.
      with subtest("pdb-ownerref: PDB exists, owned by BuilderPool, GC'd on delete"):
          pdb = "default-pdb"

          # ── PDB exists with maxUnavailable=1 ──────────────────────────
          # The reconciler's first apply() runs during waitReady (CRD
          # watch fires on the fixture's BuilderPool create). By the time
          # the prelude returns, `default-pdb` should exist. 30s margin
          # for the SSA patch + k3s apiserver admission lag.
          k3s_server.wait_until_succeeds(
              "test \"$(k3s kubectl -n ${nsBuilders} get pdb default-pdb "
              "-o jsonpath='{.spec.maxUnavailable}')\" = 1",
              timeout=30,
          )

          # ── ownerReferences[0] → BuilderPool ───────────────────────────
          # controller=true, kind=BuilderPool, name=default. K8s GC
          # cascades when the owner is deleted IFF controller=true (or
          # blockOwnerDeletion — same effect for GC). The reconciler's
          # controller_owner_ref(&()) sets controller=true.
          owner_kind = kubectl(
              f"get pdb {pdb} "
              "-o jsonpath='{.metadata.ownerReferences[0].kind}'",
              ns="${nsBuilders}",
          ).strip()
          assert owner_kind == "BuilderPool", (
              f"expected ownerRef[0].kind=BuilderPool, got {owner_kind!r}. "
              f"Without ownerRef, BuilderPool delete leaks the PDB."
          )
          owner_name = kubectl(
              f"get pdb {pdb} "
              "-o jsonpath='{.metadata.ownerReferences[0].name}'",
              ns="${nsBuilders}",
          ).strip()
          assert owner_name == "default", (
              f"expected ownerRef[0].name=default (the pool name), "
              f"got {owner_name!r}"
          )
          owner_ctrl = kubectl(
              f"get pdb {pdb} "
              "-o jsonpath='{.metadata.ownerReferences[0].controller}'",
              ns="${nsBuilders}",
          ).strip()
          assert owner_ctrl == "true", (
              f"expected ownerRef[0].controller=true (K8s GC requires "
              f"it for cascade), got {owner_ctrl!r}"
          )

          # ── Delete BuilderPool → PDB GC'd ──────────────────────────────
          # --wait=false: don't block kubectl on the finalizer. The
          # finalizer's cleanup (DrainWorker + scale STS→0 + wait for
          # pods gone + remove finalizer) completes fast here — no
          # in-flight builds (fresh fixture, no subtest submitted any).
          # terminationGracePeriodSeconds=180 still applies to the pod,
          # but with no builds the worker SIGTERM-exits immediately.
          kubectl("delete builderpool default --wait=false", ns="${nsBuilders}")

          # BuilderPool CR gone first (finalizer removed → K8s deletes).
          # 120s: grace=180s would apply if SIGTERM hung, but idle
          # worker exits in <5s. 120s absorbs the finalizer's
          # DRAIN_WAIT_SLOP (60s) + k3s controller-manager GC sweep lag.
          k3s_server.wait_until_succeeds(
              "! k3s kubectl -n ${nsBuilders} get builderpool default 2>/dev/null",
              timeout=120,
          )

          # THE ASSERTION: PDB GC'd via ownerRef cascade. k3s's
          # built-in GC controller (same as upstream kube-controller-
          # manager) runs a continuous sweep; orphan detection is
          # event-driven (owner delete → GC fires). 30s is generous.
          k3s_server.wait_until_succeeds(
              f"! k3s kubectl -n ${nsBuilders} get pdb {pdb} 2>/dev/null",
              timeout=30,
          )
          print(f"pdb-ownerref PASS: {pdb} GC'd after BuilderPool delete")
    '';

    wps-lifecycle = ''
      # ══════════════════════════════════════════════════════════════════
      # wps-lifecycle — apply WPS → 3 children → delete → children gone
      # ══════════════════════════════════════════════════════════════════
      # Proves the BuilderPoolSet reconciler end-to-end: apply a 3-class
      # WPS → reconciler's per-class loop (builderpoolset/mod.rs:131)
      # creates 3 child WorkerPools named `{wps}-{class}` → each child
      # carries sizeClass=class.name + ownerRef[0]=BuilderPoolSet
      # (controller=true). Delete WPS → finalizer cleanup() explicitly
      # deletes each child (mod.rs:375); ownerRef GC is the fallback.
      #
      # RUNS AFTER pdb-ownerref: the `default` BuilderPool is gone (no
      # STS worker to steal dispatches if the child pools' autoscaler
      # ever decided to scale up — but replicas.min=0 default means
      # they don't). Doesn't strictly require it, but keeps the
      # cluster state simple.
      #
      # NOT proven here: the per-class autoscaler actually scales a
      # child. That'd need queue pressure on a specific size class —
      # scaling.rs unit tests cover compute_desired; this covers the
      # reconcile→child-create→cleanup wiring.
      with subtest("wps-lifecycle: apply WPS → 3 children → delete → children gone"):
          # ── Apply 3-class WPS ─────────────────────────────────────────
          # poolTemplate.image=rio-all + imagePullPolicy not-in-template
          # (builders.rs:137 hardcodes None). systems=[x86_64-linux]
          # (required by child WP CEL). resources are dummy — replicas.
          # min defaults to 0 (DEFAULT_MIN_REPLICAS in builders.rs:18)
          # so no pods scheduled; we only check CR structure.
          # privileged:true matches the fixture (vmtest-full.yaml).
          #
          # Heredoc-via-stdin, same pattern as ephemeral-pool's
          # BuilderPool apply. Inline YAML so the fragment is
          # self-contained (no external fixture file to drift).
          k3s_server.succeed(
              "k3s kubectl apply -f - <<'EOF'\n"
              "apiVersion: rio.build/v1alpha1\n"
              "kind: BuilderPoolSet\n"
              "metadata:\n"
              "  name: test-wps\n"
              "  namespace: ${nsBuilders}\n"
              "spec:\n"
              "  classes:\n"
              "    - name: small\n"
              "      cutoffSecs: 60\n"
              "      resources:\n"
              "        requests: {cpu: '1', memory: 2Gi}\n"
              "    - name: medium\n"
              "      cutoffSecs: 300\n"
              "      resources:\n"
              "        requests: {cpu: '2', memory: 4Gi}\n"
              "    - name: large\n"
              "      cutoffSecs: 1800\n"
              "      resources:\n"
              "        requests: {cpu: '4', memory: 8Gi}\n"
              "  poolTemplate:\n"
              "    image: rio-all\n"
              "    systems: [x86_64-linux]\n"
              "    privileged: true\n"
              "EOF"
          )

          # ── 3 children appear, correct sizeClass + ownerRef ───────────
          # child_name = "{wps}-{class.name}" (builders.rs:43). The
          # reconciler's .owns(BuilderPool) watch fires on CR create
          # (<1s); SSA-apply for each child is fast (no pod create
          # at replicas.min=0). 30s absorbs the reconcile tick +
          # 3× apiserver admission.
          for cls in ["small", "medium", "large"]:
              child = f"test-wps-{cls}"
              k3s_server.wait_until_succeeds(
                  f"k3s kubectl -n ${nsBuilders} get builderpool {child} 2>/dev/null",
                  timeout=30,
              )
              # Pull once, assert against parsed JSON — cheaper than
              # 4× kubectl roundtrips per child and keeps the ns kwarg
              # in one place (ADR-019: builderpools live in rio-builders).
              import json as _json
              bp = _json.loads(
                  kubectl(f"get builderpool {child} -o json", ns="${nsBuilders}")
              )
              sc = bp["spec"].get("sizeClass")
              assert sc == cls, (
                  f"expected {child}.spec.sizeClass={cls}, got {sc!r}. "
                  f"builders.rs:114 sets size_class=class.name — if "
                  f"these diverge, scheduler routing breaks."
              )
              owner = bp["metadata"]["ownerReferences"][0]
              assert owner["name"] == "test-wps", (
                  f"expected {child} ownerRef[0].name=test-wps, "
                  f"got {owner['name']!r}"
              )
              assert owner["kind"] == "BuilderPoolSet", (
                  f"expected {child} ownerRef[0].kind=BuilderPoolSet, "
                  f"got {owner['kind']!r}"
              )
              # autoscaling.targetValue = class.targetQueuePerReplica
              # (default 5, per-class in builders.rs:98). Proves the
              # per-class autoscaler wiring — each child carries its
              # own target, not a shared WPS-level value.
              tv = str(bp["spec"]["autoscaling"]["targetValue"])
              assert tv == "5", (
                  f"expected {child}.spec.autoscaling.targetValue=5 "
                  f"(default targetQueuePerReplica), got {tv!r}"
              )
          print("wps-lifecycle: 3 children created, sizeClass+ownerRef+autoscaling correct")

          # ── Delete WPS → children GC'd ────────────────────────────────
          # --wait=false: don't block on the WPS finalizer. cleanup()
          # (builderpoolset/mod.rs:371) explicitly deletes each child
          # with 404 tolerance. Each child then runs ITS OWN BuilderPool
          # finalizer (DrainWorker + scale STS→0 — trivially fast at
          # replicas=0) before K8s GC deletes STS/Service/PDB.
          kubectl("delete builderpoolset test-wps --wait=false", ns="${nsBuilders}")

          # WPS CR gone: finalizer removed → K8s deletes. 60s: cleanup
          # iterates 3 children × (delete RPC + child finalizer). At
          # replicas=0, child finalizers complete in <5s each (no pods
          # to drain). 60s absorbs k3s controller lag.
          k3s_server.wait_until_succeeds(
              "! k3s kubectl -n ${nsBuilders} get builderpoolset test-wps 2>/dev/null",
              timeout=60,
          )

          # Children gone. Either cleanup() deleted them explicitly OR
          # ownerRef GC caught them — both paths converge on "not found".
          # Checked per-child so a hang names which class stuck.
          for cls in ["small", "medium", "large"]:
              k3s_server.wait_until_succeeds(
                  f"! k3s kubectl -n ${nsBuilders} get builderpool test-wps-{cls} 2>/dev/null",
                  timeout=30,
              )

          print("wps-lifecycle PASS: 3 children created + GC'd on WPS delete")
    '';

    fetcherpool-sts = ''
      # ══════════════════════════════════════════════════════════════════
      # fetcherpool-sts — FetcherPool CR → STS with ADR-019 security posture
      # ══════════════════════════════════════════════════════════════════
      # Proves the fetcherpool reconciler (ADR-019) produces a real K8s
      # StatefulSet with the ADR-specified hardening: rio.build/role:
      # fetcher label, readOnlyRootFilesystem:true, rio-fetcher.json
      # seccomp, fetcher nodeSelector+toleration. The reconciler calls
      # common/sts.rs with ExecutorRole::Fetcher; this proves the
      # params→podspec chain end-to-end against a real apiserver.
      #
      # STS-shape-only test: does NOT wait for readyReplicas. The
      # fetcher pod needs a fetcher-tainted node + the rio-fetcher.json
      # seccomp profile installed + the device plugin — none of which
      # the k3s-full fixture provides yet (P0452 scheduler routing +
      # the four-namespace helm values are the enabling work). The pod
      # spec IS the proof: if the reconciler emits the right labels/
      # securityContext/nodeSelector, the ADR-019 wiring is correct.
      #
      # Applied in rio-fetchers (ADR-019 four-namespace layout, P0454).
      # The reconciler creates the STS in the same namespace as the CR.
      #
      # Tracey: r[verify ctrl.fetcherpool.reconcile] +
      # r[verify fetcher.sandbox.strict-seccomp] +
      # r[verify fetcher.node.dedicated] — markers at the default.nix
      # subtests entry (P0341 convention).
      with subtest("fetcherpool-sts: CR → STS with fetcher labels+securityContext"):
          # ── Apply a minimal FetcherPool CR ────────────────────────────
          k3s_server.succeed(
              "k3s kubectl apply -f - <<'EOF'\n"
              "apiVersion: rio.build/v1alpha1\n"
              "kind: FetcherPool\n"
              "metadata:\n"
              "  name: test-fp\n"
              "  namespace: ${nsFetchers}\n"
              "spec:\n"
              "  replicas: 1\n"
              "  image: rio-all:dev\n"
              "  systems: [x86_64-linux]\n"
              "EOF"
          )

          # ── STS exists, owned by the CR ───────────────────────────────
          # Reconciler SSA-applies on CRD watch event. 30s margin for
          # the apply + k3s admission lag (same as pdb-ownerref above).
          sts = "test-fp-fetchers"
          k3s_server.wait_until_succeeds(
              f"k3s kubectl -n ${nsFetchers} get statefulset {sts}",
              timeout=30,
          )

          # STS-shape checks: pull once, assert against the JSON. Fetch-
          # per-field was 6 kubectl roundtrips; one -o json + jsonpath
          # on the python side is cheaper and keeps the ns kwarg in one
          # place (ADR-019: STS lives in rio-fetchers).
          import json as _json
          sts_json = _json.loads(
              kubectl(f"get sts {sts} -o json", ns="${nsFetchers}")
          )
          tmpl = sts_json["spec"]["template"]
          ctr = tmpl["spec"]["containers"][0]

          # ── Pod template label: rio.build/role=fetcher ────────────────
          role = tmpl["metadata"]["labels"].get("rio.build/role")
          assert role == "fetcher", (
              f"expected rio.build/role=fetcher, got {role!r}. "
              f"NetworkPolicies and scheduler routing key on this."
          )

          # ── securityContext: readOnlyRootFilesystem=true ──────────────
          ro_fs = ctr.get("securityContext", {}).get("readOnlyRootFilesystem")
          assert ro_fs is True, (
              f"expected readOnlyRootFilesystem=true, got {ro_fs!r}. "
              f"ADR-019 §Sandbox hardening — rootfs tampering blocked."
          )

          # ── seccomp: Localhost rio-fetcher.json ───────────────────────
          # Container-level (not pod-level — pod-level stays
          # RuntimeDefault so wait-seccomp initContainer can run
          # before the profile lands on the node).
          seccomp = (
              ctr.get("securityContext", {})
              .get("seccompProfile", {})
              .get("localhostProfile")
          )
          assert seccomp == "profiles/rio-fetcher.json", (
              f"expected profiles/rio-fetcher.json, got {seccomp!r}. "
              f"ADR-019 §Sandbox hardening — extra denies for ptrace/bpf/…"
          )

          # ── nodeSelector + toleration: fetcher node pool ──────────────
          node_role = tmpl["spec"].get("nodeSelector", {}).get("rio.build/node-role")
          assert node_role == "fetcher", (
              f"expected nodeSelector rio.build/node-role=fetcher, "
              f"got {node_role!r}. ADR-019 §Node isolation."
          )
          tols = tmpl["spec"].get("tolerations", [])
          assert any(t.get("key") == "rio.build/fetcher" for t in tols), (
              f"expected toleration key rio.build/fetcher, got {tols!r}"
          )

          # ── RIO_EXECUTOR_KIND=fetcher env ─────────────────────────────
          # rio-builder's kind-gate (ADR-019 §Executor enforcement)
          # refuses FODs when RIO_EXECUTOR_KIND!=fetcher.
          env = {e["name"]: e.get("value") for e in ctr.get("env", [])}
          assert env.get("RIO_EXECUTOR_KIND") == "fetcher", (
              f"expected RIO_EXECUTOR_KIND=fetcher in env, got:\n{env!r}"
          )

          # ── Cleanup ───────────────────────────────────────────────────
          # --wait=false + short timeout: fetcherpool cleanup is
          # immediate (no long terminationGracePeriod to wait through).
          kubectl("delete fetcherpool test-fp --wait=false", ns="${nsFetchers}")
          k3s_server.wait_until_succeeds(
              f"! k3s kubectl -n ${nsFetchers} get statefulset {sts} 2>/dev/null",
              timeout=60,
          )
          print("fetcherpool-sts PASS: STS shape matches ADR-019")
    '';

    store-rollout = ''
      # ══════════════════════════════════════════════════════════════════
      # store-rollout — scheduler store_client survives store pod rollout
      # ══════════════════════════════════════════════════════════════════
      # Proves: the scheduler's long-held StoreServiceClient channel
      # transparently reconnects after a store Deployment rollout. Before
      # the connect_lazy fix, the scheduler's eager-connected Channel
      # cached the OLD pod IP at startup; when that pod terminated,
      # FindMissingPaths RPCs failed with connection-refused and never
      # recovered — the cache-check circuit breaker tripped and
      # SubmitBuild rejected with Unavailable until the scheduler pod
      # was ALSO restarted.
      #
      # connect_lazy (rio-proto/src/client/mod.rs) re-resolves DNS on
      # each reconnect + HTTP/2 keepalive detects half-open connections
      # within ~40s. The channel reconnects to the new pod without
      # scheduler intervention.
      #
      # Test shape:
      #   1. build pre-rollout → scheduler→store FindMissingPaths OK
      #   2. rollout restart deploy/rio-store → old pod terminates,
      #      new pod comes up with a DIFFERENT IP
      #   3. wait for new store pod ready
      #   4. build post-rollout → scheduler→store must reconnect
      #   5. assert cache_check_failures_total stayed low (≤ circuit
      #      breaker threshold of 5) and post-build succeeded
      #
      # Tracey: r[verify sched.store-client.reconnect] at default.nix
      # subtests entry (P0341 convention).
      with subtest("store-rollout: scheduler reconnects to new store pod"):
          # ── Baseline: pre-rollout build ───────────────────────────────
          # Proves the scheduler→store channel works at all. mkTrivial
          # derivation: DAG merge → FindMissingPaths cache-check →
          # dispatch → build → upload.
          out_pre = build("${rolloutPreDrv}", capture_stderr=False).strip()
          assert out_pre.startswith("/nix/store/"), (
              f"pre-rollout build should succeed: {out_pre!r}"
          )

          # Capture the current store pod name. After rollout restart,
          # the Deployment controller creates a NEW pod with a DIFFERENT
          # name (and IP). Seeing a different name proves the rollout
          # actually cycled the pod.
          old_store = kubectl(
              "get pod -l app.kubernetes.io/name=rio-store "
              "-o jsonpath='{.items[0].metadata.name}'",
              ns="${nsStore}",
          ).strip()
          print(f"store-rollout: pre-rollout store pod = {old_store}")

          # Baseline cache-check failure count. Should be 0 (or low if a
          # transient blip happened during prelude's waitReady).
          m = sched_metrics()
          pre_failures = metric_value(
              m, "rio_scheduler_cache_check_failures_total"
          ) or 0.0

          # ── Rollout restart the store ─────────────────────────────────
          # `rollout restart` patches the Deployment's pod template with
          # a kubectl.kubernetes.io/restartedAt annotation → triggers
          # rolling update → old pod terminates, new pod (new name, new
          # IP) comes up. This is the exact operation helm upgrade
          # performs when store config changes.
          kubectl("rollout restart deploy/rio-store", ns="${nsStore}")

          # Wait for rollout to complete. `rollout status` blocks until
          # the new ReplicaSet is fully Available. NOTE: Available ≠ old
          # pods deleted — the Terminating pod lingers through its
          # grace period, so a bare label-selector query can still
          # return it as .items[0] (P0489).
          # 120s: store pod startup = image pull (cached) + sqlx migrate
          # + listen, ~30-60s under KVM.
          k3s_server.wait_until_succeeds(
              "k3s kubectl -n ${nsStore} rollout status "
              "deploy/rio-store --timeout=90s",
              timeout=120,
          )

          # Verify the pod actually cycled — new name ≠ old name. If
          # rollout restart was a no-op (shouldn't be, but sanity), the
          # test is hollow.
          # Filter on deletionTimestamp is None to exclude the old
          # Terminating pod. Can't use --field-selector=status.phase
          # for this: "Terminating" is metadata.deletionTimestamp!=null,
          # not a phase — status.phase stays Running until the
          # container process actually exits. jq isn't in the VM, so
          # parse in the test driver.
          import json as _json
          pods = _json.loads(k3s_server.succeed(
              "k3s kubectl -n ${nsStore} get pod "
              "-l app.kubernetes.io/name=rio-store -o json"
          ))
          new_store = next(
              p["metadata"]["name"] for p in pods["items"]
              if p["metadata"].get("deletionTimestamp") is None
          )
          assert new_store != old_store, (
              f"store pod should have cycled: old={old_store} new={new_store}"
          )
          print(f"store-rollout: post-rollout store pod = {new_store}")

          # ── Post-rollout build ────────────────────────────────────────
          # THE TEST. Before the connect_lazy fix, this hangs or fails:
          # scheduler's cached Channel points at old_store's IP → TCP
          # connection-refused → FindMissingPaths fails → after 5
          # consecutive failures circuit breaker opens → SubmitBuild
          # rejected with Unavailable. With lazy: next RPC re-resolves
          # DNS → new_store's IP → reconnect → build succeeds.
          #
          # Different marker than rolloutPreDrv → fresh derivation →
          # DAG merge runs cache-check (not a dedup hit).
          out_post = build("${rolloutPostDrv}", capture_stderr=False).strip()
          assert out_post.startswith("/nix/store/"), (
              f"post-rollout build should succeed WITHOUT scheduler "
              f"restart (connect_lazy reconnect): {out_post!r}"
          )

          # ── Bounded failure delta ─────────────────────────────────────
          # cache_check_failures_total may tick once or twice during the
          # rollout window (keepalive timeout detection ~40s can overlap
          # a build attempt). But it MUST stay below the circuit-breaker
          # threshold (5 consecutive) — if it doesn't, the breaker opened
          # and the post-build would have failed above. Belt-and-braces:
          # explicit numeric check.
          m = sched_metrics()
          post_failures = metric_value(
              m, "rio_scheduler_cache_check_failures_total"
          ) or 0.0
          delta = post_failures - pre_failures
          assert delta < 5, (
              f"cache-check failures should stay below breaker threshold "
              f"across rollout; delta={delta} (pre={pre_failures}, "
              f"post={post_failures}). If this trips, connect_lazy is "
              f"not re-resolving DNS — check tonic Channel reconnect "
              f"semantics."
          )

          print(
              f"store-rollout PASS: {old_store}→{new_store}, "
              f"post-build={out_post}, cache_check_failures delta={delta}"
          )
    '';

    bootstrap-job-ran = ''
      # ══════════════════════════════════════════════════════════════════
      # bootstrap-job-ran — PSA-restricted exec + no EROFS
      # ══════════════════════════════════════════════════════════════════
      # Prod-parity fixture only (k3s-prod-parity.nix sets
      # bootstrap.enabled=true). vmtest-full.yaml's default is false —
      # the Job never renders under the base k3s-full fixture, so this
      # fragment under a non-prod-parity fixture would wait forever at
      # the Job-exists check.
      #
      # The bootstrap script (nix/docker.nix bootstrap attr) does:
      #   1. env-check (`: ''${AWS_REGION:?} ''${CHUNK_BUCKET:?}`)
      #   2. `aws secretsmanager describe-secret` → awscli2 init,
      #      may write $HOME/.aws/cli/cache/
      #   3. if not-found: `openssl rand 32 > /tmp/hmac`
      #   4. `aws secretsmanager create-secret` → UNREACHABLE in the
      #      airgapped VM (no IRSA, no endpoint) → set -e exits nonzero
      #
      # Step 4 means the Job NEVER reaches Complete here. That's
      # EXPECTED — we're testing PSA compatibility, not AWS. The
      # a28e4b65 regression was awscli2 writing $HOME/.aws/ with HOME
      # unset → falls back to / → tries /.aws → EROFS under
      # readOnlyRootFilesystem. The fix (HOME=/tmp) means step 2-3 run
      # without EROFS; step 4 fails with "Unable to locate credentials"
      # or "Could not connect". We assert:
      #   - Pod spec has readOnlyRootFilesystem=true (PSA rendered)
      #   - Logs contain "[bootstrap] generating rio/hmac" (past
      #     env-check + awscli2 describe-secret returned not-found)
      #   - Logs DON'T contain "Read-only file system" (HOME=/tmp fix)
      #
      # Tracey: r[verify sec.psa.control-plane-restricted] at
      # default.nix subtests entry.
      with subtest("bootstrap-job-ran: PSA-restricted exec + no EROFS"):
          # Job must exist (proves bootstrap.enabled=true rendered).
          # The Job's pod may still be running its first attempt or
          # already in backoff — we don't gate on Job status here,
          # just on its existence. k3s applies 02-workloads.yaml
          # during waitReady; by the time waitReady returns, the
          # Job object is in etcd.
          kubectl("get job rio-bootstrap")

          # Pod spec: readOnlyRootFilesystem=true proves the
          # rio.containerSecurityContext helper rendered PSA-
          # restricted. Without it, the fragment proves nothing
          # (no-readOnlyRoot → no EROFS possible → hollow test).
          # jsonpath on the Job's pod-template, not a running pod —
          # the pod may already be gone (backoff) but the template
          # persists.
          rorfs = kubectl(
              "get job rio-bootstrap -o jsonpath="
              "'{.spec.template.spec.containers[0].securityContext"
              ".readOnlyRootFilesystem}'"
          ).strip()
          assert rorfs == "true", (
              f"bootstrap Job pod-template must have "
              f"readOnlyRootFilesystem=true (PSA-restricted); got "
              f"{rorfs!r}. If this fails, rio.containerSecurityContext "
              f"(_helpers.tpl) isn't being included, or PSA was "
              f"bumped to privileged (coverage mode does this — "
              f"prod-parity fixture shouldn't)."
          )

          # HOME=/tmp env proves the a28e4b65 fix is present. Without
          # it, awscli2 falls back to HOME=/ under UID 65532.
          home = kubectl(
              "get job rio-bootstrap -o jsonpath="
              "\"{.spec.template.spec.containers[0].env[?(@.name=='HOME')].value}\""
          ).strip()
          assert home == "/tmp", (
              f"bootstrap Job should set HOME=/tmp (a28e4b65 fix); "
              f"got {home!r}. awscli2 writes cache to $HOME/.aws/ — "
              f"unset HOME → / → EROFS under readOnlyRootFilesystem."
          )

          # Wait for the Job to reach a terminal state. backoffLimit=2
          # → 3 pod attempts; each runs ~5-20s (awscli2 credential
          # chain: env→file→IMDS, IMDS probe timeout ~3s, then
          # describe-secret fails → else branch → echo → openssl →
          # create-secret also fails). With exponential backoff
          # (10s, 20s) between retries: ~(20+10+20+20+20) ≈ 90s to
          # Failed. 240s timeout is generous.
          #
          # Why not `kubectl logs job/NAME`: it picks the MOST RECENT
          # pod, which during backoff may still be ContainerCreating
          # → empty logs while the earlier (terminated) pods DO have
          # logs. Why not `grep -q bootstrap` as the wait: kubectl's
          # own stderr includes "using pod/rio-bootstrap-NNNN" → the
          # grep would match THAT, not script output, returning 10s
          # early while the newest pod is still Creating.
          k3s_server.wait_until_succeeds(
              "k3s kubectl -n ${ns} wait --for=condition=Failed "
              "job/rio-bootstrap --timeout=10s",
              timeout=240,
          )

          # Logs from ALL bootstrap pods (label-selector, --prefix
          # tags each line with [pod/NAME]). All three are terminated
          # now (Job is Failed); at least one will have non-empty
          # logs. --tail=-1: everything (default is last 10 lines
          # for label-selector mode, which would miss the early
          # echo if the aws error is verbose).
          logs = k3s_server.succeed(
              "k3s kubectl -n ${ns} logs "
              "-l app.kubernetes.io/name=rio-bootstrap "
              "--prefix --tail=-1 2>&1"
          )
          print(f"bootstrap-job-ran: logs:\n{logs}")

          # Also dump pod terminal state for triage: exit code +
          # reason tells us WHERE the script died. exitCode=2 ≈
          # bash `set -e` abort; exitCode=1 ≈ explicit exit 1;
          # reason=OOMKilled ≈ awscli2 blew past memory.
          term = k3s_server.succeed(
              "k3s kubectl -n ${ns} get pod "
              "-l app.kubernetes.io/name=rio-bootstrap "
              "-o jsonpath="
              "'{range .items[*]}{.metadata.name} "
              "exit={.status.containerStatuses[0].state.terminated.exitCode} "
              "reason={.status.containerStatuses[0].state.terminated.reason}"
              "{\"\\n\"}{end}'"
          )
          print(f"bootstrap-job-ran: pod terminal states:\n{term}")

          # P0493 regression signature. The whole point of this
          # fragment. openssl's `> /tmp/hmac` redirect would emit
          # this via bash; awscli2's mkdir $HOME/.aws would emit it
          # via Python's OSError. Either way: fix verbatim,
          # unmistakable.
          assert "Read-only file system" not in logs, (
              f"bootstrap hit EROFS — P0493 regression. HOME=/tmp "
              f"should have routed awscli2's cache + the script's "
              f"/tmp/hmac write to the emptyDir mount. Logs:\n{logs}"
          )

          # Script progressed past the env-check (:? guards) AND past
          # the describe-secret call (returned not-found → fell into
          # the else branch → printed this line before openssl).
          # If this assert fires without EROFS, check: AWS_REGION/
          # CHUNK_BUCKET set? (k3s-prod-parity.nix extraValues)
          # Or did awscli2 hang >timeout on connection-refused?
          assert "[bootstrap] generating rio/hmac" in logs, (
              f"bootstrap script should have progressed to the "
              f"openssl path (proves env-check passed + awscli2 init "
              f"ran + describe-secret returned not-found). If logs "
              f"show the env-check failure instead, prod-parity "
              f"fixture's global.region/chunkBackend.bucket overrides "
              f"aren't reaching the Job env. Logs:\n{logs}"
          )

          print(
              f"bootstrap-job-ran PASS: readOnlyRootFilesystem={rorfs}, "
              f"HOME={home}, no EROFS, script reached openssl path"
          )
    '';

    bootstrap-tenant = ''
      # ══════════════════════════════════════════════════════════════════
      # bootstrap-tenant — leader-guard: standby rejects, leader accepts
      # ══════════════════════════════════════════════════════════════════
      # scheduler.replicas=2 (vmtest-full.yaml:99) → one leader + one
      # standby. AdminService writes (CreateTenant) are leader-only;
      # standby's interceptor rejects with UNAVAILABLE. Clients that
      # route via the ClusterIP Service (50% standby) must retry;
      # clients that route via Lease holderIdentity (tunnel_grpc in
      # xtask, leader_pod() here) are deterministic.
      #
      # abef66c7 fixed tunnel_grpc to use Lease-lookup instead of
      # Service forward — before that, `xtask k8s cli create-tenant`
      # failed ~50% depending on which replica the port-forward hit.
      # 5b98e311 fixed step_tenant's AlreadyExists handling — retry
      # after a transient failure should succeed OR return
      # already-exists, not crash.
      #
      # This fragment proves the standby-reject path WORKS (positive
      # test — the guard IS enforcing) AND the Lease-routed path is
      # deterministic (3 attempts, all hit leader). Unit test
      # (guards_tests.rs) proves interceptor shape; this proves it
      # end-to-end against the real 2-replica Deployment.
      #
      # Tracey: r[verify sched.grpc.leader-guard] at default.nix
      # subtests entry — first VM-level verify under replicas>1.
      with subtest("bootstrap-tenant: standby rejects, Lease-routed leader accepts"):
          # ── Find standby (all sched pods minus leader) ────────────────
          leader = leader_pod()
          all_sched = kubectl(
              "get pods -l app.kubernetes.io/name=rio-scheduler "
              "-o jsonpath='{.items[*].metadata.name}'"
          ).split()
          standby_pods = [p for p in all_sched if p != leader]
          assert len(standby_pods) >= 1, (
              f"expected ≥1 standby with scheduler.replicas=2; "
              f"leader={leader}, all={all_sched}. If this fires, "
              f"replicas may have been downscaled or podAntiAffinity "
              f"failed to spread across server+agent."
          )
          standby = standby_pods[0]
          print(f"bootstrap-tenant: leader={leader}, standby={standby}")

          # ── Standby MUST reject CreateTenant ──────────────────────────
          # Port-forward to the STANDBY. grpcurl CreateTenant fails —
          # leader-guard interceptor checks election state and returns
          # UNAVAILABLE on non-leader. grpcurl's exit is 64+status_code
          # (UNAVAILABLE=14 → exit 78). .fail() expects nonzero AND
          # returns stdout+stderr — same pattern health-shared uses
          # for the NOT_SERVING probe. Port-forward setup separate
          # (.succeed), grpcurl in .fail, cleanup in finally.
          k3s_server.succeed(
              f"k3s kubectl -n ${ns} port-forward {standby} 19301:9001 "
              f">/dev/null 2>&1 & echo $! > /tmp/pf-standby.pid; sleep 2"
          )
          try:
              standby_out = k3s_server.fail(
                  "${grpcurl} ${grpcurlTls} -max-time 15 "
                  "-protoset ${protoset}/rio.protoset "
                  '-d \'{"tenantName": "prod-parity-standby-reject"}\' '
                  "localhost:19301 rio.admin.AdminService/CreateTenant 2>&1"
              )
              print(f"bootstrap-tenant: standby CreateTenant out:\n{standby_out}")
              # UNAVAILABLE is tonic's status for "not serving" — the
              # leader-guard intercept maps election_state != Leader
              # to it. grpcurl prints the status code name. .fail()
              # already proved nonzero exit; this proves it's the
              # RIGHT error (not a TLS mishap or port-forward race).
              assert "Unavailable" in standby_out, (
                  f"standby rejection should be gRPC Unavailable (the "
                  f"leader-guard status); got a different error. If "
                  f"this is a TLS/transport error, the port-forward "
                  f"raced or the PKI paths drifted. If it's a "
                  f"different gRPC status, check the interceptor's "
                  f"status mapping (actor_guards.rs). out:\n{standby_out}"
              )
              assert "not leader" in standby_out, (
                  f"standby rejection message should say 'not leader' "
                  f"(the exact message from actor_guards.rs); got a "
                  f"different Unavailable — maybe the health reporter "
                  f"not the leader-guard? out:\n{standby_out}"
              )
          finally:
              k3s_server.execute(
                  "kill $(cat /tmp/pf-standby.pid) 2>/dev/null; "
                  "rm -f /tmp/pf-standby.pid"
              )

          # ── Lease-routed CreateTenant — 3 attempts, deterministic ─────
          # Each attempt re-queries leader_pod() → fresh Lease read →
          # port-forward to the current leader. With replicas=2 and
          # the Lease stable (no failover during this ~15s window),
          # all 3 hit the same pod. First succeeds; 2nd/3rd return
          # AlreadyExists (db/tenants.rs: ON CONFLICT DO NOTHING →
          # None → admin/mod.rs maps to AlreadyExists). `|| true`
          # swallows the AlreadyExists so we assert on output, not
          # exit code. Unique port per attempt: port-forward lacks
          # SO_REUSEADDR, TIME_WAIT takes ~60s to clear.
          tenant_name = "prod-parity-test"
          attempt_outs = []
          for i in range(3):
              ldr = leader_pod()
              port = 19310 + i
              out = k3s_server.succeed(
                  f"k3s kubectl -n ${ns} port-forward {ldr} {port}:9001 "
                  f">/dev/null 2>&1 & pf=$!; "
                  f"trap 'kill $pf 2>/dev/null' EXIT; sleep 2; "
                  f"${grpcurl} ${grpcurlTls} -max-time 15 "
                  f"-protoset ${protoset}/rio.protoset "
                  f"-d '{{\"tenantName\": \"{tenant_name}\"}}' "
                  f"localhost:{port} rio.admin.AdminService/CreateTenant "
                  f"2>&1 || true"
              )
              attempt_outs.append(out)
              print(f"bootstrap-tenant: attempt {i+1} (leader={ldr}):\n{out}")

          # None of the 3 attempts should have hit the standby-reject
          # path. leader_pod()'s Lease-lookup is the same pattern
          # xtask's tunnel_grpc uses post-abef66c7 — if this fires,
          # the Lease holderIdentity isn't pointing at the actual
          # leader (lease renew race? Lease TTL expired between
          # query and connect?).
          for i, out in enumerate(attempt_outs):
              assert "Unavailable" not in out and "UNAVAILABLE" not in out, (
                  f"attempt {i+1} hit the standby-reject path — "
                  f"Lease-lookup returned a non-leader. Before "
                  f"abef66c7 this was ~50% via Service forward; "
                  f"Lease-routed should be 0%. out:\n{out}"
              )

          # ── ListTenants proves the create landed ──────────────────────
          # Via the same Lease-routed path. ListTenants is read-only
          # but also leader-guarded (admin reads need consistent
          # view). Response is JSON (grpcurl default output).
          ldr = leader_pod()
          list_out = k3s_server.succeed(
              f"k3s kubectl -n ${ns} port-forward {ldr} 19320:9001 "
              f">/dev/null 2>&1 & pf=$!; "
              f"trap 'kill $pf 2>/dev/null' EXIT; sleep 2; "
              f"${grpcurl} ${grpcurlTls} -max-time 15 "
              f"-protoset ${protoset}/rio.protoset "
              f"-d '{{}}' "
              f"localhost:19320 rio.admin.AdminService/ListTenants 2>&1"
          )
          print(f"bootstrap-tenant: ListTenants:\n{list_out}")
          assert tenant_name in list_out, (
              f"{tenant_name!r} should appear in ListTenants after 3 "
              f"create attempts (at least one succeeded). If standby-"
              f"reject passed above but this fails, the create was "
              f"accepted then lost — check PG commit path. "
              f"out:\n{list_out}"
          )

          print(
              f"bootstrap-tenant PASS: standby rejected, 3 Lease-routed "
              f"attempts all reached leader, {tenant_name!r} in "
              f"ListTenants"
          )
    '';

  };

  mkTest = common.mkFragmentTest {
    scenario = "lifecycle";
    inherit prelude fragments fixture;
    defaultTimeout = 900;
    # Eval-time ordering guards. finalizer needs autoscaler to have
    # scaled STS to 2 first (pod-1 for reverse-ordinal termination
    # coverage — v24/v25 regression). ephemeral-pool requires
    # workers_active=0 (finalizer deletes the default STS pool); without
    # it the STS worker steals dispatches before reconcile_ephemeral's
    # 10s tick spawns a Job.
    chains = [
      {
        before = "autoscaler";
        after = "finalizer";
        msg = "finalizer requires autoscaler earlier (pod-1 reverse-ordinal coverage)";
      }
      {
        before = "finalizer";
        after = "ephemeral-pool";
        msg = "ephemeral-pool requires finalizer earlier (no STS workers stealing dispatch)";
      }
    ];
  };
in
{
  inherit fragments mkTest;
}
