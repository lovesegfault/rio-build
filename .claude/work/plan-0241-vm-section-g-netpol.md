# Plan 0241: VM Section G NetworkPolicy scenario + registration

phase4c.md:44,49 — NEW `netpol.nix` scenario on k3s-full with `networkPolicy.enabled=true`. Assert worker egress to IMDS (`169.254.169.254`) and public internet (`1.1.1.1:80`) is **blocked**. Ingress policy SKIPPED (Phase 5). Register `vm-netpol-k3s` in [`nix/tests/default.nix`](../../nix/tests/default.nix).

**DESIGN BRANCHES ON [P0220](plan-0220-netpol-preverify-decision.md) OUTCOME.** Default (A2): stock k3s kube-router enforces. If P0220's followup says `{"netpol":"needs-calico"}`, this plan BALLOONS: ~3 docker images added to `nix/docker-pulled.nix` + k3s-full fixture `--flannel-backend=none` param + ~50 extra lines. Re-estimate at dispatch.

**Soft conflict with [P0243](plan-0243-vm-fod-proxy-scenario.md) on `nix/tests/default.nix`** — both add 1 import + 1 mkTest. Independent lines; coordinator dispatches sequentially (NOT a dag dep).

**Scope shrink (GT5):** `infra/helm/rio-build/templates/networkpolicy.yaml` **already exists**. This is scenario-only, no helm template work.

## Entry criteria

- [P0220](plan-0220-netpol-preverify-decision.md) merged (followup row with `payload.netpol` recorded — design decision made)

## Tasks

### T1 — `test(vm):` netpol.nix scenario (default: kube-router path)

NEW `nix/tests/scenarios/netpol.nix`:

```nix
{ pkgs, mkTest, k3sFull, helmRender, ... }:
mkTest {
  name = "netpol";
  fixture = k3sFull;
  helmValues = {
    networkPolicy.enabled = true;  # flips the existing networkpolicy.yaml template on
  };
  testScript = ''
    import time

    # Wait for k3s + rio-worker ready
    server.wait_until_succeeds("kubectl get pods -l app=rio-worker | grep Running", timeout=300)

    # Give kube-router a beat to sync NetPol CRs (it watches them)
    time.sleep(10)

    worker_pod = server.succeed("kubectl get pod -l app=rio-worker -o jsonpath='{.items[0].metadata.name}'").strip()

    with subtest("netpol-imds: IMDS egress blocked"):
        # IMDS is the prime threat — a sandbox escapee shouldn't get AWS creds.
        rc = server.execute(f"kubectl exec {worker_pod} -- curl --max-time 5 -sS http://169.254.169.254/latest/meta-data/")[0]
        assert rc != 0, f"IMDS curl succeeded (rc=0) — NetPol NOT enforcing"
        print(f"netpol-imds PASS: IMDS blocked (curl rc={rc})")

    with subtest("netpol-internet: public egress blocked"):
        rc = server.execute(f"kubectl exec {worker_pod} -- curl --max-time 5 -sS http://1.1.1.1")[0]
        assert rc != 0, f"public egress succeeded (rc=0) — NetPol NOT enforcing"
        print(f"netpol-internet PASS: 1.1.1.1 blocked (curl rc={rc})")

    # Ingress policy: SKIPPED (Phase 5).
    # FOD proxy egress allowlist: covered by P0243 (vm-fod-proxy-k3s).
  '';
}
```

**IF P0220 outcome = needs-calico**, add before testScript:

```nix
# ADDITIONAL if P0220 → needs-calico:
fixture = k3sFull.override {
  extraServerArgs = ["--flannel-backend=none" "--disable-network-policy"];
  extraPreloadImages = [ calico-node calico-cni calico-kube-controllers ];  # from docker-pulled.nix
};
# + testScript preamble that applies Calico manifests + waits for calico-node DaemonSet ready
```

### T2 — `test(vm):` register in default.nix

MODIFY [`nix/tests/default.nix`](../../nix/tests/default.nix) — add import + mkTest:

```nix
# import (near other scenario imports):
netpol = import ./scenarios/netpol.nix;

# mkTest registration (near other vm-*-k3s entries):
vm-netpol-k3s = netpol { inherit pkgs mkTest k3sFull helmRender; };
```

Independent lines from P0243's additions. Coordinator dispatches one at a time; no dag dep.

## Exit criteria

- `/nbr .#ci` green — including new `vm-netpol-k3s`
- `netpol-imds` subtest: curl to 169.254.169.254 fails (rc≠0)
- `netpol-internet` subtest: curl to 1.1.1.1 fails (rc≠0)

## Tracey

No markers — NetPol enforcement is platform behavior (k8s + kube-router), not rio-build spec.

## Files

```json files
[
  {"path": "nix/tests/scenarios/netpol.nix", "action": "NEW", "note": "T1: NetPol scenario — IMDS + public egress blocked. k3s-full + networkPolicy.enabled=true. Design branches on P0220 outcome."},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T2: import + vm-netpol-k3s mkTest (SOFT conflict with P0243 — independent lines)"}
]
```

```
nix/tests/
├── scenarios/netpol.nix   # T1 (NEW)
└── default.nix            # T2: import + mkTest (soft conflict w/ P0243)
```

## Dependencies

```json deps
{"deps": [220], "soft_deps": [243], "note": "deps:[P0220(decision)] — design branches on netpol outcome. SOFT conflict with P0243 on default.nix (both add 1 import + 1 mkTest) — coordinator serializes dispatch, NOT a dag dep."}
```

**Depends on:** [P0220](plan-0220-netpol-preverify-decision.md) — followup row with `payload.netpol` outcome. If `needs-calico`: scope balloons; re-estimate before dispatch.
**Conflicts with:** `nix/tests/default.nix` — soft conflict with [P0243](plan-0243-vm-fod-proxy-scenario.md). Both add independent import+mkTest lines. Coordinator dispatches sequentially.

**Hidden check at dispatch (audit B2 #19):** `jq 'select(.source_plan=="P0220") | .description' .claude/state/followups-pending.jsonl` — P0220's `state.py followup P0220` CLI sets `source_plan="P0220"` and `origin="reviewer"` (origin Literal doesn't include plan IDs). Outcome is in `description` text: grep for `needs-calico`. If present: stop, re-estimate, possibly promote a Calico-preload plan first.
