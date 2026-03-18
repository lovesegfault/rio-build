# Plan 0268: Chaos harness — toxiproxy VM fixture + 4 scenarios

**USER Q6: all 4 faults** (was 2). Per GT12: ZERO toxiproxy/chaos anywhere in `nix/tests/` or `rio-*/src/` — clean slate. Pure greenfield.

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged (frontier root)

## Tasks

### T1 — `feat(nix):` toxiproxy fixture

NEW [`nix/tests/fixtures/toxiproxy.nix`](../../nix/tests/fixtures/toxiproxy.nix) — NixOS module wrapping toxiproxy as a systemd service between scheduler↔store and worker↔store.

MODIFY [`nix/docker-pulled.nix`](../../nix/docker-pulled.nix) — add `shopify/toxiproxy` image pull.

### T2 — `test(vm):` chaos scenarios (4 subtests per USER Q6)

NEW [`nix/tests/scenarios/chaos.nix`](../../nix/tests/scenarios/chaos.nix):

```nix
{ fixture, ... }: {
  name = "chaos";
  testScript = ''
    start_all()
    # Subtest 1: scheduler↔store latency 500ms — builds complete (retry/timeout paths work)
    toxiproxy.succeed("toxiproxy-cli toxic add scheduler_store -t latency -a latency=500")
    gateway.succeed("nix build -f ${trivial} --store ssh-ng://localhost")  # slow but succeeds
    toxiproxy.succeed("toxiproxy-cli toxic remove scheduler_store -n latency")

    # Subtest 2: worker↔store reset mid-PutPath — upload retries succeed
    #            (also exercises P0267's atomic-multi-output fix)
    toxiproxy.succeed("toxiproxy-cli toxic add worker_store -t reset_peer -a timeout=100")
    gateway.succeed("nix build -f ${twoOutput} --store ssh-ng://localhost")  # retries, succeeds
    toxiproxy.succeed("toxiproxy-cli toxic remove worker_store -n reset_peer")

    # Subtest 3: partition 30s — builds queue, resume after partition heals
    toxiproxy.succeed("toxiproxy-cli toxic add scheduler_store -t timeout -a timeout=30000")
    # submit build in background, assert Queued not Failed during partition
    # ... heal, assert completion

    # Subtest 4: bandwidth 1Mbps — large NAR upload completes eventually
    toxiproxy.succeed("toxiproxy-cli toxic add worker_store -t bandwidth -a rate=125")  # KB/s
    gateway.succeed("timeout 120 nix build -f ${largeOutput} --store ssh-ng://localhost")
  '';
}
```

### T3 — `test(vm):` register in default.nix

MODIFY [`nix/tests/default.nix`](../../nix/tests/default.nix) — add `vm-chaos` scenario.

## Exit criteria

- `/nbr .#ci` green (includes new `vm-chaos-*`)
- All 4 subtests pass

## Tracey

none — test infrastructure. No markers.

## Files

```json files
[
  {"path": "nix/tests/fixtures/toxiproxy.nix", "action": "NEW", "note": "T1: toxiproxy systemd module"},
  {"path": "nix/docker-pulled.nix", "action": "MODIFY", "note": "T1: shopify/toxiproxy image"},
  {"path": "nix/tests/scenarios/chaos.nix", "action": "NEW", "note": "T2: 4 fault subtests (USER Q6)"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T3: register vm-chaos"}
]
```

```
nix/tests/
├── fixtures/toxiproxy.nix        # T1: NEW
├── scenarios/chaos.nix           # T2: NEW (4 faults)
└── default.nix                   # T3: register
nix/docker-pulled.nix             # T1: toxiproxy image
```

## Dependencies

```json deps
{"deps": [245], "soft_deps": [254], "note": "USER Q6: 4 faults (latency+reset+partition+bandwidth). MAX PARALLEL — greenfield per GT12. default.nix SOFT-conflict with P0254 — coordinator serializes dispatch (4c A9 pattern)."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) — frontier root.
**Conflicts with:** `default.nix` SOFT-conflict with [P0254](plan-0254-ca-metrics-vm-demo.md) (both add one line). Zero code-file collision — MAX PARALLEL otherwise.
