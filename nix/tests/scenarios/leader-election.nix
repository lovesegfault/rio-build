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
# r[verify sched.lease.k8s-lease]
#   stable-leadership: observed-record rv tracking → no live-lease steal.
#   failover: ungraceful kill (no step_down) → standby observes unchanged
#     rv for TTL → steals → leaseTransitions +1.
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
in
pkgs.testers.runNixOSTest {
  name = "rio-leader-election";
  # waitReady (~3-4min) + stable-leadership (60s) + failover (~30s) +
  # sshKeySetup rollout (~30s) + build-during-failover (60s build +
  # failover latency). 900s is generous headroom for VM scheduling jitter.
  globalTimeout = 900;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    import threading

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
        # Running-only: during failover the deleted pod briefly lingers
        # in Terminating (which get pods still lists). Filter on phase
        # so we only see the live replicas.
        return kubectl(
            "get pods -l app.kubernetes.io/name=rio-scheduler "
            "--field-selector=status.phase=Running "
            "-o jsonpath='{.items[*].metadata.name}'"
        ).split()

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

        # Capture the OTHER pod's name BEFORE the kill. After delete,
        # the Deployment controller spawns a replacement with a fresh
        # name; we want to assert the STANDBY (pre-existing) won, not
        # the fresh pod. If the fresh pod won, observed-record expiry
        # is slower than cold-start lease acquire — suspicious.
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
        print(f"failover: killing leader={old_leader}, expecting standby={standby}")

        kubectl(f"delete pod {old_leader} --grace-period=0 --force")

        # Wait for ANY non-empty holder ≠ old_leader. step_down clears
        # holder instantly; either the standby OR the Deployment-spawned
        # replacement acquires on their next poll tick. -n guards
        # against the empty window between step_down and acquire.
        # Which one actually won is checked by `new_leader == standby`
        # below — this wait just clears the empty window.
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

        # The standby won, not a freshly-spawned replacement.
        assert new_leader == standby, (
            f"new leader {new_leader!r} is not the pre-existing standby "
            f"{standby!r}. Either a fresh pod out-raced the standby "
            f"(observed-record expiry too slow?), or holderIdentity "
            f"doesn't match pod name."
        )

        # renewTime is fresh. The new leader writes renewTime on acquire
        # and every RENEW_INTERVAL after. If it's stale, either the
        # acquire didn't set it or the renew loop isn't running.
        age = renew_age_secs()
        assert age < 10, (
            f"renewTime is {age}s old (expected <10s). New leader "
            f"{new_leader!r} acquired but isn't renewing? "
            f"RENEW_INTERVAL=5s → age should be 0-5s in steady state."
        )

    # ── SSH + busybox prep for build-during-failover ────────────────────
    # Between subtests: gateway rollout restart (sshKeySetup) doesn't
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
        # Signal: worker pod logs the derivation name. 120s: after the
        # failover subtest + gateway rollout, the gateway's balanced
        # channel has to reprobe scheduler pod IPs (replacement pod
        # has a new IP); first dispatch after that churn is slower.
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} logs default-workers-0 --tail=200 "
            "| grep -q 'rio-test-leader-failover'",
            timeout=120,
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

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
