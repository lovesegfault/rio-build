# Plan 0240: VM Section F+J scheduling fragments — cancel timing + load

phase4c.md:43,47 — two fragments in [`nix/tests/scenarios/scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) (699 lines, fragment pattern, no other 4c or 4b touch).

**Section F — cancel timing.** Submit a slow build, `CancelBuild` mid-run, assert `rio_scheduler_cancel_signals_total` increments AND the build's cgroup is gone within 5s. **R6 critical:** the 5s budget is tighter than the 10s prometheus scrape interval. Per tooling-gotchas memory: **assert cgroup-gone via direct `ls /sys/fs/cgroup/rio/...`, NOT via prom scrape.** A prom-scrape-based assertion would race the scrape interval and flake.

**Section J — load test.** 50-derivation DAG, assert `rio_scheduler_derivations_completed_total >= 50`.

Both counters already exist (`cancel_signals_total` + `derivations_completed_total`) — this is test coverage, not feature work.

## Tasks

### T1 — `test(vm):` cancel-timing fragment

MODIFY [`nix/tests/scenarios/scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) — TAIL-append fragment key `cancel-timing`:

```nix
cancel-timing = ''
  with subtest("cancel-timing: CancelBuild → cgroup gone within 5s"):
      import time

      # Submit a slow build (sleep 300 — we'll cancel long before it finishes)
      build_id = submit_build(server, drv_path="${drvs.slowSleep300}")
      server.wait_until_succeeds(
          f"curl -s localhost:9090/metrics | grep 'rio_scheduler_builds_active' | grep -v '^#' | grep -q '1'",
          timeout=30
      )

      # Find the cgroup path (drv-hash under /sys/fs/cgroup/rio/)
      # The worker creates /sys/fs/cgroup/rio/<drv-hash>/ on dispatch.
      drv_hash = "${builtins.substring 11 32 drvs.slowSleep300}"  # extract hash from /nix/store/HASH-name
      cgroup = f"/sys/fs/cgroup/rio/{drv_hash}"

      # Wait for cgroup to exist (build dispatched)
      server.wait_until_succeeds(f"kubectl exec -it $(kubectl get pod -l app=rio-worker -o name | head -1) -- ls {cgroup}")

      # Cancel
      t0 = time.time()
      server.succeed(f"rio-cli cancel-build {build_id}")

      # cgroup gone within 5s — DIRECT ls, NOT prom scrape (R6: prom=10s > 5s budget)
      server.wait_until_fails(
          f"kubectl exec -it $(kubectl get pod -l app=rio-worker -o name | head -1) -- ls {cgroup}",
          timeout=5
      )
      elapsed = time.time() - t0
      print(f"cancel-timing: cgroup gone in {elapsed:.2f}s (budget 5s)")

      # Counter incremented (this CAN use prom — it's a counter, monotone)
      server.wait_until_succeeds(
          "curl -s localhost:9090/metrics | grep 'rio_scheduler_cancel_signals_total' | grep -v '^#' | awk '{print $2}' | grep -v '^0$'",
          timeout=15  # give the scrape a beat
      )
'';
```

### T2 — `test(vm):` load-50drv fragment

TAIL-append fragment key `load-50drv`:

```nix
load-50drv = ''
  with subtest("load-50drv: 50-derivation DAG completes"):
      # 50-node linear DAG (or binary tree — whichever drvs.fiftyChain provides)
      build_id = submit_build(server, drv_path="${drvs.fiftyChain}")

      # All 50 complete — poll the counter.
      # derivations_completed_total is monotone so prom scrape timing is fine.
      server.wait_until_succeeds(
          "curl -s localhost:9090/metrics | grep 'rio_scheduler_derivations_completed_total' | grep -v '^#' | awk '{exit ($2 >= 50 ? 0 : 1)}'",
          timeout=600  # 50 builds × ~10s each worst case
      )
      print("load-50drv PASS: derivations_completed_total >= 50")
'';
```

**`drvs.fiftyChain` check:** verify `nix/tests/drvs/` has a 50-node chain fixture. If not, create one (50 `runCommand` derivations in a linear chain, each depending on the previous). ~20 lines of nix.

## Exit criteria

- `/nbr .#ci` green — including `vm-scheduling-*` tests
- `cancel-timing` subtest: cgroup gone within 5s of CancelBuild (direct `ls`, not prom)
- `load-50drv` subtest: `derivations_completed_total >= 50`

## Tracey

No markers — these test existing features (cancel signals + dispatch throughput). No new spec behavior.

## Files

```json files
[
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T1+T2: cancel-timing + load-50drv fragments (TAIL-append to fragments attrset)"}
]
```

```
nix/tests/scenarios/scheduling.nix   # T1+T2: 2 fragments (699L file, no other 4b/4c touch)
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "Wave-1 frontier. scheduling.nix 699L, fragment pattern, no other 4b/4c touch. Tests EXISTING features — cancel_signals_total + derivations_completed_total already registered."}
```

**Depends on:** none. Both counters already exist.
**Conflicts with:** none — `scheduling.nix` has no other 4b or 4c touch.

**R6 reminder:** the cgroup-gone assertion MUST use direct `kubectl exec ls /sys/fs/cgroup/rio/{hash}` — NOT a prometheus scrape. The 5s budget < 10s scrape interval.
