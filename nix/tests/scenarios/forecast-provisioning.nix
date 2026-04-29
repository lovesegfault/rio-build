# §13b nodeclaim_pool reconciler end-to-end under KWOK fake-Karpenter.
#
# What unit tests CAN'T cover (and this does):
#   - NodeClaim CRD admission accepts cover::build_nodeclaim's shape
#     (nodeClassRef required-fields, requirements[].operator enum)
#   - controller RBAC sufficient for nodeclaims:{list,create,delete}
#   - figment loading of [nodeclaim_pool.lead_time_seed] from the
#     ConfigMap-mounted controller.toml (nested map — cannot load via
#     Env provider; the helm ConfigMap is the only path)
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
    # KWOK's StagesManager runs RESTMapper discovery once on Stage-CR
    # add; the k3s deploy-controller starts kwok-controller alongside
    # the karpenter CRDs, so the karpenter.sh apigroup commonly isn't
    # served yet → `failed to get gvk for gvr` and the NodeClaim
    # resourceRef is dropped permanently. Restart now that the CRD is
    # established so discovery resolves.
    k3s_server.succeed(
        "k3s kubectl -n kube-system rollout restart deploy/kwok-controller"
    )
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n kube-system rollout status deploy/kwok-controller "
        "--timeout=60s",
        timeout=90,
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
    # populated (LiveNode::from input). On timeout dump kwok-controller
    # logs — KWOK swallows gojq selector / template errors at info
    # level, so a regressed Stage rule looks like silence otherwise.
    try:
        k3s_server.wait_until_succeeds(
            "k3s kubectl get nodeclaims -l rio.build/nodeclaim-pool=builder "
            "-o jsonpath='{.items[*].status.conditions[?(@.type==\"Registered\")].status}' "
            "| grep -q True",
            timeout=60,
        )
    except Exception:
        print("=== NodeClaim never Registered — kwok Stage rule did not fire ===")
        print(k3s_server.execute(
            "k3s kubectl get nodeclaims -o yaml; "
            "k3s kubectl get stages.kwok.x-k8s.io -o yaml; "
            "k3s kubectl -n kube-system logs deploy/kwok-controller --tail=200"
        )[1])
        raise
    alloc = k3s_server.succeed(
        "k3s kubectl get nodeclaims -l rio.build/nodeclaim-pool=builder "
        "-o jsonpath='{.items[0].status.allocatable.cpu}'"
    )
    assert alloc.strip(), f"status.allocatable.cpu unpopulated: {alloc!r}"

    # ── fake Node from NodeClaim (KWOK Stage cannot create CRs) ──────
    # Karpenter copies NodeClaim.metadata.labels onto the launched Node;
    # KWOK only patches the NodeClaim, so apply that Node manually with
    # `kwok.x-k8s.io/node=fake` so KWOK manages it as a fake kubelet
    # (Ready=True via the upstream `node-initialize` Stage, then
    # `pod-ready` for anything kube-build-scheduler binds there).
    # status.allocatable is seeded so the upstream Stage echoes it back
    # as `.status.allocatable` and kube-scheduler admits the builder
    # pod's resource requests.
    nc0 = _json.loads(k3s_server.succeed(
        "k3s kubectl get nodeclaims -l rio.build/nodeclaim-pool=builder "
        "-o json"
    ))["items"][0]
    fake_node = {
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": {
            "name": nc0["status"]["nodeName"],
            "labels": {
                **nc0["metadata"]["labels"],
                "kubernetes.io/hostname": nc0["status"]["nodeName"],
                "kubernetes.io/os": "linux",
                "kubernetes.io/arch": "amd64",
                "type": "kwok",
            },
            "annotations": {"kwok.x-k8s.io/node": "fake"},
        },
        "status": {
            "allocatable": nc0["status"]["allocatable"],
            "capacity": nc0["status"]["capacity"],
        },
    }
    k3s_server.succeed(
        "k3s kubectl apply -f - <<'EOF'\n"
        + _json.dumps(fake_node)
        + "\nEOF"
    )
    k3s_server.wait_until_succeeds(
        f"k3s kubectl get node {nc0['status']['nodeName']} "
        "-o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}' "
        "| grep -q True",
        timeout=60,
    )

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
        assert prio.startswith("rio-builder-prio-"), f"priorityClassName={prio!r}"

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
