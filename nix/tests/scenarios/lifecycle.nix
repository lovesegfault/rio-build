# Lifecycle scenario: scheduler recovery, GC, ephemeral pools,
# health-shared NOT_SERVING probe — all exercised against the k3s-full
# fixture.
#
# Ports phase3b sections S (recovery), C (GC), T (health-shared) onto the
# 2-node k3s Helm-chart fixture. Unlike phase3b (control/worker/k8s/client
# as separate systemd VMs), everything here runs as PODS — closes the
# "production uses pod path, VM tests use systemd" gap for the
# reconciler/lease surface.
#
#
# Fragment architecture: this file returns { fragments, mkTest } instead
# of a single runNixOSTest. default.nix composes fragments into parallel
# VM tests — critical path ~8min vs the prior ~14min monolith. Each
# fragment is a Python `with subtest(...)` block; mkTest concatenates a
# prelude + the selected fragments + coverage epilogue into a testScript.
# Key adaptation: scheduler pods are minimal images (no shell, no curl).
# Metric scrapes go through apiserver pods/proxy (`kubectl get --raw`);
# grpcurl (needs raw TCP) through `kubectl port-forward`.
# Scheduler has 2 replicas (podAntiAffinity spreads them
# across server+agent), so killing the leader means the STANDBY takes
# over — a strictly stronger recovery test than phase3b's single-instance
# restart.
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
# worker.cancel.cgroup-kill — verify marker at default.nix:subtests[cancel-cgroup-kill]
#   cancel-cgroup-kill calls CancelBuild via gRPC mid-exec and asserts
#   the cgroup is rmdir'd before the sleep completes. cgroup.rs:180
#   kill() writes "1" to cgroup.kill → kernel SIGKILLs the tree. No
#   other test cancels a RUNNING build (recovery kills the scheduler,
#   build keeps running on the worker).
#
# worker.cgroup.kill-on-teardown — verify marker at default.nix:subtests[build-timeout]
# worker.timeout.no-reassign — verify marker at default.nix:subtests[build-timeout]
#   build-timeout submits via gRPC SubmitBuild with buildTimeout=45 against
#   a 90s sleep. The timeout fires mid-build → run_daemon_build returns
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
# ctrl.pool.ephemeral — verify marker at default.nix:subtests[ephemeral-pool]
#   ephemeral-pool: applies an ephemeral kind=Builder Pool; asserts
#   status.desiredReplicas == replicas.max (reconcile_ephemeral ran and
#   patched status, ephemeral.rs:220-228) and the Job-spawn-on-queue path
#   end-to-end. Subtest deletes the default x86-64 Pool first
#   so its child pool's reconciler doesn't steal dispatch.
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
    ;
  drvs = import ../lib/derivations.nix { inherit pkgs; };
  protoset = import ../lib/protoset.nix { inherit pkgs; };

  # grpcurl not in k3sBase systemPackages (only curl+kubectl). Use the
  # store path directly — it's pulled into the VM closure by interpolation.
  grpcurl = "${pkgs.grpcurl}/bin/grpcurl";

  # rio-* gRPC is plaintext-on-WireGuard; grpcurl needs -plaintext.
  grpcurlTls = "-plaintext";

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

  # build-timeout victim. sleepSecs=90 vs buildTimeout=45 — wide gap so
  # neither TCG dispatch lag (timeout may fire at 8-12s wall) nor the
  # scheduler's 10s tick granularity lets the sleep finish first. Same
  # marker-in-drvname pattern so the cgroup dir is findable from the
  # VM host (sanitize_build_id: ".drv" → "_drv").
  timeoutDrv = drvs.mkTrivial {
    marker = "lifecycle-timeout";
    sleepSecs = 90;
  };

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
  # ''${...}/''' escaping: the inner .nix reads its OWN let-bound
  # busybox/dep, not this evaluation's scope.
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
        try:
            k3s_server.wait_until_succeeds(
                "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
                "  -o jsonpath='{.spec.holderIdentity}') && "
                'test -n "$leader" && '
                "k3s kubectl get --raw "
                '"/api/v1/namespaces/${ns}/pods/$leader:9091/proxy/metrics" '
                f"| {condition}",
                timeout=timeout,
            )
        except Exception:
            # I-056-style per-clause diagnostic: dispatch-stall flakes
            # (builder pod Running but derivations_running stays 0) are
            # invisible from kernel logs alone. Dump scheduler metrics,
            # scheduler logs (executor/dispatch/rejection), and builder
            # logs so the flake names which gate fired.
            k3s_server.execute(
                f"echo '=== DIAG[sched_metric_wait]: timeout={timeout}s, cond={condition!r} ===' >&2; "
                "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
                "  -o jsonpath='{.spec.holderIdentity}'); "
                'echo "leader=$leader" >&2; '
                "k3s kubectl get --raw "
                '  "/api/v1/namespaces/${ns}/pods/$leader:9091/proxy/metrics" '
                "  2>/dev/null | grep -E '^rio_scheduler_(workers_active|"
                "derivations_queued|derivations_running|dispatch_rejected)' >&2; "
                "k3s kubectl -n ${nsBuilders} get pods,jobs -o wide >&2 2>&1 || true; "
                'k3s kubectl -n ${ns} logs "$leader" --since=2m '
                "  | grep -iE 'executor|dispatch|reject|intent|heartbeat|worker|recovery' "
                "  | grep -vE '\"level\":\"DEBUG\"' | tail -60 >&2 || true; "
                "for p in $(k3s kubectl -n ${nsBuilders} get pods "
                "  -l rio.build/pool -o name 2>/dev/null); do "
                '  echo "=== builder $p ===" >&2; '
                "  k3s kubectl -n ${nsBuilders} logs $p --since=2m 2>&1 | tail -30 >&2; "
                "done || true"
            )
            raise

    # workers_active==0 wait, sized for the heartbeat-timeout FALLBACK
    # path. The ephemeral-pool drain has flaked at this
    # exact assertion four times (8cf70d94/d82a0046/69fc4ae0/e76c95b6,
    # GHA 24046508263). Each prior fix tightened the producer side; this
    # one bounds the consumer side from first principles.
    #
    # Mechanism (GHA 24046508263: pods-gone 1.24s, workers_active==0
    # 57.38s, then next workers_active==0 blew 90s; local KVM 0.17s):
    #
    #   I-195 made idle-SIGTERM `break 'reconnect` WITHOUT opening a
    #   second BuildExecution stream (the redundant ExecutorRegister
    #   that cov_factor was masking). That fix is correct, but it also
    #   removed the SECOND-CHANCE stream-EOF: pre-I-195, the worker
    #   opened S2 then immediately broke, so the scheduler saw TWO
    #   EOFs (S1-drop + S2-drop) — if one was lost under load, the
    #   other still landed `ExecutorDisconnected`. Post-I-195 there's
    #   only the S1-drop. Under GHA runner load the single RST_STREAM
    #   can be lost (CNI veth-teardown vs FIN race, h2 frame congestion);
    #   the scheduler's `worker-stream-reader` then never breaks, and
    #   the entry sits in `self.executors` until `tick_check_heartbeats`
    #   reaps it. When two pods' `last_heartbeat`s are staggered, the
    #   gauge can dip to 0 on the first reap (event-driven `decrement` at
    #   executor.rs:320) and be re-`set` to 1 by the next
    #   `tick_publish_gauges` if the second entry is still
    #   `is_registered()`. The first wait catches the dip; the next
    #   precondition wait then sits out the second entry's full
    #   fallback window.
    #
    # Budget derivation (rio-common/src/limits.rs constants):
    #   HEARTBEAT_TIMEOUT_SECS (30) = ~30s last_heartbeat→reap
    #   (tick_check_heartbeats), plus:
    #   + tick alignment (≤10s)
    #   + pod termination stagger: when two pods terminate ~20-30s apart,
    #     the second reap lands ~20-30s after the first
    #   + one in-flight heartbeat delayed under load (≤10s)
    #   ≈ 80s worst-case from pods-gone to stable workers_active==0.
    #   180s budget = 80s + ~125% GHA tail headroom. Stays under the
    #   300s pods-gone budget so a real disconnect-detection regression
    #   (e.g. heartbeat-reap broken) still trips before globalTimeout.
    #
    # Diagnostic dump on timeout: the prior three flakes were debugged
    # from k3s kernel logs alone (no scheduler-side state). This one
    # captures the live gauge value + scheduler's executor view +
    # any straggler pods, so a fifth flake names the stuck executor.
    def wait_workers_zero(ctx):
        try:
            sched_metric_wait(
                "grep -qx 'rio_scheduler_workers_active 0'",
                timeout=180,
            )
        except Exception:
            k3s_server.execute(
                f"echo '=== DIAG[{ctx}]: workers_active!=0 after 180s ===' >&2; "
                "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
                "  -o jsonpath='{.spec.holderIdentity}'); "
                "k3s kubectl get --raw "
                '  "/api/v1/namespaces/${ns}/pods/$leader:9091/proxy/metrics" '
                "  2>/dev/null | grep -E "
                "'^rio_scheduler_(workers_active|worker_disconnects_total) ' >&2; "
                "k3s kubectl -n ${nsBuilders} get pods -o wide >&2 2>&1 || true; "
                'k3s kubectl -n ${ns} logs "$leader" --since=4m '
                "  | grep -iE 'executor|disconnect|heartbeat|worker' "
                "  | grep -vE '\"level\":\"DEBUG\"' | tail -40 >&2 || true"
            )
            raise

    # Negative-apply a deliberately-invalid Pool spec. CRD CEL rules
    # (rio-crds/src/pool.rs x_kube validations) are cross-field
    # constraints that fire at kubectl-apply admission.
    # --dry-run=server sends to the apiserver (CEL evaluates) without
    # persisting. fail() asserts non-zero exit; the message-assert
    # proves it failed at the RIGHT rule — not, say, a schema error or
    # the wrong CEL rule. Quoted heredoc (<<'EOF') prevents shell
    # expansion inside the spec body.
    def assert_cel_rejects(name, spec_body, expected_msg, kind="Builder"):
        """spec_body is the YAML body UNDER `spec:` (2-space leading
        indent, no trailing newline on the last line). `kind` fills the
        required spec.kind (Builder/Fetcher); expected_msg is a
        substring of the CEL rule's .message() at rio-crds/src/pool.rs."""
        result = k3s_server.fail(
            "k3s kubectl apply --dry-run=server -f - 2>&1 <<'EOF'\n"
            "apiVersion: rio.build/v1alpha1\n"
            "kind: Pool\n"
            f"metadata:\n  name: {name}\n  namespace: ${nsBuilders}\n"
            f"spec:\n  kind: {kind}\n{spec_body}\n"
            "EOF"
        )
        assert expected_msg in result, (
            f"CEL should reject {name!r} with {expected_msg!r} in the "
            f"message, got: {result!r}. If the apply succeeded or "
            f"failed for a different reason, the CEL rule at "
            f"rio-crds/src/pool.rs isn't in the deployed CRD — "
            f"check `helm template | grep x-kubernetes-validations`."
        )
        print(f"{name}: CEL rejected with {expected_msg!r} ✓")

    # grpcurl against the scheduler's gRPC port (9001) and store (9002).
    # Plaintext gRPC (Cilium WireGuard handles encryption); port-forward
    # is a raw TCP tunnel through the apiserver. `-max-time` bounds the
    # RPC itself; port-forward is killed by trap even if grpcurl hangs.
    def sched_grpc(payload, method):
        """TriggerGC etc. on the scheduler leader. Returns stdout+stderr."""
        return pf_exec(leader_pod(), 9001,
            f"${grpcurl} ${grpcurlTls} -max-time 30 "
            f"-protoset ${protoset}/rio.protoset "
            f"-d '{payload}' localhost:__PORT__ {method}")

    def pin_live(out_path, tag):
        """Insert a scheduler_live_pins row for out_path so the GC mark
        phase treats it as a root (seed (e) in gc/mark.rs). Looks up
        store_path_hash via narinfo (PK is the BYTEA hash, not the text
        path). `tag` fills drv_hash — the table's real writer (scheduler
        dispatch) puts a derivation hash there; for test pins it's an
        arbitrary label so unpin/count assertions can scope to rows WE
        inserted, isolated from any scheduler-written rows."""
        psql_k8s(k3s_server,
            f"INSERT INTO scheduler_live_pins (store_path_hash, drv_hash) "
            f"SELECT store_path_hash, '{tag}' FROM narinfo "
            f"WHERE store_path = '{out_path}'"
        )

    def unpin_live(out_path, tag):
        psql_k8s(k3s_server,
            f"DELETE FROM scheduler_live_pins WHERE drv_hash = '{tag}' "
            f"AND store_path_hash = (SELECT store_path_hash FROM narinfo "
            f" WHERE store_path = '{out_path}')"
        )

    def submit_build_grpc(payload: dict, max_time: int = 5) -> str:
        """SubmitBuild via port-forward + grpcurl. Returns buildId.

        `max_time` caps the stream read — build usually won't finish,
        grpcurl exits DeadlineExceeded; ok_nonzero swallows. The build
        is persisted on receipt; stream is observability only. pf_exec
        auto-allocates a fresh port (TIME_WAIT-safe — SubmitBuild calls
        stack within one subtest, e.g. build-timeout submit→retry)."""
        out = pf_exec(leader_pod(), 9001,
            f"${grpcurl} ${grpcurlTls} -max-time {max_time} "
            f"-protoset ${protoset}/rio.protoset "
            f"-d '{json.dumps(payload)}' "
            f"localhost:__PORT__ rio.scheduler.SchedulerService/SubmitBuild",
            ok_nonzero=True)
        return _parse_submit_build_id(out)

    ${common.mkSubmitHelpers "k3s-server"}

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
  # Each fragment is a `with subtest(...)` block + its comment banner,
  # one file per subtest under scenarios/lifecycle/. Fragments are
  # composed by mkTest in the order given by `subtests`. Python variables
  # flow between fragments at module scope (no `with` scoping) — but in
  # the split architecture all fragments are self-contained (gc-sweep
  # builds its own paths; the old `initial` seed-subtest is gone).
  #
  # `scope` is the closure each fragment file sees via `with scope;` —
  # the fixture vars, drv let-bindings, and pkgs/common it interpolates.
  scope = {
    inherit
      pkgs
      common
      ns
      nsStore
      nsBuilders
      nsFetchers
      grpcurl
      grpcurlTls
      protoset
      pinDrv
      recoverySlowDrv
      recoveryDrv
      cancelDrv
      timeoutDrv
      gcVictimDrv
      ephemeralDrv1
      ephemeralDrv2
      tenantDrv
      rolloutPreDrv
      rolloutPostDrv
      refsDrvFile
      ;
  };
  fragments = builtins.mapAttrs (_: f: f scope) (common.importDir ./lifecycle);

  mkTest = common.mkFragmentTest {
    scenario = "lifecycle";
    inherit prelude fragments fixture;
    defaultTimeout = 900;
    chains = [ ];
  };
in
{
  inherit fragments mkTest;
}
