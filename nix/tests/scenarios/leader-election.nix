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
# never reaches STEAL_AFTER (19s) → never steals → leaseTransitions stays flat.
#
#
# Fragment architecture: returns { fragments, mkTest }. default.nix
# composes into 2 parallel VM tests (stability, build). No Python-var
# chains — each subtest queries current leader_pod() independently.
# sched.lease.k8s-lease — verify marker at default.nix:subtests[stable-leadership]
#   stable-leadership: observed-record rv tracking → no live-lease steal.
#   failover: ungraceful kill (no step_down) → standby observes unchanged
#     rv for STEAL_AFTER (19s) → steals → leaseTransitions +1.
#
# sched.lease.graceful-release — verify marker at default.nix:subtests[graceful-release]
# sched.lease.deletion-cost — verify marker at default.nix:subtests[graceful-release]
#   graceful-release: SIGTERM leader (no --force, --grace-period=30)
#   → step_down() runs to completion → standby acquires in <10s (vs
#   the 19s observed-staleness steal on ungraceful kill).
#   Graceful-release body: rio-lease run_lease_loop's shutdown path. The
#   new leader's pod carries pod-deletion-cost=1 annotation so k8s
#   RollingUpdate kills the standby first → no leadership churn.
#
# sched.lease.generation-claim — verify marker at default.nix:subtests[lease-deletion]
#   lease-deletion: kubectl delete lease + kill the holder → the next
#   acquisition derives a low generation from the fresh Lease but the
#   PG claims-ledger floor pulls it above the old regime's high-water.
#   Structural psql assertions on leader_generation_claims.
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
  jq = "${pkgs.jq}/bin/jq";

  # 60s build: long enough to span one full failover cycle (kill +
  # STEAL_AFTER=19s + one 5s poll observed-staleness steal + gateway
  # balanced-channel reprobe ~3s + worker relay reconnect). The build
  # runs on the WORKER the whole time; leader failover just churns the
  # control-plane stream.
  failoverDrv = drvs.mkTrivial {
    marker = "leader-failover";
    sleepSecs = 60;
  };

  # Distinct marker → distinct drv hash. failoverDrv is already built
  # by the time sigkill-mid-build runs (subtests order); a reused drv
  # would cache-hit and complete instantly — no mid-build window.
  sigkillDrv = drvs.mkTrivial {
    marker = "leader-sigkill";
    sleepSecs = 60;
  };

  # ── testScript prelude: bootstrap + Python helpers ────────────────────
  prelude = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSsh = false;
    }}

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
      # SchedulerLeaseHooks::on_acquire (rio-scheduler/src/lease_hooks.rs)
      # increments rio_scheduler_lease_acquired_total and its comment
      # says a VM scenario polls it. That check was lost when the legacy
      # phase fixtures were retired. Restoring it here guards two things
      # at once:
      #   1. The acquire transition in rio-lease's run_lease_loop
      #      (rio-lease/src/lib.rs) actually fires. waitReady proved a
      #      holder exists, but that only proves the
      #      election.try_acquire_or_renew() succeeded — not that the
      #      edge-detection (`if now_leading && !was_leading`) ever went
      #      TRUE. A was_leading-init bug could let the lease work while
      #      deletion-cost/LeaderAcquired never fire.
      #   2. Profraw collection. Pre-POD_NAME fix, replacement pods on
      #      the same node overwrote the predecessor's profraw (both PID 1,
      #      same %m, shared hostPath). lcov showed the loop condition ran
      #      many times but the acquire body never. This metric proves the
      #      body DID run; if lcov still shows 0 after the _helpers.tpl
      #      POD_NAME fix, the instrumentation is broken.
      with subtest("lease-acquired-metric: acquire transition fires"):
          total_acq = 0.0
          for pod in scheduler_pods():
              raw = k3s_server.wait_until_succeeds(
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
              f"Acquire transition body (LeaderState::on_acquire via run_lease_loop) never fired."
          )
    '';

    stable-leadership = ''
      # ══════════════════════════════════════════════════════════════════
      # stable-leadership — THE cdb70c2 assertion
      # ══════════════════════════════════════════════════════════════════
      # Pre-fix: flip-flop every ~35s → over 60s, leaseTransitions climbs
      # by at least 1 (likely 2-3). Post-fix: rv bumps on every renew →
      # standby's clock resets every ~5s → never reaches STEAL_AFTER (19s)
      # → no steal → leaseTransitions FLAT.
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
      # flushes profraw. Graceful-release body: step_down in run_lease_loop.
      #
      # deletion-cost: on acquire, spawn_patch_leader_marks writes
      # controller.kubernetes.io/pod-deletion-cost=1 to the pod (along
      # with the leader label the rio-scheduler-leader Service selects
      # on). K8s ReplicaSet sorts by the cost during
      # scale-down/RollingUpdate → kills standby first → no leadership
      # churn on rollout. The new leader's pod should have it.
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
          # steal on next 5s tick. The FALLBACK if step_down failed is
          # the observed-staleness steal at STEAL_AFTER=19s (~20-25s
          # after the delete). <10s proves step_down fired (5s poll +
          # slack); between 10s and the steal fallback would be
          # ambiguous; at/after the fallback = step_down broken.
          t0 = _time.time()
          k3s_server.wait_until_succeeds(
              f"h=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
              f"-o jsonpath='{{.spec.holderIdentity}}') && "
              f'test -n "$h" && test "$h" != "{leader}"',
              timeout=10,
          )
          elapsed = _time.time() - t0
          assert elapsed < 10, (
              f"standby took {elapsed:.1f}s to acquire (steal fallback 19s). "
              f"step_down didn't fire — graceful-release broken?"
          )

          # deletion-cost on the NEW leader. Detached spawn off the
          # election loop (spawn_patch_leader_marks) — may take a tick
          # to land. 5s slack.
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

    lease-deletion = ''
      # Recover from the failover subtest above: it force-deletes the
      # leader and does NOT wait for the Deployment replacement, so the
      # cluster may be at 1/2 here. We are about to kill a leader again
      # and need a standby to exist for the handover to be a real
      # cross-process re-acquisition.
      k3s_server.wait_until_succeeds(
          "test \"$(k3s kubectl -n ${ns} get deploy rio-scheduler "
          "-o jsonpath='{.status.readyReplicas}')\" = 2",
          timeout=120,
      )
      k3s_server.wait_until_succeeds(
          "k3s kubectl -n ${ns} get lease rio-scheduler-leader "
          "-o jsonpath='{.spec.holderIdentity}' | grep -q rio-scheduler",
          timeout=90,
      )

      # ══════════════════════════════════════════════════════════════════
      # lease-deletion — generation survives destruction of the epoch source
      # ══════════════════════════════════════════════════════════════════
      # The leadership generation derives from the Lease's leaseTransitions
      # (bumped atomically with the holder change by the rv-guarded PUT).
      # `kubectl delete lease` destroys that counter: the recreated Lease
      # restarts at transitions=0, so the lease-derived generation restarts
      # at 1-2. The ONLY thing keeping the new epoch above every generation
      # the old regime handed to dispatch is the PG floor read during
      # recovery: GREATEST(MAX(assignments.generation),
      # MAX(leader_generation_claims.generation)). The claims ledger is the
      # half of that floor that survives an idle cluster (the assignments
      # high-water decays via the orphan-derivation sweep + migration 034's
      # ON DELETE CASCADE) and a depose-before-persist (the claim is
      # written during recovery, BEFORE dispatch is ungated, so it exists
      # even if the leader never persisted an assignment).
      #
      # Deletion is a plausible operational event, not an exotic one:
      # `kubectl delete lease` is the documented k8s remedy for a stuck
      # election, and the platform treats leaseTransitions as advisory.
      #
      # Assertions are STRUCTURAL (psql against the claims ledger), not
      # log-greps: kubectl-logs polling churns the kubelet log stream
      # after a force-delete (see build-during-failover's comment) and
      # the ledger row IS the property under test — the generation an
      # executor would fence against is exactly MAX(claims).
      with subtest("lease-deletion: generation stays monotonic across kubectl delete lease"):
          # Every leadership acquisition so far (bootstrap + the
          # graceful-release and failover handovers) wrote one claims
          # row during its recovery. A populated ledger is the
          # precondition for this subtest to prove anything.
          rows_before = int(psql_k8s(k3s_server,
              "SELECT COUNT(*) FROM leader_generation_claims"))
          gen_before = int(psql_k8s(k3s_server,
              "SELECT COALESCE(MAX(generation), 0) "
              "FROM leader_generation_claims"))
          assert rows_before >= 1 and gen_before >= 1, (
              f"claims ledger empty before lease deletion "
              f"(rows={rows_before}, max_gen={gen_before}). The "
              f"write-ahead claim never ran during any prior "
              f"acquisition — recovery is not reaching "
              f"claim_generation()?"
          )
          old_leader = leader_pod()
          tx_before = lease_transitions()
          print(f"lease-deletion: leader={old_leader} tx={tx_before} "
                f"max_claim={gen_before} rows={rows_before}")

          # Destroy the epoch source FIRST, then kill the holder. The
          # other order would let the standby steal the still-intact
          # lease (inheriting its transition count) before the
          # deletion lands, and the subtest would degenerate into a
          # plain failover.
          kubectl("delete lease rio-scheduler-leader")
          kubectl(f"delete pod {old_leader} --grace-period=0 --force")

          # A new holder appears on a FRESH lease. Whoever wins
          # (standby or Deployment replacement) either create()s it at
          # transitions=0 or steals the live leader's 404-recreated
          # one at transitions=1 — both derive a generation of 1-2
          # from the lease alone.
          k3s_server.wait_until_succeeds(
              "h=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
              "-o jsonpath='{.spec.holderIdentity}'); "
              f"test -n \"$h\" && test \"$h\" != '{old_leader}'",
              timeout=60,
          )

          # The new epoch's claim row lands during recovery (a few PG
          # round-trips after the acquire edge). Poll the ledger
          # itself: MAX(generation) must EXCEED the old regime's
          # high-water. `SELECT expr` of a boolean prints t/f under
          # -tA; grep -qx t is the structural wait.
          k3s_server.wait_until_succeeds(
              "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
              "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
              f"'SELECT MAX(generation) > {gen_before} "
              f"FROM leader_generation_claims' | grep -qx t",
              timeout=90,
          )

          gen_after = int(psql_k8s(k3s_server,
              "SELECT MAX(generation) FROM leader_generation_claims"))
          rows_after = int(psql_k8s(k3s_server,
              "SELECT COUNT(*) FROM leader_generation_claims"))
          tx_after = lease_transitions()
          new_leader = leader_pod()

          # Monotonic across the deletion: the new epoch exceeds every
          # generation the old regime ever claimed.
          assert gen_after > gen_before, (
              f"generation did not advance past the old regime after "
              f"lease deletion: max_claim {gen_before} -> {gen_after}. "
              f"The PG floor (max_known_generation) is not pulling the "
              f"recreated-lease generation above the old high-water."
          )
          # Append-only: the old regime's rows survive (forensic
          # record), the new epoch added its own.
          assert rows_after > rows_before, (
              f"claims ledger did not grow across the deletion "
              f"({rows_before} -> {rows_after} rows). New leader "
              f"reused or overwrote an old row?"
          )
          # Non-vacuity: the lease-derived generation alone
          # (transitions+1) must be BELOW the actual generation —
          # otherwise this subtest proved nothing about the PG floor
          # (the lease could have carried the monotonicity by itself,
          # e.g. if the deletion silently failed).
          assert tx_after + 1 < gen_after, (
              f"lease-derived generation (leaseTransitions+1 = "
              f"{tx_after + 1}) is not below the claimed generation "
              f"({gen_after}) — the recreated Lease still carries the "
              f"old transition count, so the deletion did not actually "
              f"reset the epoch source and this subtest is vacuous."
          )
          print(f"lease-deletion PASS: {old_leader} -> {new_leader}, "
                f"max_claim {gen_before} -> {gen_after}, "
                f"rows {rows_before} -> {rows_after}, tx={tx_after}")
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
          timeout=90,
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
      # This is the end-to-end proof of scheduler.typ's "Workers reconnect
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
              timeout=90,
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

    sigkill-mid-build = ''
      # ══════════════════════════════════════════════════════════════════
      # sigkill-mid-build — TRUE ungraceful death: SIGKILL host PID
      # ══════════════════════════════════════════════════════════════════
      # Gap closed: every other subtest goes through kubectl delete.
      # Even `--grace-period=0 --force` sends SIGTERM before SIGKILL
      # (kubelet behavior), and post-a5b06ef step_down() wins that race
      # → holderIdentity cleared → standby acquires via empty-holder
      # fast path. NOTHING tested the no-FIN, no-step_down path: process
      # vanishes mid-build (OOM-kill, kernel panic, node hard-reset).
      #
      # Mechanism: crictl resolves the leader container's host-namespace
      # PID; `kill -9` from the node bypasses kubelet entirely. No
      # SIGTERM, no graceful shutdown hooks, no TCP FIN — sockets go
      # half-open until peer keepalive/h2-ping fires. Kubelet detects
      # the container exit and restarts it in-place (Deployment pods
      # are restartPolicy=Always); pod object survives, restartCount
      # increments. This is "old leader dies, NEW process in same pod"
      # — distinct from `kubectl delete` which evicts the pod and
      # spawns a fresh one (restartCount=0).
      #
      # Expected lease behavior (election.rs decide()):
      #   - Restarted container has same HOSTNAME → same holder_id.
      #     First tick: GET shows holder == our_id → Decision::Renew
      #     → replace(steal=false). leaseTransitions UNCHANGED. The
      #     standby sees rv bump on that renew, resets its observed
      #     clock, never reaches STEAL_AFTER.
      #   - UNLESS kubelet restart + scheduler init takes longer than
      #     STEAL_AFTER=19s (CrashLoopBackOff, slow image pull):
      #     standby's observed-rv stays unchanged for STEAL_AFTER →
      #     Decision::Steal → tx +1.
      # Either is correct. Assert tx delta ∈ {0, 1} and the build
      # completes regardless. The same-pod-resume path (delta=0) is the
      # production OOM-kill path; the standby-steal path (delta=1) is
      # the ONLY test of observed-record-expiry steal — `failover`
      # above doesn't reach it because step_down wins.
      #
      # Ordering: runs AFTER build-during-failover in vm-le-build-k3s.
      # sshKeySetup/seedBusybox already done (ssh-keygen is not
      # idempotent — don't re-run). `import threading` already loaded.
      k3s_server.wait_until_succeeds(
          "test \"$(k3s kubectl -n ${ns} get deploy rio-scheduler "
          "-o jsonpath='{.status.readyReplicas}')\" = 2",
          timeout=120,
      )
      k3s_server.wait_until_succeeds(
          "k3s kubectl -n ${ns} get lease rio-scheduler-leader "
          "-o jsonpath='{.spec.holderIdentity}' | grep -q rio-scheduler",
          timeout=90,
      )

      with subtest("sigkill-mid-build: SIGKILL leader host-PID, build survives"):
          old_leader = leader_pod()
          tx_before = lease_transitions()
          rc_before = int(kubectl(
              f"get pod {old_leader} "
              f"-o jsonpath='{{.status.containerStatuses[0].restartCount}}'"
          ).strip() or "0")

          bg = {}
          def _bg():
              try:
                  bg["out"] = client.succeed(
                      "nix-build --no-out-link "
                      "--store 'ssh-ng://k3s-server' "
                      "--arg busybox '(builtins.storePath ${common.busybox})' "
                      "${sigkillDrv}"
                  ).strip()
              except Exception as e:
                  bg["err"] = e
          bg_thread = threading.Thread(target=_bg, daemon=True)
          bg_thread.start()

          # Wait for dispatch — same metric probe as build-during-failover.
          # leader_pod() inside the shell pipeline (not old_leader): the
          # prior subtest's failover may have moved leadership; re-read
          # at probe time.
          k3s_server.wait_until_succeeds(
              "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
              "  -o jsonpath='{.spec.holderIdentity}') && "
              'test -n "$leader" && '
              "k3s kubectl get --raw "
              '"/api/v1/namespaces/${ns}/pods/$leader:9091/proxy/metrics" '
              "| grep -E '^rio_scheduler_derivations_running [1-9]'",
              timeout=90,
          )

          # Re-read leader AFTER dispatch confirmed (build-during-failover
          # may have churned it; old_leader above was a best-effort early
          # snapshot for the diagnostic — refresh tx/rc to match).
          old_leader = leader_pod()
          tx_before = lease_transitions()
          rc_before = int(kubectl(
              f"get pod {old_leader} "
              f"-o jsonpath='{{.status.containerStatuses[0].restartCount}}'"
          ).strip() or "0")

          # ── crictl → host PID → kill -9 (netpol.nix pattern) ──────────
          # antiAffinity (asserted in vm-le-stability) puts exactly one
          # scheduler per node, so kill -9 on the leader's node hits ONLY
          # the leader. Still resolve via crictl for surgical precision —
          # pkill -f would also match e.g. a kubectl exec shell.
          node_name = kubectl(
              f"get pod {old_leader} -o jsonpath='{{.spec.nodeName}}'"
          ).strip()
          host_vm = k3s_agent if node_name == "k3s-agent" else k3s_server
          cid = host_vm.succeed(
              f"k3s crictl ps -q "
              f"--label io.kubernetes.pod.name={old_leader} | head -1"
          ).strip()
          assert cid, f"no running container for leader {old_leader}"
          host_pid = host_vm.succeed(
              f"k3s crictl inspect {cid} | ${jq} -r .info.pid"
          ).strip()
          assert host_pid and host_pid != "0", (
              f"crictl inspect returned bad pid: {host_pid!r}"
          )
          print(f"sigkill: leader={old_leader} on {node_name}, "
                f"host-pid={host_pid}, tx={tx_before}, rc={rc_before}")
          host_vm.succeed(f"kill -9 {host_pid}")
          # Anchor: capture renewTime AFTER the kill. SIGKILL is
          # synchronous — the dead process cannot write again, and the
          # restarted container's first renew is seconds out (standby
          # steal ≥19s out), so this is guaranteed to be the dead
          # leader's FINAL write. Capturing BEFORE the kill is a
          # TOCTOU: the still-live leader may complete one more renew
          # (RENEW_INTERVAL=5s) in the ~100-800ms succeed()-round-trip
          # gap, and the renewTime!=renew_before check below would
          # then fire on a PRE-kill write.
          renew_before = k3s_server.succeed(
              "k3s kubectl -n ${ns} get lease rio-scheduler-leader "
              "-o jsonpath='{.spec.renewTime}'"
          ).strip()

          # ── kubelet restarted the container in-place ──────────────────
          # Proves we hit the crash path (pod survives, restartCount+1)
          # not the pod-evict path (new pod, rc=0). Kubelet container
          # status sync is ~1s; restart backoff is 0 on first crash.
          k3s_server.wait_until_succeeds(
              f"test \"$(k3s kubectl -n ${ns} get pod {old_leader} "
              f"-o jsonpath='{{.status.containerStatuses[0].restartCount}}')\" "
              f"-gt {rc_before}",
              timeout=90,
          )

          # ── leadership recovered within the steal threshold + slack ───
          # holderIdentity is NEVER cleared on this path (no step_down).
          # It stays = old_leader the whole time; what changes is WHO is
          # renewing it. renew_age_secs() going fresh again proves a live
          # process (restarted container OR standby post-steal) is
          # writing.
          k3s_server.wait_until_succeeds(
              "h=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
              "-o jsonpath='{.spec.holderIdentity}'); "
              'test -n "$h"',
              timeout=90,
          )
          # And renewing (not just a stale holder string). renewTime
          # advancing past renew_before proves a LIVE process wrote
          # post-kill; THEN age<10 proves it's fresh. Structural floor:
          # the restarted container renews as itself in ~5-15s (fast
          # path), but the FALLBACK is the standby waiting out the
          # staleness threshold — STEAL_AFTER=19s + 5s poll + init slack
          # ≈ 25-30s — and under builder CPU contention the container
          # restart can lose to the fallback. Budget for the tail
          # (5x the slow path), not the typical.
          k3s_server.wait_until_succeeds(
              "test \"$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
              "-o jsonpath='{.spec.renewTime}')\" "
              f"!= '{renew_before}'",
              timeout=120,
          )
          age = renew_age_secs()
          assert age < 10, (
              f"renewTime is {age}s old after SIGKILL+restart. "
              f"No process is renewing the lease — restarted container "
              f"stuck before first tick, AND standby didn't steal (STEAL_AFTER)?"
          )

          new_leader = leader_pod()
          tx_after = lease_transitions()
          delta = tx_after - tx_before
          # delta=0: same pod resumed via Renew (decide_pure's
          #   HolderKind::Us arm). Production OOM-kill path.
          # delta=1: standby stole (decide_pure's HolderKind::Other
          #   staleness arm). Restart took longer than STEAL_AFTER (19s).
          assert delta in (0, 1), (
              f"leaseTransitions {tx_before}→{tx_after} (delta={delta}). "
              f">1 = leadership bounced; <0 = impossible."
          )
          path = (
              "same-pod-resume (Renew, holder==our_id)"
              if delta == 0 else
              "standby-steal (observed-rv expiry)"
          )
          if delta == 0:
              assert new_leader == old_leader, (
                  f"tx unchanged but holder moved {old_leader}→{new_leader}? "
                  f"Steal without leaseTransitions++ — replace(steal)'s transitions bump broken?"
              )
          print(f"sigkill: recovered via {path}, leader={new_leader}")

          # ── build survived ────────────────────────────────────────────
          # 60s sleep + (0-20s lease recovery) + gateway balanced-channel
          # reprobe ~3s + worker relay reconnect. Gateway/worker saw the
          # gRPC stream drop with NO GOAWAY (process vanished) — h2
          # keepalive (project_heartbeat_zombie I-048) is what detects it.
          bg_thread.join(timeout=180)
          assert not bg_thread.is_alive(), (
              "build thread did not finish within 180s after SIGKILL. "
              "h2 keepalive never fired? Gateway stuck on half-open "
              "socket to dead leader? Worker relay buffer lost?"
          )
          if "err" in bg:
              dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")
              raise bg["err"]
          assert bg.get("out", "").startswith("/nix/store/"), (
              f"build returned {bg.get('out')!r}, expected a store path"
          )
    '';

  };

  # graceful-release and failover both kill the leader. graceful-release
  # leaves the cluster at 2/2 (waits for replacement); failover doesn't.
  # If build-during-failover follows failover, its own buildprep handles
  # the stabilization wait. sigkill-mid-build DEPENDS on
  # build-during-failover's `import threading` + sshKeySetup/seedBusybox
  # (:396-399, :521-523) — chained below so mkAssertChains catches a
  # mis-ordering at eval time.
  mkTest = common.mkFragmentTest {
    scenario = "leader-election";
    inherit prelude fragments fixture;
    defaultTimeout = 900;
    chains = [
      {
        before = "build-during-failover";
        after = "sigkill-mid-build";
        msg = "sigkill-mid-build reuses build-during-failover's import threading + sshKeySetup (:396-399, :521-523)";
      }
    ];
  };
in
{
  inherit fragments mkTest;
}
