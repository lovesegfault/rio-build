# ComponentScaler end-to-end: rio-store predictive autoscaling.
#
# Fixture is k3sFull with `componentScaler.store.enabled=true` +
# tightened bounds (min=1 max=4 seedRatio=10) so the slowFanout
# load (30 leaves × `sleep 120`) drives `predicted = ceil(30/10) =
# 3` > min=1. Seed 10 (not the chart default 50) keeps the test
# inside a 2-node k3s VM's pod budget while still proving the
# predictive path scales > min.
#
# What unit tests CAN'T cover (and this does):
#   - CRD applied, reconciler watching, RBAC sufficient for
#     deployments/scale + componentscalers/status
#   - .status.learnedRatio survives a controller pod restart
#     (apiserver-persisted, not in-process)
#   - rendered store Deployment has NO .spec.replicas (helm-upgrade-
#     doesn't-fight; the {{if not enabled}} wrap held)
#   - GetLoad reachable per-pod via headless-svc DNS from the
#     controller's namespace (cross-ns FQDN)
#
# What this DOESN'T cover (unit tests do): 5-min scale-down
# stabilization, ratio decay/growth curves, max −1/tick. Those are
# pure-function behaviour proven in scaling/component.rs::tests.
#
# Tracey: r[verify ctrl.scaler.component] / r[verify
# ctrl.scaler.ratio-learn] / r[verify store.admin.get-load] /
# r[verify obs.metric.store-pg-pool] live at the default.nix wiring
# point per the convention.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns nsStore;

  # 30 leaves × `sleep 120`. NOT lib/derivations/fifty-fanout.nix —
  # those leaves are `echo > $out` and complete in <5s total; the
  # queue drains before the controller's 10s reconcile tick observes
  # it (third root-cause of this scenario's scale-up timeout). With
  # 120s/leaf and the fixture's ~1 builder slot, queued+running ≈ 30
  # for the full duration of scale-up + restart-preserves-ratio.
  # seedRatio=10 → predicted = ceil(30/10) = 3 > min=1.
  #
  # Standalone (no pkgs/nixpkgs eval) so the client VM can `nix-build
  # --impure` it with only `--arg busybox`. Not in lib/derivations/
  # because it's componentscaler-specific (the sleep makes it useless
  # as a load-throughput test).
  slowFanout = pkgs.writeText "slow-fanout.nix" ''
    { busybox }:
    let
      sh = "''${busybox}/bin/sh";
      bb = "''${busybox}/bin/busybox";
      mkLeaf = i: derivation {
        name = "rio-cscaler-load-''${toString i}";
        system = builtins.currentSystem;
        builder = sh;
        args = [ "-c" "''${bb} sleep 120; ''${bb} echo ''${toString i} > $out" ];
      };
      leaves = builtins.genList mkLeaf 30;
    in
    # Collector forces all 30 into the build graph. Never actually
    # builds in the test window (depends on 30×120s of leaves); we
    # only need it to make nix-build SUBMIT all leaves.
    derivation {
      name = "rio-cscaler-load-root";
      system = builtins.currentSystem;
      builder = sh;
      args = [ "-c" "''${bb} cat ''${toString leaves} > $out" ];
    }
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-componentscaler";
  skipTypeCheck = true;
  # ~4min k3s boot + ~60s scale-up wait + ~30s restart + slack.
  globalTimeout = 900 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    ${common.kvmCheck}
    start_all()
    ${fixture.waitReady}
    ${fixture.kubectlHelpers}
    # k3s-full's sshKeySetup (NOT common.sshKeySetup): the gateway is
    # a pod, not a systemd unit. The fixture variant patches the
    # rio-gateway-ssh Secret + rollout-restarts the Deployment.
    ${fixture.sshKeySetup}
    ${common.seedBusybox "k3s-server"}

    # ── helm-no-replicas: rendered template omits spec.replicas ──────
    # With componentScaler.store.enabled=true the chart MUST omit
    # .spec.replicas from the rendered Deployment — otherwise `helm
    # upgrade` resets it and fights the controller. We check the
    # k3s-applied manifest directly (services.k3s.manifests writes
    # the rendered YAML to a known path on the server node).
    #
    # NOT a managedFields check: k3s's deploy controller does a
    # CREATE on first apply, so it owns the defaulted replicas=1
    # field even though the YAML omitted it. That's a k3s artifact;
    # a real `helm upgrade` (SSA, manager=helm) would NOT own a field
    # absent from the template. The static-YAML check is the direct
    # proof of the exit criterion.
    with subtest("helm-no-replicas: rendered store template omits spec.replicas"):
        rendered = k3s_server.succeed(
            "${pkgs.yq-go}/bin/yq "
            "'select(.kind == \"Deployment\" and .metadata.name == \"rio-store\") "
            "| .spec | has(\"replicas\")' "
            "${fixture.helmRendered}/02-workloads.yaml"
        ).strip()
        assert rendered == "false", (
            f"rendered store Deployment has .spec.replicas (yq said "
            f"{rendered!r}). The {{if not componentScaler.store.enabled}} "
            f"wrap in templates/store.yaml didn't take — helm upgrade "
            f"would fight the controller."
        )
        print("helm-no-replicas: rendered template omits spec.replicas ✓")

    # ── cr-status: reconciler populates .status.learnedRatio ─────────
    # First reconcile (10s tick) reads spec.seedRatio (no status yet),
    # writes it to .status.learnedRatio, and patches /scale to min.
    # ~30s budget: CRD watch + first tick + status SSA roundtrip.
    with subtest("cr-status: .status.learnedRatio populated"):
        k3s_server.wait_until_succeeds(
            "r=$(k3s kubectl -n ${nsStore} get componentscaler store "
            "  -o jsonpath='{.status.learnedRatio}') && "
            'test -n "$r" && '
            'awk -v r="$r" "BEGIN{exit !(r>0)}"',
            timeout=60,
        )
        ratio = kubectl(
            "get componentscaler store -o jsonpath='{.status.learnedRatio}'",
            ns="${nsStore}",
        ).strip()
        print(f"cr-status: learnedRatio={ratio} ✓")
        assert float(ratio) > 0, f"learnedRatio should be a positive float, got {ratio!r}"

    # ── scale-up-predictive: 30 slow drvs → store scales > min ───────
    # slowFanout = 30 leaves × `sleep 120` + 1 collector. With
    # seedRatio=10 (fixture override), predicted = ceil(30/10) = 3
    # > min=1. The 120s sleep keeps queued+running ≈ 30 across the
    # full 90s wait — fifty-fanout.nix's instant leaves drained in
    # 4.6s, before the controller's 10s tick could observe them.
    with subtest("scale-up-predictive: store scales > min within 90s"):
        before = int(kubectl(
            "get deploy rio-store -o jsonpath='{.spec.replicas}'",
            ns="${nsStore}",
        ).strip() or "1")
        print(f"scale-up-predictive: before={before}")

        # Backgrounded nix-build: persists the build at submit; we
        # only need scheduler queued to go non-zero. systemd-run (NOT
        # `nohup ... &`) — same pattern as substitute.nix / fetcher-
        # split.nix: the unit detaches into PID 1's tree and survives
        # the succeed() session ending; journalctl -u surfaces eval
        # errors on failure (the precondition wait dumps it on
        # timeout).
        #
        # `--arg busybox`: slowFanout takes `{ busybox }`; the
        # storePath is the same one seedBusybox copied to the gateway
        # above. WITHOUT this arg, nix-build fails at eval (unbound
        # function arg) and queues nothing.
        # PATH: systemd-run gives a minimal env; nix's ssh-ng store
        # spawns `ssh` via execvp(PATH). HOME: ssh reads
        # ~/.ssh/known_hosts (the IdentityFile in /etc/ssh/ssh_config
        # is absolute, but known_hosts isn't). Without both, the unit
        # dies with "failed to start SSH connection to 'k3s-server'".
        client.succeed(
            "systemd-run --unit=fanout-build "
            "--setenv=HOME=/root "
            "--setenv=PATH=/run/current-system/sw/bin "
            "nix-build --no-out-link --impure "
            "--store 'ssh-ng://k3s-server' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${slowFanout}"
        )

        # Wait for .spec.replicas > min IMMEDIATELY after firing the
        # load — no separate "queued>0" precondition wait first. The
        # 120s/leaf load means the queue is sustained for the full
        # window; a separate precondition would only add latency. The
        # try/except dumps the fanout-build unit's journal (on
        # `client`, where the unit runs) if the wait times out — that
        # distinguishes "build never queued" (eval/ssh error in
        # journal) from "queued but reconciler didn't scale" (clean
        # journal, controller logs needed).
        #
        # Reconciler ticks every 10s; nix-build eval+upload takes
        # ~5-15s; allow ~9 ticks total. The store
        # Deployment's rollout (pods actually Ready) is NOT what we
        # assert — that's k8s's job. We assert the controller PATCHED
        # /scale.
        try:
            k3s_server.wait_until_succeeds(
                "r=$(k3s kubectl -n ${nsStore} get deploy rio-store "
                "  -o jsonpath='{.spec.replicas}') && "
                'test "$r" -gt 1',
                timeout=90,
            )
        except Exception:
            print("=== fanout-build unit (did the load queue?) ===")
            print(client.execute(
                "systemctl status fanout-build --no-pager; "
                "journalctl -u fanout-build --no-pager -n 30"
            )[1])
            print("=== componentscaler status (what did the reconciler see?) ===")
            print(kubectl(
                "get componentscaler store -o yaml", ns="${nsStore}"
            ))
            print("=== controller logs (last 40) ===")
            print(kubectl(
                "logs deploy/rio-controller --tail=40"
            ))
            raise
        after = int(kubectl(
            "get deploy rio-store -o jsonpath='{.spec.replicas}'",
            ns="${nsStore}",
        ).strip())
        desired = kubectl(
            "get componentscaler store -o jsonpath='{.status.desiredReplicas}'",
            ns="${nsStore}",
        ).strip()
        print(f"scale-up-predictive: replicas {before}→{after} desired={desired} ✓")
        assert after > before, f"expected scale-up, got {before}→{after}"

        # observedLoadFactor populated → GetLoad fan-out reached at
        # least one store pod via headless-svc DNS. May be ~0 (the
        # VM's PG load is tiny) but MUST be present.
        load = kubectl(
            "get componentscaler store -o jsonpath='{.status.observedLoadFactor}'",
            ns="${nsStore}",
        ).strip()
        assert load != "", (
            "observedLoadFactor empty — controller couldn't reach any "
            "rio-store-headless endpoint for GetLoad. Check cross-ns "
            "DNS (loadEndpoint FQDN) and store-admin TLS."
        )
        print(f"scale-up-predictive: observedLoadFactor={load} ✓")

    # ── restart-preserves-ratio: controller restart ≠ seed reset ─────
    # The exit criterion: learnedRatio persists in .status across
    # controller restarts. We read it, delete the controller pod
    # (Deployment recreates), wait for the next reconcile, re-read.
    # The status subresource isn't touched by pod restart; the new
    # process reads it back via the CR watch.
    with subtest("restart-preserves-ratio: learnedRatio survives controller restart"):
        before = kubectl(
            "get componentscaler store -o jsonpath='{.status.learnedRatio}'",
            ns="${nsStore}",
        ).strip()

        kubectl("delete pod -l app.kubernetes.io/name=rio-controller --wait=false")
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} wait --for=condition=Ready "
            "pod -l app.kubernetes.io/name=rio-controller --timeout=120s",
            timeout=130,
        )

        # Next reconcile (≤10s after Ready) rewrites status. We
        # compare the float — equality within a small tolerance (the
        # ratio MAY have decayed/grown by one tick between reads,
        # but it MUST NOT have reset to seedRatio=10 if it had
        # drifted, and MUST NOT be empty).
        k3s_server.wait_until_succeeds(
            "r=$(k3s kubectl -n ${nsStore} get componentscaler store "
            "  -o jsonpath='{.status.learnedRatio}') && "
            'test -n "$r"',
            timeout=60,
        )
        after = kubectl(
            "get componentscaler store -o jsonpath='{.status.learnedRatio}'",
            ns="${nsStore}",
        ).strip()
        print(f"restart-preserves-ratio: before={before} after={after}")
        assert after != "", "learnedRatio empty after restart — status wiped?"
        # The ratio is bounded by [RATIO_DECAY_ON_HIGH, RATIO_GROWTH_
        # ON_LOW] per tick (×0.95 .. ×1.02). A few ticks' drift is
        # ≤10% either way. A reset-to-seed would read exactly 10.0
        # only if `before` was also ~10.0 (no high/low load in VM),
        # so the strict check is "not blank and within drift of
        # before" — both directions covered by the 10% band.
        assert abs(float(after) - float(before)) / float(before) < 0.1, (
            f"learnedRatio drifted >10% across restart — likely reset to "
            f"seedRatio. before={before} after={after}"
        )

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
