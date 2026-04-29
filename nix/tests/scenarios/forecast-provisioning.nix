# §13b nodeclaim_pool reconciler end-to-end under KWOK fake-Karpenter.
#
# What unit tests CAN'T cover (and this does):
#   - NodeClaim CRD admission accepts cover::build_nodeclaim's shape
#     (nodeClassRef required-fields, requirements[].operator enum)
#   - controller RBAC sufficient for nodeclaims:{list,create,delete}
#   - figment loading of [nodeclaim_pool.instance_menu] from the
#     ConfigMap-mounted controller.toml (nested map-of-struct-list —
#     cannot load via Env provider; the helm ConfigMap is the only path)
#   - LiveNode::from parses real apiserver-round-tripped status
#     (lastTransitionTime as RFC3339, allocatable as Quantity strings)
#   - kube-build-scheduler second kube-scheduler Deployment runs + builder Jobs
#     get schedulerName/priorityClassName per r[ctrl.nodeclaim.
#     priority-bucket]
#   - r[ctrl.nodeclaim.shim-nodepool]: created NodeClaims carry
#     karpenter.sh/nodepool=rio-nodeclaim-shim
#
# What this DOESN'T cover (unit tests do): FFD ordering / MostAllocated
# scoring, anchor+bulk selection arithmetic, NA-consolidate break-even,
# Schmitt q-adjust. Pure-function in nodeclaim_pool/{ffd,cover,
# consolidate}.rs::tests.
#
# Structural assertions only — KWOK's 2s/5s synthetic boot means
# lead_time learning isn't realistic; the test asserts NodeClaims were
# CREATED + PROGRESSED + the metric pipeline emitted, not specific
# timings.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns nsBuilders;
  drvs = import ../lib/derivations.nix { inherit pkgs; };
in
pkgs.testers.runNixOSTest {
  name = "rio-forecast-provisioning";
  skipTypeCheck = true;
  globalTimeout = 900 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSeed = true;
    }}

    import json as _json

    # ── kwok-controller + kube-build-scheduler up ────────────────────
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n kube-system rollout status deploy/kwok-controller "
        "--timeout=60s",
        timeout=120,
    )
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} rollout status deploy/kube-build-scheduler "
        "--timeout=60s",
        timeout=120,
    )
    # NodeClaim CRD established (k3s deploy-controller retries on
    # NotFound; this gates the rio-controller's first list).
    k3s_server.wait_until_succeeds(
        "k3s kubectl get crd nodeclaims.karpenter.sh", timeout=60
    )

    # ── submit a build → scheduler emits SpawnIntents ────────────────
    # 4-leaf fanout: enough to produce ≥1 unplaced intent (zero
    # NodeClaims initially → EVERY intent unplaced on tick 1). The
    # build will sit Pending until a NodeClaim registers AND a
    # builder pod schedules; the subtest only needs the
    # nodeclaim_pool side-effects, so backgrounded.
    client.succeed(
        "nix-build ${drvs.fanout} "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "--no-out-link --store 'ssh-ng://k3s-server:32222' "
        ">/tmp/build.log 2>&1 &"
    )

    # ── nodeclaim_pool tick: NodeClaim created ───────────────────────
    # cover_deficit creates ≥1 NodeClaim within ~2 ticks (10s each)
    # of the scheduler's GetSpawnIntents reflecting the queued build.
    # OWNER_LABEL selector proves build_nodeclaim stamped it.
    try:
        k3s_server.wait_until_succeeds(
            "test $(k3s kubectl get nodeclaims "
            "-l rio.build/nodeclaim-pool=builder -o name | wc -l) -ge 1",
            timeout=90,
        )
    except Exception:
        print("=== nodeclaim_pool failed to create NodeClaim ===")
        print(kubectl("logs deploy/rio-controller --tail=200"))
        raise

    with subtest("nodeclaim labelled shim-nodepool + hw-class"):
        nc = _json.loads(k3s_server.succeed(
            "k3s kubectl get nodeclaims "
            "-l rio.build/nodeclaim-pool=builder -o json"
        ))
        assert nc["items"], "no owned NodeClaims"
        labels = nc["items"][0]["metadata"]["labels"]
        assert labels.get("karpenter.sh/nodepool") == "rio-nodeclaim-shim", labels
        assert labels.get("rio.build/hw-class") == "vmtest", labels

    # ── KWOK Stage progresses NodeClaim → Registered=True ────────────
    # 2s Launched + 3s Registered delays; allow a few KWOK reconcile
    # ticks. Structural: Registered cond present + status.allocatable
    # populated (LiveNode::from input).
    k3s_server.wait_until_succeeds(
        "k3s kubectl get nodeclaims -l rio.build/nodeclaim-pool=builder "
        "-o jsonpath='{.items[*].status.conditions[?(@.type==\"Registered\")].status}' "
        "| grep -q True",
        timeout=60,
    )
    alloc = k3s_server.succeed(
        "k3s kubectl get nodeclaims -l rio.build/nodeclaim-pool=builder "
        "-o jsonpath='{.items[0].status.allocatable.cpu}'"
    )
    assert alloc.strip(), f"status.allocatable.cpu unpopulated: {alloc!r}"

    # ── builder Job stamped schedulerName=kube-build-scheduler ───────
    # PlaceableGate publishes after a Registered claim appears; next
    # Pool-reconciler tick spawns Jobs with kube-build-scheduler +
    # priorityClassName per r[ctrl.nodeclaim.priority-bucket].
    wait_worker_pod(pool="x86-64", ns="${nsBuilders}", timeout=120)
    with subtest("builder pod has schedulerName=kube-build-scheduler + priorityClass"):
        pod = k3s_server.succeed(
            "k3s kubectl -n ${nsBuilders} get pods -l rio.build/pool=x86-64 "
            "-o jsonpath='{.items[0].spec.schedulerName} "
            "{.items[0].spec.priorityClassName}'"
        ).strip()
        sched, _, prio = pod.partition(" ")
        assert sched == "kube-build-scheduler", f"schedulerName={sched!r}"
        assert prio.startswith("rio-build-"), f"priorityClassName={prio!r}"

    # ── B14 metric pipeline emitted ──────────────────────────────────
    with subtest("nodeclaim_pool metrics emitted"):
        body = pf_exec(
            "deploy/rio-controller", 9094,
            "curl -fsS localhost:__PORT__/metrics",
        )
        for needle in (
            "rio_controller_nodeclaim_created_total",
            "rio_controller_nodeclaim_live",
            "rio_controller_nodeclaim_lead_time_seconds",
            "rio_controller_nodeclaim_tick_duration_seconds",
        ):
            assert needle in body, f"{needle} not in controller /metrics"

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
