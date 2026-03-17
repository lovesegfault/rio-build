# Leader-election scenario: stable leadership, failover, build-survives-failover.
#
# THE test catching cdb70c2 (observed-record tracks resourceVersion, not
# (holder, transitions)). Pre-fix, the standby watched (holderIdentity,
# leaseTransitions) for change. But renew only touches renewTime — holder
# and tx stay fixed. So a leader renewing every 5s looked identical to a
# dead one: standby's local clock never reset, after TTL it stole a LIVE
# lease. The two replicas flip-flopped on a ~35s cycle (20s held + 15s
# observed-ttl to steal back). stable-leadership below would fail with
# leaseTransitions climbing by ~2 per 35s window → +3 over 60s.
#
# Post-fix: decide() tracks metadata.resourceVersion. The apiserver bumps
# rv on every write (including renew), so a live leader produces a fresh
# rv every RENEW_INTERVAL=5s. Standby's clock resets on each bump →
# never reaches TTL → never steals → leaseTransitions stays flat.
#
#
# Fragment architecture: returns { fragments, mkTest }. default.nix
# composes into 2 parallel VM tests (stability, build). No Python-var
# chains — each subtest queries current leader_pod() independently.
# r[verify sched.lease.k8s-lease]
#   stable-leadership: observed-record rv tracking → no live-lease steal.
#   failover: ungraceful kill (no step_down) → standby observes unchanged
#     rv for TTL → steals → leaseTransitions +1.
#
# r[verify sched.lease.graceful-release]
# r[verify sched.lease.deletion-cost]
#   graceful-release: SIGTERM leader (no --force, --grace-period=30)
#   → step_down() runs to completion → standby acquires in <10s (vs
#   ~15s TTL-steal on ungraceful kill). lease/mod.rs:409-420. The
#   new leader's pod carries pod-deletion-cost=1 annotation so k8s
#   RollingUpdate kills the standby first → no leadership churn.
#
# Fixture: k3s-full (scheduler.replicas=2, podAntiAffinity spreads across
# k3s-server + k3s-agent). Caller wiring: see nix/tests/default.nix.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  # 60s build: long enough to span one full failover cycle (kill +
  # ~15s observed-ttl steal + gateway balanced-channel reprobe ~3s +
  # worker relay reconnect). The build runs on the WORKER the whole
  # time; leader failover just churns the control-plane stream.
  failoverDrv = drvs.mkTrivial {
    marker = "leader-failover";
    sleepSecs = 60;
  };

  # ── testScript prelude: bootstrap + Python helpers ────────────────────
  prelude = ''
    ${common.assertions}


    start_all()
    ${fixture.waitReady}

    ${fixture.kubectlHelpers}

    def lease_transitions():
        # .spec.leaseTransitions is an int32, present from first acquire
        # (the in-house election code writes it; phase3a asserts =0 on
        # create path). Never empty in practice, but defend anyway.
        raw = kubectl(
            "get lease rio-scheduler-leader "
            "-o jsonpath='{.spec.leaseTransitions}'"
        ).strip()
        return int(raw or "0")

    def renew_age_secs():
        # Kubernetes MicroTime serializes as RFC 3339 with µs precision
        # (e.g., 2026-03-15T12:34:56.789012Z). GNU date -d parses this
        # directly. k3s_server hosts the apiserver → no clock skew
        # between renewTime's write and our `date +%s` read.
        rt = kubectl(
            "get lease rio-scheduler-leader "
            "-o jsonpath='{.spec.renewTime}'"
        ).strip()
        rt_epoch = int(k3s_server.succeed(f"date -d '{rt}' +%s").strip())
        now_epoch = int(k3s_server.succeed("date +%s").strip())
        return now_epoch - rt_epoch

    def scheduler_pods():
        # status.phase=Running. CAUTION: a Terminating pod keeps
        # phase=Running (only deletionTimestamp is set) — this helper
        # returns terminating pods too. Callers that need a stable
        # replica set must gate on deploy .status.readyReplicas first.
        return kubectl(
            "get pods -l app.kubernetes.io/name=rio-scheduler "
            "--field-selector=status.phase=Running "
            "-o jsonpath='{.items[*].metadata.name}'"
        ).split()
  '';

  # ── Subtest fragments ─────────────────────────────────────────────────
  # build-during-failover includes its own sshKeySetup + seedBusybox
  # prep (the stability subtests don't need SSH — no nix-build).
  fragments = {
    antiAffinity = ''
      # ══════════════════════════════════════════════════════════════════
      # antiAffinity — precondition: replicas spread across both nodes
      # ══════════════════════════════════════════════════════════════════
      # If both scheduler pods landed on k3s-server (chart podAntiAffinity
      # misconfigured, or agent node not Ready in time), leader-kill won't
      # exercise a real cross-node failover — the standby would be on the
      # same box, sharing the same containerd/kubelet/network fate. Fail
      # fast here; every later subtest depends on this topology.
      with subtest("antiAffinity: scheduler replicas on different nodes"):
          node_names = kubectl(
              "get pods -l app.kubernetes.io/name=rio-scheduler "
              "-o jsonpath='{.items[*].spec.nodeName}'"
          ).split()
          assert len(node_names) == 2, (
              f"expected exactly 2 scheduler pods, got {len(node_names)}: "
              f"{node_names!r}. scheduler.replicas != 2 in vmtest-full.yaml?"
          )
          assert len(set(node_names)) == 2, (
              f"both schedulers on same node: {node_names!r}. "
              f"podAntiAffinity not applied, or k3s-agent joined late?"
          )
    '';

    lease-acquired = ''
      # ══════════════════════════════════════════════════════════════════
      # lease-acquired-metric — acquire-transition sanity + profraw check
      # ══════════════════════════════════════════════════════════════════
      # lease/mod.rs:300-305 comment says vm-phase3a polled this metric.
      # That check was lost in the fixture migration. Restoring it here
      # guards two things at once:
      #   1. The acquire transition (lease/mod.rs:282-353) actually fires.
      #      waitReady proved a holder exists, but that only proves the
      #      election.try_acquire_or_renew() succeeded — not that the
      #      edge-detection (`if now_leading && !was_leading`) ever went
      #      TRUE. A was_leading-init bug could let the lease work while
      #      deletion-cost/LeaderAcquired never fire.
      #   2. Profraw collection. Pre-POD_NAME fix, replacement pods on
      #      the same node overwrote the predecessor's profraw (both PID 1,
      #      same %m, shared hostPath). lcov showed DA:282=91 but DA:293=0
      #      — the condition ran 91× but the body never. This metric
      #      proves the body DID run; if lcov still shows 0 after the
      #      _helpers.tpl POD_NAME fix, the instrumentation is broken.
      with subtest("lease-acquired-metric: acquire transition fires"):
          total_acq = 0.0
          for pod in scheduler_pods():
              raw = k3s_server.succeed(
                  f"k3s kubectl get --raw "
                  f"'/api/v1/namespaces/${ns}/pods/{pod}:9091/proxy/metrics'"
              )
              m = parse_prometheus(raw)
              acq = metric_value(m, "rio_scheduler_lease_acquired_total") or 0.0
              print(f"{pod}: lease_acquired_total = {acq}")
              total_acq += acq
          # Exactly one scheduler should have acquired (the leader, once).
          # ≥1 not ==1: a slow apiserver during waitReady could cause a
          # brief steal-back before we check. ==0 is unambiguously broken.
          assert total_acq >= 1.0, (
              f"total lease_acquired across all schedulers = {total_acq}. "
              f"Acquire transition body (lease/mod.rs:293-353) never fired."
          )
    '';

    stable-leadership = ''
      # ══════════════════════════════════════════════════════════════════
      # stable-leadership — THE cdb70c2 assertion
      # ══════════════════════════════════════════════════════════════════
      # Pre-fix: flip-flop every ~35s → over 60s, leaseTransitions climbs
      # by at least 1 (likely 2-3). Post-fix: rv bumps on every renew →
      # standby's clock resets every ~5s → never reaches 15s TTL → no
      # steal → leaseTransitions FLAT.
      #
      # EXACT equality, not "small delta tolerated". ANY increment is a
      # steal, which is the bug. waitReady already confirmed a leader is
      # elected; from here on, with both pods healthy and no external
      # churn, leadership must be rock-solid.
      with subtest("stable-leadership: no flip-flop over 60s"):
          tx_before = lease_transitions()
          holder_before = leader_pod()
          print(f"stable-leadership: holder={holder_before} tx={tx_before}, sleeping 60s")

          # 60s > one flip-flop cycle (~35s). Use node.sleep (not time.sleep)
          # so the test driver's status loop still runs.
          k3s_server.sleep(60)

          tx_after = lease_transitions()
          holder_after = leader_pod()
          assert tx_after == tx_before, (
              f"leaseTransitions changed during 60s stable window: "
              f"{tx_before} → {tx_after} (delta={tx_after - tx_before}). "
              f"Holder: {holder_before} → {holder_after}. "
              f"This is the cdb70c2 flip-flop: standby stole a live lease "
              f"because its observed-record didn't reset on renew."
          )
          assert holder_after == holder_before, (
              f"holderIdentity changed: {holder_before} → {holder_after} "
              f"but leaseTransitions stayed at {tx_after}? "
              f"Election code not incrementing transitions on steal?"
          )
    '';

    graceful-release = ''
      # ══════════════════════════════════════════════════════════════════
      # graceful-release — SIGTERM (no --force) → step_down → fast acquire
      # ══════════════════════════════════════════════════════════════════
      # failover below uses --grace-period=0 --force: SIGTERM + ~immediate
      # SIGKILL. step_down() RACES the SIGKILL (it wins post-a5b06ef, but
      # the profraw doesn't flush — POD_NAME fix solved the overwrite, not
      # the SIGKILL-before-atexit). This subtest is the PRODUCTION rollout
      # path: --grace-period=30, no --force → pure SIGTERM → full 30s
      # drain window → step_down() completes, main() returns, atexit
      # flushes profraw. lease/mod.rs:409-420 graceful-release body.
      #
      # deletion-cost: on acquire, spawn_patch_deletion_cost writes
      # controller.kubernetes.io/pod-deletion-cost=1 to the pod. K8s
      # ReplicaSet sorts by this during scale-down/RollingUpdate →
      # kills standby first → no leadership churn on rollout. The new
      # leader's pod should have it.
      with subtest("graceful-release: SIGTERM leader → step_down → standby acquires <10s"):
          import time as _time
          leader = leader_pod()
          tx_before = lease_transitions()

          # Graceful delete. No --force. --wait=false returns after
          # sending the DELETE (doesn't block for pod Terminating→gone)
          # so we can time the lease handover. NOT a background thread:
          # NixOS test driver Machine.succeed isn't thread-safe —
          # concurrent shell commands interleave output streams,
          # breaking the driver's return-code parse (int on empty).
          k3s_server.succeed(
              f"k3s kubectl -n ${ns} delete pod {leader} "
              f"--grace-period=30 --wait=false"
          )

          # step_down clears holderIdentity → standby sees empty →
          # Steal on next 5s tick. TTL=15s is the FALLBACK if step_down
          # failed. <10s proves step_down fired (5s poll + slack);
          # 10-15s would be ambiguous; >15s = step_down broken.
          t0 = _time.time()
          k3s_server.wait_until_succeeds(
              f"h=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
              f"-o jsonpath='{{.spec.holderIdentity}}') && "
              f'test -n "$h" && test "$h" != "{leader}"',
              timeout=10,
          )
          elapsed = _time.time() - t0
          assert elapsed < 10, (
              f"standby took {elapsed:.1f}s to acquire (TTL=15s). "
              f"step_down didn't fire — graceful-release broken?"
          )

          # deletion-cost on the NEW leader. Fire-and-forget spawn
          # (lease/mod.rs:320) — may take a tick to land. 5s slack.
          new_leader = leader_pod()
          k3s_server.wait_until_succeeds(
              f"test \"$(k3s kubectl -n ${ns} get pod {new_leader} "
              f"-o jsonpath='{{.metadata.annotations.controller\\.kubernetes\\.io/pod-deletion-cost}}')\" = 1",
              timeout=10,
          )

          # leaseTransitions bumped exactly once (one clean handover).
          tx_after = lease_transitions()
          assert tx_after == tx_before + 1, (
              f"expected leaseTransitions +1 (one clean step_down→steal), "
              f"got {tx_before}→{tx_after}"
          )

          # Wait for Deployment replacement to be Ready — failover below
          # asserts scheduler_pods() == 2. The old pod (--wait=false,
          # grace=30s) keeps status.phase=Running while Terminating, so
          # `phase=Running | wc -l = 2` passes in ~5s by counting
          # [terminating old] + [remaining] — NOT [remaining] + [new].
          # .status.readyReplicas is the Deployment controller's truth:
          # terminating pods don't count, Pending replacements don't
          # count. Replacement lands where the old pod was (antiAffinity);
          # image is already imported there → ~10-30s to Ready.
          k3s_server.wait_until_succeeds(
              "test \"$(k3s kubectl -n ${ns} get deploy rio-scheduler "
              "-o jsonpath='{.status.readyReplicas}')\" = 2",
              timeout=90,
          )
          print(f"graceful-release PASS: {leader}→{new_leader} in {elapsed:.1f}s, "
                f"deletion-cost=1 on new leader")
    '';

    failover = ''
      # ══════════════════════════════════════════════════════════════════
      # failover — kill leader, standby acquires
      # ══════════════════════════════════════════════════════════════════
      # --grace-period=0 --force still sends SIGTERM before SIGKILL
      # (kubelet behavior). The scheduler's graceful shutdown is now
      # fast enough (a5b06ef token-aware drain) that step_down()
      # completes before SIGKILL arrives — it clears holderIdentity.
      # The standby's decide() sees empty holder → Decision::Steal →
      # acquires immediately (no TTL wait) AND bumps leaseTransitions.
      #
      # The old `!= old_leader` wait passed on the EMPTY holder
      # between step_down and standby acquire — reading tx then
      # raced the standby's poll tick (5s). Wait for the standby
      # to actually hold instead.
      with subtest("failover: kill leader, standby acquires"):
          tx_before = lease_transitions()
          old_leader = leader_pod()

          # Record both pod names BEFORE the kill for the diagnostic
          # print. With graceful step_down (a5b06ef), EITHER the
          # standby OR the Deployment-spawned replacement can win the
          # empty lease — standby's next poll tick is 5s, replacement
          # startup is ~3-5s. Both are valid failover outcomes; the
          # key invariant is tx == +1 (clean single handoff).
          pods_before = scheduler_pods()
          assert old_leader in pods_before, (
              f"leader_pod()={old_leader!r} not in running pods "
              f"{pods_before!r}? holderIdentity/pod-name mismatch?"
          )
          others = [p for p in pods_before if p != old_leader]
          assert len(others) == 1, (
              f"expected exactly 1 standby, got {others!r} "
              f"(pods={pods_before!r}, leader={old_leader!r})"
          )
          standby = others[0]
          print(f"failover: killing leader={old_leader}, standby={standby}")

          kubectl(f"delete pod {old_leader} --grace-period=0 --force")

          # Wait for ANY non-empty holder ≠ old_leader. step_down clears
          # holder instantly; either the standby OR the Deployment-spawned
          # replacement acquires on their next poll tick. -n guards
          # against the empty window between step_down and acquire.
          # Either outcome is valid (see comment above).
          k3s_server.wait_until_succeeds(
              "h=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
              "-o jsonpath='{.spec.holderIdentity}'); "
              f"test -n \"$h\" && test \"$h\" != '{old_leader}'",
              timeout=45,
          )

          new_leader = leader_pod()
          tx_after = lease_transitions()

          # Exactly +1. Not >=1: multiple increments would mean the
          # standby acquired, then lost, then reacquired during the
          # wait — that's a different bug (unstable fresh leader).
          assert tx_after == tx_before + 1, (
              f"expected leaseTransitions +1 on single failover, got "
              f"{tx_before} → {tx_after} (delta={tx_after - tx_before}). "
              f">1 = leadership bounced; 0 = steal didn't increment tx."
          )

          # Diagnostic: note which pod won. Either is valid with
          # graceful step_down — the old STEAL-path expectation
          # ("standby must win because replacement needs > TTL to
          # start") no longer applies.
          winner = "standby" if new_leader == standby else "replacement"
          print(f"failover: new leader = {new_leader} ({winner})")

          # renewTime is fresh. The new leader writes renewTime on acquire
          # and every RENEW_INTERVAL after. If it's stale, either the
          # acquire didn't set it or the renew loop isn't running.
          age = renew_age_secs()
          assert age < 10, (
              f"renewTime is {age}s old (expected <10s). New leader "
              f"{new_leader!r} acquired but isn't renewing? "
              f"RENEW_INTERVAL=5s → age should be 0-5s in steady state."
          )
    '';

    build-during-failover = ''
      # ── SSH + busybox prep for build-during-failover ────────────────────
      # Between subtests: gateway scale-bounce (sshKeySetup) doesn't
      # touch scheduler pods, so leader state survives. But DO wait for
      # the deployment to recover to 2/2 first — the replacement pod
      # from the failover subtest may still be coming up, and we want
      # build-during-failover to start from a stable 2-replica topology.
      k3s_server.wait_until_succeeds(
          "k3s kubectl -n ${ns} wait --for=condition=Available "
          "deploy/rio-scheduler --timeout=90s",
          timeout=120,
      )
      # And a leader is still (or again) elected — the deploy-Available
      # wait above proves 2/2 Ready, not leader-settled.
      k3s_server.wait_until_succeeds(
          "k3s kubectl -n ${ns} get lease rio-scheduler-leader "
          "-o jsonpath='{.spec.holderIdentity}' | grep -q rio-scheduler",
          timeout=30,
      )

      ${fixture.sshKeySetup}
      ${common.seedBusybox "k3s-server"}

      import threading

      # ══════════════════════════════════════════════════════════════════
      # build-during-failover — build survives scheduler leader kill
      # ══════════════════════════════════════════════════════════════════
      # Same shape as smoke-test step 7 (worker-kill reassign) but the
      # victim is the SCHEDULER LEADER, not a worker. The build keeps
      # running on the worker the whole time; what churns is the
      # control-plane stream: gateway's balanced-channel reroutes to the
      # new leader (grpc.health.v1 probe ~3s), worker's relay reconnects
      # and replays buffered events. The client's nix-build sees nothing.
      #
      # This is the end-to-end proof of scheduler.md's "Workers reconnect
      # in place — running builds continue, no pod restarts."
      with subtest("build-during-failover: build survives scheduler leader kill"):
          # Re-check topology after the prep churn above.
          pods_before = scheduler_pods()
          assert len(pods_before) == 2, (
              f"expected 2 running scheduler pods before build-during-failover, "
              f"got {pods_before!r}"
          )

          bg = {}
          def _bg():
              try:
                  bg["out"] = client.succeed(
                      "nix-build --no-out-link "
                      "--store 'ssh-ng://k3s-server' "
                      "--arg busybox '(builtins.storePath ${common.busybox})' "
                      "${failoverDrv}"
                  ).strip()
              except Exception as e:
                  bg["err"] = e
          bg_thread = threading.Thread(target=_bg, daemon=True)
          bg_thread.start()

          # Wait for the build to actually DISPATCH before killing the
          # leader. If we kill too early (build still in SubmitBuild /
          # DAG merge on the leader), we're testing "submit during
          # failover" not "build during failover" — different codepath.
          #
          # Signal: scheduler metric derivations_running ≥ 1. NOT
          # kubectl-logs: the prior failover subtest's force-delete on
          # k3s-server breaks the k3s-agent kubelet's log stream
          # ("Failed when writing line to log file: http2: stream
          # closed" — containerd→kubelet stream dies, doesn't recover).
          # kubectl-logs returns stale/empty even though the build IS
          # running (client shows `building '...'`).
          #
          # apiserver pods/proxy subresource — no local port-forward,
          # no TIME_WAIT. Numeric port 9091 (not named `:metrics` —
          # k3s apiserver nil-derefs on named-port proxy, v20).
          k3s_server.wait_until_succeeds(
              "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
              "  -o jsonpath='{.spec.holderIdentity}') && "
              'test -n "$leader" && '
              "k3s kubectl get --raw "
              '"/api/v1/namespaces/${ns}/pods/$leader:9091/proxy/metrics" '
              "| grep -E '^rio_scheduler_derivations_running [1-9]'",
              timeout=30,
          )

          old_leader = leader_pod()
          print(f"build-during-failover: build dispatched, killing leader={old_leader}")
          kubectl(f"delete pod {old_leader} --grace-period=0 --force")

          # Build completes. 60s sleep + ~25s failover acquire + relay
          # reconnect slack. 180s is very generous — if we hit this,
          # something is hung (gateway never rerouted, worker relay
          # buffer overflowed, new leader rejected the replay).
          bg_thread.join(timeout=180)
          assert not bg_thread.is_alive(), (
              "build thread did not finish within 180s after leader kill. "
              "Gateway balanced-channel never rerouted to new leader? "
              "Worker relay buffer lost on reconnect?"
          )
          if "err" in bg:
              dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")
              raise bg["err"]
          assert bg.get("out", "").startswith("/nix/store/"), (
              f"build returned {bg.get('out')!r}, expected a store path. "
              f"Build succeeded from the worker's perspective but the "
              f"result didn't propagate back through the new leader?"
          )
    '';

  };

  # graceful-release and failover both kill the leader. graceful-release
  # leaves the cluster at 2/2 (waits for replacement); failover doesn't.
  # If build-during-failover follows failover, its own buildprep handles
  # the stabilization wait. No ordering assertion needed — fragments are
  # independent at the Python level.
  # Coverage-instrumented images are ~3-4× larger. The k3s containerd
  # tmpfs (fixtures/k3s-full.nix) removes the 3.3-5× builder-disk-write
  # variance that motivated the original +900s; remaining variance is
  # 9p reads (airgap tarballs from nix store) + zstd decompress (CPU).
  # +300s hedges for that residual tail. Additive so any explicit
  # globalTimeout override stacks. Normal-mode CI budget unchanged.
  covTimeoutHeadroom = if common.coverage then 300 else 0;

  mkTest =
    {
      name,
      subtests,
      globalTimeout ? 900,
    }:
    pkgs.testers.runNixOSTest {
      name = "rio-leader-election-${name}";
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
